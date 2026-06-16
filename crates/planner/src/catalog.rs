//! The catalog: schema metadata and table statistics.
//!
//! The SQL parser is deliberately schema-free, so the planner keeps a
//! catalog to resolve table and column names, find indexes, and read the
//! statistics the cost model needs (row counts and per-column estimates).
//!
//! This is an in-memory catalog. DDL statements (`CREATE TABLE`, `DROP
//! TABLE`, `CREATE INDEX`) are applied with [`Catalog::apply`]. Statistics
//! start at safe defaults and are refined by [`Catalog::set_row_count`] /
//! [`Catalog::set_column_stats`]; the executor will populate them from real
//! data in a later sprint.

use std::collections::HashMap;

use rustdb_sql::{DataType, Statement};

use crate::error::{PlanError, Result};

/// A column's name and declared type.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Declared type.
    pub ty: DataType,
    /// Whether this is the table's primary key (implies NOT NULL and UNIQUE).
    pub primary_key: bool,
    /// Whether NULL is rejected for this column.
    pub not_null: bool,
    /// Whether values must be unique across rows.
    pub unique: bool,
}

/// A single-column index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexMeta {
    /// Index name.
    pub name: String,
    /// Indexed column.
    pub column: String,
}

/// Per-column statistics used by the cost model.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ColumnStats {
    /// Number of distinct values (cardinality). Used for equality
    /// selectivity (`1 / distinct`). Never zero.
    pub distinct: u64,
}

impl Default for ColumnStats {
    fn default() -> Self {
        // A single distinct value is the most pessimistic non-degenerate
        // default: equality selectivity 1.0 (matches every row).
        Self { distinct: 1 }
    }
}

/// Table-level statistics.
#[derive(Clone, Debug, Default)]
pub struct TableStats {
    /// Estimated number of rows.
    pub row_count: u64,
    /// Per-column statistics, keyed by column name.
    pub columns: HashMap<String, ColumnStats>,
}

/// Everything the planner knows about one table.
#[derive(Clone, Debug)]
pub struct TableMeta {
    /// Table name.
    pub name: String,
    /// Columns in declaration order.
    pub columns: Vec<Column>,
    /// Indexes on this table.
    pub indexes: Vec<IndexMeta>,
    /// Statistics.
    pub stats: TableStats,
}

impl TableMeta {
    /// The ordinal position of `column`, or `None` if absent.
    #[must_use]
    pub fn column_index(&self, column: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == column)
    }

    /// True if some index covers `column`.
    #[must_use]
    pub fn index_on(&self, column: &str) -> Option<&IndexMeta> {
        self.indexes.iter().find(|i| i.column == column)
    }

    /// Stats for `column`, or the default if none recorded.
    #[must_use]
    pub fn column_stats(&self, column: &str) -> ColumnStats {
        self.stats.columns.get(column).copied().unwrap_or_default()
    }
}

/// The in-memory schema + statistics catalog.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    tables: HashMap<String, TableMeta>,
}

