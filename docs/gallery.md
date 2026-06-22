<div align="center">

# picklejar gallery

Every runnable demo and hand-built primitive, grouped.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Roadmap](ROADMAP.md)

</div>

---

Each is a standalone binary. Run any of them with `cargo run --release --bin <name>`.
Nothing here pulls in a library for the thing it demonstrates: the checksums, the
finite-field math, the SHA-256, and the index structures are all written from
scratch in this repo.

## Start here

| Binary | What it does |
|---|---|
| `attest` | The grand attestation: one content-hashed page proving every guarantee at once. |
| `scorecard` | One reproducible page: live throughput plus the 20 proven invariants. |
| `vecert` | The AI-memory-layer reliability certificate, regenerable from this commit. |
| `demo` | A narrated walk through picklejar's headline features, driven against the live engine. |
| `dbstat` | A one-command summary of any database file: every table, its row count, and its size. |

## Durability, recovery, and self-healing

| Binary | What it does |
|---|---|
| `dst` | 1,000,000 reproducible crash-and-recover scenarios against a fault-injecting disk. |
| `vecsim` | Deterministic crash and isolation simulation for the vector memory layer. |
| `faultsim` | Detection coverage across the four storage-write fault classes (bit flip, torn, lost, misdirected). |
| `resilientsim` | The self-healing erasure-coded store under sustained, deterministic corruption. |
| `resilientdemo` | A corruption drill for the erasure-coded store, with the mass-efficiency numbers. |
| `retrievesim` | Proof of retrievability: challenge an unreachable node to prove it still holds your data. |
| `pjbackup` | Operational snapshot backup: heal from parity first, then copy a consistent image. |
| `pjscrub` | The scrubber a deployment runs on a schedule: heal corrupt heap pages, refresh parity. |

## Formal verification

| Binary | What it does |
|---|---|
| `walmodel` | Exhaustively model-check the WAL ordering invariant: no page change is durable ahead of its log record. |
| `rlsmodel` | Exhaustively model-check that the approximate-index cache never serves a leaked, stale, or cross-tenant row. |
| `difftest` | Differential testing: random SQL run through picklejar and SQLite, compared as a sorted multiset. |

## Vector and AI-memory layer

| Binary | What it does |
|---|---|
| `vecbench` | How much faster the approximate HNSW index is than brute force, and at what recall. |
| `vecsqlbench` | End-to-end: the cached SQL index path versus an exact scan. |
| `quantsim` | Drift-adaptive vector quantization: recall held flat under distribution drift at a fixed memory budget. |
| `pqsim` | Product quantization: 16x-smaller embeddings that still rank correctly. |
| `lshsim` | Hyperplane LSH: similar embeddings land in one bucket, turning a scan into a bucket lookup. |
| `memload` | Populate a database with a realistic multi-tenant AI-memory corpus. |

## Cryptographic guarantees (hand-written)

| Binary | What it does |
|---|---|
| `authknn` | Authenticated nearest-neighbor: verify the answer from a node you do not trust. |
| `authsqlsim` | Authenticated SQL: verify a `WHERE` query result without trusting the server. |
| `blindsim` | Blind vector search: the server ranks your nearest memories without ever seeing them. |
| `pirsim` | Private information retrieval: fetch a memory the server cannot identify. |
| `homoaggsim` | Private aggregates: `SUM` and `AVG` over values no server can read. |
| `forgetsim` | Provable forgetting: a memory becomes unrecoverable even to an adversary with the disk. |
| `forwardlogsim` | Forward-secure audit log: a seized node cannot rewrite its own past. |
| `ledgersim` | Verifiable history: a tamper-evident ledger catches a forger who re-signed the whole log. |
| `shamirsim` | Shamir secret sharing: split a key so any 3 of 5 nodes can reconstruct it, fewer cannot. |
| `captokensim` | Capability tokens: a node verifies scoped, expiring grants offline. |

## Distributed systems

| Binary | What it does |
|---|---|
| `crdtsim` | Conflict-free replicated memory: two partitioned nodes edit offline and merge cleanly. |
| `crdtvecsim` | A CRDT similarity index: two partitioned nodes reconcile their indexes with no conflict. |
| `quorumsim` | Quorum-replicated memory: stays available and consistent while a node is down (`r + w > rf`). |
| `vclocksim` | Vector clocks: distinguish a causal update from a concurrent conflict. |
| `syncsim` | Merkle anti-entropy: two replicas reconcile by exchanging hashes along divergent branches only. |
| `ringsim` | Consistent hashing: a node joins the fleet and steals only its fair share of keys. |
| `hrwsim` | Rendezvous (HRW) hashing: weighted sharding with minimal movement and no ring state. |
| `ratesim` | Token-bucket rate limiting: a tenant bursts, gets throttled, and recovers. |

## Probabilistic and streaming structures

| Binary | What it does |
|---|---|
| `hllsim` | HyperLogLog: estimate distinct memories in ~16 KiB at any scale. |
| `cmsim` | Count-Min sketch: estimate per-memory access frequency in fixed space. |
| `bloomsim` | Bloom filter: a duplicate pre-check in a few bits per memory. |
| `countbloomsim` | Counting Bloom filter: a membership set a forgotten memory can leave. |
| `cuckoosim` | Cuckoo filter: deletable membership with a short fingerprint per memory. |
| `heavysim` | Space-Saving: recover the hottest memories from a skewed stream in K slots. |
| `quantilesim` | Streaming quantile sketch: latency percentiles in fixed space. |
| `reservoirsim` | Reservoir sampling: a uniform sample of a memory stream in one pass. |
| `roaringsim` | Roaring bitmap: compact id sets with chunk-wise union and intersection. |
| `skiplistsim` | Skip list: an ordered index with expected-logarithmic search, no rebalancing. |
