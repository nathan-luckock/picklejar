//! Property-based tests over the page format.
//!
//! Integration tests (separate compilation unit) because proptest cases are
//! slower than typical unit tests and we want them to run alongside `cargo
//! test --workspace --all-targets` without slowing the unit-test loop.
//!
//! Override the default case budget with `PROPTEST_CASES=N cargo test`.
//! CI bumps it to 512; local default is 256 (proptest's standard).
//!
//! What these tests cover:
//!
//! 1. **Page header round-trip.** For any combination of field values,
//!    `PageHeader::write` then `PageHeader::read` returns the original
//!    struct.
//! 2. **Checksum bit-flip coverage.** A full sweep - every byte offset in
//!    the payload, every bit position - verifies that flipping any single
//!    bit invalidates the stored checksum. The unit test samples 64
//!    offsets; the prop test does the whole 8 KiB.
//! 3. **Insert round-trip.** Any tuple in the legal size range either
//!    inserts and round-trips via `get`, or rejects cleanly with
//!    `PageFull` / `TupleTooLarge` / `EmptyTuple`.
//! 4. **Insert/delete/compact invariants.** For any sequence of insert /
//!    delete / compact operations applied to a heap page, the following
//!    invariants hold after every step:
//!    - `tuple_count + tombstoned_count == slot_count`
//!    - Every live slot reads back the tuple that was inserted into it.
//!    - `free_space()` is consistent with `slot_count` and
//!      `free_space_ptr`.
//!    - Slot IDs of survived live tuples are stable across `compact`.

use proptest::prelude::*;
use rustdb_storage::{
    compute_checksum, recompute_checksum, verify_checksum, BTree, BufferPool, FileManager,
    HeapPage, Page, PageHeader, PageId, PageType, SlotId, StorageError, TupleRef, FLAG_DIRTY,
    FLAG_NEEDS_VACUUM, HEADER_SIZE, HEADER_SIZE_U16, MAX_TUPLE_SIZE, PAGE_SIZE, PAGE_SIZE_U16,
    SLOT_SIZE_U16,
};
use std::collections::{BTreeMap, HashMap};
use std::ops::Bound;

// --- strategies ---

fn page_type_strategy() -> impl Strategy<Value = PageType> {
    prop_oneof![
        Just(PageType::Free),
        Just(PageType::Heap),
        Just(PageType::BTreeInternal),
        Just(PageType::BTreeLeaf),
        Just(PageType::Overflow),
    ]
}

fn page_header_strategy() -> impl Strategy<Value = PageHeader> {
    (
        any::<u64>(),
        any::<u32>(),
        page_type_strategy(),
        any::<u16>(),
        any::<u16>(),
        any::<u16>(),
        any::<u32>(),
    )
        .prop_map(
            |(lsn, checksum, page_type, slot_count, free_space_ptr, flags, reserved)| PageHeader {
                lsn,
                checksum,
                page_type,
                slot_count,
                free_space_ptr,
                flags,
                reserved,
            },
        )
}

// Tuples small enough that a handful fit in a single page.
fn small_tuple_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=256)
}

#[derive(Debug, Clone)]
enum Op {
    Insert(Vec<u8>),
    /// Pick the Nth live slot (modulo live count) and delete it.
    DeleteNth(u16),
    Compact,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Bias toward inserts so we generate enough population to delete from.
        4 => small_tuple_strategy().prop_map(Op::Insert),
        1 => any::<u16>().prop_map(Op::DeleteNth),
        1 => Just(Op::Compact),
    ]
}

// --- test 1: page header round-trip ---

proptest! {
    #[test]
    fn header_round_trip(h in page_header_strategy()) {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        h.write(&mut buf);
        let read_back = PageHeader::read(&buf).expect("read");
        prop_assert_eq!(read_back, h);
    }
}

// --- test 2: full checksum bit-flip sweep ---

