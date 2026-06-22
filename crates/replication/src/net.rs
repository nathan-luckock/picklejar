//! Real TCP transport for the replicated memory layer.
//!
//! Phases 1-3 proved convergence in a single deterministic process. This runs
//! the same model for real: each node is a TCP server owning its own CRDT memory
//! store ([`picklejar::crdtmem::Replica`]), and a [`Coordinator`] places, writes,
//! reads, and reconciles across nodes over the network. The wire protocol is
//! hand-framed (a 4-byte big-endian length prefix per message), with no
//! serialization dependency.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use picklejar::antientropy::MerkleSet;
use picklejar::blindvec::l2_sq;
use picklejar::consistenthash::HashRing;
use picklejar::crdtmem::{Replica, Slot};

const REQ_PUT: u8 = 1;
const REQ_GET: u8 = 2;
const REQ_PULL: u8 = 3;
const REQ_KNN: u8 = 4;
const REQ_ROOT: u8 = 5;
const VNODES: u32 = 64;
const MERKLE_DEPTH: u32 = 8;
const TIMEOUT: Duration = Duration::from_secs(2);

/// The Merkle root over a replica's slots, used to skip anti-entropy between two
/// nodes that already agree (an identical-replica check costs one 32-byte
/// exchange instead of shipping any state).
fn store_root(store: &Arc<Mutex<Replica>>) -> [u8; 32] {
    let entries: Vec<(u64, Vec<u8>)> = {
        let guard = store.lock().expect("store lock");
        guard
            .slots()
            .iter()
            .map(|(k, s)| {
                let mut b = Vec::new();
                put_slot(&mut b, s);
                (*k, b)
            })
            .collect()
    };
    MerkleSet::from_entries(MERKLE_DEPTH, &entries).root()
}

fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = usize::try_from(u32::from_be_bytes(len_buf)).unwrap_or(0);
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_be_bytes());
}
fn put_bytes(b: &mut Vec<u8>, v: &[u8]) {
    put_u32(b, u32::try_from(v.len()).unwrap_or(u32::MAX));
    b.extend_from_slice(v);
}
fn get_u32(b: &[u8], off: &mut usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[*off..*off + 4]);
    *off += 4;
    u32::from_be_bytes(a)
}
fn get_u64(b: &[u8], off: &mut usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[*off..*off + 8]);
    *off += 8;
    u64::from_be_bytes(a)
}
fn get_bytes(b: &[u8], off: &mut usize) -> Vec<u8> {
    let n = usize::try_from(get_u32(b, off)).unwrap_or(0);
    let v = b[*off..*off + n].to_vec();
    *off += n;
    v
}
fn put_slot(b: &mut Vec<u8>, s: &Slot) {
    put_u64(b, s.ts);
    put_u64(b, s.origin);
    match &s.value {
        Some(v) => {
            b.push(1);
            put_bytes(b, v);
        }
        None => b.push(0),
    }
}
fn put_f64(b: &mut Vec<u8>, v: f64) {
    b.extend_from_slice(&v.to_bits().to_be_bytes());
}
fn get_f64(b: &[u8], off: &mut usize) -> f64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[*off..*off + 8]);
    *off += 8;
    f64::from_bits(u64::from_be_bytes(a))
}
fn put_vec_f32(b: &mut Vec<u8>, v: &[f32]) {
    put_u32(b, u32::try_from(v.len()).unwrap_or(u32::MAX));
    for x in v {
        b.extend_from_slice(&x.to_bits().to_be_bytes());
    }
}
fn get_vec_f32(b: &[u8], off: &mut usize) -> Vec<f32> {
    let n = usize::try_from(get_u32(b, off)).unwrap_or(0);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut a = [0u8; 4];
        a.copy_from_slice(&b[*off..*off + 4]);
        *off += 4;
        out.push(f32::from_bits(u32::from_be_bytes(a)));
    }
    out
}

/// Encode a memory record (its embedding and payload) as the value bytes the
/// replicated CRDT map stores.
fn encode_memory(embedding: &[f32], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_vec_f32(&mut b, embedding);
    put_bytes(&mut b, payload);
    b
}

