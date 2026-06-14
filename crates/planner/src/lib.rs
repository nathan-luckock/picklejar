//! Query planner and cost-based optimizer.
//!
//! Takes a parsed SQL AST, produces a logical plan, then a physical plan,
//! choosing between scan strategies (seq scan vs index scan) and join
//! algorithms (hash join vs nested-loop join) based on table statistics.
//!
//! Outputs an `EXPLAIN`-friendly representation alongside the executable
//! plan.
//!
//! # Sprint 8 surface
//!
//! - [`catalog::Catalog`]: schema metadata + statistics.
//! - Logical plan, cost model, physical plan, and EXPLAIN land in
//!   subsequent commits.

#![forbid(unsafe_code)]

pub mod binder;
pub mod catalog;
pub mod cost;
pub mod error;
pub mod explain;
pub mod logical;
pub mod physical;

pub use binder::bind;
pub use catalog::{Catalog, Column, ColumnStats, IndexMeta, TableMeta, TableStats};
pub use error::{PlanError, Result};
pub use explain::explain;
pub use logical::LogicalPlan;
pub use physical::{plan, PhysicalPlan};
