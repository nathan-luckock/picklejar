//! Statement AST nodes and their parsers.
//!
//! Built on top of the expression core in [`crate::parser`]. Each statement
//! type's parser is a method on [`Parser`], sharing the cursor helpers and
//! the Pratt `parse_expr`. Covers DDL (CREATE TABLE, DROP TABLE, CREATE
//! INDEX), DML (INSERT, UPDATE, DELETE), and SELECT (projections, joins,
//! WHERE, GROUP BY, ORDER BY, LIMIT).
//!
//! Every node implements `Display` back to canonical SQL, which doubles as a
//! normalizer and is the oracle for the parser round-trip property test.

use std::fmt;

use crate::ast::{Expr, Value};
use crate::error::{Result, SqlError};
use crate::parser::Parser;
use crate::token::{Keyword, TokenKind};

/// A reference to a table in a FROM or JOIN clause, with optional alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRef {
    /// Table name (empty for a derived table).
    pub name: String,
    /// Optional alias (`t` in `FROM table AS t`; required for a derived table).
    pub alias: Option<String>,
    /// A derived table: `FROM (SELECT ...) AS t`. `None` for a named table.
    pub subquery: Option<Box<Statement>>,
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(sub) = &self.subquery {
            write!(f, "({sub})")?;
        } else {
            f.write_str(&self.name)?;
        }
        if let Some(a) = &self.alias {
            write!(f, " AS {a}")?;
        }
        Ok(())
    }
}

/// One item in a SELECT projection list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectItem {
    /// `*` - all columns.
    Star,
    /// An expression with an optional alias.
    Expr(Expr, Option<String>),
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Star => f.write_str("*"),
            Self::Expr(e, None) => write!(f, "{e}"),
            Self::Expr(e, Some(a)) => write!(f, "{e} AS {a}"),
        }
    }
}

/// The kind of a join.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JoinKind {
    /// `INNER JOIN`.
    Inner,
    /// `LEFT JOIN`.
    Left,
}

impl fmt::Display for JoinKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Inner => "INNER JOIN",
            Self::Left => "LEFT JOIN",
        })
    }
}

/// A join clause: `<kind> <table> ON <predicate>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Join {
    /// Inner or left.
    pub kind: JoinKind,
    /// The joined table.
    pub table: TableRef,
    /// The ON predicate.
    pub on: Expr,
}

impl fmt::Display for Join {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ON {}", self.kind, self.table, self.on)
    }
}

/// An ORDER BY item.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderItem {
    /// The sort key.
    pub expr: Expr,
    /// True for descending.
    pub desc: bool,
}

impl fmt::Display for OrderItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.desc {
            f.write_str(" DESC")?;
        }
        Ok(())
    }
}

/// A SELECT query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Select {
    /// `SELECT DISTINCT`: dedup the output rows.
    pub distinct: bool,
    /// Projection list.
    pub projections: Vec<SelectItem>,
    /// The driving table.
    pub from: TableRef,
    /// Joins (empty for a single-table select).
    pub joins: Vec<Join>,
    /// Optional WHERE predicate.
    pub where_clause: Option<Expr>,
    /// GROUP BY keys (empty if none).
    pub group_by: Vec<Expr>,
    /// HAVING predicate, applied after grouping (None if none).
    pub having: Option<Expr>,
    /// ORDER BY items (empty if none).
    pub order_by: Vec<OrderItem>,
    /// LIMIT (None if none).
    pub limit: Option<u64>,
    /// OFFSET: rows to skip before LIMIT (None if none).
    pub offset: Option<u64>,
}

impl fmt::Display for Select {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        for (i, p) in self.projections.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{p}")?;
        }
        write!(f, " FROM {}", self.from)?;
        for j in &self.joins {
            write!(f, " {j}")?;
        }
        if let Some(w) = &self.where_clause {
            write!(f, " WHERE {w}")?;
        }
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            for (i, g) in self.group_by.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{g}")?;
            }
        }
        if let Some(h) = &self.having {
            write!(f, " HAVING {h}")?;
        }
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            for (i, o) in self.order_by.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{o}")?;
            }
        }
        if let Some(n) = self.limit {
            write!(f, " LIMIT {n}")?;
        }
        if let Some(n) = self.offset {
            write!(f, " OFFSET {n}")?;
        }
        Ok(())
    }
}

/// A column data type.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DataType {
    /// 64-bit signed integer.
    Int,
    /// 64-bit IEEE-754 floating point.
    Float,
    /// Boolean.
    Bool,
    /// Variable-length text.
    Text,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int => "INT",
            Self::Float => "FLOAT",
            Self::Bool => "BOOL",
            Self::Text => "TEXT",
        })
    }
}

/// A column definition in a `CREATE TABLE`.
///
/// The four boolean attributes (primary key, NOT NULL, UNIQUE, SERIAL) are
/// independent SQL column flags, not a hidden state machine, so they stay as
/// plain bools rather than being folded into an enum.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// Declared type.
    pub ty: DataType,
    /// Whether the column is the primary key (implies NOT NULL and UNIQUE).
    pub primary_key: bool,
    /// Whether NULL is rejected for this column.
    pub not_null: bool,
    /// Whether values must be unique across rows.
    pub unique: bool,
    /// `DEFAULT <expr>` value for omitted columns (a constant expression).
    pub default: Option<Expr>,
    /// `SERIAL`: an integer column that auto-increments when omitted on insert.
    pub serial: bool,
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.serial {
            write!(f, "{} SERIAL", self.name)?;
        } else {
            write!(f, "{} {}", self.name, self.ty)?;
        }
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        if self.unique {
            f.write_str(" UNIQUE")?;
        }
        if let Some(d) = &self.default {
            write!(f, " DEFAULT {d}")?;
        }
        Ok(())
    }
}

/// A single-column foreign-key reference: `column` of this table must match
/// `parent_column` of `parent_table` (or be NULL).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ForeignKey {
    /// The referencing column in this table.
    pub column: String,
    /// The referenced (parent) table.
    pub parent_table: String,
    /// The referenced column in the parent table.
    pub parent_column: String,
}

impl fmt::Display for ForeignKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            self.column, self.parent_table, self.parent_column
        )
    }
}

/// A table-level constraint in a `CREATE TABLE`. Column-level `CHECK` and
/// `REFERENCES` are normalized into these at parse time, so a table carries a
/// single uniform list of constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TableConstraint {
    /// `CHECK (predicate)`: a row is rejected if the predicate is false.
    Check(Expr),
    /// `FOREIGN KEY (col) REFERENCES parent (col)`.
    ForeignKey(ForeignKey),
}

impl fmt::Display for TableConstraint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Check(e) => write!(f, "CHECK ({e})"),
            Self::ForeignKey(fk) => write!(f, "{fk}"),
        }
    }
}

/// A set operation combining two queries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SetOp {
    /// `UNION`: rows in either side.
    Union,
    /// `INTERSECT`: rows in both sides.
    Intersect,
    /// `EXCEPT`: rows in the left side but not the right.
    Except,
}

impl SetOp {
    /// The SQL keyword for this operation.
    #[must_use]
    pub const fn keyword(self) -> &'static str {
        match self {
            Self::Union => "UNION",
            Self::Intersect => "INTERSECT",
            Self::Except => "EXCEPT",
        }
    }
}

/// One common table expression: `name [(cols)] AS (query)` in a `WITH` clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Cte {
    /// The CTE's name, referenced like a table in the body.
    pub name: String,
    /// Optional output column names (empty if not given).
    pub columns: Vec<String>,
    /// The defining query.
    pub query: Box<Statement>,
}

impl fmt::Display for Cte {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.columns.is_empty() {
            write!(f, " ({})", self.columns.join(", "))?;
        }
        write!(f, " AS ({})", self.query)
    }
}

