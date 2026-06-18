//! A self-healing, erasure-coded block store: the detect, log, repair, heal loop
//! for data on hardware nobody can service.
//!
//! Each stored blob is split into `k` data shards and `m` parity shards by the
//! Reed-Solomon code in [`crate::erasure`], and every shard is framed with its own
//! CRC32. On read, each shard's checksum is verified; a mismatch (radiation flip,
//! bit-rot, a torn write) marks that shard as an *erasure*, and as long as no more
//! than `m` shards are bad, the original data is reconstructed exactly from the
//! survivors. Every fault is appended to a fault log, and the repaired shards are
//! written back so the next read is clean. That is the whole loop a node in orbit
//! needs: it notices the damage, records it, fixes it from redundancy, and leaves
//! itself healthy, with no human and no spare node involved.
//!
//! This is a standalone store, deliberately separate from the engine's buffer
//! pool, so the self-healing logic can be exercised on its own before it is wired
//! under the live page store. The backing slots are in memory here; a file- or
//! device-backed slot map is a later step that does not change this logic.

use std::collections::HashMap;

use crate::crc32::crc32;
use crate::erasure::{ErasureError, ReedSolomon};

/// Bytes of CRC32 prepended to each shard's payload.
const CRC_LEN: usize = 4;

/// Why a shard was treated as lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    /// The shard's stored checksum did not match its bytes.
    Checksum,
    /// The shard was missing or too short to hold a frame.
    Missing,
}

/// One detected fault and whether it was repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaultEvent {
    /// The blob the fault was found in.
    pub key: u64,
    /// Which of the `k + m` shards was bad.
    pub shard: usize,
    /// What kind of fault it was.
    pub kind: FaultKind,
    /// Whether the store reconstructed and rewrote the shard.
    pub repaired: bool,
}

/// What a scrub pass found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrubReport {
    /// Blobs checked.
    pub blobs: usize,
    /// Faults detected across all blobs.
    pub faults: usize,
    /// Faults repaired (all of them, unless a blob was unrecoverable).
    pub repaired: usize,
    /// Blobs that had too many bad shards to reconstruct.
    pub unrecoverable: usize,
}

/// A store that keeps each blob as `k` data plus `m` parity CRC-framed shards and
/// heals corrupted shards from the survivors on access.
#[derive(Debug)]
pub struct ResilientStore {
    rs: ReedSolomon,
    k: usize,
    m: usize,
    /// `key` to its `k + m` framed shards (`CRC32` then payload).
    blobs: HashMap<u64, Vec<Vec<u8>>>,
    /// Every fault ever detected, in order, the audit trail of what degraded.
    log: Vec<FaultEvent>,
}

impl ResilientStore {
    /// Create a store whose blobs survive any `m` bad shards out of `k + m`.
    ///
    /// # Errors
    ///
    /// Returns [`ErasureError::BadShape`] if `k == 0` or `k + m > 256`.
    pub fn new(k: usize, m: usize) -> Result<Self, ErasureError> {
        Ok(Self {
            rs: ReedSolomon::new(k, m)?,
            k,
            m,
            blobs: HashMap::new(),
            log: Vec::new(),
        })
    }

    /// Store `data` under `key`, replacing any previous value.
    ///
    /// # Errors
    ///
    /// Propagates an [`ErasureError`] only on an internal shape bug; in practice
    /// this does not fail for a store built by [`new`](Self::new).
    pub fn put(&mut self, key: u64, data: &[u8]) -> Result<(), ErasureError> {
        // Prepend the true length so padding can be trimmed on read, and protect
        // it by erasure-coding it alongside the data rather than storing it apart.
        let mut payload = (data.len() as u64).to_le_bytes().to_vec();
        payload.extend_from_slice(data);
        let shard_len = payload.len().div_ceil(self.k).max(1);
        payload.resize(shard_len * self.k, 0);

        let mut shards: Vec<Vec<u8>> = (0..self.k)
            .map(|i| payload[i * shard_len..(i + 1) * shard_len].to_vec())
            .collect();
        shards.extend((0..self.m).map(|_| vec![0u8; shard_len]));
        self.rs.encode(&mut shards)?;

        self.blobs
            .insert(key, shards.iter().map(|s| frame(s)).collect());
        Ok(())
    }