/// Decode a memory record back into its embedding and payload.
fn decode_memory(bytes: &[u8]) -> (Vec<f32>, Vec<u8>) {
    let mut off = 0;
    let embedding = get_vec_f32(bytes, &mut off);
    let payload = if off < bytes.len() {
        get_bytes(bytes, &mut off)
    } else {
        Vec::new()
    };
    (embedding, payload)
}

fn get_slot(b: &[u8], off: &mut usize) -> Slot {
    let ts = get_u64(b, off);
    let origin = get_u64(b, off);
    let has = b[*off];
    *off += 1;
    let value = if has == 1 {
        Some(get_bytes(b, off))
    } else {
        None
    };
    Slot { value, ts, origin }
}

/// A replication node: a TCP server owning one CRDT memory replica.
#[derive(Debug)]
pub struct Node {
    id: u64,
    store: Arc<Mutex<Replica>>,
}

impl Node {
    /// A fresh node with a stable id and an empty store.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            store: Arc::new(Mutex::new(Replica::new(id))),
        }
    }

    /// The node's id.
    #[must_use]
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// A handle to the node's store, for anti-entropy or inspection.
    #[must_use]
    pub fn store(&self) -> Arc<Mutex<Replica>> {
        Arc::clone(&self.store)
    }

    /// Serve peer and client connections on `listener` forever, one thread per
    /// connection. Runs until the process ends.
    pub fn serve(&self, listener: &TcpListener) {
        for stream in listener.incoming().flatten() {
            let store = Arc::clone(&self.store);
            thread::spawn(move || {
                let mut stream = stream;
                let _ = serve_conn(&mut stream, &store);
            });
        }
    }
}

fn serve_conn(stream: &mut TcpStream, store: &Arc<Mutex<Replica>>) -> io::Result<()> {
    loop {
        let Ok(req) = read_frame(stream) else {
            return Ok(());
        };
        let resp = handle(&req, store);
        write_frame(stream, &resp)?;
    }
}

fn handle(req: &[u8], store: &Arc<Mutex<Replica>>) -> Vec<u8> {
    match req.first().copied() {
        Some(REQ_PUT) => {
            let mut off = 1;
            let key = get_u64(req, &mut off);
            let value = get_bytes(req, &mut off);
            store.lock().expect("store lock").set(key, &value);
            vec![REQ_PUT]
        }
        Some(REQ_GET) => {
            let mut off = 1;
            let key = get_u64(req, &mut off);
            let slot = store.lock().expect("store lock").slots().get(&key).cloned();
            let mut out = vec![REQ_GET];
            if let Some(s) = slot {
                out.push(1);
                put_slot(&mut out, &s);
            } else {
                out.push(0);
            }
            out
        }
        Some(REQ_PULL) => {
            let entries: Vec<(u64, Slot)> = {
                let guard = store.lock().expect("store lock");
                guard.slots().iter().map(|(k, s)| (*k, s.clone())).collect()
            };
            let mut out = vec![REQ_PULL];
            put_u32(&mut out, u32::try_from(entries.len()).unwrap_or(u32::MAX));
            for (k, s) in &entries {
                put_u64(&mut out, *k);
                put_slot(&mut out, s);
            }
            out
        }
        Some(REQ_KNN) => {
            let mut off = 1;
            let query = get_vec_f32(req, &mut off);
            let k = usize::try_from(get_u32(req, &mut off)).unwrap_or(0);
            let memories: Vec<(u64, Vec<u8>)> = {
                let guard = store.lock().expect("store lock");
                guard
                    .slots()
                    .iter()
                    .filter_map(|(id, s)| s.value.as_ref().map(|v| (*id, v.clone())))
                    .collect()
            };
            let mut hits: Vec<(u64, f64, Vec<u8>)> = memories
                .iter()
                .map(|(id, val)| {
                    let (embedding, payload) = decode_memory(val);
                    (*id, l2_sq(&query, &embedding), payload)
                })
                .collect();
            hits.sort_by(|a, b| a.1.total_cmp(&b.1));
            hits.truncate(k);
            let mut out = vec![REQ_KNN];
            put_u32(&mut out, u32::try_from(hits.len()).unwrap_or(u32::MAX));
            for (id, dist, payload) in &hits {
                put_u64(&mut out, *id);
                put_f64(&mut out, *dist);
                put_bytes(&mut out, payload);
            }
            out
        }
        Some(REQ_ROOT) => {
            let mut out = vec![REQ_ROOT];
            out.extend_from_slice(&store_root(store));
            out
        }
        _ => vec![255],
    }
}