/// A parsed SQL statement. Grows as SELECT and DML parsers land.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE name (cols..., constraints...)`.
    CreateTable {
        /// Table name.
        name: String,
        /// Column definitions.
        columns: Vec<ColumnDef>,
        /// Table-level constraints (`CHECK`, `FOREIGN KEY`), including any
        /// normalized from column-level `CHECK` / `REFERENCES` clauses.
        constraints: Vec<TableConstraint>,
    },
    /// `CREATE TABLE name AS <query>`: create a table from a query's result,
    /// inferring its columns and populating it with the rows.
    CreateTableAs {
        /// New table name.
        name: String,
        /// The query whose result becomes the table.
        query: Box<Self>,
    },
    /// `DROP TABLE name`.
    DropTable {
        /// Table name.
        name: String,
    },
    /// `CREATE INDEX name ON table (column)`.
    CreateIndex {
        /// Index name.
        name: String,
        /// Indexed table.
        table: String,
        /// Indexed column.
        column: String,
    },
    /// `CREATE VIEW name AS <query>`: a named, stored query expanded as a
    /// derived table wherever the view name appears in a FROM or JOIN.
    CreateView {
        /// View name.
        name: String,
        /// The defining query (a `Select` or a `Union`).
        query: Box<Self>,
    },
    /// `DROP VIEW name`.
    DropView {
        /// View name.
        name: String,
    },
    /// A `SELECT` query.
    Select(Box<Select>),
    /// `INSERT INTO t (cols) VALUES (...), (...)`.
    Insert {
        /// Target table.
        table: String,
        /// Column list.
        columns: Vec<String>,
        /// One or more value rows; each row has one expression per column.
        rows: Vec<Vec<Expr>>,
        /// `ON CONFLICT ...` clause (None if absent).
        on_conflict: Option<OnConflict>,
        /// `RETURNING` projection over the inserted rows (empty if absent).
        returning: Vec<SelectItem>,
    },
    /// `UPDATE t SET c = e, ... [WHERE pred]`.
    Update {
        /// Target table.
        table: String,
        /// `(column, value)` assignments.
        assignments: Vec<(String, Expr)>,
        /// Optional WHERE predicate.
        where_clause: Option<Expr>,
        /// `RETURNING` projection over the updated rows (empty if absent).
        returning: Vec<SelectItem>,
    },
    /// `DELETE FROM t [WHERE pred]`.
    Delete {
        /// Target table.
        table: String,
        /// Optional WHERE predicate.
        where_clause: Option<Expr>,
        /// `RETURNING` projection over the deleted rows (empty if absent).
        returning: Vec<SelectItem>,
    },
    /// `EXPLAIN [ANALYZE] <statement>`: plan the inner statement instead of
    /// running it; `ANALYZE` also runs it and reports actual rows and time.
    Explain {
        /// Whether `ANALYZE` was given.
        analyze: bool,
        /// The statement to plan (and, under `ANALYZE`, run).
        statement: Box<Self>,
    },
    /// `TRUNCATE TABLE t`: remove all rows from `t`.
    Truncate {
        /// Target table.
        table: String,
    },
    /// `ANALYZE [table]`: recompute planner statistics. `None` analyzes every
    /// table.
    Analyze {
        /// Target table, or `None` for all tables.
        table: Option<String>,
    },
    /// `VACUUM [table]`: compact a table, reclaiming dead row versions and index
    /// bloat. `None` vacuums every table.
    Vacuum {
        /// Target table, or `None` for all tables.
        table: Option<String>,
    },
    /// `COPY table {FROM | TO} 'path' [HEADER]`: bulk-load a table from a CSV
    /// file, or write its rows out to one.
    Copy {
        /// The table to load into or read from.
        table: String,
        /// `true` for `TO` (export), `false` for `FROM` (import).
        to: bool,
        /// The CSV file path.
        path: String,
        /// Whether the file has (import) or should get (export) a header row.
        header: bool,
    },
    /// `ALTER TABLE t ADD COLUMN c TYPE ...`: append a column.
    AlterTableAddColumn {
        /// Target table.
        table: String,
        /// The new column's definition.
        column: ColumnDef,
    },
    /// `BEGIN`: start an explicit transaction.
    Begin,
    /// `COMMIT`: commit the current transaction.
    Commit,
    /// `ROLLBACK`: abort the current transaction.
    Rollback,
    /// `WITH [RECURSIVE] cte, ... body`: common table expressions scoped to the
    /// body query. The engine inlines each reference before planning.
    With {
        /// Whether `RECURSIVE` was given (a CTE may reference itself).
        recursive: bool,
        /// The named CTEs, in declaration order (a later one may reference an
        /// earlier one).
        ctes: Vec<Cte>,
        /// The query the CTEs are visible to.
        body: Box<Self>,
    },
    /// `left {UNION|INTERSECT|EXCEPT} [ALL] right`. `left` and `right` are
    /// queries (a `Select` or a nested set operation). Without `all`, duplicate
    /// rows are removed. A trailing `ORDER BY` / `LIMIT` / `OFFSET` applies to
    /// the whole result and lives on the outermost node (inner nodes of a chain
    /// leave them empty). The variant is named `Union` for historical reasons;
    /// `op` selects which set operation it is.
    Union {
        /// Which set operation: `UNION`, `INTERSECT`, or `EXCEPT`.
        op: SetOp,
        /// `ALL` keeps duplicates; without it, duplicate rows are removed.
        all: bool,
        /// Left query.
        left: Box<Self>,
        /// Right query.
        right: Box<Self>,
        /// ORDER BY over the output (empty if none).
        order_by: Vec<OrderItem>,
        /// LIMIT over the output (None if none).
        limit: Option<u64>,
        /// OFFSET over the output (None if none).
        offset: Option<u64>,
    },
}

/// The `ON CONFLICT` clause of an `INSERT`: what to do when a proposed row
/// would violate a unique or primary-key constraint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OnConflict {
    /// The conflict-target columns named in `ON CONFLICT (a, b)`. Empty means
    /// the action fires on a conflict in any unique column, which is the only
    /// form Postgres allows with `DO NOTHING`.
    pub target: Vec<String>,
    /// What to do when a conflict is detected.
    pub action: ConflictAction,
}

/// The action half of an `ON CONFLICT` clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConflictAction {
    /// `DO NOTHING`: skip the conflicting row without error.
    Nothing,
    /// `DO UPDATE SET ... [WHERE ...]`: update the existing row in place.
    /// Assignment right-hand sides may reference `excluded.col`, which binds to
    /// the rejected row's proposed value (Postgres `EXCLUDED`).
    Update {
        /// `(column, value)` assignments applied to the existing row.
        assignments: Vec<(String, Expr)>,
        /// Optional predicate; the update is skipped when it is false.
        where_clause: Option<Expr>,
    },
}

impl fmt::Display for OnConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ON CONFLICT")?;
        if !self.target.is_empty() {
            write!(f, " ({})", self.target.join(", "))?;
        }
        match &self.action {
            ConflictAction::Nothing => f.write_str(" DO NOTHING"),
            ConflictAction::Update {
                assignments,
                where_clause,
            } => {
                f.write_str(" DO UPDATE SET ")?;
                for (i, (col, val)) in assignments.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{col} = {val}")?;
                }
                if let Some(w) = where_clause {
                    write!(f, " WHERE {w}")?;
                }
                Ok(())
            }
        }
    }
}

impl OnConflict {
    /// Bind positional parameters in a `DO UPDATE` action's expressions.
    #[must_use]
    fn substitute_params(&self, params: &[Value]) -> Self {
        let action = match &self.action {
            ConflictAction::Nothing => ConflictAction::Nothing,
            ConflictAction::Update {
                assignments,
                where_clause,
            } => ConflictAction::Update {
                assignments: assignments
                    .iter()
                    .map(|(c, e)| (c.clone(), e.substitute_params(params)))
                    .collect(),
                where_clause: where_clause.as_ref().map(|w| w.substitute_params(params)),
            },
        };
        Self {
            target: self.target.clone(),
            action,
        }
    }
}

/// Write a ` ON CONFLICT ...` clause, or nothing when absent.
fn write_on_conflict(f: &mut fmt::Formatter<'_>, on_conflict: Option<&OnConflict>) -> fmt::Result {
    if let Some(oc) = on_conflict {
        write!(f, " {oc}")?;
    }
    Ok(())
}

