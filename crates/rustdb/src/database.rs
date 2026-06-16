//! The embedded database: the single object that wires every layer together
//! and runs SQL statements.
//!
//! `Database` owns the storage stack (file manager, buffer pool, WAL, and
//! transaction manager), an in-memory [`Catalog`], and a descriptor per table
//! ([`TableStore`]). DDL updates the catalog and creates or drops the backing
//! [`MvccTable`]; `INSERT` encodes each row and stores it under an
//! auto-increment rowid.
//!
//! # Why tables are reopened per operation
//!
//! An [`MvccTable`] borrows the buffer pool and the transaction manager. If
//! `Database` stored both the pool and a table that borrows it, the struct
//! would be self-referential, which Rust forbids. Instead each table's two
//! anchor pages (the index B+ tree root and the current version heap page)
//! are stored in its [`TableStore`], and a transient `MvccTable` is rebuilt
//! for the duration of each operation via [`MvccTable::open`]. After a write,
//! the (possibly changed) anchor pages are read back and persisted.
//!
//! # Persistence
//!
//! The catalog and the per-table anchor pages are persisted to a sidecar
//! file (see [`crate::persist`]) after each statement that changes the schema
//! or a table's data, and the buffer pool is flushed at the same time. On
//! open the sidecar is read back to rebuild the catalog and descriptors, so a
//! table and its rows survive closing and reopening the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rustdb_executor::eval::{eval, is_truthy};
use rustdb_executor::{decode_row, encode_row, run, Relation, TableSource};
use rustdb_planner::{bind, explain, plan, Catalog};
use rustdb_sql::statement::{ColumnDef, DataType};
use rustdb_sql::{Expr, Parser, Statement, UnOp, Value};
use rustdb_storage::{BufferPool, FileManager, PageId};
use rustdb_txn::{MvccTable, Transaction, TransactionManager};
use rustdb_wal::{WalSyncHandle, WalWriter};

use crate::error::{DbError, Result};
use crate::persist::{self, TableRecord};

/// Buffer pool size in pages. Generous for the capstone's working set.
const POOL_PAGES: usize = 256;

/// Per-table storage descriptor. The catalog holds the logical schema (the
/// column types are derived from it on demand); this holds the physical
/// anchors the engine needs to reopen the table.
#[derive(Debug, Clone)]
struct TableStore {
    /// Root page of the table's index B+ tree.
    index_root: PageId,
    /// Heap page currently receiving new versions.
    version_page: PageId,
    /// Next auto-increment rowid (the `MvccTable` key).
    next_rowid: u64,
}

/// The outcome of running one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryOutcome {
    /// A DDL statement (CREATE / DROP) succeeded.
    Ddl,
    /// A DML statement changed `affected` rows.
    Mutation {
        /// Number of rows affected.
        affected: usize,
    },
    /// A query returned rows.
    Rows {
        /// Output column names.
        columns: Vec<String>,
        /// Result rows, each one value per column.
        rows: Vec<Vec<Value>>,
    },
    /// An `EXPLAIN`: the cost-annotated plan tree, ready to print.
    Explain(String),
}