    /// Read `key`, repairing any corrupted shards from redundancy and recording
    /// the faults. Returns the original bytes.
    ///
    /// # Errors
    ///
    /// Returns [`ErasureError::TooManyErasures`] if more than `m` shards are bad
    /// (the data is genuinely lost), or [`ErasureError::ShardLayout`] for an
    /// unknown key.
    pub fn get(&mut self, key: u64) -> Result<Vec<u8>, ErasureError> {
        let framed = self.blobs.get(&key).ok_or(ErasureError::ShardLayout)?;
        let shard_len = framed
            .iter()
            .find_map(|f| verify(f).map(<[u8]>::len))
            .unwrap_or(0);

        // Verify every shard; a bad one becomes an erasure to be reconstructed.
        let mut present = vec![true; self.k + self.m];
        let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(self.k + self.m);
        let mut faults: Vec<FaultEvent> = Vec::new();
        for (i, f) in framed.iter().enumerate() {
            if let Some(payload) = verify(f) {
                payloads.push(payload.to_vec());
            } else {
                present[i] = false;
                payloads.push(vec![0u8; shard_len]);
                let kind = if f.len() < CRC_LEN {
                    FaultKind::Missing
                } else {
                    FaultKind::Checksum
                };
                faults.push(FaultEvent {
                    key,
                    shard: i,
                    kind,
                    repaired: false,
                });
            }
        }

        let healthy = faults.is_empty();
        if !healthy {
            let recoverable = self.rs.reconstruct(&mut payloads, &present).is_ok();
            for mut fault in faults {
                fault.repaired = recoverable;
                self.log.push(fault);
            }
            if !recoverable {
                return Err(ErasureError::TooManyErasures {
                    have: present.iter().filter(|p| **p).count(),
                    need: self.k,
                });
            }
            // Heal: rewrite the now-correct shards so the next read is clean.
            let healed: Vec<Vec<u8>> = payloads.iter().map(|s| frame(s)).collect();
            self.blobs.insert(key, healed);
        }

        // Reassemble the data from the k data shards and trim the padding.
        let mut joined = Vec::with_capacity(shard_len * self.k);
        for p in payloads.iter().take(self.k) {
            joined.extend_from_slice(p);
        }
        let len = u64::from_le_bytes(joined[..8].try_into().expect("8 bytes of length"));
        let len = usize::try_from(len).unwrap_or(0);
        Ok(joined[8..8 + len].to_vec())
    }

    /// Read and heal every blob, reporting what was found. This is the periodic
    /// scrub a deployment runs to catch latent corruption before a second fault
    /// on the same blob makes it unrecoverable.
    pub fn scrub(&mut self) -> ScrubReport {
        let mut report = ScrubReport::default();
        let before = self.log.len();
        let keys: Vec<u64> = self.blobs.keys().copied().collect();
        for key in keys {
            report.blobs += 1;
            if self.get(key).is_err() {
                report.unrecoverable += 1;
            }
        }
        let new_faults = &self.log[before..];
        report.faults = new_faults.len();
        report.repaired = new_faults.iter().filter(|e| e.repaired).count();
        report
    }

    /// The fault log: every detected fault, in detection order.
    #[must_use]
    pub fn fault_log(&self) -> &[FaultEvent] {
        &self.log
    }

    /// Overwrite one shard of one blob with `bytes`, for tests and fault drills.
    /// A real deployment never calls this; radiation does it instead.
    pub fn corrupt_shard(&mut self, key: u64, shard: usize, bytes: &[u8]) {
        if let Some(shards) = self.blobs.get_mut(&key) {
            if let Some(slot) = shards.get_mut(shard) {
                *slot = bytes.to_vec();
            }
        }
    }
}

/// Frame a shard payload as `CRC32` (little-endian) followed by the payload.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = crc32(payload).to_le_bytes().to_vec();
    out.extend_from_slice(payload);
    out
}

/// Return a framed shard's payload if its checksum matches, else `None`.
fn verify(framed: &[u8]) -> Option<&[u8]> {
    if framed.len() < CRC_LEN {
        return None;
    }
    let (crc_bytes, payload) = framed.split_at(CRC_LEN);
    let stored = u32::from_le_bytes(crc_bytes.try_into().expect("4 bytes"));
    (crc32(payload) == stored).then_some(payload)
}

#[cfg(test)]
mod tests {
    use super::{FaultKind, ResilientStore};
    use crate::erasure::ErasureError;

