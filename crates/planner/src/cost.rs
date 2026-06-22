//! The cost model: selectivity estimation and the seq-vs-index decision
//! inputs.
//!
//! All estimates are heuristic, driven by the catalog statistics. The point
//! is not perfect accuracy but a *defensible, monotone* model: a more
//! selective predicate on an indexed column lowers the index-scan cost
//! relative to a full scan, so the planner switches to the index exactly
//! when it pays off.
//!
//! # Selectivity
//!
//! - `col = const` -> `1 / distinct(col)` (an equality matches one of the
//!   distinct values, assuming a uniform distribution). Clamped to a small
//!   floor so a huge cardinality never yields zero rows.
//! - a range (`<`, `<=`, `>`, `>=`) against a constant -> `0.3` (a default
//!   third of the table, the textbook range guess).
//! - `a AND b` -> `sel(a) * sel(b)` (independence assumption).
//! - `a OR b` -> `sel(a) + sel(b) - sel(a) * sel(b)` (inclusion-exclusion).
//! - `NOT a` -> `1 - sel(a)`.
//! - anything else -> `0.5` (no information).

use picklejar_sql::{BinOp, Expr, UnOp, Value};

use crate::catalog::TableMeta;

/// The smallest selectivity we will estimate, so a high-cardinality column
/// never produces a zero-row estimate.
const MIN_SELECTIVITY: f64 = 1e-6;
/// Default selectivity for a range comparison against a constant.
const RANGE_SELECTIVITY: f64 = 0.3;
/// Default selectivity when the predicate shape carries no information.
const UNKNOWN_SELECTIVITY: f64 = 0.5;

/// Estimate the fraction of `table`'s rows that satisfy `predicate`, in
/// `[MIN_SELECTIVITY, 1.0]`.
#[must_use]
pub fn selectivity(predicate: &Expr, table: &TableMeta) -> f64 {
    let raw = match predicate {
        Expr::Binary { op, left, right } => binary_selectivity(*op, left, right, table),
        Expr::Unary {
            op: UnOp::Not,
            expr,
        } => 1.0 - selectivity(expr, table),
        // A bare column / literal as a predicate: no information.
        _ => UNKNOWN_SELECTIVITY,
    };
    raw.clamp(MIN_SELECTIVITY, 1.0)
}

fn binary_selectivity(op: BinOp, left: &Expr, right: &Expr, table: &TableMeta) -> f64 {
    match op {
        BinOp::And => selectivity(left, table) * selectivity(right, table),
        BinOp::Or => {
            let a = selectivity(left, table);
            let b = selectivity(right, table);
            a.mul_add(-b, a + b)
        }
        BinOp::Eq => column_and_literal(left, right)
            .map_or(UNKNOWN_SELECTIVITY, |(col, lit)| eq_selectivity(col, lit, table)),
        BinOp::Ne => column_and_literal(left, right)
            .map_or(UNKNOWN_SELECTIVITY, |(col, lit)| {
                1.0 - eq_selectivity(col, lit, table)
            }),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => range_selectivity(op, left, right, table),
        // LIKE has no cheap cardinality estimate, and the arithmetic, concat,
        // and JSON-access operators are not boolean predicates: all default.
        BinOp::Like
        | BinOp::Concat
        | BinOp::Add
        | BinOp::Sub
        | BinOp::Mul
        | BinOp::Div
        | BinOp::JsonGet
        | BinOp::JsonGetText
        // Vector distance yields a FLOAT, not a boolean, so as a bare predicate
        // it carries no selectivity information.
        | BinOp::VecL2
        | BinOp::VecCosine
        | BinOp::VecInner
        | BinOp::VecL1 => UNKNOWN_SELECTIVITY,
    }
}

