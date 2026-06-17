//! Differential testing of the picklejar engine against SQLite.
//!
//! For each seed, a generator builds a small random schema, some rows, and a
//! random `SELECT`, then runs the identical SQL through both picklejar and SQLite
//! (the reference oracle) and compares the results. Any divergence is a picklejar
//! bug or a documented dialect difference. Because everything is derived from
//! one `u64` seed, a failure replays exactly.
//!
//! # Staying inside a shared semantics
//!
//! SQLite and picklejar are not the same dialect (SQLite is dynamically typed,
//! picklejar is statically typed like Postgres). To compare meaningfully, the
//! generator stays inside a subset where the two agree: only `INT` and `TEXT`
//! columns and literals, type-correct predicates (never comparing an int to a
//! text), the integer aggregates `COUNT` / `SUM` / `MIN` / `MAX` (not `AVG`,
//! whose type differs), no division (truncation differs), and no `ORDER BY`
//! reliance (results are compared as a sorted multiset, so the two engines'
//! row order and NULL-ordering never matter). SQLite runs first; if it rejects
//! the SQL, the seed is skipped rather than blamed on picklejar.

use std::fmt::Write as _;

use picklejar::{Database, QueryOutcome, Value};

/// `SplitMix64`, the same deterministic PRNG the crash simulator uses.
#[derive(Debug)]
pub struct Rng(u64);

impl Rng {
    /// Seed the generator.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        let i = usize::try_from(self.below(xs.len() as u64)).unwrap_or(0);
        &xs[i]
    }
}

/// A column's static type in the shared subset.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Ty {
    Int,
    Text,
}

/// A generated program: setup statements plus the query under test.
#[derive(Debug, Clone)]
pub struct Program {
    /// `CREATE TABLE` and `INSERT` statements, run in order.
    pub setup: Vec<String>,
    /// The `SELECT` to compare across engines.
    pub query: String,
}

impl Program {
    /// The full program as runnable SQL, for failure reports.
    #[must_use]
    pub fn to_sql(&self) -> String {
        let mut out = String::new();
        for s in &self.setup {
            let _ = writeln!(out, "{s};");
        }
        let _ = writeln!(out, "{};", self.query);
        out
    }
}

/// The result of one comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// picklejar and SQLite agreed; the query returned `rows` rows.
    Match {
        /// Rows returned.
        rows: usize,
    },
    /// SQLite rejected the SQL, so the seed was not compared.
    Skipped,
}

/// The two base tables. Schemas are fixed; only the data and the query vary.
const A_COLS: &[(&str, Ty)] = &[("id", Ty::Int), ("n", Ty::Int), ("label", Ty::Text)];
const B_COLS: &[(&str, Ty)] = &[("id", Ty::Int), ("aid", Ty::Int), ("m", Ty::Int)];
const WORDS: &[&str] = &["x", "yy", "zzz", "foo", "bar", "baz"];

/// Generate a program from `rng`.
fn generate(rng: &mut Rng) -> Program {
    let mut setup = vec![
        "CREATE TABLE a (id INT, n INT, label TEXT)".to_string(),
        "CREATE TABLE b (id INT, aid INT, m INT)".to_string(),
    ];

    let na = rng.below(5) + 1;
    for i in 0..na {
        let n = nullable_int(rng);
        let label = nullable_text(rng);
        setup.push(format!("INSERT INTO a VALUES ({i}, {n}, {label})"));
    }
    let nb = rng.below(5) + 1;
    for j in 0..nb {
        let aid = if rng.chance(15) {
            "NULL".to_string()
        } else {
            rng.below(na + 1).to_string()
        };
        let m = nullable_int(rng);
        setup.push(format!("INSERT INTO b VALUES ({j}, {aid}, {m})"));
    }

    let query = if rng.chance(35) {
        gen_join(rng)
    } else if rng.chance(40) {
        gen_aggregate(rng)
    } else if rng.chance(25) {
        gen_set_op(rng)
    } else {
        gen_single(rng)
    };

    Program { setup, query }
}

fn nullable_int(rng: &mut Rng) -> String {
    if rng.chance(20) {
        "NULL".to_string()
    } else {
        rng.below(10).to_string()
    }
}

fn nullable_text(rng: &mut Rng) -> String {
    if rng.chance(20) {
        "NULL".to_string()
    } else {
        format!("'{}'", rng.pick(WORDS))
    }
}

/// A single-table select with an optional WHERE and DISTINCT.
fn gen_single(rng: &mut Rng) -> String {
    let (table, cols) = if rng.chance(50) {
        ("a", A_COLS)
    } else {
        ("b", B_COLS)
    };
    let qualified: Vec<(String, Ty)> = cols.iter().map(|(c, t)| ((*c).to_string(), *t)).collect();
    let distinct = if rng.chance(30) { "DISTINCT " } else { "" };
    let proj = gen_projection(rng, &qualified);
    let mut q = format!("SELECT {distinct}{proj} FROM {table}");
    if rng.chance(70) {
        q.push_str(" WHERE ");
        q.push_str(&gen_predicate(rng, &qualified, 0));
    }
    q
}