/// An embedded rustdb instance.
pub struct Database {
    pool: BufferPool,
    wal: WalSyncHandle,
    mgr: TransactionManager,
    catalog: Catalog,
    tables: HashMap<String, TableStore>,
    /// Sidecar file recording the catalog and per-table anchor pages.
    meta_path: PathBuf,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The storage stack is not usefully printable; show the table names.
        f.debug_struct("Database")
            .field("tables", &self.tables.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open (or create) a database at `base`. The data file is `base` and the
    /// write-ahead log is `base` with a `.wal` extension.
    ///
    /// # Errors
    ///
    /// Returns an error if the data file or WAL cannot be opened.
    pub fn open(base: impl AsRef<Path>) -> Result<Self> {
        let base = base.as_ref();
        let wal_path = base.with_extension("wal");
        let meta_path = base.with_extension("meta");
        let writer = WalWriter::open(&wal_path)?;
        let wal = WalSyncHandle::new(writer);
        let file = FileManager::open(base)?;
        let pool = BufferPool::with_wal(file, POOL_PAGES, wal.as_hook());
        let mut db = Self {
            pool,
            wal,
            mgr: TransactionManager::new(),
            catalog: Catalog::new(),
            tables: HashMap::new(),
            meta_path,
        };
        db.load_catalog()?;
        Ok(db)
    }

    /// Rebuild the catalog and table descriptors from the sidecar so the
    /// existing on-disk pages are reachable again.
    fn load_catalog(&mut self) -> Result<()> {
        for r in persist::load(&self.meta_path)? {
            let columns: Vec<ColumnDef> = r
                .columns
                .iter()
                .map(|(name, ty, primary_key)| ColumnDef {
                    name: name.clone(),
                    ty: *ty,
                    primary_key: *primary_key,
                })
                .collect();
            self.catalog.apply(&Statement::CreateTable {
                name: r.name.clone(),
                columns,
            })?;
            for (index, column) in &r.indexes {
                self.catalog.apply(&Statement::CreateIndex {
                    name: index.clone(),
                    table: r.name.clone(),
                    column: column.clone(),
                })?;
            }
            self.catalog.set_row_count(&r.name, r.next_rowid)?;
            self.tables.insert(
                r.name.clone(),
                TableStore {
                    index_root: PageId(r.index_root),
                    version_page: PageId(r.version_page),
                    next_rowid: r.next_rowid,
                },
            );
        }
        Ok(())
    }

    /// Flush every dirty page to the data file and rewrite the catalog
    /// sidecar, so the database is durable across a clean restart. Called
    /// after each statement that changes the schema or a table's data.
    fn persist(&self) -> Result<()> {
        self.pool.flush_all()?;
        let records: Vec<TableRecord> = self
            .tables
            .iter()
            .filter_map(|(name, store)| {
                let meta = self.catalog.get_table(name)?;
                Some(TableRecord {
                    name: name.clone(),
                    columns: meta
                        .columns
                        .iter()
                        .map(|c| (c.name.clone(), c.ty, c.primary_key))
                        .collect(),
                    indexes: meta
                        .indexes
                        .iter()
                        .map(|i| (i.name.clone(), i.column.clone()))
                        .collect(),
                    index_root: store.index_root.0,
                    version_page: store.version_page.0,
                    next_rowid: store.next_rowid,
                })
            })
            .collect();
        persist::save(&self.meta_path, &records)?;
        Ok(())
    }

    /// Parse and run one SQL statement.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement does not parse, names something the
    /// catalog does not have, has a type or arity mismatch, or is a form not
    /// yet supported.
    pub fn execute(&mut self, sql: &str) -> Result<QueryOutcome> {
        let stmt = Parser::from_sql(sql)?.parse_statement()?;
        match stmt {
            Statement::CreateTable { .. } => self.create_table(&stmt),
            Statement::CreateIndex { .. } => {
                self.catalog.apply(&stmt)?;
                self.persist()?;
                Ok(QueryOutcome::Ddl)
            }
            Statement::DropTable { ref name } => self.drop_table(&stmt, name),
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.insert(&table, &columns, &rows),
            Statement::Select(_) => self.run_select(&stmt),
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => self.run_update(&table, &assignments, where_clause.as_ref()),
            Statement::Delete {
                table,
                where_clause,
            } => self.run_delete(&table, where_clause.as_ref()),
            Statement::Explain(_) => self.run_explain(&stmt),
        }
    }

    /// Number of tables currently known to the catalog.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// The names of all tables, sorted, for `\dt`-style listings.
    #[must_use]
    pub fn table_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        names
    }

    /// The `(name, type)` of each column of `table`, for `\d`-style describes.
    #[must_use]
    pub fn columns(&self, table: &str) -> Option<Vec<(String, DataType)>> {
        self.catalog
            .get_table(table)
            .map(|m| m.columns.iter().map(|c| (c.name.clone(), c.ty)).collect())
    }

    // --- statement handlers ---

