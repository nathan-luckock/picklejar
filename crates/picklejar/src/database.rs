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

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use picklejar_executor::eval::{eval, is_truthy};
use picklejar_executor::{decode_row, encode_row, run, run_with, Relation, TableSource};
use picklejar_planner::{bind, explain, plan, Catalog, ColumnStats};

use crate::correlated;
use crate::correlated::{CorrelatedRunner, MaterializedSource};
use crate::hnsw::{Hnsw, Metric};
use picklejar_sql::statement::{
    AlterAction, ColumnDef, ConflictAction, Cte, DataType, ForeignKey, Grantee, Join, OnConflict,
    OrderItem, PolicyCommand, Privilege, RefAction, RoleOption, Select, SelectItem,
    TableConstraint, TableRef,
};
use picklejar_sql::{BinOp, Expr, Parser, SetOp, Statement, UnOp, Value};
use picklejar_storage::{BufferPool, FileManager, PageId};
use picklejar_txn::{MvccTable, Transaction, TransactionManager};
use picklejar_wal::{WalSyncHandle, WalWriter};

use crate::error::{DbError, Result};
use crate::index::{Index, MultiIndex};
use crate::persist::{self, TableRecord};
use crate::security::{self, RoleAttrs, SecurityCatalog};

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
    /// Physical secondary indexes (the unique fixed-type `u64` map), one per
    /// auto-indexed column.
    secondary: Vec<SecondaryIndex>,
    /// Variable-key secondary indexes from explicit `CREATE INDEX`: any
    /// indexable type, non-unique, possibly multi-column.
    multi_secondary: Vec<MultiSecondaryIndex>,
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

/// A variable-key secondary index from `CREATE INDEX`: the indexed columns'
/// positions (one for a single-column index, more for a composite one) and the
/// root page of its [`MultiIndex`] tree.
#[derive(Debug, Clone)]
struct MultiSecondaryIndex {
    /// The index's name (so `DROP INDEX` and persistence can find it).
    name: String,
    /// Positions of the indexed columns in the table's schema, in index order.
    columns: Vec<usize>,
    /// Root page of the variable-key B+ tree.
    root: PageId,
    /// Distinct values observed in the leading column when the index was built,
    /// so the cost model can judge an equality's selectivity after a reopen
    /// (stats are otherwise in-memory only).
    distinct: u64,
    /// Whether the index enforces uniqueness of the indexed value tuple.
    unique: bool,
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
    /// What to do to this child when the parent row is deleted.
    on_delete: RefAction,
    /// What to do to this child when the parent key is updated.
    on_update: RefAction,
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

/// One row-level-security policy on a table.
#[derive(Debug, Clone)]
struct Policy {
    /// Policy name (unique within the table).
    name: String,
    /// The command(s) it applies to.
    command: PolicyCommand,
    /// The roles it applies to; empty means `PUBLIC` (every role).
    roles: Vec<String>,
    /// `USING` predicate: which existing rows are visible / affectable.
    using: Option<Expr>,
    /// `WITH CHECK` predicate: which new rows a write may produce.
    check: Option<Expr>,
}

/// The row-level-security state of one table.
#[derive(Debug, Clone, Default)]
struct TableRls {
    /// Whether RLS is enabled (policies apply).
    enabled: bool,
    /// Whether RLS is forced (applies even to the table owner).
    forced: bool,
    /// The policies declared on the table.
    policies: Vec<Policy>,
}

/// An embedded picklejar instance.
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
    /// Roles, privileges, memberships, and table ownership.
    security: SecurityCatalog,
    /// The role the session authenticated as (what `RESET ROLE` returns to and
    /// what `session_user` reports).
    session_user: String,
    /// The role privileges are currently checked against (changed by `SET ROLE`,
    /// reported by `current_user` / `current_role`).
    current_role: String,
    /// Sidecar file recording roles, grants, memberships, and ownership.
    acl_path: PathBuf,
    /// Per-table row-level-security state (flags and policies).
    rls: HashMap<String, TableRls>,
    /// Sidecar file recording the row-level-security state.
    pol_path: PathBuf,
    /// Sidecar file recording the variable-key (`CREATE INDEX`) secondary
    /// indexes (their roots and columns).
    midx_path: PathBuf,
    /// When on, a `SELECT ... ORDER BY col <op> :q LIMIT k` over a vector column
    /// (with no WHERE, join, or grouping, and where row-level security does not
    /// apply) is served from an HNSW index instead of an exact scan. Off by
    /// default, so the exact path stays the default and approximate results are an
    /// explicit opt-in.
    vector_index_on: bool,
    /// Cached HNSW indexes for the index path, keyed by `(current_role, table,
    /// column, metric)`. The whole map is cleared on any statement that is not a
    /// pure read, so an entry can never outlive a change to the data, the schema,
    /// or the grants it was built under. Keying by role means a role can only ever
    /// reuse an index it was itself permitted to build, never one another role
    /// built; keying by metric means an L2 index is never reused for a cosine query.
    vector_index_cache: HashMap<(String, String, String, Metric), CachedVectorIndex>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The storage stack is not usefully printable; show the table names.
        f.debug_struct("Database")
            .field("tables", &self.tables.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// A built HNSW index over one vector column, with the row snapshot it was built
/// from. Held in the engine's cache so repeated nearest-neighbor queries on an
/// unchanged table skip the rebuild; invalidated wholesale on any write.
struct CachedVectorIndex {
    index: crate::hnsw::Hnsw,
    rows: Vec<Vec<Value>>,
    columns: Vec<String>,
    /// Map from an index node id back to its row in `rows`.
    node_to_row: Vec<usize>,
    /// The width of the indexed vectors, to reject a wrong-width query literal.
    dim: usize,
}

/// Whether a statement only reads and so cannot invalidate a cached index. Every
/// other statement (writes, DDL, grants, transaction control, session changes)
/// clears the cache, which keeps invalidation conservative and correct by
/// construction rather than tracking exactly what each statement touched.
const fn statement_is_read_only(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Select(_)
            | Statement::Union { .. }
            | Statement::With { .. }
            | Statement::Explain { .. }
    )
}

/// Match an `ORDER BY` item against the `col <vector-op> <vector-literal>` shape
/// the HNSW index path accepts, returning the column name, distance metric, and
/// query vector. Returns `None` for anything else (a descending sort, a non-vector
/// operator, a non-column left side, or a right side that is not a vector literal).
fn match_knn_order(order: &OrderItem) -> Option<(String, crate::hnsw::Metric, Vec<f32>)> {
    use crate::hnsw::Metric;
    if order.desc {
        return None;
    }
    let Expr::Binary { op, left, right } = &order.expr else {
        return None;
    };
    let metric = match op {
        BinOp::VecL2 => Metric::L2,
        BinOp::VecCosine => Metric::Cosine,
        BinOp::VecInner => Metric::InnerProduct,
        BinOp::VecL1 => Metric::L1,
        _ => return None,
    };
    let col = match left.as_ref() {
        Expr::Column(c) | Expr::QualifiedColumn(_, c) => c.clone(),
        _ => return None,
    };
    let query = match right.as_ref() {
        Expr::Literal(Value::Vector(v)) => v.clone(),
        Expr::Literal(Value::Text(t)) => picklejar_sql::ast::parse_vector(t)?,
        _ => return None,
    };
    Some((col, metric, query))
}

/// The output shape of an index-served `SELECT`: either every column (`*`) or a
/// fixed list of source-column indexes with their output names.
enum Projection {
    Star,
    Columns(Vec<(usize, String)>),
}

impl Projection {
    /// Resolve a projection list against the table's columns, or `None` if it is
    /// anything richer than `*` or plain column references (which the index path
    /// declines so the exact evaluator can handle it).
    fn resolve(items: &[SelectItem], columns: &[String]) -> Option<Self> {
        if items.len() == 1 && matches!(items[0], SelectItem::Star) {
            return Some(Self::Star);
        }
        let mut picked = Vec::with_capacity(items.len());
        for item in items {
            let SelectItem::Expr(expr, alias) = item else {
                return None;
            };
            let name = match expr {
                Expr::Column(c) | Expr::QualifiedColumn(_, c) => c.clone(),
                _ => return None,
            };
            let idx = columns.iter().position(|c| *c == name)?;
            picked.push((idx, alias.clone().unwrap_or(name)));
        }
        Some(Self::Columns(picked))
    }

    /// The names of the output columns for this projection.
    fn output_columns(&self, columns: &[String]) -> Vec<String> {
        match self {
            Self::Star => columns.to_vec(),
            Self::Columns(picked) => picked.iter().map(|(_, name)| name.clone()).collect(),
        }
    }