fn request(addr: &str, req: &[u8]) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    write_frame(&mut stream, req)?;
    read_frame(&mut stream)
}

/// Pull a peer's full state over TCP and merge it into `store` (anti-entropy).
/// Returns the number of slots received.
///
/// # Errors
/// Returns an error if the peer cannot be reached or the response is malformed.
pub fn pull_into(store: &Arc<Mutex<Replica>>, peer: &str) -> io::Result<usize> {
    // Skip the sync entirely when the peer's Merkle root matches ours: two
    // replicas that already agree exchange 32 bytes and ship nothing.
    let root_resp = request(peer, &[REQ_ROOT])?;
    if root_resp.first() == Some(&REQ_ROOT)
        && root_resp.len() >= 33
        && root_resp[1..33] == store_root(store)
    {
        return Ok(0);
    }

    let resp = request(peer, &[REQ_PULL])?;
    if resp.first() != Some(&REQ_PULL) {
        return Ok(0);
    }
    let mut off = 1;
    let count = usize::try_from(get_u32(&resp, &mut off)).unwrap_or(0);
    let mut pairs = Vec::with_capacity(count);
    for _ in 0..count {
        let key = get_u64(&resp, &mut off);
        let slot = get_slot(&resp, &mut off);
        pairs.push((key, slot));
    }
    {
        let mut guard = store.lock().expect("store lock");
        for (key, slot) in pairs {
            guard.merge_slot(key, slot);
        }
    }
    Ok(count)
}

/// One result of a distributed nearest-neighbor recall.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The memory's id.
    pub id: u64,
    /// Its squared L2 distance to the query (the engine's `l2_sq`).
    pub distance: f64,
    /// The memory's stored payload (its content bytes).
    pub payload: Vec<u8>,
}

/// Encode a node's full state (every slot) for a durable on-disk snapshot.
#[must_use]
pub fn snapshot(store: &Arc<Mutex<Replica>>) -> Vec<u8> {
    let entries: Vec<(u64, Slot)> = {
        let guard = store.lock().expect("store lock");
        guard.slots().iter().map(|(k, s)| (*k, s.clone())).collect()
    };
    let mut out = Vec::new();
    put_u32(&mut out, u32::try_from(entries.len()).unwrap_or(u32::MAX));
    for (k, s) in &entries {
        put_u64(&mut out, *k);
        put_slot(&mut out, s);
    }
    out
}

/// Rebuild a replica for node `id` from a snapshot made by [`snapshot`].
///
/// Slots keep their `(ts, origin)` versions, so a reloaded node merges cleanly
/// with peers and any writes it missed while down arrive by anti-entropy.
#[must_use]
pub fn restore(id: u64, bytes: &[u8]) -> Replica {
    let mut replica = Replica::new(id);
    if bytes.is_empty() {
        return replica;
    }
    let mut off = 0;
    let count = usize::try_from(get_u32(bytes, &mut off)).unwrap_or(0);
    for _ in 0..count {
        let key = get_u64(bytes, &mut off);
        let slot = get_slot(bytes, &mut off);
        replica.merge_slot(key, slot);
    }
    replica
}

/// Places keys on a ring of nodes and drives quorum writes and reads over TCP.
#[derive(Debug)]
pub struct Coordinator {
    ring: HashRing,
    addrs: BTreeMap<u64, String>,
    rf: usize,
    r: usize,
    w: usize,
}

