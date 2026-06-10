//! SQL lexer and parser, hand-written.
//!
//! No `sqlparser-rs`. The point of the project is to write this from scratch.
//! Target dialect: a meaningful subset — SELECT, INSERT, UPDATE, DELETE,
//! CREATE TABLE, JOIN, WHERE, GROUP BY, ORDER BY, LIMIT.
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
    ColumnDef, DataType, Join, JoinKind, OrderItem, Select, SelectItem, Statement, TableRef,
};
pub use token::{Keyword, Span, Token, TokenKind};