    fn create_table(&mut self, stmt: &Statement) -> Result<QueryOutcome> {
        let Statement::CreateTable { name, .. } = stmt else {
            unreachable!("guarded by execute");
        };
        // The catalog rejects a duplicate table, keeping it the single source
        // of truth for which tables exist.
        self.catalog.apply(stmt)?;
        let table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;
        let store = TableStore {
            index_root: table.index_root(),
            version_page: table.version_page(),
            next_rowid: 0,
        };
        self.tables.insert(name.clone(), store);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    fn drop_table(&mut self, stmt: &Statement, name: &str) -> Result<QueryOutcome> {
        self.catalog.apply(stmt)?;
        self.tables.remove(name);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    fn insert(
        &mut self,
        table: &str,
        columns: &[String],
        rows: &[Vec<Expr>],
    ) -> Result<QueryOutcome> {
        // Resolve the schema and the column-to-position mapping from the
        // catalog, then drop that borrow before touching the storage fields.
        let (schema, positions) = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
            let positions: Vec<usize> = if columns.is_empty() {
                (0..meta.columns.len()).collect()
            } else {
                columns
                    .iter()
                    .map(|c| {
                        meta.column_index(c).ok_or_else(|| DbError::UnknownColumn {
                            table: table.to_string(),
                            column: c.clone(),
                        })
                    })
                    .collect::<Result<_>>()?
            };
            (schema, positions)
        };

        let store = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let handle = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            store.index_root,
            store.version_page,
        );

        // One transaction for the whole statement: all rows commit together.
        let txn = self.mgr.begin();
        let mut affected = 0;
        for row in rows {
            if row.len() != positions.len() {
                return Err(DbError::ValueCount {
                    expected: positions.len(),
                    got: row.len(),
                });
            }
            // Build a full row, NULL-filling any column not named.
            let mut values = vec![Value::Null; schema.len()];
            for (expr, &pos) in row.iter().zip(&positions) {
                values[pos] = const_eval(expr)?;
            }
            let bytes = encode_row(&values, &schema)?;
            handle.insert(&txn, store.next_rowid, &bytes)?;
            store.next_rowid += 1;
            affected += 1;
        }
        self.mgr.commit(&txn);

        // Persist the (possibly advanced) anchor pages.
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        // Feed the planner real statistics: with no deletes yet, the live row
        // count equals the rowid high-water mark. This is what makes EXPLAIN
        // show true costs instead of zero.
        let row_count = store.next_rowid;
        self.catalog.set_row_count(table, row_count)?;
        self.persist()?;
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_select(&self, stmt: &Statement) -> Result<QueryOutcome> {
        // Bind and plan against the catalog, then run the physical plan over
        // a reader snapshot. Base tables are read through `EngineSource`.
        let logical = bind(&self.catalog, stmt)?;
        let physical = plan(&logical, &self.catalog)?;
        let txn = self.mgr.begin();
        let source = EngineSource {
            pool: &self.pool,
            wal: self.wal.clone(),
            mgr: &self.mgr,
            catalog: &self.catalog,
            tables: &self.tables,
            txn: &txn,
        };
        let (columns, rows) = run(&physical, &source)?;
        self.mgr.commit(&txn);
        Ok(QueryOutcome::Rows { columns, rows })
    }

    fn run_update(
        &mut self,
        table: &str,
        assignments: &[(String, Expr)],
        where_clause: Option<&Expr>,
    ) -> Result<QueryOutcome> {
        let (schema, columns, targets) = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
            let columns: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
            let targets = assignments
                .iter()
                .map(|(col, _)| {
                    meta.column_index(col)
                        .ok_or_else(|| DbError::UnknownColumn {
                            table: table.to_string(),
                            column: col.clone(),
                        })
                })
                .collect::<Result<Vec<usize>>>()?;
            (schema, columns, targets)
        };

        let store = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let handle = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            store.index_root,
            store.version_page,
        );
        let txn = self.mgr.begin();
        let mut affected = 0;
        for (key, bytes) in handle.scan(&txn)? {
            let row = decode_row(&bytes, &schema)?;
            if let Some(pred) = where_clause {
                if !is_truthy(&eval(pred, &row, &columns)?) {
                    continue;
                }
            }
            // Each SET expression is evaluated against the existing row, so
            // `SET n = n + 1` sees the old value.
            let mut new_row = row.clone();
            for ((_, expr), &pos) in assignments.iter().zip(&targets) {
                new_row[pos] = eval(expr, &row, &columns)?;
            }
            handle.update(&txn, key, &encode_row(&new_row, &schema)?)?;
            affected += 1;
        }
        self.mgr.commit(&txn);
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        self.persist()?;
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_delete(&mut self, table: &str, where_clause: Option<&Expr>) -> Result<QueryOutcome> {
        let (schema, columns) = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
            let columns: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
            (schema, columns)
        };

