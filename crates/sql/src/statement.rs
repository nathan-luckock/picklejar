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

use crate::ast::Expr;
use crate::error::{Result, SqlError};
use crate::parser::Parser;
use crate::token::{Keyword, TokenKind};

/// A reference to a table in a FROM or JOIN clause, with optional alias.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRef {
    /// Table name.
    pub name: String,
    /// Optional alias (`t` in `FROM table AS t`).
    pub alias: Option<String>,
}

impl fmt::Display for TableRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        if self.unique {
            f.write_str(" UNIQUE")?;
        }
        Ok(())
    }
}

/// A parsed SQL statement. Grows as SELECT and DML parsers land.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE name (cols...)`.
    CreateTable {
        /// Table name.
        name: String,
        /// Column definitions.
        columns: Vec<ColumnDef>,
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
    },
    /// `UPDATE t SET c = e, ... [WHERE pred]`.
    Update {
        /// Target table.
        table: String,
        /// `(column, value)` assignments.
        assignments: Vec<(String, Expr)>,
        /// Optional WHERE predicate.
        where_clause: Option<Expr>,
    },
    /// `DELETE FROM t [WHERE pred]`.
    Delete {
        /// Target table.
        table: String,
        /// Optional WHERE predicate.
        where_clause: Option<Expr>,
    },
    /// `EXPLAIN <statement>`: plan the inner statement instead of running it.
    Explain(Box<Self>),
    /// `BEGIN`: start an explicit transaction.
    Begin,
    /// `COMMIT`: commit the current transaction.
    Commit,
    /// `ROLLBACK`: abort the current transaction.
    Rollback,
    /// `left UNION [ALL] right`. `left` and `right` are queries (a `Select` or
    /// a nested `Union`). Without `all`, duplicate rows are removed. A trailing
    /// `ORDER BY` / `LIMIT` / `OFFSET` applies to the whole union and lives on
    /// the outermost node (inner nodes of a chain leave them empty).
    Union {
        /// `UNION ALL` keeps duplicates; `UNION` removes them.
        all: bool,
        /// Left query.
        left: Box<Self>,
        /// Right query.
        right: Box<Self>,
        /// ORDER BY over the union output (empty if none).
        order_by: Vec<OrderItem>,
        /// LIMIT over the union (None if none).
        limit: Option<u64>,
        /// OFFSET over the union (None if none).
        offset: Option<u64>,
    },
}