/// A grouped aggregate over one table.
fn gen_aggregate(rng: &mut Rng) -> String {
    let (table, cols) = if rng.chance(50) {
        ("a", A_COLS)
    } else {
        ("b", B_COLS)
    };
    let group = *rng.pick(cols);
    // Aggregate over an INT column (or COUNT(*)), so the aggregate is always
    // INT-typed and the HAVING comparison to an integer literal is well typed.
    // SQLite would coerce a TEXT aggregate compared to an int; picklejar, like
    // Postgres, rejects that as a type error, so we never generate it.
    let int_cols: Vec<&str> = cols
        .iter()
        .filter(|(_, t)| *t == Ty::Int)
        .map(|(c, _)| *c)
        .collect();
    let agg = match rng.below(4) {
        0 => "COUNT(*)".to_string(),
        1 => format!("SUM({})", rng.pick(&int_cols)),
        2 => format!("MIN({})", rng.pick(&int_cols)),
        _ => format!("MAX({})", rng.pick(&int_cols)),
    };
    let qualified: Vec<(String, Ty)> = cols.iter().map(|(c, t)| ((*c).to_string(), *t)).collect();
    let mut q = format!("SELECT {}, {agg} FROM {table}", group.0);
    if rng.chance(50) {
        q.push_str(" WHERE ");
        q.push_str(&gen_predicate(rng, &qualified, 0));
    }
    let _ = write!(q, " GROUP BY {}", group.0);
    if rng.chance(40) {
        let _ = write!(q, " HAVING {agg} > {}", rng.below(3));
    }
    q
}

/// An inner join of a and b on a.id = b.aid.
fn gen_join(rng: &mut Rng) -> String {
    let mut available: Vec<(String, Ty)> = Vec::new();
    for (c, t) in A_COLS {
        available.push((format!("a.{c}"), *t));
    }
    for (c, t) in B_COLS {
        available.push((format!("b.{c}"), *t));
    }
    let distinct = if rng.chance(30) { "DISTINCT " } else { "" };
    let proj = gen_projection(rng, &available);
    let mut q = format!("SELECT {distinct}{proj} FROM a INNER JOIN b ON a.id = b.aid");
    if rng.chance(60) {
        q.push_str(" WHERE ");
        q.push_str(&gen_predicate(rng, &available, 0));
    }
    q
}

/// A set operation over two single-INT-column selects (so the sides are
/// union-compatible). `ALL` is only paired with `UNION`: SQLite rejects
/// `INTERSECT ALL` / `EXCEPT ALL`, which would skip the seed rather than test
/// anything.
fn gen_set_op(rng: &mut Rng) -> String {
    let (op, allow_all) = match rng.below(3) {
        0 => ("UNION", true),
        1 => ("INTERSECT", false),
        _ => ("EXCEPT", false),
    };
    let all = if allow_all && rng.chance(50) {
        " ALL"
    } else {
        ""
    };
    let left = if rng.chance(50) { "id" } else { "n" };
    let right = if rng.chance(50) { "id" } else { "m" };
    format!("SELECT {left} FROM a {op}{all} SELECT {right} FROM b")
}

/// A projection list: `*` or a non-empty subset of the available columns.
fn gen_projection(rng: &mut Rng, cols: &[(String, Ty)]) -> String {
    if rng.chance(25) {
        return "*".to_string();
    }
    let mut chosen: Vec<String> = cols
        .iter()
        .filter(|_| rng.chance(55))
        .map(|(c, _)| c.clone())
        .collect();
    if chosen.is_empty() {
        chosen.push(cols[0].0.clone());
    }
    chosen.join(", ")
}

/// A boolean predicate over `cols`, up to a small depth.
fn gen_predicate(rng: &mut Rng, cols: &[(String, Ty)], depth: u32) -> String {
    if depth < 2 && rng.chance(45) {
        let op = if rng.chance(50) { "AND" } else { "OR" };
        let left = gen_predicate(rng, cols, depth + 1);
        let right = gen_predicate(rng, cols, depth + 1);
        return format!("({left} {op} {right})");
    }
    let leaf = gen_leaf(rng, cols);
    if rng.chance(20) {
        format!("NOT ({leaf})")
    } else {
        leaf
    }
}

/// A single comparison or null test.
fn gen_leaf(rng: &mut Rng, cols: &[(String, Ty)]) -> String {
    let (col, ty) = rng.pick(cols).clone();
    if rng.chance(25) {
        let null_op = if rng.chance(50) {
            "IS NULL"
        } else {
            "IS NOT NULL"
        };
        return format!("{col} {null_op}");
    }
    let cmp = *rng.pick(&["=", "<>", "<", "<=", ">", ">="]);
    // Right-hand side: a same-typed literal, or another same-typed column.
    let same_type: Vec<(String, Ty)> = cols.iter().filter(|(_, t)| *t == ty).cloned().collect();
    let rhs = if rng.chance(60) || same_type.len() < 2 {
        match ty {
            Ty::Int => rng.below(10).to_string(),
            Ty::Text => format!("'{}'", rng.pick(WORDS)),
        }
    } else {
        rng.pick(&same_type).0.clone()
    };
    format!("{col} {cmp} {rhs}")
}

