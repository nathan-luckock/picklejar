//! Statement AST nodes and their parsers.
//!
//! Built on top of the expression core in [`crate::parser`]. Each statement
//! type's parser is a method on [`Parser`] added alongside its AST node.
//! This commit covers DDL (CREATE TABLE, DROP TABLE, CREATE INDEX); SELECT
//! and DML follow.
//!
//! Every node implements `Display` back to canonical SQL, which doubles as a
//! normalizer and is the oracle for the parser round-trip property test.

use std::fmt;

use crate::error::{Result, SqlError};
use crate::parser::Parser;
use crate::token::{Keyword, TokenKind};

/// A column data type.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DataType {
    /// 64-bit signed integer.
    Int,
    /// Variable-length text.
    Text,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Int => "INT",
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
    /// Whether the column is the primary key.
    pub primary_key: bool,
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.ty)?;
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
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
}

impl fmt::Display for Statement {
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
        }
    }
}

impl Parser {
    /// Parse a single statement, consuming an optional trailing semicolon.
    pub fn parse_statement(&mut self) -> Result<Statement> {
        let stmt = match self.peek() {
            TokenKind::Keyword(Keyword::Create) => self.parse_create()?,
            TokenKind::Keyword(Keyword::Drop) => self.parse_drop()?,
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
            TokenKind::Keyword(Keyword::Text) => {
                self.advance();
                DataType::Text
            }
            other => {
                return Err(SqlError::parse(
                    format!("expected a column type (INT or TEXT), found {other:?}"),
                    self.span(),
                ));
            }
        };
        // Optional PRIMARY KEY.
        let primary_key = if self.eat_keyword(Keyword::Primary) {
            self.expect_keyword(Keyword::Key)?;
            true
        } else {
            false
        };
        Ok(ColumnDef {
            name,
            ty,
            primary_key,
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
        let mut p = Parser::from_sql("CREATE TABLE t (a FLOAT)").expect("lex");
        assert!(p.parse_statement().is_err());
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
}