proptest! {
    // Heavier per-case work - keep the case count modest.
    #![proptest_config(ProptestConfig {
        cases: 8,
        ..ProptestConfig::default()
    })]

    #[test]
    fn checksum_catches_every_single_bit_flip(seed in any::<u64>()) {
        // Build a deterministic page from the seed (proptest input).
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        PageHeader::new_heap().write(&mut buf);
        // Fill the payload with seed-derived bytes.
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for b in &mut buf[HEADER_SIZE..] {
            state = state.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
            *b = (state >> 56) as u8;
        }
        recompute_checksum(&mut buf);
        prop_assert!(verify_checksum(&buf), "baseline checksum must verify");

        let baseline = compute_checksum(&buf);
        let original = buf.clone();

        // Full sweep: every payload byte, every bit position.
        for offset in HEADER_SIZE..PAGE_SIZE {
            for bit in 0..8u8 {
                buf[offset] ^= 1 << bit;
                let new_sum = compute_checksum(&buf);
                prop_assert_ne!(
                    new_sum, baseline,
                    "flipping byte {} bit {} did not change the checksum", offset, bit
                );
                // Restore.
                buf.copy_from_slice(&*original);
            }
        }
    }
}

// --- test 3: insert / get round-trip across the full legal size range ---

proptest! {
    #[test]
    fn insert_or_error_round_trips(tuple in prop::collection::vec(any::<u8>(), 0..=MAX_TUPLE_SIZE + 32)) {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        let mut page = HeapPage::init(&mut buf);
        match page.insert(&tuple) {
            Ok(id) => {
                prop_assert!(!tuple.is_empty());
                prop_assert!(tuple.len() <= MAX_TUPLE_SIZE);
                prop_assert_eq!(page.get(id), Some(&tuple[..]));
            }
            Err(StorageError::EmptyTuple) => {
                prop_assert!(tuple.is_empty(), "EmptyTuple only valid for zero-length");
            }
            Err(StorageError::TupleTooLarge { size }) => {
                prop_assert_eq!(size, tuple.len());
                prop_assert!(tuple.len() > MAX_TUPLE_SIZE);
            }
            Err(StorageError::PageFull { .. }) => {
                prop_assert!(false, "single insert into empty page should not be PageFull");
            }
            Err(other) => prop_assert!(false, "unexpected error: {:?}", other),
        }
    }
}

// --- test 4: insert/delete/compact invariants under arbitrary op sequences ---

#[derive(Default, Debug)]
struct OracleState {
    /// For each slot ID, the bytes inserted into it (None = tombstoned).
    slots: Vec<Option<Vec<u8>>>,
}

impl OracleState {
    fn live_ids(&self) -> Vec<u16> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                if s.is_some() {
                    Some(u16::try_from(i).expect("slot count is u16"))
                } else {
                    None
                }
            })
            .collect()
    }
}

