//! Authenticated SQL: a query result you can verify without trusting the server.
//!
//! Authenticated KNN proved a single nearest-neighbor answer. This generalizes
//! the idea to ordinary relational queries: the committed rows are bound to one
//! Merkle root, and a `WHERE` query returns the matching rows each with an
//! inclusion proof. A thin client that pins only the root can then confirm two
//! things without re-running anything against the data:
//!
//! - **Soundness.** Every returned row is a genuine committed row (its proof
//!   hashes into the pinned root) and it actually satisfies the predicate (the
//!   client re-evaluates the predicate on the authenticated row). A server cannot
//!   fabricate a row, alter one, or slip in a row that does not match.
//! - **Completeness, by disclosure.** Soundness alone cannot stop a server from
//!   quietly omitting a matching row. For the exact case, the server can instead
//!   return every committed row; the client recomputes the root from them, and if
//!   it equals the pinned root the set is provably the whole table, so the client
//!   filters it itself and the result is complete. That costs the full table, the
//!   honest price of completeness without heavier cryptography.

use crate::authmem::sha256;

/// One committed row: a row id and its numeric fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The primary key.
    pub rowid: u64,
    /// The row's numeric columns.
    pub fields: Vec<i64>,
}

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(16 + self.fields.len() * 8);
        buf.extend_from_slice(&self.rowid.to_be_bytes());
        buf.extend_from_slice(&(self.fields.len() as u64).to_be_bytes());
        for f in &self.fields {
            buf.extend_from_slice(&f.to_be_bytes());
        }
        buf
    }
}

/// A comparison operator for a predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmp {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
}

/// A single-column predicate: `fields[field] <op> value`.
#[derive(Clone, Copy, Debug)]
pub struct Predicate {
    /// The column index.
    pub field: usize,
    /// The comparison.
    pub op: Cmp,
    /// The constant to compare against.
    pub value: i64,
}

impl Predicate {
    /// Whether `record` satisfies the predicate (absent column never matches).
    #[must_use]
    pub fn matches(&self, record: &Record) -> bool {
        let Some(&x) = record.fields.get(self.field) else {
            return false;
        };
        match self.op {
            Cmp::Eq => x == self.value,
            Cmp::Ne => x != self.value,
            Cmp::Lt => x < self.value,
            Cmp::Le => x <= self.value,
            Cmp::Gt => x > self.value,
            Cmp::Ge => x >= self.value,
        }
    }
}

const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

fn leaf_hash(record: &Record) -> [u8; 32] {
    let mut buf = vec![LEAF_PREFIX];
    buf.extend_from_slice(&record.encode());
    sha256::hash(&buf)
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut buf = [0u8; 65];
    buf[0] = NODE_PREFIX;
    buf[1..33].copy_from_slice(left);
    buf[33..].copy_from_slice(right);
    sha256::hash(&buf)
}

fn parent_level(level: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let mut next = Vec::with_capacity(level.len().div_ceil(2));
    let mut i = 0;
    while i < level.len() {
        let left = &level[i];
        let right = if i + 1 < level.len() {
            &level[i + 1]
        } else {
            left
        };
        next.push(node_hash(left, right));
        i += 2;
    }
    next
}

fn root_of(leaves: &[[u8; 32]]) -> [u8; 32] {
    if leaves.is_empty() {
        return sha256::hash(b"empty table");
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = parent_level(&level);
    }
    level[0]
}

fn prove(leaves: &[[u8; 32]], index: usize) -> Vec<[u8; 32]> {
    let mut siblings = Vec::new();
    let mut level = leaves.to_vec();
    let mut idx = index;
    while level.len() > 1 {
        let sib = if idx % 2 == 0 {
            if idx + 1 < level.len() {
                level[idx + 1]
            } else {
                level[idx]
            }
        } else {
            level[idx - 1]
        };
        siblings.push(sib);
        level = parent_level(&level);
        idx /= 2;
    }
    siblings
}

fn replay(mut node: [u8; 32], index: usize, siblings: &[[u8; 32]]) -> [u8; 32] {
    let mut idx = index;
    for sib in siblings {
        node = if idx % 2 == 0 {
            node_hash(&node, sib)
        } else {
            node_hash(sib, &node)
        };
        idx /= 2;
    }
    node
}

/// The client's pinned commitment to a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commitment(pub [u8; 32]);

/// A row returned by an authenticated query, with its inclusion proof.
#[derive(Clone, Debug)]
pub struct AuthRow {
    /// The committed row.
    pub record: Record,
    /// Its index in the canonical (row-id-sorted) order.
    pub index: usize,
    /// Sibling hashes from the leaf to the root.
    pub siblings: Vec<[u8; 32]>,
}

/// The server side: a table that commits to its rows and answers queries with
/// proofs.
#[derive(Clone, Debug)]
pub struct Table {
    records: Vec<Record>,
}

impl Table {
    /// Build a table (rows are kept in canonical row-id order).
    #[must_use]
    pub fn new(mut records: Vec<Record>) -> Self {
        records.sort_by_key(|r| r.rowid);
        Self { records }
    }

    fn leaves(&self) -> Vec<[u8; 32]> {
        self.records.iter().map(leaf_hash).collect()
    }

    /// The commitment a client pins.
    #[must_use]
    pub fn commit(&self) -> Commitment {
        Commitment(root_of(&self.leaves()))
    }

