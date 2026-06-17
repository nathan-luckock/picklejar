//! Recursive-descent parser with a Pratt expression core.
//!
//! Statement parsing (DDL / SELECT / DML) is layered on in later commits;
//! this module provides the [`Parser`] cursor over the token stream and the
//! precedence-climbing [`Parser::parse_expr`].
//!
//! # Precedence (lowest binds loosest)
//!
//! `OR` < `AND` < `NOT` < comparison (`= <> < <= > >=`) < `+ -` < `* /` <
//! unary `-` < atoms. Binary operators are left-associative.
//!
//! `NOT` sits between `AND` and comparison, matching SQL: `NOT a = b` parses
//! as `NOT (a = b)`, and `NOT a AND b` as `(NOT a) AND b`.

use crate::ast::{BinOp, Expr, UnOp, Value};
use crate::error::{Result, SqlError};
use crate::token::{Keyword, Span, Token, TokenKind};

/// A cursor over a token slice with the shared parsing helpers.
#[derive(Debug)]
pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    /// Build a parser over an already-tokenized statement. The token vector
    /// is expected to end in [`TokenKind::Eof`].
    #[must_use]
    pub const fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Tokenize `src` and build a parser over it.
    pub fn from_sql(src: &str) -> Result<Self> {
        let tokens = crate::lexer::Lexer::new(src).tokenize()?;
        Ok(Self::new(tokens))
    }

    // --- cursor helpers (shared by all statement parsers) ---

    /// The current token's kind.
    #[must_use]
    pub fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    /// The current token (kind + span).
    #[must_use]
    pub fn peek_token(&self) -> &Token {
        &self.tokens[self.pos]
    }

    /// The current token's span.
    #[must_use]
    pub fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    /// True if the parser is at end of input.
    #[must_use]
    pub fn at_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    /// Consume and return the current token.
    pub fn advance(&mut self) -> Token {
        let tok = self.tokens[self.pos].clone();
        if !matches!(tok.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        tok
    }

    /// Consume the current token if its kind equals `kind`.
    pub fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek() == kind {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the current token if it is the given keyword.
    pub fn eat_keyword(&mut self, kw: Keyword) -> bool {
        if matches!(self.peek(), TokenKind::Keyword(k) if *k == kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume a bareword that matches `word` case-insensitively, for
    /// context-sensitive keywords (e.g. `TO` / `HEADER` in `COPY`) that are not
    /// reserved and so stay usable as ordinary identifiers.
    pub(crate) fn eat_ident_kw(&mut self, word: &str) -> bool {
        if matches!(self.peek(), TokenKind::Ident(s) if s.eq_ignore_ascii_case(word)) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Require the current token to equal `kind`, consuming it.
    pub fn expect(&mut self, kind: &TokenKind) -> Result<Token> {
        if self.peek() == kind {
            Ok(self.advance())
        } else {
            Err(SqlError::parse(
                format!("expected {kind:?}, found {:?}", self.peek()),
                self.span(),
            ))
        }
    }

    /// Require the given keyword, consuming it.
    pub fn expect_keyword(&mut self, kw: Keyword) -> Result<()> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(SqlError::parse(
                format!("expected keyword {kw:?}, found {:?}", self.peek()),
                self.span(),
            ))
        }
    }

    /// Parse a bareword identifier, returning its text.
    pub fn parse_ident(&mut self) -> Result<String> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            other => Err(SqlError::parse(
                format!("expected identifier, found {other:?}"),
                self.span(),
            )),
        }
    }

    /// Parse a single-quoted string literal, returning its (unescaped) text.
    pub(crate) fn parse_string(&mut self) -> Result<String> {
        match self.peek().clone() {
            TokenKind::Str(s) => {
                self.advance();
                Ok(s)
            }
            other => Err(SqlError::parse(
                format!("expected a string literal, found {other:?}"),
                self.span(),
            )),
        }
    }

    // --- Pratt expression parser ---

    /// Parse a full scalar expression.
    pub fn parse_expr(&mut self) -> Result<Expr> {
        self.parse_bp(0)
    }

    /// Precedence-climbing core. `min_bp` is the minimum left binding power
    /// an infix operator must have to be consumed at this level.
    fn parse_bp(&mut self, min_bp: u8) -> Result<Expr> {
        let mut lhs = self.parse_prefix()?;
        loop {
            // Keyword predicates (IN, BETWEEN, LIKE, IS [NOT] NULL), optionally
            // negated, bind at comparison precedence.
            if COMPARISON_BP >= min_bp {
                if let Some(expr) = self.try_keyword_predicate(&lhs)? {
                    lhs = expr;
                    continue;
                }
            }
            let Some((op, l_bp, r_bp)) = infix_binding_power(self.peek()) else {
                break;
            };
            if l_bp < min_bp {
                break;
            }
            self.advance(); // consume the operator
            let rhs = self.parse_bp(r_bp)?;
            lhs = Expr::binary(op, lhs, rhs);
        }
        Ok(lhs)
    }

    /// If the next token introduces a keyword predicate applied to `lhs`
    /// (`IN`, `BETWEEN`, `LIKE`, each optionally preceded by `NOT`, or `IS
    /// [NOT] NULL`), consume it and return the resulting expression. Otherwise
    /// returns `None` and consumes nothing.
    fn try_keyword_predicate(&mut self, lhs: &Expr) -> Result<Option<Expr>> {
        let negated = matches!(self.peek(), TokenKind::Keyword(Keyword::Not));
        let kw = if negated {
            // `NOT IN` / `NOT BETWEEN` / `NOT LIKE`: peek past the NOT.
            match self.tokens.get(self.pos + 1).map(|t| &t.kind) {
                Some(TokenKind::Keyword(k @ (Keyword::In | Keyword::Between | Keyword::Like))) => {
                    *k
                }
                _ => return Ok(None),
            }
        } else {
            match self.peek() {
                TokenKind::Keyword(
                    k @ (Keyword::In | Keyword::Between | Keyword::Like | Keyword::Is),
                ) => *k,
                _ => return Ok(None),
            }
        };
        if negated {
            self.advance(); // consume NOT
        }
        self.advance(); // consume the predicate keyword
        let expr = match kw {
            Keyword::In => self.parse_in(lhs, negated)?,
            Keyword::Between => self.parse_between(lhs, negated)?,
            Keyword::Like => self.parse_like(lhs.clone(), negated)?,
            Keyword::Is => self.parse_is_null(lhs.clone())?,
            _ => unreachable!("guarded by the match above"),
        };
        Ok(Some(expr))
    }

    /// `x [NOT] IN (a, b, ...)`, desugared to an OR of equalities (or, when
    /// negated, an AND of inequalities).
    fn parse_in(&mut self, lhs: &Expr, negated: bool) -> Result<Expr> {
        self.expect(&TokenKind::LParen)?;
        // `x [NOT] IN (SELECT ...)`: a subquery membership test.
        if matches!(self.peek(), TokenKind::Keyword(Keyword::Select)) {
            let query = self.parse_query()?;
            self.expect(&TokenKind::RParen)?;
            return Ok(Expr::InSubquery {
                expr: Box::new(lhs.clone()),
                query: Box::new(query),
                negated,
            });
        }
        let mut items = vec![self.parse_expr()?];
        while self.eat(&TokenKind::Comma) {
            items.push(self.parse_expr()?);
        }
        self.expect(&TokenKind::RParen)?;
        let (cmp, join) = if negated {
            (BinOp::Ne, BinOp::And)
        } else {
            (BinOp::Eq, BinOp::Or)
        };
        let mut iter = items.into_iter();
        let first = iter.next().expect("IN list is never empty");
        let mut acc = Expr::binary(cmp, lhs.clone(), first);
        for item in iter {
            acc = Expr::binary(join, acc, Expr::binary(cmp, lhs.clone(), item));
        }
        Ok(acc)
    }

    /// `x [NOT] BETWEEN low AND high`, desugared to a pair of comparisons.
    fn parse_between(&mut self, lhs: &Expr, negated: bool) -> Result<Expr> {
        // Parse the bounds above AND's binding power so the `AND` separator and
        // any trailing `AND` terminate them.
        let low = self.parse_bp(COMPARISON_BP + 1)?;
        self.expect_keyword(Keyword::And)?;
        let high = self.parse_bp(COMPARISON_BP + 1)?;
        if negated {
            Ok(Expr::binary(
                BinOp::Or,
                Expr::binary(BinOp::Lt, lhs.clone(), low),
                Expr::binary(BinOp::Gt, lhs.clone(), high),
            ))
        } else {
            Ok(Expr::binary(
                BinOp::And,
                Expr::binary(BinOp::Ge, lhs.clone(), low),
                Expr::binary(BinOp::Le, lhs.clone(), high),
            ))
        }
    }

    /// `x [NOT] LIKE pattern`, a `Like` comparison wrapped in `NOT` if negated.
    fn parse_like(&mut self, lhs: Expr, negated: bool) -> Result<Expr> {
        let pattern = self.parse_bp(COMPARISON_BP + 1)?;
        let like = Expr::binary(BinOp::Like, lhs, pattern);
        Ok(if negated {
            Expr::unary(UnOp::Not, like)
        } else {
            like
        })
    }

    /// `x IS [NOT] NULL` (the `IS` is already consumed).
    fn parse_is_null(&mut self, lhs: Expr) -> Result<Expr> {
        let negated = self.eat_keyword(Keyword::Not);
        self.expect_keyword(Keyword::Null)?;
        let op = if negated {
            UnOp::IsNotNull
        } else {
            UnOp::IsNull
        };
        Ok(Expr::unary(op, lhs))
    }

    /// Parse a function-call argument list (the inside of the parentheses),
    /// up to but not including the closing `)`. Comma-separated expressions;
    /// empty for `f()`. `COUNT(*)` arrives as a single [`Expr::Star`].
    fn parse_call_args(&mut self) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        if matches!(self.peek(), TokenKind::RParen) {
            return Ok(args);
        }
        args.push(self.parse_expr()?);
        while self.eat(&TokenKind::Comma) {
            args.push(self.parse_expr()?);
        }
        Ok(args)
    }

    /// Parse a window's `( [PARTITION BY ...] [ORDER BY ...] )`, with the
    /// `OVER` keyword already consumed. Returns the partition keys and the
    /// in-window order items (either may be empty).
    fn parse_window_over(&mut self) -> Result<(Vec<Expr>, Vec<crate::statement::OrderItem>)> {
        self.expect(&TokenKind::LParen)?;
        let partition_by = if self.eat_keyword(Keyword::Partition) {
            self.expect_keyword(Keyword::By)?;
            let mut keys = Vec::new();
            loop {
                keys.push(self.parse_expr()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            keys
        } else {
            Vec::new()
        };
        let order_by = self.parse_order_by()?;
        self.expect(&TokenKind::RParen)?;
        Ok((partition_by, order_by))
    }

    /// Parse a `CASE` expression (the `CASE` keyword is already consumed).
    /// Supports both the searched form (`CASE WHEN cond THEN r ... END`) and
    /// the simple form (`CASE x WHEN v THEN r ... END`).
    fn parse_case(&mut self) -> Result<Expr> {
        // A simple-CASE operand is present when a value, not `WHEN`, follows.
        let operand = if matches!(self.peek(), TokenKind::Keyword(Keyword::When)) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_keyword(Keyword::When) {
            let when = self.parse_expr()?;
            self.expect_keyword(Keyword::Then)?;
            let then = self.parse_expr()?;
            whens.push((when, then));
        }
        if whens.is_empty() {
            return Err(SqlError::parse(
                "CASE requires at least one WHEN branch".to_string(),
                self.span(),
            ));
        }
        let else_result = if self.eat_keyword(Keyword::Else) {
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect_keyword(Keyword::End)?;
        Ok(Expr::Case {
            operand,
            whens,
            else_result,
        })
    }

    /// Parse a prefix position: literals, columns, parens, and the prefix
    /// operators `NOT` and unary `-`.
    fn parse_prefix(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            TokenKind::Keyword(Keyword::Not) => {
                self.advance();
                // NOT binds looser than comparison: its operand is parsed at
                // comparison binding power so `NOT a = b` is `NOT (a = b)`.
                let operand = self.parse_bp(NOT_OPERAND_BP)?;
                Ok(Expr::unary(UnOp::Not, operand))
            }
            TokenKind::Minus => {
                self.advance();
                // Unary minus binds tighter than any binary operator.
                let operand = self.parse_bp(NEG_OPERAND_BP)?;
                Ok(Expr::unary(UnOp::Neg, operand))
            }
            TokenKind::LParen => {
                self.advance();
                if matches!(self.peek(), TokenKind::Keyword(Keyword::Select)) {
                    // A parenthesized query used as a scalar value.
                    let query = self.parse_query()?;
                    self.expect(&TokenKind::RParen)?;
                    Ok(Expr::Subquery(Box::new(query)))
                } else {
                    let inner = self.parse_expr()?;
                    self.expect(&TokenKind::RParen)?;
                    Ok(inner)
                }
            }
            TokenKind::Int(n) => {
                self.advance();
                Ok(Expr::Literal(Value::Int(n)))
            }
            TokenKind::Float(x) => {
                self.advance();
                Ok(Expr::Literal(Value::Float(x)))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Expr::Literal(Value::Text(s)))
            }
            TokenKind::Param(n) => {
                self.advance();
                Ok(Expr::Parameter(n))
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.advance();
                Ok(Expr::Literal(Value::Bool(true)))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.advance();
                Ok(Expr::Literal(Value::Bool(false)))
            }
            TokenKind::Star => {
                self.advance();
                Ok(Expr::Star)
            }
            TokenKind::Keyword(Keyword::Case) => {
                self.advance();
                self.parse_case()
            }
            TokenKind::Keyword(Keyword::Exists) => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                let query = self.parse_query()?;
                self.expect(&TokenKind::RParen)?;
                Ok(Expr::Exists(Box::new(query)))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if self.eat(&TokenKind::LParen) {
                    // Function call: name '(' [DISTINCT] [args] ')', e.g.
                    // SUM(total), COUNT(*), or COUNT(DISTINCT col). The name is
                    // stored upper-cased and canonical.
                    let distinct = self.eat_keyword(Keyword::Distinct);
                    let args = self.parse_call_args()?;
                    self.expect(&TokenKind::RParen)?;
                    // A trailing `OVER (...)` makes this a window function.
                    if self.eat_keyword(Keyword::Over) {
                        let (partition_by, order_by) = self.parse_window_over()?;
                        return Ok(Expr::Window {
                            func: name.to_ascii_uppercase(),
                            distinct,
                            args,
                            partition_by,
                            order_by,
                        });
                    }
                    Ok(Expr::Func {
                        name: name.to_ascii_uppercase(),
                        distinct,
                        args,
                    })
                } else if self.eat(&TokenKind::Dot) {
                    // Qualified column: name '.' name.
                    let col = self.parse_ident()?;
                    Ok(Expr::QualifiedColumn(name, col))
                } else {
                    Ok(Expr::Column(name))
                }
            }
            other => Err(SqlError::parse(
                format!("expected an expression, found {other:?}"),
                self.span(),
            )),
        }
    }
}