fn assert_invariants(page: &HeapPage<'_>, oracle: &OracleState) -> Result<(), TestCaseError> {
    let slot_count = page.slot_count();
    prop_assert_eq!(
        slot_count as usize,
        oracle.slots.len(),
        "slot_count drifted from oracle",
    );

    let live_oracle: HashMap<u16, &Vec<u8>> = oracle
        .slots
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            s.as_ref()
                .map(|v| (u16::try_from(i).expect("slot count is u16"), v))
        })
        .collect();

    let tuple_count = page.tuple_count();
    prop_assert_eq!(
        tuple_count as usize,
        live_oracle.len(),
        "tuple_count drifted from oracle",
    );

    // Every live slot must read back the exact bytes from the oracle.
    for (&slot_id, want) in &live_oracle {
        let got = page.get(SlotId::new(slot_id));
        prop_assert_eq!(
            got,
            Some(want.as_slice()),
            "tuple bytes at slot {} drifted",
            slot_id
        );
    }

    // Tombstoned slots must read None.
    for (i, s) in oracle.slots.iter().enumerate() {
        if s.is_none() {
            let id = SlotId::new(u16::try_from(i).expect("slot count is u16"));
            prop_assert_eq!(page.get(id), None, "tombstoned slot {} read as live", i);
        }
    }

    // free_space() should never exceed PAGE_SIZE - HEADER_SIZE - slot_dir_bytes.
    let slot_dir_bytes = slot_count.saturating_mul(SLOT_SIZE_U16);
    let max_free = PAGE_SIZE_U16
        .saturating_sub(HEADER_SIZE_U16)
        .saturating_sub(slot_dir_bytes);
    prop_assert!(
        page.free_space() <= max_free,
        "free_space {} > max_free {}",
        page.free_space(),
        max_free,
    );

    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    #[test]
    fn arbitrary_op_sequence_preserves_invariants(
        ops in prop::collection::vec(op_strategy(), 1..=128)
    ) {
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        let mut page = HeapPage::init(&mut buf);
        let mut oracle = OracleState::default();

        for op in ops {
            match op {
                Op::Insert(tuple) => {
                    if tuple.is_empty() || tuple.len() > MAX_TUPLE_SIZE {
                        // Don't bother - those error paths are covered above.
                        continue;
                    }
                    match page.insert(&tuple) {
                        Ok(id) => {
                            prop_assert_eq!(
                                id,
                                SlotId::new(u16::try_from(oracle.slots.len()).unwrap()),
                                "slot ID drifted from oracle",
                            );
                            oracle.slots.push(Some(tuple));
                        }
                        Err(StorageError::PageFull { .. }) => {
                            // Expected when the page fills up. Oracle unchanged.
                        }
                        Err(other) => prop_assert!(false, "unexpected insert error: {:?}", other),
                    }
                }
                Op::DeleteNth(n) => {
                    let live = oracle.live_ids();
                    if live.is_empty() {
                        continue;
                    }
                    let pick = live[(n as usize) % live.len()];
                    page.delete(SlotId::new(pick)).expect("delete live slot");
                    oracle.slots[pick as usize] = None;
                }
                Op::Compact => {
                    page.compact();
                    // Oracle unchanged - compact preserves logical state.
                }
            }
            assert_invariants(&page, &oracle)?;
        }
    }
}

// --- test 5: FileManager round-trip across random pages ---

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 16,
        ..ProptestConfig::default()
    })]

    #[test]
    fn file_manager_round_trips_random_pages(
        payloads in prop::collection::vec(prop::collection::vec(any::<u8>(), PAGE_SIZE..=PAGE_SIZE), 1..=8)
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prop.db");
        let mut fm = FileManager::open(&path).expect("open");
        let mut ids: Vec<(PageId, Page)> = Vec::new();
        for payload in &payloads {
            let id = fm.allocate_page().expect("allocate");
            let mut page: Page = [0u8; PAGE_SIZE];
            page.copy_from_slice(payload);
            fm.write_page(id, &page).expect("write");
            ids.push((id, page));
        }
        fm.fsync().expect("fsync");

        // Drop and reopen - verify durability + read-back.
        drop(fm);
        let mut fm = FileManager::open(&path).expect("reopen");
        prop_assert_eq!(
            usize::try_from(fm.page_count()).expect("page count fits in usize"),
            payloads.len()
        );

        for (id, expected) in &ids {
            let mut got: Page = [0u8; PAGE_SIZE];
            fm.read_page(*id, &mut got).expect("read");
            prop_assert_eq!(got, *expected);
        }
    }
}

// ============================================================================
// Buffer pool: arbitrary fetch / drop sequences preserve invariants
// ============================================================================

#[derive(Debug, Clone)]
enum PoolOp {
    /// Pin a page id (one of a few candidates) and immediately drop the guard.
    FetchAndDrop(u8),
    /// Allocate a new page, write a byte, drop guard.
    NewPage(u8),
    /// Flush a previously-allocated page.
    Flush(u8),
}