impl Catalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a DDL statement (`CREATE TABLE`, `DROP TABLE`, `CREATE INDEX`).
    /// Non-DDL statements are an [`PlanError::Unsupported`] error.
    pub fn apply(&mut self, stmt: &Statement) -> Result<()> {
        match stmt {
            Statement::CreateTable { name, columns } => self.create_table(name, columns),
            Statement::DropTable { name } => self.drop_table(name),
            Statement::CreateIndex {
                name,
                table,
                column,
            } => self.create_index(name, table, column),
            other => Err(PlanError::Unsupported(format!("{other}"))),
        }
    }

    fn create_table(&mut self, name: &str, columns: &[rustdb_sql::ColumnDef]) -> Result<()> {
        if self.tables.contains_key(name) {
            return Err(PlanError::TableExists(name.to_string()));
        }
        let columns = columns
            .iter()
            .map(|c| Column {
                name: c.name.clone(),
                ty: c.ty,
                primary_key: c.primary_key,
                // A primary key implies NOT NULL and UNIQUE.
                not_null: c.not_null || c.primary_key,
                unique: c.unique || c.primary_key,
            })
            .collect();
        self.tables.insert(
            name.to_string(),
            TableMeta {
                name: name.to_string(),
                columns,
                indexes: Vec::new(),
                stats: TableStats::default(),
            },
        );
        Ok(())
    }

    fn drop_table(&mut self, name: &str) -> Result<()> {
        if self.tables.remove(name).is_none() {
            return Err(PlanError::UnknownTable(name.to_string()));
        }
        Ok(())
    }

    fn create_index(&mut self, index_name: &str, table: &str, column: &str) -> Result<()> {
        let meta = self
            .tables
            .get_mut(table)
            .ok_or_else(|| PlanError::UnknownTable(table.to_string()))?;
        if meta.column_index(column).is_none() {
            return Err(PlanError::IndexUnknownColumn {
                table: table.to_string(),
                column: column.to_string(),
            });
        }
        meta.indexes.push(IndexMeta {
            name: index_name.to_string(),
            column: column.to_string(),
        });
        Ok(())
    }

    /// Look up a table.
    #[must_use]
    pub fn get_table(&self, name: &str) -> Option<&TableMeta> {
        self.tables.get(name)
    }

    /// Resolve `table.column` to the column's ordinal, erroring if either is
    /// unknown.
    pub fn column_index(&self, table: &str, column: &str) -> Result<usize> {
        let meta = self
            .get_table(table)
            .ok_or_else(|| PlanError::UnknownTable(table.to_string()))?;
        meta.column_index(column)
            .ok_or_else(|| PlanError::UnknownColumn {
                table: table.to_string(),
                column: column.to_string(),
            })
    }

    /// Set a table's estimated row count.
    pub fn set_row_count(&mut self, table: &str, rows: u64) -> Result<()> {
        let meta = self
            .tables
            .get_mut(table)
            .ok_or_else(|| PlanError::UnknownTable(table.to_string()))?;
        meta.stats.row_count = rows;
        Ok(())
    }

    /// Set a column's statistics.
    pub fn set_column_stats(
        &mut self,
        table: &str,
        column: &str,
        stats: ColumnStats,
    ) -> Result<()> {
        let meta = self
            .tables
            .get_mut(table)
            .ok_or_else(|| PlanError::UnknownTable(table.to_string()))?;
        if meta.column_index(column).is_none() {
            return Err(PlanError::UnknownColumn {
                table: table.to_string(),
                column: column.to_string(),
            });
        }
        meta.stats.columns.insert(column.to_string(), stats);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustdb_sql::Parser;

    fn ddl(src: &str) -> Statement {
        Parser::from_sql(src)
            .expect("lex")
            .parse_statement()
            .expect("parse")
    }

    fn catalog_with(stmts: &[&str]) -> Catalog {
        let mut c = Catalog::new();
        for s in stmts {
            c.apply(&ddl(s)).expect("apply");
        }
        c
    }

    #[test]
    fn create_table_registers_columns() {
        let c = catalog_with(&["CREATE TABLE t (id INT PRIMARY KEY, name TEXT)"]);
        let t = c.get_table("t").expect("table");
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.column_index("id"), Some(0));
        assert_eq!(t.column_index("name"), Some(1));
        assert!(t.columns[0].primary_key);
        assert_eq!(t.columns[1].ty, DataType::Text);
    }

    #[test]
    fn duplicate_table_errors() {
        let mut c = catalog_with(&["CREATE TABLE t (a INT)"]);
        let err = c.apply(&ddl("CREATE TABLE t (b INT)")).expect_err("dup");
        assert!(matches!(err, PlanError::TableExists(n) if n == "t"));
    }

    #[test]
    fn drop_table_removes_it() {
        let mut c = catalog_with(&["CREATE TABLE t (a INT)"]);
        c.apply(&ddl("DROP TABLE t")).expect("drop");
        assert!(c.get_table("t").is_none());
    }

    #[test]
    fn drop_unknown_table_errors() {
        let mut c = Catalog::new();
        let err = c.apply(&ddl("DROP TABLE ghost")).expect_err("err");
        assert!(matches!(err, PlanError::UnknownTable(n) if n == "ghost"));
    }

    #[test]
    fn create_index_attaches_to_table() {
        let c = catalog_with(&[
            "CREATE TABLE parts (id INT, name TEXT)",
            "CREATE INDEX idx_id ON parts (id)",
        ]);
        let t = c.get_table("parts").expect("table");
        assert_eq!(t.indexes.len(), 1);
        assert!(t.index_on("id").is_some());
        assert!(t.index_on("name").is_none());
    }

    #[test]
    fn index_on_missing_table_errors() {
        let mut c = Catalog::new();
        let err = c
            .apply(&ddl("CREATE INDEX i ON nope (x)"))
            .expect_err("err");
        assert!(matches!(err, PlanError::UnknownTable(n) if n == "nope"));
    }

    #[test]
    fn index_on_missing_column_errors() {
        let mut c = catalog_with(&["CREATE TABLE t (a INT)"]);
        let err = c
            .apply(&ddl("CREATE INDEX i ON t (missing)"))
            .expect_err("err");
        assert!(matches!(
            err,
            PlanError::IndexUnknownColumn { table, column } if table == "t" && column == "missing"
        ));
    }

    #[test]
    fn column_index_resolves_or_errors() {
        let c = catalog_with(&["CREATE TABLE t (a INT, b INT)"]);
        assert_eq!(c.column_index("t", "b").expect("resolve"), 1);
        assert!(matches!(
            c.column_index("t", "z").expect_err("err"),
            PlanError::UnknownColumn { .. }
        ));
        assert!(matches!(
            c.column_index("ghost", "a").expect_err("err"),
            PlanError::UnknownTable(_)
        ));
    }

    #[test]
    fn stats_default_and_set() {
        let mut c = catalog_with(&["CREATE TABLE t (a INT)"]);
        // Defaults.
        let t = c.get_table("t").expect("table");
        assert_eq!(t.stats.row_count, 0);
        assert_eq!(t.column_stats("a"), ColumnStats { distinct: 1 });
        // Set.
        c.set_row_count("t", 1000).expect("rows");
        c.set_column_stats("t", "a", ColumnStats { distinct: 500 })
            .expect("stats");
        let t = c.get_table("t").expect("table");
        assert_eq!(t.stats.row_count, 1000);
        assert_eq!(t.column_stats("a").distinct, 500);
    }

    #[test]
    fn applying_non_ddl_is_unsupported() {
        let mut c = Catalog::new();
        let err = c.apply(&ddl("SELECT a FROM t")).expect_err("unsupported");
        assert!(matches!(err, PlanError::Unsupported(_)));
    }
}