/// `NOT`'s operand binding power: above `AND` (3/4), at/below comparison (5).
const NOT_OPERAND_BP: u8 = 5;
/// Unary minus binds tighter than `* /` (9/10).
const NEG_OPERAND_BP: u8 = 11;
/// Binding power of the keyword predicates (`IN`, `BETWEEN`, `LIKE`, `IS
/// NULL`), the same comparison level as `=` and `<`.
const COMPARISON_BP: u8 = 5;

/// Left and right binding powers for an infix operator, or `None` if the
/// token is not an infix operator. Left-associative: `r_bp = l_bp + 1`.
const fn infix_binding_power(kind: &TokenKind) -> Option<(BinOp, u8, u8)> {
    let (op, l_bp) = match kind {
        TokenKind::Keyword(Keyword::Or) => (BinOp::Or, 1),
        TokenKind::Keyword(Keyword::And) => (BinOp::And, 3),
        TokenKind::Concat => (BinOp::Concat, 6),
        TokenKind::Eq => (BinOp::Eq, 5),
        TokenKind::NotEq => (BinOp::Ne, 5),
        TokenKind::Lt => (BinOp::Lt, 5),
        TokenKind::LtEq => (BinOp::Le, 5),
        TokenKind::Gt => (BinOp::Gt, 5),
        TokenKind::GtEq => (BinOp::Ge, 5),
        TokenKind::Plus => (BinOp::Add, 7),
        TokenKind::Minus => (BinOp::Sub, 7),
        TokenKind::Star => (BinOp::Mul, 9),
        TokenKind::Slash => (BinOp::Div, 9),
        _ => return None,
    };
    Some((op, l_bp, l_bp + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Expr {
        let mut p = Parser::from_sql(src).expect("lex");
        let e = p.parse_expr().expect("parse");
        assert!(
            p.at_eof(),
            "leftover tokens parsing {src:?}: {:?}",
            p.peek()
        );
        e
    }

    /// Display fully parenthesizes, so it shows the parse structure.
    fn shape(src: &str) -> String {
        parse(src).to_string()
    }

    #[test]
    fn literals_and_columns() {
        assert_eq!(shape("42"), "42");
        assert_eq!(shape("'hi'"), "'hi'");
        assert_eq!(shape("NULL"), "NULL");
        assert_eq!(shape("TRUE"), "TRUE");
        assert_eq!(shape("col"), "col");
        assert_eq!(shape("t.col"), "t.col");
        assert_eq!(shape("*"), "*");
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // a OR b AND c  ==  a OR (b AND c)
        assert_eq!(shape("a OR b AND c"), "(a OR (b AND c))");
    }

    #[test]
    fn comparison_binds_tighter_than_and() {
        // a = 1 AND b = 2  ==  (a = 1) AND (b = 2)
        assert_eq!(shape("a = 1 AND b = 2"), "((a = 1) AND (b = 2))");
    }

    #[test]
    fn arithmetic_precedence() {
        // a + b * c  ==  a + (b * c)
        assert_eq!(shape("a + b * c"), "(a + (b * c))");
        // a * b + c  ==  (a * b) + c
        assert_eq!(shape("a * b + c"), "((a * b) + c)");
    }

    #[test]
    fn left_associativity() {
        // a - b - c  ==  (a - b) - c
        assert_eq!(shape("a - b - c"), "((a - b) - c)");
    }

    #[test]
    fn parens_override_precedence() {
        assert_eq!(shape("(a + b) * c"), "((a + b) * c)");
        assert_eq!(shape("a AND (b OR c)"), "(a AND (b OR c))");
    }

    #[test]
    fn not_is_looser_than_comparison_tighter_than_and() {
        // NOT a = b  ==  NOT (a = b)
        assert_eq!(shape("NOT a = b"), "(NOT (a = b))");
        // NOT a AND b  ==  (NOT a) AND b
        assert_eq!(shape("NOT a AND b"), "((NOT a) AND b)");
    }

    #[test]
    fn unary_minus_binds_tightest() {
        // -a * b  ==  (-a) * b
        assert_eq!(shape("-a * b"), "((-a) * b)");
    }

    #[test]
    fn full_predicate() {
        assert_eq!(
            shape("a = 1 AND b <> 'x' OR c >= 3"),
            "(((a = 1) AND (b <> 'x')) OR (c >= 3))"
        );
    }

    #[test]
    fn dangling_operator_errors() {
        let mut p = Parser::from_sql("a +").expect("lex");
        assert!(p.parse_expr().is_err());
    }

    #[test]
    fn unclosed_paren_errors() {
        let mut p = Parser::from_sql("(a + b").expect("lex");
        assert!(p.parse_expr().is_err());
    }

    #[test]
    fn display_round_trips_through_reparse() {
        for src in [
            "a = 1 AND b = 2",
            "NOT a OR b",
            "a + b * c - d",
            "(a OR b) AND NOT c",
            "x.y = z.w",
        ] {
            let first = parse(src);
            let printed = first.to_string();
            let second = parse(&printed);
            assert_eq!(
                first, second,
                "round-trip failed for {src:?} -> {printed:?}"
            );
        }
    }
}