/// Write a `RETURNING <items>` clause, or nothing when `returning` is empty.
fn write_returning(f: &mut fmt::Formatter<'_>, returning: &[SelectItem]) -> fmt::Result {
    if returning.is_empty() {
        return Ok(());
    }
    f.write_str(" RETURNING ")?;
    for (i, item) in returning.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl fmt::Display for Statement {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateTable {
                name,
                columns,
                constraints,
            } => {
                write!(f, "CREATE TABLE {name} (")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{c}")?;
                }
                for con in constraints {
                    write!(f, ", {con}")?;
                }
                f.write_str(")")
            }
            Self::DropTable { name } => write!(f, "DROP TABLE {name}"),
            Self::CreateIndex {
                name,
                table,
                column,
            } => write!(f, "CREATE INDEX {name} ON {table} ({column})"),
            Self::CreateTableAs { name, query } => write!(f, "CREATE TABLE {name} AS {query}"),
            Self::CreateView { name, query } => write!(f, "CREATE VIEW {name} AS {query}"),
            Self::DropView { name } => write!(f, "DROP VIEW {name}"),
            Self::Select(s) => write!(f, "{s}"),
            Self::Insert {
                table,
                columns,
                rows,
                on_conflict,
                returning,
            } => {
                write!(f, "INSERT INTO {table}")?;
                if !columns.is_empty() {
                    f.write_str(" (")?;
                    for (i, c) in columns.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        f.write_str(c)?;
                    }
                    f.write_str(")")?;
                }
                f.write_str(" VALUES ")?;
                for (ri, row) in rows.iter().enumerate() {
                    if ri > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str("(")?;
                    for (i, v) in row.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{v}")?;
                    }
                    f.write_str(")")?;
                }
                write_on_conflict(f, on_conflict.as_ref())?;
                write_returning(f, returning)
            }
            Self::Update {
                table,
                assignments,
                where_clause,
                returning,
            } => {
                write!(f, "UPDATE {table} SET ")?;
                for (i, (col, val)) in assignments.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{col} = {val}")?;
                }
                if let Some(w) = where_clause {
                    write!(f, " WHERE {w}")?;
                }
                write_returning(f, returning)
            }
            Self::Delete {
                table,
                where_clause,
                returning,
            } => {
                write!(f, "DELETE FROM {table}")?;
                if let Some(w) = where_clause {
                    write!(f, " WHERE {w}")?;
                }
                write_returning(f, returning)
            }
            Self::Explain { analyze, statement } => {
                if *analyze {
                    write!(f, "EXPLAIN ANALYZE {statement}")
                } else {
                    write!(f, "EXPLAIN {statement}")
                }
            }
            Self::Truncate { table } => write!(f, "TRUNCATE TABLE {table}"),
            Self::Analyze { table } => match table {
                Some(t) => write!(f, "ANALYZE {t}"),
                None => f.write_str("ANALYZE"),
            },
            Self::Vacuum { table } => match table {
                Some(t) => write!(f, "VACUUM {t}"),
                None => f.write_str("VACUUM"),
            },
            Self::Copy {
                table,
                to,
                path,
                header,
            } => {
                let dir = if *to { "TO" } else { "FROM" };
                let quoted = path.replace('\'', "''");
                write!(f, "COPY {table} {dir} '{quoted}'")?;
                if *header {
                    f.write_str(" HEADER")?;
                }
                Ok(())
            }
            Self::AlterTableAddColumn { table, column } => {
                write!(f, "ALTER TABLE {table} ADD COLUMN {column}")
            }
            Self::Begin => f.write_str("BEGIN"),
            Self::Commit => f.write_str("COMMIT"),
            Self::Rollback => f.write_str("ROLLBACK"),
            Self::With {
                recursive,
                ctes,
                body,
            } => {
                f.write_str("WITH ")?;
                if *recursive {
                    f.write_str("RECURSIVE ")?;
                }
                for (i, cte) in ctes.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{cte}")?;
                }
                write!(f, " {body}")
            }
            Self::Union {
                op,
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => {
                let kw = if *all {
                    format!("{} ALL", op.keyword())
                } else {
                    op.keyword().to_string()
                };
                write!(f, "{left} {kw} {right}")?;
                if !order_by.is_empty() {
                    f.write_str(" ORDER BY ")?;
                    for (i, o) in order_by.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{o}")?;
                    }
                }
                if let Some(n) = limit {
                    write!(f, " LIMIT {n}")?;
                }
                if let Some(n) = offset {
                    write!(f, " OFFSET {n}")?;
                }
                Ok(())
            }
        }
    }
}

impl Parser {
    /// Parse a single statement, consuming an optional trailing semicolon.
    pub fn parse_statement(&mut self) -> Result<Statement> {
        let stmt = match self.peek() {
            TokenKind::Keyword(Keyword::Create) => self.parse_create()?,
            TokenKind::Keyword(Keyword::Drop) => self.parse_drop()?,
            TokenKind::Keyword(Keyword::Truncate) => {
                self.advance();
                self.eat_keyword(Keyword::Table); // optional TABLE keyword
                let table = self.parse_ident()?;
                Statement::Truncate { table }
            }
            TokenKind::Keyword(Keyword::Analyze) => {
                self.advance();
                // An optional table name; bare `ANALYZE` covers every table.
                let table = if matches!(self.peek(), TokenKind::Ident(_)) {
                    Some(self.parse_ident()?)
                } else {
                    None
                };
                Statement::Analyze { table }
            }
            TokenKind::Keyword(Keyword::Vacuum) => {
                self.advance();
                let table = if matches!(self.peek(), TokenKind::Ident(_)) {
                    Some(self.parse_ident()?)
                } else {
                    None
                };
                Statement::Vacuum { table }
            }
            TokenKind::Keyword(Keyword::Copy) => {
                self.advance();
                let table = self.parse_ident()?;
                // FROM imports, TO exports. TO and HEADER are context-sensitive
                // words (not reserved), so they stay usable as identifiers.
                let to = if self.eat_keyword(Keyword::From) {
                    false
                } else if self.eat_ident_kw("to") {
                    true
                } else {
                    return Err(SqlError::parse(
                        "expected FROM or TO after COPY <table>".to_string(),
                        self.span(),
                    ));
                };
                let path = self.parse_string()?;
                let header = self.eat_ident_kw("header");
                Statement::Copy {
                    table,
                    to,
                    path,
                    header,
                }
            }
            TokenKind::Keyword(Keyword::Alter) => {
                self.advance();
                self.expect_keyword(Keyword::Table)?;
                let table = self.parse_ident()?;
                self.expect_keyword(Keyword::Add)?;
                self.eat_keyword(Keyword::Column); // optional COLUMN keyword
                                                   // Inline constraints on an added column are not yet enforced;
                                                   // accept the column definition and ignore them.
                let (column, _inline) = self.parse_column_def()?;
                Statement::AlterTableAddColumn { table, column }
            }
            TokenKind::Keyword(Keyword::Select) => self.parse_query()?,
            TokenKind::Keyword(Keyword::With) => self.parse_with()?,
            TokenKind::Keyword(Keyword::Insert) => self.parse_insert()?,
            TokenKind::Keyword(Keyword::Update) => self.parse_update()?,
            TokenKind::Keyword(Keyword::Delete) => self.parse_delete()?,
            TokenKind::Keyword(Keyword::Explain) => {
                self.advance();
                let analyze = self.eat_keyword(Keyword::Analyze);
                Statement::Explain {
                    analyze,
                    statement: Box::new(self.parse_statement()?),
                }
            }
            TokenKind::Keyword(Keyword::Begin) => {
                self.advance();
                Statement::Begin
            }
            TokenKind::Keyword(Keyword::Commit) => {
                self.advance();
                Statement::Commit
            }
            TokenKind::Keyword(Keyword::Rollback) => {
                self.advance();
                Statement::Rollback
            }
            other => {
                return Err(SqlError::parse(
                    format!("expected a statement, found {other:?}"),
                    self.span(),
                ));
            }
        };
        self.eat(&TokenKind::Semicolon);
        Ok(stmt)
    }

    /// Parse a SELECT query, including JOIN / WHERE / GROUP BY / ORDER BY /
    /// LIMIT.
    /// Parse a query: a `SELECT`, optionally combined with more `SELECT`s via
    /// `UNION` / `UNION ALL` (folded left-associatively), with any trailing
    /// `ORDER BY` / `LIMIT` / `OFFSET` applying to the whole query.
    pub(crate) fn parse_query(&mut self) -> Result<Statement> {
        let mut query = Statement::Select(Box::new(self.parse_select()?));
        // UNION / INTERSECT / EXCEPT chain left-associatively at equal
        // precedence (matching SQLite's left-to-right grouping).
        while let Some(op) = self.eat_set_op() {
            let all = self.eat_keyword(Keyword::All);
            let right = Statement::Select(Box::new(self.parse_select()?));
            query = Statement::Union {
                op,
                all,
                left: Box::new(query),
                right: Box::new(right),
                order_by: Vec::new(),
                limit: None,
                offset: None,
            };
        }
        // A trailing ORDER BY / LIMIT / OFFSET binds the whole query. For a
        // single SELECT it lives on the Select; for a union, on its node.
        let order_by = self.parse_order_by()?;
        let limit = self.parse_limit()?;
        let offset = self.parse_offset()?;
        match &mut query {
            Statement::Select(s) => {
                s.order_by = order_by;
                s.limit = limit;
                s.offset = offset;
            }
            Statement::Union {
                order_by: o,
                limit: l,
                offset: off,
                ..
            } => {
                *o = order_by;
                *l = limit;
                *off = offset;
            }
            _ => unreachable!("parse_query builds only Select or Union"),
        }
        Ok(query)
    }