fn pool_op_strategy() -> impl Strategy<Value = PoolOp> {
    prop_oneof![
        any::<u8>().prop_map(PoolOp::FetchAndDrop),
        any::<u8>().prop_map(PoolOp::NewPage),
        any::<u8>().prop_map(PoolOp::Flush),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        ..ProptestConfig::default()
    })]

    #[test]
    fn buffer_pool_invariants_hold_across_op_sequences(
        ops in prop::collection::vec(pool_op_strategy(), 1..=64)
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prop_pool.db");
        let file = FileManager::open(&path).expect("open");
        let pool = BufferPool::new(file, 8);
        let mut allocated: Vec<PageId> = Vec::new();
        // Expected page contents (per allocated page).
        let mut oracle: HashMap<PageId, u8> = HashMap::new();

        for op in ops {
            // Guards always dropped at end of each branch; pinned_count
            // must be zero before the next op.
            prop_assert_eq!(pool.pinned_count(), 0, "pin count leaked from prior op");
            match op {
                PoolOp::NewPage(byte) => {
                    let (id, mut g) = pool.new_page().expect("new");
                    g.page_mut()[0] = byte;
                    allocated.push(id);
                    oracle.insert(id, byte);
                }
                PoolOp::FetchAndDrop(idx) => {
                    if allocated.is_empty() {
                        continue;
                    }
                    let id = allocated[(idx as usize) % allocated.len()];
                    let g = pool.fetch_page(id).expect("fetch");
                    if let Some(&expected) = oracle.get(&id) {
                        prop_assert_eq!(g.page()[0], expected, "page contents drifted");
                    }
                }
                PoolOp::Flush(idx) => {
                    if allocated.is_empty() {
                        continue;
                    }
                    let id = allocated[(idx as usize) % allocated.len()];
                    pool.flush_page(id).expect("flush");
                }
            }
        }

        // Final: every allocated page reads back its expected byte.
        for (id, &want) in &oracle {
            let g = pool.fetch_page(*id).expect("final fetch");
            prop_assert_eq!(g.page()[0], want, "page {} mismatched", id);
        }
    }
}

// ============================================================================
// B+ tree: invariants under arbitrary insert sequences
// ============================================================================

