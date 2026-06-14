//! Row codec round-trip property test.
//!
//! Oracle: for any schema and any row matching it (including NULLs, empty
//! strings, and extreme integers), `decode_row(encode_row(r)) == r`.

use proptest::prelude::*;
use rustdb_executor::{decode_row, encode_row};
use rustdb_sql::statement::DataType;
use rustdb_sql::Value;

/// A column type paired with a strategy for a value of that type (or NULL).
fn column() -> impl Strategy<Value = (DataType, Value)> {
    prop_oneof![
        // INT column: any i64, or NULL.
        prop_oneof![any::<i64>().prop_map(Value::Int), Just(Value::Null),]
            .prop_map(|v| (DataType::Int, v)),
        // TEXT column: arbitrary UTF-8 (including empty), or NULL.
        prop_oneof![".*".prop_map(Value::Text), Just(Value::Null),]
            .prop_map(|v| (DataType::Text, v)),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn encode_decode_round_trips(cols in prop::collection::vec(column(), 0..12)) {
        let schema: Vec<DataType> = cols.iter().map(|(t, _)| *t).collect();
        let row: Vec<Value> = cols.iter().map(|(_, v)| v.clone()).collect();

        let bytes = encode_row(&row, &schema).expect("encode");
        let back = decode_row(&bytes, &schema).expect("decode");
        prop_assert_eq!(row, back);
    }
}
