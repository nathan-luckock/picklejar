//! SQL lexer.
//!
//! Turns a SQL string into a flat `Vec<Token>` ending in [`TokenKind::Eof`].
//! No regex, no dependencies: a single forward pass over the bytes.
//!
//! - Whitespace and `-- line comments` are skipped.
//! - Keywords are matched case-insensitively; identifiers keep their case.
//! - String literals are single-quoted, with `''` as the escape for a quote.

use crate::error::{Result, SqlError};
use crate::token::{Keyword, Span, Token, TokenKind};

/// A streaming tokenizer over a SQL source string.
#[derive(Debug)]
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    /// Create a lexer over `src`.
    #[must_use]
    pub const fn new(src: &'a str) -> Self {
        Self {
            src,
            bytes: src.as_bytes(),
            pos: 0,
        }
    }

    /// Tokenize the entire input. The returned vector always ends with an
    /// [`TokenKind::Eof`] token.
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.pos;
            let Some(c) = self.peek() else {
                tokens.push(Token::new(TokenKind::Eof, Span::new(start, start)));
                return Ok(tokens);
            };
            let kind = match c {
                b'(' => self.single(TokenKind::LParen),
                b')' => self.single(TokenKind::RParen),
                b',' => self.single(TokenKind::Comma),
                b';' => self.single(TokenKind::Semicolon),
                b'.' => self.single(TokenKind::Dot),
                b'+' => self.single(TokenKind::Plus),
                b'-' => self.dash(),
                b'*' => self.single(TokenKind::Star),
                b'/' => self.single(TokenKind::Slash),
                b'=' => self.single(TokenKind::Eq),
                b':' => self.colon()?,
                b'<' => self.less(),
                b'>' => self.greater(),
                b'!' => self.bang()?,
                b'|' => self.pipe()?,
                b'\'' => self.string()?,
                b'$' => self.param()?,
                b'0'..=b'9' => self.number()?,
                c if is_ident_start(c) => self.word(),
                other => {
                    return Err(SqlError::UnexpectedChar {
                        ch: other as char,
                        pos: start,
                    });
                }
            };
            tokens.push(Token::new(kind, Span::new(start, self.pos)));
        }
    }

    // --- character helpers ---

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.bump();
        kind
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => self.bump(),
                // Line comment: -- to end of line.
                Some(b'-') if self.peek2() == Some(b'-') => {
                    while let Some(c) = self.peek() {
                        self.bump();
                        if c == b'\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    // --- token scanners ---

    /// `-` is subtraction / unary minus, `->` is JSON get, `->>` is JSON get as
    /// text. (Line comments `--` are removed earlier by `skip_trivia`.)
    fn dash(&mut self) -> TokenKind {
        self.bump();
        if self.peek() == Some(b'>') {
            self.bump();
            if self.peek() == Some(b'>') {
                self.bump();
                TokenKind::ArrowText
            } else {
                TokenKind::Arrow
            }
        } else {
            TokenKind::Minus
        }
    }

    /// `::` is the cast operator; a lone `:` is not valid in this dialect.
    fn colon(&mut self) -> Result<TokenKind> {
        let start = self.pos;
        self.bump();
        if self.peek() == Some(b':') {
            self.bump();
            Ok(TokenKind::ColonColon)
        } else {
            Err(SqlError::UnexpectedChar {
                ch: ':',
                pos: start,
            })
        }
    }

    /// `<` is less-than; `<=` less-or-equal; `<>` not-equal. The vector distance
    /// operators share the `<` prefix: `<->` (L2), `<=>` (cosine), `<#>` (negative
    /// inner product), `<+>` (L1 / taxicab), each requiring the trailing `>`.
    fn less(&mut self) -> TokenKind {
        self.bump();
        match self.peek() {
            Some(b'=') => {
                self.bump();
                // `<=>` is cosine distance; a lone `<=` is less-or-equal.
                if self.peek() == Some(b'>') {
                    self.bump();
                    TokenKind::VecCosine
                } else {
                    TokenKind::LtEq
                }
            }
            Some(b'>') => {
                self.bump();
                TokenKind::NotEq
            }
            Some(b'-') if self.peek2() == Some(b'>') => {
                self.bump();
                self.bump();
                TokenKind::VecL2
            }
            Some(b'#') if self.peek2() == Some(b'>') => {
                self.bump();
                self.bump();
                TokenKind::VecInner
            }
            Some(b'+') if self.peek2() == Some(b'>') => {
                self.bump();
                self.bump();
                TokenKind::VecL1
            }
            _ => TokenKind::Lt,
        }
    }

    fn greater(&mut self) -> TokenKind {
        self.bump();
        if self.peek() == Some(b'=') {
            self.bump();
            TokenKind::GtEq
        } else {
            TokenKind::Gt
        }
    }

    fn bang(&mut self) -> Result<TokenKind> {
        let pos = self.pos;
        self.bump();
        if self.peek() == Some(b'=') {
            self.bump();
            Ok(TokenKind::NotEq)
        } else {
            Err(SqlError::UnexpectedChar { ch: '!', pos })
        }
    }

    fn pipe(&mut self) -> Result<TokenKind> {
        let pos = self.pos;
        self.bump();
        if self.peek() == Some(b'|') {
            self.bump();
            Ok(TokenKind::Concat)
        } else {
            // A single `|` is not an operator in this dialect.
            Err(SqlError::UnexpectedChar { ch: '|', pos })
        }
    }

    fn string(&mut self) -> Result<TokenKind> {
        let open = self.pos;
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(SqlError::UnterminatedString { pos: open }),
                Some(b'\'') => {
                    self.bump();
                    // '' is an escaped single quote.
                    if self.peek() == Some(b'\'') {
                        out.push('\'');
                        self.bump();
                    } else {
                        return Ok(TokenKind::Str(out));
                    }
                }
                Some(_) => {
                    // Copy one UTF-8 char.
                    let ch = self.src[self.pos..].chars().next().expect("non-empty");
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Lex a positional parameter `$N`. The `$` is consumed here, then the
    /// digits. A `$` with no following digit is an error.
    fn param(&mut self) -> Result<TokenKind> {
        self.bump(); // consume '$'
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
        let text = &self.src[start..self.pos];
        let n = text.parse::<u32>().map_err(|_| SqlError::UnexpectedChar {
            ch: '$',
            pos: start,
        })?;
        Ok(TokenKind::Param(n))
    }

    fn number(&mut self) -> Result<TokenKind> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
        }
        // A fractional part (a `.` followed by at least one digit) makes this a
        // float. A bare trailing `.` is left for the `Dot` token, so `t.col`
        // still lexes as ident-dot-ident.
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek2(), Some(b'0'..=b'9')) {
            is_float = true;
            self.bump(); // consume '.'
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.bump();
            }
        }
        let text = &self.src[start..self.pos];
        if is_float {
            let value = text
                .parse::<f64>()
                .map_err(|_| SqlError::IntOverflow { pos: start })?;
            Ok(TokenKind::Float(value))
        } else {
            let value = text
                .parse::<i64>()
                .map_err(|_| SqlError::IntOverflow { pos: start })?;
            Ok(TokenKind::Int(value))
        }
    }

    fn word(&mut self) -> TokenKind {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if is_ident_continue(c)) {
            self.bump();
        }
        let word = &self.src[start..self.pos];
        Keyword::from_word(word)
            .map_or_else(|| TokenKind::Ident(word.to_string()), TokenKind::Keyword)
    }
}

const fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

const fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src)
            .tokenize()
            .expect("tokenize")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn empty_input_is_just_eof() {
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
        assert_eq!(kinds("   \n\t "), vec![TokenKind::Eof]);
    }

    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            kinds("SELECT select SeLeCt"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn identifiers_keep_case() {
        assert_eq!(
            kinds("MyTable col_2"),
            vec![
                TokenKind::Ident("MyTable".into()),
                TokenKind::Ident("col_2".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn integers() {
        assert_eq!(
            kinds("0 42 1000000"),
            vec![
                TokenKind::Int(0),
                TokenKind::Int(42),
                TokenKind::Int(1_000_000),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn strings_with_escape() {
        assert_eq!(
            kinds("'hello' 'it''s' ''"),
            vec![
                TokenKind::Str("hello".into()),
                TokenKind::Str("it's".into()),
                TokenKind::Str(String::new()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn operators_and_punctuation() {
        assert_eq!(
            kinds("= <> != < <= > >= + - * / ( ) , ; ."),
            vec![
                TokenKind::Eq,
                TokenKind::NotEq,
                TokenKind::NotEq,
                TokenKind::Lt,
                TokenKind::LtEq,
                TokenKind::Gt,
                TokenKind::GtEq,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::Comma,
                TokenKind::Semicolon,
                TokenKind::Dot,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn line_comments_are_skipped() {
        assert_eq!(
            kinds("SELECT -- this is a comment\n42"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Int(42),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn a_full_statement_tokenizes() {
        let toks = kinds("SELECT a, b FROM t WHERE a = 1;");
        assert_eq!(
            toks,
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Ident("a".into()),
                TokenKind::Comma,
                TokenKind::Ident("b".into()),
                TokenKind::Keyword(Keyword::From),
                TokenKind::Ident("t".into()),
                TokenKind::Keyword(Keyword::Where),
                TokenKind::Ident("a".into()),
                TokenKind::Eq,
                TokenKind::Int(1),
                TokenKind::Semicolon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn qualified_column_dots() {
        assert_eq!(
            kinds("t.col"),
            vec![
                TokenKind::Ident("t".into()),
                TokenKind::Dot,
                TokenKind::Ident("col".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn spans_point_at_source() {
        let toks = Lexer::new("SELECT  x").tokenize().expect("lex");
        // "SELECT" spans 0..6; "x" spans 8..9.
        assert_eq!(toks[0].span, Span::new(0, 6));
        assert_eq!(toks[1].span, Span::new(8, 9));
    }

    #[test]
    fn unterminated_string_errors() {
        let err = Lexer::new("'oops").tokenize().expect_err("must error");
        assert!(matches!(err, SqlError::UnterminatedString { pos: 0 }));
    }

    #[test]
    fn unexpected_char_errors() {
        let err = Lexer::new("a @ b").tokenize().expect_err("must error");
        assert!(matches!(err, SqlError::UnexpectedChar { ch: '@', pos: 2 }));
    }

    #[test]
    fn bare_bang_errors() {
        let err = Lexer::new("a ! b").tokenize().expect_err("must error");
        assert!(matches!(err, SqlError::UnexpectedChar { ch: '!', .. }));
    }

    #[test]
    fn type_keywords() {
        assert_eq!(
            kinds("INT TEXT integer varchar"),
            vec![
                TokenKind::Keyword(Keyword::Int),
                TokenKind::Keyword(Keyword::Text),
                TokenKind::Keyword(Keyword::Int),
                TokenKind::Keyword(Keyword::Text),
                TokenKind::Eof,
            ]
        );
    }
}