        let store = self
            .tables
            .get_mut(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let handle = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            store.index_root,
            store.version_page,
        );
        let txn = self.mgr.begin();
        let mut affected = 0;
        for (key, bytes) in handle.scan(&txn)? {
            if let Some(pred) = where_clause {
                let row = decode_row(&bytes, &schema)?;
                if !is_truthy(&eval(pred, &row, &columns)?) {
                    continue;
                }
            }
            handle.delete(&txn, key)?;
            affected += 1;
        }
        self.mgr.commit(&txn);
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        self.persist()?;
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_explain(&self, stmt: &Statement) -> Result<QueryOutcome> {
        let Statement::Explain(inner) = stmt else {
            unreachable!("guarded by execute");
        };
        match inner.as_ref() {
            Statement::Select(_) => {
                let logical = bind(&self.catalog, inner)?;
                let physical = plan(&logical, &self.catalog)?;
                Ok(QueryOutcome::Explain(explain(&physical)))
            }
            _ => Err(DbError::Unsupported(
                "EXPLAIN of a non-SELECT statement".into(),
            )),
        }
    }
}

/// Bridges the executor to storage: scans a table's MVCC store under the
/// query's snapshot and decodes each row for the operators above.
struct EngineSource<'a> {
    pool: &'a BufferPool,
    wal: WalSyncHandle,
    mgr: &'a TransactionManager,
    catalog: &'a Catalog,
    tables: &'a HashMap<String, TableStore>,
    txn: &'a Transaction,
}

impl TableSource for EngineSource<'_> {
    fn scan(&self, table: &str) -> std::result::Result<Relation, rustdb_executor::ExecError> {
        use rustdb_executor::ExecError;
        let meta = self
            .catalog
            .get_table(table)
            .ok_or_else(|| ExecError::Source(format!("unknown table {table}")))?;
        let columns: Vec<String> = meta.columns.iter().map(|c| c.name.clone()).collect();
        let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
        let store = self
            .tables
            .get(table)
            .ok_or_else(|| ExecError::Source(format!("no store for table {table}")))?;
        let handle = MvccTable::open(
            self.pool,
            self.wal.clone(),
            self.mgr,
            store.index_root,
            store.version_page,
        );
        let raw = handle
            .scan(self.txn)
            .map_err(|e| ExecError::Source(e.to_string()))?;
        let rows = raw
            .into_iter()
            .map(|(_key, bytes)| decode_row(&bytes, &schema))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Relation { columns, rows })
    }
}

