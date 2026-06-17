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
use rustdb_planner::{bind, explain, plan, Catalog, ColumnStats};
use rustdb_sql::statement::{ColumnDef, DataType, Join, OrderItem, Select, SelectItem, TableRef};
use rustdb_sql::{BinOp, Expr, Parser, Statement, UnOp, Value};
use rustdb_storage::{BufferPool, FileManager, PageId};
use rustdb_txn::{MvccTable, Transaction, TransactionManager};
use rustdb_wal::{WalSyncHandle, WalWriter};

use crate::error::{DbError, Result};
use crate::index::Index;
use crate::persist::{self, TableRecord};

/// Buffer pool size in pages. Generous for the capstone's working set.
const POOL_PAGES: usize = 256;

/// Per-table storage descriptor. The catalog holds the logical schema (the
/// column types are derived from it on demand); this holds the physical
/// anchors the engine needs to reopen the table.
#[derive(Debug, Clone)]
struct TableStore {
    /// Root page of the table's primary index B+ tree (rowid -> version).
    index_root: PageId,
    /// Heap page currently receiving new versions.
    version_page: PageId,
    /// Next auto-increment rowid (the `MvccTable` key).
    next_rowid: u64,
    /// Physical secondary indexes, one per indexed column (unique INT columns).
    secondary: Vec<SecondaryIndex>,
    /// Per-column DEFAULT value (in schema order), used to fill omitted columns
    /// on INSERT. `None` for a column with no default.
    defaults: Vec<Option<Value>>,
}