/// Estimate a range comparison's selectivity. With `ANALYZE`-gathered min/max
/// for the column, the fraction of `[min, max]` the bound admits is used;
/// otherwise the textbook default. The column may be on either side of the
/// operator, so the comparison is normalized to `column <op> const`.
fn range_selectivity(op: BinOp, left: &Expr, right: &Expr, table: &TableMeta) -> f64 {
    let Some(col) = column_const(left, right) else {
        return UNKNOWN_SELECTIVITY;
    };
    // Pull the integer constant and orient the operator so the column is on the
    // left (flip when the literal was the left operand).
    let (k, op) = if is_literal(left) {
        (int_literal(left), flip(op))
    } else {
        (int_literal(right), op)
    };
    let (Some(k), stats) = (k, table.column_stats(col)) else {
        return RANGE_SELECTIVITY;
    };
    // An equi-depth histogram follows the real distribution; prefer it over the
    // uniform-across-[min, max] estimate when ANALYZE has built one.
    if !stats.histogram.is_empty() {
        let below = fraction_below(&stats.histogram, k);
        let frac = match op {
            BinOp::Lt | BinOp::Le => below,
            BinOp::Gt | BinOp::Ge => 1.0 - below,
            _ => return RANGE_SELECTIVITY,
        };
        return frac.clamp(MIN_SELECTIVITY, 1.0);
    }
    let (Some(min), Some(max)) = (stats.min, stats.max) else {
        return RANGE_SELECTIVITY;
    };
    if max <= min {
        // A single-valued (or empty) column: the bound either admits all or
        // none of the rows.
        let admits = match op {
            BinOp::Lt => min < k,
            BinOp::Le => min <= k,
            BinOp::Gt => min > k,
            BinOp::Ge => min >= k,
            _ => return RANGE_SELECTIVITY,
        };
        return if admits { 1.0 } else { MIN_SELECTIVITY };
    }
    #[allow(clippy::cast_precision_loss)]
    let span = (max - min) as f64;
    // Fraction of the value span the predicate admits, clamped to [0, 1].
    #[allow(clippy::cast_precision_loss)]
    let frac = match op {
        BinOp::Lt | BinOp::Le => (k - min) as f64 / span,
        BinOp::Gt | BinOp::Ge => (max - k) as f64 / span,
        _ => return RANGE_SELECTIVITY,
    };
    frac.clamp(MIN_SELECTIVITY, 1.0)
}

/// The fraction of rows below `k`, read off an equi-depth histogram of
/// monotonic bucket boundaries. Each of the `bounds.len() - 1` buckets holds an
/// equal share of rows; within the bucket containing `k`, the position is
/// interpolated linearly. Returns `0.0` below the first boundary and `1.0` at or
/// above the last.
#[allow(clippy::cast_precision_loss)]
fn fraction_below(bounds: &[i64], k: i64) -> f64 {
    let buckets = bounds.len().saturating_sub(1);
    if buckets == 0 || k <= bounds[0] {
        return 0.0;
    }
    if k >= bounds[buckets] {
        return 1.0;
    }
    for i in 0..buckets {
        if k < bounds[i + 1] {
            let (lo, hi) = (bounds[i], bounds[i + 1]);
            let within = if hi > lo {
                (k - lo) as f64 / (hi - lo) as f64
            } else {
                0.0
            };
            return (i as f64 + within) / buckets as f64;
        }
    }
    1.0
}

/// Flip a comparison operator's sense (for `const <op> column`).
const fn flip(op: BinOp) -> BinOp {
    match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::Le => BinOp::Ge,
        BinOp::Gt => BinOp::Lt,
        BinOp::Ge => BinOp::Le,
        other => other,
    }
}

/// The `i64` value of an integer literal expression, if it is one.
const fn int_literal(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Literal(picklejar_sql::Value::Int(n)) => Some(*n),
        _ => None,
    }
}

/// Equality selectivity for `col = lit`.
///
/// A value in the column's most-common-values list (gathered by `ANALYZE` from
/// a Space-Saving summary) uses its measured frequency, so a skewed value is
/// costed by how common it really is. A value not in the list takes the
/// residual: the fraction of rows the list does not cover, spread uniformly
/// over the remaining distinct values. With an empty list this reduces exactly
/// to the old `1 / distinct`.
#[allow(clippy::cast_precision_loss)]
fn eq_selectivity(col: &str, lit: &Value, table: &TableMeta) -> f64 {
    let stats = table.column_stats(col);
    if let Some((_, freq)) = stats.most_common.iter().find(|(v, _)| v == lit) {
        return *freq;
    }
    let mcv_count = stats.most_common.len() as u64;
    let distinct = stats.distinct.max(1);
    if distinct > mcv_count {
        let mcv_freq: f64 = stats.most_common.iter().map(|(_, f)| f).sum();
        let remaining = (distinct - mcv_count) as f64;
        (1.0 - mcv_freq).max(0.0) / remaining
    } else {
        1.0 / distinct as f64
    }
}

/// If exactly one side is a column reference and the other a literal, return
/// the column name paired with the literal's value.
fn column_and_literal<'a>(left: &'a Expr, right: &'a Expr) -> Option<(&'a str, &'a Value)> {
    if let Some(v) = literal_value(right) {
        Some((column_name(left)?, v))
    } else if let Some(v) = literal_value(left) {
        Some((column_name(right)?, v))
    } else {
        None
    }
}

/// The value of a literal expression, if it is one.
const fn literal_value(expr: &Expr) -> Option<&Value> {
    match expr {
        Expr::Literal(v) => Some(v),
        _ => None,
    }
}

