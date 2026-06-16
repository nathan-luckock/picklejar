//! A minimal JSON value and writer, just enough to encode API responses.
//!
//! The engine and its HTTP layer are dependency-free, so the JSON output is
//! written here rather than pulled from `serde_json`. Only serialization is
//! needed (requests carry raw SQL in the body, not JSON).

use std::fmt::Write as _;

/// A JSON value.
#[derive(Debug, Clone)]
pub enum Json {
    /// `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer number.
    Int(i64),
    /// A floating-point number.
    Float(f64),
    /// A string.
    Str(String),
    /// An array.
    Array(Vec<Self>),
    /// An object, preserving key order.
    Object(Vec<(String, Self)>),
}

impl Json {
    /// Render to a compact JSON string.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Float(x) => {
                // JSON has no NaN/Infinity, so emit null for those.
                if x.is_finite() {
                    let _ = write!(out, "{x}");
                } else {
                    out.push_str("null");
                }
            }
            Self::Str(s) => write_escaped(s, out),
            Self::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Write a JSON string literal, escaping per the JSON spec.
fn write_escaped(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_primitives_and_nesting() {
        assert_eq!(Json::Null.render(), "null");
        assert_eq!(Json::Bool(true).render(), "true");
        assert_eq!(Json::Int(-7).render(), "-7");
        assert_eq!(
            Json::Object(vec![
                ("a".into(), Json::Int(1)),
                (
                    "b".into(),
                    Json::Array(vec![Json::Str("x".into()), Json::Null]),
                ),
            ])
            .render(),
            r#"{"a":1,"b":["x",null]}"#
        );
    }

    #[test]
    fn escapes_strings() {
        assert_eq!(
            Json::Str("he\"ll\no\t\\".into()).render(),
            r#""he\"ll\no\t\\""#
        );
        // A control character becomes a \u escape.
        let ctrl = String::from(char::from(1u8));
        assert_eq!(Json::Str(ctrl).render(), "\"\\u0001\"");
    }
}