impl Coordinator {
    /// A coordinator over `nodes` (id, address) with replication factor `rf` and
    /// read/write quorums `r` / `w`.
    #[must_use]
    pub fn new(nodes: &[(u64, String)], rf: usize, r: usize, w: usize) -> Self {
        let mut ring = HashRing::new(VNODES);
        let mut addrs = BTreeMap::new();
        for (id, addr) in nodes {
            ring.add_node(&id.to_string());
            addrs.insert(*id, addr.clone());
        }
        Self {
            ring,
            addrs,
            rf,
            r,
            w,
        }
    }

    /// The replica node ids responsible for `key`.
    #[must_use]
    pub fn replica_ids(&self, key: u64) -> Vec<u64> {
        self.ring
            .preference(&key.to_be_bytes(), self.rf)
            .into_iter()
            .filter_map(|n| n.parse().ok())
            .collect()
    }

    /// Write `value` for `key` to its replicas; returns how many acknowledged.
    /// At least `w` is a durable write; one or more is an available (degraded)
    /// write under partition.
    #[must_use]
    pub fn write(&self, key: u64, value: &[u8]) -> usize {
        let mut req = vec![REQ_PUT];
        put_u64(&mut req, key);
        put_bytes(&mut req, value);
        let mut acks = 0;
        for id in self.replica_ids(key) {
            if let Some(addr) = self.addrs.get(&id) {
                if let Ok(resp) = request(addr, &req) {
                    if resp.first() == Some(&REQ_PUT) {
                        acks += 1;
                    }
                }
            }
        }
        acks
    }

    /// Read `key`, reconciling the reachable replicas by `(ts, origin)` version,
    /// and return the winning live value.
    #[must_use]
    pub fn read(&self, key: u64) -> Option<Vec<u8>> {
        let mut req = vec![REQ_GET];
        put_u64(&mut req, key);
        let mut best: Option<Slot> = None;
        for id in self.replica_ids(key) {
            if let Some(addr) = self.addrs.get(&id) {
                if let Ok(resp) = request(addr, &req) {
                    if resp.get(1) == Some(&1) {
                        let mut off = 2;
                        let s = get_slot(&resp, &mut off);
                        let take = match &best {
                            Some(b) => (s.ts, s.origin) > (b.ts, b.origin),
                            None => true,
                        };
                        if take {
                            best = Some(s);
                        }
                    }
                }
            }
        }
        best.and_then(|s| s.value)
    }

    /// Store a memory (its embedding and payload) under `id` on the key's
    /// replicas. Returns how many replicas acknowledged.
    #[must_use]
    pub fn store_memory(&self, id: u64, embedding: &[f32], payload: &[u8]) -> usize {
        self.write(id, &encode_memory(embedding, payload))
    }

    /// Distributed nearest-neighbor recall: scatter the query to every node,
    /// each returns its local top-`k` by the engine's `l2_sq` distance, and the
    /// merged global top-`k` is returned. A memory replicated on several nodes is
    /// de-duplicated by id.
    #[must_use]
    pub fn recall(&self, query: &[f32], k: usize) -> Vec<Hit> {
        let mut req = vec![REQ_KNN];
        put_vec_f32(&mut req, query);
        put_u32(&mut req, u32::try_from(k).unwrap_or(u32::MAX));
        let mut hits: Vec<Hit> = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for addr in self.addrs.values() {
            let Ok(resp) = request(addr, &req) else {
                continue;
            };
            if resp.first() != Some(&REQ_KNN) {
                continue;
            }
            let mut off = 1;
            let count = usize::try_from(get_u32(&resp, &mut off)).unwrap_or(0);
            for _ in 0..count {
                let id = get_u64(&resp, &mut off);
                let distance = get_f64(&resp, &mut off);
                let payload = get_bytes(&resp, &mut off);
                if seen.insert(id) {
                    hits.push(Hit {
                        id,
                        distance,
                        payload,
                    });
                }
            }
        }
        hits.sort_by(|a, b| a.distance.total_cmp(&b.distance));
        hits.truncate(k);
        hits
    }

    /// The read quorum.
    #[must_use]
    pub const fn read_quorum(&self) -> usize {
        self.r
    }