/// Run one differential comparison for `seed`.
///
/// # Errors
///
/// Returns a human-readable report (including the seed and full SQL) if picklejar
/// rejects SQL that SQLite accepts, or if their results differ.
pub fn run_seed(seed: u64) -> Result<Outcome, String> {
    let mut rng = Rng::new(seed);
    let prog = generate(&mut rng);

    // SQLite is the oracle; if it rejects the SQL, skip rather than blame picklejar.
    let Some(expected) = run_sqlite(&prog) else {
        return Ok(Outcome::Skipped);
    };

    let actual = run_picklejar(&prog).map_err(|e| {
        report(
            seed,
            &prog,
            &format!("picklejar rejected valid SQL: {e}"),
            None,
            None,
        )
    })?;

    if expected == actual {
        Ok(Outcome::Match { rows: actual.len() })
    } else {
        Err(report(
            seed,
            &prog,
            "result sets differ",
            Some(&expected),
            Some(&actual),
        ))
    }
}

/// Run the program through SQLite, returning the query's rows as a sorted
/// multiset of canonical cells, or `None` if SQLite rejected any statement.
fn run_sqlite(prog: &Program) -> Option<Vec<String>> {
    let conn = rusqlite::Connection::open_in_memory().ok()?;
    for stmt in &prog.setup {
        conn.execute(stmt, []).ok()?;
    }
    let mut stmt = conn.prepare(&prog.query).ok()?;
    let ncols = stmt.column_count();
    let mut rows = stmt
        .query_map([], |row| {
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let v: rusqlite::types::Value = row.get(i)?;
                cells.push(canon_sqlite(&v));
            }
            Ok(cells.join("|"))
        })
        .ok()?
        .collect::<Result<Vec<String>, _>>()
        .ok()?;
    rows.sort();
    Some(rows)
}

/// Run the program through picklejar, returning the query's rows as a sorted
/// multiset of canonical cells.
fn run_picklejar(prog: &Program) -> Result<Vec<String>, String> {
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let mut db = Database::open(dir.path().join("diff.db")).map_err(|e| format!("open: {e}"))?;
    for stmt in &prog.setup {
        db.execute(stmt)
            .map_err(|e| format!("setup `{stmt}`: {e}"))?;
    }
    match db.execute(&prog.query).map_err(|e| e.to_string())? {
        QueryOutcome::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(canon_picklejar)
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect();
            out.sort();
            Ok(out)
        }
        other => Err(format!("expected rows, got {other:?}")),
    }
}

/// Canonical text for a picklejar value (matches [`canon_sqlite`]).
fn canon_picklejar(v: &Value) -> String {
    match v {
        Value::Null => "N".to_string(),
        // The generator does not emit DATE / TIMESTAMP columns, so the temporal
        // arms never reach the comparison; they fold in with INT for compactness.
        Value::Int(n) | Value::Date(n) | Value::Timestamp(n) => format!("i{n}"),
        // The generator emits no JSON columns either; fold it in with text.
        Value::Text(s) | Value::Json(s) => format!("t{s}"),
        Value::Bool(b) => format!("i{}", i64::from(*b)),
        Value::Float(x) => canon_float(*x),
        // No DECIMAL columns are generated; canonicalize as a float for safety.
        Value::Decimal(m, s) => canon_float(picklejar::decimal::to_f64(*m, *s)),
    }
}

/// Canonical text for a SQLite value (matches [`canon_picklejar`]).
fn canon_sqlite(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value as V;
    match v {
        V::Null => "N".to_string(),
        V::Integer(n) => format!("i{n}"),
        V::Real(x) => canon_float(*x),
        V::Text(s) => format!("t{s}"),
        V::Blob(b) => format!("x{}", b.len()),
    }
}

/// A float that is integral renders as an int, so `SUM` returning `Real` in one
/// engine and `Integer` in the other still compares equal.
#[allow(clippy::cast_possible_truncation)] // guarded: only integral values in i64 range
fn canon_float(x: f64) -> String {
    if x.fract() == 0.0 && x.abs() < 9.0e15 {
        format!("i{}", x as i64)
    } else {
        format!("r{x}")
    }
}

/// Build a reproducible failure report.
fn report(
    seed: u64,
    prog: &Program,
    what: &str,
    expected: Option<&[String]>,
    actual: Option<&[String]>,
) -> String {
    let mut out = format!("seed {seed}: {what}\n--- SQL ---\n{}", prog.to_sql());
    if let Some(e) = expected {
        let _ = write!(out, "--- SQLite ({} rows) ---\n{}\n", e.len(), preview(e));
    }
    if let Some(a) = actual {
        let _ = write!(
            out,
            "--- picklejar ({} rows) ---\n{}\n",
            a.len(),
            preview(a)
        );
    }
    let _ = write!(out, "reproduce: cargo run --bin difftest -- --seed {seed}");
    out
}

fn preview(rows: &[String]) -> String {
    rows.iter().take(20).cloned().collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_and_picklejar_agree_on_a_handful_of_seeds() {
        for seed in 0..32u64 {
            run_seed(seed).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}
