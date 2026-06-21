//! Private information retrieval: fetch a memory without revealing which one.
//!
//! Blind vector search hid your embeddings from the server. This hides your
//! *query*: a client retrieves record `i` from a database held on two
//! non-colluding servers, and neither server learns anything about `i`. The
//! trick is exclusive-or. The client draws a uniformly random selection vector
//! and sends it to server A, then sends server B the same vector with bit `i`
//! flipped. Each server returns the exclusive-or of the records its vector
//! selects. The two selections differ in exactly one position, `i`, so the
//! exclusive-or of the two answers is precisely record `i`, while each server on
//! its own saw only a uniformly random selection that reveals nothing about which
//! record was wanted.
//!
//! Honest scope: this is the classic two-server information-theoretic scheme. It
//! assumes the two servers do not collude, and each query downloads one record's
//! width of data. It is novel here as an AI-memory feature, not as cryptography.

/// A database of equal-width records, replicated to each server.
#[derive(Clone, Debug)]
pub struct Pir {
    records: Vec<Vec<u8>>,
    width: usize,
}

impl Pir {
    /// Build the database. All records must share a width.
    ///
    /// # Panics
    /// Panics if the records are empty or not all the same width.
    #[must_use]
    pub fn new(records: Vec<Vec<u8>>) -> Self {
        assert!(!records.is_empty(), "need at least one record");
        let width = records[0].len();
        assert!(
            records.iter().all(|r| r.len() == width),
            "records must share a width"
        );
        Self { records, width }
    }

    /// The number of records.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the database is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// A server's answer: the exclusive-or of every record its `query` selects.
    /// The server learns only this uniformly random selection.
    ///
    /// # Panics
    /// Panics if `query` is not one bit per record.
    #[must_use]
    pub fn answer(&self, query: &[bool]) -> Vec<u8> {
        assert_eq!(query.len(), self.records.len(), "one query bit per record");
        let mut acc = vec![0u8; self.width];
        for (selected, record) in query.iter().zip(&self.records) {
            if *selected {
                for (a, b) in acc.iter_mut().zip(record) {
                    *a ^= b;
                }
            }
        }
        acc
    }
}

/// A tiny deterministic generator so a query is reproducible from a nonce.
struct Rng(u64);
impl Rng {
    fn bit(&mut self) -> bool {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x & 1 == 1
    }
}

/// Build the two server queries for record `index` over `n` records.
///
/// The first query is a uniformly random selection independent of `index`; the
/// second is the same with bit `index` flipped, both drawn from a fresh nonce.
///
/// # Panics
/// Panics if `index >= n`.
#[must_use]
pub fn make_queries(n: usize, index: usize, nonce: u64) -> (Vec<bool>, Vec<bool>) {
    assert!(index < n, "index out of range");
    let mut rng = Rng(nonce | 1);
    let q1: Vec<bool> = (0..n).map(|_| rng.bit()).collect();
    let mut q2 = q1.clone();
    q2[index] = !q2[index];
    (q1, q2)
}

/// Reconstruct the requested record from the two servers' answers.
#[must_use]
pub fn reconstruct(answer_a: &[u8], answer_b: &[u8]) -> Vec<u8> {
    answer_a.iter().zip(answer_b).map(|(a, b)| a ^ b).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Pir {
        Pir::new(
            (0..16u8)
                .map(|i| vec![i, i.wrapping_mul(3), i ^ 0x5A, i.wrapping_add(7)])
                .collect(),
        )
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn retrieves_every_record_correctly() {
        let pir = db();
        for i in 0..pir.len() {
            let (q1, q2) = make_queries(pir.len(), i, 0xABCD + i as u64);
            let got = reconstruct(&pir.answer(&q1), &pir.answer(&q2));
            let want = vec![
                i as u8,
                (i as u8).wrapping_mul(3),
                (i as u8) ^ 0x5A,
                (i as u8).wrapping_add(7),
            ];
            assert_eq!(got, want, "record {i} must reconstruct");
        }
    }

    #[test]
    fn server_a_view_is_independent_of_the_index() {
        // With the same nonce, server A's query is identical regardless of which
        // record the client actually wants. So server A learns nothing about i.
        let (a3, _) = make_queries(16, 3, 42);
        let (a7, _) = make_queries(16, 7, 42);
        assert_eq!(a3, a7, "server A's query must not depend on the index");
    }

    #[test]
    fn the_two_queries_differ_in_exactly_one_position() {
        let (q1, q2) = make_queries(16, 9, 7);
        let diffs: Vec<usize> = (0..16).filter(|&j| q1[j] != q2[j]).collect();
        assert_eq!(diffs, vec![9], "queries differ only at the requested index");
    }

    #[test]
    fn a_single_query_is_not_constant() {
        // Sanity that the selection is actually random (roughly balanced), so it
        // hides the request rather than being all-zeros or all-ones.
        let (q1, _) = make_queries(1000, 0, 0xF00D);
        let set = q1.iter().filter(|b| **b).count();
        assert!(
            (350..=650).contains(&set),
            "selection should be roughly balanced, was {set}"
        );
    }
}