/// Evaluate a constant expression (an `INSERT` value) with no row context.
///
/// Handles literals and unary `-` / `NOT`. Column references and binary
/// arithmetic are deliberately out of scope here; the row-aware evaluator
/// that the executor introduces will supersede this for richer expressions.
fn const_eval(expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Unary {
            op: UnOp::Neg,
            expr,
        } => match const_eval(expr)? {
            Value::Int(n) => Ok(Value::Int(n.wrapping_neg())),
            _ => Err(DbError::Unsupported("unary minus on a non-integer".into())),
        },
        Expr::Unary {
            op: UnOp::Not,
            expr,
        } => match const_eval(expr)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            _ => Err(DbError::Unsupported("NOT on a non-boolean".into())),
        },
        _ => Err(DbError::Unsupported(
            "non-constant expression in INSERT".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_executor::decode_row;
    use tempfile::TempDir;

    fn db() -> (TempDir, Database) {
        let dir = tempfile::tempdir().expect("tempdir");
        let database = Database::open(dir.path().join("test.db")).expect("open");
        (dir, database)
    }

    /// Read every stored row of `table` back through a fresh scan, decoded to
    /// values, in rowid (insertion) order.
    fn dump(db: &Database, table: &str) -> Vec<Vec<Value>> {
        let meta = db.catalog.get_table(table).expect("table meta");
        let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
        let store = db.tables.get(table).expect("table store");
        let handle = MvccTable::open(
            &db.pool,
            db.wal.clone(),
            &db.mgr,
            store.index_root,
            store.version_page,
        );
        let reader = db.mgr.begin();
        handle
            .scan(&reader)
            .expect("scan")
            .into_iter()
            .map(|(_k, bytes)| decode_row(&bytes, &schema).expect("decode"))
            .collect()
    }

    #[test]
    fn create_then_insert_round_trips() {
        let (_dir, mut db) = db();
        assert_eq!(
            db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap(),
            QueryOutcome::Ddl
        );
        assert_eq!(
            db.execute("INSERT INTO t (id, name) VALUES (1, 'alice'), (2, 'bob')")
                .unwrap(),
            QueryOutcome::Mutation { affected: 2 }
        );
        assert_eq!(
            db.execute("INSERT INTO t (id, name) VALUES (3, 'carol')")
                .unwrap(),
            QueryOutcome::Mutation { affected: 1 }
        );

        let rows = dump(&db, "t");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("alice".into())],
                vec![Value::Int(2), Value::Text("bob".into())],
                vec![Value::Int(3), Value::Text("carol".into())],
            ]
        );
    }

    #[test]
    fn insert_column_list_null_fills_unnamed() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t (id) VALUES (7)").unwrap();
        assert_eq!(
            dump(&db, "t"),
            vec![vec![Value::Int(7), Value::Null]],
            "the unnamed TEXT column is NULL"
        );
    }

    #[test]
    fn insert_negative_literal_is_evaluated() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (n INT)").unwrap();
        db.execute("INSERT INTO t (n) VALUES (-42)").unwrap();
        assert_eq!(dump(&db, "t"), vec![vec![Value::Int(-42)]]);
    }

    #[test]
    fn insert_arity_mismatch_errors() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (a INT, b INT)").unwrap();
        let err = db.execute("INSERT INTO t (a, b) VALUES (1)").unwrap_err();
        assert!(matches!(
            err,
            DbError::ValueCount {
                expected: 2,
                got: 1
            }
        ));
    }

    #[test]
    fn insert_type_mismatch_errors() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        // A TEXT value into an INT column is rejected by the row codec.
        let err = db
            .execute("INSERT INTO t (id) VALUES ('nope')")
            .unwrap_err();
        assert!(matches!(err, DbError::Exec(_)), "got {err:?}");
    }

    #[test]
    fn insert_into_unknown_table_errors() {
        let (_dir, mut db) = db();
        let err = db.execute("INSERT INTO ghost (a) VALUES (1)").unwrap_err();
        assert!(matches!(err, DbError::UnknownTable(t) if t == "ghost"));
    }

    #[test]
    fn duplicate_create_errors() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        assert!(db.execute("CREATE TABLE t (id INT)").is_err());
    }

    #[test]
    fn drop_table_removes_it() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        assert_eq!(db.table_count(), 1);
        db.execute("DROP TABLE t").unwrap();
        assert_eq!(db.table_count(), 0);
        // Inserting into the dropped table now fails.
        let err = db.execute("INSERT INTO t (id) VALUES (1)").unwrap_err();
        assert!(matches!(err, DbError::UnknownTable(t) if t == "t"));
    }

    /// Run a query and unwrap its rows.
    fn query(db: &mut Database, sql: &str) -> (Vec<String>, Vec<Vec<Value>>) {
        match db.execute(sql).unwrap() {
            QueryOutcome::Rows { columns, rows } => (columns, rows),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn names(cols: &[String]) -> Vec<&str> {
        cols.iter().map(String::as_str).collect()
    }

    #[test]
    fn select_star_returns_all_rows() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .unwrap();
        let (cols, rows) = query(&mut db, "SELECT * FROM t");
        assert_eq!(names(&cols), ["id", "name"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
                vec![Value::Int(3), Value::Text("c".into())],
            ]
        );
    }

    #[test]
    fn select_where_and_projection() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c')")
            .unwrap();
        let (cols, rows) = query(&mut db, "SELECT name FROM t WHERE id > 1");
        assert_eq!(names(&cols), ["name"]);
        assert_eq!(
            rows,
            vec![vec![Value::Text("b".into())], vec![Value::Text("c".into())],]
        );
    }

    #[test]
    fn select_order_by_desc_and_limit() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t (id) VALUES (2), (5), (1), (4), (3)")
            .unwrap();
        let (_cols, rows) = query(&mut db, "SELECT id FROM t ORDER BY id DESC LIMIT 2");
        assert_eq!(rows, vec![vec![Value::Int(5)], vec![Value::Int(4)]]);
    }

    #[test]
    fn select_with_or_predicate() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t (id) VALUES (1), (2), (3), (4)")
            .unwrap();
        let (_cols, rows) = query(&mut db, "SELECT id FROM t WHERE id = 1 OR id = 4");
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(4)]]);
    }

    #[test]
    fn select_unknown_column_errors() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        assert!(db.execute("SELECT bogus FROM t").is_err());
    }
}
