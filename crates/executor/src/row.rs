//! The row (tuple) codec: encode a row of SQL values to bytes and back.
//!
//! A stored row is what the engine writes as the `MvccTable` value and what
//! the executor decodes when scanning. The format is driven by the catalog
//! schema (a list of [`DataType`]s), so the bytes need only carry the data,
//! not its types.
//!
//! # Layout
//!
//! ```text
//! [ null bitmap: ceil(n/8) bytes ][ column 0 ][ column 1 ] ...
//! ```
//!
//! Bit `i` of the null bitmap (LSB first) is set when column `i` is NULL; a
//! null column contributes no bytes after the bitmap. Each non-null column is
//! encoded by its declared type:
//!
//! - `INT`  -> 8 bytes, little-endian `i64`.
//! - `TEXT` -> 4-byte little-endian length prefix, then that many UTF-8 bytes.
//!
//! `BOOL` is not a column type (the catalog only has `INT` and `TEXT`), so a
//! boolean value in a row is a type error.

use rustdb_sql::statement::DataType;
use rustdb_sql::Value;

use crate::error::{ExecError, Result};

/// The type name of a value, for error messages.
const fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "INT",
        Value::Text(_) => "TEXT",
        Value::Bool(_) => "BOOL",
        Value::Null => "NULL",
    }
}

/// The type name of a column.
const fn data_type_name(t: DataType) -> &'static str {
    match t {
        DataType::Int => "INT",
        DataType::Text => "TEXT",
    }
}

/// Number of bytes in the null bitmap for `n` columns.
const fn null_bitmap_len(n: usize) -> usize {
    n.div_ceil(8)
}

/// Encode `values` into the stored-row byte format, validated against
/// `schema`. Fails if the arity or any column's type does not match.
pub fn encode_row(values: &[Value], schema: &[DataType]) -> Result<Vec<u8>> {
    if values.len() != schema.len() {
        return Err(ExecError::RowArity {
            expected: schema.len(),
            got: values.len(),
        });
    }

    let mut bytes = vec![0u8; null_bitmap_len(schema.len())];
    for (i, (value, &ty)) in values.iter().zip(schema).enumerate() {
        match value {
            Value::Null => bytes[i / 8] |= 1 << (i % 8),
            Value::Int(n) if ty == DataType::Int => bytes.extend_from_slice(&n.to_le_bytes()),
            Value::Text(s) if ty == DataType::Text => {
                let len = u32::try_from(s.len()).map_err(|_| ExecError::RowType {
                    column: i,
                    expected: data_type_name(ty),
                    found: "oversized TEXT",
                })?;
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.extend_from_slice(s.as_bytes());
            }
            other => {
                return Err(ExecError::RowType {
                    column: i,
                    expected: data_type_name(ty),
                    found: value_type_name(other),
                });
            }
        }
    }
    Ok(bytes)
}

/// Decode a stored row produced by [`encode_row`] against `schema`.
pub fn decode_row(bytes: &[u8], schema: &[DataType]) -> Result<Vec<Value>> {
    let bitmap_len = null_bitmap_len(schema.len());
    if bytes.len() < bitmap_len {
        return Err(ExecError::RowTruncated { column: 0 });
    }
    let (bitmap, mut rest) = bytes.split_at(bitmap_len);

    let mut out = Vec::with_capacity(schema.len());
    for (i, &ty) in schema.iter().enumerate() {
        let is_null = bitmap[i / 8] & (1 << (i % 8)) != 0;
        if is_null {
            out.push(Value::Null);
            continue;
        }
        match ty {
            DataType::Int => {
                let raw = rest.get(..8).ok_or(ExecError::RowTruncated { column: i })?;
                let n = i64::from_le_bytes(raw.try_into().expect("checked 8 bytes"));
                out.push(Value::Int(n));
                rest = &rest[8..];
            }
            DataType::Text => {
                let len_bytes = rest.get(..4).ok_or(ExecError::RowTruncated { column: i })?;
                let len =
                    u32::from_le_bytes(len_bytes.try_into().expect("checked 4 bytes")) as usize;
                rest = &rest[4..];
                let str_bytes = rest
                    .get(..len)
                    .ok_or(ExecError::RowTruncated { column: i })?;
                let s =
                    std::str::from_utf8(str_bytes).map_err(|_| ExecError::RowUtf8 { column: i })?;
                out.push(Value::Text(s.to_string()));
                rest = &rest[len..];
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(values: &[Value], schema: &[DataType]) {
        let bytes = encode_row(values, schema).expect("encode");
        let back = decode_row(&bytes, schema).expect("decode");
        assert_eq!(values, back.as_slice());
    }

    #[test]
    fn round_trips_ints_text_and_nulls() {
        let schema = [DataType::Int, DataType::Text, DataType::Int];
        rt(
            &[Value::Int(7), Value::Text("hello".into()), Value::Int(-3)],
            &schema,
        );
        rt(
            &[Value::Null, Value::Text(String::new()), Value::Null],
            &schema,
        );
        rt(
            &[
                Value::Int(i64::MIN),
                Value::Text("x".into()),
                Value::Int(i64::MAX),
            ],
            &schema,
        );
    }

    #[test]
    fn empty_schema_round_trips() {
        rt(&[], &[]);
    }

    #[test]
    fn arity_mismatch_errors() {
        let err = encode_row(&[Value::Int(1)], &[DataType::Int, DataType::Int]).unwrap_err();
        assert_eq!(
            err,
            ExecError::RowArity {
                expected: 2,
                got: 1
            }
        );
    }

    #[test]
    fn type_mismatch_errors() {
        let err = encode_row(&[Value::Text("no".into())], &[DataType::Int]).unwrap_err();
        assert!(matches!(err, ExecError::RowType { column: 0, .. }));
        // BOOL is never a column type.
        let err = encode_row(&[Value::Bool(true)], &[DataType::Int]).unwrap_err();
        assert!(matches!(err, ExecError::RowType { found: "BOOL", .. }));
    }

    #[test]
    fn truncated_bytes_error() {
        // Encode an int row, then chop it.
        let bytes = encode_row(&[Value::Int(123)], &[DataType::Int]).unwrap();
        let err = decode_row(&bytes[..bytes.len() - 1], &[DataType::Int]).unwrap_err();
        assert!(matches!(err, ExecError::RowTruncated { column: 0 }));
    }

    #[test]
    fn null_bitmap_spans_multiple_bytes() {
        // 9 columns forces a 2-byte bitmap; alternate null / non-null.
        let schema = [DataType::Int; 9];
        let values: Vec<Value> = (0..9)
            .map(|i| {
                if i % 2 == 0 {
                    Value::Int(i)
                } else {
                    Value::Null
                }
            })
            .collect();
        rt(&values, &schema);
    }
}
