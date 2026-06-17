//! SQL tokens.
//!
//! A [`Token`] is a [`TokenKind`] plus the byte [`Span`] it came from. Spans
//! are carried from the very first stage so the parser and later layers can
//! point at the exact offending text in error messages.

/// A half-open byte range `[start, end)` into the original SQL source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Span {
    /// Byte offset of the first character.
    pub start: usize,
    /// Byte offset one past the last character.
    pub end: usize,
}

impl Span {
    /// Construct a span.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
}

/// SQL keywords. Matched case-insensitively by the lexer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Keyword {
    Select,
    From,
    Where,
    Insert,
    Into,
    Values,
    Update,
    Set,
    Delete,
    Create,
    Table,
    Index,
    Drop,
    Truncate,
    Alter,
    Add,
    Column,
    View,
    Join,
    Inner,
    Left,
    Cross,
    On,
    Group,
    Having,
    Distinct,
    Order,
    By,
    Asc,
    Desc,
    Limit,
    Offset,
    Returning,
    Explain,
    Begin,
    Commit,
    Rollback,
    Union,
    All,
    And,
    Or,
    Not,
    Null,
    As,
    Primary,
    Key,
    Unique,
    Default,
    Check,
    Foreign,
    References,
    Constraint,
    True,
    False,
    // Predicates.
    In,
    Between,
    Like,
    Is,
    Case,
    When,
    Then,
    Else,
    End,
    Exists,
    // Column types.
    Int,
    Text,
    Float,
    Bool,
}

impl Keyword {
    /// Resolve an identifier word (already known to be a bareword) to a
    /// keyword, case-insensitively. Returns `None` for non-keywords.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        // ASCII-lowercase compare without allocating.
        let lower = word.to_ascii_lowercase();
        Some(match lower.as_str() {
            "select" => Self::Select,
            "from" => Self::From,
            "where" => Self::Where,
            "insert" => Self::Insert,
            "into" => Self::Into,
            "values" => Self::Values,
            "update" => Self::Update,
            "set" => Self::Set,
            "delete" => Self::Delete,
            "create" => Self::Create,
            "table" => Self::Table,
            "index" => Self::Index,
            "drop" => Self::Drop,
            "truncate" => Self::Truncate,
            "alter" => Self::Alter,
            "add" => Self::Add,
            "column" => Self::Column,
            "view" => Self::View,
            "join" => Self::Join,
            "inner" => Self::Inner,
            "left" => Self::Left,
            "cross" => Self::Cross,
            "on" => Self::On,
            "group" => Self::Group,
            "having" => Self::Having,
            "distinct" => Self::Distinct,
            "order" => Self::Order,
            "by" => Self::By,
            "asc" => Self::Asc,
            "desc" => Self::Desc,
            "limit" => Self::Limit,
            "offset" => Self::Offset,
            "returning" => Self::Returning,
            "explain" => Self::Explain,
            "begin" => Self::Begin,
            "commit" => Self::Commit,
            "rollback" => Self::Rollback,
            "union" => Self::Union,
            "all" => Self::All,
            "and" => Self::And,
            "or" => Self::Or,
            "not" => Self::Not,
            "null" => Self::Null,
            "as" => Self::As,
            "primary" => Self::Primary,
            "key" => Self::Key,
            "unique" => Self::Unique,
            "default" => Self::Default,
            "check" => Self::Check,
            "foreign" => Self::Foreign,
            "references" => Self::References,
            "constraint" => Self::Constraint,
            "true" => Self::True,
            "false" => Self::False,
            "in" => Self::In,
            "between" => Self::Between,
            "like" => Self::Like,
            "is" => Self::Is,
            "case" => Self::Case,
            "when" => Self::When,
            "then" => Self::Then,
            "else" => Self::Else,
            "end" => Self::End,
            "exists" => Self::Exists,
            "int" | "integer" => Self::Int,
            "text" | "varchar" => Self::Text,
            "float" | "real" | "double" => Self::Float,
            "bool" | "boolean" => Self::Bool,
            _ => return None,
        })
    }
}

/// The lexical category of a token.
///
/// `Eq` is not derived because the `Float` literal carries an `f64`. Token
/// equality (used in tests) is `PartialEq`, which is all that `==` needs.
#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    /// A reserved keyword.
    Keyword(Keyword),
    /// A bareword identifier (table or column name).
    Ident(String),
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A single-quoted string literal (contents, unescaped).
    Str(String),
    /// A positional parameter placeholder `$N` (the extended wire protocol
    /// binds a value to each before execution).
    Param(u32),

    // Operators and punctuation.
    /// `=`
    Eq,
    /// `<>` or `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `||` (string concatenation)
    Concat,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `,`
    Comma,
    /// `;`
    Semicolon,
    /// `.`
    Dot,
    /// End of input.
    Eof,
}

/// A token: its kind and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// The byte span in the source.
    pub span: Span,
}

impl Token {
    /// Construct a token.
    #[must_use]
    pub const fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }
}