impl fmt::Display for Statement {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateTable { name, columns } => {
                write!(f, "CREATE TABLE {name} (")?;
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{c}")?;
                }
                f.write_str(")")
            }
            Self::DropTable { name } => write!(f, "DROP TABLE {name}"),
            Self::CreateIndex {
                name,
                table,
                column,
            } => write!(f, "CREATE INDEX {name} ON {table} ({column})"),
            Self::Select(s) => write!(f, "{s}"),
            Self::Insert {
                table,
                columns,
                rows,
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
                Ok(())
            }
            Self::Update {
                table,
                assignments,
                where_clause,
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
                Ok(())
            }
            Self::Delete {
                table,
                where_clause,
            } => {
                write!(f, "DELETE FROM {table}")?;
                if let Some(w) = where_clause {
                    write!(f, " WHERE {w}")?;
                }
                Ok(())
            }
            Self::Explain(inner) => write!(f, "EXPLAIN {inner}"),
            Self::Begin => f.write_str("BEGIN"),
            Self::Commit => f.write_str("COMMIT"),
            Self::Rollback => f.write_str("ROLLBACK"),
            Self::Union {
                all,
                left,
                right,
                order_by,
                limit,
                offset,
            } => {
                let kw = if *all { "UNION ALL" } else { "UNION" };
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
            TokenKind::Keyword(Keyword::Select) => self.parse_query()?,
            TokenKind::Keyword(Keyword::Insert) => self.parse_insert()?,
            TokenKind::Keyword(Keyword::Update) => self.parse_update()?,
            TokenKind::Keyword(Keyword::Delete) => self.parse_delete()?,
            TokenKind::Keyword(Keyword::Explain) => {
                self.advance();
                Statement::Explain(Box::new(self.parse_statement()?))
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
        while self.eat_keyword(Keyword::Union) {
            let all = self.eat_keyword(Keyword::All);
            let right = Statement::Select(Box::new(self.parse_select()?));
            query = Statement::Union {
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
        let mut joins = Vec::new();
        loop {
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

    fn parse_order_by(&mut self) -> Result<Vec<OrderItem>> {
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
        let name = self.parse_ident()?;
        Ok(TableRef {
            name,
            alias: self.parse_optional_alias(),
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
        } else {
            Err(SqlError::parse(
                format!(
                    "expected TABLE or INDEX after CREATE, found {:?}",
                    self.peek()
                ),
                self.span(),
            ))
        }
    }

    fn parse_create_table_tail(&mut self) -> Result<Statement> {
        let name = self.parse_ident()?;
        self.expect(&TokenKind::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.parse_column_def()?);
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
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_column_def(&mut self) -> Result<ColumnDef> {
        let name = self.parse_ident()?;
        let ty = match self.peek() {
            TokenKind::Keyword(Keyword::Int) => {
                self.advance();
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
                    format!("expected a column type (INT, FLOAT, BOOL, or TEXT), found {other:?}"),
                    self.span(),
                ));
            }
        };
        // Optional column constraints, in any order: PRIMARY KEY, NOT NULL,
        // UNIQUE.
        let mut primary_key = false;
        let mut not_null = false;
        let mut unique = false;
        loop {
            if self.eat_keyword(Keyword::Primary) {
                self.expect_keyword(Keyword::Key)?;
                primary_key = true;
            } else if self.eat_keyword(Keyword::Not) {
                self.expect_keyword(Keyword::Null)?;
                not_null = true;
            } else if self.eat_keyword(Keyword::Unique) {
                unique = true;
            } else {
                break;
            }
        }
        Ok(ColumnDef {
            name,
            ty,
            primary_key,
            not_null,
            unique,
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
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
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
        Ok(Statement::Update {
            table,
            assignments,
            where_clause,
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
        Ok(Statement::Delete {
            table,
            where_clause,
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
    fn subquery_round_trips() {
        round_trip("SELECT a FROM t WHERE x > (SELECT MAX(y) FROM u)");
        round_trip("SELECT (SELECT COUNT(*) FROM u) FROM t");
        round_trip("SELECT a FROM t WHERE x IN (SELECT y FROM u)");
        round_trip("SELECT a FROM t WHERE x NOT IN (SELECT y FROM u WHERE z > 0)");
    }

    #[test]
    fn union_round_trips() {
        round_trip("SELECT a FROM t UNION SELECT b FROM u");
        round_trip("SELECT a FROM t UNION ALL SELECT b FROM u");
        // Left-associative chain.
        round_trip("SELECT a FROM t UNION SELECT b FROM u UNION ALL SELECT c FROM v");
    }

    #[test]
    fn count_distinct_round_trips() {
        round_trip("SELECT COUNT(DISTINCT col) FROM t");
        round_trip("SELECT g, SUM(DISTINCT n) FROM t GROUP BY g");
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
                }],
            }
        );
    }

    #[test]
    fn create_table_multi_column_with_pk() {
        let s = round_trip("CREATE TABLE parts (id INT PRIMARY KEY, name TEXT)");
        let Statement::CreateTable { name, columns } = s else {
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
            }
        );
        assert_eq!(
            round_trip("DELETE FROM t"),
            Statement::Delete {
                table: "t".into(),
                where_clause: None,
            }
        );
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
    fn explain_wraps_inner_statement() {
        let s = round_trip("EXPLAIN SELECT id FROM t WHERE id = 5");
        let Statement::Explain(inner) = s else {
            panic!("expected Explain");
        };
        assert!(
            matches!(*inner, Statement::Select(_)),
            "inner must be Select"
        );
    }

    #[test]
    fn explain_display_prefixes_keyword() {
        assert_eq!(
            parse("explain select * from t").to_string(),
            "EXPLAIN SELECT * FROM t"
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