    /// The write quorum.
    #[must_use]
    pub const fn write_quorum(&self) -> usize {
        self.w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_node(id: u64) -> (String, Arc<Mutex<Replica>>) {
        let node = Node::new(id);
        let store = node.store();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        thread::spawn(move || node.serve(&listener));
        (addr, store)
    }

    #[test]
    fn slot_codec_roundtrips() {
        let s = Slot {
            value: Some(b"abc".to_vec()),
            ts: 5,
            origin: 2,
        };
        let mut buf = Vec::new();
        put_slot(&mut buf, &s);
        let mut off = 0;
        assert_eq!(get_slot(&buf, &mut off), s);

        let tomb = Slot {
            value: None,
            ts: 9,
            origin: 1,
        };
        let mut buf2 = Vec::new();
        put_slot(&mut buf2, &tomb);
        let mut off2 = 0;
        assert_eq!(get_slot(&buf2, &mut off2), tomb);
    }

    #[test]
    fn replicates_and_reads_over_tcp() {
        let nodes: Vec<(u64, String)> = (0..3)
            .map(|id| {
                let (addr, _store) = spawn_node(id);
                (id, addr)
            })
            .collect();
        let coord = Coordinator::new(&nodes, 3, 2, 2);

        let acks = coord.write(42, b"hello");
        assert!(acks >= 2, "write reached a quorum, got {acks}");
        assert_eq!(coord.read(42), Some(b"hello".to_vec()));
        assert_eq!(coord.read(999), None, "an unknown key reads as absent");
    }

    #[test]
    fn distributed_knn_finds_the_nearest_across_nodes() {
        let nodes: Vec<(u64, String)> = (0..3)
            .map(|id| {
                let (addr, _store) = spawn_node(id);
                (id, addr)
            })
            .collect();
        let coord = Coordinator::new(&nodes, 2, 1, 1);

        // Memories spread across the cluster by placement.
        assert!(coord.store_memory(1, &[0.1, 0.2, 0.9], b"sky") >= 1);
        assert!(coord.store_memory(2, &[0.9, 0.1, 0.1], b"fire") >= 1);
        assert!(coord.store_memory(3, &[0.1, 0.2, 0.8], b"ocean") >= 1);

        // A query nearest to memory 3, then memory 1; memory 2 is far.
        let hits = coord.recall(&[0.1, 0.2, 0.82], 2);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].id, 3, "nearest is memory 3");
        assert_eq!(hits[0].payload, b"ocean");
        let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        assert!(
            !ids.contains(&2),
            "the far memory is not in the top 2: {ids:?}"
        );
    }

    #[test]
    fn snapshot_restore_roundtrips() {
        let node = Node::new(5);
        let store = node.store();
        {
            let mut s = store.lock().expect("lock");
            s.set(1, b"a");
            s.set(2, b"b");
        }
        let bytes = snapshot(&store);
        let restored = restore(5, &bytes);
        assert_eq!(restored.get(1), Some(b"a".as_slice()));
        assert_eq!(restored.get(2), Some(b"b".as_slice()));
    }

    #[test]
    fn anti_entropy_pull_converges_over_tcp() {
        let (addr_a, store_a) = spawn_node(0);
        let (_addr_b, store_b) = spawn_node(1);
        {
            let mut a = store_a.lock().expect("lock");
            a.set(7, b"x");
            a.set(8, b"y");
        }
        let merged = pull_into(&store_b, &addr_a).expect("pull");
        assert_eq!(merged, 2);
        // Now identical: a second pull sees matching Merkle roots and skips.
        assert_eq!(
            pull_into(&store_b, &addr_a).expect("pull"),
            0,
            "already in sync, nothing transferred"
        );
        let (v7, v8) = {
            let b = store_b.lock().expect("lock");
            (b.get(7).map(<[u8]>::to_vec), b.get(8).map(<[u8]>::to_vec))
        };
        assert_eq!(v7, Some(b"x".to_vec()));
        assert_eq!(v8, Some(b"y".to_vec()));
    }
}