/// A physical secondary index: the indexed column's position and the root page
/// of its `value -> rowid` B+ tree.
#[derive(Debug, Clone)]
struct SecondaryIndex {
    /// Position of the indexed column in the table's schema.
    column: usize,
    /// Root page of the index B+ tree.
    root: PageId,
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
    /// A transaction-control statement (`BEGIN` / `COMMIT` / `ROLLBACK`).
    Message(&'static str),
}

/// An embedded rustdb instance.
pub struct Database {
    pool: BufferPool,
    wal: WalSyncHandle,
    mgr: TransactionManager,
    catalog: Catalog,
    tables: HashMap<String, TableStore>,
    /// Stored views by name: the defining query, normalized. A view reference
    /// in a FROM/JOIN clause is expanded to this query as a derived table.
    views: HashMap<String, Statement>,
    /// The open explicit transaction, if the session ran `BEGIN`. In
    /// auto-commit mode (no explicit transaction) this is `None` and each
    /// statement runs in its own transaction.
    current_txn: Option<Transaction>,
    /// Sidecar file recording the catalog and per-table anchor pages.
    meta_path: PathBuf,
    /// Sidecar file recording the transaction watermark and aborted xids, so
    /// committed data stays visible across a reopen.
    txn_path: PathBuf,
    /// Sidecar file recording the views as `(name, sql)` pairs.
    view_path: PathBuf,
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
        let txn_path = base.with_extension("txn");
        let view_path = base.with_extension("view");
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
            views: HashMap::new(),
            current_txn: None,
            meta_path,
            txn_path,
            view_path,
        };
        db.load_catalog()?;
        db.load_views()?;
        // Restore the transaction watermark so data committed in a previous
        // session stays visible (its xids would otherwise read as aborted).
        let (next_xid, aborted) = persist::load_txn(&db.txn_path)?;
        db.mgr.recover(next_xid, &aborted);
        Ok(db)
    }

    /// Rebuild the catalog and table descriptors from the sidecar so the
    /// existing on-disk pages are reachable again.
    fn load_catalog(&mut self) -> Result<()> {
        for r in persist::load(&self.meta_path)? {
            let columns: Vec<ColumnDef> = r
                .columns
                .iter()
                .map(
                    |(name, ty, primary_key, not_null, unique, _default)| ColumnDef {
                        name: name.clone(),
                        ty: *ty,
                        primary_key: *primary_key,
                        not_null: *not_null,
                        unique: *unique,
                        // The catalog does not use defaults; the engine restores
                        // them into the table's descriptor from the sidecar.
                        default: None,
                    },
                )
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
            // Rebuild the physical secondary indexes, mapping each stored
            // column name back to its position. Unique columns also carry
            // distinct = row count, so the planner costs index scans the same
            // way it would after a fresh load of rows.
            let mut secondary = Vec::new();
            for (col, root) in &r.secondary {
                if let Some(column) = self
                    .catalog
                    .get_table(&r.name)
                    .and_then(|m| m.column_index(col))
                {
                    secondary.push(SecondaryIndex {
                        column,
                        root: PageId(*root),
                    });
                    self.catalog.set_column_stats(
                        &r.name,
                        col,
                        ColumnStats {
                            distinct: r.next_rowid.max(1),
                        },
                    )?;
                }
            }
            let defaults = r.columns.iter().map(|c| c.5.clone()).collect();
            self.tables.insert(
                r.name.clone(),
                TableStore {
                    index_root: PageId(r.index_root),
                    version_page: PageId(r.version_page),
                    next_rowid: r.next_rowid,
                    secondary,
                    defaults,
                },
            );
        }
        Ok(())
    }

    /// Rebuild the view registry from its sidecar by re-parsing each stored
    /// query. A malformed entry is skipped rather than failing the open.
    fn load_views(&mut self) -> Result<()> {
        for (name, sql) in persist::load_views(&self.view_path)? {
            if let Ok(stmt) = Parser::from_sql(&sql).and_then(|mut p| p.parse_statement()) {
                self.views.insert(name, stmt);
            }
        }
        Ok(())
    }

    /// Write the view registry to its sidecar as `(name, canonical SQL)` pairs.
    fn save_views(&self) -> Result<()> {
        let mut views: Vec<(String, String)> = self
            .views
            .iter()
            .map(|(name, query)| (name.clone(), query.to_string()))
            .collect();
        views.sort();
        persist::save_views(&self.view_path, &views)?;
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
                        .enumerate()
                        .map(|(i, c)| {
                            let default = store.defaults.get(i).cloned().flatten();
                            (
                                c.name.clone(),
                                c.ty,
                                c.primary_key,
                                c.not_null,
                                c.unique,
                                default,
                            )
                        })
                        .collect(),
                    indexes: meta
                        .indexes
                        .iter()
                        .map(|i| (i.name.clone(), i.column.clone()))
                        .collect(),
                    secondary: store
                        .secondary
                        .iter()
                        .filter_map(|s| {
                            meta.columns
                                .get(s.column)
                                .map(|c| (c.name.clone(), s.root.0))
                        })
                        .collect(),
                    index_root: store.index_root.0,
                    version_page: store.version_page.0,
                    next_rowid: store.next_rowid,
                })
            })
            .collect();
        persist::save(&self.meta_path, &records)?;
        // Save the transaction watermark (the next xid) and the aborted set, so
        // a reopen knows which past transactions committed.
        persist::save_txn(
            &self.txn_path,
            self.mgr.next_xid(),
            &self.mgr.aborted_xids(),
        )?;
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
            // Transaction control.
            Statement::Begin => self.begin_txn(),
            Statement::Commit => self.commit_txn(),
            Statement::Rollback => self.rollback_txn(),
            // DDL auto-commits: it persists immediately regardless of any open
            // transaction.
            Statement::CreateTable { .. } => self.create_table(&stmt),
            Statement::CreateIndex { .. } => {
                self.catalog.apply(&stmt)?;
                self.persist()?;
                Ok(QueryOutcome::Ddl)
            }
            Statement::DropTable { ref name } => self.drop_table(&stmt, name),
            Statement::CreateView {
                ref name,
                ref query,
            } => self.create_view(name, query),
            Statement::DropView { ref name } => self.drop_view(name),
            Statement::Truncate { ref table } => self.truncate_table(table),
            Statement::AlterTableAddColumn {
                ref table,
                ref column,
            } => self.alter_add_column(table, column),
            // EXPLAIN only plans; it touches no data.
            Statement::Explain(_) => self.run_explain(&stmt),
            // DML and SELECT run inside the open transaction, or a fresh
            // auto-commit one.
            Statement::Insert { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
            | Statement::Select(_)
            | Statement::Union { .. } => self.run_in_txn(&stmt),
        }
    }

    /// Start an explicit transaction.
    fn begin_txn(&mut self) -> Result<QueryOutcome> {
        if self.current_txn.is_some() {
            return Err(DbError::Unsupported("a transaction is already open".into()));
        }
        self.current_txn = Some(self.mgr.begin());
        Ok(QueryOutcome::Message("BEGIN"))
    }

    /// Commit the open transaction and make its writes durable.
    fn commit_txn(&mut self) -> Result<QueryOutcome> {
        let txn = self
            .current_txn
            .take()
            .ok_or_else(|| DbError::Unsupported("COMMIT without an open transaction".into()))?;
        self.mgr.commit(&txn);
        self.persist()?;
        Ok(QueryOutcome::Message("COMMIT"))
    }

    /// Abort the open transaction; its writes become invisible.
    fn rollback_txn(&mut self) -> Result<QueryOutcome> {
        let txn = self
            .current_txn
            .take()
            .ok_or_else(|| DbError::Unsupported("ROLLBACK without an open transaction".into()))?;
        self.mgr.abort(&txn);
        Ok(QueryOutcome::Message("ROLLBACK"))
    }

    /// Run a DML or SELECT statement inside the open transaction, or, in
    /// auto-commit mode, inside a fresh transaction that is committed and
    /// persisted (or aborted on error) immediately.
    fn run_in_txn(&mut self, stmt: &Statement) -> Result<QueryOutcome> {
        let explicit = self.current_txn.is_some();
        let txn = self.current_txn.take().unwrap_or_else(|| self.mgr.begin());
        let result = self.dispatch(stmt, &txn);
        if explicit {
            // Keep the transaction open; COMMIT/ROLLBACK decide its fate.
            self.current_txn = Some(txn);
            return result;
        }
        match result {
            Ok(outcome) => {
                self.mgr.commit(&txn);
                self.persist()?;
                Ok(outcome)
            }
            Err(e) => {
                self.mgr.abort(&txn);
                Err(e)
            }
        }
    }

    /// Dispatch a DML or SELECT statement against `txn`.
    fn dispatch(&mut self, stmt: &Statement, txn: &Transaction) -> Result<QueryOutcome> {
        match stmt {
            Statement::Insert {
                table,
                columns,
                rows,
            } => self.insert(txn, table, columns, rows),
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => self.run_update(txn, table, assignments, where_clause.as_ref()),
            Statement::Delete {
                table,
                where_clause,
            } => self.run_delete(txn, table, where_clause.as_ref()),
            Statement::Select(_) | Statement::Union { .. } => self.run_select(txn, stmt),
            other => Err(DbError::Unsupported(format!("cannot run: {other}"))),
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
        let Statement::CreateTable { name, columns } = stmt else {
            unreachable!("guarded by execute");
        };
        // The catalog rejects a duplicate table, keeping it the single source
        // of truth for which tables exist.
        self.catalog.apply(stmt)?;
        let table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;

        // Build a physical secondary index for every unique INT column, so an
        // equality lookup on it becomes a point get. Register each in the
        // catalog as well, so the planner can cost and choose an index scan.
        // Uniqueness guarantees the index keys never collide, which is what
        // lets the plain unique-keyed B+ tree serve as the index.
        let mut secondary = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            if col.ty == DataType::Int && (col.primary_key || col.unique) {
                let index = Index::create(&self.pool)?;
                secondary.push(SecondaryIndex {
                    column: i,
                    root: index.root(),
                });
                self.catalog.apply(&Statement::CreateIndex {
                    name: format!("{name}_{}_idx", col.name),
                    table: name.clone(),
                    column: col.name.clone(),
                })?;
            }
        }

        // Const-evaluate each column's DEFAULT now, so INSERT just substitutes
        // the value. A non-constant default (e.g. a function call) is rejected.
        let defaults = columns
            .iter()
            .map(|c| c.default.as_ref().map(const_eval).transpose())
            .collect::<Result<Vec<_>>>()?;

        let store = TableStore {
            index_root: table.index_root(),
            version_page: table.version_page(),
            next_rowid: 0,
            secondary,
            defaults,
        };
        self.tables.insert(name.clone(), store);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Remove every row of `table` by replacing its storage with fresh, empty
    /// pages and resetting its rowid counter and indexes. The schema (and its
    /// column defaults) is untouched.
    fn truncate_table(&mut self, name: &str) -> Result<QueryOutcome> {
        if self.catalog.get_table(name).is_none() {
            return Err(DbError::UnknownTable(name.to_string()));
        }
        let table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;
        let new_index_root = table.index_root();
        let new_version_page = table.version_page();
        let store = self
            .tables
            .get_mut(name)
            .ok_or_else(|| DbError::UnknownTable(name.to_string()))?;
        store.index_root = new_index_root;
        store.version_page = new_version_page;
        store.next_rowid = 0;
        for sec in &mut store.secondary {
            sec.root = Index::create(&self.pool)?.root();
        }
        self.catalog.set_row_count(name, 0)?;
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Append a column to a table, rewriting every existing row into fresh
    /// storage under the new schema (the new column takes its DEFAULT or NULL).
    /// Rewriting into new pages avoids leaving old-schema versions that a later
    /// snapshot could not decode.
    #[allow(clippy::too_many_lines)]
    fn alter_add_column(&mut self, table: &str, col: &ColumnDef) -> Result<QueryOutcome> {
        // 1. Read every current row under the existing schema.
        let old_schema: Vec<DataType> = self
            .catalog
            .get_table(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?
            .columns
            .iter()
            .map(|c| c.ty)
            .collect();
        let reader = self.mgr.begin();
        let old_rows: Vec<Vec<Value>> = {
            let store = self
                .tables
                .get(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            let handle = MvccTable::open(
                &self.pool,
                self.wal.clone(),
                &self.mgr,
                store.index_root,
                store.version_page,
            );
            handle
                .scan(&reader)?
                .into_iter()
                .map(|(_k, bytes)| decode_row(&bytes, &old_schema))
                .collect::<std::result::Result<_, _>>()?
        };
        self.mgr.commit(&reader);

        // 2. Extend the catalog and const-evaluate the new column's default.
        self.catalog.add_column(table, col)?;
        let default = col.default.as_ref().map(const_eval).transpose()?;
        let new_schema: Vec<DataType> = self
            .catalog
            .get_table(table)
            .expect("just added")
            .columns
            .iter()
            .map(|c| c.ty)
            .collect();

        // 3. Fresh storage and a rebuilt secondary index per unique INT column
        //    (registering a catalog index for any newly indexable column).
        let new_table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;
        let cols = self
            .catalog
            .get_table(table)
            .expect("present")
            .columns
            .clone();
        let mut secondary = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            if c.ty == DataType::Int && c.unique {
                if self
                    .catalog
                    .get_table(table)
                    .and_then(|m| m.index_on(&c.name))
                    .is_none()
                {
                    self.catalog.apply(&Statement::CreateIndex {
                        name: format!("{table}_{}_idx", c.name),
                        table: table.to_string(),
                        column: c.name.clone(),
                    })?;
                }
                secondary.push(SecondaryIndex {
                    column: i,
                    root: Index::create(&self.pool)?.root(),
                });
            }
        }

        // 4. Re-insert each row with the new column appended.
        let writer = self.mgr.begin();
        let mut rowid: u64 = 0;
        for mut values in old_rows {
            values.push(default.clone().unwrap_or(Value::Null));
            let bytes = encode_row(&values, &new_schema)?;
            new_table.insert(&writer, rowid, &bytes)?;
            for sec in &mut secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&values[sec.column], rowid)?;
                sec.root = index.root();
            }
            rowid += 1;
        }
        self.mgr.commit(&writer);

        // 5. Swap in the new anchors and persist.
        let index_root = new_table.index_root();
        let version_page = new_table.version_page();
        let store = self.tables.get_mut(table).expect("present");
        store.index_root = index_root;
        store.version_page = version_page;
        store.next_rowid = rowid;
        store.secondary = secondary;
        store.defaults.push(default);
        self.catalog.set_row_count(table, rowid)?;
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    fn drop_table(&mut self, stmt: &Statement, name: &str) -> Result<QueryOutcome> {
        self.catalog.apply(stmt)?;
        self.tables.remove(name);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Register a view: store its defining query under `name`.
    ///
    /// The query is validated (expanded against existing views, subqueries
    /// folded, then bound) before it is stored, so a broken definition is
    /// rejected at creation time as in Postgres. The raw query is kept, not the
    /// folded one, so the view re-evaluates against current data on every
    /// reference.
    fn create_view(&mut self, name: &str, query: &Statement) -> Result<QueryOutcome> {
        if self.tables.contains_key(name) || self.views.contains_key(name) {
            return Err(DbError::Constraint(format!(
                "a table or view named {name} already exists"
            )));
        }
        // Bind the query (with views expanded and subqueries folded) in a
        // throwaway read transaction to surface unknown tables or columns now.
        let txn = self.mgr.begin();
        let validated = self
            .fold_query(&txn, query)
            .and_then(|folded| bind(&self.catalog, &folded).map_err(DbError::from));
        self.mgr.abort(&txn);
        validated?;
        self.views.insert(name.to_string(), query.clone());
        self.save_views()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Remove a view. Errors if no view by that name exists.
    fn drop_view(&mut self, name: &str) -> Result<QueryOutcome> {
        if self.views.remove(name).is_none() {
            return Err(DbError::Constraint(format!("view {name} does not exist")));
        }
        self.save_views()?;
        Ok(QueryOutcome::Ddl)
    }

    #[allow(clippy::too_many_lines)]
    fn insert(
        &mut self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        rows: &[Vec<Expr>],
    ) -> Result<QueryOutcome> {
        // Resolve the schema, the column-to-position mapping, and the column
        // constraints from the catalog, then drop that borrow.
        let (schema, positions, col_meta) = {
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
            // (name, not_null, unique) in column order.
            let col_meta: Vec<(String, bool, bool)> = meta
                .columns
                .iter()
                .map(|c| (c.name.clone(), c.not_null, c.unique))
                .collect();
            (schema, positions, col_meta)
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

        // For each UNIQUE column, gather the values already present so
        // duplicates (including duplicates within this statement) are caught.
        let unique_cols: Vec<usize> = col_meta
            .iter()
            .enumerate()
            .filter_map(|(i, (_, _, unique))| unique.then_some(i))
            .collect();
        let mut seen: Vec<Vec<Value>> = vec![Vec::new(); unique_cols.len()];
        if !unique_cols.is_empty() {
            for (_, bytes) in handle.scan(txn)? {
                let row = decode_row(&bytes, &schema)?;
                for (slot, &col) in unique_cols.iter().enumerate() {
                    if !matches!(row[col], Value::Null) {
                        seen[slot].push(row[col].clone());
                    }
                }
            }
        }

        let mut affected = 0;
        for row in rows {
            if row.len() != positions.len() {
                return Err(DbError::ValueCount {
                    expected: positions.len(),
                    got: row.len(),
                });
            }
            // Build a full row: each column starts at its DEFAULT (NULL when it
            // has none), then the named columns are overwritten.
            let mut values: Vec<Value> = (0..schema.len())
                .map(|i| {
                    store
                        .defaults
                        .get(i)
                        .cloned()
                        .flatten()
                        .unwrap_or(Value::Null)
                })
                .collect();
            for (expr, &pos) in row.iter().zip(&positions) {
                values[pos] = const_eval(expr)?;
            }
            // NOT NULL.
            for (i, (name, not_null, _)) in col_meta.iter().enumerate() {
                if *not_null && matches!(values[i], Value::Null) {
                    return Err(DbError::Constraint(format!("column {name} cannot be NULL")));
                }
            }
            // UNIQUE (NULLs do not conflict).
            for (slot, &col) in unique_cols.iter().enumerate() {
                if !matches!(values[col], Value::Null) {
                    if seen[slot].contains(&values[col]) {
                        return Err(DbError::Constraint(format!(
                            "duplicate value in column {}",
                            col_meta[col].0
                        )));
                    }
                    seen[slot].push(values[col].clone());
                }
            }
            let rowid = store.next_rowid;
            let bytes = encode_row(&values, &schema)?;
            handle.insert(txn, rowid, &bytes)?;
            // Maintain the physical secondary indexes: record value -> rowid.
            for sec in &mut store.secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&values[sec.column], rowid)?;
                sec.root = index.root();
            }
            store.next_rowid += 1;
            affected += 1;
        }

        // Persist the (possibly advanced) anchor pages.
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        // Feed the planner real statistics: with no deletes yet, the live row
        // count equals the rowid high-water mark. This is what makes EXPLAIN
        // show true costs instead of zero.
        let row_count = store.next_rowid;
        let indexed_cols: Vec<usize> = store.secondary.iter().map(|s| s.column).collect();
        self.catalog.set_row_count(table, row_count)?;
        // Each indexed column is unique, so its distinct count is the row count.
        // That low equality selectivity is what lets the planner cost an index
        // scan below a sequential scan once the table is large enough.
        for col in indexed_cols {
            let name = col_meta[col].0.clone();
            self.catalog.set_column_stats(
                table,
                &name,
                ColumnStats {
                    distinct: row_count.max(1),
                },
            )?;
        }
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_select(&self, txn: &Transaction, stmt: &Statement) -> Result<QueryOutcome> {
        // Replace uncorrelated subqueries with their results, then plan and run
        // the now subquery-free query against the transaction's snapshot.
        let folded = self.fold_query(txn, stmt)?;
        let (columns, rows) = self.execute_query(txn, &folded)?;
        Ok(QueryOutcome::Rows { columns, rows })
    }

    /// Bind, plan, and run a query, returning its columns and rows. Base tables
    /// are read through `EngineSource` under `txn`'s snapshot.
    fn execute_query(
        &self,
        txn: &Transaction,
        stmt: &Statement,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let logical = bind(&self.catalog, stmt)?;
        let physical = plan(&logical, &self.catalog)?;
        let source = EngineSource {
            pool: &self.pool,
            wal: self.wal.clone(),
            mgr: &self.mgr,
            catalog: &self.catalog,
            tables: &self.tables,
            txn,
        };
        Ok(run(&physical, &source)?)
    }

    /// Rewrite a query, replacing every uncorrelated subquery with its result
    /// (a scalar becomes a literal; `IN (subquery)` becomes an `IN`-list).
    fn fold_query(&self, txn: &Transaction, stmt: &Statement) -> Result<Statement> {
        match stmt {
            Statement::Select(s) => Ok(Statement::Select(Box::new(self.fold_select(txn, s)?))),
            Statement::Union {
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => Ok(Statement::Union {
                all: *all,
                left: Box::new(self.fold_query(txn, left)?),
                right: Box::new(self.fold_query(txn, right)?),
                order_by: self.fold_order_keys(txn, order_by)?,
                limit: *limit,
                offset: *offset,
            }),
            other => Ok(other.clone()),
        }
    }

    fn fold_select(&self, txn: &Transaction, s: &Select) -> Result<Select> {
        let projections = s
            .projections
            .iter()
            .map(|p| match p {
                SelectItem::Star => Ok(SelectItem::Star),
                SelectItem::Expr(e, alias) => {
                    Ok(SelectItem::Expr(self.fold_expr(txn, e)?, alias.clone()))
                }
            })
            .collect::<Result<_>>()?;
        let joins = s
            .joins
            .iter()
            .map(|j| {
                Ok(Join {
                    kind: j.kind,
                    table: self.fold_table_ref(txn, &j.table)?,
                    on: self.fold_expr(txn, &j.on)?,
                })
            })
            .collect::<Result<_>>()?;
        Ok(Select {
            distinct: s.distinct,
            projections,
            from: self.fold_table_ref(txn, &s.from)?,
            joins,
            where_clause: s
                .where_clause
                .as_ref()
                .map(|w| self.fold_expr(txn, w))
                .transpose()?,
            group_by: s
                .group_by
                .iter()
                .map(|g| self.fold_expr(txn, g))
                .collect::<Result<_>>()?,
            having: s
                .having
                .as_ref()
                .map(|h| self.fold_expr(txn, h))
                .transpose()?,
            order_by: self.fold_order_keys(txn, &s.order_by)?,
            limit: s.limit,
            offset: s.offset,
        })
    }

    /// Fold any subqueries nested inside a `FROM`/join relation, and expand a
    /// view reference to a derived table over the view's defining query.
    ///
    /// A derived table `(SELECT ...) AS x` has its inner query folded so its own
    /// uncorrelated subqueries are resolved before binding. A bare name that
    /// matches a view is rewritten as `(<view query>) AS <alias>`, where the
    /// alias is the one written in the query or, failing that, the view name.
    /// The view's query is itself folded, so views may reference other views
    /// and contain their own uncorrelated subqueries.
    fn fold_table_ref(&self, txn: &Transaction, table: &TableRef) -> Result<TableRef> {
        // An explicit derived table: fold its inner query.
        if let Some(q) = &table.subquery {
            return Ok(TableRef {
                name: table.name.clone(),
                alias: table.alias.clone(),
                subquery: Some(Box::new(self.fold_query(txn, q)?)),
            });
        }
        // A view reference expands to a derived table over the view's query.
        if let Some(view) = self.views.get(&table.name) {
            let expanded = self.fold_query(txn, view)?;
            let alias = table.alias.clone().unwrap_or_else(|| table.name.clone());
            return Ok(TableRef {
                name: String::new(),
                alias: Some(alias),
                subquery: Some(Box::new(expanded)),
            });
        }
        // A plain base table.
        Ok(table.clone())
    }

    fn fold_order_keys(&self, txn: &Transaction, keys: &[OrderItem]) -> Result<Vec<OrderItem>> {
        keys.iter()
            .map(|o| {
                Ok(OrderItem {
                    expr: self.fold_expr(txn, &o.expr)?,
                    desc: o.desc,
                })
            })
            .collect()
    }

    /// Recursively rewrite subqueries inside an expression.
    fn fold_expr(&self, txn: &Transaction, expr: &Expr) -> Result<Expr> {
        match expr {
            Expr::Subquery(q) => Ok(Expr::Literal(self.scalar_subquery(txn, q)?)),
            Expr::InSubquery {
                expr,
                query,
                negated,
            } => {
                let lhs = self.fold_expr(txn, expr)?;
                let values = self.column_subquery(txn, query)?;
                Ok(in_list_expr(&lhs, &values, *negated))
            }
            Expr::Exists(query) => {
                let folded = self.fold_query(txn, query)?;
                let (_cols, rows) = self.execute_query(txn, &folded)?;
                Ok(Expr::Literal(Value::Bool(!rows.is_empty())))
            }
            Expr::Binary { op, left, right } => Ok(Expr::Binary {
                op: *op,
                left: Box::new(self.fold_expr(txn, left)?),
                right: Box::new(self.fold_expr(txn, right)?),
            }),
            Expr::Unary { op, expr } => Ok(Expr::Unary {
                op: *op,
                expr: Box::new(self.fold_expr(txn, expr)?),
            }),
            Expr::Func {
                name,
                distinct,
                args,
            } => Ok(Expr::Func {
                name: name.clone(),
                distinct: *distinct,
                args: args
                    .iter()
                    .map(|a| self.fold_expr(txn, a))
                    .collect::<Result<_>>()?,
            }),
            Expr::Case {
                operand,
                whens,
                else_result,
            } => Ok(Expr::Case {
                operand: operand
                    .as_ref()
                    .map(|o| self.fold_expr(txn, o).map(Box::new))
                    .transpose()?,
                whens: whens
                    .iter()
                    .map(|(w, t)| Ok((self.fold_expr(txn, w)?, self.fold_expr(txn, t)?)))
                    .collect::<Result<_>>()?,
                else_result: else_result
                    .as_ref()
                    .map(|e| self.fold_expr(txn, e).map(Box::new))
                    .transpose()?,
            }),
            // Leaves carry no nested expressions.
            Expr::Column(_) | Expr::QualifiedColumn(..) | Expr::Literal(_) | Expr::Star => {
                Ok(expr.clone())
            }
        }
    }

    /// Run a scalar subquery and return its single value (NULL if it returns no
    /// rows). Errors if it returns more than one column or row.
    fn scalar_subquery(&self, txn: &Transaction, stmt: &Statement) -> Result<Value> {
        let folded = self.fold_query(txn, stmt)?;
        let (columns, mut rows) = self.execute_query(txn, &folded)?;
        if columns.len() != 1 {
            return Err(DbError::Unsupported(
                "a scalar subquery must return exactly one column".into(),
            ));
        }
        match rows.len() {
            0 => Ok(Value::Null),
            1 => Ok(rows.remove(0).remove(0)),
            _ => Err(DbError::Unsupported(
                "a scalar subquery returned more than one row".into(),
            )),
        }
    }

    /// Run a one-column subquery and return all its values (for `IN`).
    fn column_subquery(&self, txn: &Transaction, stmt: &Statement) -> Result<Vec<Value>> {
        let folded = self.fold_query(txn, stmt)?;
        let (columns, rows) = self.execute_query(txn, &folded)?;
        if columns.len() != 1 {
            return Err(DbError::Unsupported(
                "an IN subquery must return exactly one column".into(),
            ));
        }
        Ok(rows.into_iter().map(|mut r| r.remove(0)).collect())
    }

    fn run_update(
        &mut self,
        txn: &Transaction,
        table: &str,
        assignments: &[(String, Expr)],
        where_clause: Option<&Expr>,
    ) -> Result<QueryOutcome> {
        let (schema, columns, targets, not_null) = {
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
            let not_null: Vec<usize> = meta
                .columns
                .iter()
                .enumerate()
                .filter_map(|(i, c)| c.not_null.then_some(i))
                .collect();
            (schema, columns, targets, not_null)
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
        let mut affected = 0;
        for (key, bytes) in handle.scan(txn)? {
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
            for &col in &not_null {
                if matches!(new_row[col], Value::Null) {
                    return Err(DbError::Constraint(format!(
                        "column {} cannot be NULL",
                        columns[col]
                    )));
                }
            }
            handle.update(txn, key, &encode_row(&new_row, &schema)?)?;
            // Point each indexed column's key at this rowid's new value. Old
            // values are left in the tree (upsert only, never delete) and are
            // filtered out on read; see `crate::index`.
            for sec in &mut store.secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&new_row[sec.column], key)?;
                sec.root = index.root();
            }
            affected += 1;
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_delete(
        &mut self,
        txn: &Transaction,
        table: &str,
        where_clause: Option<&Expr>,
    ) -> Result<QueryOutcome> {
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
        let mut affected = 0;
        for (key, bytes) in handle.scan(txn)? {
            if let Some(pred) = where_clause {
                let row = decode_row(&bytes, &schema)?;
                if !is_truthy(&eval(pred, &row, &columns)?) {
                    continue;
                }
            }
            handle.delete(txn, key)?;
            affected += 1;
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        Ok(QueryOutcome::Mutation { affected })
    }

    fn run_explain(&self, stmt: &Statement) -> Result<QueryOutcome> {
        let Statement::Explain(inner) = stmt else {
            unreachable!("guarded by execute");
        };
        match inner.as_ref() {
            Statement::Select(_) | Statement::Union { .. } => {
                // Fold subqueries under a transient read snapshot so EXPLAIN
                // plans the same query the executor would run.
                let txn = self.mgr.begin();
                let folded = self.fold_query(&txn, inner)?;
                let logical = bind(&self.catalog, &folded)?;
                let physical = plan(&logical, &self.catalog)?;
                Ok(QueryOutcome::Explain(explain(&physical)))
            }
            _ => Err(DbError::Unsupported(
                "EXPLAIN of a non-query statement".into(),
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

    fn index_scan(
        &self,
        table: &str,
        _index: &str,
        predicate: &Expr,
    ) -> std::result::Result<Relation, rustdb_executor::ExecError> {
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

        // Find a physical index whose column the predicate constrains with an
        // equality, then turn that into a point get. The matched row is a
        // candidate only: the executor re-applies `predicate` as a residual
        // filter, so a stale index entry is filtered, never returned wrong.
        for sec in &store.secondary {
            let col_name = &meta.columns[sec.column].name;
            let Some(value) = find_equality(predicate, col_name) else {
                continue;
            };
            let index = Index::open(self.pool, sec.root);
            let rowid = index
                .lookup(&value)
                .map_err(|e| ExecError::Source(e.to_string()))?;
            let mvcc = MvccTable::open(
                self.pool,
                self.wal.clone(),
                self.mgr,
                store.index_root,
                store.version_page,
            );
            let rows = match rowid {
                Some(r) => match mvcc
                    .get(self.txn, r)
                    .map_err(|e| ExecError::Source(e.to_string()))?
                {
                    Some(bytes) => vec![decode_row(&bytes, &schema)?],
                    None => Vec::new(),
                },
                None => Vec::new(),
            };
            return Ok(Relation { columns, rows });
        }

        // No physical index matched this predicate: a full scan is still
        // correct (the executor's residual filter does the rest).
        self.scan(table)
    }
}

/// Find a constant `value` such that the predicate constrains `col` with
/// `col = value` (in either operand order), descending through `AND`. Returns
/// `None` if there is no such equality, which makes the caller fall back to a
/// full scan.
fn find_equality(predicate: &Expr, col: &str) -> Option<Value> {
    match predicate {
        Expr::Binary {
            op: BinOp::Eq,
            left,
            right,
        } => {
            if expr_is_column(left, col) {
                const_eval(right).ok()
            } else if expr_is_column(right, col) {
                const_eval(left).ok()
            } else {
                None
            }
        }
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => find_equality(left, col).or_else(|| find_equality(right, col)),
        _ => None,
    }
}

/// Desugar `lhs [NOT] IN (v1, v2, ...)` to a chain of equalities, the same
/// shape the parser produces for a literal `IN`-list. An empty set is a
/// constant: `IN ()` is false, `NOT IN ()` is true.
fn in_list_expr(lhs: &Expr, values: &[Value], negated: bool) -> Expr {
    if values.is_empty() {
        return Expr::Literal(Value::Bool(negated));
    }
    let (cmp, join) = if negated {
        (BinOp::Ne, BinOp::And)
    } else {
        (BinOp::Eq, BinOp::Or)
    };
    let eq = |v: &Value| Expr::Binary {
        op: cmp,
        left: Box::new(lhs.clone()),
        right: Box::new(Expr::Literal(v.clone())),
    };
    let mut iter = values.iter();
    let mut acc = eq(iter.next().expect("non-empty"));
    for v in iter {
        acc = Expr::Binary {
            op: join,
            left: Box::new(acc),
            right: Box::new(eq(v)),
        };
    }
    acc
}

/// Whether `expr` is a reference (bare or qualified) to the column `col`.
fn expr_is_column(expr: &Expr, col: &str) -> bool {
    match expr {
        Expr::Column(c) | Expr::QualifiedColumn(_, c) => c == col,
        _ => false,
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
            Value::Float(x) => Ok(Value::Float(-x)),
            _ => Err(DbError::Unsupported("unary minus on a non-number".into())),
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

    // --- secondary index ---

    /// Fill `t(id INT PRIMARY KEY, name TEXT)` with `n` rows `(i, "n{i}")`,
    /// each in its own auto-commit transaction.
    fn seed_indexed(db: &mut Database, n: i64) {
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        for i in 0..n {
            db.execute(&format!("INSERT INTO t VALUES ({i}, 'n{i}')"))
                .unwrap();
        }
    }

    fn explain(db: &mut Database, select_sql: &str) -> String {
        match db.execute(&format!("EXPLAIN {select_sql}")).unwrap() {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        }
    }

    #[test]
    fn index_scan_is_chosen_and_returns_the_right_row() {
        let (_dir, mut db) = db();
        seed_indexed(&mut db, 300);
        // The planner picks the index for a selective equality on the key.
        let plan = explain(&mut db, "SELECT name FROM t WHERE id = 137");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        // The index path resolves the row.
        let (cols, rows) = query(&mut db, "SELECT id, name FROM t WHERE id = 137");
        assert_eq!(names(&cols), ["id", "name"]);
        assert_eq!(
            rows,
            vec![vec![Value::Int(137), Value::Text("n137".into())]]
        );
        // A key with no row returns nothing.
        let (_c, miss) = query(&mut db, "SELECT id FROM t WHERE id = 99999");
        assert!(miss.is_empty());
    }

    #[test]
    fn index_lookup_handles_an_extra_conjunct() {
        let (_dir, mut db) = db();
        seed_indexed(&mut db, 300);
        // The index drives off `id = 5`; the residual `name = ...` still applies.
        let (_c, hit) = query(&mut db, "SELECT id FROM t WHERE id = 5 AND name = 'n5'");
        assert_eq!(hit, vec![vec![Value::Int(5)]]);
        let (_c, miss) = query(&mut db, "SELECT id FROM t WHERE id = 5 AND name = 'wrong'");
        assert!(miss.is_empty());
    }

    #[test]
    fn index_reflects_updates_via_upsert() {
        let (_dir, mut db) = db();
        // 300 separate inserts: many transactions and a version page that rolls
        // over, exercising both the durability and same-length-update fixes.
        seed_indexed(&mut db, 300);
        // The post-update reads go through the index (confirm the plan).
        assert!(explain(&mut db, "SELECT id FROM t WHERE id = 1000").contains("IndexScan"));
        // Move row 5 to a new key.
        db.execute("UPDATE t SET id = 1000 WHERE id = 5").unwrap();
        // The old key resolves a candidate whose value no longer matches, so
        // the residual filter drops it: no row.
        let (_c, old) = query(&mut db, "SELECT id FROM t WHERE id = 5");
        assert!(old.is_empty(), "old key should be gone, got {old:?}");
        // The new key resolves the moved row through the index.
        let (_c, moved) = query(&mut db, "SELECT name FROM t WHERE id = 1000");
        assert_eq!(moved, vec![vec![Value::Text("n5".into())]]);
    }

    #[test]
    fn index_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("idx.db");
        {
            let mut db = Database::open(&path).expect("open");
            seed_indexed(&mut db, 300);
        }
        // Reopen: the persisted index roots are reloaded.
        let mut db = Database::open(&path).expect("reopen");
        let plan = explain(&mut db, "SELECT name FROM t WHERE id = 42");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        let (_c, rows) = query(&mut db, "SELECT name FROM t WHERE id = 42");
        assert_eq!(rows, vec![vec![Value::Text("n42".into())]]);
    }

    // --- transaction durability across reopen ---

    #[test]
    fn data_from_many_transactions_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
            // Each insert is its own auto-commit transaction, so the rows span
            // 50 distinct xids. Before the watermark fix, reopening made all of
            // them invisible (their xids read as aborted).
            for i in 0..50i64 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, 'n{i}')"))
                    .unwrap();
            }
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id FROM t");
        assert_eq!(rows.len(), 50, "rows from every transaction should survive");
        let (_c, one) = query(&mut db, "SELECT name FROM t WHERE id = 30");
        assert_eq!(one, vec![vec![Value::Text("n30".into())]]);
    }

    #[test]
    fn rolled_back_data_does_not_reappear_after_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("r.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1)").unwrap(); // committed
            db.execute("BEGIN").unwrap();
            db.execute("INSERT INTO t VALUES (999)").unwrap(); // to be rolled back
            db.execute("ROLLBACK").unwrap();
            // This commit's flush writes the rolled-back version's page to disk
            // and persists the aborted xid, so the reopen must hide it by xid.
            db.execute("INSERT INTO t VALUES (2)").unwrap(); // committed
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id FROM t ORDER BY id");
        assert_eq!(
            rows,
            vec![vec![Value::Int(1)], vec![Value::Int(2)]],
            "the rolled-back row 999 must not reappear"
        );
    }

    // --- FLOAT and BOOL column types ---

    fn approx(rows: &[Vec<Value>], col: usize) -> Vec<f64> {
        rows.iter()
            .map(|r| match r[col] {
                Value::Float(x) => x,
                ref other => panic!("expected float, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn float_and_bool_round_trip_through_sql() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE p (name TEXT, price FLOAT, active BOOL)")
            .unwrap();
        db.execute("INSERT INTO p VALUES ('a', 9.99, TRUE), ('b', 14.5, FALSE), ('c', -2.0, TRUE)")
            .unwrap();
        let (cols, rows) = query(&mut db, "SELECT name, price, active FROM p ORDER BY name");
        assert_eq!(names(&cols), ["name", "price", "active"]);
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Text("a".into()),
                    Value::Float(9.99),
                    Value::Bool(true)
                ],
                vec![
                    Value::Text("b".into()),
                    Value::Float(14.5),
                    Value::Bool(false)
                ],
                vec![
                    Value::Text("c".into()),
                    Value::Float(-2.0),
                    Value::Bool(true)
                ],
            ]
        );
    }

    #[test]
    fn float_predicate_and_order_by() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE p (name TEXT, price FLOAT)")
            .unwrap();
        db.execute("INSERT INTO p VALUES ('a', 9.99), ('b', 14.5), ('c', 3.25)")
            .unwrap();
        // A float comparison, and a mixed float-vs-int comparison.
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM p WHERE price > 5.0 ORDER BY price",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Text("a".into())], vec![Value::Text("b".into())]]
        );
        let (_c, rows2) = query(
            &mut db,
            "SELECT name FROM p WHERE price < 10 ORDER BY price DESC",
        );
        assert_eq!(
            rows2,
            vec![vec![Value::Text("a".into())], vec![Value::Text("c".into())]]
        );
    }

    #[test]
    fn bool_predicate_filters() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE u (name TEXT, active BOOL)")
            .unwrap();
        db.execute("INSERT INTO u VALUES ('a', TRUE), ('b', FALSE), ('c', TRUE)")
            .unwrap();
        let (_c, rows) = query(&mut db, "SELECT name FROM u WHERE active ORDER BY name");
        assert_eq!(
            rows,
            vec![vec![Value::Text("a".into())], vec![Value::Text("c".into())]]
        );
        let (_c, rows2) = query(&mut db, "SELECT name FROM u WHERE NOT active");
        assert_eq!(rows2, vec![vec![Value::Text("b".into())]]);
    }

    #[test]
    fn float_arithmetic_and_int_promotion() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE p (price FLOAT, qty INT)").unwrap();
        db.execute("INSERT INTO p VALUES (2.5, 4)").unwrap();
        // float * int -> float; int / int -> int stays int.
        let (_c, rows) = query(&mut db, "SELECT price * qty FROM p");
        assert_eq!(approx(&rows, 0), vec![10.0]);
        let (_c, rows2) = query(&mut db, "SELECT price + 1 FROM p");
        assert_eq!(approx(&rows2, 0), vec![3.5]);
    }

    #[test]
    fn float_aggregates() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE p (g TEXT, price FLOAT)").unwrap();
        db.execute("INSERT INTO p VALUES ('x', 1.0), ('x', 2.0), ('y', 10.5)")
            .unwrap();
        let (cols, rows) = query(
            &mut db,
            "SELECT g, SUM(price), AVG(price), MIN(price), MAX(price) FROM p GROUP BY g ORDER BY g",
        );
        assert_eq!(
            names(&cols),
            ["g", "SUM(price)", "AVG(price)", "MIN(price)", "MAX(price)"]
        );
        // group x: sum 3.0, avg 1.5, min 1.0, max 2.0
        assert_eq!(rows[0][0], Value::Text("x".into()));
        assert_eq!(rows[0][1], Value::Float(3.0));
        assert_eq!(rows[0][2], Value::Float(1.5));
        assert_eq!(rows[0][3], Value::Float(1.0));
        assert_eq!(rows[0][4], Value::Float(2.0));
        // group y: single row
        assert_eq!(rows[1][1], Value::Float(10.5));
    }

    #[test]
    fn float_and_bool_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ty.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE p (price FLOAT, active BOOL)")
                .unwrap();
            db.execute("INSERT INTO p VALUES (1.5, TRUE), (0.25, FALSE)")
                .unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT price, active FROM p ORDER BY price DESC");
        assert_eq!(
            rows,
            vec![
                vec![Value::Float(1.5), Value::Bool(true)],
                vec![Value::Float(0.25), Value::Bool(false)],
            ]
        );
    }

    // --- predicates: IN, BETWEEN, LIKE, IS NULL ---

    fn seed_fruit(db: &mut Database) {
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute(
            "INSERT INTO t VALUES (1, 'apple'), (2, 'banana'), (3, 'cherry'), (4, 'avocado')",
        )
        .unwrap();
    }

    fn id_set(rows: &[Vec<Value>]) -> Vec<i64> {
        rows.iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref o => panic!("want int, got {o:?}"),
            })
            .collect()
    }

    #[test]
    fn in_and_not_in() {
        let (_d, mut db) = db();
        seed_fruit(&mut db);
        let (_c, rows) = query(&mut db, "SELECT id FROM t WHERE id IN (1, 3) ORDER BY id");
        assert_eq!(id_set(&rows), vec![1, 3]);
        let (_c, rows2) = query(
            &mut db,
            "SELECT id FROM t WHERE id NOT IN (1, 3) ORDER BY id",
        );
        assert_eq!(id_set(&rows2), vec![2, 4]);
    }

    #[test]
    fn between_and_not_between() {
        let (_d, mut db) = db();
        seed_fruit(&mut db);
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE id BETWEEN 2 AND 3 ORDER BY id",
        );
        assert_eq!(id_set(&rows), vec![2, 3]);
        let (_c, rows2) = query(
            &mut db,
            "SELECT id FROM t WHERE id NOT BETWEEN 2 AND 3 ORDER BY id",
        );
        assert_eq!(id_set(&rows2), vec![1, 4]);
    }

    #[test]
    fn like_and_not_like() {
        let (_d, mut db) = db();
        seed_fruit(&mut db);
        // a% -> apple, avocado
        let (_c, rows) = query(&mut db, "SELECT id FROM t WHERE name LIKE 'a%' ORDER BY id");
        assert_eq!(id_set(&rows), vec![1, 4]);
        // %a_a -> banana (ends 'a','n','a'? no) ; use %rr% style: cherry has 'rr'
        let (_c, rows2) = query(&mut db, "SELECT id FROM t WHERE name LIKE '%rr%'");
        assert_eq!(id_set(&rows2), vec![3]);
        // _pple -> apple
        let (_c, rows3) = query(&mut db, "SELECT id FROM t WHERE name LIKE '_pple'");
        assert_eq!(id_set(&rows3), vec![1]);
        // NOT LIKE 'a%' -> banana, cherry
        let (_c, rows4) = query(
            &mut db,
            "SELECT id FROM t WHERE name NOT LIKE 'a%' ORDER BY id",
        );
        assert_eq!(id_set(&rows4), vec![2, 3]);
    }

    #[test]
    fn is_null_and_is_not_null() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, note TEXT)").unwrap();
        db.execute("INSERT INTO t (id, note) VALUES (1, 'x')")
            .unwrap();
        db.execute("INSERT INTO t (id) VALUES (2)").unwrap(); // note defaults to NULL
        db.execute("INSERT INTO t (id, note) VALUES (3, 'y')")
            .unwrap();
        let (_c, nulls) = query(&mut db, "SELECT id FROM t WHERE note IS NULL");
        assert_eq!(id_set(&nulls), vec![2]);
        let (_c, present) = query(
            &mut db,
            "SELECT id FROM t WHERE note IS NOT NULL ORDER BY id",
        );
        assert_eq!(id_set(&present), vec![1, 3]);
    }

    #[test]
    fn predicates_compose_with_and_or() {
        let (_d, mut db) = db();
        seed_fruit(&mut db);
        // (id IN (1,2,4)) AND (name LIKE 'a%') -> apple(1), avocado(4)
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE id IN (1, 2, 4) AND name LIKE 'a%' ORDER BY id",
        );
        assert_eq!(id_set(&rows), vec![1, 4]);
        // id BETWEEN 1 AND 2 OR id = 4
        let (_c, rows2) = query(
            &mut db,
            "SELECT id FROM t WHERE id BETWEEN 1 AND 2 OR id = 4 ORDER BY id",
        );
        assert_eq!(id_set(&rows2), vec![1, 2, 4]);
    }

    // --- SELECT DISTINCT and HAVING ---

    fn seed_groups(db: &mut Database) {
        db.execute("CREATE TABLE t (g TEXT, n INT)").unwrap();
        db.execute(
            "INSERT INTO t VALUES ('a', 1), ('a', 2), ('b', 3), ('b', 4), ('b', 5), ('c', 6)",
        )
        .unwrap();
    }

    #[test]
    fn distinct_single_and_multi_column() {
        let (_d, mut db) = db();
        seed_groups(&mut db);
        let (_c, rows) = query(&mut db, "SELECT DISTINCT g FROM t ORDER BY g");
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".into())],
                vec![Value::Text("b".into())],
                vec![Value::Text("c".into())],
            ]
        );
        // Distinct over a derived column.
        db.execute("INSERT INTO t VALUES ('a', 1)").unwrap(); // (a,1) duplicates an existing row
        let (_c, pairs) = query(
            &mut db,
            "SELECT DISTINCT g, n FROM t WHERE g = 'a' ORDER BY n",
        );
        assert_eq!(
            pairs,
            vec![
                vec![Value::Text("a".into()), Value::Int(1)],
                vec![Value::Text("a".into()), Value::Int(2)],
            ]
        );
    }

    #[test]
    fn distinct_preserves_sorted_order() {
        let (_d, mut db) = db();
        seed_groups(&mut db);
        let (_c, rows) = query(&mut db, "SELECT DISTINCT g FROM t ORDER BY g DESC");
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("c".into())],
                vec![Value::Text("b".into())],
                vec![Value::Text("a".into())],
            ]
        );
    }

    #[test]
    fn having_filters_groups() {
        let (_d, mut db) = db();
        seed_groups(&mut db);
        // Groups with more than one row: a (2), b (3).
        let (cols, rows) = query(
            &mut db,
            "SELECT g, COUNT(*) FROM t GROUP BY g HAVING COUNT(*) > 1 ORDER BY g",
        );
        assert_eq!(names(&cols), ["g", "COUNT(*)"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".into()), Value::Int(2)],
                vec![Value::Text("b".into()), Value::Int(3)],
            ]
        );
    }

    #[test]
    fn having_on_aggregate_not_in_projection() {
        let (_d, mut db) = db();
        seed_groups(&mut db);
        // HAVING references SUM(n), which is not selected.
        let (cols, rows) = query(
            &mut db,
            "SELECT g FROM t GROUP BY g HAVING SUM(n) >= 9 ORDER BY g",
        );
        assert_eq!(names(&cols), ["g"]);
        // sums: a=3, b=12, c=6 -> only b qualifies.
        assert_eq!(rows, vec![vec![Value::Text("b".into())]]);
    }

    #[test]
    fn explain_shows_distinct_node() {
        let (_d, mut db) = db();
        seed_groups(&mut db);
        let plan = match db.execute("EXPLAIN SELECT DISTINCT g FROM t").unwrap() {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("Distinct"), "plan was:\n{plan}");
    }

    // --- scalar functions ---

    #[test]
    fn string_and_numeric_scalar_functions() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (name TEXT, n INT, x FLOAT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES ('Alice', -7, 2.34567)")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT LENGTH(name), UPPER(name), LOWER(name), ABS(n), ROUND(x), ROUND(x, 2) FROM t",
        );
        assert_eq!(
            rows[0],
            vec![
                Value::Int(5),
                Value::Text("ALICE".into()),
                Value::Text("alice".into()),
                Value::Int(7),
                Value::Float(2.0),
                Value::Float(2.35),
            ]
        );
    }

    #[test]
    fn coalesce_nullif_and_concat() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a TEXT, b TEXT)").unwrap();
        db.execute("INSERT INTO t (a, b) VALUES ('x', 'y')")
            .unwrap();
        db.execute("INSERT INTO t (b) VALUES ('z')").unwrap(); // a is NULL
        let (_c, rows) = query(
            &mut db,
            "SELECT COALESCE(a, b), NULLIF(a, 'x'), CONCAT(a, '-', b) FROM t ORDER BY b",
        );
        // row1: a='x',b='y' -> COALESCE 'x', NULLIF 'x'='x' -> NULL, CONCAT 'x-y'
        assert_eq!(
            rows[0],
            vec![
                Value::Text("x".into()),
                Value::Null,
                Value::Text("x-y".into())
            ]
        );
        // row2: a=NULL,b='z' -> COALESCE 'z', NULLIF NULL, CONCAT '-z' (NULL skipped)
        assert_eq!(
            rows[1],
            vec![
                Value::Text("z".into()),
                Value::Null,
                Value::Text("-z".into())
            ]
        );
    }

    #[test]
    fn scalar_function_in_where_and_null_propagation() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'banana'), (2, 'fig'), (3, 'cherry')")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE LENGTH(name) > 4 ORDER BY id",
        );
        assert_eq!(id_set(&rows), vec![1, 3]);
        // LENGTH(NULL) is NULL, which excludes the row from a WHERE.
        db.execute("INSERT INTO t (id) VALUES (4)").unwrap();
        let (_c, present) = query(&mut db, "SELECT id FROM t WHERE LENGTH(name) IS NULL");
        assert_eq!(id_set(&present), vec![4]);
    }

    // --- CASE expressions ---

    #[test]
    fn searched_case_in_projection() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 5), (2, -3), (3, 0)")
            .unwrap();
        let (cols, rows) = query(
            &mut db,
            "SELECT id, CASE WHEN n > 0 THEN 'pos' WHEN n < 0 THEN 'neg' ELSE 'zero' END FROM t ORDER BY id",
        );
        assert_eq!(names(&cols)[0], "id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("pos".into())],
                vec![Value::Int(2), Value::Text("neg".into())],
                vec![Value::Int(3), Value::Text("zero".into())],
            ]
        );
    }

    #[test]
    fn simple_case_and_missing_else_is_null() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (g TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES ('a'), ('b'), ('c')")
            .unwrap();
        // Simple CASE with no ELSE: unmatched 'c' yields NULL.
        let (_c, rows) = query(
            &mut db,
            "SELECT CASE g WHEN 'a' THEN 1 WHEN 'b' THEN 2 END FROM t ORDER BY g",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Null],]
        );
    }

    #[test]
    fn case_in_where_and_inside_aggregate() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (2, -1), (3, 20), (4, -5)")
            .unwrap();
        // CASE in WHERE: keep rows the CASE maps to a positive number.
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE CASE WHEN n > 0 THEN n ELSE 0 END > 0 ORDER BY id",
        );
        assert_eq!(id_set(&rows), vec![1, 3]);
        // Aggregate over a CASE: sum only the positive values.
        let (_c, agg) = query(
            &mut db,
            "SELECT SUM(CASE WHEN n > 0 THEN n ELSE 0 END) FROM t",
        );
        assert_eq!(agg, vec![vec![Value::Int(30)]]);
    }

    // --- string concat (||) and LIMIT OFFSET ---

    #[test]
    fn string_concatenation() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a TEXT, b TEXT, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES ('foo', 'bar', 7)")
            .unwrap();
        db.execute("INSERT INTO t (b, n) VALUES ('x', 1)").unwrap(); // a is NULL
        let (_c, rows) = query(&mut db, "SELECT a || '-' || b FROM t ORDER BY b");
        // row b='bar': 'foo-bar'; row b='x': a is NULL, so the whole || is NULL
        assert_eq!(
            rows,
            vec![vec![Value::Text("foo-bar".into())], vec![Value::Null]]
        );
        // Concatenating a number coerces it to text.
        let (_c, mixed) = query(&mut db, "SELECT b || n FROM t WHERE n = 7");
        assert_eq!(mixed, vec![vec![Value::Text("bar7".into())]]);
    }

    #[test]
    fn limit_offset() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)")
            .unwrap();
        // Skip 1, take 2.
        let (_c, page) = query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1");
        assert_eq!(id_set(&page), vec![2, 3]);
        // OFFSET past the end yields nothing.
        let (_c, none) = query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 10");
        assert!(none.is_empty());
        // OFFSET with no LIMIT skips and returns the rest.
        let (_c, rest) = query(&mut db, "SELECT id FROM t ORDER BY id OFFSET 3");
        assert_eq!(id_set(&rest), vec![4, 5]);
    }

    // --- DISTINCT aggregates ---

    #[test]
    fn count_and_sum_distinct() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (g TEXT, n INT)").unwrap();
        // group a: values 1,1,2 -> distinct {1,2}; group b: 5,5 -> {5}
        db.execute("INSERT INTO t VALUES ('a',1),('a',1),('a',2),('b',5),('b',5)")
            .unwrap();
        let (cols, rows) = query(
            &mut db,
            "SELECT g, COUNT(n), COUNT(DISTINCT n), SUM(DISTINCT n) FROM t GROUP BY g ORDER BY g",
        );
        assert_eq!(
            names(&cols),
            ["g", "COUNT(n)", "COUNT(DISTINCT n)", "SUM(DISTINCT n)"]
        );
        // a: count 3, distinct count 2, distinct sum 3 ; b: 2, 1, 5
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Text("a".into()),
                    Value::Int(3),
                    Value::Int(2),
                    Value::Int(3)
                ],
                vec![
                    Value::Text("b".into()),
                    Value::Int(2),
                    Value::Int(1),
                    Value::Int(5)
                ],
            ]
        );
    }

    #[test]
    fn count_distinct_whole_table() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (c TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES ('x'),('y'),('x'),('z'),('y')")
            .unwrap();
        let (_c, rows) = query(&mut db, "SELECT COUNT(DISTINCT c) FROM t");
        assert_eq!(rows, vec![vec![Value::Int(3)]]);
    }

    // --- UNION / UNION ALL ---

    #[test]
    fn union_dedups_and_union_all_keeps_duplicates() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2), (3)").unwrap();
        db.execute("INSERT INTO b VALUES (3), (4)").unwrap();
        // UNION removes the duplicate 3.
        let (_c, u) = query(&mut db, "SELECT x FROM a UNION SELECT y FROM b ORDER BY x");
        assert_eq!(id_set(&u), vec![1, 2, 3, 4]);
        // UNION ALL keeps it (two 3s).
        let (_c, ua) = query(&mut db, "SELECT x FROM a UNION ALL SELECT y FROM b");
        let mut got: Vec<i64> = id_set(&ua);
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 3, 4]);
    }

    #[test]
    fn union_takes_left_column_names_and_explains() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1)").unwrap();
        db.execute("INSERT INTO b VALUES (2)").unwrap();
        let (cols, _r) = query(&mut db, "SELECT x FROM a UNION SELECT y FROM b");
        assert_eq!(names(&cols), ["x"]); // output names come from the left query
        let plan = match db
            .execute("EXPLAIN SELECT x FROM a UNION ALL SELECT y FROM b")
            .unwrap()
        {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("Union ALL"), "plan was:\n{plan}");
    }

    #[test]
    fn union_arity_mismatch_errors() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT, y INT)").unwrap();
        db.execute("CREATE TABLE b (z INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1, 2)").unwrap();
        db.execute("INSERT INTO b VALUES (3)").unwrap();
        assert!(db
            .execute("SELECT x, y FROM a UNION SELECT z FROM b")
            .is_err());
    }

    // --- subqueries (uncorrelated scalar and IN) ---

    fn seed_emp(db: &mut Database) {
        db.execute("CREATE TABLE emp (name TEXT, dept TEXT, salary INT)")
            .unwrap();
        db.execute("CREATE TABLE dept (name TEXT, region TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO emp VALUES ('a','eng',100),('b','eng',60),('c','sales',80),('d','sales',40)",
        )
        .unwrap();
        db.execute("INSERT INTO dept VALUES ('eng','west'),('sales','east')")
            .unwrap();
    }

    fn name_set(rows: &[Vec<Value>]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r[0] {
                Value::Text(s) => s.clone(),
                o => panic!("want text, got {o:?}"),
            })
            .collect()
    }

    #[test]
    fn scalar_subquery_in_where_and_projection() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Average salary is 70; names above it: a (100), c (80).
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM emp WHERE salary > (SELECT AVG(salary) FROM emp) ORDER BY name",
        );
        assert_eq!(name_set(&rows), vec!["a", "c"]);
        // A scalar subquery in the projection (same value on every row).
        let (_c, proj) = query(
            &mut db,
            "SELECT name, (SELECT MAX(salary) FROM emp) FROM emp ORDER BY name LIMIT 1",
        );
        assert_eq!(proj, vec![vec![Value::Text("a".into()), Value::Int(100)]]);
    }

    #[test]
    fn in_and_not_in_subquery() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Employees whose dept is in the west region (eng): a, b.
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM emp WHERE dept IN (SELECT name FROM dept WHERE region = 'west') ORDER BY name",
        );
        assert_eq!(name_set(&rows), vec!["a", "b"]);
        // NOT IN: the others.
        let (_c, rows2) = query(
            &mut db,
            "SELECT name FROM emp WHERE dept NOT IN (SELECT name FROM dept WHERE region = 'west') ORDER BY name",
        );
        assert_eq!(name_set(&rows2), vec!["c", "d"]);
    }

    #[test]
    fn empty_scalar_subquery_is_null() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // No rows match, so the scalar subquery is NULL and the comparison is
        // unknown, excluding every row.
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM emp WHERE salary > (SELECT AVG(salary) FROM emp WHERE dept = 'nope')",
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn correlated_subquery_is_rejected() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // The inner query references the outer `dept`, which it cannot see.
        let err = db.execute(
            "SELECT name FROM emp WHERE salary > (SELECT AVG(salary) FROM dept WHERE dept = region)",
        );
        assert!(err.is_err());
    }

    #[test]
    fn exists_and_not_exists() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // EXISTS over a non-empty subquery is true (returns all rows).
        let (_c, all) = query(
            &mut db,
            "SELECT name FROM emp WHERE EXISTS (SELECT 1 FROM dept) ORDER BY name",
        );
        assert_eq!(name_set(&all), vec!["a", "b", "c", "d"]);
        // NOT EXISTS over an empty subquery is true; over a non-empty one,
        // false (no rows).
        let (_c, none) = query(
            &mut db,
            "SELECT name FROM emp WHERE NOT EXISTS (SELECT 1 FROM dept)",
        );
        assert!(none.is_empty());
        let (_c, empty_ok) = query(
            &mut db,
            "SELECT name FROM emp WHERE NOT EXISTS (SELECT 1 FROM dept WHERE region = 'north') ORDER BY name LIMIT 1",
        );
        assert_eq!(name_set(&empty_ok), vec!["a"]);
    }

    // --- derived tables (FROM subquery) ---

    #[test]
    fn derived_table_filters_and_projects() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // A derived table feeds an outer filter; columns are visible under the
        // alias and bare.
        let (cols, rows) = query(
            &mut db,
            "SELECT e.name, e.salary FROM (SELECT name, salary FROM emp WHERE salary >= 80) AS e WHERE e.salary < 100 ORDER BY e.name",
        );
        // A qualified reference projects under the bare column name, as in Postgres.
        assert_eq!(names(&cols), ["name", "salary"]);
        assert_eq!(rows, vec![vec![Value::Text("c".into()), Value::Int(80)]]);
    }

    #[test]
    fn derived_table_aggregate_then_filter() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Aggregate per dept inside the derived table, then keep big departments.
        let (_c, rows) = query(
            &mut db,
            "SELECT d.dept, d.total FROM (SELECT dept, SUM(salary) AS total FROM emp GROUP BY dept) AS d WHERE d.total > 120 ORDER BY d.dept",
        );
        assert_eq!(rows, vec![vec![Value::Text("eng".into()), Value::Int(160)]]);
    }

    #[test]
    fn derived_table_joined_to_base_table() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Join a derived table against a base table on the dept name.
        let (_c, rows) = query(
            &mut db,
            "SELECT e.name, d.region FROM (SELECT name, dept FROM emp WHERE salary > 50) AS e INNER JOIN dept AS d ON e.dept = d.name ORDER BY e.name",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".into()), Value::Text("west".into())],
                vec![Value::Text("b".into()), Value::Text("west".into())],
                vec![Value::Text("c".into()), Value::Text("east".into())],
            ]
        );
    }

    #[test]
    fn derived_table_with_inner_subquery() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // The derived table's own (uncorrelated) scalar subquery is folded
        // before binding: above-average earners, then re-filtered outside.
        let (_c, rows) = query(
            &mut db,
            "SELECT e.name FROM (SELECT name FROM emp WHERE salary > (SELECT AVG(salary) FROM emp)) AS e ORDER BY e.name",
        );
        assert_eq!(name_set(&rows), vec!["a", "c"]);
    }

    #[test]
    fn derived_table_explains() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        let plan = match db
            .execute("EXPLAIN SELECT e.name FROM (SELECT name FROM emp) AS e")
            .unwrap()
        {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("DerivedScan AS e"), "plan was:\n{plan}");
    }

    // --- CREATE VIEW / DROP VIEW ---

    #[test]
    fn view_select_filters_and_projects() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        assert_eq!(
            db.execute("CREATE VIEW well_paid AS SELECT name, salary FROM emp WHERE salary >= 80")
                .unwrap(),
            QueryOutcome::Ddl
        );
        // Plain select from the view.
        let (cols, rows) = query(&mut db, "SELECT name, salary FROM well_paid ORDER BY name");
        assert_eq!(names(&cols), ["name", "salary"]);
        assert_eq!(name_set(&rows), vec!["a", "c"]);
        // The view can be filtered and its columns qualified by the view name.
        let (_c, r2) = query(
            &mut db,
            "SELECT well_paid.name FROM well_paid WHERE well_paid.salary < 100 ORDER BY name",
        );
        assert_eq!(name_set(&r2), vec!["c"]);
    }

    #[test]
    fn view_select_star_and_alias() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW eng AS SELECT name, salary FROM emp WHERE dept = 'eng'")
            .unwrap();
        // SELECT * over a view yields its bare column names.
        let (cols, rows) = query(&mut db, "SELECT * FROM eng ORDER BY name");
        assert_eq!(names(&cols), ["name", "salary"]);
        assert_eq!(name_set(&rows), vec!["a", "b"]);
        // An explicit alias on the view reference qualifies its columns.
        let (_c, r2) = query(
            &mut db,
            "SELECT e.salary FROM eng AS e ORDER BY e.salary DESC",
        );
        assert_eq!(r2, vec![vec![Value::Int(100)], vec![Value::Int(60)]]);
    }

    #[test]
    fn view_joins_base_table() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW earners AS SELECT name, dept FROM emp WHERE salary > 50")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT earners.name, d.region FROM earners INNER JOIN dept AS d ON earners.dept = d.name ORDER BY earners.name",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".into()), Value::Text("west".into())],
                vec![Value::Text("b".into()), Value::Text("west".into())],
                vec![Value::Text("c".into()), Value::Text("east".into())],
            ]
        );
    }

    #[test]
    fn view_references_another_view() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW base AS SELECT name, salary FROM emp")
            .unwrap();
        // A view built on top of another view.
        db.execute("CREATE VIEW top AS SELECT name FROM base WHERE salary >= 80")
            .unwrap();
        let (_c, rows) = query(&mut db, "SELECT name FROM top ORDER BY name");
        assert_eq!(name_set(&rows), vec!["a", "c"]);
    }

    #[test]
    fn view_reflects_later_writes() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW all_names AS SELECT name FROM emp")
            .unwrap();
        db.execute("INSERT INTO emp VALUES ('e','eng',120)")
            .unwrap();
        // The view re-evaluates against current data, so the new row appears.
        let (_c, rows) = query(&mut db, "SELECT name FROM all_names ORDER BY name");
        assert_eq!(name_set(&rows), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn view_explains_as_derived_scan() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW v AS SELECT name FROM emp").unwrap();
        let plan = match db.execute("EXPLAIN SELECT name FROM v").unwrap() {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("DerivedScan AS v"), "plan was:\n{plan}");
    }

    #[test]
    fn create_view_rejects_name_in_use_and_broken_definition() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // A name already taken by a table is rejected.
        assert!(db
            .execute("CREATE VIEW emp AS SELECT name FROM emp")
            .is_err());
        db.execute("CREATE VIEW v AS SELECT name FROM emp").unwrap();
        // A duplicate view name is rejected.
        assert!(db
            .execute("CREATE VIEW v AS SELECT name FROM dept")
            .is_err());
        // A definition over a missing table is rejected at creation time.
        assert!(db
            .execute("CREATE VIEW bad AS SELECT x FROM ghost")
            .is_err());
        // A definition over a missing column is rejected at creation time.
        assert!(db
            .execute("CREATE VIEW bad AS SELECT nope FROM emp")
            .is_err());
    }

    #[test]
    fn drop_view_removes_it() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        db.execute("CREATE VIEW v AS SELECT name FROM emp").unwrap();
        query(&mut db, "SELECT name FROM v");
        db.execute("DROP VIEW v").unwrap();
        // Once dropped, the name no longer resolves.
        assert!(db.execute("SELECT name FROM v").is_err());
        // Dropping a view that does not exist errors.
        assert!(db.execute("DROP VIEW ghost").is_err());
    }

    #[test]
    fn views_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("view.db");
        {
            let mut db = Database::open(&path).expect("open");
            seed_emp(&mut db);
            db.execute("CREATE VIEW well_paid AS SELECT name FROM emp WHERE salary >= 80")
                .unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT name FROM well_paid ORDER BY name");
        assert_eq!(name_set(&rows), vec!["a", "c"]);
    }

    // --- ALTER TABLE ADD COLUMN ---

    #[test]
    fn alter_add_column_backfills_existing_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        // Add a column with a default; existing rows get the default.
        db.execute("ALTER TABLE t ADD COLUMN active BOOL DEFAULT TRUE")
            .unwrap();
        // And one without a default (NULL-backfilled).
        db.execute("ALTER TABLE t ADD COLUMN note TEXT").unwrap();
        let (cols, rows) = query(&mut db, "SELECT id, name, active, note FROM t ORDER BY id");
        assert_eq!(names(&cols), ["id", "name", "active", "note"]);
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("a".into()),
                    Value::Bool(true),
                    Value::Null
                ],
                vec![
                    Value::Int(2),
                    Value::Text("b".into()),
                    Value::Bool(true),
                    Value::Null
                ],
            ]
        );
        // New inserts can supply the added columns.
        db.execute("INSERT INTO t (id, name, active, note) VALUES (3, 'c', FALSE, 'hi')")
            .unwrap();
        let (_c, r3) = query(&mut db, "SELECT active, note FROM t WHERE id = 3");
        assert_eq!(r3, vec![vec![Value::Bool(false), Value::Text("hi".into())]]);
    }

    #[test]
    fn alter_add_column_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("alt.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1)").unwrap();
            db.execute("ALTER TABLE t ADD COLUMN score INT DEFAULT 5")
                .unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        db.execute("INSERT INTO t (id) VALUES (2)").unwrap();
        let (_c, rows) = query(&mut db, "SELECT id, score FROM t ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(5)],
                vec![Value::Int(2), Value::Int(5)],
            ]
        );
    }

    // --- TRUNCATE ---

    #[test]
    fn truncate_empties_table_but_keeps_schema() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT DEFAULT 'x')")
            .unwrap();
        db.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        db.execute("TRUNCATE TABLE t").unwrap();
        let (_c, empty) = query(&mut db, "SELECT id FROM t");
        assert!(empty.is_empty());
        // The schema (and its default) survives; inserts work again, and the
        // unique PK index was reset (reusing id 1 is fine).
        db.execute("INSERT INTO t (id) VALUES (1)").unwrap();
        let (_c, rows) = query(&mut db, "SELECT id, name FROM t");
        assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("x".into())]]);
    }

    // --- DEFAULT column values ---

    #[test]
    fn default_values_fill_omitted_columns() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, status TEXT DEFAULT 'new', n INT DEFAULT 7, active BOOL DEFAULT TRUE)")
            .unwrap();
        // Only id is provided; the rest take their defaults.
        db.execute("INSERT INTO t (id) VALUES (1)").unwrap();
        // An explicit value overrides the default.
        db.execute("INSERT INTO t (id, status) VALUES (2, 'done')")
            .unwrap();
        let (cols, rows) = query(&mut db, "SELECT id, status, n, active FROM t ORDER BY id");
        assert_eq!(names(&cols), ["id", "status", "n", "active"]);
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("new".into()),
                    Value::Int(7),
                    Value::Bool(true)
                ],
                vec![
                    Value::Int(2),
                    Value::Text("done".into()),
                    Value::Int(7),
                    Value::Bool(true)
                ],
            ]
        );
    }

    #[test]
    fn default_satisfies_not_null_and_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("def.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, tag TEXT NOT NULL DEFAULT 'x with space')")
                .unwrap();
            // NOT NULL is satisfied by the default even though tag is omitted.
            db.execute("INSERT INTO t (id) VALUES (1)").unwrap();
        }
        // The default persists (note the embedded space round-trips).
        let mut db = Database::open(&path).expect("reopen");
        db.execute("INSERT INTO t (id) VALUES (2)").unwrap();
        let (_c, rows) = query(&mut db, "SELECT id, tag FROM t ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("x with space".into())],
                vec![Value::Int(2), Value::Text("x with space".into())],
            ]
        );
    }

    // --- CROSS JOIN and comma joins ---

    #[test]
    fn cross_join_and_comma_join() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO b VALUES (10), (20)").unwrap();
        // CROSS JOIN: 2 x 2 = 4 rows.
        let (_c, rows) = query(&mut db, "SELECT x, y FROM a CROSS JOIN b ORDER BY x, y");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(1), Value::Int(20)],
                vec![Value::Int(2), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
        // Comma join is the same cartesian product; a WHERE makes it an
        // old-style equi-join.
        let (_c, filtered) = query(&mut db, "SELECT x, y FROM a, b WHERE x * 10 = y ORDER BY x");
        assert_eq!(
            filtered,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    // --- ORDER BY ordinal and output alias ---

    #[test]
    fn order_by_ordinal_and_alias() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a INT, b INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 30), (2, 10), (3, 20)")
            .unwrap();
        // ORDER BY 2 sorts by the second projected column (b).
        let (_c, rows) = query(&mut db, "SELECT a, b FROM t ORDER BY 2");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Int(10)],
                vec![Value::Int(3), Value::Int(20)],
                vec![Value::Int(1), Value::Int(30)],
            ]
        );
        // ORDER BY an output alias backed by an expression.
        let (cols, sums) = query(&mut db, "SELECT a + b AS s FROM t ORDER BY s DESC");
        assert_eq!(names(&cols), ["s"]);
        assert_eq!(
            sums,
            vec![
                vec![Value::Int(31)],
                vec![Value::Int(23)],
                vec![Value::Int(12)],
            ]
        );
    }

    // --- more built-in functions ---

    #[test]
    fn string_and_math_functions() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (s TEXT, n INT, x FLOAT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES ('  Hello World  ', 17, 2.5)")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT SUBSTR(TRIM(s), 1, 5), REPLACE(TRIM(s), 'World', 'SQL'), MOD(n, 5), POWER(x, 2), SQRT(x), FLOOR(x), CEIL(x) FROM t",
        );
        assert_eq!(
            rows[0],
            vec![
                Value::Text("Hello".into()),
                Value::Text("Hello SQL".into()),
                Value::Int(2),
                Value::Float(6.25),
                Value::Float(2.5_f64.sqrt()),
                Value::Float(2.0),
                Value::Float(3.0),
            ]
        );
        // NULL propagation: SUBSTR of a NULL is NULL.
        db.execute("INSERT INTO t (n) VALUES (1)").unwrap();
        let (_c, nulls) = query(&mut db, "SELECT SUBSTR(s, 1, 2) FROM t WHERE n = 1");
        assert_eq!(nulls, vec![vec![Value::Null]]);
    }
}
