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
use std::rc::Rc;

use rustdb_executor::eval::{eval, is_truthy};
use rustdb_executor::{decode_row, encode_row, run, run_with, Relation, TableSource};
use rustdb_planner::{bind, explain, plan, Catalog, ColumnStats};

use crate::correlated;
use crate::correlated::{CorrelatedRunner, MaterializedSource};
use rustdb_sql::statement::{
    ColumnDef, ConflictAction, Cte, DataType, Join, OnConflict, OrderItem, Select, SelectItem,
    TableConstraint, TableRef,
};
use rustdb_sql::{BinOp, Expr, Parser, SetOp, Statement, UnOp, Value};
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

/// A single-column foreign key on a table, resolved for enforcement.
#[derive(Debug, Clone)]
struct ForeignKeyMeta {
    /// The referencing column in this (child) table.
    column: String,
    /// The referenced (parent) table.
    parent_table: String,
    /// The referenced column in the parent table.
    parent_column: String,
}

/// The constraints attached to one table, beyond the per-column NOT NULL /
/// UNIQUE handled by the storage layer.
#[derive(Debug, Clone, Default)]
struct TableConstraints {
    /// `CHECK` predicates, rejected when a row makes one false.
    checks: Vec<Expr>,
    /// Foreign keys this table declares (it is the child).
    foreign_keys: Vec<ForeignKeyMeta>,
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
    /// Per-table `CHECK` and `FOREIGN KEY` constraints, enforced on writes.
    constraints: HashMap<String, TableConstraints>,
    /// Per-table `SERIAL` (auto-increment) column names. When such a column is
    /// omitted on insert, the engine assigns the next value (max so far + 1).
    serial_cols: HashMap<String, Vec<String>>,
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
    /// Sidecar file recording the `CHECK` and `FOREIGN KEY` constraints.
    cons_path: PathBuf,
    /// Sidecar file recording the `SERIAL` columns.
    seq_path: PathBuf,
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
        let cons_path = base.with_extension("cons");
        let seq_path = base.with_extension("seq");
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
            constraints: HashMap::new(),
            serial_cols: HashMap::new(),
            current_txn: None,
            meta_path,
            txn_path,
            view_path,
            cons_path,
            seq_path,
        };
        db.load_catalog()?;
        db.load_views()?;
        db.load_constraints()?;
        db.load_serials()?;
        // Register the read-only information_schema views so introspection
        // queries bind. They are catalog-only (no physical store) and are built
        // on demand by `EngineSource`, so they are never persisted.
        db.register_system_tables();
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
                        // The catalog does not use defaults or the serial flag;
                        // the engine restores those from its own sidecars.
                        default: None,
                        serial: false,
                    },
                )
                .collect();
            self.catalog.apply(&Statement::CreateTable {
                name: r.name.clone(),
                columns,
                constraints: vec![],
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
                            min: None,
                            max: None,
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

    /// Rebuild the constraint registry from its sidecar, re-parsing each check
    /// predicate. A malformed entry is skipped rather than failing the open.
    fn load_constraints(&mut self) -> Result<()> {
        for c in persist::load_constraints(&self.cons_path)? {
            match c {
                persist::Constraint::Check { table, sql } => {
                    if let Ok(expr) = Parser::from_sql(&sql).and_then(|mut p| p.parse_expr()) {
                        self.constraints.entry(table).or_default().checks.push(expr);
                    }
                }
                persist::Constraint::ForeignKey {
                    table,
                    column,
                    parent_table,
                    parent_column,
                } => self
                    .constraints
                    .entry(table)
                    .or_default()
                    .foreign_keys
                    .push(ForeignKeyMeta {
                        column,
                        parent_table,
                        parent_column,
                    }),
            }
        }
        Ok(())
    }

    /// Write the constraint registry to its sidecar.
    fn save_constraints(&self) -> Result<()> {
        let mut records = Vec::new();
        let mut tables: Vec<&String> = self.constraints.keys().collect();
        tables.sort();
        for table in tables {
            let tc = &self.constraints[table];
            for check in &tc.checks {
                records.push(persist::Constraint::Check {
                    table: table.clone(),
                    sql: check.to_string(),
                });
            }
            for fk in &tc.foreign_keys {
                records.push(persist::Constraint::ForeignKey {
                    table: table.clone(),
                    column: fk.column.clone(),
                    parent_table: fk.parent_table.clone(),
                    parent_column: fk.parent_column.clone(),
                });
            }
        }
        persist::save_constraints(&self.cons_path, &records)?;
        Ok(())
    }

    /// Rebuild the serial-column registry from its sidecar.
    fn load_serials(&mut self) -> Result<()> {
        for (table, column) in persist::load_sequences(&self.seq_path)? {
            self.serial_cols.entry(table).or_default().push(column);
        }
        Ok(())
    }

    /// Write the serial-column registry to its sidecar.
    fn save_serials(&self) -> Result<()> {
        let mut records: Vec<(String, String)> = Vec::new();
        let mut tables: Vec<&String> = self.serial_cols.keys().collect();
        tables.sort();
        for table in tables {
            for col in &self.serial_cols[table] {
                records.push((table.clone(), col.clone()));
            }
        }
        persist::save_sequences(&self.seq_path, &records)?;
        Ok(())
    }

    /// Register the `information_schema` views in the catalog so introspection
    /// queries bind. They carry no physical store; [`EngineSource`] materializes
    /// their rows from the live catalog on each scan.
    fn register_system_tables(&mut self) {
        for name in SYSTEM_TABLES {
            if self.catalog.get_table(name).is_some() {
                continue;
            }
            let columns = system_table_schema(name)
                .expect("a known system table")
                .into_iter()
                .map(|(c, ty)| ColumnDef {
                    name: c.to_string(),
                    ty,
                    primary_key: false,
                    not_null: false,
                    unique: false,
                    default: None,
                    serial: false,
                })
                .collect();
            let create = Statement::CreateTable {
                name: name.to_string(),
                columns,
                constraints: Vec::new(),
            };
            let _ = self.catalog.apply(&create);
        }
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
        // Inline any WITH common table expressions into the body before routing,
        // so the rest of the engine only sees plain queries and derived tables.
        let stmt = expand_ctes(stmt)?;
        match stmt {
            // Transaction control.
            Statement::Begin => self.begin_txn(),
            Statement::Commit => self.commit_txn(),
            Statement::Rollback => self.rollback_txn(),
            // DDL auto-commits: it persists immediately regardless of any open
            // transaction.
            Statement::CreateTable { .. } => self.create_table(&stmt),
            Statement::CreateTableAs {
                ref name,
                ref query,
            } => self.create_table_as(name, query),
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
            Statement::Analyze { ref table } => self.run_analyze(table.as_deref()),
            Statement::Vacuum { ref table } => self.run_vacuum(table.as_deref()),
            Statement::AlterTableAddColumn {
                ref table,
                ref column,
            } => self.alter_add_column(table, column),
            // EXPLAIN plans; EXPLAIN ANALYZE also runs the query.
            Statement::Explain { .. } => self.run_explain(&stmt),
            // DML and SELECT run inside the open transaction, or a fresh
            // auto-commit one. `expand_ctes` inlined any non-recursive WITH;
            // only a recursive WITH reaches here, evaluated against a snapshot.
            Statement::With { .. }
            | Statement::Insert { .. }
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
                on_conflict,
                returning,
            } => self.insert(txn, table, columns, rows, on_conflict.as_ref(), returning),
            Statement::Update {
                table,
                assignments,
                where_clause,
                returning,
            } => self.run_update(txn, table, assignments, where_clause.as_ref(), returning),
            Statement::Delete {
                table,
                where_clause,
                returning,
            } => self.run_delete(txn, table, where_clause.as_ref(), returning),
            Statement::Select(_) | Statement::Union { .. } => self.run_select(txn, stmt),
            Statement::With { ctes, body, .. } => self.run_with_ctes(txn, ctes, body),
            other => Err(DbError::Unsupported(format!("cannot run: {other}"))),
        }
    }

    /// Number of tables currently known to the catalog.
    #[must_use]
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Whether an explicit transaction (`BEGIN` without a matching `COMMIT` /
    /// `ROLLBACK`) is currently open. Used by the wire protocol to report the
    /// transaction status in `ReadyForQuery`.
    #[must_use]
    pub const fn in_transaction(&self) -> bool {
        self.current_txn.is_some()
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
        let Statement::CreateTable {
            name,
            columns,
            constraints,
        } = stmt
        else {
            unreachable!("guarded by execute");
        };
        // Validate and resolve the table-level constraints before creating
        // anything, so a bad reference rejects the whole statement.
        let table_constraints = self.build_constraints(name, columns, constraints)?;
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
        if !table_constraints.checks.is_empty() || !table_constraints.foreign_keys.is_empty() {
            self.constraints.insert(name.clone(), table_constraints);
            self.save_constraints()?;
        }
        let serials: Vec<String> = columns
            .iter()
            .filter(|c| c.serial)
            .map(|c| c.name.clone())
            .collect();
        if !serials.is_empty() {
            self.serial_cols.insert(name.clone(), serials);
            self.save_serials()?;
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `CREATE TABLE name AS <query>`: run the query, infer a column per result
    /// column (name from the projection, type from the data), create the table,
    /// and bulk-load the rows.
    fn create_table_as(&mut self, name: &str, query: &Statement) -> Result<QueryOutcome> {
        if self.catalog.get_table(name).is_some() || self.views.contains_key(name) {
            return Err(DbError::Constraint(format!("table {name} already exists")));
        }
        // Run the query under a read snapshot.
        let txn = self.mgr.begin();
        let (columns, rows) = self.run_query_collect(&txn, query)?;
        self.mgr.commit(&txn);

        // Infer a column per result column.
        let coldefs: Vec<ColumnDef> = columns
            .iter()
            .enumerate()
            .map(|(i, col)| ColumnDef {
                name: ctas_column_name(col),
                ty: column_type(&rows, i),
                primary_key: false,
                not_null: false,
                unique: false,
                default: None,
                serial: false,
            })
            .collect();
        let schema: Vec<DataType> = coldefs.iter().map(|c| c.ty).collect();

        // Create the (empty) table, then bulk-load the rows. The inferred
        // columns carry no constraints, so there are no secondary indexes to
        // maintain.
        let create = Statement::CreateTable {
            name: name.to_string(),
            columns: coldefs,
            constraints: Vec::new(),
        };
        self.create_table(&create)?;
        let writer = self.mgr.begin();
        let store = self.tables.get_mut(name).expect("just created");
        let handle = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            store.index_root,
            store.version_page,
        );
        let mut rowid: u64 = 0;
        for values in &rows {
            handle.insert(&writer, rowid, &encode_row(values, &schema)?)?;
            rowid += 1;
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        store.next_rowid = rowid;
        self.mgr.commit(&writer);
        self.catalog.set_row_count(name, rowid)?;
        self.persist()?;
        Ok(QueryOutcome::Mutation {
            affected: rows.len(),
        })
    }

    /// Run a query statement (a `SELECT`, set operation, or `WITH`) under `txn`
    /// and return its columns and rows. Handles CTE expansion so the same forms
    /// `run_select` accepts work here too.
    fn run_query_collect(
        &self,
        txn: &Transaction,
        stmt: &Statement,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        match expand_ctes(stmt.clone())? {
            // A recursive WITH is evaluated by the materializing path.
            with @ Statement::With { .. } => {
                let Statement::With { ctes, body, .. } = &with else {
                    unreachable!("matched With");
                };
                match self.run_with_ctes(txn, ctes, body)? {
                    QueryOutcome::Rows { columns, rows } => Ok((columns, rows)),
                    other => Err(DbError::Unsupported(format!(
                        "CREATE TABLE AS expected rows, got {other:?}"
                    ))),
                }
            }
            plain => {
                let folded = self.fold_query(txn, &plain)?;
                self.execute_query(txn, &folded)
            }
        }
    }

    /// Resolve and validate a table's parsed constraints: every check is kept as
    /// a predicate; every foreign key must name a column of this table and an
    /// existing parent table and column.
    fn build_constraints(
        &self,
        table: &str,
        columns: &[ColumnDef],
        parsed: &[TableConstraint],
    ) -> Result<TableConstraints> {
        let mut out = TableConstraints::default();
        let has_col = |c: &str| columns.iter().any(|col| col.name == c);
        for con in parsed {
            match con {
                TableConstraint::Check(expr) => out.checks.push(expr.clone()),
                TableConstraint::ForeignKey(fk) => {
                    if !has_col(&fk.column) {
                        return Err(DbError::UnknownColumn {
                            table: table.to_string(),
                            column: fk.column.clone(),
                        });
                    }
                    let parent = self
                        .catalog
                        .get_table(&fk.parent_table)
                        .ok_or_else(|| DbError::UnknownTable(fk.parent_table.clone()))?;
                    if !parent.columns.iter().any(|c| c.name == fk.parent_column) {
                        return Err(DbError::UnknownColumn {
                            table: fk.parent_table.clone(),
                            column: fk.parent_column.clone(),
                        });
                    }
                    out.foreign_keys.push(ForeignKeyMeta {
                        column: fk.column.clone(),
                        parent_table: fk.parent_table.clone(),
                        parent_column: fk.parent_column.clone(),
                    });
                }
            }
        }
        Ok(out)
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

    /// Recompute planner statistics for one table or all of them: scan the live
    /// rows and record each column's distinct count and (for integers) its
    /// min/max, so the cost model estimates selectivity from real data instead
    /// of defaults. Statistics live in the in-memory catalog, so a reopen needs
    /// a fresh `ANALYZE` (writes also keep a rough distinct count current).
    fn run_analyze(&mut self, table: Option<&str>) -> Result<QueryOutcome> {
        let targets: Vec<String> = if let Some(t) = table {
            if !self.tables.contains_key(t) {
                return Err(DbError::UnknownTable(t.to_string()));
            }
            vec![t.to_string()]
        } else {
            let mut all: Vec<String> = self.tables.keys().cloned().collect();
            all.sort();
            all
        };

        // Pass 1: scan and compute, holding only shared borrows of the engine.
        let txn = self.mgr.begin();
        let source = EngineSource {
            pool: &self.pool,
            wal: self.wal.clone(),
            mgr: &self.mgr,
            catalog: &self.catalog,
            tables: &self.tables,
            txn: &txn,
        };
        // (table, row count, [(column, stats)]) computed before any catalog write.
        let mut computed: Vec<(String, u64, ColumnStatList)> = Vec::new();
        for name in &targets {
            let relation = source.scan(name)?;
            let cols: ColumnStatList = relation
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| (col.clone(), analyze_column(&relation.rows, i)))
                .collect();
            let rows = u64::try_from(relation.rows.len()).unwrap_or(u64::MAX);
            computed.push((name.clone(), rows, cols));
        }
        drop(source);

        // Pass 2: store the statistics in the catalog.
        for (name, rows, cols) in computed {
            self.catalog.set_row_count(&name, rows)?;
            for (col, stats) in cols {
                self.catalog.set_column_stats(&name, &col, stats)?;
            }
        }
        Ok(QueryOutcome::Message("ANALYZE"))
    }

    /// `VACUUM`: compact one table or all of them, reclaiming the space held by
    /// dead row versions and stale secondary-index entries.
    ///
    /// Because the engine is single-threaded, a vacuum runs with no other live
    /// snapshot, so it is safe to keep only each row's currently visible version
    /// and discard the rest. It rewrites the table's live rows into a fresh MVCC
    /// store with rebuilt indexes (a compacting, `VACUUM FULL`-style rewrite),
    /// which is why it is refused inside an open transaction whose older
    /// snapshot the rewrite would invalidate.
    fn run_vacuum(&mut self, table: Option<&str>) -> Result<QueryOutcome> {
        if self.current_txn.is_some() {
            return Err(DbError::Unsupported(
                "VACUUM cannot run inside a transaction block".into(),
            ));
        }
        let targets: Vec<String> = if let Some(t) = table {
            if !self.tables.contains_key(t) {
                return Err(DbError::UnknownTable(t.to_string()));
            }
            vec![t.to_string()]
        } else {
            let mut all: Vec<String> = self.tables.keys().cloned().collect();
            all.sort();
            all
        };
        for name in &targets {
            self.vacuum_table(name)?;
        }
        self.persist()?;
        Ok(QueryOutcome::Message("VACUUM"))
    }

    /// Rewrite `name`'s currently visible rows into a fresh MVCC store and fresh
    /// secondary indexes, then swap the new storage in. Dead versions and stale
    /// index entries are simply left behind in the old (now unreferenced) pages.
    fn vacuum_table(&mut self, name: &str) -> Result<()> {
        let schema: Vec<DataType> = self
            .catalog
            .get_table(name)
            .ok_or_else(|| DbError::UnknownTable(name.to_string()))?
            .columns
            .iter()
            .map(|c| c.ty)
            .collect();

        // 1. Read the live rows under a fresh snapshot (latest committed values).
        let reader = self.mgr.begin();
        let (rows, sec_cols) = {
            let store = self
                .tables
                .get(name)
                .ok_or_else(|| DbError::UnknownTable(name.to_string()))?;
            let handle = MvccTable::open(
                &self.pool,
                self.wal.clone(),
                &self.mgr,
                store.index_root,
                store.version_page,
            );
            let rows: Vec<Vec<Value>> = handle
                .scan(&reader)?
                .into_iter()
                .map(|(_k, bytes)| decode_row(&bytes, &schema))
                .collect::<std::result::Result<_, _>>()?;
            let sec_cols: Vec<usize> = store.secondary.iter().map(|s| s.column).collect();
            (rows, sec_cols)
        };
        self.mgr.commit(&reader);

        // 2. Fresh storage plus a fresh secondary index per previously indexed
        //    column.
        let new_table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;
        let mut secondary: Vec<SecondaryIndex> = Vec::with_capacity(sec_cols.len());
        for column in sec_cols {
            secondary.push(SecondaryIndex {
                column,
                root: Index::create(&self.pool)?.root(),
            });
        }

        // 3. Re-insert each live row, rebuilding the indexes as we go.
        let writer = self.mgr.begin();
        let mut rowid: u64 = 0;
        for values in rows {
            new_table.insert(&writer, rowid, &encode_row(&values, &schema)?)?;
            for sec in &mut secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&values[sec.column], rowid)?;
                sec.root = index.root();
            }
            rowid += 1;
        }
        self.mgr.commit(&writer);

        // 4. Swap in the compacted storage.
        let index_root = new_table.index_root();
        let version_page = new_table.version_page();
        let store = self.tables.get_mut(name).expect("present");
        store.index_root = index_root;
        store.version_page = version_page;
        store.next_rowid = rowid;
        store.secondary = secondary;
        self.catalog.set_row_count(name, rowid)?;
        Ok(())
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
        // Reject the drop if another table still has a foreign key into this one
        // (RESTRICT). The table's own constraints are dropped with it.
        if let Some(child) = self.referencing_table(name) {
            return Err(DbError::Constraint(format!(
                "cannot drop table {name}: it is referenced by a foreign key on {child}"
            )));
        }
        self.catalog.apply(stmt)?;
        self.tables.remove(name);
        if self.constraints.remove(name).is_some() {
            self.save_constraints()?;
        }
        if self.serial_cols.remove(name).is_some() {
            self.save_serials()?;
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// The name of some other table holding a foreign key into `parent`, if any.
    fn referencing_table(&self, parent: &str) -> Option<String> {
        self.constraints.iter().find_map(|(child, tc)| {
            (child != parent && tc.foreign_keys.iter().any(|fk| fk.parent_table == parent))
                .then(|| child.clone())
        })
    }

    /// Reject `row` if it makes any of `table`'s `CHECK` predicates false.
    /// `columns` are the table's column names, aligned with `row`. A predicate
    /// that is NULL (unknown) passes, matching SQL `CHECK` semantics.
    fn enforce_checks(&self, table: &str, columns: &[String], row: &[Value]) -> Result<()> {
        let Some(tc) = self.constraints.get(table) else {
            return Ok(());
        };
        for check in &tc.checks {
            if matches!(eval(check, row, columns)?, Value::Bool(false)) {
                return Err(DbError::Constraint(format!(
                    "new row for {table} violates CHECK ({check})"
                )));
            }
        }
        Ok(())
    }

    /// Reject `row` if a foreign-key column points at a parent row that does not
    /// exist. A NULL referencing value is allowed (it references nothing).
    fn enforce_fk_child(
        &self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        row: &[Value],
    ) -> Result<()> {
        let Some(tc) = self.constraints.get(table) else {
            return Ok(());
        };
        for fk in &tc.foreign_keys {
            let Some(idx) = columns.iter().position(|c| c == &fk.column) else {
                continue;
            };
            let value = &row[idx];
            if matches!(value, Value::Null) {
                continue;
            }
            if !self.column_has_value(txn, &fk.parent_table, &fk.parent_column, value)? {
                return Err(DbError::Constraint(format!(
                    "foreign key violation: {table}.{} references a missing {}.{}",
                    fk.column, fk.parent_table, fk.parent_column
                )));
            }
        }
        Ok(())
    }

    /// Reject the change to a parent `row` if a child still references the value
    /// being removed (foreign-key `RESTRICT`). Used on `DELETE`, and on `UPDATE`
    /// when a referenced column changes.
    fn enforce_fk_restrict(
        &self,
        txn: &Transaction,
        parent_table: &str,
        parent_columns: &[String],
        parent_row: &[Value],
    ) -> Result<()> {
        for (child_table, tc) in &self.constraints {
            for fk in &tc.foreign_keys {
                if fk.parent_table != parent_table {
                    continue;
                }
                let Some(pidx) = parent_columns.iter().position(|c| c == &fk.parent_column) else {
                    continue;
                };
                let value = &parent_row[pidx];
                if matches!(value, Value::Null) {
                    continue;
                }
                if self.column_has_value(txn, child_table, &fk.column, value)? {
                    return Err(DbError::Constraint(format!(
                        "foreign key violation: {child_table}.{} still references {parent_table}.{}",
                        fk.column, fk.parent_column
                    )));
                }
            }
        }
        Ok(())
    }

    /// Reject an `UPDATE` of a parent `table` that changes a referenced column
    /// to a new value while a child still references the old one (`RESTRICT`).
    fn enforce_fk_parent_update(
        &self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        old_row: &[Value],
        new_row: &[Value],
    ) -> Result<()> {
        for (child_table, tc) in &self.constraints {
            for fk in &tc.foreign_keys {
                if fk.parent_table != table {
                    continue;
                }
                let Some(pidx) = columns.iter().position(|c| c == &fk.parent_column) else {
                    continue;
                };
                let old = &old_row[pidx];
                // Only a real change to the referenced value can orphan a child.
                if old == &new_row[pidx] || matches!(old, Value::Null) {
                    continue;
                }
                if self.column_has_value(txn, child_table, &fk.column, old)? {
                    return Err(DbError::Constraint(format!(
                        "foreign key violation: changing {table}.{} would orphan {child_table}.{}",
                        fk.parent_column, fk.column
                    )));
                }
            }
        }
        Ok(())
    }

    /// Whether any visible row of `table` has `column` equal to `value`.
    fn column_has_value(
        &self,
        txn: &Transaction,
        table: &str,
        column: &str,
        value: &Value,
    ) -> Result<bool> {
        let (idx, schema) = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            let idx = meta
                .column_index(column)
                .ok_or_else(|| DbError::UnknownColumn {
                    table: table.to_string(),
                    column: column.to_string(),
                })?;
            let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
            (idx, schema)
        };
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
        for (_key, bytes) in handle.scan(txn)? {
            let row = decode_row(&bytes, &schema)?;
            if row.get(idx).is_some_and(|v| v == value) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Highest integer value stored in column `idx` of `table`, visible to
    /// `txn`. Returns 0 when the table is empty or the column holds no
    /// integers, so a serial column's first auto value is 1. Used to derive
    /// the next SERIAL value (max + 1) without persisting a counter.
    fn column_max_int(&self, txn: &Transaction, table: &str, idx: usize) -> Result<i64> {
        let schema: Vec<DataType> = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            meta.columns.iter().map(|c| c.ty).collect()
        };
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
        let mut max = 0i64;
        for (_key, bytes) in handle.scan(txn)? {
            let row = decode_row(&bytes, &schema)?;
            if let Some(Value::Int(n)) = row.get(idx) {
                if *n > max {
                    max = *n;
                }
            }
        }
        Ok(max)
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
        on_conflict: Option<&OnConflict>,
        returning: &[SelectItem],
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

        // Snapshot the per-column defaults and names for the validation pass.
        let defaults: Vec<Option<Value>> = self
            .tables
            .get(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?
            .defaults
            .clone();
        let column_names: Vec<String> = col_meta.iter().map(|(n, _, _)| n.clone()).collect();

        // Serial columns that this INSERT leaves unset get the next auto value
        // (max existing + 1), assigned sequentially across the inserted rows.
        // A column that is named in the INSERT keeps its explicit value, and
        // the max-derivation picks up from there on the next insert.
        let mut serial_next: Vec<(usize, i64)> = Vec::new();
        if let Some(serials) = self.serial_cols.get(table) {
            let omitted: Vec<usize> = serials
                .iter()
                .filter_map(|name| column_names.iter().position(|c| c == name))
                .filter(|idx| !positions.contains(idx))
                .collect();
            for idx in omitted {
                let start = self.column_max_int(txn, table, idx)? + 1;
                serial_next.push((idx, start));
            }
        }

        // Pass 1: build each row and validate it (NOT NULL, CHECK, FOREIGN KEY)
        // before touching storage, so a violation rejects the statement before
        // any write. This also keeps the per-row checks (which read other tables
        // through `&self`) clear of the mutable table borrow taken below.
        let mut built: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
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
                .map(|i| defaults.get(i).cloned().flatten().unwrap_or(Value::Null))
                .collect();
            for (expr, &pos) in row.iter().zip(&positions) {
                values[pos] = const_eval(expr)?;
            }
            // Assign the next value to each omitted SERIAL column.
            for (idx, next) in &mut serial_next {
                values[*idx] = Value::Int(*next);
                *next += 1;
            }
            // NOT NULL.
            for (i, (name, not_null, _)) in col_meta.iter().enumerate() {
                if *not_null && matches!(values[i], Value::Null) {
                    return Err(DbError::Constraint(format!("column {name} cannot be NULL")));
                }
            }
            self.enforce_checks(table, &column_names, &values)?;
            self.enforce_fk_child(txn, table, &column_names, &values)?;
            built.push(values);
        }

        // The UNIQUE columns and the ON CONFLICT arbiter (the columns whose
        // collision the clause resolves). An explicit `ON CONFLICT (cols)`
        // names the arbiter; otherwise every UNIQUE column arbitrates.
        let unique_cols: Vec<usize> = col_meta
            .iter()
            .enumerate()
            .filter_map(|(i, (_, _, unique))| unique.then_some(i))
            .collect();
        let arbiter: Vec<usize> = match on_conflict {
            Some(oc) if !oc.target.is_empty() => oc
                .target
                .iter()
                .map(|name| {
                    column_names.iter().position(|c| c == name).ok_or_else(|| {
                        DbError::UnknownColumn {
                            table: table.to_string(),
                            column: name.clone(),
                        }
                    })
                })
                .collect::<Result<_>>()?,
            _ => unique_cols.clone(),
        };
        let has_target = matches!(on_conflict, Some(oc) if !oc.target.is_empty());
        if let Some(OnConflict {
            target,
            action: ConflictAction::Update { .. },
        }) = on_conflict
        {
            if target.is_empty() {
                return Err(DbError::Unsupported(
                    "ON CONFLICT DO UPDATE requires a conflict target".into(),
                ));
            }
        }

        // Snapshot the live rows (rowid plus values) once, so conflict
        // detection and the DO UPDATE target lookup read a stable picture.
        let (snap_root, snap_version) = {
            let s = self
                .tables
                .get(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            (s.index_root, s.version_page)
        };
        let snap = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            snap_root,
            snap_version,
        );
        let mut existing: Vec<(u64, Vec<Value>)> = Vec::new();
        for (key, bytes) in snap.scan(txn)? {
            existing.push((key, decode_row(&bytes, &schema)?));
        }

        // Closure-free conflict test: does `row` collide with `cand` on the
        // arbiter (with a target) or on any single UNIQUE column (without one)?
        let collides = |cand: &[Value], row: &[Value]| -> bool {
            if has_target {
                rows_match_on(cand, row, &arbiter)
            } else {
                unique_cols.iter().any(|&c| rows_match_on(cand, row, &[c]))
            }
        };

        // Plan each row: an insert, a skip (DO NOTHING / failed WHERE), or an
        // update of the conflicting row. Values claimed by earlier rows of this
        // same statement are tracked so an intra-statement duplicate is caught.
        let mut planned: Vec<Vec<Value>> = Vec::new();
        let mut plans: Vec<RowPlan> = Vec::with_capacity(built.len());
        for values in built {
            let hit = existing
                .iter()
                .find(|(_, row)| collides(&values, row))
                .map(|(rid, _)| *rid);
            let intra = planned.iter().any(|row| collides(&values, row));

            match on_conflict {
                None => {
                    if hit.is_some() || intra {
                        let col = unique_cols
                            .iter()
                            .find(|&&c| {
                                !matches!(values[c], Value::Null)
                                    && (existing
                                        .iter()
                                        .any(|(_, r)| rows_match_on(&values, r, &[c]))
                                        || planned.iter().any(|r| rows_match_on(&values, r, &[c])))
                            })
                            .copied()
                            .unwrap_or(0);
                        return Err(DbError::Constraint(format!(
                            "duplicate value in column {}",
                            col_meta[col].0
                        )));
                    }
                    planned.push(values.clone());
                    plans.push(RowPlan::Insert(values));
                }
                Some(oc) => match &oc.action {
                    ConflictAction::Nothing => {
                        if hit.is_some() || intra {
                            plans.push(RowPlan::Skip);
                        } else {
                            planned.push(values.clone());
                            plans.push(RowPlan::Insert(values));
                        }
                    }
                    ConflictAction::Update {
                        assignments,
                        where_clause,
                    } => {
                        if let Some(rowid) = hit {
                            let existing_row = existing
                                .iter()
                                .find(|(rid, _)| *rid == rowid)
                                .map(|(_, r)| r.clone())
                                .expect("conflicting rowid is present in the snapshot");
                            // Evaluate the SET list and WHERE against a combined
                            // row: bare names bind to the existing row, and
                            // `excluded.col` to the rejected (proposed) row.
                            let mut combined_cols: Vec<String> = column_names.clone();
                            combined_cols
                                .extend(column_names.iter().map(|c| format!("excluded.{c}")));
                            let mut combined_row: Vec<Value> = existing_row.clone();
                            combined_row.extend(values.clone());
                            let apply = match where_clause {
                                Some(w) => is_truthy(&eval(w, &combined_row, &combined_cols)?),
                                None => true,
                            };
                            if apply {
                                let mut new_row = existing_row;
                                for (col, expr) in assignments {
                                    let idx = column_names
                                        .iter()
                                        .position(|c| c == col)
                                        .ok_or_else(|| DbError::UnknownColumn {
                                            table: table.to_string(),
                                            column: col.clone(),
                                        })?;
                                    new_row[idx] = eval(expr, &combined_row, &combined_cols)?;
                                }
                                // Validate the updated row as any written row is.
                                for (i, (name, not_null, _)) in col_meta.iter().enumerate() {
                                    if *not_null && matches!(new_row[i], Value::Null) {
                                        return Err(DbError::Constraint(format!(
                                            "column {name} cannot be NULL"
                                        )));
                                    }
                                }
                                self.enforce_checks(table, &column_names, &new_row)?;
                                self.enforce_fk_child(txn, table, &column_names, &new_row)?;
                                // Reflect the change in the snapshot so a later
                                // row of this statement sees the updated value.
                                if let Some(slot) =
                                    existing.iter_mut().find(|(rid, _)| *rid == rowid)
                                {
                                    slot.1.clone_from(&new_row);
                                }
                                plans.push(RowPlan::Update { rowid, new_row });
                            } else {
                                plans.push(RowPlan::Skip);
                            }
                        } else if intra {
                            return Err(DbError::Unsupported(
                                "ON CONFLICT DO UPDATE cannot affect one row twice in a statement"
                                    .into(),
                            ));
                        } else {
                            planned.push(values.clone());
                            plans.push(RowPlan::Insert(values));
                        }
                    }
                },
            }
        }

        // Execute the plan under the mutable table borrow.
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
        let mut inserted: Vec<Vec<Value>> = Vec::with_capacity(plans.len());
        for plan in plans {
            match plan {
                RowPlan::Skip => {}
                RowPlan::Insert(values) => {
                    let rowid = store.next_rowid;
                    handle.insert(txn, rowid, &encode_row(&values, &schema)?)?;
                    for sec in &mut store.secondary {
                        let index = Index::open(&self.pool, sec.root);
                        index.put(&values[sec.column], rowid)?;
                        sec.root = index.root();
                    }
                    store.next_rowid += 1;
                    inserted.push(values);
                }
                RowPlan::Update { rowid, new_row } => {
                    handle.update(txn, rowid, &encode_row(&new_row, &schema)?)?;
                    for sec in &mut store.secondary {
                        let index = Index::open(&self.pool, sec.root);
                        index.put(&new_row[sec.column], rowid)?;
                        sec.root = index.root();
                    }
                    inserted.push(new_row);
                }
            }
        }
        let affected = inserted.len();

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
                    min: None,
                    max: None,
                },
            )?;
        }
        if !returning.is_empty() {
            return project_returning(returning, &column_names, &inserted);
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

    /// Run a `WITH RECURSIVE` query. Each CTE is materialized into an in-memory
    /// relation (a self-referencing one by fixpoint iteration), registered in a
    /// scratch catalog, then the body is bound and run against those relations
    /// plus a snapshot of the base tables.
    ///
    /// Scope: the CTE and body terms read base tables and earlier CTEs only;
    /// nested subqueries inside them are not folded, and a `WITH` column-rename
    /// list is unsupported.
    fn run_with_ctes(
        &self,
        txn: &Transaction,
        ctes: &[Cte],
        body: &Statement,
    ) -> Result<QueryOutcome> {
        let source = EngineSource {
            pool: &self.pool,
            wal: self.wal.clone(),
            mgr: &self.mgr,
            catalog: &self.catalog,
            tables: &self.tables,
            txn,
        };
        // Start from a snapshot of every base table plus a scratch catalog we
        // can extend with each CTE's schema.
        let mut rels = self.materialize_tables(txn, &source)?;
        let mut cat = self.catalog.clone();

        for cte in ctes {
            if !cte.columns.is_empty() {
                return Err(DbError::Unsupported(
                    "a column list on a WITH query is not yet supported".into(),
                ));
            }
            let rel = if references_table(&cte.query, &cte.name) {
                Self::evaluate_recursive_cte(&cat, &rels, cte)?
            } else {
                let (columns, rows) = eval_in_catalog(&cat, &rels, &cte.query)?;
                Relation { columns, rows }
            };
            register_relation(&mut cat, &cte.name, &rel);
            rels.insert(cte.name.clone(), rel);
        }

        let (columns, rows) = eval_in_catalog(&cat, &rels, body)?;
        Ok(QueryOutcome::Rows { columns, rows })
    }

    /// Fixpoint-evaluate a self-referencing CTE: run the anchor term once, then
    /// repeatedly run the recursive term with the CTE bound to the rows found so
    /// far, until a round adds nothing new.
    fn evaluate_recursive_cte(
        cat: &Catalog,
        rels: &HashMap<String, Relation>,
        cte: &Cte,
    ) -> Result<Relation> {
        // A safety cap so a non-terminating recursion fails loudly instead of
        // hanging or exhausting memory.
        const MAX_ROWS: usize = 1_000_000;
        // A recursive CTE must be `anchor UNION [ALL] recursive`.
        let (anchor, recursive, all) = match cte.query.as_ref() {
            Statement::Union {
                op: SetOp::Union,
                all,
                left,
                right,
                ..
            } => (left.as_ref(), right.as_ref(), *all),
            _ => {
                return Err(DbError::Unsupported(
                    "a recursive CTE must be a UNION of an anchor term and a recursive term".into(),
                ))
            }
        };
        if references_table(anchor, &cte.name) {
            return Err(DbError::Unsupported(
                "the anchor term of a recursive CTE must not reference the CTE".into(),
            ));
        }

        // The column names come from the anchor; register the CTE so the
        // recursive term can bind against it.
        let (columns, anchor_rows) = eval_in_catalog(cat, rels, anchor)?;
        let mut cat = cat.clone();
        let mut rels = rels.clone();
        register_relation(
            &mut cat,
            &cte.name,
            &Relation {
                columns: columns.clone(),
                rows: Vec::new(),
            },
        );

        let mut all_rows = anchor_rows.clone();
        let mut working = anchor_rows;
        while !working.is_empty() {
            rels.insert(
                cte.name.clone(),
                Relation {
                    columns: columns.clone(),
                    rows: working.clone(),
                },
            );
            let (_c, produced) = eval_in_catalog(&cat, &rels, recursive)?;
            let mut fresh: Vec<Vec<Value>> = Vec::new();
            for row in produced {
                // UNION ALL keeps every produced row; UNION keeps only rows not
                // already in the result or this round.
                if all || (!all_rows.contains(&row) && !fresh.contains(&row)) {
                    fresh.push(row);
                }
            }
            if fresh.is_empty() {
                break;
            }
            all_rows.extend(fresh.iter().cloned());
            if all_rows.len() > MAX_ROWS {
                return Err(DbError::Unsupported(format!(
                    "recursive CTE {} exceeded {MAX_ROWS} rows (it may not terminate)",
                    cte.name
                )));
            }
            working = fresh;
        }
        Ok(Relation {
            columns,
            rows: all_rows,
        })
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
        // A correlated subquery survives folding as a node in the plan. When one
        // is present, build a per-row runner over a consistent snapshot of the
        // base tables and let the executor call back into it.
        if correlated::has_subquery(stmt) {
            let snapshot = self.materialize_tables(txn, &source)?;
            let runner = Rc::new(CorrelatedRunner::new(
                self.catalog.clone(),
                MaterializedSource::new(snapshot),
            ));
            return Ok(run_with(&physical, &source, runner)?);
        }
        Ok(run(&physical, &source)?)
    }

    /// Materialize every base table's visible rows under `txn`, for correlated
    /// subqueries to read repeatedly from a fixed snapshot.
    fn materialize_tables(
        &self,
        _txn: &Transaction,
        source: &EngineSource<'_>,
    ) -> Result<HashMap<String, Relation>> {
        let mut snapshot = HashMap::new();
        for name in self.tables.keys() {
            snapshot.insert(name.clone(), source.scan(name)?);
        }
        Ok(snapshot)
    }

    /// Rewrite a query, replacing every uncorrelated subquery with its result
    /// (a scalar becomes a literal; `IN (subquery)` becomes an `IN`-list).
    fn fold_query(&self, txn: &Transaction, stmt: &Statement) -> Result<Statement> {
        match stmt {
            Statement::Select(s) => Ok(Statement::Select(Box::new(self.fold_select(txn, s)?))),
            Statement::Union {
                op,
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => Ok(Statement::Union {
                op: *op,
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
    ///
    /// An uncorrelated subquery is run now and replaced with its result (a
    /// literal, or an `IN`-list). A correlated one (it references an outer
    /// column) is left in place for the executor to evaluate per row via the
    /// [`crate::correlated::CorrelatedRunner`].
    #[allow(clippy::too_many_lines)]
    fn fold_expr(&self, txn: &Transaction, expr: &Expr) -> Result<Expr> {
        match expr {
            Expr::Subquery(q) => {
                if correlated::is_correlated(&self.catalog, q) {
                    Ok(expr.clone())
                } else {
                    Ok(Expr::Literal(self.scalar_subquery(txn, q)?))
                }
            }
            Expr::InSubquery {
                expr,
                query,
                negated,
            } => {
                if correlated::is_correlated(&self.catalog, query) {
                    // Keep the node, but fold any subqueries in the outer LHS.
                    Ok(Expr::InSubquery {
                        expr: Box::new(self.fold_expr(txn, expr)?),
                        query: query.clone(),
                        negated: *negated,
                    })
                } else {
                    let lhs = self.fold_expr(txn, expr)?;
                    let values = self.column_subquery(txn, query)?;
                    Ok(in_list_expr(&lhs, &values, *negated))
                }
            }
            Expr::Exists(query) => {
                if correlated::is_correlated(&self.catalog, query) {
                    Ok(expr.clone())
                } else {
                    let folded = self.fold_query(txn, query)?;
                    let (_cols, rows) = self.execute_query(txn, &folded)?;
                    Ok(Expr::Literal(Value::Bool(!rows.is_empty())))
                }
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
            Expr::Window {
                func,
                distinct,
                args,
                partition_by,
                order_by,
            } => Ok(Expr::Window {
                func: func.clone(),
                distinct: *distinct,
                args: args
                    .iter()
                    .map(|a| self.fold_expr(txn, a))
                    .collect::<Result<_>>()?,
                partition_by: partition_by
                    .iter()
                    .map(|a| self.fold_expr(txn, a))
                    .collect::<Result<_>>()?,
                order_by: order_by
                    .iter()
                    .map(|o| {
                        Ok(rustdb_sql::statement::OrderItem {
                            expr: self.fold_expr(txn, &o.expr)?,
                            desc: o.desc,
                        })
                    })
                    .collect::<Result<_>>()?,
            }),
            // Leaves carry no nested expressions.
            Expr::Column(_)
            | Expr::QualifiedColumn(..)
            | Expr::Literal(_)
            | Expr::Parameter(_)
            | Expr::Star => Ok(expr.clone()),
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
        returning: &[SelectItem],
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

        // Read anchors immutably for the validation scan (constraint checks
        // read other tables through `&self`).
        let (index_root, version_page) = {
            let s = self
                .tables
                .get(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            (s.index_root, s.version_page)
        };
        let read = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            index_root,
            version_page,
        );

        // Pass 1: find matching rows and validate the new versions.
        let mut updates: Vec<(u64, Vec<Value>)> = Vec::new();
        for (key, bytes) in read.scan(txn)? {
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
            self.enforce_checks(table, &columns, &new_row)?;
            self.enforce_fk_child(txn, table, &columns, &new_row)?;
            self.enforce_fk_parent_update(txn, table, &columns, &row, &new_row)?;
            updates.push((key, new_row));
        }

        // Pass 2: apply the validated updates.
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
        let mut new_rows: Vec<Vec<Value>> = Vec::with_capacity(updates.len());
        for (key, new_row) in updates {
            handle.update(txn, key, &encode_row(&new_row, &schema)?)?;
            // Point each indexed column's key at this rowid's new value. Old
            // values are left in the tree (upsert only, never delete) and are
            // filtered out on read; see `crate::index`.
            for sec in &mut store.secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&new_row[sec.column], key)?;
                sec.root = index.root();
            }
            new_rows.push(new_row);
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        if !returning.is_empty() {
            return project_returning(returning, &columns, &new_rows);
        }
        Ok(QueryOutcome::Mutation {
            affected: new_rows.len(),
        })
    }

    fn run_delete(
        &mut self,
        txn: &Transaction,
        table: &str,
        where_clause: Option<&Expr>,
        returning: &[SelectItem],
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

        // Read anchors immutably for the validation scan.
        let (index_root, version_page) = {
            let s = self
                .tables
                .get(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            (s.index_root, s.version_page)
        };
        let read = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            index_root,
            version_page,
        );

        // Pass 1: find matching rows; reject any deletion a child still
        // references (foreign-key RESTRICT). Keep each deleted row for RETURNING.
        let mut victims: Vec<(u64, Vec<Value>)> = Vec::new();
        for (key, bytes) in read.scan(txn)? {
            let row = decode_row(&bytes, &schema)?;
            if let Some(pred) = where_clause {
                if !is_truthy(&eval(pred, &row, &columns)?) {
                    continue;
                }
            }
            self.enforce_fk_restrict(txn, table, &columns, &row)?;
            victims.push((key, row));
        }

        // Pass 2: delete.
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
        let mut deleted: Vec<Vec<Value>> = Vec::with_capacity(victims.len());
        for (key, row) in victims {
            handle.delete(txn, key)?;
            deleted.push(row);
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        if !returning.is_empty() {
            return project_returning(returning, &columns, &deleted);
        }
        Ok(QueryOutcome::Mutation {
            affected: deleted.len(),
        })
    }

    fn run_explain(&self, stmt: &Statement) -> Result<QueryOutcome> {
        let Statement::Explain { analyze, statement } = stmt else {
            unreachable!("guarded by execute");
        };
        match statement.as_ref() {
            Statement::Select(_) | Statement::Union { .. } => {
                // Fold subqueries under a transient read snapshot so EXPLAIN
                // plans the same query the executor would run.
                let txn = self.mgr.begin();
                let folded = self.fold_query(&txn, statement)?;
                let logical = bind(&self.catalog, &folded)?;
                let physical = plan(&logical, &self.catalog)?;
                let mut out = explain(&physical);
                if *analyze {
                    // ANALYZE actually runs the query and reports the real row
                    // count and wall-clock time alongside the estimates above.
                    use std::fmt::Write as _;
                    let start = std::time::Instant::now();
                    let (_columns, rows) = self.execute_query(&txn, &folded)?;
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    let _ = write!(out, "Execution: actual rows={} time={ms:.3}ms", rows.len());
                }
                Ok(QueryOutcome::Explain(out))
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

impl EngineSource<'_> {
    /// Build a system view's rows from the live catalog. `table` must be a known
    /// `information_schema` view.
    fn system_relation(&self, table: &str) -> Relation {
        let schema = system_table_schema(table).expect("a known system table");
        let columns: Vec<String> = schema.iter().map(|(c, _)| (*c).to_string()).collect();
        // List user tables only, in a stable order. The system views are
        // catalog-only (absent from `tables`), so they exclude themselves.
        let mut names: Vec<&String> = self.tables.keys().collect();
        names.sort();
        let mut rows: Vec<Vec<Value>> = Vec::new();
        match table {
            "information_schema.tables" => {
                for name in names {
                    rows.push(vec![
                        Value::Text(name.clone()),
                        Value::Text("BASE TABLE".to_string()),
                    ]);
                }
            }
            "information_schema.columns" => {
                for name in names {
                    let Some(meta) = self.catalog.get_table(name) else {
                        continue;
                    };
                    for (i, col) in meta.columns.iter().enumerate() {
                        rows.push(vec![
                            Value::Text(name.clone()),
                            Value::Text(col.name.clone()),
                            Value::Int(i64::try_from(i + 1).unwrap_or(i64::MAX)),
                            Value::Text(sql_type_name(col.ty).to_string()),
                            Value::Text(if col.not_null { "NO" } else { "YES" }.to_string()),
                        ]);
                    }
                }
            }
            _ => {}
        }
        Relation { columns, rows }
    }
}

impl TableSource for EngineSource<'_> {
    fn scan(&self, table: &str) -> std::result::Result<Relation, rustdb_executor::ExecError> {
        use rustdb_executor::ExecError;
        // The information_schema views are computed from the catalog, not stored.
        if system_table_schema(table).is_some() {
            return Ok(self.system_relation(table));
        }
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

/// The column name a `CREATE TABLE AS` gives a result column: the bare name
/// (after any qualifier), so `t.id` becomes `id`.
fn ctas_column_name(col: &str) -> String {
    col.rsplit('.').next().unwrap_or(col).to_string()
}

/// Infer a column's type from the first non-NULL value in `rows` at index `i`,
/// defaulting to `INT` for an all-NULL or empty column.
fn column_type(rows: &[Vec<Value>], i: usize) -> DataType {
    rows.iter()
        .find_map(|r| match r.get(i) {
            Some(Value::Int(_)) => Some(DataType::Int),
            Some(Value::Float(_)) => Some(DataType::Float),
            Some(Value::Text(_)) => Some(DataType::Text),
            Some(Value::Bool(_)) => Some(DataType::Bool),
            _ => None,
        })
        .unwrap_or(DataType::Int)
}

/// A table's freshly computed `(column name, statistics)` list.
type ColumnStatList = Vec<(String, ColumnStats)>;

/// Compute one column's statistics over `rows`: its distinct count (NULLs
/// excluded) and, for an integer column, its min and max.
fn analyze_column(rows: &[Vec<Value>], col: usize) -> ColumnStats {
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    let mut min: Option<i64> = None;
    let mut max: Option<i64> = None;
    for row in rows {
        let Some(value) = row.get(col) else { continue };
        if matches!(value, Value::Null) {
            continue;
        }
        seen.insert(stat_key(value));
        if let Value::Int(n) = value {
            min = Some(min.map_or(*n, |m| m.min(*n)));
            max = Some(max.map_or(*n, |m| m.max(*n)));
        }
    }
    ColumnStats {
        distinct: u64::try_from(seen.len()).unwrap_or(u64::MAX).max(1),
        min,
        max,
    }
}

/// A canonical byte key for a value, so equal values hash equal when counting
/// distinct values.
fn stat_key(value: &Value) -> Vec<u8> {
    let mut b = Vec::new();
    match value {
        Value::Null => b.push(0),
        Value::Int(n) => {
            b.push(1);
            b.extend_from_slice(&n.to_le_bytes());
        }
        Value::Text(s) => {
            b.push(2);
            b.extend_from_slice(s.as_bytes());
        }
        Value::Bool(x) => {
            b.push(3);
            b.push(u8::from(*x));
        }
        Value::Float(x) => {
            b.push(4);
            b.extend_from_slice(&x.to_bits().to_le_bytes());
        }
    }
    b
}

/// The `information_schema` views the engine exposes for introspection.
const SYSTEM_TABLES: [&str; 2] = ["information_schema.tables", "information_schema.columns"];

/// The fixed schema of a system view, or `None` if `name` is not one. Used both
/// to register the view in the catalog and to build its rows on scan.
fn system_table_schema(name: &str) -> Option<Vec<(&'static str, DataType)>> {
    match name {
        "information_schema.tables" => Some(vec![
            ("table_name", DataType::Text),
            ("table_type", DataType::Text),
        ]),
        "information_schema.columns" => Some(vec![
            ("table_name", DataType::Text),
            ("column_name", DataType::Text),
            ("ordinal_position", DataType::Int),
            ("data_type", DataType::Text),
            ("is_nullable", DataType::Text),
        ]),
        _ => None,
    }
}

/// The SQL type name reported by `information_schema.columns`, in the
/// Postgres-style spelling tools expect.
const fn sql_type_name(ty: DataType) -> &'static str {
    match ty {
        DataType::Int => "integer",
        DataType::Float => "double precision",
        DataType::Bool => "boolean",
        DataType::Text => "text",
    }
}

/// Bind, plan, and run `stmt` against a scratch catalog and a set of in-memory
/// relations (base tables plus already-evaluated CTEs). Used by the recursive
/// `WITH` evaluator, which works with relations rather than the live store.
fn eval_in_catalog(
    cat: &Catalog,
    rels: &HashMap<String, Relation>,
    stmt: &Statement,
) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let logical = bind(cat, stmt)?;
    let physical = plan(&logical, cat)?;
    let source = MaterializedSource::new(rels.clone());
    Ok(run(&physical, &source)?)
}

/// Register `rel` as a table named `name` in `cat`, shadowing any existing
/// table of that name (a CTE shadows a base table). Column types are inferred
/// from the relation's data; they only steer name resolution and costing, since
/// the rows are served directly.
fn register_relation(cat: &mut Catalog, name: &str, rel: &Relation) {
    let _ = cat.apply(&Statement::DropTable {
        name: name.to_string(),
    });
    let create = Statement::CreateTable {
        name: name.to_string(),
        columns: infer_columns(rel),
        constraints: Vec::new(),
    };
    let _ = cat.apply(&create);
}

/// Infer a `ColumnDef` per relation column, taking each column's type from its
/// first non-NULL value (defaulting to `INT` for an all-NULL or empty column).
fn infer_columns(rel: &Relation) -> Vec<ColumnDef> {
    rel.columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let ty = rel
                .rows
                .iter()
                .find_map(|r| match r.get(i) {
                    Some(Value::Int(_)) => Some(DataType::Int),
                    Some(Value::Float(_)) => Some(DataType::Float),
                    Some(Value::Text(_)) => Some(DataType::Text),
                    Some(Value::Bool(_)) => Some(DataType::Bool),
                    _ => None,
                })
                .unwrap_or(DataType::Int);
            ColumnDef {
                name: name.clone(),
                ty,
                primary_key: false,
                not_null: false,
                unique: false,
                default: None,
                serial: false,
            }
        })
        .collect()
}

/// Inline a top-level non-recursive `WITH` (or `EXPLAIN WITH`) into a plain
/// query. A `WITH RECURSIVE` is left intact for the runtime evaluator
/// ([`Database::run_with_ctes`]); every other statement is untouched.
fn expand_ctes(stmt: Statement) -> Result<Statement> {
    match stmt {
        // A recursive WITH can't be a static rewrite; the engine evaluates it
        // at run time and routes it through `run_with_ctes`.
        Statement::With {
            recursive: true, ..
        } => Ok(stmt),
        Statement::With {
            recursive: false,
            ctes,
            body,
        } => inline_with(&ctes, &body),
        Statement::Explain { analyze, statement } => Ok(Statement::Explain {
            analyze,
            statement: Box::new(expand_ctes(*statement)?),
        }),
        other => Ok(other),
    }
}

/// Rewrite every reference to a CTE in `body` (and in later CTEs) into a derived
/// table over the CTE's query. CTEs are processed in order so a later one may
/// reference an earlier one. A self-reference here means `RECURSIVE` was
/// omitted, which is rejected.
fn inline_with(ctes: &[Cte], body: &Statement) -> Result<Statement> {
    let mut resolved: HashMap<String, Statement> = HashMap::new();
    for cte in ctes {
        if !cte.columns.is_empty() {
            return Err(DbError::Unsupported(
                "a column list on a WITH query is not yet supported".into(),
            ));
        }
        // A CTE that names itself would need RECURSIVE; reject it clearly.
        if references_table(&cte.query, &cte.name) {
            return Err(DbError::Unsupported(format!(
                "CTE {} references itself; WITH RECURSIVE is required and not yet supported",
                cte.name
            )));
        }
        let inlined = rewrite_cte_query(&cte.query, &resolved);
        resolved.insert(cte.name.clone(), inlined);
    }
    Ok(rewrite_cte_query(body, &resolved))
}

/// Rewrite CTE references in a query node (a `Select` or set operation),
/// recursing through set-operation branches.
fn rewrite_cte_query(stmt: &Statement, ctes: &HashMap<String, Statement>) -> Statement {
    match stmt {
        Statement::Select(s) => Statement::Select(Box::new(rewrite_cte_select(s, ctes))),
        Statement::Union {
            op,
            all,
            left,
            right,
            order_by,
            limit,
            offset,
        } => Statement::Union {
            op: *op,
            all: *all,
            left: Box::new(rewrite_cte_query(left, ctes)),
            right: Box::new(rewrite_cte_query(right, ctes)),
            order_by: order_by.clone(),
            limit: *limit,
            offset: *offset,
        },
        other => other.clone(),
    }
}

/// Rewrite CTE references in a `Select`'s FROM and JOIN relations.
fn rewrite_cte_select(s: &Select, ctes: &HashMap<String, Statement>) -> Select {
    let mut out = s.clone();
    out.from = rewrite_cte_table_ref(&s.from, ctes);
    out.joins = s
        .joins
        .iter()
        .map(|j| Join {
            kind: j.kind,
            table: rewrite_cte_table_ref(&j.table, ctes),
            on: j.on.clone(),
        })
        .collect();
    out
}

/// Turn a reference to a CTE into a derived table over its query; recurse into
/// an existing derived table's own subquery. A plain table reference is left
/// as is.
fn rewrite_cte_table_ref(tr: &TableRef, ctes: &HashMap<String, Statement>) -> TableRef {
    if tr.subquery.is_none() {
        if let Some(query) = ctes.get(&tr.name) {
            return TableRef {
                name: String::new(),
                // A reference keeps its alias; an unaliased one is qualified by
                // the CTE name (as the planner does for any derived table).
                alias: Some(tr.alias.clone().unwrap_or_else(|| tr.name.clone())),
                subquery: Some(Box::new(query.clone())),
            };
        }
        return tr.clone();
    }
    TableRef {
        name: tr.name.clone(),
        alias: tr.alias.clone(),
        subquery: tr
            .subquery
            .as_ref()
            .map(|q| Box::new(rewrite_cte_query(q, ctes))),
    }
}

/// Whether a query references a base table named `name` in any FROM or JOIN
/// (recursing through derived tables and set operations). Used to detect a
/// self-referencing (recursive) CTE.
fn references_table(stmt: &Statement, name: &str) -> bool {
    match stmt {
        Statement::Select(s) => {
            table_ref_names(&s.from, name)
                || s.joins.iter().any(|j| table_ref_names(&j.table, name))
        }
        Statement::Union { left, right, .. } => {
            references_table(left, name) || references_table(right, name)
        }
        _ => false,
    }
}

/// Whether a table reference (or a derived table's inner query) names `name`.
fn table_ref_names(tr: &TableRef, name: &str) -> bool {
    tr.subquery
        .as_ref()
        .map_or_else(|| tr.name == name, |q| references_table(q, name))
}

/// The planned outcome for one row of an `INSERT ... ON CONFLICT`.
enum RowPlan {
    /// Write the row as a fresh tuple.
    Insert(Vec<Value>),
    /// Drop the row (a `DO NOTHING` conflict, or a `DO UPDATE` whose `WHERE`
    /// was false).
    Skip,
    /// Overwrite the conflicting existing row (`rowid`) with `new_row`.
    Update { rowid: u64, new_row: Vec<Value> },
}

/// Whether `cand` and `row` are equal on every column in `cols`, treating a
/// NULL on either side as never matching (SQL UNIQUE semantics). An empty
/// `cols` never matches.
fn rows_match_on(cand: &[Value], row: &[Value], cols: &[usize]) -> bool {
    !cols.is_empty()
        && cols.iter().all(|&c| {
            !matches!(cand[c], Value::Null) && !matches!(row[c], Value::Null) && cand[c] == row[c]
        })
}

/// Desugar `lhs [NOT] IN (v1, v2, ...)` to a chain of equalities, the same
/// shape the parser produces for a literal `IN`-list. An empty set is a
/// constant: `IN ()` is false, `NOT IN ()` is true.
/// Project a `RETURNING` list over the affected rows, producing a result set.
/// `columns` are the table's column names, aligned with each row in `rows`.
fn project_returning(
    returning: &[SelectItem],
    columns: &[String],
    rows: &[Vec<Value>],
) -> Result<QueryOutcome> {
    let mut out_columns = Vec::new();
    let mut exprs: Vec<Expr> = Vec::new();
    for item in returning {
        match item {
            SelectItem::Star => {
                for c in columns {
                    out_columns.push(c.clone());
                    exprs.push(Expr::Column(c.clone()));
                }
            }
            SelectItem::Expr(e, alias) => {
                out_columns.push(alias.clone().unwrap_or_else(|| returning_name(e)));
                exprs.push(e.clone());
            }
        }
    }
    let mut out_rows = Vec::with_capacity(rows.len());
    for row in rows {
        let r = exprs
            .iter()
            .map(|e| eval(e, row, columns).map_err(DbError::from))
            .collect::<Result<Vec<_>>>()?;
        out_rows.push(r);
    }
    Ok(QueryOutcome::Rows {
        columns: out_columns,
        rows: out_rows,
    })
}

/// The output column name for a `RETURNING` item: its column name, else its
/// printed form.
fn returning_name(e: &Expr) -> String {
    match e {
        Expr::Column(n) | Expr::QualifiedColumn(_, n) => n.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn in_list_expr(lhs: &Expr, values: &[Value], negated: bool) -> Expr {
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
        assert!(plan.contains("UNION ALL"), "plan was:\n{plan}");
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

    /// Collect the single INT column of each row into a sorted Vec for
    /// order-insensitive comparison of set-operation output.
    fn int_col_sorted(rows: &[Vec<Value>]) -> Vec<i64> {
        let mut v: Vec<i64> = rows
            .iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref o => panic!("want int, got {o:?}"),
            })
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn intersect_keeps_only_common_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2), (2), (3)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (2), (3), (3), (4)")
            .unwrap();
        // INTERSECT is distinct: the shared values 2 and 3, once each.
        let (_c, rows) = query(&mut db, "SELECT x FROM a INTERSECT SELECT y FROM b");
        assert_eq!(int_col_sorted(&rows), vec![2, 3]);
    }

    #[test]
    fn except_subtracts_the_right_side() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2), (2), (3)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (2), (4)").unwrap();
        // EXCEPT is distinct: values in a not present in b, namely 1 and 3.
        let (_c, rows) = query(&mut db, "SELECT x FROM a EXCEPT SELECT y FROM b");
        assert_eq!(int_col_sorted(&rows), vec![1, 3]);
    }

    #[test]
    fn intersect_all_keeps_minimum_multiplicity() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (2), (2), (2)").unwrap();
        db.execute("INSERT INTO b VALUES (2), (2)").unwrap();
        // min(3, 2) = 2 copies of the value 2.
        let (_c, rows) = query(&mut db, "SELECT x FROM a INTERSECT ALL SELECT y FROM b");
        assert_eq!(int_col_sorted(&rows), vec![2, 2]);
    }

    #[test]
    fn except_all_subtracts_multiplicity() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (2), (2), (2), (5)")
            .unwrap();
        db.execute("INSERT INTO b VALUES (2)").unwrap();
        // max(0, 3 - 1) = 2 copies of 2, plus the lone 5.
        let (_c, rows) = query(&mut db, "SELECT x FROM a EXCEPT ALL SELECT y FROM b");
        assert_eq!(int_col_sorted(&rows), vec![2, 2, 5]);
    }

    #[test]
    fn intersect_explains_with_its_keyword() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        let plan = match db
            .execute("EXPLAIN SELECT x FROM a EXCEPT SELECT y FROM b")
            .unwrap()
        {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("EXCEPT"), "plan was:\n{plan}");
    }

    // --- WITH (common table expressions) ---

    #[test]
    fn with_cte_is_usable_as_a_table() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 5), (2, 15), (3, 25)")
            .unwrap();
        let (cols, rows) = query(
            &mut db,
            "WITH big AS (SELECT id FROM t WHERE n > 10) SELECT id FROM big",
        );
        assert_eq!(names(&cols), ["id"]);
        assert_eq!(int_col_sorted(&rows), vec![2, 3]);
    }

    #[test]
    fn with_cte_can_be_joined_to_itself() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 5), (2, 15)").unwrap();
        // The same CTE referenced under two aliases, joined on the key.
        let (_c, rows) = query(
            &mut db,
            "WITH c AS (SELECT id FROM t WHERE n > 0) SELECT a.id FROM c AS a INNER JOIN c AS b ON a.id = b.id",
        );
        assert_eq!(int_col_sorted(&rows), vec![1, 2]);
    }

    #[test]
    fn with_later_cte_references_earlier_one() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        let (_c, rows) = query(
            &mut db,
            "WITH a AS (SELECT id FROM t), b AS (SELECT id FROM a WHERE id > 1) SELECT id FROM b",
        );
        assert_eq!(int_col_sorted(&rows), vec![2, 3]);
    }

    #[test]
    fn with_recursive_counts_up() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE seed (n INT)").unwrap();
        db.execute("INSERT INTO seed VALUES (1)").unwrap();
        // Classic counter: the anchor seeds 1, the recursive term adds one until
        // the guard stops it.
        let (cols, rows) = query(
            &mut db,
            "WITH RECURSIVE c AS (SELECT n FROM seed UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT n FROM c",
        );
        assert_eq!(names(&cols), ["n"]);
        assert_eq!(int_col_sorted(&rows), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn with_recursive_computes_transitive_closure() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE edges (src INT, dst INT)").unwrap();
        db.execute("INSERT INTO edges VALUES (1, 2), (2, 3), (3, 4)")
            .unwrap();
        // Everything reachable from node 1, with UNION dedup ending the walk.
        let (_c, rows) = query(
            &mut db,
            "WITH RECURSIVE reach AS (SELECT dst FROM edges WHERE src = 1 UNION SELECT e.dst FROM edges AS e INNER JOIN reach AS r ON e.src = r.dst) SELECT dst FROM reach",
        );
        assert_eq!(int_col_sorted(&rows), vec![2, 3, 4]);
    }

    #[test]
    fn with_recursive_non_union_body_is_rejected() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        // A self-referencing CTE that is not a UNION cannot be a fixpoint.
        let err = db
            .execute("WITH RECURSIVE c AS (SELECT id FROM c) SELECT id FROM c")
            .unwrap_err();
        assert!(matches!(err, DbError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn with_self_reference_without_recursive_is_rejected() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        let err = db
            .execute("WITH c AS (SELECT id FROM c) SELECT id FROM c")
            .unwrap_err();
        assert!(matches!(err, DbError::Unsupported(_)), "got {err:?}");
    }

    // --- information_schema ---

    #[test]
    fn information_schema_tables_lists_user_tables() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE alpha (id INT)").unwrap();
        db.execute("CREATE TABLE beta (id INT, name TEXT)").unwrap();
        // Only user tables appear; the views exclude themselves.
        let (cols, rows) = query(
            &mut db,
            "SELECT table_name, table_type FROM information_schema.tables ORDER BY table_name",
        );
        assert_eq!(names(&cols), ["table_name", "table_type"]);
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Text("alpha".into()),
                    Value::Text("BASE TABLE".into())
                ],
                vec![Value::Text("beta".into()), Value::Text("BASE TABLE".into())],
            ]
        );
    }

    #[test]
    fn information_schema_columns_describes_a_table() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        // A PRIMARY KEY column is NOT NULL; a plain column is nullable.
        let (_c, rows) = query(
            &mut db,
            "SELECT column_name, ordinal_position, data_type, is_nullable FROM information_schema.columns WHERE table_name = 't' ORDER BY ordinal_position",
        );
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Text("id".into()),
                    Value::Int(1),
                    Value::Text("integer".into()),
                    Value::Text("NO".into()),
                ],
                vec![
                    Value::Text("name".into()),
                    Value::Int(2),
                    Value::Text("text".into()),
                    Value::Text("YES".into()),
                ],
            ]
        );
    }

    #[test]
    fn information_schema_can_be_aliased_and_counted() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (x INT)").unwrap();
        // An aggregate over a system view, with an alias on the qualified name.
        let (_c, rows) = query(
            &mut db,
            "SELECT COUNT(*) FROM information_schema.tables AS t",
        );
        assert_eq!(rows, vec![vec![Value::Int(2)]]);
    }

    // --- ANALYZE ---

    #[test]
    fn analyze_records_distinct_and_min_max() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (10, 'a'), (20, 'b'), (20, 'c'), (30, 'a')")
            .unwrap();
        assert_eq!(
            db.execute("ANALYZE t").unwrap(),
            QueryOutcome::Message("ANALYZE")
        );
        let meta = db.catalog.get_table("t").expect("table");
        let id = meta.column_stats("id");
        assert_eq!(id.distinct, 3, "10, 20, 30");
        assert_eq!(id.min, Some(10));
        assert_eq!(id.max, Some(30));
        let name = meta.column_stats("name");
        assert_eq!(name.distinct, 3, "a, b, c");
        assert_eq!(name.min, None, "text has no integer min/max");
        assert_eq!(meta.stats.row_count, 4);
    }

    #[test]
    fn analyze_all_tables_and_unknown_errors() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (x INT)").unwrap();
        db.execute("CREATE TABLE b (y INT)").unwrap();
        db.execute("INSERT INTO a VALUES (1), (2)").unwrap();
        db.execute("INSERT INTO b VALUES (9)").unwrap();
        // Bare ANALYZE covers every table.
        db.execute("ANALYZE").unwrap();
        assert_eq!(
            db.catalog.get_table("a").unwrap().column_stats("x").max,
            Some(2)
        );
        assert_eq!(
            db.catalog.get_table("b").unwrap().column_stats("y").min,
            Some(9)
        );
        // A named, unknown table is an error.
        assert!(db.execute("ANALYZE ghost").is_err());
    }

    #[test]
    fn explain_analyze_reports_actual_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        // EXPLAIN ANALYZE runs the query and appends the real row count.
        let plan = match db
            .execute("EXPLAIN ANALYZE SELECT id FROM t WHERE id > 1")
            .unwrap()
        {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(
            plan.contains("Execution: actual rows=2"),
            "plan was:\n{plan}"
        );
        // Plain EXPLAIN never runs the query, so it has no execution line.
        let bare = match db.execute("EXPLAIN SELECT id FROM t").unwrap() {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(!bare.contains("Execution:"), "plan was:\n{bare}");
    }

    // --- VACUUM ---

    #[test]
    fn vacuum_keeps_visible_rows_and_rewrites_storage() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
            .unwrap();
        db.execute("UPDATE t SET n = 99 WHERE id = 2").unwrap(); // a dead version
        db.execute("DELETE FROM t WHERE id = 3").unwrap(); // a tombstone
        let before = db.tables.get("t").unwrap().version_page;
        assert_eq!(
            db.execute("VACUUM t").unwrap(),
            QueryOutcome::Message("VACUUM")
        );
        let after = db.tables.get("t").unwrap().version_page;
        assert_ne!(before, after, "VACUUM rewrites into fresh storage");
        // The live data is exactly the current visible rows.
        let (_c, rows) = query(&mut db, "SELECT id, n FROM t ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(99)],
            ]
        );
    }

    #[test]
    fn vacuum_rebuilds_a_usable_index() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
        db.execute("VACUUM t").unwrap();
        // A point lookup on the rebuilt primary index still finds the row.
        let (_c, rows) = query(&mut db, "SELECT n FROM t WHERE id = 2");
        assert_eq!(rows, vec![vec![Value::Int(20)]]);
    }

    #[test]
    fn vacuum_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, n INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 1), (2, 2)").unwrap();
            db.execute("UPDATE t SET n = 5 WHERE id = 1").unwrap();
            db.execute("VACUUM").unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id, n FROM t ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(5)],
                vec![Value::Int(2), Value::Int(2)],
            ]
        );
    }

    #[test]
    fn vacuum_inside_transaction_is_rejected() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("BEGIN").unwrap();
        let err = db.execute("VACUUM t").unwrap_err();
        assert!(matches!(err, DbError::Unsupported(_)), "got {err:?}");
        db.execute("ROLLBACK").unwrap();
    }

    // --- CREATE TABLE AS SELECT ---

    #[test]
    fn create_table_as_copies_query_result() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE src (id INT, n INT, label TEXT)")
            .unwrap();
        db.execute("INSERT INTO src VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c')")
            .unwrap();
        let out = db
            .execute("CREATE TABLE big AS SELECT id, label FROM src WHERE n >= 20")
            .unwrap();
        assert_eq!(out, QueryOutcome::Mutation { affected: 2 });
        // The new table holds the projected, filtered rows.
        let (cols, rows) = query(&mut db, "SELECT id, label FROM big ORDER BY id");
        assert_eq!(names(&cols), ["id", "label"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Text("b".into())],
                vec![Value::Int(3), Value::Text("c".into())],
            ]
        );
        // Its columns are typed from the data (id INT, label TEXT).
        let (_c, types) = query(
            &mut db,
            "SELECT column_name, data_type FROM information_schema.columns WHERE table_name = 'big' ORDER BY ordinal_position",
        );
        assert_eq!(
            types,
            vec![
                vec![Value::Text("id".into()), Value::Text("integer".into())],
                vec![Value::Text("label".into()), Value::Text("text".into())],
            ]
        );
    }

    #[test]
    fn create_table_as_survives_reopen_and_rejects_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE src (id INT)").unwrap();
            db.execute("INSERT INTO src VALUES (7), (8)").unwrap();
            db.execute("CREATE TABLE copy AS SELECT id FROM src")
                .unwrap();
            // A second CREATE of the same name is rejected.
            assert!(db
                .execute("CREATE TABLE copy AS SELECT id FROM src")
                .is_err());
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id FROM copy ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(7)], vec![Value::Int(8)]]);
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
    fn correlated_scalar_subquery_per_group() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Each employee earning above their own department's average.
        // eng avg = 80 (a passes, b not); sales avg = 60 (c passes, d not).
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM emp AS e WHERE salary > (SELECT AVG(salary) FROM emp WHERE dept = e.dept) ORDER BY name",
        );
        assert_eq!(name_set(&rows), vec!["a", "c"]);
    }

    #[test]
    fn correlated_scalar_subquery_in_projection() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // The per-department average alongside each employee.
        let (_c, rows) = query(
            &mut db,
            "SELECT name, (SELECT AVG(salary) FROM emp WHERE dept = e.dept) FROM emp AS e ORDER BY name",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Text("a".into()), Value::Int(80)],
                vec![Value::Text("b".into()), Value::Int(80)],
                vec![Value::Text("c".into()), Value::Int(60)],
                vec![Value::Text("d".into()), Value::Int(60)],
            ]
        );
    }

    #[test]
    fn correlated_exists_and_not_exists() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // EXISTS a higher-paid colleague in the same department: everyone but
        // the top earner of each department.
        let (_c, not_top) = query(
            &mut db,
            "SELECT name FROM emp AS e WHERE EXISTS (SELECT 1 FROM emp AS x WHERE x.dept = e.dept AND x.salary > e.salary) ORDER BY name",
        );
        assert_eq!(name_set(&not_top), vec!["b", "d"]);
        // NOT EXISTS the same: the top earner of each department.
        let (_c, top) = query(
            &mut db,
            "SELECT name FROM emp AS e WHERE NOT EXISTS (SELECT 1 FROM emp AS x WHERE x.dept = e.dept AND x.salary > e.salary) ORDER BY name",
        );
        assert_eq!(name_set(&top), vec!["a", "c"]);
    }

    #[test]
    fn correlated_in_subquery() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // Departments having an employee paid over 90 (only eng, via a=100).
        let (_c, rows) = query(
            &mut db,
            "SELECT name FROM dept AS d WHERE d.name IN (SELECT dept FROM emp WHERE salary > 90 AND emp.dept = d.name) ORDER BY name",
        );
        assert_eq!(name_set(&rows), vec!["eng"]);
    }

    #[test]
    fn correlated_subquery_explains() {
        let (_d, mut db) = db();
        seed_emp(&mut db);
        // The correlated subquery survives folding, so the plan still filters on
        // the EXISTS predicate rather than a constant.
        let plan = match db
            .execute("EXPLAIN SELECT name FROM emp AS e WHERE EXISTS (SELECT 1 FROM emp AS x WHERE x.dept = e.dept AND x.salary > e.salary)")
            .unwrap()
        {
            QueryOutcome::Explain(p) => p,
            other => panic!("expected explain, got {other:?}"),
        };
        assert!(plan.contains("EXISTS"), "plan was:\n{plan}");
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

    // --- RETURNING ---

    #[test]
    fn returning_on_insert_update_delete() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        // INSERT ... RETURNING returns the inserted rows.
        let (cols, rows) = query(
            &mut db,
            "INSERT INTO t VALUES (1, 'a'), (2, 'b') RETURNING id, name",
        );
        assert_eq!(names(&cols), ["id", "name"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
            ]
        );
        // UPDATE ... RETURNING returns the new versions.
        let (_c, u) = query(
            &mut db,
            "UPDATE t SET name = 'z' WHERE id = 1 RETURNING id, name",
        );
        assert_eq!(u, vec![vec![Value::Int(1), Value::Text("z".into())]]);
        // DELETE ... RETURNING * returns the deleted rows.
        let (_c, d) = query(&mut db, "DELETE FROM t WHERE id = 2 RETURNING *");
        assert_eq!(d, vec![vec![Value::Int(2), Value::Text("b".into())]]);
        // RETURNING can compute an expression with an alias.
        let (cols2, r2) = query(
            &mut db,
            "INSERT INTO t VALUES (5, 'q') RETURNING id + 1 AS next",
        );
        assert_eq!(names(&cols2), ["next"]);
        assert_eq!(r2, vec![vec![Value::Int(6)]]);
    }

    // --- SERIAL columns ---

    #[test]
    fn serial_auto_assigns_sequential_ids() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id SERIAL, name TEXT)").unwrap();
        // Omitting the serial column assigns 1, 2, 3 in insertion order, even
        // across separate statements and within a multi-row insert.
        db.execute("INSERT INTO t (name) VALUES ('a')").unwrap();
        db.execute("INSERT INTO t (name) VALUES ('b'), ('c')")
            .unwrap();
        assert_eq!(
            dump(&db, "t"),
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
                vec![Value::Int(3), Value::Text("c".into())],
            ]
        );
    }

    #[test]
    fn serial_explicit_value_advances_the_sequence() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id SERIAL, name TEXT)").unwrap();
        db.execute("INSERT INTO t (name) VALUES ('a')").unwrap();
        // An explicit value above the running max is honored.
        db.execute("INSERT INTO t (id, name) VALUES (10, 'b')")
            .unwrap();
        // The next omitted value derives from the new max, not the old count.
        db.execute("INSERT INTO t (name) VALUES ('c')").unwrap();
        assert_eq!(
            dump(&db, "t"),
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(10), Value::Text("b".into())],
                vec![Value::Int(11), Value::Text("c".into())],
            ]
        );
    }

    #[test]
    fn serial_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id SERIAL, name TEXT)").unwrap();
            db.execute("INSERT INTO t (name) VALUES ('a'), ('b')")
                .unwrap();
        }
        // After reopening, the serial registry is reloaded from the sidecar, so
        // the next omitted value continues from the persisted max (3, not 1).
        let mut db = Database::open(&path).expect("reopen");
        db.execute("INSERT INTO t (name) VALUES ('c')").unwrap();
        assert_eq!(
            dump(&db, "t"),
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
                vec![Value::Int(3), Value::Text("c".into())],
            ]
        );
    }

    #[test]
    fn serial_pairs_with_returning() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id SERIAL, name TEXT)").unwrap();
        // RETURNING surfaces the value the engine chose for the serial column.
        let (cols, rows) = query(
            &mut db,
            "INSERT INTO t (name) VALUES ('a'), ('b') RETURNING id",
        );
        assert_eq!(names(&cols), ["id"]);
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }

    // --- ON CONFLICT ---

    #[test]
    fn on_conflict_do_nothing_skips_the_duplicate() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        // The conflicting row is skipped; the fresh row is inserted; no error.
        let out = db
            .execute("INSERT INTO t VALUES (1, 99), (2, 20) ON CONFLICT DO NOTHING")
            .unwrap();
        assert_eq!(out, QueryOutcome::Mutation { affected: 1 });
        assert_eq!(
            dump(&db, "t"),
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    #[test]
    fn on_conflict_do_update_overwrites_existing() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        // The upsert updates the existing row in place, using EXCLUDED for the
        // rejected row's value, and inserts the non-conflicting row.
        db.execute(
            "INSERT INTO t VALUES (1, 5), (2, 20) ON CONFLICT (id) DO UPDATE SET n = excluded.n",
        )
        .unwrap();
        assert_eq!(
            dump(&db, "t"),
            vec![
                vec![Value::Int(1), Value::Int(5)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    #[test]
    fn on_conflict_do_update_can_read_the_existing_row() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        // SET n = n + excluded.n accumulates: existing 10 plus proposed 5.
        db.execute("INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET n = n + excluded.n")
            .unwrap();
        assert_eq!(dump(&db, "t"), vec![vec![Value::Int(1), Value::Int(15)]]);
    }

    #[test]
    fn on_conflict_do_update_where_gates_the_update() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 100)").unwrap();
        // The guard is false (100 < 5 is false), so the existing row stays.
        db.execute(
            "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE n < excluded.n",
        )
        .unwrap();
        assert_eq!(dump(&db, "t"), vec![vec![Value::Int(1), Value::Int(100)]]);
    }

    #[test]
    fn on_conflict_do_update_returns_the_final_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        let (cols, rows) = query(
            &mut db,
            "INSERT INTO t VALUES (1, 7), (2, 20) ON CONFLICT (id) DO UPDATE SET n = excluded.n RETURNING id, n",
        );
        assert_eq!(names(&cols), ["id", "n"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(7)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        );
    }

    #[test]
    fn without_on_conflict_a_duplicate_still_errors() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        // The clause is opt-in: a plain INSERT of a duplicate is still rejected.
        assert!(db.execute("INSERT INTO t VALUES (1, 99)").is_err());
    }

    #[test]
    fn on_conflict_upsert_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
                .unwrap();
            db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
            db.execute(
                "INSERT INTO t VALUES (1, 42) ON CONFLICT (id) DO UPDATE SET n = excluded.n",
            )
            .unwrap();
        }
        let db = Database::open(&path).expect("reopen");
        assert_eq!(dump(&db, "t"), vec![vec![Value::Int(1), Value::Int(42)]]);
    }

    // --- window functions ---

    #[test]
    fn window_row_number_orders_the_whole_input() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();
        let (cols, rows) = query(&mut db, "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t");
        assert_eq!(names(&cols), ["id", "ROW_NUMBER() OVER (ORDER BY id)"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Int(3), Value::Int(3)],
            ]
        );
    }

    #[test]
    fn window_row_number_restarts_per_partition() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, g INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (2, 10), (3, 20)")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT id, ROW_NUMBER() OVER (PARTITION BY g ORDER BY id) FROM t",
        );
        // g=10 holds id 1,2 -> 1,2; g=20 holds id 3 -> 1.
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Int(3), Value::Int(1)],
            ]
        );
    }

    #[test]
    fn window_rank_and_dense_rank_handle_ties() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, score INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 50), (2, 50), (3, 90)")
            .unwrap();
        // Ordered by score the two 50s tie: RANK gaps to 3, DENSE_RANK does not.
        let (_c, rows) = query(
            &mut db,
            "SELECT RANK() OVER (ORDER BY score), DENSE_RANK() OVER (ORDER BY score) FROM t",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(1), Value::Int(1)],
                vec![Value::Int(3), Value::Int(2)],
            ]
        );
    }

    #[test]
    fn window_lag_and_lead_navigate_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (10), (20), (30)").unwrap();
        // LAG/LEAD with an explicit offset and default fall off the partition
        // ends to -1.
        let (_c, rows) = query(
            &mut db,
            "SELECT id, LAG(id, 1, -1) OVER (ORDER BY id), LEAD(id, 1, -1) OVER (ORDER BY id) FROM t",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(10), Value::Int(-1), Value::Int(20)],
                vec![Value::Int(20), Value::Int(10), Value::Int(30)],
                vec![Value::Int(30), Value::Int(20), Value::Int(-1)],
            ]
        );
    }

    #[test]
    fn window_aggregate_spans_the_partition() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (g INT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 5)")
            .unwrap();
        // SUM over the partition is constant for every row in that partition.
        let (_c, rows) = query(&mut db, "SELECT g, n, SUM(n) OVER (PARTITION BY g) FROM t");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(10), Value::Int(30)],
                vec![Value::Int(1), Value::Int(20), Value::Int(30)],
                vec![Value::Int(2), Value::Int(5), Value::Int(5)],
            ]
        );
    }

    #[test]
    fn window_runs_below_an_outer_order_by() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (3), (1), (2)").unwrap();
        // The window numbers rows by id ascending; the query then sorts the
        // output descending, proving the sort sits above the window.
        let (_c, rows) = query(
            &mut db,
            "SELECT id, ROW_NUMBER() OVER (ORDER BY id) FROM t ORDER BY id DESC",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(3), Value::Int(3)],
                vec![Value::Int(2), Value::Int(2)],
                vec![Value::Int(1), Value::Int(1)],
            ]
        );
    }

    // --- CHECK constraints ---

    #[test]
    fn check_rejects_violating_insert_and_update() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT CHECK (n > 0))")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 5)").unwrap();
        // A violating insert is rejected; the boundary (0) too.
        assert!(db.execute("INSERT INTO t VALUES (2, -1)").is_err());
        assert!(db.execute("INSERT INTO t VALUES (3, 0)").is_err());
        // NULL makes the predicate unknown, which passes (SQL CHECK semantics).
        db.execute("INSERT INTO t VALUES (4, NULL)").unwrap();
        // A violating update is rejected and the row is left intact.
        assert!(db.execute("UPDATE t SET n = -9 WHERE id = 1").is_err());
        let (_c, rows) = query(&mut db, "SELECT n FROM t WHERE id = 1");
        assert_eq!(rows, vec![vec![Value::Int(5)]]);
    }

    #[test]
    fn check_table_level_spans_columns() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (lo INT, hi INT, CHECK (lo <= hi))")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 5)").unwrap();
        assert!(db.execute("INSERT INTO t VALUES (9, 2)").is_err());
    }

    // --- FOREIGN KEY constraints ---

    fn seed_fk(db: &mut Database) {
        db.execute("CREATE TABLE p (id INT PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("CREATE TABLE c (id INT, pid INT REFERENCES p (id))")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1, 'a'), (2, 'b')")
            .unwrap();
    }

    #[test]
    fn fk_insert_requires_existing_parent() {
        let (_d, mut db) = db();
        seed_fk(&mut db);
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
        // A child pointing at a non-existent parent is rejected.
        assert!(db.execute("INSERT INTO c VALUES (11, 99)").is_err());
        // A NULL reference is allowed.
        db.execute("INSERT INTO c VALUES (12, NULL)").unwrap();
        // Updating a child to a bad parent is rejected too.
        assert!(db.execute("UPDATE c SET pid = 77 WHERE id = 10").is_err());
    }

    #[test]
    fn fk_restrict_blocks_parent_delete_and_key_update() {
        let (_d, mut db) = db();
        seed_fk(&mut db);
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
        // The referenced parent cannot be deleted or have its key changed.
        assert!(db.execute("DELETE FROM p WHERE id = 1").is_err());
        assert!(db.execute("UPDATE p SET id = 5 WHERE id = 1").is_err());
        // An unreferenced parent can be deleted.
        db.execute("DELETE FROM p WHERE id = 2").unwrap();
        let (_c, rows) = query(&mut db, "SELECT id FROM p ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(1)]]);
    }

    #[test]
    fn fk_create_validates_parent_and_blocks_drop() {
        let (_d, mut db) = db();
        seed_fk(&mut db);
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
        // A foreign key to a missing table is rejected at creation.
        assert!(db
            .execute("CREATE TABLE bad (x INT, FOREIGN KEY (x) REFERENCES ghost (id))")
            .is_err());
        // A referenced table cannot be dropped while a child points at it.
        assert!(db.execute("DROP TABLE p").is_err());
    }

    #[test]
    fn fk_and_check_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cons.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute("CREATE TABLE c (id INT, pid INT REFERENCES p (id), q INT CHECK (q >= 0))")
                .unwrap();
            db.execute("INSERT INTO p VALUES (1)").unwrap();
            db.execute("INSERT INTO c VALUES (1, 1, 5)").unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        // Both constraints are still enforced after reopen.
        assert!(db.execute("INSERT INTO c VALUES (2, 99, 5)").is_err());
        assert!(db.execute("INSERT INTO c VALUES (3, 1, -1)").is_err());
        assert!(db.execute("DROP TABLE p").is_err());
        // A valid row still inserts.
        db.execute("INSERT INTO c VALUES (4, 1, 0)").unwrap();
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