    /// `SplitMix64`, so corruption drills replay exactly.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            usize::try_from(self.next() % n as u64).expect("fits")
        }
    }

    #[test]
    fn round_trips_without_corruption() {
        let mut store = ResilientStore::new(6, 3).unwrap();
        store.put(1, b"docking sequence nominal").unwrap();
        store.put(2, &[]).unwrap();
        store.put(3, &vec![0xABu8; 5000]).unwrap();
        assert_eq!(store.get(1).unwrap(), b"docking sequence nominal");
        assert_eq!(store.get(2).unwrap(), Vec::<u8>::new());
        assert_eq!(store.get(3).unwrap(), vec![0xABu8; 5000]);
        assert!(store.fault_log().is_empty(), "no faults on a clean store");
    }

    #[test]
    fn heals_up_to_m_corrupt_shards_and_logs_them() {
        let mut rng = Rng(0x5EED);
        let (k, m) = (8usize, 4usize);
        for trial in 0..50 {
            let mut store = ResilientStore::new(k, m).unwrap();
            let data: Vec<u8> = (0..777u32)
                .map(|i| u8::try_from((i ^ trial) & 0xFF).expect("masked"))
                .collect();
            store.put(42, &data).unwrap();

            // Corrupt up to m distinct shards with garbage.
            let bad = rng.below(m + 1);
            let mut hit = std::collections::HashSet::new();
            while hit.len() < bad {
                hit.insert(rng.below(k + m));
            }
            for &s in &hit {
                store.corrupt_shard(42, s, b"radiation flipped me");
            }

            // The data still comes back exactly, and the faults were logged.
            assert_eq!(
                store.get(42).unwrap(),
                data,
                "trial {trial}: data not recovered"
            );
            assert_eq!(
                store.fault_log().len(),
                bad,
                "trial {trial}: expected {bad} faults logged"
            );
            assert!(
                store.fault_log().iter().all(|e| e.repaired),
                "every logged fault should be marked repaired"
            );

            // It healed: a second read finds it clean and logs nothing new.
            let before = store.fault_log().len();
            assert_eq!(store.get(42).unwrap(), data);
            assert_eq!(store.fault_log().len(), before, "re-read should be clean");
        }
    }

    #[test]
    fn too_many_corrupt_shards_is_an_error_not_a_wrong_answer() {
        let mut store = ResilientStore::new(5, 2).unwrap();
        let data = vec![7u8; 200];
        store.put(9, &data).unwrap();
        // Three bad shards with only two parity: unrecoverable.
        store.corrupt_shard(9, 0, b"x");
        store.corrupt_shard(9, 1, b"y");
        store.corrupt_shard(9, 2, b"z");
        let err = store.get(9).unwrap_err();
        assert!(matches!(err, ErasureError::TooManyErasures { .. }));
        // The faults are still recorded, marked unrepaired.
        assert_eq!(store.fault_log().len(), 3);
        assert!(store.fault_log().iter().all(|e| !e.repaired));
    }

    #[test]
    fn a_truncated_shard_is_a_missing_fault() {
        let mut store = ResilientStore::new(4, 2).unwrap();
        store.put(1, b"hello orbit").unwrap();
        store.corrupt_shard(1, 3, &[]); // empty: too short to hold a frame
        assert_eq!(store.get(1).unwrap(), b"hello orbit");
        let event = store.fault_log()[0];
        assert_eq!(event.kind, FaultKind::Missing);
        assert!(event.repaired);
    }

    #[test]
    fn scrub_finds_and_heals_latent_corruption() {
        let mut store = ResilientStore::new(6, 2).unwrap();
        for key in 0..10u64 {
            store
                .put(key, &vec![u8::try_from(key).unwrap(); 300])
                .unwrap();
        }
        // Corrupt one shard in three different blobs.
        store.corrupt_shard(2, 1, b"bad");
        store.corrupt_shard(5, 4, b"bad");
        store.corrupt_shard(8, 0, b"bad");

        let report = store.scrub();
        assert_eq!(report.blobs, 10);
        assert_eq!(report.faults, 3);
        assert_eq!(report.repaired, 3);
        assert_eq!(report.unrecoverable, 0);

        // A second scrub is clean: the first one healed everything.
        let clean = store.scrub();
        assert_eq!(clean.faults, 0);
    }
}
