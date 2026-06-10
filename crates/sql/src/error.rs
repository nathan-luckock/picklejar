//! Errors for the SQL lexer and parser.

use crate::token::Span;

/// A lex or parse error with the byte position where it occurred.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SqlError {
    /// The lexer hit a character it does not understand.
    #[error("unexpected character {ch:?} at byte {pos}")]
    UnexpectedChar {
        /// The offending character.
        ch: char,
        /// Byte offset.
        pos: usize,
    },

    /// A string literal was never closed.
    #[error("unterminated string literal starting at byte {pos}")]
    UnterminatedString {
        /// Byte offset of the opening quote.
        pos: usize,
    },

    /// An integer literal did not fit in an `i64`.
    #[error("integer literal at byte {pos} is out of range")]
    IntOverflow {
        /// Byte offset.
        pos: usize,
    },

    /// The parser expected something else.
    #[error("parse error at byte {}: {message}", span.start)]
    Parse {
        /// Human-readable description (what was expected, what was found).
        message: String,
        /// Where it happened.
        span: Span,
    },
}

impl SqlError {
    /// Build a parse error.
    #[must_use]
    pub fn parse(message: impl Into<String>, span: Span) -> Self {
        Self::Parse {
            message: message.into(),
            span,
        }
    }
}

/// Result alias for the SQL crate.
pub type Result<T> = std::result::Result<T, SqlError>;