    /// Project one source row into the output row this projection describes.
    fn project(&self, row: &[Value]) -> Vec<Value> {
        match self {
            Self::Star => row.to_vec(),
            Self::Columns(picked) => picked.iter().map(|(i, _)| row[*i].clone()).collect(),
        }
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
        let acl_path = base.with_extension("acl");
        let pol_path = base.with_extension("pol");
        let midx_path = base.with_extension("midx");
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
            security: SecurityCatalog::new(),
            session_user: security::BOOTSTRAP_SUPERUSER.to_string(),
            current_role: security::BOOTSTRAP_SUPERUSER.to_string(),
            acl_path,
            rls: HashMap::new(),
            pol_path,
            midx_path,
            vector_index_on: false,
            vector_index_cache: HashMap::new(),
        };
        db.load_catalog()?;
        db.load_views()?;
        db.load_constraints()?;
        db.load_serials()?;
        db.load_security()?;
        db.load_rls()?;
        db.load_multi_indexes()?;
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
                if_not_exists: false,
                name: r.name.clone(),
                columns,
                constraints: vec![],
            })?;
            for (index, column) in &r.indexes {
                self.catalog.apply(&Statement::CreateIndex {
                    name: index.clone(),
                    table: r.name.clone(),
                    columns: vec![column.clone()],
                    unique: false,
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
                    multi_secondary: Vec::new(),
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
                    on_delete,
                    on_update,
                } => self
                    .constraints
                    .entry(table)
                    .or_default()
                    .foreign_keys
                    .push(ForeignKeyMeta {
                        column,
                        parent_table,
                        parent_column,
                        on_delete: ref_action_from_token(&on_delete),
                        on_update: ref_action_from_token(&on_update),
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
                    on_delete: ref_action_token(fk.on_delete).to_string(),
                    on_update: ref_action_token(fk.on_update).to_string(),
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

    /// Load the security catalog (roles, grants, memberships, ownership) from its
    /// sidecar, rebuilding on top of the default bootstrap superuser.
    fn load_security(&mut self) -> Result<()> {
        let acl = persist::load_acl(&self.acl_path)?;
        for (name, [su, login, cr, brls, pw]) in acl.roles {
            self.security.put_role(
                &name,
                RoleAttrs {
                    superuser: su,
                    login,
                    createrole: cr,
                    bypassrls: brls,
                    has_password: pw,
                },
            );
        }
        for (grantee, table, bits) in acl.grants {
            self.security.grant(&grantee, &table, bits);
        }
        for (member, group) in acl.members {
            self.security.add_member(&member, &group);
        }
        for (table, owner) in acl.owners {
            self.security.set_owner(&table, &owner);
        }
        Ok(())
    }

    /// Snapshot the security catalog into its sidecar.
    fn save_security(&self) -> Result<()> {
        let acl = persist::AclData {
            roles: self
                .security
                .roles()
                .map(|(name, a)| {
                    (
                        name.clone(),
                        [
                            a.superuser,
                            a.login,
                            a.createrole,
                            a.bypassrls,
                            a.has_password,
                        ],
                    )
                })
                .collect(),
            grants: self
                .security
                .grants()
                .map(|(g, t, b)| (g.clone(), t.clone(), *b))
                .collect(),
            members: self
                .security
                .memberships()
                .map(|(m, g)| (m.clone(), g.clone()))
                .collect(),
            owners: self
                .security
                .owners()
                .map(|(t, o)| (t.clone(), o.clone()))
                .collect(),
        };
        persist::save_acl(&self.acl_path, &acl)?;
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
                if_not_exists: false,
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
        self.save_security()?;
        self.save_rls()?;
        self.save_multi_indexes()?;
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
        // Any statement that is not a pure read may change the data, schema, or
        // grants a cached index was built under, so drop every cached index. This
        // is the cache's whole invalidation rule: conservative, but it makes a
        // stale or wrongly-scoped index impossible to serve.
        if !self.vector_index_cache.is_empty() && !statement_is_read_only(&stmt) {
            self.vector_index_cache.clear();
        }
        // Publish the active role names so `current_user` / `session_user`
        // expressions (and, later, RLS policies) resolve to the right role.
        picklejar_executor::set_session_identity(&self.current_role, &self.session_user);
        // Authorize the statement against the current role before running it.
        // A superuser session (the default) passes every check, so an
        // unconfigured database is unaffected.
        self.check_permission(&stmt)?;
        // Enforce row-level security by folding policy predicates into the
        // statement's reads. A role that bypasses RLS gets it unchanged.
        let stmt = self.apply_rls(stmt);
        match stmt {
            // Transaction control.
            Statement::Begin => self.begin_txn(),
            Statement::Commit => self.commit_txn(),
            Statement::Rollback => self.rollback_txn(),
            // Roles, privileges, and session role. These manage the security
            // catalog and persist it immediately, like other DDL.
            Statement::CreateRole {
                ref name,
                is_user,
                ref options,
            } => self.create_role(name, is_user, options),
            Statement::AlterRole {
                ref name,
                ref options,
            } => self.alter_role(name, options),
            Statement::DropRole {
                if_exists,
                ref name,
            } => self.drop_role(if_exists, name),
            Statement::Grant {
                ref privileges,
                ref table,
                ref roles,
                ref grantees,
                ..
            } => self.run_grant(privileges, table.as_deref(), roles, grantees, true),
            Statement::Revoke {
                ref privileges,
                ref table,
                ref roles,
                ref grantees,
            } => self.run_grant(privileges, table.as_deref(), roles, grantees, false),
            Statement::SetRole { ref name } => self.set_role(name.as_deref()),
            Statement::CreatePolicy {
                ref name,
                ref table,
                command,
                ref roles,
                ref using,
                ref check,
            } => self.create_policy(name, table, command, roles, using.clone(), check.clone()),
            Statement::DropPolicy {
                if_exists,
                ref name,
                ref table,
            } => self.drop_policy(if_exists, name, table),
            // DDL auto-commits: it persists immediately regardless of any open
            // transaction.
            Statement::CreateTable { .. } => self.create_table(&stmt),
            Statement::CreateTableAs {
                ref name,
                ref query,
            } => self.create_table_as(name, query),
            Statement::CreateIndex {
                ref name,
                ref table,
                ref columns,
                unique,
            } => self.create_index(name, table, columns, unique),
            Statement::DropTable {
                if_exists,
                ref name,
            } => self.drop_table(&stmt, if_exists, name),
            Statement::CreateView {
                ref name,
                ref query,
            } => self.create_view(name, query),
            Statement::DropView {
                if_exists,
                ref name,
            } => self.drop_view(if_exists, name),
            Statement::Truncate { ref table } => self.truncate_table(table),
            Statement::Analyze { ref table } => self.run_analyze(table.as_deref()),
            Statement::Vacuum { ref table } => self.run_vacuum(table.as_deref()),
            Statement::AlterTable {
                ref table,
                ref action,
            } => self.alter_table(table, action),
            // EXPLAIN plans; EXPLAIN ANALYZE also runs the query.
            Statement::Explain { .. } => self.run_explain(&stmt),
            // A pure nearest-neighbor `SELECT` may be served from the HNSW index
            // when that path is enabled. Anything the index path declines (the
            // common case, and every RLS-fenced query, which now carries a folded
            // WHERE) falls through to the exact, transactional path below.
            Statement::Select(ref s) if self.vector_index_on => self
                .run_vector_index_select(s)?
                .map_or_else(|| self.run_in_txn(&stmt), Ok),
            // DML and SELECT run inside the open transaction, or a fresh
            // auto-commit one. `expand_ctes` inlined any non-recursive WITH;
            // only a recursive WITH reaches here, evaluated against a snapshot.
            Statement::With { .. }
            | Statement::Insert { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
            | Statement::Select(_)
            | Statement::Union { .. }
            | Statement::Copy { .. } => self.run_in_txn(&stmt),
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

    // --- roles, privileges, and authorization ---

    /// Run the session as `user` (e.g. the role the wire server authenticated),
    /// resetting both `session_user` and the current role. An unknown role falls
    /// back to the bootstrap superuser, so a database with no roles defined stays
    /// fully open (trust behaviour). This fully resets the session identity on
    /// every call, which the wire server relies on to keep connections that share
    /// one engine from inheriting each other's role.
    pub fn set_session_user(&mut self, user: &str) {
        let role = if self.security.role_exists(user) {
            user
        } else {
            security::BOOTSTRAP_SUPERUSER
        };
        self.session_user = role.to_string();
        self.current_role = role.to_string();
    }

    /// Approximate nearest-neighbor search over `vec_col` of `table`: the `k` rows
    /// nearest to `query` under `metric`, found through an HNSW index.
    ///
    /// The rows it indexes come from a `SELECT` through the engine, so the
    /// row-level-security fence is applied *before* the index is built: the result
    /// can only ever contain rows the session is allowed to see, the same
    /// isolation the exact path has. This is the approximate counterpart to the
    /// exact `ORDER BY col <-> :q LIMIT k` path, which stays the correctness
    /// baseline it is checked against.
    ///
    /// # Errors
    ///
    /// Returns an error if the table cannot be queried or `vec_col` is not a
    /// column of it. A column with no vectors yields an empty result.
    pub fn knn(
        &mut self,
        table: &str,
        vec_col: &str,
        query: &[f32],
        k: usize,
        metric: Metric,
    ) -> Result<Vec<Vec<Value>>> {
        let (columns, rows) = match self.execute(&format!("SELECT * FROM {table}"))? {
            QueryOutcome::Rows { columns, rows } => (columns, rows),
            other => {
                return Err(DbError::Unsupported(format!(
                    "knn expected rows from {table}, got {other:?}"
                )))
            }
        };
        let col = columns
            .iter()
            .position(|c| c == vec_col)
            .ok_or_else(|| DbError::Unsupported(format!("no column {vec_col:?} in {table}")))?;
        // The dimension comes from the first row that actually carries a vector.
        let Some(dim) = rows.iter().find_map(|r| match r.get(col) {
            Some(Value::Vector(v)) => Some(v.len()),
            _ => None,
        }) else {
            return Ok(Vec::new());
        };
        let mut index = Hnsw::new_with_metric(dim, 16, 200, 0x5EED_5EED, metric);
        let mut node_to_row: Vec<usize> = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            if let Some(Value::Vector(v)) = r.get(col) {
                if v.len() == dim {
                    index.insert(v.clone());
                    node_to_row.push(i);
                }
            }
        }
        let ef = k.max(64).saturating_mul(2);
        Ok(index
            .search(query, k, ef)
            .into_iter()
            .map(|(id, _)| rows[node_to_row[id]].clone())
            .collect())
    }

    /// Turn the HNSW index path for vector `ORDER BY ... LIMIT` queries on or off.
    /// It is off by default, so the exact scan stays the default and serving an
    /// approximate result is always an explicit opt-in.
    pub fn set_vector_index(&mut self, on: bool) {
        self.vector_index_on = on;
    }

    /// Whether the HNSW index path is currently enabled.
    #[must_use]
    pub const fn vector_index_enabled(&self) -> bool {
        self.vector_index_on
    }

    /// If `s` is a pure nearest-neighbor query this engine can serve from an HNSW
    /// index, build the index over the visible rows, search it, and return the
    /// projected rows. Otherwise return `None` so the caller runs the exact path.
    ///
    /// The accepted shape is deliberately narrow: a single base table; no WHERE,
    /// join, grouping, DISTINCT, or OFFSET; an ORDER BY of exactly one ascending
    /// `col <vector-op> <vector-literal>`; a LIMIT; and a projection that is `*`
    /// or a list of plain column references. Row-level security is folded into a
    /// WHERE before this runs, so an RLS-protected query always carries a WHERE
    /// and never matches here; it falls through to the exact, fenced path. The
    /// candidate rows are fetched through the engine's own `SELECT`, which
    /// enforces table permissions, so this path cannot read rows the role could
    /// not already read.
    fn run_vector_index_select(&mut self, s: &Select) -> Result<Option<QueryOutcome>> {
        // Structural gate: only the bare KNN shape is eligible.
        if !s.joins.is_empty()
            || s.where_clause.is_some()
            || !s.group_by.is_empty()
            || s.having.is_some()
            || s.distinct
            || s.offset.is_some()
            || s.from.subquery.is_some()
            || s.from.name.is_empty()
            || s.order_by.len() != 1
        {
            return Ok(None);
        }
        let Some(limit) = s.limit else {
            return Ok(None);
        };
        let k = usize::try_from(limit).unwrap_or(usize::MAX);
        let Some((col, metric, query)) = match_knn_order(&s.order_by[0]) else {
            return Ok(None);
        };

        // Reuse a cached index for this (role, table, column, metric), or build one
        // now. Building goes through the engine's own SELECT, which enforces table
        // permissions; a cached entry only exists because this same role built it
        // and no intervening write cleared it, so a hit needs no recheck.
        let key = (
            self.current_role.clone(),
            s.from.name.clone(),
            col.clone(),
            metric,
        );
        if !self.vector_index_cache.contains_key(&key) {
            // Since no RLS applies (a folded WHERE would have disqualified this
            // shape), this returns every row of the table.
            let QueryOutcome::Rows { columns, rows } =
                self.execute(&format!("SELECT * FROM {}", s.from.name))?
            else {
                return Ok(None);
            };
            let Some(col_idx) = columns.iter().position(|c| *c == col) else {
                return Ok(None);
            };
            // With no vectors the answer is empty, exactly as the exact path would
            // return; there is nothing to index, so do not cache.
            let Some(dim) = rows.iter().find_map(|r| match r.get(col_idx) {
                Some(Value::Vector(v)) => Some(v.len()),
                _ => None,
            }) else {
                let Some(projection) = Projection::resolve(&s.projections, &columns) else {
                    return Ok(None);
                };
                return Ok(Some(QueryOutcome::Rows {
                    columns: projection.output_columns(&columns),
                    rows: Vec::new(),
                }));
            };
            let mut index = Hnsw::new_with_metric(dim, 16, 200, 0x5EED_15A1, metric);
            let mut node_to_row: Vec<usize> = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                if let Some(Value::Vector(v)) = r.get(col_idx) {
                    if v.len() == dim {
                        index.insert(v.clone());
                        node_to_row.push(i);
                    }
                }
            }
            self.vector_index_cache.insert(
                key.clone(),
                CachedVectorIndex {
                    index,
                    rows,
                    columns,
                    node_to_row,
                    dim,
                },
            );
        }

        let entry = &self.vector_index_cache[&key];
        let Some(projection) = Projection::resolve(&s.projections, &entry.columns) else {
            return Ok(None);
        };
        // A dimension mismatch is a query error; let the exact path raise it so the
        // message and behavior match the non-indexed path exactly.
        if query.len() != entry.dim {
            return Ok(None);
        }
        let out_columns = projection.output_columns(&entry.columns);
        let ef = k.max(64).saturating_mul(2);
        let out_rows = entry
            .index
            .search(&query, k, ef)
            .into_iter()
            .map(|(id, _)| projection.project(&entry.rows[entry.node_to_row[id]]))
            .collect();
        Ok(Some(QueryOutcome::Rows {
            columns: out_columns,
            rows: out_rows,
        }))
    }

    /// The role the session authenticated as (`session_user`).
    #[must_use]
    pub fn session_user(&self) -> &str {
        &self.session_user
    }

    /// The role privileges are currently checked against (`current_user`).
    #[must_use]
    pub fn current_role(&self) -> &str {
        &self.current_role
    }

    /// `CREATE ROLE` / `CREATE USER`.
    fn create_role(
        &mut self,
        name: &str,
        is_user: bool,
        options: &[RoleOption],
    ) -> Result<QueryOutcome> {
        if self.security.role_exists(name) {
            return Err(DbError::Constraint(format!(
                "role \"{name}\" already exists"
            )));
        }
        let mut attrs = RoleAttrs {
            login: is_user,
            ..RoleAttrs::default()
        };
        apply_role_options(&mut attrs, options);
        self.security.put_role(name, attrs);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `ALTER ROLE`.
    fn alter_role(&mut self, name: &str, options: &[RoleOption]) -> Result<QueryOutcome> {
        let mut attrs = self
            .security
            .attrs(name)
            .cloned()
            .ok_or_else(|| DbError::Unsupported(format!("role \"{name}\" does not exist")))?;
        apply_role_options(&mut attrs, options);
        self.security.put_role(name, attrs);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `DROP ROLE [IF EXISTS]`.
    fn drop_role(&mut self, if_exists: bool, name: &str) -> Result<QueryOutcome> {
        if name == security::BOOTSTRAP_SUPERUSER {
            return Err(DbError::PermissionDenied(
                "the bootstrap superuser cannot be dropped".into(),
            ));
        }
        if !self.security.role_exists(name) {
            if if_exists {
                return Ok(QueryOutcome::Ddl);
            }
            return Err(DbError::Unsupported(format!(
                "role \"{name}\" does not exist"
            )));
        }
        self.security.remove_role(name);
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `GRANT` / `REVOKE`, for both table privileges and role membership.
    /// `grant` is `true` for `GRANT`, `false` for `REVOKE`.
    fn run_grant(
        &mut self,
        privileges: &[Privilege],
        table: Option<&str>,
        roles: &[String],
        grantees: &[Grantee],
        grant: bool,
    ) -> Result<QueryOutcome> {
        if let Some(table) = table {
            // A privilege grant: the grantor must own the table (or be a
            // superuser, already short-circuited by check_permission).
            if !self.security.is_superuser(&self.current_role)
                && !self.security.owns(&self.current_role, table)
            {
                return Err(DbError::PermissionDenied(format!(
                    "must own table \"{table}\" to grant privileges on it"
                )));
            }
            if !self.tables.contains_key(table) {
                return Err(DbError::UnknownTable(table.to_string()));
            }
            let bits = privileges
                .iter()
                .fold(0u8, |acc, p| acc | security::priv_bits(*p));
            for grantee in grantees {
                let who = grantee_name(grantee);
                if grant {
                    self.security.grant(&who, table, bits);
                } else {
                    self.security.revoke(&who, table, bits);
                }
            }
        } else {
            // Role membership.
            for role in roles {
                if !self.security.role_exists(role) {
                    return Err(DbError::Unsupported(format!(
                        "role \"{role}\" does not exist"
                    )));
                }
                for grantee in grantees {
                    let member = grantee_name(grantee);
                    if grant {
                        self.security.add_member(&member, role);
                    } else {
                        self.security.remove_member(&member, role);
                    }
                }
            }
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `SET ROLE name` / `SET ROLE NONE` / `RESET ROLE`.
    fn set_role(&mut self, name: Option<&str>) -> Result<QueryOutcome> {
        match name {
            None => {
                self.current_role = self.session_user.clone();
            }
            Some(role) => {
                if !self.security.role_exists(role) {
                    return Err(DbError::Unsupported(format!(
                        "role \"{role}\" does not exist"
                    )));
                }
                // A session may assume a role it is a member of (or is).
                let allowed = self.security.is_superuser(&self.session_user)
                    || self.session_user == role
                    || self.security.is_member_of(&self.session_user, role);
                if !allowed {
                    return Err(DbError::PermissionDenied(format!(
                        "permission denied to set role \"{role}\""
                    )));
                }
                self.current_role = role.to_string();
            }
        }
        Ok(QueryOutcome::Message("SET"))
    }

    /// Authorize `stmt` against the current role. A superuser passes everything;
    /// role-management statements are checked in their handlers.
    fn check_permission(&self, stmt: &Statement) -> Result<()> {
        if self.security.is_superuser(&self.current_role) {
            return Ok(());
        }
        match stmt {
            Statement::Select(_) | Statement::Union { .. } | Statement::With { .. } => {
                self.require_select(&referenced_tables(stmt))?;
            }
            Statement::Insert {
                table,
                source,
                rows,
                ..
            } => {
                self.require_priv(table, security::PRIV_INSERT)?;
                let mut reads = HashSet::new();
                if let Some(src) = source {
                    collect_stmt_tables(src, &mut reads);
                }
                for row in rows {
                    for e in row {
                        collect_expr_tables(e, &mut reads);
                    }
                }
                self.require_select(&reads)?;
            }
            Statement::Update { table, .. } => {
                self.require_priv(table, security::PRIV_UPDATE)?;
                self.require_select(&others(referenced_tables(stmt), table))?;
            }
            Statement::Delete { table, .. } => {
                self.require_priv(table, security::PRIV_DELETE)?;
                self.require_select(&others(referenced_tables(stmt), table))?;
            }
            Statement::Truncate { table } => {
                self.require_owner_or_priv(table, security::PRIV_TRUNCATE)?;
            }
            Statement::Copy { table, to, .. } => {
                let needed = if *to {
                    security::PRIV_SELECT
                } else {
                    security::PRIV_INSERT
                };
                self.require_priv(table, needed)?;
            }
            Statement::CreateTableAs { query, .. } | Statement::CreateView { query, .. } => {
                self.require_select(&referenced_tables(query))?;
            }
            Statement::DropTable { name: t, .. }
            | Statement::AlterTable { table: t, .. }
            | Statement::Analyze { table: Some(t) }
            | Statement::Vacuum { table: Some(t) } => self.require_owner(t)?,
            Statement::Explain { statement, .. } => self.check_permission(statement)?,
            // Role management and role-membership grants require CREATEROLE (a
            // superuser was already cleared above). Table GRANT/REVOKE checks
            // ownership in its handler.
            Statement::CreateRole { .. }
            | Statement::AlterRole { .. }
            | Statement::DropRole { .. }
            | Statement::Grant { table: None, .. }
            | Statement::Revoke { table: None, .. }
                if !self.security.can_create_role(&self.current_role) =>
            {
                return Err(DbError::PermissionDenied(
                    "permission denied to manage roles".into(),
                ));
            }
            // CREATE TABLE / INDEX, DROP VIEW, bare ANALYZE/VACUUM, transaction
            // control, table GRANT/REVOKE (checked in the handler), and SET ROLE
            // (checked in its handler) need no table-level privilege here.
            _ => {}
        }
        Ok(())
    }

    /// Require `SELECT` on every table in `tables` (skipping non-base names).
    fn require_select(&self, tables: &HashSet<String>) -> Result<()> {
        for t in tables {
            self.require_priv(t, security::PRIV_SELECT)?;
        }
        Ok(())
    }

    /// Require `needed` on `table`. Names with no physical store (views, system
    /// catalogs, derived/CTE names) are not enforced here.
    fn require_priv(&self, table: &str, needed: security::PrivSet) -> Result<()> {
        if !self.tables.contains_key(table) {
            return Ok(());
        }
        if self
            .security
            .has_privilege(&self.current_role, table, needed)
        {
            Ok(())
        } else {
            Err(DbError::PermissionDenied(format!(
                "permission denied for table \"{table}\""
            )))
        }
    }

    /// Require ownership (or superuser) of `table`.
    fn require_owner(&self, table: &str) -> Result<()> {
        if !self.tables.contains_key(table) {
            return Ok(());
        }
        if self.security.is_superuser(&self.current_role)
            || self.security.owns(&self.current_role, table)
        {
            Ok(())
        } else {
            Err(DbError::PermissionDenied(format!(
                "must be owner of table \"{table}\""
            )))
        }
    }

    /// Require ownership of `table` or the `needed` privilege on it.
    fn require_owner_or_priv(&self, table: &str, needed: security::PrivSet) -> Result<()> {
        if self.require_owner(table).is_ok() {
            return Ok(());
        }
        self.require_priv(table, needed)
    }

    // --- row-level security ---

    /// `CREATE POLICY`.
    fn create_policy(
        &mut self,
        name: &str,
        table: &str,
        command: PolicyCommand,
        roles: &[Grantee],
        using: Option<Expr>,
        check: Option<Expr>,
    ) -> Result<QueryOutcome> {
        self.require_owner(table)?;
        if self.catalog.get_table(table).is_none() {
            return Err(DbError::UnknownTable(table.to_string()));
        }
        let entry = self.rls.entry(table.to_string()).or_default();
        if entry.policies.iter().any(|p| p.name == name) {
            return Err(DbError::Constraint(format!(
                "policy \"{name}\" for table \"{table}\" already exists"
            )));
        }
        entry.policies.push(Policy {
            name: name.to_string(),
            command,
            roles: policy_roles(roles),
            using,
            check,
        });
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `DROP POLICY [IF EXISTS]`.
    fn drop_policy(&mut self, if_exists: bool, name: &str, table: &str) -> Result<QueryOutcome> {
        self.require_owner(table)?;
        let removed = self.rls.get_mut(table).is_some_and(|entry| {
            let before = entry.policies.len();
            entry.policies.retain(|p| p.name != name);
            entry.policies.len() != before
        });
        if !removed && !if_exists {
            return Err(DbError::Unsupported(format!(
                "policy \"{name}\" for table \"{table}\" does not exist"
            )));
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Rebuild the variable-key secondary index descriptors from their sidecar,
    /// mapping each persisted column name back to its position.
    fn load_multi_indexes(&mut self) -> Result<()> {
        for r in persist::load_multi_indexes(&self.midx_path)? {
            let Some(meta) = self.catalog.get_table(&r.table) else {
                continue;
            };
            let columns: Option<Vec<usize>> =
                r.columns.iter().map(|c| meta.column_index(c)).collect();
            let Some(columns) = columns else {
                continue;
            };
            // Restore the leading column's distinct stat so the cost model still
            // chooses the index after a reopen.
            if let Some(lead) = r.columns.first() {
                self.catalog.set_column_stats(
                    &r.table,
                    lead,
                    ColumnStats {
                        distinct: r.distinct,
                        min: None,
                        max: None,
                    },
                )?;
            }
            if let Some(store) = self.tables.get_mut(&r.table) {
                store.multi_secondary.push(MultiSecondaryIndex {
                    name: r.name,
                    columns,
                    root: PageId(r.root),
                    distinct: r.distinct,
                    unique: r.unique,
                });
            }
        }
        Ok(())
    }

    /// Snapshot the variable-key secondary indexes to their sidecar.
    fn save_multi_indexes(&self) -> Result<()> {
        let mut records = Vec::new();
        let mut tables: Vec<&String> = self.tables.keys().collect();
        tables.sort();
        for table in tables {
            let store = &self.tables[table];
            let Some(meta) = self.catalog.get_table(table) else {
                continue;
            };
            for m in &store.multi_secondary {
                records.push(persist::MultiIndexRecord {
                    table: table.clone(),
                    name: m.name.clone(),
                    root: m.root.0,
                    distinct: m.distinct,
                    unique: m.unique,
                    columns: m
                        .columns
                        .iter()
                        .map(|&c| meta.columns[c].name.clone())
                        .collect(),
                });
            }
        }
        persist::save_multi_indexes(&self.midx_path, &records)?;
        Ok(())
    }

    /// Load the row-level-security state from its sidecar.
    fn load_rls(&mut self) -> Result<()> {
        let data = persist::load_rls(&self.pol_path)?;
        for (table, enabled, forced) in data.flags {
            let entry = self.rls.entry(table).or_default();
            entry.enabled = enabled;
            entry.forced = forced;
        }
        for sql in data.policies {
            let stmt = Parser::from_sql(&sql)?.parse_statement()?;
            if let Statement::CreatePolicy {
                name,
                table,
                command,
                roles,
                using,
                check,
            } = stmt
            {
                self.rls.entry(table).or_default().policies.push(Policy {
                    name,
                    command,
                    roles: policy_roles(&roles),
                    using,
                    check,
                });
            }
        }
        Ok(())
    }

    /// Snapshot the row-level-security state to its sidecar.
    fn save_rls(&self) -> Result<()> {
        let mut tables: Vec<&String> = self.rls.keys().collect();
        tables.sort();
        let mut flags = Vec::new();
        let mut policies = Vec::new();
        for table in tables {
            let entry = &self.rls[table];
            flags.push((table.clone(), entry.enabled, entry.forced));
            for p in &entry.policies {
                policies.push(policy_to_sql(table, p));
            }
        }
        persist::save_rls(&self.pol_path, &persist::RlsData { flags, policies })?;
        Ok(())
    }

    /// Whether row-level security applies to `table` for the current role: RLS is
    /// enabled, the role is not exempt (superuser / `BYPASSRLS`), and the role is
    /// not the owner unless the table forces RLS.
    fn rls_applies(&self, table: &str) -> bool {
        let Some(entry) = self.rls.get(table) else {
            return false;
        };
        if !entry.enabled || self.security.can_bypass_rls(&self.current_role) {
            return false;
        }
        entry.forced || !self.security.owns(&self.current_role, table)
    }

    /// Build the visibility predicate for `table` and `command`: the `OR` of the
    /// `USING` clauses of every policy applicable to the current role, qualified
    /// by `qualifier`. Returns `None` if RLS does not apply, or `Some(FALSE)` when
    /// it applies but no policy grants visibility (default deny).
    fn rls_predicate(
        &self,
        table: &str,
        command: PolicyCommand,
        qualifier: Option<&str>,
    ) -> Option<Expr> {
        if !self.rls_applies(table) {
            return None;
        }
        let entry = self.rls.get(table)?;
        let mut terms: Vec<Expr> = Vec::new();
        for p in &entry.policies {
            if policy_matches(p, command, &self.current_role) {
                if let Some(using) = &p.using {
                    // SELECT may join or alias the table, so its columns are
                    // qualified; an UPDATE/DELETE targets the bare table, where
                    // the executor resolves only unqualified names.
                    terms.push(qualifier.map_or_else(|| using.clone(), |q| qualify_expr(using, q)));
                }
            }
        }
        Some(or_all(terms))
    }

    /// Rewrite a statement to enforce row-level security on the tables it reads.
    /// A role that bypasses RLS gets the statement unchanged.
    fn apply_rls(&self, stmt: Statement) -> Statement {
        if self.security.can_bypass_rls(&self.current_role) {
            return stmt;
        }
        match stmt {
            Statement::Select(s) => Statement::Select(Box::new(self.rls_select(*s))),
            Statement::Union {
                op,
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => Statement::Union {
                op,
                all,
                left: Box::new(self.apply_rls(*left)),
                right: Box::new(self.apply_rls(*right)),
                order_by,
                limit,
                offset,
            },
            Statement::Update {
                table,
                assignments,
                where_clause,
                returning,
            } => {
                let guard = self.rls_predicate(&table, PolicyCommand::Update, None);
                Statement::Update {
                    table,
                    assignments,
                    where_clause: and_opt(where_clause, guard),
                    returning,
                }
            }
            Statement::Delete {
                table,
                where_clause,
                returning,
            } => {
                let guard = self.rls_predicate(&table, PolicyCommand::Delete, None);
                Statement::Delete {
                    table,
                    where_clause: and_opt(where_clause, guard),
                    returning,
                }
            }
            Statement::Explain { analyze, statement } => Statement::Explain {
                analyze,
                statement: Box::new(self.apply_rls(*statement)),
            },
            other => other,
        }
    }

    /// Apply RLS to one `SELECT`: guard its `FROM` and `JOIN` tables, and recurse
    /// into derived-table subqueries.
    fn rls_select(&self, mut select: Select) -> Select {
        let mut guards: Vec<Expr> = Vec::new();
        rls_guard_table_ref(self, &mut select.from, &mut guards);
        for join in &mut select.joins {
            rls_guard_table_ref(self, &mut join.table, &mut guards);
        }
        if !guards.is_empty() {
            let combined = and_all(guards);
            select.where_clause = Some(and_two(select.where_clause.take(), combined));
        }
        select
    }

    /// Reject `row` if it fails the row-level-security `WITH CHECK` for `command`
    /// on `table`. `columns` are aligned with `row`. A write produces a row that
    /// must satisfy some applicable policy's check (its `USING` stands in when a
    /// policy has no explicit check); with RLS on and no applicable policy, the
    /// write is denied.
    fn enforce_rls_check(
        &self,
        table: &str,
        command: PolicyCommand,
        columns: &[String],
        row: &[Value],
    ) -> Result<()> {
        if !self.rls_applies(table) {
            return Ok(());
        }
        let entry = self.rls.get(table).expect("rls_applies checked presence");
        for p in &entry.policies {
            if !policy_matches(p, command, &self.current_role) {
                continue;
            }
            // A policy's WITH CHECK governs writes; if it has none, its USING
            // does (Postgres semantics).
            match p.check.as_ref().or(p.using.as_ref()) {
                None => return Ok(()), // a permissive policy with no check passes.
                Some(expr) => {
                    if matches!(eval(expr, row, columns)?, Value::Bool(true)) {
                        return Ok(());
                    }
                }
            }
        }
        Err(DbError::PermissionDenied(format!(
            "new row violates row-level security policy for table \"{table}\""
        )))
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
                source,
                on_conflict,
                returning,
            } => match source {
                // `INSERT ... SELECT`: run the query, then insert its rows.
                Some(query) => {
                    self.insert_select(txn, table, columns, query, on_conflict.as_ref(), returning)
                }
                None => self.insert(txn, table, columns, rows, on_conflict.as_ref(), returning),
            },
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
            Statement::Copy {
                table,
                to,
                path,
                header,
            } => self.run_copy(txn, table, *to, path, *header),
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

    /// Produce a self-contained SQL script that recreates this database: the
    /// schema (tables in foreign-key-safe order, then explicit indexes and
    /// views) followed by every row as an `INSERT`. Running the script on an
    /// empty database reproduces this one. This is picklejar's `pg_dump`.
    ///
    /// # Errors
    ///
    /// Returns an error if a table cannot be scanned.
    pub fn dump(&self) -> Result<String> {
        use std::fmt::Write as _;
        let mut out = String::new();
        let order = self.fk_safe_table_order();

        // 1. Schema: CREATE TABLE in dependency order (a parent before any
        //    child whose foreign key references it).
        for table in &order {
            let create = self.reconstruct_create_table(table)?;
            let _ = writeln!(out, "{create};");
        }
        // 2. Explicit indexes: those whose name is not the auto-generated
        //    `{table}_{column}_idx` created for a PRIMARY KEY / UNIQUE column.
        for table in &order {
            if let Some(meta) = self.catalog.get_table(table) {
                for ix in &meta.indexes {
                    if ix.name != format!("{table}_{}_idx", ix.column) {
                        let _ =
                            writeln!(out, "CREATE INDEX {} ON {table} ({});", ix.name, ix.column);
                    }
                }
            }
        }
        // 3. Views, in name order.
        let mut views: Vec<(&String, &Statement)> = self.views.iter().collect();
        views.sort_by(|a, b| a.0.cmp(b.0));
        for (name, query) in views {
            let _ = writeln!(out, "CREATE VIEW {name} AS {query};");
        }
        // 4. Data: an INSERT per non-empty table, in dependency order so a
        //    child's rows land after the parent rows they reference.
        for table in &order {
            let Some(meta) = self.catalog.get_table(table) else {
                continue;
            };
            let schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
            let col_names = meta
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let rows = self.scan_all_rows(table, &schema)?;
            if rows.is_empty() {
                continue;
            }
            let _ = write!(out, "INSERT INTO {table} ({col_names}) VALUES ");
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let cells = row
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write!(out, "({cells})");
            }
            out.push_str(";\n");
        }
        Ok(out)
    }

    /// Order the user tables so every table follows the tables its foreign keys
    /// reference. A reference cycle (or self-reference) falls back to name order
    /// for the tables it involves.
    fn fk_safe_table_order(&self) -> Vec<String> {
        let mut names: Vec<String> = self.tables.keys().cloned().collect();
        names.sort();
        let parents = |t: &str| -> Vec<String> {
            self.constraints
                .get(t)
                .map(|tc| {
                    tc.foreign_keys
                        .iter()
                        .map(|fk| fk.parent_table.clone())
                        .filter(|p| p != t)
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut ordered: Vec<String> = Vec::new();
        while ordered.len() < names.len() {
            let mut progressed = false;
            for n in &names {
                if ordered.contains(n) {
                    continue;
                }
                if parents(n)
                    .iter()
                    .all(|p| ordered.contains(p) || !names.contains(p))
                {
                    ordered.push(n.clone());
                    progressed = true;
                }
            }
            if !progressed {
                // A cycle: emit whatever is left in name order and stop.
                for n in &names {
                    if !ordered.contains(n) {
                        ordered.push(n.clone());
                    }
                }
                break;
            }
        }
        ordered
    }

    /// Rebuild the `CREATE TABLE` statement for `table` from the catalog plus
    /// the engine's own sidecars (defaults, serials, constraints), so its
    /// `Display` reproduces the original DDL.
    fn reconstruct_create_table(&self, table: &str) -> Result<Statement> {
        let meta = self
            .catalog
            .get_table(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let serials = self.serial_cols.get(table);
        let store = self.tables.get(table);
        let columns = meta
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let serial = serials.is_some_and(|s| s.iter().any(|n| n == &c.name));
                // A serial column carries no DEFAULT of its own.
                let default = if serial {
                    None
                } else {
                    store
                        .and_then(|s| s.defaults.get(i))
                        .cloned()
                        .flatten()
                        .map(Expr::Literal)
                };
                ColumnDef {
                    name: c.name.clone(),
                    ty: c.ty,
                    primary_key: c.primary_key,
                    // PRIMARY KEY already implies NOT NULL and UNIQUE, so don't
                    // re-emit them and produce redundant DDL.
                    not_null: c.not_null && !c.primary_key,
                    unique: c.unique && !c.primary_key,
                    default,
                    serial,
                }
            })
            .collect();
        let constraints = self
            .constraints
            .get(table)
            .map(|tc| {
                let mut v: Vec<TableConstraint> = Vec::new();
                for chk in &tc.checks {
                    v.push(TableConstraint::Check(chk.clone()));
                }
                for fk in &tc.foreign_keys {
                    v.push(TableConstraint::ForeignKey(ForeignKey {
                        column: fk.column.clone(),
                        parent_table: fk.parent_table.clone(),
                        parent_column: fk.parent_column.clone(),
                        on_delete: fk.on_delete,
                        on_update: fk.on_update,
                    }));
                }
                v
            })
            .unwrap_or_default();
        Ok(Statement::CreateTable {
            if_not_exists: false,
            name: table.to_string(),
            columns,
            constraints,
        })
    }

    // --- statement handlers ---

    fn create_table(&mut self, stmt: &Statement) -> Result<QueryOutcome> {
        let Statement::CreateTable {
            if_not_exists,
            name,
            columns,
            constraints,
        } = stmt
        else {
            unreachable!("guarded by execute");
        };
        // `IF NOT EXISTS`: if a table or view of this name is already here,
        // succeed quietly without recreating it.
        if *if_not_exists
            && (self.catalog.get_table(name).is_some() || self.views.contains_key(name))
        {
            return Ok(QueryOutcome::Ddl);
        }
        // Validate and resolve the table-level constraints before creating
        // anything, so a bad reference rejects the whole statement.
        let table_constraints = self.build_constraints(name, columns, constraints)?;
        // The catalog rejects a duplicate table, keeping it the single source
        // of truth for which tables exist.
        self.catalog.apply(stmt)?;
        let table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;

        // Build a physical secondary index for every unique column of an
        // indexable type, so an equality lookup becomes a point get and a range
        // predicate becomes a range scan. Register each in the catalog as well,
        // so the planner can cost and choose an index scan. Uniqueness plus the
        // order-preserving, bijective key map (see `index::index_key`) guarantees
        // the keys never collide, which is what lets the plain unique-keyed B+
        // tree serve as the index.
        let mut secondary = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            if is_indexable_type(col.ty) && (col.primary_key || col.unique) {
                let index = Index::create(&self.pool)?;
                secondary.push(SecondaryIndex {
                    column: i,
                    root: index.root(),
                });
                self.catalog.apply(&Statement::CreateIndex {
                    name: format!("{name}_{}_idx", col.name),
                    table: name.clone(),
                    columns: vec![col.name.clone()],
                    unique: false,
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
            multi_secondary: Vec::new(),
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
        // The creating role owns the new table (and so holds every privilege on
        // it). `persist` writes the ownership through `save_security`.
        let owner = self.current_role.clone();
        self.security.set_owner(name, &owner);
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
            if_not_exists: false,
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
                        on_delete: fk.on_delete,
                        on_update: fk.on_update,
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
        for m in &mut store.multi_secondary {
            m.root = MultiIndex::create(&self.pool)?.root();
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
        let (rows, sec_cols, multi_cols) = {
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
            let multi_cols: Vec<(String, Vec<usize>, u64, bool)> = store
                .multi_secondary
                .iter()
                .map(|m| (m.name.clone(), m.columns.clone(), m.distinct, m.unique))
                .collect();
            (rows, sec_cols, multi_cols)
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
        let mut multi: Vec<MultiSecondaryIndex> = Vec::with_capacity(multi_cols.len());
        for (name, columns, distinct, unique) in multi_cols {
            multi.push(MultiSecondaryIndex {
                name,
                columns,
                root: MultiIndex::create(&self.pool)?.root(),
                distinct,
                unique,
            });
        }

        // 3. Re-insert each live row, rebuilding the indexes as we go.
        let writer = self.mgr.begin();
        let mut rowid: u64 = 0;
        for values in rows {
            new_table.insert(&writer, rowid, &encode_row(&values, &schema)?)?;
            put_secondaries(&self.pool, &mut secondary, &mut multi, &values, rowid)?;
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
        store.multi_secondary = multi;
        self.catalog.set_row_count(name, rowid)?;
        Ok(())
    }

    /// `COPY`: bulk-import a table from a CSV file, or export its rows to one.
    fn run_copy(
        &mut self,
        txn: &Transaction,
        table: &str,
        to: bool,
        path: &str,
        header: bool,
    ) -> Result<QueryOutcome> {
        if to {
            self.copy_to(txn, table, path, header)
        } else {
            self.copy_from(txn, table, path, header)
        }
    }

    /// `COPY table TO 'path'`: write the table's visible rows out as CSV. A
    /// NULL is written as an empty field.
    fn copy_to(
        &self,
        txn: &Transaction,
        table: &str,
        path: &str,
        header: bool,
    ) -> Result<QueryOutcome> {
        let source = EngineSource {
            pool: &self.pool,
            wal: self.wal.clone(),
            mgr: &self.mgr,
            catalog: &self.catalog,
            tables: &self.tables,
            txn,
        };
        let relation = source.scan(table)?;
        let mut out = String::new();
        if header {
            out.push_str(&csv_record(&relation.columns));
            out.push('\n');
        }
        for row in &relation.rows {
            let fields: Vec<String> = row.iter().map(csv_field_from_value).collect();
            out.push_str(&csv_record(&fields));
            out.push('\n');
        }
        std::fs::write(path, out)?;
        Ok(QueryOutcome::Mutation {
            affected: relation.rows.len(),
        })
    }

    /// `COPY table FROM 'path'`: read CSV rows and insert them through the
    /// normal insert path (so NOT NULL / CHECK / FOREIGN KEY / UNIQUE all
    /// apply). Each record must supply every column, in order. An empty field
    /// is read as NULL.
    fn copy_from(
        &mut self,
        txn: &Transaction,
        table: &str,
        path: &str,
        header: bool,
    ) -> Result<QueryOutcome> {
        let schema: Vec<DataType> = self
            .catalog
            .get_table(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?
            .columns
            .iter()
            .map(|c| c.ty)
            .collect();
        let content = std::fs::read_to_string(path)?;
        let mut records = parse_csv(&content);
        if header && !records.is_empty() {
            records.remove(0);
        }
        let mut rows: Vec<Vec<Expr>> = Vec::with_capacity(records.len());
        for record in records {
            if record.len() != schema.len() {
                return Err(DbError::ValueCount {
                    expected: schema.len(),
                    got: record.len(),
                });
            }
            let exprs = record
                .iter()
                .zip(&schema)
                .map(|(field, ty)| Ok(Expr::Literal(value_from_csv_field(field, *ty)?)))
                .collect::<Result<Vec<_>>>()?;
            rows.push(exprs);
        }
        if rows.is_empty() {
            return Ok(QueryOutcome::Mutation { affected: 0 });
        }
        self.insert(txn, table, &[], &rows, None, &[])
    }

    /// Route an `ALTER TABLE` action to its handler.
    fn alter_table(&mut self, table: &str, action: &AlterAction) -> Result<QueryOutcome> {
        match action {
            AlterAction::AddColumn(col) => self.alter_add_column(table, col),
            AlterAction::DropColumn { name, if_exists } => {
                self.alter_drop_column(table, name, *if_exists)
            }
            AlterAction::RenameColumn { from, to } => self.alter_rename_column(table, from, to),
            AlterAction::RenameTable { to } => self.alter_rename_table(table, to),
            AlterAction::EnableRls => self.set_table_rls(table, |r| r.enabled = true),
            AlterAction::DisableRls => self.set_table_rls(table, |r| r.enabled = false),
            AlterAction::ForceRls => self.set_table_rls(table, |r| r.forced = true),
            AlterAction::NoForceRls => self.set_table_rls(table, |r| r.forced = false),
        }
    }

    /// Apply a change to a table's row-level-security flags and persist it.
    fn set_table_rls(
        &mut self,
        table: &str,
        change: impl FnOnce(&mut TableRls),
    ) -> Result<QueryOutcome> {
        if self.catalog.get_table(table).is_none() {
            return Err(DbError::UnknownTable(table.to_string()));
        }
        change(self.rls.entry(table.to_string()).or_default());
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `ALTER TABLE t DROP COLUMN c`: rewrite every row without the column into
    /// fresh storage. Refused when the column is the table's only one or is used
    /// by a `CHECK` or `FOREIGN KEY` constraint (whose reference would go stale).
    fn alter_drop_column(
        &mut self,
        table: &str,
        column: &str,
        if_exists: bool,
    ) -> Result<QueryOutcome> {
        let meta = self
            .catalog
            .get_table(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        let Some(idx) = meta.column_index(column) else {
            // `IF EXISTS`: a missing column is a no-op, not an error.
            if if_exists {
                return Ok(QueryOutcome::Ddl);
            }
            return Err(DbError::Constraint(format!(
                "column {column} does not exist on {table}"
            )));
        };
        if meta.columns.len() == 1 {
            return Err(DbError::Constraint(format!(
                "cannot drop the only column of {table}"
            )));
        }
        self.ensure_column_unconstrained(table, column)?;

        // Read every row under the current schema, then drop the column's value
        // from each and rebuild storage under the new (shorter) schema.
        let old_schema: Vec<DataType> = meta.columns.iter().map(|c| c.ty).collect();
        let old_rows = self.scan_all_rows(table, &old_schema)?;
        self.catalog.drop_column(table, column)?;
        let new_rows: Vec<Vec<Value>> = old_rows
            .into_iter()
            .map(|mut r| {
                r.remove(idx);
                r
            })
            .collect();
        self.rebuild_storage(table, &new_rows)?;

        // Keep the positional defaults aligned, and forget the column if it was
        // a serial.
        if let Some(store) = self.tables.get_mut(table) {
            if idx < store.defaults.len() {
                store.defaults.remove(idx);
            }
        }
        if let Some(list) = self.serial_cols.get_mut(table) {
            list.retain(|n| n != column);
            if list.is_empty() {
                self.serial_cols.remove(table);
            }
            self.save_serials()?;
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `ALTER TABLE t RENAME COLUMN a TO b`: a metadata-only rename, since rows
    /// are stored positionally. Refused when the column is named by a `CHECK` or
    /// `FOREIGN KEY` constraint, which would otherwise reference a stale name.
    fn alter_rename_column(&mut self, table: &str, from: &str, to: &str) -> Result<QueryOutcome> {
        let meta = self
            .catalog
            .get_table(table)
            .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
        if meta.column_index(from).is_none() {
            return Err(DbError::Constraint(format!(
                "column {from} does not exist on {table}"
            )));
        }
        if meta.column_index(to).is_some() {
            return Err(DbError::Constraint(format!(
                "column {to} already exists on {table}"
            )));
        }
        self.ensure_column_unconstrained(table, from)?;
        self.catalog.rename_column(table, from, to)?;
        if let Some(list) = self.serial_cols.get_mut(table) {
            for name in list.iter_mut() {
                if name == from {
                    *name = to.to_string();
                }
            }
            self.save_serials()?;
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `ALTER TABLE t RENAME TO u`: rename the table across the catalog and the
    /// engine's name-keyed maps. Refused when another table holds a foreign key
    /// into this one (the reference is by name), or the target name is taken.
    fn alter_rename_table(&mut self, from: &str, to: &str) -> Result<QueryOutcome> {
        if self.catalog.get_table(from).is_none() {
            return Err(DbError::UnknownTable(from.to_string()));
        }
        if self.catalog.get_table(to).is_some() || self.views.contains_key(to) {
            return Err(DbError::Constraint(format!("table {to} already exists")));
        }
        if let Some(child) = self.referencing_table(from) {
            return Err(DbError::Constraint(format!(
                "cannot rename table {from}: it is referenced by a foreign key on {child}"
            )));
        }
        self.catalog.rename_table(from, to)?;
        if let Some(store) = self.tables.remove(from) {
            self.tables.insert(to.to_string(), store);
        }
        if let Some(constraints) = self.constraints.remove(from) {
            self.constraints.insert(to.to_string(), constraints);
            self.save_constraints()?;
        }
        if let Some(serials) = self.serial_cols.remove(from) {
            self.serial_cols.insert(to.to_string(), serials);
            self.save_serials()?;
        }
        // Move ownership, grants, and row-level-security state to the new name.
        self.security.rename_table(from, to);
        if let Some(rls) = self.rls.remove(from) {
            self.rls.insert(to.to_string(), rls);
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Read every live row of `table` under `schema` (a snapshot scan), for an
    /// `ALTER` rewrite.
    fn scan_all_rows(&self, table: &str, schema: &[DataType]) -> Result<Vec<Vec<Value>>> {
        let reader = self.mgr.begin();
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
        let rows = handle
            .scan(&reader)?
            .into_iter()
            .map(|(_k, bytes)| decode_row(&bytes, schema))
            .collect::<std::result::Result<_, _>>()?;
        self.mgr.commit(&reader);
        Ok(rows)
    }

    /// Like [`scan_all_rows`](Self::scan_all_rows) but keeps each row's id (the
    /// MVCC key), for building a secondary index over an existing table.
    fn scan_rows_with_rowid(
        &self,
        table: &str,
        schema: &[DataType],
    ) -> Result<Vec<(u64, Vec<Value>)>> {
        let reader = self.mgr.begin();
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
        let rows = handle
            .scan(&reader)?
            .into_iter()
            .map(|(k, bytes)| decode_row(&bytes, schema).map(|r| (k, r)))
            .collect::<std::result::Result<_, _>>()?;
        self.mgr.commit(&reader);
        Ok(rows)
    }

    /// `CREATE [UNIQUE] INDEX name ON table (col, ...)`: register the index in
    /// the catalog and, when every column has an indexable type and the leading
    /// column is not already physically indexed, build and populate a
    /// variable-key [`MultiIndex`] over the live rows.
    fn create_index(
        &mut self,
        name: &str,
        table: &str,
        columns: &[String],
        unique: bool,
    ) -> Result<QueryOutcome> {
        // Only the table owner (or a superuser) may add an index.
        self.require_owner(table)?;
        // Validate and register in the catalog (the planner sees the leading
        // column from here).
        self.catalog.apply(&Statement::CreateIndex {
            name: name.to_string(),
            table: table.to_string(),
            columns: columns.to_vec(),
            unique,
        })?;

        // Resolve each column to its position and type.
        let cols: Vec<(usize, DataType)> = {
            let meta = self
                .catalog
                .get_table(table)
                .ok_or_else(|| DbError::UnknownTable(table.to_string()))?;
            columns
                .iter()
                .map(|c| {
                    meta.column_index(c)
                        .map(|i| (i, meta.columns[i].ty))
                        .ok_or_else(|| DbError::UnknownColumn {
                            table: table.to_string(),
                            column: c.clone(),
                        })
                })
                .collect::<Result<_>>()?
        };
        let col_idxs: Vec<usize> = cols.iter().map(|(i, _)| *i).collect();
        let leading = col_idxs[0];

        // Skip when every column is already covered by the leading u64 index or
        // an identical variable-key index, or when a column's type is not keyed.
        let all_indexable = cols.iter().all(|(_, ty)| is_multi_indexable(*ty));
        let already = self.tables.get(table).is_some_and(|s| {
            (col_idxs.len() == 1 && s.secondary.iter().any(|x| x.column == leading))
                || s.multi_secondary.iter().any(|m| m.columns == col_idxs)
        });
        if all_indexable && !already {
            let schema: Vec<DataType> = self
                .catalog
                .get_table(table)
                .expect("table just validated")
                .columns
                .iter()
                .map(|c| c.ty)
                .collect();
            let rows = self.scan_rows_with_rowid(table, &schema)?;
            let mindex = MultiIndex::create(&self.pool)?;
            // Distinct values of the *leading* column, for the cost model.
            let mut distinct = std::collections::HashSet::new();
            for (rowid, values) in &rows {
                let key_vals: Vec<&Value> = col_idxs.iter().map(|&c| &values[c]).collect();
                // A UNIQUE index refuses a build over data that already has a
                // duplicate tuple (every entry built so far is from a live row).
                if unique && !mindex.lookup_prefix(&key_vals)?.is_empty() {
                    return Err(DbError::Constraint(format!(
                        "could not create unique index \"{name}\": table contains duplicate values"
                    )));
                }
                mindex.put(&key_vals, *rowid)?;
                let mut enc = Vec::new();
                if crate::keyenc::encode_field(&values[leading], &mut enc) {
                    distinct.insert(enc);
                }
            }
            let root = mindex.root();
            let distinct = u64::try_from(distinct.len().max(1)).unwrap_or(u64::MAX);
            if let Some(store) = self.tables.get_mut(table) {
                store.multi_secondary.push(MultiSecondaryIndex {
                    name: name.to_string(),
                    columns: col_idxs,
                    root,
                    distinct,
                    unique,
                });
            }
            self.catalog.set_column_stats(
                table,
                &columns[0],
                ColumnStats {
                    distinct,
                    min: None,
                    max: None,
                },
            )?;
        }
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    /// Reject a write whose `values` would duplicate the tuple of any `UNIQUE`
    /// variable-key index on `table`. Checks both the rows already stored (the
    /// index plus an MVCC visibility check under `txn`, skipping `exclude`'s own
    /// row on an update) and `peers` (other rows produced earlier in the same
    /// statement). A tuple with a `NULL` never conflicts (SQL semantics).
    fn check_unique_multi(
        &self,
        txn: &Transaction,
        table: &str,
        values: &[Value],
        exclude: Option<u64>,
        peers: &[Vec<Value>],
    ) -> Result<()> {
        let Some(store) = self.tables.get(table) else {
            return Ok(());
        };
        if !store.multi_secondary.iter().any(|m| m.unique) {
            return Ok(());
        }
        let schema: Vec<DataType> = self
            .catalog
            .get_table(table)
            .map(|m| m.columns.iter().map(|c| c.ty).collect())
            .unwrap_or_default();
        let mvcc = MvccTable::open(
            &self.pool,
            self.wal.clone(),
            &self.mgr,
            store.index_root,
            store.version_page,
        );
        for m in &store.multi_secondary {
            if !m.unique {
                continue;
            }
            let key_vals: Vec<&Value> = m.columns.iter().map(|&c| &values[c]).collect();
            if key_vals.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let same = |row: &[Value]| m.columns.iter().all(|&c| row[c] == values[c]);
            // Another row earlier in this same statement.
            if peers.iter().any(|p| same(p)) {
                return Err(unique_violation(&m.name));
            }
            // A row already committed (or written earlier and visible to `txn`).
            let mindex = MultiIndex::open(&self.pool, m.root);
            for rowid in mindex.lookup_prefix(&key_vals)? {
                if Some(rowid) == exclude {
                    continue;
                }
                if let Some(bytes) = mvcc.get(txn, rowid)? {
                    if same(&decode_row(&bytes, &schema)?) {
                        return Err(unique_violation(&m.name));
                    }
                }
            }
        }
        Ok(())
    }

    /// Rebuild `table`'s physical storage and secondary indexes from `rows`,
    /// which must already be shaped to the table's *current* catalog columns.
    /// Swaps the fresh anchors into the table's store and updates its row count.
    fn rebuild_storage(&mut self, table: &str, rows: &[Vec<Value>]) -> Result<()> {
        let cols = self
            .catalog
            .get_table(table)
            .expect("present")
            .columns
            .clone();
        let new_schema: Vec<DataType> = cols.iter().map(|c| c.ty).collect();
        let new_table = MvccTable::create(&self.pool, self.wal.clone(), &self.mgr)?;
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
                        columns: vec![c.name.clone()],
                        unique: false,
                    })?;
                }
                secondary.push(SecondaryIndex {
                    column: i,
                    root: Index::create(&self.pool)?.root(),
                });
            }
        }
        let writer = self.mgr.begin();
        let mut rowid: u64 = 0;
        for values in rows {
            new_table.insert(&writer, rowid, &encode_row(values, &new_schema)?)?;
            for sec in &mut secondary {
                let index = Index::open(&self.pool, sec.root);
                index.put(&values[sec.column], rowid)?;
                sec.root = index.root();
            }
            rowid += 1;
        }
        self.mgr.commit(&writer);
        let index_root = new_table.index_root();
        let version_page = new_table.version_page();
        let store = self.tables.get_mut(table).expect("present");
        store.index_root = index_root;
        store.version_page = version_page;
        store.next_rowid = rowid;
        store.secondary = secondary;
        // A column rewrite renumbers rowids (and can shift column positions), so
        // any variable-key index would be stale; drop it and let queries fall
        // back to a scan. The user can re-`CREATE INDEX`.
        store.multi_secondary.clear();
        self.catalog.set_row_count(table, rowid)?;
        Ok(())
    }

    /// Reject altering `column` of `table` when a stored `CHECK` or `FOREIGN KEY`
    /// constraint names it (here or, as a foreign-key parent, in another table),
    /// since the rename/drop would leave that constraint pointing at a stale name.
    fn ensure_column_unconstrained(&self, table: &str, column: &str) -> Result<()> {
        if let Some(tc) = self.constraints.get(table) {
            if tc.checks.iter().any(|c| expr_mentions_column(c, column)) {
                return Err(DbError::Constraint(format!(
                    "cannot alter column {column}: it is used by a CHECK constraint on {table}"
                )));
            }
            if tc.foreign_keys.iter().any(|fk| fk.column == column) {
                return Err(DbError::Constraint(format!(
                    "cannot alter column {column}: it is used by a foreign key on {table}"
                )));
            }
        }
        for (child, tc) in &self.constraints {
            if child != table
                && tc
                    .foreign_keys
                    .iter()
                    .any(|fk| fk.parent_table == table && fk.parent_column == column)
            {
                return Err(DbError::Constraint(format!(
                    "cannot alter column {column}: it is referenced by a foreign key on {child}"
                )));
            }
        }
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
                        columns: vec![c.name.clone()],
                        unique: false,
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
        // The rewrite renumbered rowids, so any variable-key index is now stale.
        store.multi_secondary.clear();
        self.catalog.set_row_count(table, rowid)?;
        self.persist()?;
        Ok(QueryOutcome::Ddl)
    }

    fn drop_table(
        &mut self,
        stmt: &Statement,
        if_exists: bool,
        name: &str,
    ) -> Result<QueryOutcome> {
        // `IF EXISTS`: a missing table is a no-op, not an error.
        if if_exists && self.catalog.get_table(name).is_none() {
            return Ok(QueryOutcome::Ddl);
        }
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
        // Forget the table's ownership, grants, and row-level-security policies.
        self.security.clear_owner(name);
        self.rls.remove(name);
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

    /// Apply each referencing child's `ON DELETE` action when a parent row is
    /// deleted: reject (`NO ACTION` / `RESTRICT`), delete the children
    /// (`CASCADE`), or NULL their referencing column (`SET NULL`). Cascades run
    /// through the normal delete/update path, so they recurse and stay
    /// transactional.
    fn apply_fk_on_delete(
        &mut self,
        txn: &Transaction,
        parent_table: &str,
        parent_columns: &[String],
        parent_row: &[Value],
    ) -> Result<()> {
        // Collect (child_table, child_column, action, referenced_value) first, so
        // the borrow of self.constraints ends before a child table is mutated.
        let mut actions: Vec<(String, String, RefAction, Value)> = Vec::new();
        for (child_table, tc) in &self.constraints {
            for fk in &tc.foreign_keys {
                if fk.parent_table != parent_table {
                    continue;
                }
                let Some(pidx) = parent_columns.iter().position(|c| c == &fk.parent_column) else {
                    continue;
                };
                let value = parent_row[pidx].clone();
                if matches!(value, Value::Null) {
                    continue;
                }
                actions.push((child_table.clone(), fk.column.clone(), fk.on_delete, value));
            }
        }
        for (child_table, child_col, action, value) in actions {
            let pred = eq_literal(&child_col, &value);
            match action {
                RefAction::NoAction | RefAction::Restrict => {
                    if self.column_has_value(txn, &child_table, &child_col, &value)? {
                        return Err(DbError::Constraint(format!(
                            "foreign key violation: {child_table}.{child_col} still references \
                             {parent_table}"
                        )));
                    }
                }
                RefAction::Cascade => {
                    self.run_delete(txn, &child_table, Some(&pred), &[])?;
                }
                RefAction::SetNull => {
                    let assignments = [(child_col.clone(), Expr::Literal(Value::Null))];
                    self.run_update(txn, &child_table, &assignments, Some(&pred), &[])?;
                }
            }
        }
        Ok(())
    }

    /// Reject (`NO ACTION` / `RESTRICT`) an update that changes a referenced key
    /// while a child still references the old value. Runs before the parent is
    /// written; the `CASCADE` / `SET NULL` actions run after (see
    /// [`Self::apply_fk_on_update`]).
    fn check_fk_update_restrict(
        &self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        old_row: &[Value],
        new_row: &[Value],
    ) -> Result<()> {
        for (child_table, tc) in &self.constraints {
            for fk in &tc.foreign_keys {
                if fk.parent_table != table
                    || !matches!(fk.on_update, RefAction::NoAction | RefAction::Restrict)
                {
                    continue;
                }
                let Some(pidx) = columns.iter().position(|c| c == &fk.parent_column) else {
                    continue;
                };
                let old = &old_row[pidx];
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

    /// Apply the `CASCADE` / `SET NULL` actions to children when a parent's
    /// referenced key changes (the parent already carries the new value). The
    /// `NO ACTION` / `RESTRICT` cases are handled earlier by
    /// [`Self::check_fk_update_restrict`].
    fn apply_fk_on_update(
        &mut self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        old_row: &[Value],
        new_row: &[Value],
    ) -> Result<()> {
        let mut actions: Vec<(String, String, RefAction, Value, Value)> = Vec::new();
        for (child_table, tc) in &self.constraints {
            for fk in &tc.foreign_keys {
                if fk.parent_table != table
                    || !matches!(fk.on_update, RefAction::Cascade | RefAction::SetNull)
                {
                    continue;
                }
                let Some(pidx) = columns.iter().position(|c| c == &fk.parent_column) else {
                    continue;
                };
                let old = old_row[pidx].clone();
                if old == new_row[pidx] || matches!(old, Value::Null) {
                    continue;
                }
                actions.push((
                    child_table.clone(),
                    fk.column.clone(),
                    fk.on_update,
                    old,
                    new_row[pidx].clone(),
                ));
            }
        }
        for (child_table, child_col, action, old, new) in actions {
            let pred = eq_literal(&child_col, &old);
            let assignments = if matches!(action, RefAction::SetNull) {
                [(child_col.clone(), Expr::Literal(Value::Null))]
            } else {
                [(child_col.clone(), Expr::Literal(new))]
            };
            self.run_update(txn, &child_table, &assignments, Some(&pred), &[])?;
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
    fn drop_view(&mut self, if_exists: bool, name: &str) -> Result<QueryOutcome> {
        if self.views.remove(name).is_none() {
            // `IF EXISTS`: a missing view is a no-op, not an error.
            if if_exists {
                return Ok(QueryOutcome::Ddl);
            }
            return Err(DbError::Constraint(format!("view {name} does not exist")));
        }
        self.save_views()?;
        Ok(QueryOutcome::Ddl)
    }

    /// `INSERT INTO t [(cols)] <query>`: run the query, then insert its result
    /// rows through the normal insert path (so constraints, defaults, serials,
    /// and `ON CONFLICT` all apply). The query's column count must match the
    /// insert target's.
    fn insert_select(
        &mut self,
        txn: &Transaction,
        table: &str,
        columns: &[String],
        query: &Statement,
        on_conflict: Option<&OnConflict>,
        returning: &[SelectItem],
    ) -> Result<QueryOutcome> {
        let (_cols, rows) = self.run_query_collect(txn, query)?;
        let exprs: Vec<Vec<Expr>> = rows
            .iter()
            .map(|row| row.iter().map(|v| Expr::Literal(v.clone())).collect())
            .collect();
        if exprs.is_empty() {
            return Ok(QueryOutcome::Mutation { affected: 0 });
        }
        self.insert(txn, table, columns, &exprs, on_conflict, returning)
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
            // Coerce a text value into a DATE / TIMESTAMP column by parsing it,
            // so `INSERT ... VALUES ('2024-01-15')` lands in a date column.
            for (i, &ty) in schema.iter().enumerate() {
                let v = std::mem::replace(&mut values[i], Value::Null);
                values[i] = coerce_value(v, ty)?;
            }
            // NOT NULL.
            for (i, (name, not_null, _)) in col_meta.iter().enumerate() {
                if *not_null && matches!(values[i], Value::Null) {
                    return Err(DbError::Constraint(format!("column {name} cannot be NULL")));
                }
            }
            self.enforce_checks(table, &column_names, &values)?;
            self.enforce_rls_check(table, PolicyCommand::Insert, &column_names, &values)?;
            self.enforce_fk_child(txn, table, &column_names, &values)?;
            // A UNIQUE variable-key index rejects a duplicate value tuple, against
            // both stored rows and earlier rows of this same INSERT.
            self.check_unique_multi(txn, table, &values, None, &built)?;
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
                                    new_row[idx] = coerce_value(
                                        eval(expr, &combined_row, &combined_cols)?,
                                        schema[idx],
                                    )?;
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
                                self.enforce_rls_check(
                                    table,
                                    PolicyCommand::Update,
                                    &column_names,
                                    &new_row,
                                )?;
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
                    put_secondaries(
                        &self.pool,
                        &mut store.secondary,
                        &mut store.multi_secondary,
                        &values,
                        rowid,
                    )?;
                    store.next_rowid += 1;
                    inserted.push(values);
                }
                RowPlan::Update { rowid, new_row } => {
                    handle.update(txn, rowid, &encode_row(&new_row, &schema)?)?;
                    put_secondaries(
                        &self.pool,
                        &mut store.secondary,
                        &mut store.multi_secondary,
                        &new_row,
                        rowid,
                    )?;
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
                    using: j.using.clone(),
                    natural: j.natural,
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
                    nulls_first: o.nulls_first,
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
            Expr::Cast { expr, ty } => Ok(Expr::Cast {
                expr: Box::new(self.fold_expr(txn, expr)?),
                ty: *ty,
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
                        Ok(picklejar_sql::statement::OrderItem {
                            expr: self.fold_expr(txn, &o.expr)?,
                            desc: o.desc,
                            nulls_first: o.nulls_first,
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
        // Pass 1: find matching rows and validate the new versions. Scoped so
        // the scan's borrow of the pool is released before a referential action
        // below mutates a child table.
        let updates: Vec<(u64, Vec<Value>, Vec<Value>)> = {
            let read = MvccTable::open(
                &self.pool,
                self.wal.clone(),
                &self.mgr,
                index_root,
                version_page,
            );
            let mut updates = Vec::new();
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
                    new_row[pos] = coerce_value(eval(expr, &row, &columns)?, schema[pos])?;
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
                self.enforce_rls_check(table, PolicyCommand::Update, &columns, &new_row)?;
                self.enforce_fk_child(txn, table, &columns, &new_row)?;
                // A UNIQUE variable-key index rejects an update that collides with
                // another row (excluding this row's own current entry).
                self.check_unique_multi(txn, table, &new_row, Some(key), &[])?;
                // RESTRICT / NO ACTION on a changed referenced key blocks before
                // any write; CASCADE / SET NULL run after the parent is updated.
                self.check_fk_update_restrict(txn, table, &columns, &row, &new_row)?;
                updates.push((key, row, new_row));
            }
            updates
        };

        // Pass 2: write the validated updates (a mutable table borrow held only
        // inside the helper, so it is released before the actions below).
        let new_rows = self.write_updates(txn, table, &updates, &schema)?;

        // Now the parent rows carry their new keys, cascade ON UPDATE CASCADE /
        // SET NULL to the children that referenced the old keys.
        for (_key, old_row, new_row) in &updates {
            self.apply_fk_on_update(txn, table, &columns, old_row, new_row)?;
        }

        if !returning.is_empty() {
            return project_returning(returning, &columns, &new_rows);
        }
        Ok(QueryOutcome::Mutation {
            affected: new_rows.len(),
        })
    }

    /// Write each validated `(key, old, new)` update into `table`'s storage and
    /// refresh its secondary indexes, returning the new rows. The mutable table
    /// borrow is confined here.
    fn write_updates(
        &mut self,
        txn: &Transaction,
        table: &str,
        updates: &[(u64, Vec<Value>, Vec<Value>)],
        schema: &[DataType],
    ) -> Result<Vec<Vec<Value>>> {
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
        let mut new_rows = Vec::with_capacity(updates.len());
        for (key, _old_row, new_row) in updates {
            handle.update(txn, *key, &encode_row(new_row, schema)?)?;
            // Point each indexed column's key at this rowid's new value. Old
            // values stay in the tree (upsert only) and are filtered on read.
            put_secondaries(
                &self.pool,
                &mut store.secondary,
                &mut store.multi_secondary,
                new_row,
                *key,
            )?;
            new_rows.push(new_row.clone());
        }
        store.index_root = handle.index_root();
        store.version_page = handle.version_page();
        Ok(new_rows)
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

        // Pass 1: find the matching rows (keep each for RETURNING). Scoped so the
        // scan's borrow of the pool is released before a referential action below
        // mutates a child table.
        let victims: Vec<(u64, Vec<Value>)> = {
            let read = MvccTable::open(
                &self.pool,
                self.wal.clone(),
                &self.mgr,
                index_root,
                version_page,
            );
            let mut victims = Vec::new();
            for (key, bytes) in read.scan(txn)? {
                let row = decode_row(&bytes, &schema)?;
                if let Some(pred) = where_clause {
                    if !is_truthy(&eval(pred, &row, &columns)?) {
                        continue;
                    }
                }
                victims.push((key, row));
            }
            victims
        };

        // Apply ON DELETE referential actions (cascade / set null), or reject
        // (RESTRICT), on the children of each row being deleted.
        for (_key, row) in &victims {
            self.apply_fk_on_delete(txn, table, &columns, row)?;
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
    fn scan(&self, table: &str) -> std::result::Result<Relation, picklejar_executor::ExecError> {
        use picklejar_executor::ExecError;
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
    ) -> std::result::Result<Relation, picklejar_executor::ExecError> {
        use picklejar_executor::ExecError;
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

        // No equality matched. Try a range: an order-preserving index turns
        // `col > x` / `col BETWEEN a AND b` into a single B+ tree range scan over
        // the candidate rowids. Each candidate is still resolved through MVCC and
        // re-filtered by the executor, so stale or out-of-snapshot entries drop.
        for sec in &store.secondary {
            let col_name = &meta.columns[sec.column].name;
            let Some((lo, hi)) = find_range(predicate, col_name) else {
                continue;
            };
            let index = Index::open(self.pool, sec.root);
            let candidates = index
                .range_lookup(lo.as_ref(), hi.as_ref())
                .map_err(|e| ExecError::Source(e.to_string()))?;
            let mvcc = MvccTable::open(
                self.pool,
                self.wal.clone(),
                self.mgr,
                store.index_root,
                store.version_page,
            );
            // The index is upsert-only, so a rowid can appear under several keys
            // (an updated value leaves its old key behind). Resolve each rowid at
            // most once, or a row would be returned more than once.
            let mut seen = std::collections::HashSet::new();
            let mut rows = Vec::new();
            for rowid in candidates {
                if !seen.insert(rowid) {
                    continue;
                }
                if let Some(bytes) = mvcc
                    .get(self.txn, rowid)
                    .map_err(|e| ExecError::Source(e.to_string()))?
                {
                    rows.push(decode_row(&bytes, &schema)?);
                }
            }
            return Ok(Relation { columns, rows });
        }

        // Variable-key indexes (explicit CREATE INDEX, including TEXT and
        // non-unique columns). An equality on the leading column is a prefix
        // lookup; a range is a leading-column range scan. Candidates are still
        // deduped, MVCC-resolved, and re-filtered by the executor.
        for m in &store.multi_secondary {
            let Some(&leading) = m.columns.first() else {
                continue;
            };
            let col_name = &meta.columns[leading].name;
            let mindex = MultiIndex::open(self.pool, m.root);
            let candidates = if let Some(value) = find_equality(predicate, col_name) {
                mindex.lookup_prefix(&[&value])
            } else if let Some((lo, hi)) = find_range(predicate, col_name) {
                mindex.range_leading(lo.as_ref(), hi.as_ref())
            } else {
                continue;
            }
            .map_err(|e| ExecError::Source(e.to_string()))?;
            return self.resolve_candidates(store, &columns, &schema, candidates);
        }

        // No physical index matched this predicate: a full scan is still
        // correct (the executor's residual filter does the rest).
        self.scan(table)
    }
}

impl EngineSource<'_> {
    /// Resolve index candidate rowids into rows: dedup (the index is
    /// upsert-only, so a row can appear under several stale keys), fetch each
    /// through MVCC under the current snapshot, and decode. The executor then
    /// re-applies the predicate, so stale or out-of-snapshot entries drop.
    fn resolve_candidates(
        &self,
        store: &TableStore,
        columns: &[String],
        schema: &[DataType],
        candidates: Vec<u64>,
    ) -> std::result::Result<Relation, picklejar_executor::ExecError> {
        use picklejar_executor::ExecError;
        let mvcc = MvccTable::open(
            self.pool,
            self.wal.clone(),
            self.mgr,
            store.index_root,
            store.version_page,
        );
        let mut seen = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for rowid in candidates {
            if !seen.insert(rowid) {
                continue;
            }
            if let Some(bytes) = mvcc
                .get(self.txn, rowid)
                .map_err(|e| ExecError::Source(e.to_string()))?
            {
                rows.push(decode_row(&bytes, schema)?);
            }
        }
        Ok(Relation {
            columns: columns.to_vec(),
            rows,
        })
    }
}

/// Whether a column type gets a physical secondary index. These are exactly the
/// types `index::index_key` maps bijectively and order-preservingly into `u64`.
const fn is_indexable_type(ty: DataType) -> bool {
    matches!(
        ty,
        DataType::Int | DataType::Date | DataType::Timestamp | DataType::Bool
    )
}

/// Whether a column type can be keyed by the variable-key [`MultiIndex`] (see
/// `keyenc`): the order-preserving, self-delimiting types, which add `TEXT` to
/// the fixed set. `FLOAT` / `DECIMAL` / `JSON` are not keyed (a `CREATE INDEX` on
/// them stays catalog-only and the query falls back to a sequential scan).
const fn is_multi_indexable(ty: DataType) -> bool {
    matches!(
        ty,
        DataType::Int | DataType::Date | DataType::Timestamp | DataType::Bool | DataType::Text
    )
}

/// Maintain every physical secondary index after writing `(rowid, values)`:
/// the unique `u64` indexes and the variable-key [`MultiIndex`]es. Each index's
/// root may move as its tree grows, so it is read back into the descriptor.
///
/// Taking the pool and the two index slices (rather than `&self` and the store)
/// keeps the borrows disjoint, so a caller holding `&mut store` can invoke it.
fn put_secondaries(
    pool: &BufferPool,
    secondary: &mut [SecondaryIndex],
    multi: &mut [MultiSecondaryIndex],
    values: &[Value],
    rowid: u64,
) -> Result<()> {
    for sec in secondary.iter_mut() {
        let index = Index::open(pool, sec.root);
        index.put(&values[sec.column], rowid)?;
        sec.root = index.root();
    }
    for m in multi.iter_mut() {
        let mindex = MultiIndex::open(pool, m.root);
        let cols: Vec<&Value> = m.columns.iter().map(|&c| &values[c]).collect();
        mindex.put(&cols, rowid)?;
        m.root = mindex.root();
    }
    Ok(())
}

/// Extract the tightest `[lo, hi]` value bounds a predicate places on `col`
/// through its top-level `AND` conjuncts, or `None` if it constrains `col` with
/// no range comparison.
///
/// Only `AND` is descended, so every bound returned is a necessary condition on
/// `col`: the index range scan it drives can never miss a qualifying row (`OR`,
/// `NOT`, and the like are left for the residual filter). On a conflicting pair
/// (`col > 5 AND col > 8`) the looser bound is kept, which only widens the scan.
fn find_range(predicate: &Expr, col: &str) -> Option<(Bound<Value>, Bound<Value>)> {
    let mut lo = Bound::Unbounded;
    let mut hi = Bound::Unbounded;
    collect_bounds(predicate, col, &mut lo, &mut hi);
    if matches!(lo, Bound::Unbounded) && matches!(hi, Bound::Unbounded) {
        None
    } else {
        Some((lo, hi))
    }
}

/// Accumulate the value bounds `predicate` places on `col` into `lo` / `hi`,
/// descending `AND` only.
fn collect_bounds(predicate: &Expr, col: &str, lo: &mut Bound<Value>, hi: &mut Bound<Value>) {
    match predicate {
        Expr::Binary {
            op: BinOp::And,
            left,
            right,
        } => {
            collect_bounds(left, col, lo, hi);
            collect_bounds(right, col, lo, hi);
        }
        Expr::Binary { op, left, right }
            if matches!(op, BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) =>
        {
            // Orient the comparison so the column is on the left.
            let (op, value) = if expr_is_column(left, col) {
                (*op, const_eval(right))
            } else if expr_is_column(right, col) {
                (flip_comparison(*op), const_eval(left))
            } else {
                return;
            };
            let Ok(value) = value else {
                return;
            };
            match op {
                BinOp::Gt => widen_lower(lo, Bound::Excluded(value)),
                BinOp::Ge => widen_lower(lo, Bound::Included(value)),
                BinOp::Lt => widen_upper(hi, Bound::Excluded(value)),
                BinOp::Le => widen_upper(hi, Bound::Included(value)),
                _ => {}
            }
        }
        _ => {}
    }
}

/// Keep the looser of two lower bounds (so the scan never under-covers).
fn widen_lower(current: &mut Bound<Value>, candidate: Bound<Value>) {
    let looser = match (&*current, &candidate) {
        (Bound::Unbounded, _) => candidate,
        (cur, cand) => {
            if bound_value(cand).is_some_and(|c| bound_value(cur).is_some_and(|v| compare_lt(c, v)))
            {
                candidate
            } else {
                return;
            }
        }
    };
    *current = looser;
}

/// Keep the looser of two upper bounds.
fn widen_upper(current: &mut Bound<Value>, candidate: Bound<Value>) {
    let looser = match (&*current, &candidate) {
        (Bound::Unbounded, _) => candidate,
        (cur, cand) => {
            if bound_value(cand).is_some_and(|c| bound_value(cur).is_some_and(|v| compare_lt(v, c)))
            {
                candidate
            } else {
                return;
            }
        }
    };
    *current = looser;
}

/// The value carried by a bound, if any.
const fn bound_value(bound: &Bound<Value>) -> Option<&Value> {
    match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v),
        Bound::Unbounded => None,
    }
}

/// True if `a` orders strictly before `b` under the index key map (so it is only
/// meaningful for the indexable, order-preserving types).
const fn compare_lt(a: &Value, b: &Value) -> bool {
    match (crate::index::index_key(a), crate::index::index_key(b)) {
        (Some(ka), Some(kb)) => ka < kb,
        _ => false,
    }
}

/// Flip a comparison operator's sense (for `const <op> column`).
const fn flip_comparison(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
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

/// Apply parsed role attributes onto `attrs`, last write winning.
fn apply_role_options(attrs: &mut RoleAttrs, options: &[RoleOption]) {
    for opt in options {
        match opt {
            RoleOption::Superuser(on) => attrs.superuser = *on,
            RoleOption::Login(on) => attrs.login = *on,
            RoleOption::CreateRole(on) => attrs.createrole = *on,
            RoleOption::BypassRls(on) => attrs.bypassrls = *on,
            // The password value itself is not stored (wire authentication is
            // handled by the server); only whether one is set.
            RoleOption::Password(p) => attrs.has_password = p.is_some(),
        }
    }
}

/// The catalog key for a grantee: a role's name, or `"public"` for `PUBLIC`.
fn grantee_name(grantee: &Grantee) -> String {
    match grantee {
        Grantee::Role(name) => name.clone(),
        Grantee::Public => "public".to_string(),
    }
}

/// `set` with `victim` removed (used to split an UPDATE/DELETE target, which
/// needs a write privilege, from the other tables it reads).
fn others(mut set: HashSet<String>, victim: &str) -> HashSet<String> {
    set.remove(victim);
    set
}

/// A duplicate-key error for a `UNIQUE` index named `index`.
fn unique_violation(index: &str) -> DbError {
    DbError::Constraint(format!(
        "duplicate key value violates unique index \"{index}\""
    ))
}

/// The role names a policy applies to. An empty list, or one naming `PUBLIC`,
/// means every role.
fn policy_roles(grantees: &[Grantee]) -> Vec<String> {
    if grantees.is_empty() || grantees.iter().any(|g| matches!(g, Grantee::Public)) {
        Vec::new()
    } else {
        grantees.iter().map(grantee_name).collect()
    }
}

/// Whether a policy applies to `command` for `role`.
fn policy_matches(policy: &Policy, command: PolicyCommand, role: &str) -> bool {
    let command_ok = matches!(policy.command, PolicyCommand::All) || policy.command == command;
    let role_ok = policy.roles.is_empty() || policy.roles.iter().any(|r| r == role);
    command_ok && role_ok
}

/// Reconstruct a policy's `CREATE POLICY` text for persistence.
fn policy_to_sql(table: &str, policy: &Policy) -> String {
    Statement::CreatePolicy {
        name: policy.name.clone(),
        table: table.to_string(),
        command: policy.command,
        roles: policy
            .roles
            .iter()
            .map(|r| Grantee::Role(r.clone()))
            .collect(),
        using: policy.using.clone(),
        check: policy.check.clone(),
    }
    .to_string()
}

/// Qualify every bare column reference in `expr` with `qualifier`, so a policy
/// predicate is unambiguous when its table is aliased or joined. Already-qualified
/// columns, literals, and functions (e.g. `current_user`) are left alone, and the
/// rewrite does not descend into subqueries (they carry their own scope).
fn qualify_expr(expr: &Expr, qualifier: &str) -> Expr {
    match expr {
        Expr::Column(c) => Expr::QualifiedColumn(qualifier.to_string(), c.clone()),
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: Box::new(qualify_expr(left, qualifier)),
            right: Box::new(qualify_expr(right, qualifier)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op: *op,
            expr: Box::new(qualify_expr(expr, qualifier)),
        },
        Expr::Func {
            name,
            distinct,
            args,
        } => Expr::Func {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(|a| qualify_expr(a, qualifier)).collect(),
        },
        Expr::Cast { expr, ty } => Expr::Cast {
            expr: Box::new(qualify_expr(expr, qualifier)),
            ty: *ty,
        },
        Expr::Case {
            operand,
            whens,
            else_result,
        } => Expr::Case {
            operand: operand
                .as_ref()
                .map(|o| Box::new(qualify_expr(o, qualifier))),
            whens: whens
                .iter()
                .map(|(w, t)| (qualify_expr(w, qualifier), qualify_expr(t, qualifier)))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(qualify_expr(e, qualifier))),
        },
        other => other.clone(),
    }
}

/// `OR` a list of predicates, or `FALSE` when empty (RLS default deny).
fn or_all(mut terms: Vec<Expr>) -> Expr {
    let Some(first) = terms.pop() else {
        return Expr::Literal(Value::Bool(false));
    };
    terms
        .into_iter()
        .fold(first, |acc, t| Expr::binary(BinOp::Or, t, acc))
}

/// `AND` a non-empty list of predicates.
fn and_all(mut terms: Vec<Expr>) -> Expr {
    let first = terms.pop().expect("and_all called with at least one term");
    terms
        .into_iter()
        .fold(first, |acc, t| Expr::binary(BinOp::And, t, acc))
}

/// `AND` an optional existing predicate with a required one.
fn and_two(existing: Option<Expr>, extra: Expr) -> Expr {
    match existing {
        Some(w) => Expr::binary(BinOp::And, w, extra),
        None => extra,
    }
}

/// `AND` an optional guard into an optional WHERE clause.
fn and_opt(existing: Option<Expr>, guard: Option<Expr>) -> Option<Expr> {
    match guard {
        None => existing,
        Some(g) => Some(and_two(existing, g)),
    }
}

/// Add an RLS guard for one table reference: recurse into a derived-table
/// subquery, or push the base table's policy predicate (qualified by its alias or
/// name) onto `guards`.
fn rls_guard_table_ref(db: &Database, table: &mut TableRef, guards: &mut Vec<Expr>) {
    if let Some(sub) = table.subquery.take() {
        table.subquery = Some(Box::new(db.apply_rls(*sub)));
        return;
    }
    let qualifier = table.alias.clone().unwrap_or_else(|| table.name.clone());
    if let Some(pred) = db.rls_predicate(&table.name, PolicyCommand::Select, Some(&qualifier)) {
        guards.push(pred);
    }
}

/// Every base-table name a statement reads, for privilege checks. Walks
/// `FROM` / `JOIN`, derived tables, and subqueries in expressions. Names with no
/// physical store (views, CTEs, system catalogs) are harmless: the privilege
/// check skips them.
fn referenced_tables(stmt: &Statement) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_stmt_tables(stmt, &mut out);
    out
}

/// Accumulate the base tables a statement reads into `out`.
fn collect_stmt_tables(stmt: &Statement, out: &mut HashSet<String>) {
    match stmt {
        Statement::Select(select) => collect_select_tables(select, out),
        Statement::Union { left, right, .. } => {
            collect_stmt_tables(left, out);
            collect_stmt_tables(right, out);
        }
        Statement::With { body, ctes, .. } => {
            collect_stmt_tables(body, out);
            for cte in ctes {
                collect_stmt_tables(&cte.query, out);
            }
        }
        Statement::Insert { source, rows, .. } => {
            if let Some(src) = source {
                collect_stmt_tables(src, out);
            }
            for row in rows {
                for e in row {
                    collect_expr_tables(e, out);
                }
            }
        }
        Statement::Update {
            table,
            assignments,
            where_clause,
            ..
        } => {
            out.insert(table.clone());
            for (_, e) in assignments {
                collect_expr_tables(e, out);
            }
            if let Some(w) = where_clause {
                collect_expr_tables(w, out);
            }
        }
        Statement::Delete {
            table,
            where_clause,
            ..
        } => {
            out.insert(table.clone());
            if let Some(w) = where_clause {
                collect_expr_tables(w, out);
            }
        }
        Statement::Explain { statement, .. }
        | Statement::CreateTableAs {
            query: statement, ..
        } => {
            collect_stmt_tables(statement, out);
        }
        Statement::CreateView { query, .. } => collect_stmt_tables(query, out),
        _ => {}
    }
}

/// Accumulate the base tables a `SELECT` reads: its `FROM`, joins, and any
/// subqueries in its clauses.
fn collect_select_tables(select: &Select, out: &mut HashSet<String>) {
    collect_table_ref(&select.from, out);
    for join in &select.joins {
        collect_table_ref(&join.table, out);
        collect_expr_tables(&join.on, out);
    }
    for item in &select.projections {
        if let SelectItem::Expr(e, _) = item {
            collect_expr_tables(e, out);
        }
    }
    if let Some(w) = &select.where_clause {
        collect_expr_tables(w, out);
    }
    for e in &select.group_by {
        collect_expr_tables(e, out);
    }
    if let Some(h) = &select.having {
        collect_expr_tables(h, out);
    }
    for o in &select.order_by {
        collect_expr_tables(&o.expr, out);
    }
}

/// Add a table reference: a base name directly, or recurse into a derived-table
/// subquery.
fn collect_table_ref(table: &TableRef, out: &mut HashSet<String>) {
    if let Some(sub) = &table.subquery {
        collect_stmt_tables(sub, out);
    } else {
        out.insert(table.name.clone());
    }
}

/// Accumulate base tables referenced by subqueries inside an expression.
fn collect_expr_tables(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Subquery(s) | Expr::Exists(s) => collect_stmt_tables(s, out),
        Expr::InSubquery { expr, query, .. } => {
            collect_expr_tables(expr, out);
            collect_stmt_tables(query, out);
        }
        Expr::Binary { left, right, .. } => {
            collect_expr_tables(left, out);
            collect_expr_tables(right, out);
        }
        Expr::Unary { expr, .. } | Expr::Cast { expr, .. } => collect_expr_tables(expr, out),
        Expr::Func { args, .. } | Expr::Window { args, .. } => {
            for a in args {
                collect_expr_tables(a, out);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            if let Some(op) = operand {
                collect_expr_tables(op, out);
            }
            for (w, t) in whens {
                collect_expr_tables(w, out);
                collect_expr_tables(t, out);
            }
            if let Some(e) = else_result {
                collect_expr_tables(e, out);
            }
        }
        _ => {}
    }
}

/// Parse a CSV file body into records of string fields (RFC-4180 style: fields
/// are comma-separated; a `"`-quoted field may contain commas, newlines, and
/// `""`-escaped quotes). A trailing newline does not add an empty record.
fn parse_csv(content: &str) -> Vec<Vec<String>> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => record.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(c),
            }
        }
    }
    // Flush a final record that did not end in a newline.
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

/// Render one CSV record: each field comma-joined, quoting only those that need
/// it (containing a comma, quote, or newline).
fn csv_record(fields: &[String]) -> String {
    fields
        .iter()
        .map(|f| {
            if f.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", f.replace('"', "\"\""))
            } else {
                f.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The raw (unquoted) CSV text for a value; NULL becomes an empty field.
fn csv_field_from_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Int(n) => n.to_string(),
        Value::Float(x) => format!("{x}"),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Date(days) => picklejar_sql::datetime::format_date(*days),
        Value::Timestamp(micros) => picklejar_sql::datetime::format_timestamp(*micros),
        Value::Decimal(m, s) => picklejar_sql::decimal::format(*m, *s),
        Value::Vector(v) => picklejar_sql::ast::format_vector(v),
        Value::Text(s) | Value::Json(s) => s.clone(),
    }
}

/// Convert one CSV field to a value of the column's type; an empty field is
/// NULL.
fn value_from_csv_field(field: &str, ty: DataType) -> Result<Value> {
    if field.is_empty() {
        return Ok(Value::Null);
    }
    Ok(match ty {
        DataType::Int => Value::Int(
            field
                .parse()
                .map_err(|_| DbError::Constraint(format!("invalid integer in CSV: {field:?}")))?,
        ),
        DataType::Float => Value::Float(
            field
                .parse()
                .map_err(|_| DbError::Constraint(format!("invalid float in CSV: {field:?}")))?,
        ),
        DataType::Bool => match field.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Value::Bool(true),
            "false" | "f" | "0" => Value::Bool(false),
            _ => {
                return Err(DbError::Constraint(format!(
                    "invalid boolean in CSV: {field:?}"
                )))
            }
        },
        DataType::Date => Value::Date(
            picklejar_sql::datetime::parse_date(field)
                .ok_or_else(|| DbError::Constraint(format!("invalid date in CSV: {field:?}")))?,
        ),
        DataType::Timestamp => Value::Timestamp(
            picklejar_sql::datetime::parse_timestamp(field).ok_or_else(|| {
                DbError::Constraint(format!("invalid timestamp in CSV: {field:?}"))
            })?,
        ),
        DataType::Json => Value::Json(if picklejar_sql::json::is_valid(field) {
            field.to_string()
        } else {
            return Err(DbError::Constraint(format!(
                "invalid json in CSV: {field:?}"
            )));
        }),
        DataType::Decimal => {
            let (m, s) = picklejar_sql::decimal::parse(field)
                .ok_or_else(|| DbError::Constraint(format!("invalid decimal in CSV: {field:?}")))?;
            Value::Decimal(m, s)
        }
        DataType::Vector(_) => Value::Vector(
            picklejar_sql::ast::parse_vector(field)
                .ok_or_else(|| DbError::Constraint(format!("invalid vector in CSV: {field:?}")))?,
        ),
        DataType::Text => Value::Text(field.to_string()),
    })
}

/// Infer a column's type from the first non-NULL value in `rows` at index `i`,
/// defaulting to `INT` for an all-NULL or empty column.
fn column_type(rows: &[Vec<Value>], i: usize) -> DataType {
    rows.iter()
        .find_map(|r| match r.get(i) {
            Some(Value::Int(_)) => Some(DataType::Int),
            Some(Value::Float(_)) => Some(DataType::Float),
            Some(Value::Json(_)) => Some(DataType::Json),
            Some(Value::Decimal(..)) => Some(DataType::Decimal),
            Some(Value::Text(_)) => Some(DataType::Text),
            Some(Value::Bool(_)) => Some(DataType::Bool),
            Some(Value::Date(_)) => Some(DataType::Date),
            Some(Value::Timestamp(_)) => Some(DataType::Timestamp),
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
        Value::Json(s) => {
            b.push(8);
            b.extend_from_slice(s.as_bytes());
        }
        Value::Decimal(m, s) => {
            let (nm, ns) = picklejar_sql::decimal::normalize(*m, *s);
            b.push(9);
            b.extend_from_slice(&nm.to_le_bytes());
            b.extend_from_slice(&ns.to_le_bytes());
        }
        Value::Bool(x) => {
            b.push(3);
            b.push(u8::from(*x));
        }
        Value::Float(x) => {
            b.push(4);
            b.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Date(n) => {
            b.push(5);
            b.extend_from_slice(&n.to_le_bytes());
        }
        Value::Timestamp(n) => {
            b.push(6);
            b.extend_from_slice(&n.to_le_bytes());
        }
        Value::Vector(v) => {
            b.push(10);
            for x in v {
                b.extend_from_slice(&x.to_bits().to_le_bytes());
            }
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
        DataType::Date => "date",
        DataType::Timestamp => "timestamp without time zone",
        DataType::Json => "json",
        DataType::Decimal => "numeric",
        DataType::Vector(_) => "vector",
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
        if_exists: true,
        name: name.to_string(),
    });
    let create = Statement::CreateTable {
        if_not_exists: false,
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
            using: j.using.clone(),
            natural: j.natural,
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

/// Whether `expr` mentions the column `col` anywhere in its tree. Used to keep
/// `ALTER TABLE` from renaming or dropping a column a `CHECK` predicate names.
fn expr_mentions_column(expr: &Expr, col: &str) -> bool {
    match expr {
        Expr::Column(c) | Expr::QualifiedColumn(_, c) => c == col,
        Expr::Binary { left, right, .. } => {
            expr_mentions_column(left, col) || expr_mentions_column(right, col)
        }
        Expr::Unary { expr, .. } | Expr::InSubquery { expr, .. } => expr_mentions_column(expr, col),
        Expr::Func { args, .. } => args.iter().any(|a| expr_mentions_column(a, col)),
        Expr::Case {
            operand,
            whens,
            else_result,
        } => {
            operand
                .as_deref()
                .is_some_and(|o| expr_mentions_column(o, col))
                || whens
                    .iter()
                    .any(|(w, t)| expr_mentions_column(w, col) || expr_mentions_column(t, col))
                || else_result
                    .as_deref()
                    .is_some_and(|e| expr_mentions_column(e, col))
        }
        Expr::Window {
            args,
            partition_by,
            order_by,
            ..
        } => {
            args.iter().any(|a| expr_mentions_column(a, col))
                || partition_by.iter().any(|p| expr_mentions_column(p, col))
                || order_by.iter().any(|o| expr_mentions_column(&o.expr, col))
        }
        _ => false,
    }
}

/// Build the predicate `column = value`, for cascading a referential action to
/// the child rows that reference a given key.
fn eq_literal(column: &str, value: &Value) -> Expr {
    Expr::Binary {
        op: BinOp::Eq,
        left: Box::new(Expr::Column(column.to_string())),
        right: Box::new(Expr::Literal(value.clone())),
    }
}

/// The compact sidecar token for a referential action.
const fn ref_action_token(a: RefAction) -> &'static str {
    match a {
        RefAction::NoAction => "noaction",
        RefAction::Restrict => "restrict",
        RefAction::Cascade => "cascade",
        RefAction::SetNull => "setnull",
    }
}

/// Parse a referential-action token back from the sidecar (unknown defaults to
/// `NO ACTION`).
fn ref_action_from_token(s: &str) -> RefAction {
    match s {
        "restrict" => RefAction::Restrict,
        "cascade" => RefAction::Cascade,
        "setnull" => RefAction::SetNull,
        _ => RefAction::NoAction,
    }
}

/// Coerce a value to a column's declared type where there is a well-defined
/// implicit conversion: a text literal into a `DATE` / `TIMESTAMP` (parsed), and
/// a `DATE` into a `TIMESTAMP` (taken at midnight). Every other value passes
/// through unchanged for the row codec to type-check.
fn coerce_value(value: Value, ty: DataType) -> Result<Value> {
    match (&value, ty) {
        (Value::Text(s), DataType::Date) => picklejar_sql::datetime::parse_date(s)
            .map(Value::Date)
            .ok_or_else(|| DbError::Constraint(format!("invalid date literal {s:?}"))),
        (Value::Text(s), DataType::Timestamp) => picklejar_sql::datetime::parse_timestamp(s)
            .map(Value::Timestamp)
            .ok_or_else(|| DbError::Constraint(format!("invalid timestamp literal {s:?}"))),
        (Value::Date(days), DataType::Timestamp) => Ok(Value::Timestamp(
            days * picklejar_sql::datetime::MICROS_PER_DAY,
        )),
        // A text value into a JSON column is validated and stored.
        (Value::Text(s), DataType::Json) => {
            if picklejar_sql::json::is_valid(s) {
                Ok(Value::Json(s.clone()))
            } else {
                Err(DbError::Constraint(format!("invalid json literal {s:?}")))
            }
        }
        // Into a DECIMAL column: text and float are parsed exactly from their
        // text form; an integer takes scale 0.
        (Value::Text(s), DataType::Decimal) => picklejar_sql::decimal::parse(s)
            .map(|(m, sc)| Value::Decimal(m, sc))
            .ok_or_else(|| DbError::Constraint(format!("invalid decimal literal {s:?}"))),
        (Value::Int(n), DataType::Decimal) => {
            let (m, sc) = picklejar_sql::decimal::from_i64(*n);
            Ok(Value::Decimal(m, sc))
        }
        (Value::Float(x), DataType::Decimal) => {
            let text = x.to_string();
            picklejar_sql::decimal::parse(&text)
                .map(|(m, sc)| Value::Decimal(m, sc))
                .ok_or_else(|| DbError::Constraint(format!("invalid decimal {text:?}")))
        }
        // A text literal into a VECTOR column is parsed from its `[a,b,c]` form,
        // then width-checked against the column's declared dimension.
        (Value::Text(s), DataType::Vector(dim)) => {
            let v = picklejar_sql::ast::parse_vector(s)
                .ok_or_else(|| DbError::Constraint(format!("invalid vector literal {s:?}")))?;
            check_vector_dim(&v, dim)?;
            Ok(Value::Vector(v))
        }
        // A vector value already of the right kind still has to match the
        // declared width.
        (Value::Vector(v), DataType::Vector(dim)) => {
            check_vector_dim(v, dim)?;
            Ok(value)
        }
        _ => Ok(value),
    }
}

/// Validate a vector's length against a column's declared dimension. A declared
/// dimension of `0` means the column is width-agnostic and accepts any length.
fn check_vector_dim(v: &[f32], dim: u32) -> Result<()> {
    if dim != 0 && v.len() != dim as usize {
        return Err(DbError::Constraint(format!(
            "vector has {} dimensions, column expects {dim}",
            v.len()
        )));
    }
    Ok(())
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
        // A cast over a constant (e.g. `CAST('5' AS INT)` in VALUES or a
        // DEFAULT) folds to its converted value.
        Expr::Cast { expr, ty } => Ok(picklejar_executor::eval::cast(&const_eval(expr)?, *ty)?),
        _ => Err(DbError::Unsupported(
            "non-constant expression in INSERT".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use picklejar_executor::decode_row;
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
    fn if_not_exists_is_idempotent() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t (id) VALUES (1)").unwrap();
        // A second CREATE with IF NOT EXISTS succeeds and leaves the table (and
        // its row) untouched, even with a different column shape.
        db.execute("CREATE TABLE IF NOT EXISTS t (other TEXT)")
            .unwrap();
        assert_eq!(db.table_count(), 1);
        let (cols, rows) = query(&mut db, "SELECT id FROM t");
        assert_eq!(names(&cols), ["id"]);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn drop_if_exists_is_a_noop_when_absent() {
        let (_dir, mut db) = db();
        // Dropping a table that was never created is an error normally...
        assert!(db.execute("DROP TABLE ghost").is_err());
        // ...but a no-op (Ddl, no error) under IF EXISTS.
        assert!(matches!(
            db.execute("DROP TABLE IF EXISTS ghost").unwrap(),
            QueryOutcome::Ddl
        ));
        assert!(matches!(
            db.execute("DROP VIEW IF EXISTS ghost_view").unwrap(),
            QueryOutcome::Ddl
        ));
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

    // --- VECTOR columns ---

    #[test]
    fn vector_column_round_trips() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE docs (id INT, embedding VECTOR(3))")
            .unwrap();
        // A typed VECTOR literal and a bare text literal both reach the column.
        db.execute("INSERT INTO docs VALUES (1, VECTOR '[1, 2, 3]')")
            .unwrap();
        db.execute("INSERT INTO docs VALUES (2, '[4, 5, 6]')")
            .unwrap();
        let (_, rows) = query(&mut db, "SELECT embedding FROM docs ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Vector(vec![1.0, 2.0, 3.0])],
                vec![Value::Vector(vec![4.0, 5.0, 6.0])],
            ]
        );
    }

    #[test]
    fn vector_dimension_mismatch_is_rejected() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE docs (id INT, embedding VECTOR(3))")
            .unwrap();
        let err = db
            .execute("INSERT INTO docs VALUES (1, '[1, 2]')")
            .unwrap_err();
        assert!(matches!(err, DbError::Constraint(m) if m.contains("dimensions")));
    }

    #[test]
    fn dimensionless_vector_accepts_any_width() {
        let (_dir, mut db) = db();
        // No declared dimension: the column accepts vectors of any length.
        db.execute("CREATE TABLE docs (id INT, embedding VECTOR)")
            .unwrap();
        db.execute("INSERT INTO docs VALUES (1, '[1, 2]')").unwrap();
        db.execute("INSERT INTO docs VALUES (2, '[3, 4, 5, 6]')")
            .unwrap();
        let (_, rows) = query(&mut db, "SELECT embedding FROM docs ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Vector(vec![1.0, 2.0])],
                vec![Value::Vector(vec![3.0, 4.0, 5.0, 6.0])],
            ]
        );
    }

    #[test]
    fn vector_column_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("vec.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE docs (id INT, embedding VECTOR(3))")
                .unwrap();
            db.execute("INSERT INTO docs VALUES (1, '[0.5, -1, 2]')")
                .unwrap();
        }
        // The declared dimension and stored components both reload.
        let mut db = Database::open(&path).expect("reopen");
        let (_, rows) = query(&mut db, "SELECT embedding FROM docs");
        assert_eq!(rows, vec![vec![Value::Vector(vec![0.5, -1.0, 2.0])]]);
        // The reloaded column still enforces its width.
        assert!(db.execute("INSERT INTO docs VALUES (2, '[1, 2]')").is_err());
    }

    /// Pull a single FLOAT scalar out of a one-row, one-column result.
    fn scalar_float(db: &mut Database, sql: &str) -> f64 {
        match query(db, sql).1.as_slice() {
            [row] => match row.as_slice() {
                [Value::Float(x)] => *x,
                other => panic!("expected one float, got {other:?}"),
            },
            other => panic!("expected one row, got {other:?}"),
        }
    }

    #[test]
    fn vector_distance_operators_compute() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE d (id INT, e VECTOR(3))").unwrap();
        db.execute("INSERT INTO d VALUES (1, '[1, 2, 3]')").unwrap();
        // L2: |[1,2,3] - [4,6,3]| = |(3,4,0)| = 5.
        let l2 = scalar_float(&mut db, "SELECT e <-> '[4, 6, 3]' FROM d");
        assert!((l2 - 5.0).abs() < 1e-9, "l2 was {l2}");
        // Negative inner product: -(1*4 + 2*6 + 3*3) = -25.
        let ip = scalar_float(&mut db, "SELECT e <#> '[4, 6, 3]' FROM d");
        assert!((ip + 25.0).abs() < 1e-9, "inner product was {ip}");
        // L1 (Manhattan): |1-4| + |2-6| + |3-3| = 7.
        let l1 = scalar_float(&mut db, "SELECT e <+> '[4, 6, 3]' FROM d");
        assert!((l1 - 7.0).abs() < 1e-9, "l1 was {l1}");
        // The function form agrees with the operator.
        let l1f = scalar_float(&mut db, "SELECT l1_distance(e, '[4, 6, 3]') FROM d");
        assert!((l1f - 7.0).abs() < 1e-9, "l1_distance was {l1f}");
        // Cosine distance against an identical direction is 0; against an
        // orthogonal vector it is 1.
        let same = scalar_float(&mut db, "SELECT e <=> '[2, 4, 6]' FROM d");
        assert!(same.abs() < 1e-9, "cosine of parallel vectors was {same}");
        db.execute("INSERT INTO d VALUES (2, '[1, 0, 0]')").unwrap();
        let orth = scalar_float(&mut db, "SELECT e <=> '[0, 1, 0]' FROM d WHERE id = 2");
        assert!(
            (orth - 1.0).abs() < 1e-9,
            "cosine of orthogonal vectors was {orth}"
        );
    }

    #[test]
    fn knn_orders_by_distance() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE docs (id INT, e VECTOR(2))")
            .unwrap();
        db.execute(
            "INSERT INTO docs VALUES (1, '[0, 0]'), (2, '[1, 1]'), (3, '[3, 4]'), (4, '[10, 10]')",
        )
        .unwrap();
        // Nearest two to the origin are ids 1 (dist 0) then 2 (dist sqrt 2).
        let (_, rows) = query(
            &mut db,
            "SELECT id FROM docs ORDER BY e <-> '[0, 0]' LIMIT 2",
        );
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        // A distance threshold in WHERE binds tighter than the comparison, so
        // `e <-> q < 6` reads as `(e <-> q) < 6` and keeps ids 1, 2, 3 (their
        // distances 0, sqrt 2, and 5 are all under 6; id 4 at ~14.1 is out).
        let (_, rows) = query(
            &mut db,
            "SELECT id FROM docs WHERE e <-> '[0, 0]' < 6 ORDER BY id",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1)],
                vec![Value::Int(2)],
                vec![Value::Int(3)]
            ]
        );
    }

    #[test]
    fn vector_scalar_functions_compute() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE d (id INT, e VECTOR(3))").unwrap();
        db.execute("INSERT INTO d VALUES (1, '[3, 4, 0]')").unwrap();
        // vector_dims and l2_norm (|[3,4,0]| = 5).
        let (_c, rows) = query(&mut db, "SELECT vector_dims(e), l2_norm(e) FROM d");
        assert_eq!(rows[0][0], Value::Int(3));
        match rows[0][1] {
            Value::Float(x) => assert!((x - 5.0).abs() < 1e-9, "l2_norm was {x}"),
            ref other => panic!("expected float, got {other:?}"),
        }
        // The function forms of the operators agree with the operators.
        let l2 = scalar_float(&mut db, "SELECT l2_distance(e, '[0, 0, 0]') FROM d");
        assert!((l2 - 5.0).abs() < 1e-9, "l2_distance was {l2}");
        // inner_product is the positive dot product: [3,4,0].[1,1,1] = 7.
        let ip = scalar_float(&mut db, "SELECT inner_product(e, '[1, 1, 1]') FROM d");
        assert!((ip - 7.0).abs() < 1e-9, "inner_product was {ip}");
    }

    #[test]
    fn vector_distance_dimension_mismatch_errors() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE d (id INT, e VECTOR(3))").unwrap();
        db.execute("INSERT INTO d VALUES (1, '[1, 2, 3]')").unwrap();
        assert!(db.execute("SELECT e <-> '[1, 2]' FROM d").is_err());
    }

    #[test]
    fn knn_index_search_matches_exact_and_respects_rls() {
        use crate::hnsw::Metric;
        let (_d, mut db) = db();
        db.execute("CREATE TABLE m (id INT, tenant TEXT, e VECTOR(2))")
            .unwrap();
        db.execute(
            "INSERT INTO m VALUES (1,'a','[0,0]'),(2,'a','[1,1]'),(3,'a','[5,5]'),(4,'b','[0,0]')",
        )
        .unwrap();
        let ids = |rows: &[Vec<Value>]| -> Vec<i64> {
            rows.iter()
                .map(|r| match r[0] {
                    Value::Int(n) => n,
                    ref o => panic!("expected int id, got {o:?}"),
                })
                .collect()
        };
        // No RLS: the two nearest to the origin are the two [0,0] rows (ids 1, 4).
        let got = db.knn("m", "e", &[0.0, 0.0], 2, Metric::L2).unwrap();
        let near = ids(&got);
        assert!(near.contains(&1) && near.contains(&4), "got {near:?}");

        // With RLS, a tenant's index search can only ever return its own rows,
        // because the rows are sourced through a SELECT that the policy filters,
        // even though tenant b holds the globally-nearest vector.
        db.execute("GRANT SELECT ON m TO PUBLIC").unwrap();
        db.execute("CREATE ROLE a LOGIN").unwrap();
        db.execute("CREATE POLICY p ON m USING ((tenant = current_user()))")
            .unwrap();
        db.execute("ALTER TABLE m ENABLE ROW LEVEL SECURITY")
            .unwrap();
        db.set_session_user("a");
        let got = db.knn("m", "e", &[0.0, 0.0], 5, Metric::L2).unwrap();
        let mine = ids(&got);
        assert!(
            mine.iter().all(|id| (1..=3).contains(id)),
            "the index search leaked another tenant's row: {mine:?}"
        );
    }

    fn names(cols: &[String]) -> Vec<&str> {
        cols.iter().map(String::as_str).collect()
    }

    // --- roles, privileges, and ownership ---

    /// The bootstrap superuser, used to switch a test session back to full
    /// rights after acting as a restricted role.
    const SUPER: &str = security::BOOTSTRAP_SUPERUSER;

    #[test]
    fn table_privileges_are_enforced_for_non_superusers() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        db.execute("CREATE ROLE alice").unwrap();

        db.set_session_user("alice");
        // With no grant, every access is refused.
        assert!(matches!(
            db.execute("SELECT * FROM t").unwrap_err(),
            DbError::PermissionDenied(_)
        ));
        assert!(matches!(
            db.execute("INSERT INTO t VALUES (3, 'c')").unwrap_err(),
            DbError::PermissionDenied(_)
        ));

        // The owner (superuser) grants SELECT.
        db.set_session_user(SUPER);
        db.execute("GRANT SELECT ON t TO alice").unwrap();
        db.set_session_user("alice");
        let (_c, rows) = query(&mut db, "SELECT id FROM t ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        // SELECT does not imply INSERT.
        assert!(db.execute("INSERT INTO t VALUES (3, 'c')").is_err());

        // Grant INSERT too, then it works; REVOKE takes it back.
        db.set_session_user(SUPER);
        db.execute("GRANT INSERT ON t TO alice").unwrap();
        db.set_session_user("alice");
        db.execute("INSERT INTO t VALUES (3, 'c')").unwrap();
        db.set_session_user(SUPER);
        db.execute("REVOKE INSERT ON t FROM alice").unwrap();
        db.set_session_user("alice");
        assert!(db.execute("INSERT INTO t VALUES (4, 'd')").is_err());
    }

    #[test]
    fn creator_owns_its_table_and_holds_every_privilege() {
        let (_d, mut db) = db();
        db.execute("CREATE ROLE carol LOGIN").unwrap();
        db.set_session_user("carol");
        // CREATE TABLE is allowed for any role; the creator owns the result.
        db.execute("CREATE TABLE c (id INT)").unwrap();
        db.execute("INSERT INTO c VALUES (1), (2)").unwrap();
        let (_c, rows) = query(&mut db, "SELECT id FROM c ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        // The owner can even drop it.
        db.execute("DROP TABLE c").unwrap();
    }

    #[test]
    fn public_and_membership_grants_reach_a_role() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("CREATE ROLE readers").unwrap();
        db.execute("CREATE ROLE dave").unwrap();
        db.execute("GRANT SELECT ON t TO readers").unwrap();
        db.execute("GRANT readers TO dave").unwrap();

        // dave inherits readers' SELECT through membership.
        db.set_session_user("dave");
        let (_c, rows) = query(&mut db, "SELECT id FROM t");
        assert_eq!(rows, vec![vec![Value::Int(1)]]);

        // A PUBLIC grant reaches an unrelated role.
        db.set_session_user(SUPER);
        db.execute("CREATE ROLE erin").unwrap();
        db.execute("GRANT SELECT ON t TO PUBLIC").unwrap();
        db.set_session_user("erin");
        assert_eq!(
            query(&mut db, "SELECT id FROM t").1,
            vec![vec![Value::Int(1)]]
        );
    }

    /// Create a one-row table readable by everyone, for evaluating niladic
    /// session functions (the parser requires a `FROM` clause).
    fn one_row_public(db: &mut Database) {
        db.execute("CREATE TABLE one (x INT)").unwrap();
        db.execute("INSERT INTO one VALUES (1)").unwrap();
        db.execute("GRANT SELECT ON one TO PUBLIC").unwrap();
    }

    #[test]
    fn session_functions_report_the_active_role() {
        let (_d, mut db) = db();
        one_row_public(&mut db);
        db.execute("CREATE ROLE frank LOGIN").unwrap();
        db.set_session_user("frank");
        assert_eq!(
            query(&mut db, "SELECT current_user FROM one").1,
            vec![vec![Value::Text("frank".into())]]
        );
        assert_eq!(
            query(&mut db, "SELECT session_user, current_role FROM one").1,
            vec![vec![
                Value::Text("frank".into()),
                Value::Text("frank".into())
            ]]
        );
    }

    #[test]
    fn set_role_changes_current_user_but_not_session_user() {
        let (_d, mut db) = db();
        one_row_public(&mut db);
        db.execute("CREATE ROLE grace LOGIN").unwrap();
        // A superuser session may assume any role.
        db.execute("SET ROLE grace").unwrap();
        assert_eq!(
            query(&mut db, "SELECT current_user, session_user FROM one").1,
            vec![vec![Value::Text("grace".into()), Value::Text(SUPER.into())]]
        );
        db.execute("RESET ROLE").unwrap();
        assert_eq!(
            query(&mut db, "SELECT current_user FROM one").1,
            vec![vec![Value::Text(SUPER.into())]]
        );
    }

    #[test]
    fn roles_and_grants_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("acl.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT)").unwrap();
            db.execute("INSERT INTO t VALUES (1)").unwrap();
            db.execute("CREATE ROLE heidi").unwrap();
            db.execute("GRANT SELECT ON t TO heidi").unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        // The role, its grant, and the table's ownership all reload.
        db.set_session_user("heidi");
        assert_eq!(
            query(&mut db, "SELECT id FROM t").1,
            vec![vec![Value::Int(1)]]
        );
        assert!(db.execute("INSERT INTO t VALUES (2)").is_err());
    }

    #[test]
    fn non_superuser_cannot_create_roles() {
        let (_d, mut db) = db();
        db.execute("CREATE ROLE ivan LOGIN").unwrap();
        db.set_session_user("ivan");
        assert!(matches!(
            db.execute("CREATE ROLE mallory").unwrap_err(),
            DbError::PermissionDenied(_)
        ));
    }

    // --- row-level security ---

    /// Set up a multi-tenant table with a USING policy keyed on `current_user`,
    /// RLS enabled, and SELECT granted to PUBLIC. Two tenant rows are seeded.
    fn rls_tenant_table(db: &mut Database) {
        db.execute("CREATE TABLE docs (id INT, owner TEXT, body TEXT)")
            .unwrap();
        db.execute("INSERT INTO docs VALUES (1, 'alice', 'a-doc'), (2, 'bob', 'b-doc')")
            .unwrap();
        db.execute("GRANT SELECT, INSERT, UPDATE, DELETE ON docs TO PUBLIC")
            .unwrap();
        db.execute("CREATE ROLE alice LOGIN").unwrap();
        db.execute("CREATE ROLE bob LOGIN").unwrap();
        db.execute("CREATE POLICY tenant ON docs USING ((owner = current_user()))")
            .unwrap();
        db.execute("ALTER TABLE docs ENABLE ROW LEVEL SECURITY")
            .unwrap();
    }

    #[test]
    fn rls_using_policy_filters_rows_by_role() {
        let (_d, mut db) = db();
        rls_tenant_table(&mut db);

        db.set_session_user("alice");
        let (_c, rows) = query(&mut db, "SELECT id FROM docs");
        assert_eq!(rows, vec![vec![Value::Int(1)]], "alice sees only her row");

        db.set_session_user("bob");
        let (_c, rows) = query(&mut db, "SELECT id FROM docs");
        assert_eq!(rows, vec![vec![Value::Int(2)]], "bob sees only his row");

        // A superuser bypasses RLS and sees everything.
        db.set_session_user(SUPER);
        let (_c, rows) = query(&mut db, "SELECT id FROM docs ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
    }

    #[test]
    fn rls_isolates_similarity_search_by_tenant() {
        // The core isolation claim of the AI memory layer: a tenant's
        // nearest-neighbor search can only ever rank that tenant's own vectors,
        // enforced by the engine rather than by application code.
        let (_d, mut db) = db();
        db.execute("CREATE TABLE memories (id INT, tenant TEXT, e VECTOR(2))")
            .unwrap();
        // bob's vector [0,0] is globally nearest to the query [0,0], but alice
        // must never see it: her search ranks only her own rows.
        db.execute(
            "INSERT INTO memories VALUES \
             (1, 'alice', '[1, 0]'), (2, 'bob', '[0, 0]'), (3, 'alice', '[5, 5]')",
        )
        .unwrap();
        db.execute("GRANT SELECT ON memories TO PUBLIC").unwrap();
        db.execute("CREATE ROLE alice LOGIN").unwrap();
        db.execute("CREATE ROLE bob LOGIN").unwrap();
        db.execute("CREATE POLICY tenant ON memories USING ((tenant = current_user()))")
            .unwrap();
        db.execute("ALTER TABLE memories ENABLE ROW LEVEL SECURITY")
            .unwrap();

        // alice's nearest-neighbor search returns her own closest row (id 1),
        // never bob's id 2 even though it is globally nearest to the query.
        db.set_session_user("alice");
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM memories ORDER BY e <-> '[0, 0]' LIMIT 1",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int(1)]],
            "alice's KNN must not reach bob's globally-nearest vector"
        );
        // Her full ranking is exactly her two rows, nearest first.
        let (_c, rows) = query(&mut db, "SELECT id FROM memories ORDER BY e <-> '[0, 0]'");
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);

        // bob's search sees only his own row, even asking for more than exist.
        db.set_session_user("bob");
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM memories ORDER BY e <-> '[0, 0]' LIMIT 5",
        );
        assert_eq!(rows, vec![vec![Value::Int(2)]]);

        // A superuser bypasses RLS and ranks every tenant's vectors together.
        db.set_session_user(SUPER);
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM memories ORDER BY e <-> '[0, 0]' LIMIT 1",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Int(2)]],
            "the unfiltered nearest is bob's"
        );
    }

    #[test]
    fn rls_restricts_update_and_delete_to_visible_rows() {
        let (_d, mut db) = db();
        rls_tenant_table(&mut db);

        db.set_session_user("alice");
        // alice can update only her own row; bob's row is invisible, so the
        // UPDATE affects nothing.
        let out = db
            .execute("UPDATE docs SET body = 'edited' WHERE id = 2")
            .unwrap();
        assert!(matches!(out, QueryOutcome::Mutation { affected: 0 }));
        let out = db
            .execute("UPDATE docs SET body = 'edited' WHERE id = 1")
            .unwrap();
        assert!(matches!(out, QueryOutcome::Mutation { affected: 1 }));
        // Likewise DELETE only reaches her row.
        let out = db.execute("DELETE FROM docs WHERE id = 2").unwrap();
        assert!(matches!(out, QueryOutcome::Mutation { affected: 0 }));
    }

    #[test]
    fn rls_with_check_blocks_inserting_other_tenants_rows() {
        let (_d, mut db) = db();
        rls_tenant_table(&mut db);

        db.set_session_user("alice");
        // The USING predicate doubles as the WITH CHECK: alice may insert her own
        // row but not one owned by bob.
        db.execute("INSERT INTO docs VALUES (3, 'alice', 'mine')")
            .unwrap();
        assert!(matches!(
            db.execute("INSERT INTO docs VALUES (4, 'bob', 'spoof')")
                .unwrap_err(),
            DbError::PermissionDenied(_)
        ));
    }

    #[test]
    fn rls_default_denies_when_no_policy_matches() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT)").unwrap();
        db.execute("INSERT INTO t VALUES (1), (2)").unwrap();
        db.execute("GRANT SELECT ON t TO PUBLIC").unwrap();
        db.execute("CREATE ROLE carol LOGIN").unwrap();
        // RLS enabled but no policy: a non-owner sees nothing.
        db.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY")
            .unwrap();
        db.set_session_user("carol");
        assert!(query(&mut db, "SELECT id FROM t").1.is_empty());
    }

    #[test]
    fn rls_force_applies_to_the_owner_too() {
        let (_d, mut db) = db();
        db.execute("CREATE ROLE dan LOGIN").unwrap();
        db.set_session_user("dan");
        // dan owns the table.
        db.execute("CREATE TABLE t (id INT, owner TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'dan'), (2, 'eve')")
            .unwrap();
        db.execute("CREATE POLICY p ON t USING ((owner = current_user()))")
            .unwrap();
        db.execute("ALTER TABLE t ENABLE ROW LEVEL SECURITY")
            .unwrap();
        // Without FORCE, the owner bypasses the policy and sees both rows.
        assert_eq!(query(&mut db, "SELECT id FROM t ORDER BY id").1.len(), 2);
        // FORCE makes the policy apply to the owner as well.
        db.execute("ALTER TABLE t FORCE ROW LEVEL SECURITY")
            .unwrap();
        assert_eq!(
            query(&mut db, "SELECT id FROM t").1,
            vec![vec![Value::Int(1)]]
        );
    }

    #[test]
    fn rls_policies_survive_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rls.db");
        {
            let mut db = Database::open(&path).expect("open");
            rls_tenant_table(&mut db);
        }
        let mut db = Database::open(&path).expect("reopen");
        db.set_session_user("alice");
        // The policy and the enabled flag both reload.
        assert_eq!(
            query(&mut db, "SELECT id FROM docs").1,
            vec![vec![Value::Int(1)]]
        );
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

    #[test]
    fn range_predicate_uses_the_index_and_returns_all_rows() {
        let (_dir, mut db) = db();
        seed_indexed(&mut db, 300);
        // A range on the indexed key is sargable: the planner picks the index.
        let plan = explain(&mut db, "SELECT id FROM t WHERE id >= 10 AND id < 15");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        // The range scan returns every qualifying row, sorted for comparison.
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE id >= 10 AND id < 15 ORDER BY id",
        );
        assert_eq!(
            rows,
            (10..15).map(|i| vec![Value::Int(i)]).collect::<Vec<_>>()
        );
        // BETWEEN desugars to `>= AND <=`, so it also drives the index.
        let (_c, between) = query(
            &mut db,
            "SELECT id FROM t WHERE id BETWEEN 297 AND 305 ORDER BY id",
        );
        assert_eq!(
            between,
            (297..300).map(|i| vec![Value::Int(i)]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn range_index_does_not_double_count_updated_rows() {
        let (_dir, mut db) = db();
        seed_indexed(&mut db, 20);
        // Move a row up and back down: its old keys linger in the upsert-only
        // index, so the row sits under several keys in the scanned range.
        db.execute("UPDATE t SET id = 100 WHERE id = 5").unwrap();
        db.execute("UPDATE t SET id = 5 WHERE id = 100").unwrap();
        // A wide range covers all of those stale keys; the row must appear once.
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE id >= 0 AND id <= 100 ORDER BY id",
        );
        assert_eq!(rows.len(), 20, "each row exactly once, got {rows:?}");
        let fives = rows.iter().filter(|r| r == &&vec![Value::Int(5)]).count();
        assert_eq!(fives, 1, "the moved row should not be double-counted");
    }

    #[test]
    fn date_column_is_indexed_for_equality_and_range() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE e (d DATE PRIMARY KEY, label TEXT)")
            .unwrap();
        for (d, l) in [
            ("2024-01-01", "a"),
            ("2024-02-01", "b"),
            ("2024-03-01", "c"),
            ("2024-04-01", "d"),
        ] {
            db.execute(&format!("INSERT INTO e VALUES (DATE '{d}', '{l}')"))
                .unwrap();
        }
        // A DATE key is i64-backed, so it is indexable: equality is a point get.
        assert!(
            explain(&mut db, "SELECT label FROM e WHERE d = DATE '2024-02-01'")
                .contains("IndexScan")
        );
        let (_c, one) = query(&mut db, "SELECT label FROM e WHERE d = DATE '2024-02-01'");
        assert_eq!(one, vec![vec![Value::Text("b".into())]]);
        // And a date range scans in chronological order via the index.
        let plan = explain(
            &mut db,
            "SELECT label FROM e WHERE d > DATE '2024-01-15' AND d < DATE '2024-03-15'",
        );
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        let (_c, rows) = query(
            &mut db,
            "SELECT label FROM e WHERE d > DATE '2024-01-15' AND d < DATE '2024-03-15' ORDER BY d",
        );
        assert_eq!(
            rows,
            vec![vec![Value::Text("b".into())], vec![Value::Text("c".into())],]
        );
    }

    // --- variable-key (CREATE INDEX) secondary indexes ---

    #[test]
    fn create_index_on_text_column_is_used_and_correct() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE u (id INT, email TEXT, status TEXT)")
            .unwrap();
        for i in 0..200 {
            let st = if i % 3 == 0 { "active" } else { "inactive" };
            db.execute(&format!(
                "INSERT INTO u VALUES ({i}, 'user{i}@x.com', '{st}')"
            ))
            .unwrap();
        }
        db.execute("CREATE INDEX u_email ON u (email)").unwrap();
        db.execute("CREATE INDEX u_status ON u (status)").unwrap();

        // A TEXT equality on the indexed column uses the index.
        let plan = explain(&mut db, "SELECT id FROM u WHERE email = 'user42@x.com'");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        let (_c, rows) = query(&mut db, "SELECT id FROM u WHERE email = 'user42@x.com'");
        assert_eq!(rows, vec![vec![Value::Int(42)]]);

        // A non-unique TEXT column: the index returns every matching row.
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM u WHERE status = 'active' ORDER BY id",
        );
        assert_eq!(rows.len(), (0..200).filter(|i| i % 3 == 0).count());
        assert_eq!(rows[0], vec![Value::Int(0)]);

        // A miss returns nothing.
        assert!(
            query(&mut db, "SELECT id FROM u WHERE email = 'nobody@x.com'")
                .1
                .is_empty()
        );
    }

    #[test]
    fn create_index_text_reflects_updates_and_deletes() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'alice'), (2, 'bob'), (3, 'carol')")
            .unwrap();
        db.execute("CREATE INDEX t_name ON t (name)").unwrap();
        // Move a row to a new value; the old key is stale and filtered.
        db.execute("UPDATE t SET name = 'alice2' WHERE id = 1")
            .unwrap();
        assert!(query(&mut db, "SELECT id FROM t WHERE name = 'alice'")
            .1
            .is_empty());
        assert_eq!(
            query(&mut db, "SELECT id FROM t WHERE name = 'alice2'").1,
            vec![vec![Value::Int(1)]]
        );
        // Delete a row; the index entry resolves to no live row.
        db.execute("DELETE FROM t WHERE name = 'bob'").unwrap();
        assert!(query(&mut db, "SELECT id FROM t WHERE name = 'bob'")
            .1
            .is_empty());
    }

    #[test]
    fn create_index_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("midx.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, tag TEXT)").unwrap();
            for i in 0..100 {
                db.execute(&format!("INSERT INTO t VALUES ({i}, 'tag{}')", i % 5))
                    .unwrap();
            }
            db.execute("CREATE INDEX t_tag ON t (tag)").unwrap();
        }
        let mut db = Database::open(&path).expect("reopen");
        // The physical index reloads and is still chosen.
        let plan = explain(&mut db, "SELECT id FROM t WHERE tag = 'tag3'");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        let (_c, rows) = query(&mut db, "SELECT id FROM t WHERE tag = 'tag3' ORDER BY id");
        assert_eq!(
            rows,
            (0..100)
                .filter(|i| i % 5 == 3)
                .map(|i| vec![Value::Int(i)])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn create_index_text_rows_survive_vacuum() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        for i in 0..60 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, 'n{}')", i % 4))
                .unwrap();
        }
        db.execute("CREATE INDEX t_name ON t (name)").unwrap();
        // Churn to create dead versions, then compact.
        db.execute("UPDATE t SET name = 'n0' WHERE id < 10")
            .unwrap();
        db.execute("VACUUM t").unwrap();
        // After vacuum the index is rebuilt with the new rowids and still right.
        let (_c, rows) = query(&mut db, "SELECT id FROM t WHERE name = 'n2' ORDER BY id");
        assert_eq!(
            rows,
            (0..60)
                .filter(|i| i % 4 == 2 && *i >= 10)
                .map(|i| vec![Value::Int(i)])
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn composite_index_serves_leading_and_full_equality() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE t (tenant INT, status TEXT, id INT)")
            .unwrap();
        for tenant in 0..5 {
            for i in 0..10 {
                let st = if i % 2 == 0 { "open" } else { "closed" };
                db.execute(&format!("INSERT INTO t VALUES ({tenant}, '{st}', {i})"))
                    .unwrap();
            }
        }
        db.execute("CREATE INDEX t_ts ON t (tenant, status)")
            .unwrap();
        // Leading-column equality uses the composite index.
        let plan = explain(&mut db, "SELECT id FROM t WHERE tenant = 2");
        assert!(plan.contains("IndexScan"), "plan was:\n{plan}");
        let (_c, rows) = query(&mut db, "SELECT id FROM t WHERE tenant = 2 ORDER BY id");
        assert_eq!(rows.len(), 10);
        // Full-tuple equality returns the right rows (residual filter narrows the
        // leading-prefix candidates by the second column).
        let (_c, rows) = query(
            &mut db,
            "SELECT id FROM t WHERE tenant = 3 AND status = 'open' ORDER BY id",
        );
        assert_eq!(rows, [0, 2, 4, 6, 8].map(|i| vec![Value::Int(i)]).to_vec());
    }

    #[test]
    fn unique_index_rejects_duplicates_on_insert_and_update() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE u (id INT, email TEXT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 'a@x.com'), (2, 'b@x.com')")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX u_email ON u (email)")
            .unwrap();
        // A duplicate insert is refused.
        assert!(matches!(
            db.execute("INSERT INTO u VALUES (3, 'a@x.com')")
                .unwrap_err(),
            DbError::Constraint(_)
        ));
        // A distinct value is fine.
        db.execute("INSERT INTO u VALUES (3, 'c@x.com')").unwrap();
        // An update into an existing value is refused; updating the row to its own
        // value is fine.
        assert!(matches!(
            db.execute("UPDATE u SET email = 'b@x.com' WHERE id = 1")
                .unwrap_err(),
            DbError::Constraint(_)
        ));
        db.execute("UPDATE u SET email = 'a@x.com' WHERE id = 1")
            .unwrap();
        // Two duplicate rows in one multi-row INSERT are caught.
        assert!(matches!(
            db.execute("INSERT INTO u VALUES (4, 'd@x.com'), (5, 'd@x.com')")
                .unwrap_err(),
            DbError::Constraint(_)
        ));
    }

    #[test]
    fn unique_index_refuses_to_build_over_duplicate_data() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE u (id INT, email TEXT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 'dup@x.com'), (2, 'dup@x.com')")
            .unwrap();
        assert!(matches!(
            db.execute("CREATE UNIQUE INDEX u_email ON u (email)")
                .unwrap_err(),
            DbError::Constraint(_)
        ));
    }

    #[test]
    fn composite_unique_allows_distinct_tuples() {
        let (_dir, mut db) = db();
        db.execute("CREATE TABLE m (tenant INT, slug TEXT)")
            .unwrap();
        db.execute("CREATE UNIQUE INDEX m_u ON m (tenant, slug)")
            .unwrap();
        db.execute("INSERT INTO m VALUES (1, 'a'), (1, 'b'), (2, 'a')")
            .unwrap();
        // Same (tenant, slug) tuple is rejected.
        assert!(matches!(
            db.execute("INSERT INTO m VALUES (1, 'a')").unwrap_err(),
            DbError::Constraint(_)
        ));
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
            db.execute("CREATE TABLE mirror AS SELECT id FROM src")
                .unwrap();
            // A second CREATE of the same name is rejected.
            assert!(db
                .execute("CREATE TABLE mirror AS SELECT id FROM src")
                .is_err());
        }
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id FROM mirror ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(7)], vec![Value::Int(8)]]);
    }

    // --- INSERT ... SELECT ---

    #[test]
    fn insert_select_copies_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE src (id INT, n INT)").unwrap();
        db.execute("CREATE TABLE dst (id INT, n INT)").unwrap();
        db.execute("INSERT INTO src VALUES (1, 10), (2, 20), (3, 30)")
            .unwrap();
        let out = db
            .execute("INSERT INTO dst SELECT id, n FROM src WHERE n >= 20")
            .unwrap();
        assert_eq!(out, QueryOutcome::Mutation { affected: 2 });
        let (_c, rows) = query(&mut db, "SELECT id, n FROM dst ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(2), Value::Int(20)],
                vec![Value::Int(3), Value::Int(30)],
            ]
        );
    }

    #[test]
    fn insert_select_respects_named_columns_and_constraints() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE src (a INT, b INT)").unwrap();
        db.execute("CREATE TABLE dst (id INT PRIMARY KEY, n INT)")
            .unwrap();
        db.execute("INSERT INTO src VALUES (1, 100), (1, 200)")
            .unwrap();
        // Two rows map a=1 onto the primary key: the duplicate is rejected and
        // nothing is written.
        assert!(db
            .execute("INSERT INTO dst (id, n) SELECT a, b FROM src")
            .is_err());
        assert_eq!(query(&mut db, "SELECT id FROM dst").1.len(), 0);
        // Distinct keys (a + b) go in fine, into the named columns.
        db.execute("INSERT INTO dst (id, n) SELECT a + b, b FROM src")
            .unwrap();
        let (_c, rows) = query(&mut db, "SELECT id FROM dst ORDER BY id");
        assert_eq!(rows, vec![vec![Value::Int(101)], vec![Value::Int(201)]]);
    }

    // --- COPY (CSV import/export) ---

    #[test]
    fn copy_to_then_from_round_trips() {
        let csv_dir = tempfile::tempdir().expect("csv dir");
        let csv = csv_dir.path().join("t.csv");
        let csv_path = csv.to_str().unwrap().replace('\\', "/");
        let (_d, mut db) = db();
        db.execute("CREATE TABLE src (id INT, name TEXT, active BOOL)")
            .unwrap();
        db.execute(
            "INSERT INTO src VALUES (1, 'alice', TRUE), (2, 'bob, jr', FALSE), (3, NULL, TRUE)",
        )
        .unwrap();
        // Export with a header row.
        assert_eq!(
            db.execute(&format!("COPY src TO '{csv_path}' HEADER"))
                .unwrap(),
            QueryOutcome::Mutation { affected: 3 }
        );
        // The file is real CSV: header, a quoted field with a comma, an empty
        // field for NULL.
        let text = std::fs::read_to_string(&csv).unwrap();
        assert!(text.starts_with("id,name,active\n"), "got:\n{text}");
        assert!(text.contains("\"bob, jr\""), "comma field quoted:\n{text}");

        // Import into a fresh table of the same shape; values round-trip.
        db.execute("CREATE TABLE dst (id INT, name TEXT, active BOOL)")
            .unwrap();
        assert_eq!(
            db.execute(&format!("COPY dst FROM '{csv_path}' HEADER"))
                .unwrap(),
            QueryOutcome::Mutation { affected: 3 }
        );
        let (_c, rows) = query(&mut db, "SELECT id, name, active FROM dst ORDER BY id");
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Int(1),
                    Value::Text("alice".into()),
                    Value::Bool(true)
                ],
                vec![
                    Value::Int(2),
                    Value::Text("bob, jr".into()),
                    Value::Bool(false)
                ],
                vec![Value::Int(3), Value::Null, Value::Bool(true)],
            ]
        );
    }

    #[test]
    fn copy_from_enforces_arity_and_constraints() {
        let csv_dir = tempfile::tempdir().expect("csv dir");
        let csv = csv_dir.path().join("bad.csv");
        std::fs::write(&csv, "1,2,3\n").unwrap();
        let csv_path = csv.to_str().unwrap().replace('\\', "/");
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a INT, b INT)").unwrap();
        // Three fields into a two-column table is an arity error.
        let err = db
            .execute(&format!("COPY t FROM '{csv_path}'"))
            .unwrap_err();
        assert!(matches!(err, DbError::ValueCount { .. }), "got {err:?}");
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

    #[test]
    fn alter_drop_column_rewrites_rows() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT, note TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a', 'x'), (2, 'b', 'y')")
            .unwrap();
        db.execute("ALTER TABLE t DROP COLUMN name").unwrap();
        let (cols, rows) = query(&mut db, "SELECT id, note FROM t ORDER BY id");
        assert_eq!(names(&cols), ["id", "note"]);
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("x".into())],
                vec![Value::Int(2), Value::Text("y".into())],
            ]
        );
        // The dropped column is gone from the schema.
        assert!(db.execute("SELECT name FROM t").is_err());
        // DROP COLUMN IF EXISTS on the now-absent column is a no-op.
        assert!(matches!(
            db.execute("ALTER TABLE t DROP COLUMN IF EXISTS name")
                .unwrap(),
            QueryOutcome::Ddl
        ));
        // Dropping a missing column without IF EXISTS errors.
        assert!(db.execute("ALTER TABLE t DROP COLUMN ghost").is_err());
    }

    #[test]
    fn alter_drop_column_refused_when_constrained() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, n INT CHECK (n > 0))")
            .unwrap();
        // The column n is named by a CHECK; dropping it is refused.
        assert!(db.execute("ALTER TABLE t DROP COLUMN n").is_err());
        // The only-column rule.
        db.execute("CREATE TABLE one (a INT)").unwrap();
        assert!(db.execute("ALTER TABLE one DROP COLUMN a").is_err());
    }

    #[test]
    fn alter_rename_column_keeps_data() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        db.execute("ALTER TABLE t RENAME COLUMN name TO label")
            .unwrap();
        let (cols, rows) = query(&mut db, "SELECT id, label FROM t");
        assert_eq!(names(&cols), ["id", "label"]);
        assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("a".into())]]);
        // The old name no longer resolves.
        assert!(db.execute("SELECT name FROM t").is_err());
    }

    #[test]
    fn alter_rename_table_moves_everything() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rt.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE t (id INT, name TEXT)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
            db.execute("ALTER TABLE t RENAME TO users").unwrap();
            // Old name gone, new name works.
            assert!(db.execute("SELECT id FROM t").is_err());
            let (_c, rows) = query(&mut db, "SELECT id, name FROM users");
            assert_eq!(rows, vec![vec![Value::Int(1), Value::Text("a".into())]]);
        }
        // The rename survives a reopen.
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT id FROM users");
        assert_eq!(rows, vec![vec![Value::Int(1)]]);
    }

    #[test]
    fn dump_reproduces_the_database() {
        // Bind the fresh restore target up front, before `db` is shadowed by the
        // source database below (a second `db()` would otherwise resolve to the
        // local variable, not the helper).
        let (_d2, mut restored) = db();
        let (_d, mut db) = db();
        // A schema exercising serial, defaults, constraints, a foreign key, an
        // explicit index, and a view.
        db.execute("CREATE TABLE parent (id INT PRIMARY KEY, name TEXT NOT NULL)")
            .unwrap();
        db.execute(
            "CREATE TABLE child (id SERIAL PRIMARY KEY, pid INT REFERENCES parent (id), \
             qty INT DEFAULT 1 CHECK (qty > 0))",
        )
        .unwrap();
        db.execute("CREATE INDEX child_qty_ix ON child (qty)")
            .unwrap();
        db.execute("CREATE VIEW big AS SELECT id FROM child WHERE qty > 5")
            .unwrap();
        db.execute("INSERT INTO parent VALUES (1, 'a'), (2, 'b')")
            .unwrap();
        db.execute("INSERT INTO child (pid, qty) VALUES (1, 3), (2, 9)")
            .unwrap();

        let script = db.dump().unwrap();
        // child must be created and loaded after parent (foreign-key safe).
        let create_parent = script.find("CREATE TABLE parent").unwrap();
        let create_child = script.find("CREATE TABLE child").unwrap();
        assert!(create_parent < create_child, "parent DDL before child DDL");

        // Restoring the script into the fresh database reproduces every row.
        for stmt in script.split(";\n").map(str::trim).filter(|s| !s.is_empty()) {
            restored.execute(stmt).unwrap();
        }
        let (_c, parents) = query(&mut restored, "SELECT id, name FROM parent ORDER BY id");
        assert_eq!(
            parents,
            vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Int(2), Value::Text("b".into())],
            ]
        );
        let (_c, kids) = query(&mut restored, "SELECT id, pid, qty FROM child ORDER BY id");
        assert_eq!(
            kids,
            vec![
                vec![Value::Int(1), Value::Int(1), Value::Int(3)],
                vec![Value::Int(2), Value::Int(2), Value::Int(9)],
            ]
        );
        // The view and the CHECK constraint came across too.
        let (_c, v) = query(&mut restored, "SELECT id FROM big");
        assert_eq!(v, vec![vec![Value::Int(2)]]);
        assert!(restored
            .execute("INSERT INTO child (pid, qty) VALUES (1, 0)")
            .is_err());
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
    fn fk_on_delete_cascade_and_set_null() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("casc.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
            db.execute(
                "CREATE TABLE child_c (id INT, pid INT REFERENCES p (id) ON DELETE CASCADE)",
            )
            .unwrap();
            db.execute(
                "CREATE TABLE child_n (id INT, pid INT REFERENCES p (id) ON DELETE SET NULL)",
            )
            .unwrap();
            db.execute("INSERT INTO p VALUES (1), (2)").unwrap();
            db.execute("INSERT INTO child_c VALUES (10, 1), (11, 2)")
                .unwrap();
            db.execute("INSERT INTO child_n VALUES (20, 1), (21, 2)")
                .unwrap();

            // Deleting parent 1 cascades to child_c (row gone) and sets child_n's
            // pid to NULL.
            db.execute("DELETE FROM p WHERE id = 1").unwrap();
            let (_c, c_rows) = query(&mut db, "SELECT id FROM child_c ORDER BY id");
            assert_eq!(c_rows, vec![vec![Value::Int(11)]]); // 10 cascaded away
            let (_c, n_rows) = query(&mut db, "SELECT id, pid FROM child_n ORDER BY id");
            assert_eq!(
                n_rows,
                vec![
                    vec![Value::Int(20), Value::Null], // pid nulled
                    vec![Value::Int(21), Value::Int(2)],
                ]
            );
        }
        // The actions survive a reopen (persisted in the .cons sidecar).
        let mut db = Database::open(&path).expect("reopen");
        db.execute("DELETE FROM p WHERE id = 2").unwrap();
        let (_c, c_rows) = query(&mut db, "SELECT id FROM child_c");
        assert_eq!(c_rows, Vec::<Vec<Value>>::new()); // 11 cascaded away too
    }

    #[test]
    fn fk_on_update_cascade() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        db.execute("CREATE TABLE c (id INT, pid INT REFERENCES p (id) ON UPDATE CASCADE)")
            .unwrap();
        db.execute("INSERT INTO p VALUES (1)").unwrap();
        db.execute("INSERT INTO c VALUES (10, 1)").unwrap();
        // Changing the parent key cascades the new value to the child.
        db.execute("UPDATE p SET id = 99 WHERE id = 1").unwrap();
        let (_c, rows) = query(&mut db, "SELECT pid FROM c");
        assert_eq!(rows, vec![vec![Value::Int(99)]]);
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

    #[test]
    fn using_and_natural_joins() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE a (id INT, x TEXT)").unwrap();
        db.execute("CREATE TABLE b (id INT, y TEXT)").unwrap();
        db.execute("INSERT INTO a VALUES (1, 'a1'), (2, 'a2')")
            .unwrap();
        db.execute("INSERT INTO b VALUES (1, 'b1'), (3, 'b3')")
            .unwrap();
        // USING (id) equates a.id and b.id.
        let (_c, u) = query(
            &mut db,
            "SELECT a.x, b.y FROM a JOIN b USING (id) ORDER BY a.id",
        );
        assert_eq!(
            u,
            vec![vec![Value::Text("a1".into()), Value::Text("b1".into())]]
        );
        // NATURAL JOIN finds the common column (id) automatically.
        let (_c, n) = query(
            &mut db,
            "SELECT a.x, b.y FROM a NATURAL JOIN b ORDER BY a.id",
        );
        assert_eq!(
            n,
            vec![vec![Value::Text("a1".into()), Value::Text("b1".into())]]
        );
        // USING on a LEFT join keeps the unmatched left row.
        let (_c, l) = query(
            &mut db,
            "SELECT a.id, b.y FROM a LEFT JOIN b USING (id) ORDER BY a.id",
        );
        assert_eq!(
            l,
            vec![
                vec![Value::Int(1), Value::Text("b1".into())],
                vec![Value::Int(2), Value::Null],
            ]
        );
    }

    #[test]
    fn order_by_nulls_first_and_last() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a INT)").unwrap();
        db.execute("INSERT INTO t VALUES (2), (NULL), (1)").unwrap();
        // Default ASC: NULLs last.
        let (_c, def) = query(&mut db, "SELECT a FROM t ORDER BY a");
        assert_eq!(
            def,
            vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Null]]
        );
        // NULLS FIRST overrides the default.
        let (_c, first) = query(&mut db, "SELECT a FROM t ORDER BY a NULLS FIRST");
        assert_eq!(
            first,
            vec![vec![Value::Null], vec![Value::Int(1)], vec![Value::Int(2)]]
        );
        // DESC defaults to NULLs first; NULLS LAST overrides it.
        let (_c, desc_last) = query(&mut db, "SELECT a FROM t ORDER BY a DESC NULLS LAST");
        assert_eq!(
            desc_last,
            vec![vec![Value::Int(2)], vec![Value::Int(1)], vec![Value::Null]]
        );
    }

    #[test]
    fn right_and_full_outer_joins() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE emp (id INT, dept INT, name TEXT)")
            .unwrap();
        db.execute("CREATE TABLE dept (id INT, label TEXT)")
            .unwrap();
        // alice -> 1, bob -> 2, carol -> NULL dept; dept 1 has staff, dept 3 has none.
        db.execute("INSERT INTO emp VALUES (1, 1, 'alice'), (2, 2, 'bob'), (3, NULL, 'carol')")
            .unwrap();
        db.execute("INSERT INTO dept VALUES (1, 'eng'), (3, 'ops')")
            .unwrap();

        // RIGHT JOIN keeps every dept, including 'ops' with no employees.
        let (_c, right) = query(
            &mut db,
            "SELECT e.name, d.label FROM emp e RIGHT JOIN dept d ON e.dept = d.id ORDER BY d.label",
        );
        assert_eq!(
            right,
            vec![
                vec![Value::Text("alice".into()), Value::Text("eng".into())],
                vec![Value::Null, Value::Text("ops".into())], // dept 3, no employee
            ]
        );

        // FULL JOIN keeps unmatched rows from both sides: bob and carol (no
        // matching dept) and 'ops' (no employee).
        let (_c, full) = query(
            &mut db,
            "SELECT e.name, d.label FROM emp e FULL OUTER JOIN dept d ON e.dept = d.id \
             ORDER BY e.name, d.label",
        );
        // NULL names sort last (NULLS LAST), so the unmatched 'ops' row trails.
        assert_eq!(
            full,
            vec![
                vec![Value::Text("alice".into()), Value::Text("eng".into())],
                vec![Value::Text("bob".into()), Value::Null],
                vec![Value::Text("carol".into()), Value::Null],
                vec![Value::Null, Value::Text("ops".into())],
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

    #[test]
    fn more_string_and_numeric_functions() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (s TEXT, n INT)").unwrap();
        db.execute("INSERT INTO t VALUES ('hello world', -4)")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT RIGHT(s, 5), REVERSE(s), REPEAT('ab', 3), INITCAP(s), STRPOS(s, 'world'), \
             SIGN(n), TRUNC(3.78), TRUNC(3.789, 1) FROM t",
        );
        assert_eq!(
            rows[0],
            vec![
                Value::Text("world".into()),
                Value::Text("dlrow olleh".into()),
                Value::Text("ababab".into()),
                Value::Text("Hello World".into()),
                Value::Int(7),
                Value::Int(-1),
                Value::Float(3.0),
                Value::Float(3.7),
            ]
        );
    }

    #[test]
    fn date_and_timestamp_columns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dt.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE events (id INT, on_day DATE, at_time TIMESTAMP)")
                .unwrap();
            // A typed literal, and a bare string coerced into the date column.
            db.execute(
                "INSERT INTO events VALUES \
                 (1, DATE '2024-01-15', TIMESTAMP '2024-01-15 09:30:00'), \
                 (2, '2023-12-31', '2023-12-31 23:59:59')",
            )
            .unwrap();
            // Dates compare and order as dates, not strings.
            let (cols, rows) = query(
                &mut db,
                "SELECT id, on_day FROM events WHERE on_day < DATE '2024-01-01' ORDER BY on_day",
            );
            assert_eq!(names(&cols), ["id", "on_day"]);
            assert_eq!(
                rows,
                vec![vec![Value::Int(2), Value::Date(parse_day("2023-12-31"))]]
            );
        }
        // Values survive a reopen (codec + sidecar round-trip).
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT at_time FROM events WHERE id = 1");
        assert_eq!(
            rows,
            vec![vec![Value::Timestamp(parse_micros("2024-01-15 09:30:00"))]]
        );
        // An unparseable date is rejected.
        assert!(db
            .execute("INSERT INTO events VALUES (3, 'not-a-date', NULL)")
            .is_err());
    }

    #[test]
    fn date_part_trunc_and_extract() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (ts TIMESTAMP)").unwrap();
        db.execute("INSERT INTO t VALUES (TIMESTAMP '2024-03-15 09:30:45')")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT EXTRACT(year FROM ts), EXTRACT(month FROM ts), DATE_PART('day', ts), \
             DATE_PART('hour', ts), DATE_PART('minute', ts) FROM t",
        );
        assert_eq!(
            rows[0],
            vec![
                Value::Int(2024),
                Value::Int(3),
                Value::Int(15),
                Value::Int(9),
                Value::Int(30),
            ]
        );
        // DATE_TRUNC floors to the start of the field, as a timestamp.
        let (_c, t) = query(&mut db, "SELECT DATE_TRUNC('month', ts) FROM t");
        assert_eq!(
            t,
            vec![vec![Value::Timestamp(parse_micros("2024-03-01 00:00:00"))]]
        );
        // EXTRACT desugars to DATE_PART, so a query can GROUP BY it.
        db.execute("INSERT INTO t VALUES (TIMESTAMP '2024-03-20 12:00:00')")
            .unwrap();
        let (_c, g) = query(
            &mut db,
            "SELECT EXTRACT(month FROM ts), COUNT(*) FROM t GROUP BY EXTRACT(month FROM ts)",
        );
        assert_eq!(g, vec![vec![Value::Int(3), Value::Int(2)]]);
    }

    #[test]
    fn cast_converts_between_types() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (n INT, x FLOAT, s TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (5, 2.7, '42')").unwrap();
        let (cols, rows) = query(
            &mut db,
            "SELECT CAST(x AS INT), CAST(n AS TEXT), CAST(s AS INT), CAST(n AS BOOL), \
             '2024-01-15'::date, CAST(x AS FLOAT) FROM t",
        );
        assert_eq!(cols.len(), 6);
        assert_eq!(
            rows[0],
            vec![
                Value::Int(3), // 2.7 rounds to 3
                Value::Text("5".into()),
                Value::Int(42),
                Value::Bool(true),
                Value::Date(parse_day("2024-01-15")),
                Value::Float(2.7),
            ]
        );
        // A cast can drive a WHERE predicate.
        let (_c, hit) = query(&mut db, "SELECT n FROM t WHERE CAST(s AS INT) > 40");
        assert_eq!(hit, vec![vec![Value::Int(5)]]);
        // An unparseable text cast errors.
        assert!(db.execute("SELECT CAST('nope' AS INT) FROM t").is_err());
        // A constant cast folds in INSERT VALUES.
        db.execute("INSERT INTO t (n) VALUES (CAST('7' AS INT))")
            .unwrap();
        let (_c, n) = query(&mut db, "SELECT n FROM t WHERE x IS NULL");
        assert_eq!(n, vec![vec![Value::Int(7)]]);
    }

    #[test]
    fn json_column_and_access_operators() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("j.db");
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE docs (id INT, body JSON)").unwrap();
            db.execute(
                r#"INSERT INTO docs VALUES (1, '{"name": "ada", "tags": ["x", "y"], "age": 30}')"#,
            )
            .unwrap();
            // -> returns JSON, ->> returns text; nested and indexed access.
            let (cols, rows) = query(
                &mut db,
                "SELECT body ->> 'name', body -> 'tags' ->> 1, body ->> 'age', body -> 'missing' \
                 FROM docs",
            );
            assert_eq!(cols.len(), 4);
            assert_eq!(
                rows[0],
                vec![
                    Value::Text("ada".into()),
                    Value::Text("y".into()),
                    Value::Text("30".into()),
                    Value::Null,
                ]
            );
            // A ->> result drives a WHERE predicate.
            let (_c, hit) = query(&mut db, "SELECT id FROM docs WHERE body ->> 'name' = 'ada'");
            assert_eq!(hit, vec![vec![Value::Int(1)]]);
            // Invalid JSON is rejected on insert.
            assert!(db
                .execute("INSERT INTO docs VALUES (2, '{not json}')")
                .is_err());
        }
        // The document survives a reopen and a `::json` cast works.
        let mut db = Database::open(&path).expect("reopen");
        let (_c, n) = query(
            &mut db,
            "SELECT body -> 'tags' ->> 0 FROM docs WHERE id = 1",
        );
        assert_eq!(n, vec![vec![Value::Text("x".into())]]);
        let (_c, c) = query(&mut db, "SELECT ('[1,2,3]'::json) ->> 2 FROM docs");
        assert_eq!(c, vec![vec![Value::Text("3".into())]]);
    }

    #[test]
    fn decimal_exact_arithmetic_and_storage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.db");
        let dec = |s: &str| {
            let (m, sc) = picklejar_sql::decimal::parse(s).expect("dec");
            Value::Decimal(m, sc)
        };
        {
            let mut db = Database::open(&path).expect("open");
            db.execute("CREATE TABLE money (id INT, amount DECIMAL(10, 2))")
                .unwrap();
            // A bare numeric coerces; a string is exact.
            db.execute("INSERT INTO money VALUES (1, 0.1), (2, '0.2'), (3, 100)")
                .unwrap();
            // Exact addition: 0.1 + 0.2 = 0.3 (not 0.30000000000000004).
            let (_c, sum) = query(
                &mut db,
                "SELECT amount + DECIMAL '0.2' FROM money WHERE id = 1",
            );
            assert_eq!(sum, vec![vec![dec("0.3")]]);
            // SUM over a decimal column stays exact, and ORDER BY sorts as numbers.
            let (_c, total) = query(&mut db, "SELECT SUM(amount) FROM money");
            assert_eq!(total, vec![vec![dec("100.30")]]);
            let (_c, ordered) = query(&mut db, "SELECT amount FROM money ORDER BY amount DESC");
            assert_eq!(
                ordered,
                vec![vec![dec("100.00")], vec![dec("0.2")], vec![dec("0.1")]]
            );
            // A comparison against a literal works across scales.
            let (_c, big) = query(&mut db, "SELECT id FROM money WHERE amount > 0.15");
            assert_eq!(big, vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
        }
        // Values survive a reopen, and a cast round-trips.
        let mut db = Database::open(&path).expect("reopen");
        let (_c, rows) = query(&mut db, "SELECT amount FROM money WHERE id = 3");
        assert_eq!(rows, vec![vec![dec("100.00")]]);
        let (_c, c) = query(&mut db, "SELECT (22 / 7)::decimal FROM money WHERE id = 1");
        // 22 and 7 are integers, so 22/7 is integer division (3), cast to decimal.
        assert_eq!(c, vec![vec![dec("3")]]);
    }

    fn parse_day(s: &str) -> i64 {
        picklejar_sql::datetime::parse_date(s).expect("date")
    }

    fn parse_micros(s: &str) -> i64 {
        picklejar_sql::datetime::parse_timestamp(s).expect("timestamp")
    }

    #[test]
    fn greatest_and_least_skip_nulls() {
        let (_d, mut db) = db();
        db.execute("CREATE TABLE t (a INT, b INT, c INT)").unwrap();
        db.execute("INSERT INTO t VALUES (3, 7, 1), (5, NULL, 2)")
            .unwrap();
        let (_c, rows) = query(
            &mut db,
            "SELECT GREATEST(a, b, c), LEAST(a, b, c) FROM t ORDER BY a",
        );
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(7), Value::Int(1)],
                // The NULL b is ignored, not propagated.
                vec![Value::Int(5), Value::Int(2)],
            ]
        );
        // All-NULL is NULL.
        let (_c, n) = query(&mut db, "SELECT GREATEST(NULL, NULL) FROM t WHERE a = 3");
        assert_eq!(n, vec![vec![Value::Null]]);
    }
}