/// If exactly one side is a column reference and the other a literal, return
/// the column name.
fn column_const<'a>(left: &'a Expr, right: &'a Expr) -> Option<&'a str> {
    if is_literal(right) {
        column_name(left)
    } else if is_literal(left) {
        column_name(right)
    } else {
        None
    }
}

/// The column name of a bare or qualified column reference.
fn column_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column(name) | Expr::QualifiedColumn(_, name) => Some(name),
        _ => None,
    }
}

const fn is_literal(expr: &Expr) -> bool {
    matches!(expr, Expr::Literal(_))
}

/// Find an indexed, sargable column in `predicate`.
///
/// Looks for an equality or range comparison against a literal on a column
/// backed by an index on `table`, returning the index name and column. Walks
/// `AND` conjuncts.
#[must_use]
pub fn sargable_index<'a>(predicate: &Expr, table: &'a TableMeta) -> Option<(&'a str, &'a str)> {
    match predicate {
        Expr::Binary { op, left, right } => match op {
            BinOp::Eq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let col = column_const(left, right)?;
                let idx = table.index_on(col)?;
                Some((idx.name.as_str(), idx.column.as_str()))
            }
            BinOp::And => sargable_index(left, table).or_else(|| sargable_index(right, table)),
            _ => None,
        },
        _ => None,
    }
}

/// Round a `sel * rows` estimate up to at least one row.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn estimate_rows(selectivity: f64, rows: u64) -> u64 {
    let est = (selectivity * rows as f64).ceil() as u64;
    est.max(1).min(rows.max(1))
}

/// Cost of a full sequential scan of `rows` rows: one unit per row.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub const fn seq_scan_cost(rows: u64) -> f64 {
    rows as f64
}

