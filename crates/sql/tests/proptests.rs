//! Parser round-trip property test.
//!
//! The oracle: for any AST the generator produces, printing it via `Display`
//! and re-parsing yields the identical AST. `parse(print(ast)) == ast`.
//! Because `Display` fully parenthesizes expressions and emits canonical
//! clauses, this holds regardless of operator precedence - a structural
//! check that the printer and parser are exact inverses.

use picklejar_sql::ast::{BinOp, Expr, UnOp, Value};
use picklejar_sql::statement::{
    ColumnDef, DataType, Join, JoinKind, OrderItem, Select, SelectItem, Statement, TableRef,
};
use picklejar_sql::Parser;
use proptest::prelude::*;

// --- generators ---

fn ident() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a"),
        Just("b"),
        Just("c"),
        Just("t"),
        Just("x"),
        Just("col1"),
        Just("name"),
    ]
    .prop_map(String::from)
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        // Non-negative ints only: a negative literal is unary minus applied
        // to a positive literal in SQL, so Int(-5) would not round-trip.
        (0i64..1000).prop_map(Value::Int),
        "[a-z ]{0,8}".prop_map(Value::Text),
        any::<bool>().prop_map(Value::Bool),
        Just(Value::Null),
    ]
}

fn bin_op() -> impl Strategy<Value = BinOp> {
    prop_oneof![
        Just(BinOp::Eq),
        Just(BinOp::Ne),
        Just(BinOp::Lt),
        Just(BinOp::Le),
        Just(BinOp::Gt),
        Just(BinOp::Ge),
        Just(BinOp::And),
        Just(BinOp::Or),
        Just(BinOp::Add),
        Just(BinOp::Sub),
        Just(BinOp::Mul),
        Just(BinOp::Div),
    ]
}

fn un_op() -> impl Strategy<Value = UnOp> {
    prop_oneof![Just(UnOp::Not), Just(UnOp::Neg)]
}

fn expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        ident().prop_map(Expr::Column),
        (ident(), ident()).prop_map(|(t, c)| Expr::QualifiedColumn(t, c)),
        value().prop_map(Expr::Literal),
    ];
    // Depth up to 3, at most ~32 nodes, branching factor 2.
    leaf.prop_recursive(3, 32, 2, |inner| {
        prop_oneof![
            (bin_op(), inner.clone(), inner.clone()).prop_map(|(op, l, r)| Expr::binary(op, l, r)),
            (un_op(), inner).prop_map(|(op, e)| Expr::unary(op, e)),
        ]
    })
}

fn table_ref() -> impl Strategy<Value = TableRef> {
    (ident(), proptest::option::of(ident())).prop_map(|(name, alias)| TableRef {
        name,
        alias,
        subquery: None,
    })
}

fn select_item() -> impl Strategy<Value = SelectItem> {
    prop_oneof![
        Just(SelectItem::Star),
        (expr(), proptest::option::of(ident())).prop_map(|(e, a)| SelectItem::Expr(e, a)),
    ]
}

fn data_type() -> impl Strategy<Value = DataType> {
    prop_oneof![Just(DataType::Int), Just(DataType::Text)]
}

fn column_def() -> impl Strategy<Value = ColumnDef> {
    (
        ident(),
        data_type(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(name, ty, primary_key, not_null, unique)| ColumnDef {
            name,
            ty,
            primary_key,
            not_null,
            unique,
            default: None,
            serial: false,
        })
}

fn join() -> impl Strategy<Value = Join> {
    let kind = prop_oneof![Just(JoinKind::Inner), Just(JoinKind::Left)];
    (kind, table_ref(), expr()).prop_map(|(kind, table, on)| Join { kind, table, on })
}

fn order_item() -> impl Strategy<Value = OrderItem> {
    (expr(), any::<bool>(), proptest::option::of(any::<bool>())).prop_map(
        |(expr, desc, nulls_first)| OrderItem {
            expr,
            desc,
            nulls_first,
        },
    )
}

fn select() -> impl Strategy<Value = Select> {
    (
        prop::bool::ANY,
        prop::collection::vec(select_item(), 1..3),
        table_ref(),
        prop::collection::vec(join(), 0..2),
        proptest::option::of(expr()),
        prop::collection::vec(expr(), 0..2),
        proptest::option::of(expr()),
        prop::collection::vec(order_item(), 0..2),
        proptest::option::of(0u64..1000),
        proptest::option::of(0u64..1000),
    )
        .prop_map(
            |(
                distinct,
                projections,
                from,
                joins,
                where_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
            )| Select {
                distinct,
                projections,
                from,
                joins,
                where_clause,
                group_by,
                having,
                order_by,
                limit,
                offset,
            },
        )
}

fn statement() -> impl Strategy<Value = Statement> {
    prop_oneof![
        (ident(), prop::collection::vec(column_def(), 1..4)).prop_map(|(name, columns)| {
            Statement::CreateTable {
                if_not_exists: false,
                name,
                columns,
                constraints: vec![],
            }
        }),
        ident().prop_map(|name| Statement::DropTable {
            if_exists: false,
            name,
        }),
        (ident(), ident(), ident()).prop_map(|(name, table, column)| Statement::CreateIndex {
            name,
            table,
            column
        }),
        select().prop_map(|s| Statement::Select(Box::new(s))),
        (
            ident(),
            prop::collection::vec(ident(), 1..3),
            prop::collection::vec(prop::collection::vec(expr(), 1..3), 1..3),
        )
            .prop_map(|(table, columns, rows)| Statement::Insert {
                table,
                columns,
                rows,
                source: None,
                on_conflict: None,
                returning: vec![],
            }),
        (
            ident(),
            prop::collection::vec((ident(), expr()), 1..3),
            proptest::option::of(expr()),
        )
            .prop_map(|(table, assignments, where_clause)| Statement::Update {
                table,
                assignments,
                where_clause,
                returning: vec![],
            }),
        (ident(), proptest::option::of(expr())).prop_map(|(table, where_clause)| {
            Statement::Delete {
                table,
                where_clause,
                returning: vec![],
            }
        }),
    ]
}

// --- properties ---

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn expr_round_trips(e in expr()) {
        let printed = e.to_string();
        let mut p = Parser::from_sql(&printed).expect("lex");
        let reparsed = p.parse_expr().expect("parse");
        prop_assert!(p.at_eof(), "leftover tokens after {printed:?}");
        prop_assert_eq!(reparsed, e, "expr did not round-trip: {}", printed);
    }

    #[test]
    fn statement_round_trips(stmt in statement()) {
        let printed = stmt.to_string();
        let mut p = Parser::from_sql(&printed).expect("lex");
        let reparsed = p.parse_statement().expect("parse");
        prop_assert!(p.at_eof(), "leftover tokens after {printed:?}");
        prop_assert_eq!(reparsed, stmt, "statement did not round-trip: {}", printed);
    }
}
