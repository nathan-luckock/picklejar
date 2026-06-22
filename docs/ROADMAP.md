<div align="center">

# picklejar roadmap

A proof-driven engine for hardware nobody can reach. What is built, and where the frontier is.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Build log](sprints.md)

</div>

---

## The thesis

For hardware you cannot physically service, the only acceptable reliability is
**proven before deployment**. You cannot attach a debugger to a satellite, swap a
corrupted drive in orbit, or ship a patch when the link is down for nine hours. So
the interesting property of a data layer for those places is not that it is fast
or does vector search. Plenty of things do that. It is that it can hand you a
**machine-checkable, regenerable proof that it survives the exact fault model of
its environment**, including that no tenant's data is ever silently corrupted or
leaked. That proof is what the project is built around, and most of it is built.

## What is built

The relational engine is complete and crash-proven: storage, a write-ahead log
with ARIES recovery, MVCC snapshot isolation, a cost-based planner, roles,
row-level security, and the PostgreSQL wire protocol, verified by 1,000,000
deterministic crash simulations and differentially tested against SQLite.

On top of it, the reliability infrastructure for AI memory, all shipped:

- **Vector memory.** A `VECTOR(n)` type, four distance operators, brute-force KNN,
  and a durable HNSW index, reachable from SQL through a cached, write-invalidated,
  RLS-safe path (~150x faster warm), so a tenant's search can only ever rank its
  own vectors.
- **Drift-adaptive quantization.** A scalar-quantized index at one byte per
  dimension (4x smaller) holds recall near the full-precision ceiling (~0.97 vs
  ~0.005 for a calibrate-once quantizer) as the distribution drifts, recalibrating
  from durable rows on under 2% of inserts. This is the one place the engine makes
  a benchmarked contribution rather than re-implementing solved art (`quantsim`,
  certified in `vecert`).
- **Contradiction detection.** `INSERT ... ON CONFLICT (key) DO ASSERT` rejects a
  write that records a *different* value for a held key, naming the column, key,
  and both values, instead of silently overwriting a belief. The unsolved
  AI-memory consistency problem, enforced by the engine.
- **Self-healing storage.** A from-scratch Reed-Solomon code (GF(2^8)) backs a
  block store and a page-heap parity layer: `protect(k, m)` writes a parity
  snapshot, `open_resilient` reconstructs any corrupt heap page before the engine
  reads it, at `m/k` overhead instead of `+m*100%`. Operated by a `PROTECT`
  statement, a durable fault log (`pg_fault_log`), and the `pjscrub` scrubber.
- **A modeled fault environment.** The deterministic simulator irradiates a
  committed multi-tenant workload across every persistent file at a named orbit's
  single-event-upset rate, and proves it never serves a silently corrupted answer.
  `faultsim` measures detection across all four storage-write fault classes (bit
  flip, torn, lost, misdirected).
- **Correctness oracles.** `recall@k` gated in CI against the brute-force oracle
  (which found and fixed a real 0.47-to-0.98 bug), plus a metamorphic oracle
  (self-retrieval, monotonicity, deletion consistency) for approximate search where
  no ground truth exists.
- **Bitemporal time travel.** Two read-only as-of axes on a parser-safe session
  mechanism that sidesteps the `AS OF` collision: valid-time rewinds reads on
  temporal tables to what was true in the world then; transaction-time travels to
  the MVCC snapshot of a past transaction, what the database knew then.
- **Backup, PITR, and a physical standby.** Snapshot backup and a logical
  point-in-time restore that rebuilds a fresh database as of a past transaction.
  The physical engine underneath (`redo_through` plus logged `IndexUpdate`
  mappings) rebuilds an `MvccTable`'s heap and primary index from the log to an
  arbitrary LSN, validated against an independent model across 40 seeded workloads,
  and adds no fsync to the write path.
- **Exhaustive model-checking.** From-scratch bounded model checkers prove five
  core invariants over *every* reachable interleaving, each with a buggy variant
  that yields a counterexample so the proof is not vacuous: WAL ordering, MVCC
  read-stability, and through the approximate index, tenant isolation, cache
  freshness, and valid-time travel. No vector or AI-memory database is known to
  model-check its filtered retrieval this way.
- **A WAL-logged catalog and isolation state.** Schema and row-level-security
  changes are logged and replayed on open, so a crash can never silently drop a
  tenant fence, and forward replay reconstructs later schema state, not just the
  base.

So the engine detects every corruption it is built to catch, repairs from
redundancy with no human in the loop, never serves a silently wrong or
cross-tenant answer, and proves it three independent ways: 1,000,000-seed crash
sampling, an independent SQLite oracle, and exhaustive model-checking.

## The frontier

What is genuinely still ahead, stated honestly:

- **Physical-restore SQL wiring (engine built).** A table's heap and primary index
  already rebuild from the log to any LSN. What remains is the `Database`-level
  wiring across a whole multi-index database (secondary indexes, page anchors,
  sidecars) behind one `restore_physical_to_lsn`. Logical restore covers the need
  today; the physical path adds speed and MVCC-history fidelity.
- **Partition tolerance.** Folded into the replication path, not built ahead of it:
  the node serves locally with bounded staleness while a link is down, and
  reconciles on reconnect.
- **A self-identifying page id.** The one residual fault class: a misdirected write
  that lands *newer* content slips, because the page format stores no id to check
  its location against. A header page-id guard (a page-format change) closes it.
- **Deeper fault coverage.** Per-part, shielding-aware upset rates that only a
  specific flight build can supply, beyond the modeled-orbit and four-fault-class
  rates the simulator now sweeps.

## What this is, and is not

Stated plainly, because the project only survives scrutiny if this does:

- **Not novel on its own.** Vector search plus engine-enforced isolation already
  ship together (Postgres + `pgvector` + RLS, Oracle 23ai). Deterministic
  simulation testing is respected practice (FoundationDB, TigerBeetle, Antithesis).
  Erasure coding is in every object store, and the corruption protection overlaps
  ECC memory and checksumming filesystems.
- **The uncontested combination.** A single from-scratch engine that is reliability
  infrastructure for AI memory, self-healing under a modeled fault environment,
  whose durability and isolation are *proven* by deterministic simulation and
  exhaustive model-checking. Reliability testing of vector databases is still posed
  as an open problem in the literature
  ([arXiv 2502.20812](https://arxiv.org/abs/2502.20812)).
- **A demonstration, not a product.** This is a from-scratch proof that an
  AI-memory engine can be made provably correct and self-healing in the hardest
  fault environment that could be modeled. It is not a claim to a market and not a
  replacement for Postgres. The radiation story is the proving ground; the rigor is
  the substance.