    /// Answer `SELECT * WHERE predicate`, returning matching rows with proofs.
    #[must_use]
    pub fn query(&self, predicate: &Predicate) -> Vec<AuthRow> {
        let leaves = self.leaves();
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| predicate.matches(r))
            .map(|(i, r)| AuthRow {
                record: r.clone(),
                index: i,
                siblings: prove(&leaves, i),
            })
            .collect()
    }

    /// Return every committed row with its proof, for completeness checking.
    #[must_use]
    pub fn full(&self) -> Vec<AuthRow> {
        let leaves = self.leaves();
        self.records
            .iter()
            .enumerate()
            .map(|(i, r)| AuthRow {
                record: r.clone(),
                index: i,
                siblings: prove(&leaves, i),
            })
            .collect()
    }
}

/// Why a verification failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// A row is not in the committed table (fabricated or altered).
    Forged { rowid: u64 },
    /// A returned row does not satisfy the predicate.
    NotMatching { rowid: u64 },
    /// The disclosed rows do not reconstruct the pinned commitment (a row was
    /// withheld or added).
    Incomplete,
}

/// Verify soundness: every returned row is committed and matches the predicate.
///
/// # Errors
/// Returns the first [`Rejected`] reason found.
pub fn verify_sound(
    commit: Commitment,
    predicate: &Predicate,
    rows: &[AuthRow],
) -> Result<(), Rejected> {
    for row in rows {
        if replay(leaf_hash(&row.record), row.index, &row.siblings) != commit.0 {
            return Err(Rejected::Forged {
                rowid: row.record.rowid,
            });
        }
        if !predicate.matches(&row.record) {
            return Err(Rejected::NotMatching {
                rowid: row.record.rowid,
            });
        }
    }
    Ok(())
}

/// Verify completeness by disclosure: the disclosed rows reconstruct the pinned
/// commitment, so the client can filter them itself for the true result.
///
/// # Errors
/// Returns [`Rejected::Incomplete`] if the rows do not reconstruct the
/// commitment, or [`Rejected::Forged`] if any individual proof fails.
pub fn verify_complete(
    commit: Commitment,
    predicate: &Predicate,
    all_rows: &[AuthRow],
) -> Result<Vec<Record>, Rejected> {
    // Each disclosed row must be authentic.
    for row in all_rows {
        if replay(leaf_hash(&row.record), row.index, &row.siblings) != commit.0 {
            return Err(Rejected::Forged {
                rowid: row.record.rowid,
            });
        }
    }
    // The disclosed records, in canonical order, must reproduce the commitment.
    let mut records: Vec<Record> = all_rows.iter().map(|r| r.record.clone()).collect();
    records.sort_by_key(|r| r.rowid);
    let leaves: Vec<[u8; 32]> = records.iter().map(leaf_hash).collect();
    if Commitment(root_of(&leaves)) != commit {
        return Err(Rejected::Incomplete);
    }
    Ok(records
        .into_iter()
        .filter(|r| predicate.matches(r))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        // rows: (rowid, [salary, dept])
        Table::new(vec![
            Record {
                rowid: 1,
                fields: vec![50_000, 1],
            },
            Record {
                rowid: 2,
                fields: vec![120_000, 2],
            },
            Record {
                rowid: 3,
                fields: vec![90_000, 1],
            },
            Record {
                rowid: 4,
                fields: vec![60_000, 2],
            },
            Record {
                rowid: 5,
                fields: vec![200_000, 1],
            },
        ])
    }

    fn pred() -> Predicate {
        Predicate {
            field: 0,
            op: Cmp::Gt,
            value: 80_000,
        }
    }

    #[test]
    fn an_honest_query_verifies() {
        let t = sample();
        let commit = t.commit();
        let rows = t.query(&pred());
        assert_eq!(rows.len(), 3, "salaries > 80k: rows 2, 3, 5");
        assert!(verify_sound(commit, &pred(), &rows).is_ok());
    }

    #[test]
    fn a_fabricated_row_is_rejected() {
        let t = sample();
        let commit = t.commit();
        let mut rows = t.query(&pred());
        rows[0].record.fields[0] = 999_999; // alter an authenticated row
        assert!(matches!(
            verify_sound(commit, &pred(), &rows),
            Err(Rejected::Forged { .. })
        ));
    }

    #[test]
    fn a_non_matching_row_is_rejected() {
        let t = sample();
        let commit = t.commit();
        // The server returns a real committed row that does not match (rowid 1).
        let full = t.full();
        let sneaked: Vec<AuthRow> = full.into_iter().filter(|r| r.record.rowid == 1).collect();
        assert!(matches!(
            verify_sound(commit, &pred(), &sneaked),
            Err(Rejected::NotMatching { .. })
        ));
    }

    #[test]
    fn completeness_catches_a_withheld_row() {
        let t = sample();
        let commit = t.commit();
        let mut all = t.full();
        all.retain(|r| r.record.rowid != 3); // hide a matching row
        assert_eq!(
            verify_complete(commit, &pred(), &all),
            Err(Rejected::Incomplete)
        );
    }

    #[test]
    fn completeness_returns_the_true_result() {
        let t = sample();
        let commit = t.commit();
        let result = verify_complete(commit, &pred(), &t.full()).expect("complete");
        let ids: Vec<u64> = result.iter().map(|r| r.rowid).collect();
        assert_eq!(ids, vec![2, 3, 5]);
    }
}
