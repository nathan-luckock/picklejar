//! The logical plan: a relational-algebra tree.
//!
//! The binder ([`crate::binder`]) lowers a parsed `SELECT` into this tree,
//! resolving table and column names against the [`Catalog`](crate::Catalog).
//! The physical planner ([`crate::physical`]) then turns it into an
//! executable, cost-annotated plan.
//!
//! Nodes are emitted bottom-up in SQL's logical evaluation order:
//! `Scan -> Join* -> Filter (WHERE) -> Aggregate (GROUP BY) -> Project
//! (SELECT) -> Sort (ORDER BY) -> Limit`.

use std::fmt;

use rustdb_sql::{Expr, JoinKind, SelectItem};

/// A node in the logical plan tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalPlan {
    /// Read all rows of a base table.
    Scan {
        /// Table name.
        table: String,
        /// Column qualifier visible to the query: the alias if one was given,
        /// otherwise the table name. Used to resolve `q.col` references and to
        /// disambiguate columns across a join (including self-joins).
        qualifier: String,
    },
    /// Keep only rows satisfying `predicate`.
    Filter {
        /// The boolean predicate.
        predicate: Expr,
        /// Child plan.
        input: Box<Self>,
    },
    /// Compute the projection list.
    Project {
        /// Projection items (`*` or expressions with optional aliases).
        items: Vec<SelectItem>,
        /// Child plan.
        input: Box<Self>,
    },
    /// Join two inputs on a predicate.
    Join {
        /// Inner or left.
        kind: JoinKind,
        /// Left input.
        left: Box<Self>,
        /// Right input.
        right: Box<Self>,
        /// The ON predicate.
        on: Expr,
    },
    /// Group rows by the given keys and compute aggregate functions.
    Aggregate {
        /// Grouping keys (empty for a whole-table aggregate).
        group_by: Vec<Expr>,
        /// Aggregate function calls to compute (e.g. `COUNT(*)`, `SUM(x)`).
        aggregates: Vec<Expr>,
        /// Child plan.
        input: Box<Self>,
    },
    /// Sort rows by the given keys (`(expr, descending)`).
    Sort {
        /// Sort keys with their direction.
        keys: Vec<(Expr, bool)>,
        /// Child plan.
        input: Box<Self>,
    },
    /// Keep at most `n` rows.
    Limit {
        /// Row cap.
        n: u64,
        /// Child plan.
        input: Box<Self>,
    },
}

impl LogicalPlan {
    fn fmt_indented(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        let pad = "  ".repeat(depth);
        match self {
            Self::Scan { table, .. } => writeln!(f, "{pad}Scan {table}"),
            Self::Filter { predicate, input } => {
                writeln!(f, "{pad}Filter {predicate}")?;
                input.fmt_indented(f, depth + 1)
            }
            Self::Project { items, input } => {
                let cols = items
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(f, "{pad}Project {cols}")?;
                input.fmt_indented(f, depth + 1)
            }
            Self::Join {
                kind,
                left,
                right,
                on,
            } => {
                writeln!(f, "{pad}{kind} ON {on}")?;
                left.fmt_indented(f, depth + 1)?;
                right.fmt_indented(f, depth + 1)
            }
            Self::Aggregate {
                group_by,
                aggregates,
                input,
            } => {
                let keys = group_by
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let aggs = aggregates
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(f, "{pad}Aggregate GROUP BY [{keys}] AGG [{aggs}]")?;
                input.fmt_indented(f, depth + 1)
            }
            Self::Sort { keys, input } => {
                let ks = keys
                    .iter()
                    .map(|(e, desc)| {
                        if *desc {
                            format!("{e} DESC")
                        } else {
                            e.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(f, "{pad}Sort {ks}")?;
                input.fmt_indented(f, depth + 1)
            }
            Self::Limit { n, input } => {
                writeln!(f, "{pad}Limit {n}")?;
                input.fmt_indented(f, depth + 1)
            }
        }
    }
}

impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_indented(f, 0)
    }
}