/// Walk all leaves from the left-most. Returns the keys in the order they
/// appear (which must be sorted ascending for a valid B+ tree).
fn walk_leaf_chain(tree: &BTree<'_>, pool: &BufferPool) -> Result<Vec<u64>, StorageError> {
    // Walk down from the root following first_child until we hit a leaf.
    let mut current = tree.root_page();
    loop {
        let guard = pool.fetch_page(current)?;
        let header = PageHeader::read(guard.page())?;
        match header.page_type {
            PageType::BTreeLeaf => break,
            PageType::BTreeInternal => {
                // first_child is at the documented offset; cheat by using
                // find_child(0) which falls through to first_child when
                // the node has keys, and returns first_child for empty
                // internals.
                drop(guard);
                let g2 = pool.fetch_page(current)?;
                let raw_first = u64::from_le_bytes(g2.page()[26..34].try_into().expect("8 bytes"));
                current = PageId::new(raw_first);
            }
            _ => panic!("unexpected page type in tree"),
        }
    }
    // current is the left-most leaf.
    let mut keys = Vec::new();
    loop {
        let guard = pool.fetch_page(current)?;
        let count = u16::from_le_bytes(guard.page()[24..26].try_into().expect("2 bytes"));
        for i in 0..count {
            let off = 34 + (i as usize) * 18;
            let k = u64::from_le_bytes(guard.page()[off..off + 8].try_into().expect("8 bytes"));
            keys.push(k);
        }
        let next_raw = u64::from_le_bytes(guard.page()[26..34].try_into().expect("8 bytes"));
        let next = PageId::new(next_raw);
        if next.is_invalid() {
            break;
        }
        drop(guard);
        current = next;
    }
    Ok(keys)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 32,
        ..ProptestConfig::default()
    })]

    #[test]
    fn btree_keys_are_sorted_after_arbitrary_inserts(
        keys in prop::collection::vec(any::<u64>(), 1..=600)
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prop_tree.db");
        let file = FileManager::open(&path).expect("open");
        let pool = BufferPool::new(file, 64);
        let tree = BTree::create(&pool).expect("create");
        let oracle_tref = TupleRef::new(PageId::new(1), SlotId::new(0));

        let mut oracle: BTreeMap<u64, TupleRef> = BTreeMap::new();
        for k in &keys {
            if oracle.contains_key(k) {
                continue;
            }
            tree.insert(*k, oracle_tref).expect("insert");
            oracle.insert(*k, oracle_tref);
        }

        // Sibling chain must be sorted.
        let chain = walk_leaf_chain(&tree, &pool).expect("walk");
        let mut chain_sorted = chain.clone();
        chain_sorted.sort_unstable();
        prop_assert_eq!(chain, chain_sorted, "sibling chain is not sorted");
    }

    #[test]
    fn btree_search_matches_oracle(
        keys in prop::collection::vec(any::<u64>(), 1..=600),
        queries in prop::collection::vec(any::<u64>(), 1..=64)
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prop_tree2.db");
        let file = FileManager::open(&path).expect("open");
        let pool = BufferPool::new(file, 64);
        let tree = BTree::create(&pool).expect("create");
        let tref = TupleRef::new(PageId::new(1), SlotId::new(0));

        let mut oracle: BTreeMap<u64, TupleRef> = BTreeMap::new();
        for k in &keys {
            if oracle.contains_key(k) { continue; }
            tree.insert(*k, tref).expect("insert");
            oracle.insert(*k, tref);
        }
        for q in &queries {
            prop_assert_eq!(tree.search(*q).expect("search"), oracle.get(q).copied());
        }
        // Every inserted key must also be findable.
        for k in oracle.keys() {
            prop_assert_eq!(tree.search(*k).expect("search"), Some(tref));
        }
    }

    #[test]
    fn btree_range_scan_matches_oracle(
        keys in prop::collection::vec(any::<u64>(), 1..=600),
        lo in any::<u64>(),
        hi in any::<u64>()
    ) {
        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("prop_tree3.db");
        let file = FileManager::open(&path).expect("open");
        let pool = BufferPool::new(file, 64);
        let tree = BTree::create(&pool).expect("create");
        let tref = TupleRef::new(PageId::new(1), SlotId::new(0));

        let mut oracle: BTreeMap<u64, TupleRef> = BTreeMap::new();
        for k in &keys {
            if oracle.contains_key(k) { continue; }
            tree.insert(*k, tref).expect("insert");
            oracle.insert(*k, tref);
        }

        // Inclusive range scan compared against the oracle's filter.
        let got: Vec<u64> = tree
            .range_scan(Bound::Included(lo), Bound::Included(hi))
            .expect("scan")
            .map(|r| r.unwrap().0)
            .collect();
        let want: Vec<u64> = oracle
            .range(lo..=hi)
            .map(|(k, _)| *k)
            .collect();
        prop_assert_eq!(got, want);
    }
}

// --- sanity: re-export coverage check that all crate symbols still link ---

#[test]
fn public_surface_links() {
    // Touch all re-exports so a future refactor that drops one fails compilation
    // before it lands in master.
    let _ = (
        HEADER_SIZE,
        HEADER_SIZE_U16,
        PAGE_SIZE,
        PAGE_SIZE_U16,
        MAX_TUPLE_SIZE,
        SLOT_SIZE_U16,
        FLAG_DIRTY,
        FLAG_NEEDS_VACUUM,
        PageType::Heap,
    );
}
