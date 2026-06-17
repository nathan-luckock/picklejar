//! SQL lexer and parser.
//!
//! No `sqlparser-rs`; the parser is implemented in this crate. Target dialect:
//! a meaningful subset - SELECT, INSERT, UPDATE, DELETE, CREATE TABLE, JOIN,
//! WHERE, GROUP BY, ORDER BY, LIMIT.
//!
//! # Sprint 7 surface
//!
//! - [`lexer::Lexer`] turns SQL text into [`token::Token`]s.
//! - The AST and recursive-descent parser land in subsequent commits.

#![forbid(unsafe_code)]

pub mod ast;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod statement;
pub mod token;

pub use ast::{BinOp, Expr, UnOp, Value};
pub use error::{Result, SqlError};
pub use lexer::Lexer;
pub use parser::Parser;
pub use statement::{
    ColumnDef, Cte, DataType, ForeignKey, Join, JoinKind, OrderItem, Select, SelectItem, SetOp,
    Statement, TableConstraint, TableRef,
};
pub use token::{Keyword, Span, Token, TokenKind};