    /// Parse `WITH [RECURSIVE] cte, ... body`, where each CTE is
    /// `name [(cols)] AS (query)`.
    fn parse_with(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::With)?;
        let recursive = self.eat_keyword(Keyword::Recursive);
        let mut ctes = Vec::new();
        loop {
            let name = self.parse_ident()?;
            let columns = if self.eat(&TokenKind::LParen) {
                let cols = self.parse_ident_list()?;
                self.expect(&TokenKind::RParen)?;
                cols
            } else {
                Vec::new()
            };
            self.expect_keyword(Keyword::As)?;
            self.expect(&TokenKind::LParen)?;
            let query = self.parse_query()?;
            self.expect(&TokenKind::RParen)?;
            ctes.push(Cte {
                name,
                columns,
                query: Box::new(query),
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let body = self.parse_query()?;
        Ok(Statement::With {
            recursive,
            ctes,
            body: Box::new(body),
        })
    }

    /// Consume a leading set-operation keyword (`UNION` / `INTERSECT` /
    /// `EXCEPT`) if present, returning which one.
    fn eat_set_op(&mut self) -> Option<SetOp> {
        if self.eat_keyword(Keyword::Union) {
            Some(SetOp::Union)
        } else if self.eat_keyword(Keyword::Intersect) {
            Some(SetOp::Intersect)
        } else if self.eat_keyword(Keyword::Except) {
            Some(SetOp::Except)
        } else {
            None
        }
    }

    /// Parse one `SELECT` term, up to but not including any trailing `ORDER BY`
    /// / `LIMIT` / `OFFSET` (those bind the whole query, see `parse_query`).
    fn parse_select(&mut self) -> Result<Select> {
        self.expect_keyword(Keyword::Select)?;
        let distinct = self.eat_keyword(Keyword::Distinct);
        let projections = self.parse_projections()?;
        self.expect_keyword(Keyword::From)?;
        let from = self.parse_table_ref()?;
        let joins = self.parse_joins()?;
        let where_clause = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let group_by = self.parse_group_by()?;
        let having = if self.eat_keyword(Keyword::Having) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        // ORDER BY / LIMIT / OFFSET are parsed by the caller (parse_query), so
        // they apply to the whole query, not just this term of a union.
        Ok(Select {
            distinct,
            projections,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by: Vec::new(),
            limit: None,
            offset: None,
        })
    }

    /// Parse zero or more JOIN clauses. A bare `JOIN` means `INNER JOIN`.
    fn parse_joins(&mut self) -> Result<Vec<Join>> {
        // A cross join (`CROSS JOIN` or a comma in FROM) is an inner join with
        // an always-true predicate, i.e. the cartesian product.
        let cross = || Join {
            kind: JoinKind::Inner,
            table: TableRef {
                name: String::new(),
                alias: None,
                subquery: None,
            },
            on: Expr::Literal(crate::ast::Value::Bool(true)),
        };
        let mut joins = Vec::new();
        loop {
            if self.eat(&TokenKind::Comma) {
                let table = self.parse_table_ref()?;
                joins.push(Join { table, ..cross() });
                continue;
            }
            if self.eat_keyword(Keyword::Cross) {
                self.expect_keyword(Keyword::Join)?;
                let table = self.parse_table_ref()?;
                joins.push(Join { table, ..cross() });
                continue;
            }
            let kind = if self.eat_keyword(Keyword::Inner) {
                self.expect_keyword(Keyword::Join)?;
                JoinKind::Inner
            } else if self.eat_keyword(Keyword::Left) {
                self.expect_keyword(Keyword::Join)?;
                JoinKind::Left
            } else if self.eat_keyword(Keyword::Join) {
                JoinKind::Inner
            } else {
                break;
            };
            let table = self.parse_table_ref()?;
            self.expect_keyword(Keyword::On)?;
            let on = self.parse_expr()?;
            joins.push(Join { kind, table, on });
        }
        Ok(joins)
    }

    fn parse_group_by(&mut self) -> Result<Vec<Expr>> {
        if !self.eat_keyword(Keyword::Group) {
            return Ok(Vec::new());
        }
        self.expect_keyword(Keyword::By)?;
        let mut keys = Vec::new();
        loop {
            keys.push(self.parse_expr()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(keys)
    }

    pub(crate) fn parse_order_by(&mut self) -> Result<Vec<OrderItem>> {
        if !self.eat_keyword(Keyword::Order) {
            return Ok(Vec::new());
        }
        self.expect_keyword(Keyword::By)?;
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            // ASC is the default; DESC flips it.
            let desc = if self.eat_keyword(Keyword::Desc) {
                true
            } else {
                self.eat_keyword(Keyword::Asc);
                false
            };
            items.push(OrderItem { expr, desc });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    fn parse_limit(&mut self) -> Result<Option<u64>> {
        if !self.eat_keyword(Keyword::Limit) {
            return Ok(None);
        }
        self.parse_row_count("LIMIT")
    }

    fn parse_offset(&mut self) -> Result<Option<u64>> {
        if !self.eat_keyword(Keyword::Offset) {
            return Ok(None);
        }
        self.parse_row_count("OFFSET")
    }

    /// Parse the non-negative integer following `LIMIT` / `OFFSET`.
    fn parse_row_count(&mut self, clause: &str) -> Result<Option<u64>> {
        match self.peek().clone() {
            TokenKind::Int(n) if n >= 0 => {
                self.advance();
                // n >= 0 just checked, so the cast is exact.
                #[allow(clippy::cast_sign_loss)]
                Ok(Some(n as u64))
            }
            other => Err(SqlError::parse(
                format!("expected a non-negative integer after {clause}, found {other:?}"),
                self.span(),
            )),
        }
    }

    fn parse_projections(&mut self) -> Result<Vec<SelectItem>> {
        let mut items = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            let item = if expr == Expr::Star {
                SelectItem::Star
            } else {
                SelectItem::Expr(expr, self.parse_optional_alias())
            };
            items.push(item);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(items)
    }

    /// Parse a table reference with an optional alias.
    fn parse_table_ref(&mut self) -> Result<TableRef> {
        // A derived table: `(SELECT ...) [AS] alias`.
        if matches!(self.peek(), TokenKind::LParen) {
            self.advance();
            let query = self.parse_query()?;
            self.expect(&TokenKind::RParen)?;
            let alias = self.parse_optional_alias().ok_or_else(|| {
                SqlError::parse("a derived table requires an alias".to_string(), self.span())
            })?;
            return Ok(TableRef {
                name: String::new(),
                alias: Some(alias),
                subquery: Some(Box::new(query)),
            });
        }
        // A table name, optionally schema-qualified (`schema.table`, e.g.
        // `information_schema.tables`). The qualified form is stored verbatim as
        // a single dotted name.
        let mut name = self.parse_ident()?;
        if self.eat(&TokenKind::Dot) {
            let part = self.parse_ident()?;
            name = format!("{name}.{part}");
        }
        Ok(TableRef {
            name,
            alias: self.parse_optional_alias(),
            subquery: None,
        })
    }

    /// Parse an optional alias: `AS ident`, or a bare trailing identifier.
    fn parse_optional_alias(&mut self) -> Option<String> {
        if self.eat_keyword(Keyword::As) {
            // After AS an identifier is required, but tolerate a missing one
            // by returning None (the next expect will report the real error).
            if let TokenKind::Ident(name) = self.peek().clone() {
                self.advance();
                return Some(name);
            }
            return None;
        }
        if let TokenKind::Ident(name) = self.peek().clone() {
            self.advance();
            return Some(name);
        }
        None
    }

    fn parse_create(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        if self.eat_keyword(Keyword::Table) {
            self.parse_create_table_tail()
        } else if self.eat_keyword(Keyword::Index) {
            self.parse_create_index_tail()
        } else if self.eat_keyword(Keyword::View) {
            self.parse_create_view_tail()
        } else {
            Err(SqlError::parse(
                format!(
                    "expected TABLE, INDEX, or VIEW after CREATE, found {:?}",
                    self.peek()
                ),
                self.span(),
            ))
        }
    }

    fn parse_create_view_tail(&mut self) -> Result<Statement> {
        let name = self.parse_ident()?;
        self.expect_keyword(Keyword::As)?;
        let query = self.parse_query()?;
        if !matches!(query, Statement::Select(_) | Statement::Union { .. }) {
            return Err(SqlError::parse(
                "CREATE VIEW requires a SELECT or UNION query",
                self.span(),
            ));
        }
        Ok(Statement::CreateView {
            name,
            query: Box::new(query),
        })
    }

    fn parse_create_table_tail(&mut self) -> Result<Statement> {
        let name = self.parse_ident()?;
        // `CREATE TABLE name AS <query>` builds the table from a query instead
        // of an explicit column list.
        if self.eat_keyword(Keyword::As) {
            let query = self.parse_query()?;
            return Ok(Statement::CreateTableAs {
                name,
                query: Box::new(query),
            });
        }
        self.expect(&TokenKind::LParen)?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        loop {
            // A leading CONSTRAINT name, CHECK, or FOREIGN starts a table-level
            // constraint; anything else is a column definition (which may carry
            // its own inline CHECK / REFERENCES, normalized to table level).
            if self.eat_keyword(Keyword::Constraint) {
                let _name = self.parse_ident()?; // optional constraint name, ignored
                constraints.push(self.parse_table_constraint()?);
            } else if matches!(
                self.peek(),
                TokenKind::Keyword(Keyword::Check | Keyword::Foreign)
            ) {
                constraints.push(self.parse_table_constraint()?);
            } else {
                let (column, inline) = self.parse_column_def()?;
                columns.push(column);
                constraints.extend(inline);
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen)?;
        if columns.is_empty() {
            return Err(SqlError::parse(
                "CREATE TABLE needs at least one column",
                self.span(),
            ));
        }
        Ok(Statement::CreateTable {
            name,
            columns,
            constraints,
        })
    }

    /// Parse a table-level `CHECK (expr)` or `FOREIGN KEY (col) REFERENCES
    /// parent (col)`.
    fn parse_table_constraint(&mut self) -> Result<TableConstraint> {
        if self.eat_keyword(Keyword::Check) {
            self.expect(&TokenKind::LParen)?;
            let expr = self.parse_expr()?;
            self.expect(&TokenKind::RParen)?;
            Ok(TableConstraint::Check(expr))
        } else if self.eat_keyword(Keyword::Foreign) {
            self.expect_keyword(Keyword::Key)?;
            self.expect(&TokenKind::LParen)?;
            let column = self.parse_ident()?;
            self.expect(&TokenKind::RParen)?;
            Ok(TableConstraint::ForeignKey(self.parse_references(column)?))
        } else {
            Err(SqlError::parse(
                "expected CHECK or FOREIGN KEY",
                self.span(),
            ))
        }
    }

    /// Parse the `REFERENCES parent (col)` tail, given the referencing column.
    fn parse_references(&mut self, column: String) -> Result<ForeignKey> {
        self.expect_keyword(Keyword::References)?;
        self.parse_references_tail(column)
    }

    /// Parse a column definition, returning it together with any table-level
    /// constraints normalized from inline `CHECK` / `REFERENCES` clauses.
    fn parse_column_def(&mut self) -> Result<(ColumnDef, Vec<TableConstraint>)> {
        let name = self.parse_ident()?;
        // SERIAL is an auto-incrementing integer column.
        let mut serial = false;
        let ty = match self.peek() {
            TokenKind::Keyword(Keyword::Int) => {
                self.advance();
                DataType::Int
            }
            TokenKind::Keyword(Keyword::Serial) => {
                self.advance();
                serial = true;
                DataType::Int
            }
            TokenKind::Keyword(Keyword::Float) => {
                self.advance();
                DataType::Float
            }
            TokenKind::Keyword(Keyword::Bool) => {
                self.advance();
                DataType::Bool
            }
            TokenKind::Keyword(Keyword::Text) => {
                self.advance();
                DataType::Text
            }
            other => {
                return Err(SqlError::parse(
                    format!(
                        "expected a column type (INT, SERIAL, FLOAT, BOOL, or TEXT), found {other:?}"
                    ),
                    self.span(),
                ));
            }
        };
        // Optional column constraints, in any order: PRIMARY KEY, NOT NULL,
        // UNIQUE.
        let mut primary_key = false;
        let mut not_null = false;
        let mut unique = false;
        let mut default = None;
        let mut inline: Vec<TableConstraint> = Vec::new();
        loop {
            if self.eat_keyword(Keyword::Primary) {
                self.expect_keyword(Keyword::Key)?;
                primary_key = true;
            } else if self.eat_keyword(Keyword::Not) {
                self.expect_keyword(Keyword::Null)?;
                not_null = true;
            } else if self.eat_keyword(Keyword::Unique) {
                unique = true;
            } else if self.eat_keyword(Keyword::Default) {
                default = Some(self.parse_expr()?);
            } else if self.eat_keyword(Keyword::Check) {
                self.expect(&TokenKind::LParen)?;
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                inline.push(TableConstraint::Check(expr));
            } else if self.eat_keyword(Keyword::References) {
                // Column-level `REFERENCES parent (col)`: the referencing column
                // is this column. `parse_references` expects to consume the
                // REFERENCES keyword, so feed it back via a dedicated tail.
                inline.push(TableConstraint::ForeignKey(
                    self.parse_references_tail(name.clone())?,
                ));
            } else {
                break;
            }
        }
        Ok((
            ColumnDef {
                name,
                ty,
                primary_key,
                not_null,
                unique,
                default,
                serial,
            },
            inline,
        ))
    }

    /// Parse the `parent (col)` part of a column-level `REFERENCES`, after the
    /// `REFERENCES` keyword has already been consumed.
    fn parse_references_tail(&mut self, column: String) -> Result<ForeignKey> {
        let parent_table = self.parse_ident()?;
        self.expect(&TokenKind::LParen)?;
        let parent_column = self.parse_ident()?;
        self.expect(&TokenKind::RParen)?;
        Ok(ForeignKey {
            column,
            parent_table,
            parent_column,
        })
    }

    fn parse_create_index_tail(&mut self) -> Result<Statement> {
        let name = self.parse_ident()?;
        self.expect_keyword(Keyword::On)?;
        let table = self.parse_ident()?;
        self.expect(&TokenKind::LParen)?;
        let column = self.parse_ident()?;
        self.expect(&TokenKind::RParen)?;
        Ok(Statement::CreateIndex {
            name,
            table,
            column,
        })
    }

    fn parse_drop(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Drop)?;
        if self.eat_keyword(Keyword::View) {
            let name = self.parse_ident()?;
            return Ok(Statement::DropView { name });
        }
        self.expect_keyword(Keyword::Table)?;
        let name = self.parse_ident()?;
        Ok(Statement::DropTable { name })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.parse_ident()?;
        // The column list is optional: `INSERT INTO t VALUES (...)` inserts
        // into all columns in declaration order.
        let columns = if self.eat(&TokenKind::LParen) {
            let cols = self.parse_ident_list()?;
            self.expect(&TokenKind::RParen)?;
            cols
        } else {
            Vec::new()
        };
        self.expect_keyword(Keyword::Values)?;
        let mut rows = Vec::new();
        loop {
            self.expect(&TokenKind::LParen)?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen)?;
            rows.push(row);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let on_conflict = self.parse_on_conflict()?;
        let returning = self.parse_returning()?;
        Ok(Statement::Insert {
            table,
            columns,
            rows,
            on_conflict,
            returning,
        })
    }

    /// Parse an optional `ON CONFLICT [(cols)] DO {NOTHING | UPDATE SET ...}`
    /// clause (None if absent).
    fn parse_on_conflict(&mut self) -> Result<Option<OnConflict>> {
        if !self.eat_keyword(Keyword::On) {
            return Ok(None);
        }
        self.expect_keyword(Keyword::Conflict)?;
        // Optional conflict target: the columns whose unique constraint the
        // action arbitrates. Omitted means any unique conflict triggers it.
        let target = if self.eat(&TokenKind::LParen) {
            let cols = self.parse_ident_list()?;
            self.expect(&TokenKind::RParen)?;
            cols
        } else {
            Vec::new()
        };
        self.expect_keyword(Keyword::Do)?;
        let action = if self.eat_keyword(Keyword::Nothing) {
            ConflictAction::Nothing
        } else {
            self.expect_keyword(Keyword::Update)?;
            self.expect_keyword(Keyword::Set)?;
            let mut assignments = Vec::new();
            loop {
                let col = self.parse_ident()?;
                self.expect(&TokenKind::Eq)?;
                let val = self.parse_expr()?;
                assignments.push((col, val));
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            let where_clause = if self.eat_keyword(Keyword::Where) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            ConflictAction::Update {
                assignments,
                where_clause,
            }
        };
        Ok(Some(OnConflict { target, action }))
    }

    /// Parse an optional `RETURNING <projection>` clause (empty if absent).
    fn parse_returning(&mut self) -> Result<Vec<SelectItem>> {
        if self.eat_keyword(Keyword::Returning) {
            self.parse_projections()
        } else {
            Ok(Vec::new())
        }
    }

    fn parse_update(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Update)?;
        let table = self.parse_ident()?;
        self.expect_keyword(Keyword::Set)?;
        let mut assignments = Vec::new();
        loop {
            let col = self.parse_ident()?;
            self.expect(&TokenKind::Eq)?;
            let val = self.parse_expr()?;
            assignments.push((col, val));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        let where_clause = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Update {
            table,
            assignments,
            where_clause,
            returning,
        })
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.parse_ident()?;
        let where_clause = if self.eat_keyword(Keyword::Where) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        let returning = self.parse_returning()?;
        Ok(Statement::Delete {
            table,
            where_clause,
            returning,
        })
    }

    /// Parse a comma-separated list of bareword identifiers (at least one).
    fn parse_ident_list(&mut self) -> Result<Vec<String>> {
        let mut idents = Vec::new();
        loop {
            idents.push(self.parse_ident()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Ok(idents)
    }
}

impl Statement {
    /// Replace every positional parameter (`$N`) anywhere in the statement with
    /// `params[N-1]`, so a prepared statement bound by the wire protocol becomes
    /// an ordinary, runnable statement.
    #[must_use]
    pub fn substitute_params(&self, params: &[Value]) -> Self {
        match self {
            Self::Select(s) => Self::Select(Box::new(s.substitute_params(params))),
            Self::Insert {
                table,
                columns,
                rows,
                on_conflict,
                returning,
            } => Self::Insert {
                table: table.clone(),
                columns: columns.clone(),
                rows: rows
                    .iter()
                    .map(|r| r.iter().map(|e| e.substitute_params(params)).collect())
                    .collect(),
                on_conflict: on_conflict.as_ref().map(|oc| oc.substitute_params(params)),
                returning: returning.clone(),
            },
            Self::Update {
                table,
                assignments,
                where_clause,
                returning,
            } => Self::Update {
                table: table.clone(),
                assignments: assignments
                    .iter()
                    .map(|(c, e)| (c.clone(), e.substitute_params(params)))
                    .collect(),
                where_clause: where_clause.as_ref().map(|w| w.substitute_params(params)),
                returning: returning.clone(),
            },
            Self::Delete {
                table,
                where_clause,
                returning,
            } => Self::Delete {
                table: table.clone(),
                where_clause: where_clause.as_ref().map(|w| w.substitute_params(params)),
                returning: returning.clone(),
            },
            Self::With {
                recursive,
                ctes,
                body,
            } => Self::With {
                recursive: *recursive,
                ctes: ctes
                    .iter()
                    .map(|c| Cte {
                        name: c.name.clone(),
                        columns: c.columns.clone(),
                        query: Box::new(c.query.substitute_params(params)),
                    })
                    .collect(),
                body: Box::new(body.substitute_params(params)),
            },
            Self::Union {
                op,
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => Self::Union {
                op: *op,
                all: *all,
                left: Box::new(left.substitute_params(params)),
                right: Box::new(right.substitute_params(params)),
                order_by: order_by
                    .iter()
                    .map(|o| OrderItem {
                        expr: o.expr.substitute_params(params),
                        desc: o.desc,
                    })
                    .collect(),
                limit: *limit,
                offset: *offset,
            },
            Self::Explain { analyze, statement } => Self::Explain {
                analyze: *analyze,
                statement: Box::new(statement.substitute_params(params)),
            },
            Self::CreateView { name, query } => Self::CreateView {
                name: name.clone(),
                query: Box::new(query.substitute_params(params)),
            },
            Self::CreateTableAs { name, query } => Self::CreateTableAs {
                name: name.clone(),
                query: Box::new(query.substitute_params(params)),
            },
            // DDL and transaction control carry no expressions to bind.
            other => other.clone(),
        }
    }
}

impl Select {
    fn substitute_params(&self, params: &[Value]) -> Self {
        Self {
            distinct: self.distinct,
            projections: self
                .projections
                .iter()
                .map(|p| match p {
                    SelectItem::Star => SelectItem::Star,
                    SelectItem::Expr(e, alias) => {
                        SelectItem::Expr(e.substitute_params(params), alias.clone())
                    }
                })
                .collect(),
            from: self.from.substitute_params(params),
            joins: self
                .joins
                .iter()
                .map(|j| Join {
                    kind: j.kind,
                    table: j.table.substitute_params(params),
                    on: j.on.substitute_params(params),
                })
                .collect(),
            where_clause: self
                .where_clause
                .as_ref()
                .map(|w| w.substitute_params(params)),
            group_by: self
                .group_by
                .iter()
                .map(|g| g.substitute_params(params))
                .collect(),
            having: self.having.as_ref().map(|h| h.substitute_params(params)),
            order_by: self
                .order_by
                .iter()
                .map(|o| OrderItem {
                    expr: o.expr.substitute_params(params),
                    desc: o.desc,
                })
                .collect(),
            limit: self.limit,
            offset: self.offset,
        }
    }
}

impl TableRef {
    fn substitute_params(&self, params: &[Value]) -> Self {
        Self {
            name: self.name.clone(),
            alias: self.alias.clone(),
            subquery: self
                .subquery
                .as_ref()
                .map(|q| Box::new(q.substitute_params(params))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Statement {
        let mut p = Parser::from_sql(src).expect("lex");
        let s = p.parse_statement().expect("parse");
        assert!(p.at_eof(), "leftover tokens for {src:?}");
        s
    }

    /// Parse, print, and re-parse; the AST must be identical.
    fn round_trip(src: &str) -> Statement {
        let first = parse(src);
        let printed = first.to_string();
        let second = parse(&printed);
        assert_eq!(first, second, "round-trip mismatch: {src:?} -> {printed:?}");
        first
    }

    #[test]
    fn alter_and_truncate_round_trip() {
        round_trip("ALTER TABLE t ADD COLUMN c INT DEFAULT 0");
        round_trip("ALTER TABLE t ADD COLUMN flag BOOL NOT NULL DEFAULT TRUE");
        round_trip("TRUNCATE TABLE t");
    }

    #[test]
    fn create_table_as_round_trips() {
        round_trip("CREATE TABLE big AS SELECT id FROM t WHERE (n > 5)");
        round_trip("CREATE TABLE c AS SELECT a FROM t UNION SELECT b FROM u");
        assert!(matches!(
            parse("CREATE TABLE x AS SELECT id FROM t"),
            Statement::CreateTableAs { name, .. } if name == "x"
        ));
    }

    #[test]
    fn analyze_round_trips() {
        round_trip("ANALYZE t");
        round_trip("ANALYZE");
        assert!(matches!(
            parse("ANALYZE t"),
            Statement::Analyze { table: Some(t) } if t == "t"
        ));
        assert!(matches!(
            parse("ANALYZE"),
            Statement::Analyze { table: None }
        ));
    }

    #[test]
    fn copy_round_trips() {
        round_trip("COPY t FROM 'data.csv'");
        round_trip("COPY t TO 'out.csv' HEADER");
        assert!(matches!(
            parse("COPY t FROM 'f.csv'"),
            Statement::Copy {
                to: false,
                header: false,
                ..
            }
        ));
        assert!(matches!(
            parse("COPY t TO 'f.csv' HEADER"),
            Statement::Copy {
                to: true,
                header: true,
                ..
            }
        ));
    }

    #[test]
    fn vacuum_round_trips() {
        round_trip("VACUUM t");
        round_trip("VACUUM");
        assert!(matches!(
            parse("VACUUM orders"),
            Statement::Vacuum { table: Some(t) } if t == "orders"
        ));
        assert!(matches!(parse("VACUUM"), Statement::Vacuum { table: None }));
    }

    #[test]
    fn default_column_round_trips() {
        round_trip("CREATE TABLE t (id INT, status TEXT DEFAULT 'new', n INT DEFAULT 0)");
        round_trip("CREATE TABLE t (a INT NOT NULL DEFAULT 1, b BOOL DEFAULT TRUE)");
    }

    #[test]
    fn serial_column_round_trips() {
        // SERIAL parses to an INT column flagged auto-increment and prints back
        // as SERIAL, with the usual constraints attaching as on any column.
        let stmt = round_trip("CREATE TABLE t (id SERIAL, name TEXT)");
        let Statement::CreateTable { columns, .. } = stmt else {
            panic!("expected CREATE TABLE");
        };
        assert!(columns[0].serial, "id is serial");
        assert_eq!(columns[0].ty, DataType::Int, "serial is stored as INT");
        assert!(!columns[1].serial, "name is not serial");
        round_trip("CREATE TABLE t (id SERIAL PRIMARY KEY, name TEXT NOT NULL)");
    }

    #[test]
    fn constraint_round_trips() {
        // Column-level CHECK / REFERENCES normalize to table constraints, which
        // is the stable form the printer emits and re-parses to.
        round_trip("CREATE TABLE t (id INT, n INT CHECK (n > 0))");
        round_trip("CREATE TABLE t (lo INT, hi INT, CHECK (lo <= hi))");
        round_trip("CREATE TABLE c (id INT, pid INT REFERENCES p (id))");
        round_trip("CREATE TABLE c (id INT, pid INT, FOREIGN KEY (pid) REFERENCES p (id))");
    }

    #[test]
    fn subquery_round_trips() {
        round_trip("SELECT a FROM t WHERE x > (SELECT MAX(y) FROM u)");
        round_trip("SELECT (SELECT COUNT(*) FROM u) FROM t");
        round_trip("SELECT a FROM t WHERE x IN (SELECT y FROM u)");
        round_trip("SELECT a FROM t WHERE x NOT IN (SELECT y FROM u WHERE z > 0)");
        round_trip("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM u)");
        round_trip("SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM u WHERE u.k = 1)");
    }

    #[test]
    fn union_round_trips() {
        round_trip("SELECT a FROM t UNION SELECT b FROM u");
        round_trip("SELECT a FROM t UNION ALL SELECT b FROM u");
        // Left-associative chain.
        round_trip("SELECT a FROM t UNION SELECT b FROM u UNION ALL SELECT c FROM v");
    }

    #[test]
    fn schema_qualified_table_round_trips() {
        round_trip("SELECT table_name FROM information_schema.tables");
        round_trip(
            "SELECT t.table_name FROM information_schema.columns AS t WHERE (t.table_name = 'x')",
        );
        let s = parse("SELECT table_name FROM information_schema.tables");
        let Statement::Select(sel) = s else {
            panic!("expected SELECT");
        };
        assert_eq!(sel.from.name, "information_schema.tables");
    }

    #[test]
    fn with_cte_round_trips() {
        round_trip("WITH c AS (SELECT a FROM t) SELECT a FROM c");
        round_trip("WITH c (x, y) AS (SELECT a, b FROM t) SELECT x FROM c");
        round_trip(
            "WITH a AS (SELECT x FROM t), b AS (SELECT y FROM u) SELECT x FROM a INNER JOIN b ON a.x = b.y",
        );
        round_trip("WITH RECURSIVE c AS (SELECT 1 FROM t) SELECT a FROM c");
        let s = parse("WITH c AS (SELECT a FROM t) SELECT a FROM c");
        let Statement::With {
            recursive, ctes, ..
        } = s
        else {
            panic!("expected WITH");
        };
        assert!(!recursive);
        assert_eq!(ctes.len(), 1);
        assert_eq!(ctes[0].name, "c");
    }

    #[test]
    fn intersect_and_except_round_trip() {
        round_trip("SELECT a FROM t INTERSECT SELECT b FROM u");
        round_trip("SELECT a FROM t EXCEPT SELECT b FROM u");
        round_trip("SELECT a FROM t INTERSECT ALL SELECT b FROM u");
        round_trip("SELECT a FROM t EXCEPT ALL SELECT b FROM u");
        // A mixed chain stays left-associative at equal precedence.
        round_trip("SELECT a FROM t UNION SELECT b FROM u EXCEPT SELECT c FROM v");
        let s = parse("SELECT a FROM t INTERSECT SELECT b FROM u");
        assert!(matches!(
            s,
            Statement::Union {
                op: SetOp::Intersect,
                ..
            }
        ));
    }

    #[test]
    fn count_distinct_round_trips() {
        round_trip("SELECT COUNT(DISTINCT col) FROM t");
        round_trip("SELECT g, SUM(DISTINCT n) FROM t GROUP BY g");
    }

    #[test]
    fn view_round_trips() {
        round_trip("CREATE VIEW v AS SELECT a, b FROM t");
        round_trip("CREATE VIEW v AS SELECT a FROM t WHERE x > 0 ORDER BY a LIMIT 5");
        round_trip("CREATE VIEW v AS SELECT a FROM t UNION ALL SELECT b FROM u");
        round_trip("DROP VIEW v");
    }

    #[test]
    fn derived_table_round_trips() {
        round_trip("SELECT e.a FROM (SELECT a FROM t) AS e");
        round_trip("SELECT e.a FROM (SELECT a FROM t WHERE x > 0) AS e WHERE e.a < 10");
        round_trip("SELECT e.a, d.b FROM (SELECT a, k FROM t) AS e INNER JOIN u AS d ON e.k = d.k");
        round_trip("SELECT g.n FROM (SELECT dept, SUM(n) AS n FROM t GROUP BY dept) AS g");
    }

    #[test]
    fn concat_and_offset_round_trip() {
        round_trip("SELECT a || '-' || b FROM t");
        round_trip("SELECT id FROM t ORDER BY id LIMIT 5 OFFSET 10");
        round_trip("SELECT id FROM t OFFSET 3");
    }

    #[test]
    fn case_expression_round_trips() {
        // Searched CASE.
        round_trip("SELECT CASE WHEN n > 0 THEN 'p' WHEN n < 0 THEN 'm' ELSE 'z' END FROM t");
        // Simple CASE without ELSE.
        round_trip("SELECT CASE g WHEN 'a' THEN 1 WHEN 'b' THEN 2 END FROM t");
        // CASE in a WHERE predicate.
        round_trip("SELECT id FROM t WHERE CASE WHEN flag THEN n ELSE 0 END > 5");
    }

    #[test]
    fn create_table_single_column() {
        let s = round_trip("CREATE TABLE t (id INT)");
        assert_eq!(
            s,
            Statement::CreateTable {
                name: "t".into(),
                columns: vec![ColumnDef {
                    name: "id".into(),
                    ty: DataType::Int,
                    primary_key: false,
                    not_null: false,
                    unique: false,
                    default: None,
                    serial: false,
                }],
                constraints: vec![],
            }
        );
    }

    #[test]
    fn create_table_multi_column_with_pk() {
        let s = round_trip("CREATE TABLE parts (id INT PRIMARY KEY, name TEXT)");
        let Statement::CreateTable { name, columns, .. } = s else {
            panic!("wrong variant");
        };
        assert_eq!(name, "parts");
        assert_eq!(columns.len(), 2);
        assert!(columns[0].primary_key);
        assert_eq!(columns[1].ty, DataType::Text);
        assert!(!columns[1].primary_key);
    }

    #[test]
    fn trailing_semicolon_is_optional() {
        assert_eq!(parse("DROP TABLE t;"), parse("DROP TABLE t"));
    }

    #[test]
    fn drop_table() {
        assert_eq!(
            round_trip("DROP TABLE widgets"),
            Statement::DropTable {
                name: "widgets".into()
            }
        );
    }

    #[test]
    fn create_index() {
        let s = round_trip("CREATE INDEX idx_name ON parts (name)");
        assert_eq!(
            s,
            Statement::CreateIndex {
                name: "idx_name".into(),
                table: "parts".into(),
                column: "name".into(),
            }
        );
    }

    #[test]
    fn create_table_display_normalizes() {
        // Lowercase keywords + extra spaces normalize to canonical form.
        let printed = parse("create   table  t ( a int , b text )").to_string();
        assert_eq!(printed, "CREATE TABLE t (a INT, b TEXT)");
    }

    #[test]
    fn empty_column_list_errors() {
        let mut p = Parser::from_sql("CREATE TABLE t ()").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    #[test]
    fn unknown_type_errors() {
        let mut p = Parser::from_sql("CREATE TABLE t (a BLOB)").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    #[test]
    fn float_and_bool_column_types_parse() {
        for ty in ["FLOAT", "REAL", "DOUBLE", "BOOL", "BOOLEAN"] {
            let mut p = Parser::from_sql(&format!("CREATE TABLE t (a {ty})")).expect("lex");
            assert!(
                p.parse_statement().is_ok(),
                "type {ty} should parse as a column type"
            );
        }
    }

    #[test]
    fn missing_paren_errors() {
        let mut p = Parser::from_sql("CREATE TABLE t a INT)").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    #[test]
    fn non_statement_keyword_errors() {
        let mut p = Parser::from_sql("WHERE a = 1").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    // --- SELECT ---

    fn as_select(src: &str) -> Select {
        match round_trip(src) {
            Statement::Select(s) => *s,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    #[test]
    fn select_star() {
        let s = as_select("SELECT * FROM t");
        assert_eq!(s.projections, vec![SelectItem::Star]);
        assert_eq!(s.from.name, "t");
        assert!(s.where_clause.is_none());
    }

    #[test]
    fn select_columns() {
        let s = as_select("SELECT a, b, c FROM t");
        assert_eq!(s.projections.len(), 3);
        assert_eq!(
            s.projections[0],
            SelectItem::Expr(Expr::Column("a".into()), None)
        );
    }

    #[test]
    fn select_expr_with_alias() {
        let s = as_select("SELECT a + 1 AS x, b AS y FROM t");
        assert_eq!(
            s.projections[0],
            SelectItem::Expr(
                Expr::binary(
                    crate::ast::BinOp::Add,
                    Expr::Column("a".into()),
                    Expr::Literal(crate::ast::Value::Int(1))
                ),
                Some("x".into())
            )
        );
        assert_eq!(
            s.projections[1],
            SelectItem::Expr(Expr::Column("b".into()), Some("y".into()))
        );
    }

    #[test]
    fn select_from_with_alias() {
        let s = as_select("SELECT * FROM orders AS o");
        assert_eq!(s.from.name, "orders");
        assert_eq!(s.from.alias.as_deref(), Some("o"));
    }

    #[test]
    fn select_with_where() {
        let s = as_select("SELECT a FROM t WHERE a = 1 AND b > 2");
        assert!(s.where_clause.is_some());
        // The WHERE expression keeps its precedence.
        assert_eq!(s.where_clause.unwrap().to_string(), "((a = 1) AND (b > 2))");
    }

    #[test]
    fn select_qualified_columns() {
        let s = as_select("SELECT o.id, c.name FROM orders AS o WHERE o.id = 5");
        assert_eq!(
            s.projections[0],
            SelectItem::Expr(Expr::QualifiedColumn("o".into(), "id".into()), None)
        );
    }

    #[test]
    fn select_display_normalizes() {
        assert_eq!(
            Statement::Select(Box::new(as_select("select  a ,  b  from  t  where a=1")))
                .to_string(),
            "SELECT a, b FROM t WHERE (a = 1)"
        );
    }

    #[test]
    fn select_missing_from_errors() {
        let mut p = Parser::from_sql("SELECT a").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    // --- DML ---

    #[test]
    fn insert_single_row() {
        let s = round_trip("INSERT INTO t (a, b) VALUES (1, 'x')");
        assert_eq!(
            s,
            Statement::Insert {
                table: "t".into(),
                columns: vec!["a".into(), "b".into()],
                rows: vec![vec![
                    Expr::Literal(crate::ast::Value::Int(1)),
                    Expr::Literal(crate::ast::Value::Text("x".into())),
                ]],
                on_conflict: None,
                returning: vec![],
            }
        );
    }

    #[test]
    fn insert_multi_row() {
        let s = round_trip("INSERT INTO t (a) VALUES (1), (2), (3)");
        let Statement::Insert { rows, .. } = s else {
            panic!("wrong variant");
        };
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn update_with_where() {
        let s = round_trip("UPDATE t SET a = 1, b = a + 2 WHERE id = 5");
        let Statement::Update {
            table,
            assignments,
            where_clause,
            ..
        } = s
        else {
            panic!("wrong variant");
        };
        assert_eq!(table, "t");
        assert_eq!(assignments.len(), 2);
        assert_eq!(assignments[0].0, "a");
        assert!(where_clause.is_some());
    }

    #[test]
    fn update_without_where() {
        let s = round_trip("UPDATE t SET a = 1");
        let Statement::Update { where_clause, .. } = s else {
            panic!("wrong variant");
        };
        assert!(where_clause.is_none());
    }

    #[test]
    fn delete_with_and_without_where() {
        assert_eq!(
            round_trip("DELETE FROM t WHERE a = 1"),
            Statement::Delete {
                table: "t".into(),
                where_clause: Some(Expr::binary(
                    crate::ast::BinOp::Eq,
                    Expr::Column("a".into()),
                    Expr::Literal(crate::ast::Value::Int(1))
                )),
                returning: vec![],
            }
        );
        assert_eq!(
            round_trip("DELETE FROM t"),
            Statement::Delete {
                table: "t".into(),
                where_clause: None,
                returning: vec![],
            }
        );
    }

    #[test]
    fn returning_round_trips() {
        round_trip("INSERT INTO t (a, b) VALUES (1, 2) RETURNING a");
        round_trip("INSERT INTO t VALUES (1) RETURNING *");
        round_trip("UPDATE t SET a = 1 WHERE id = 2 RETURNING a, b AS x");
        round_trip("DELETE FROM t WHERE id = 1 RETURNING *");
    }

    #[test]
    fn dml_display_normalizes() {
        assert_eq!(
            parse("insert into t ( a , b ) values ( 1 , 2 )").to_string(),
            "INSERT INTO t (a, b) VALUES (1, 2)"
        );
        assert_eq!(
            parse("update t set a=1 where b=2").to_string(),
            "UPDATE t SET a = 1 WHERE (b = 2)"
        );
    }

    #[test]
    fn on_conflict_round_trips() {
        round_trip("INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT DO NOTHING");
        round_trip("INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT (id) DO NOTHING");
        round_trip("INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET n = 5");
        // EXCLUDED references and a guard predicate survive the round-trip, as
        // does a trailing RETURNING after the conflict clause.
        let s = round_trip(
            "INSERT INTO t (id, n) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET n = excluded.n WHERE (t.n < excluded.n) RETURNING id",
        );
        let Statement::Insert {
            on_conflict,
            returning,
            ..
        } = s
        else {
            panic!("expected INSERT");
        };
        let oc = on_conflict.expect("has ON CONFLICT");
        assert_eq!(oc.target, vec!["id".to_string()]);
        assert!(matches!(oc.action, ConflictAction::Update { .. }));
        assert_eq!(returning.len(), 1);
    }

    #[test]
    fn window_functions_round_trip() {
        round_trip("SELECT ROW_NUMBER() OVER () FROM t");
        round_trip("SELECT ROW_NUMBER() OVER (ORDER BY x) FROM t");
        round_trip("SELECT RANK() OVER (PARTITION BY g ORDER BY x DESC) FROM t");
        round_trip("SELECT g, SUM(n) OVER (PARTITION BY g) FROM t");
        round_trip("SELECT LAG(n, 1, 0) OVER (PARTITION BY g ORDER BY x) FROM t");
        // A window result nested in an arithmetic expression.
        round_trip("SELECT (ROW_NUMBER() OVER (ORDER BY x) + 1) FROM t");
    }

    #[test]
    fn explain_wraps_inner_statement() {
        let s = round_trip("EXPLAIN SELECT id FROM t WHERE id = 5");
        let Statement::Explain { analyze, statement } = s else {
            panic!("expected Explain");
        };
        assert!(!analyze);
        assert!(
            matches!(*statement, Statement::Select(_)),
            "inner must be Select"
        );
    }

    #[test]
    fn explain_analyze_round_trips() {
        let s = round_trip("EXPLAIN ANALYZE SELECT id FROM t WHERE id = 5");
        assert!(matches!(s, Statement::Explain { analyze: true, .. }));
    }

    #[test]
    fn explain_display_prefixes_keyword() {
        assert_eq!(
            parse("explain select * from t").to_string(),
            "EXPLAIN SELECT * FROM t"
        );
        assert_eq!(
            parse("explain analyze select * from t").to_string(),
            "EXPLAIN ANALYZE SELECT * FROM t"
        );
    }

    #[test]
    fn column_constraints_round_trip() {
        assert_eq!(
            parse("create table t (id int primary key, e text unique, n text not null)")
                .to_string(),
            "CREATE TABLE t (id INT PRIMARY KEY, e TEXT UNIQUE, n TEXT NOT NULL)"
        );
        assert!(matches!(
            round_trip("CREATE TABLE t (id INT PRIMARY KEY, n TEXT NOT NULL)"),
            Statement::CreateTable { .. }
        ));
    }

    #[test]
    fn transaction_control_round_trips() {
        assert!(matches!(round_trip("BEGIN"), Statement::Begin));
        assert!(matches!(round_trip("commit"), Statement::Commit));
        assert!(matches!(round_trip("ROLLBACK"), Statement::Rollback));
    }

    #[test]
    fn insert_without_column_list() {
        let s = round_trip("INSERT INTO t VALUES (1, 'x')");
        assert!(matches!(s, Statement::Insert { ref columns, .. } if columns.is_empty()));
        // No empty `()` is printed when the column list is omitted.
        assert_eq!(
            parse("insert into t values (1, 2)").to_string(),
            "INSERT INTO t VALUES (1, 2)"
        );
    }

    #[test]
    fn aggregate_functions_parse_and_round_trip() {
        // Function names canonicalize to upper-case; COUNT(*) carries a Star.
        assert_eq!(
            parse("select count(*), sum(amount) from t group by region").to_string(),
            "SELECT COUNT(*), SUM(amount) FROM t GROUP BY region"
        );
        let s = round_trip("SELECT region, COUNT(*), SUM(amount) FROM sales GROUP BY region");
        assert!(matches!(s, Statement::Select(_)));
    }

    #[test]
    fn insert_missing_values_errors() {
        let mut p = Parser::from_sql("INSERT INTO t (a)").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    #[test]
    fn update_empty_set_errors() {
        let mut p = Parser::from_sql("UPDATE t SET WHERE a = 1").expect("lex");
        assert!(p.parse_statement().is_err());
    }

    // --- SELECT extensions ---

    #[test]
    fn inner_join() {
        let s = as_select("SELECT * FROM a JOIN b ON a.id = b.aid");
        assert_eq!(s.joins.len(), 1);
        assert_eq!(s.joins[0].kind, JoinKind::Inner);
        assert_eq!(s.joins[0].table.name, "b");
    }

    #[test]
    fn left_join_and_explicit_inner() {
        let s =
            as_select("SELECT * FROM a LEFT JOIN b ON a.id = b.aid INNER JOIN c ON b.id = c.bid");
        assert_eq!(s.joins.len(), 2);
        assert_eq!(s.joins[0].kind, JoinKind::Left);
        assert_eq!(s.joins[1].kind, JoinKind::Inner);
    }

    #[test]
    fn group_by_order_by_limit() {
        let s = as_select("SELECT a FROM t GROUP BY a, b ORDER BY a DESC, b LIMIT 10");
        assert_eq!(s.group_by.len(), 2);
        assert_eq!(s.order_by.len(), 2);
        assert!(s.order_by[0].desc);
        assert!(!s.order_by[1].desc);
        assert_eq!(s.limit, Some(10));
    }

    #[test]
    fn order_by_asc_is_default() {
        let s = as_select("SELECT a FROM t ORDER BY a ASC");
        assert!(!s.order_by[0].desc);
        // ASC is not re-emitted (it is the default), but the AST is the same.
        assert_eq!(
            Statement::Select(Box::new(s)).to_string(),
            "SELECT a FROM t ORDER BY a"
        );
    }

    #[test]
    fn full_complex_query_round_trips() {
        let src = "SELECT o.id, c.name FROM orders AS o \
                   INNER JOIN customers AS c ON o.cid = c.id \
                   WHERE o.total > 100 GROUP BY c.name ORDER BY o.id DESC LIMIT 5";
        let s = round_trip(src);
        let Statement::Select(sel) = &s else {
            panic!("expected select");
        };
        assert_eq!(sel.joins.len(), 1);
        assert!(sel.where_clause.is_some());
        assert_eq!(sel.group_by.len(), 1);
        assert_eq!(sel.order_by.len(), 1);
        assert_eq!(sel.limit, Some(5));
    }

    #[test]
    fn negative_limit_errors() {
        let mut p = Parser::from_sql("SELECT a FROM t LIMIT -1").expect("lex");
        assert!(p.parse_statement().is_err());
    }
}