/// Cost of an index scan: a logarithmic tree descent plus one unit per
/// matched row.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn index_scan_cost(rows: u64, selectivity: f64) -> f64 {
    let descent = (rows as f64 + 1.0).log2();
    selectivity.mul_add(rows as f64, descent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Catalog, ColumnStats};
    use picklejar_sql::Parser;

    fn table(distinct_id: u64) -> TableMeta {
        let mut c = Catalog::new();
        c.apply(
            &Parser::from_sql("CREATE TABLE t (id INT, name TEXT)")
                .unwrap()
                .parse_statement()
                .unwrap(),
        )
        .unwrap();
        c.apply(
            &Parser::from_sql("CREATE INDEX idx ON t (id)")
                .unwrap()
                .parse_statement()
                .unwrap(),
        )
        .unwrap();
        c.set_row_count("t", 1000).unwrap();
        c.set_column_stats(
            "t",
            "id",
            ColumnStats {
                distinct: distinct_id,
                ..Default::default()
            },
        )
        .unwrap();
        c.get_table("t").unwrap().clone()
    }

    fn pred(src: &str) -> Expr {
        Parser::from_sql(src).unwrap().parse_expr().unwrap()
    }

    #[test]
    fn equality_selectivity_is_one_over_distinct() {
        let t = table(100);
        let s = selectivity(&pred("id = 5"), &t);
        assert!((s - 0.01).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn range_selectivity_is_default_third() {
        let t = table(100);
        assert!((selectivity(&pred("id > 5"), &t) - RANGE_SELECTIVITY).abs() < 1e-9);
    }

    #[test]
    fn equality_uses_most_common_value_frequency() {
        use picklejar_sql::Value;
        let mut c = Catalog::new();
        c.apply(
            &Parser::from_sql("CREATE TABLE t (id INT, name TEXT)")
                .unwrap()
                .parse_statement()
                .unwrap(),
        )
        .unwrap();
        c.set_row_count("t", 1000).unwrap();
        // id = 7 covers 90% of rows; the other four distinct values share the rest.
        c.set_column_stats(
            "t",
            "id",
            ColumnStats {
                distinct: 5,
                most_common: vec![(Value::Int(7), 0.9)],
                ..Default::default()
            },
        )
        .unwrap();
        let t = c.get_table("t").unwrap().clone();
        // The heavy hitter is costed by its real frequency, not 1/5 = 0.2.
        assert!((selectivity(&pred("id = 7"), &t) - 0.9).abs() < 1e-9);
        // A value outside the list takes the residual: (1 - 0.9) / (5 - 1) = 0.025.
        assert!((selectivity(&pred("id = 1"), &t) - 0.025).abs() < 1e-9);
        // != is the complement of the heavy hitter.
        assert!((selectivity(&pred("id != 7"), &t) - 0.1).abs() < 1e-9);
    }

    /// A table whose `id` column has known min/max from `ANALYZE`.
    fn table_with_range(min: i64, max: i64) -> TableMeta {
        let mut c = Catalog::new();
        c.apply(
            &Parser::from_sql("CREATE TABLE t (id INT, name TEXT)")
                .unwrap()
                .parse_statement()
                .unwrap(),
        )
        .unwrap();
        c.set_column_stats(
            "t",
            "id",
            ColumnStats {
                distinct: 100,
                min: Some(min),
                max: Some(max),
                ..Default::default()
            },
        )
        .unwrap();
        c.get_table("t").unwrap().clone()
    }

    #[test]
    fn range_selectivity_follows_the_histogram() {
        let mut c = Catalog::new();
        c.apply(
            &Parser::from_sql("CREATE TABLE t (id INT, name TEXT)")
                .unwrap()
                .parse_statement()
                .unwrap(),
        )
        .unwrap();
        c.set_row_count("t", 1000).unwrap();
        // Equi-depth boundaries clustered low (8 of 9 buckets in [0, 8)), then a
        // jump to 1000: most rows are small even though the span reaches 1000.
        c.set_column_stats(
            "t",
            "id",
            ColumnStats {
                min: Some(0),
                max: Some(1000),
                histogram: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 1000],
                ..Default::default()
            },
        )
        .unwrap();
        let t = c.get_table("t").unwrap().clone();
        // id < 9 covers 8 of 9 buckets (~0.89); a uniform [0, 1000] guess would
        // have said (9 - 0) / 1000 = 0.009.
        let s = selectivity(&pred("id < 9"), &t);
        assert!(
            s > 0.8,
            "histogram-aware range selectivity {s} should be high"
        );
        // The complement holds for the other direction.
        let g = selectivity(&pred("id > 9"), &t);
        assert!(g < 0.2, "id > 9 should be a small fraction, got {g}");
    }

    #[test]
    fn range_selectivity_uses_min_max() {
        let t = table_with_range(0, 100);
        // id > 75 admits the top quarter of [0, 100].
        assert!((selectivity(&pred("id > 75"), &t) - 0.25).abs() < 1e-9);
        // id < 25 admits the bottom quarter.
        assert!((selectivity(&pred("id < 25"), &t) - 0.25).abs() < 1e-9);
        // The literal may be on the left: 75 < id is id > 75.
        assert!((selectivity(&pred("75 < id"), &t) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn range_outside_min_max_is_tiny_or_full() {
        let t = table_with_range(10, 20);
        // Everything is below 100, so id < 100 matches all.
        assert!((selectivity(&pred("id < 100"), &t) - 1.0).abs() < 1e-9);
        // Nothing is above 100, so id > 100 matches almost none.
        assert!(selectivity(&pred("id > 100"), &t) <= MIN_SELECTIVITY * 2.0);
    }

    #[test]
    fn and_multiplies_selectivity() {
        let t = table(100);
        // id = 5 (0.01) AND id > 0 (0.3) -> 0.003
        let s = selectivity(&pred("id = 5 AND id > 0"), &t);
        assert!((s - 0.003).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn or_uses_inclusion_exclusion() {
        let t = table(100);
        // id = 5 (0.01) OR id = 6 (0.01) -> 0.01 + 0.01 - 0.0001 = 0.0199
        let s = selectivity(&pred("id = 5 OR id = 6"), &t);
        assert!((s - 0.0199).abs() < 1e-9, "got {s}");
    }

    #[test]
    fn sargable_finds_indexed_equality() {
        let t = table(100);
        let found = sargable_index(&pred("id = 5"), &t);
        assert_eq!(found, Some(("idx", "id")));
        // A non-indexed column is not sargable.
        assert_eq!(sargable_index(&pred("name = 'x'"), &t), None);
    }

    #[test]
    fn sargable_walks_and_conjuncts() {
        let t = table(100);
        let found = sargable_index(&pred("name = 'x' AND id = 5"), &t);
        assert_eq!(found, Some(("idx", "id")));
    }

    #[test]
    fn selective_index_beats_seq_scan() {
        // High cardinality: equality selectivity 1/1000, index much cheaper.
        let sel = 1.0 / 1000.0;
        assert!(index_scan_cost(1000, sel) < seq_scan_cost(1000));
    }

    #[test]
    fn nonselective_index_loses_to_seq_scan() {
        // distinct = 1 -> selectivity 1.0 -> index scans everything + descent.
        assert!(index_scan_cost(1000, 1.0) > seq_scan_cost(1000));
    }

    #[test]
    fn estimate_rows_rounds_up_and_clamps() {
        assert_eq!(estimate_rows(0.001, 1000), 1);
        assert_eq!(estimate_rows(0.5, 1000), 500);
        assert_eq!(estimate_rows(1.0, 1000), 1000);
        assert_eq!(estimate_rows(0.0, 1000), 1, "never zero rows");
    }
}
