<div align="center">

# picklejar roadmap

A proof-driven engine for hardware nobody can reach. What is built, and where the frontier is.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Build log](sprints.md)

</div>

---

## The thesis

For hardware you cannot physically service, the only acceptable form of
reliability is **proven before deployment**. You cannot attach a debugger to a
satellite. You cannot swap a corrupted drive in orbit. You cannot ship a patch
and reboot when the link is down for nine hours. So the interesting property of a
data layer for those places is not that it is fast, or that it does vector
search. Plenty of things do that. It is that it can hand you a
**machine-checkable, regenerable proof that it survives the exact fault model of
its environment**, including the guarantee that no tenant's data is ever silently
corrupted or leaked.

That proof is what the project is built around. Everything here drives toward it,
and most of it is now built.

## What is built

The relational engine is complete and crash-proven: storage, a write-ahead log
with ARIES recovery, MVCC snapshot isolation, a cost-based planner, roles,
row-level security, and the PostgreSQL wire protocol, verified by 1,000,000
deterministic crash simulations and differentially tested against SQLite.

On top of it sits the AI-memory layer and its full reliability story, all shipped:

- **Vector memory.** A `VECTOR(n)` type, four distance operators, brute-force
  KNN, an HNSW index (four metrics, durable), and row-level-security-filtered
  similarity, so a tenant's search can only ever rank its own vectors. The index
  is reachable from SQL through a cached, write-invalidated, RLS-safe path, about
  150x faster on a warm query than the exact scan.
- **Contradiction detection.** `INSERT ... ON CONFLICT (key) DO ASSERT` is
  write-time contradiction detection for memory facts: re-asserting an identical
  fact is idempotent, but a write that records a different value for a key the
  store already holds is rejected as a contradiction (the column, key, and both
  values are named), instead of silently overwriting a held belief. This is the
  unsolved AI-memory consistency problem from the research, enforced by the engine.
- **Drift-adaptive quantization.** A scalar-quantized index stores each embedding
  at one byte per dimension (a 4x smaller index), and holds recall flat as the
  embedding distribution drifts by watching the live distribution and recalibrating
  only when it has outgrown the calibrated range, re-quantizing from the durable
  full-precision rows so the index never grows. The benchmark (`quantsim`,
  certified in `vecert`) shows the adaptive index near the full-precision ceiling
  (recall ~0.97) where a static calibrate-once quantizer collapses (~0.005) under
  the same drift, at the same compression, recalibrating on under 2% of inserts
  rather than reindexing. Holding recall flat under drift at a fixed memory budget,
  rather than reindexing, is the one place this engine makes a benchmarked
  contribution rather than re-implementing solved art.
- **Recall as a CI gate.** `recall@k` held against the exact brute-force oracle
  on clustered, near-duplicate, and unit-norm distributions. Building it found and
  fixed a real recall bug (0.47 to 0.98 on clustered data).
- **A metamorphic oracle for approximate search.** Self-retrieval, monotonic
  insertion, deletion consistency, and recall monotonicity: the accepted answer to
  the oracle problem when the exact result cannot be known.
- **A space fault model.** The deterministic simulator irradiates a committed
  multi-tenant workload across every persistent file (heap, WAL, and the
  checksummed metadata sidecars) at a named orbit's single-event-upset rate for a
  chosen dwell, and proves it never serves a silently corrupted answer.
- **Self-healing storage.** A from-scratch Reed-Solomon erasure code (GF(2^8))
  backs a self-healing block store and a page-heap parity layer. `protect(k, m)`
  writes a parity snapshot, and `open_resilient` reconstructs any corrupt heap
  page before the engine reads it, at `m/k` overhead instead of the `+m*100%` of
  redundant copies.
- **Operability of self-healing.** A `PROTECT` statement, a durable fault log
  (`pg_fault_log`), and the `pjscrub` scrubber that heals and refreshes parity on
  a cadence.
- **Bitemporal time travel.** Two independent as-of axes. Valid-time
  (`SET valid_time = TIMESTAMP '...'`) rewinds reads on temporal tables (those with
  `valid_from` / `valid_to` columns) to the rows valid at that instant over the
  half-open interval: what was true in the world then. Transaction-time
  (`SET transaction_time = <point>`, a transaction-id watermark from
  `txid_current()`) travels a read to the MVCC snapshot as of a past transaction,
  walking each retained version chain to the version live then: what the database
  knew then. With both set a query is a full bitemporal as-of. Both ride the same
  parser-safe session mechanism as the index toggle, sidestepping the `AS OF`
  syntax collision, and both are read-only (writes act on the latest state);
  transaction-time is bounded by retained version history (pre-`VACUUM`).
- **Backup, point-in-time recovery, and a physical standby replica.** Backup
  copies a consistent snapshot; logical point-in-time restore (`restore_as_of`)
  rebuilds a fresh database holding the state as of a past transaction point, read
  through the transaction-time-travel path and re-materialized through the normal
  write path, so the result has fresh ids, a real index, and correct anchors.
- **Physical forward-replay (the engine of point-in-time recovery).** An
  `MvccTable` can rebuild itself physically as of an arbitrary target LSN straight
  from the log: `redo_through` replays every heap version write up to the target
  into a fresh file (the version pages land at their original ids), and the
  primary index, whose B+ tree pages are never logged, is rebuilt from the
  `IndexUpdate` mappings the engine now logs on every version write (last write per
  key winning, the chain head as of the target). The recovered table reads exactly
  the state as of the target: a row updated since shows its old value, a row
  inserted since is absent. A differential oracle validates it: across forty
  seeded random workloads, the table is recovered to every checkpoint LSN and its
  scan is checked against an independent model of the committed state then. The
  `IndexUpdate` log record is additive and append-only, so it adds no fsync to the
  write path and leaves the 1,000,000-seed-validated recovery untouched.
- **Exhaustive model-checking.** From-scratch bounded model checkers prove the
  write-ahead-logging ordering invariant (no page change is ever durable ahead of
  its log record), the MVCC snapshot read-stability invariant, and, through the
  approximate index, both tenant isolation (a tenant's query never returns another
  tenant's row) and cache freshness (a query never returns a deleted row, so a
  forgotten memory cannot resurface), and valid-time travel (a read at a session
  as-of instant returns a row exactly when it is valid then, so the half-open
  boundary never serves a superseded row nor drops a current one). Each is proved
  over its whole bounded model, with a deliberately buggy variant that yields a
  concrete counterexample so the proof is not vacuous. No vector or AI-memory
  database is known to model-check its filtered retrieval this way, which is the
  sharpest piece of open ground the project sits on.
- **A storage-fault taxonomy and coverage simulator.** Beyond the radiation
  bit-flip model, `faultsim` injects all four storage-write fault classes (bit
  flip, torn write, lost write, misdirected write) and measures the engine's
  detection rate per class under its layered page check. The payload checksum
  catches every bit flip and torn write, and the LSN-versus-log guard catches
  every lost write; a misdirected write that lands newer content is the honest
  residual, reported and recorded, that a self-identifying page id would close.
- **The reliability certificate.** `vecert` runs every invariant above and emits
  a content-hashed, regenerable report, framed in a named orbit's upset rate.
- **A WAL-logged catalog and isolation state.** Schema changes and row-level-
  security policy changes are written to the WAL as snapshot records and replayed
  on open, so the log is authoritative for both: a change that reached the log is
  recovered even if its sidecar write was lost, which for isolation means a crash
  can never silently drop a tenant fence, and forward replay reconstructs later
  schema and policy state rather than only the base state.

So the engine detects every corruption it is built to catch, repairs from
redundancy with no human in the loop, never serves a silently wrong or
cross-tenant answer, and proves these properties three independent ways: random
crash sampling at 1,000,000 seeds, an independent SQLite oracle, and exhaustive
model-checking of the core invariants.

## The frontier

What is genuinely still ahead, stated honestly:

- **Physical forward-replay point-in-time recovery (engine built; SQL wiring
  ahead).** The mechanism is built and tested (see "What is built"): an `MvccTable`
  rebuilds its heap *and* its primary index from the log to an arbitrary target
  LSN, since each heap version write now also logs an `IndexUpdate` mapping (the
  index pages themselves are still never logged, but the key-to-version mappings
  are). What remains is the `Database`-level wiring: rebuilding the non-logged
  *secondary* indexes from the replayed heap, patching the per-table page anchors
  in the as-of catalog, and reconstructing the sidecars, so a single
  `restore_physical_to_lsn` covers a whole multi-index database. The logical
  restore covers the recovery need today; the physical path adds speed (no
  re-insert) and fidelity (it preserves the MVCC history), now resting on a proven
  core.
- **Partition tolerance.** Meaningful only once there is a multi-node replica to
  diverge from, so it is folded into the replication path rather than built ahead
  of it: the link is down for a bounded interval, the node serves locally, and it
  reconciles on reconnect with bounded staleness.
- **A self-identifying page id (the misdirected-write residual).** The fault-
  coverage simulator catches every bit flip, torn write, and lost write, but a
  misdirected write that lands *newer* content slips, because the page format
  stores no page id to check its location against. Closing it means adding a page-
  id guard to the header (a page-format change), after which misdirected writes
  are caught completely.
- **Deeper fault coverage.** Per-part, shielding-aware upset rates that only a
  specific flight build can supply, beyond the modeled-orbit rates and the
  bit-flip, torn, lost, and misdirected fault classes the simulator now sweeps.

## What this is, and is not

Stated plainly, because the project only survives scrutiny if this does:

- **Not novel on its own.** Vector search plus engine-enforced isolation already
  ship together (Postgres with `pgvector` and RLS, Oracle 23ai). Deterministic
  simulation testing is a respected technique (FoundationDB, TigerBeetle,
  Antithesis). Erasure coding is in every object store. The corruption protection
  overlaps what ECC memory and checksumming filesystems already provide.
- **The uncontested combination.** A single from-scratch engine that is an
  isolated AI-memory layer, self-healing under a space fault model, whose
  durability and isolation are *proven* by deterministic simulation and exhaustive
  model-checking. Reliability testing of vector databases is still posed as an open
  problem in the literature
  ([arXiv 2502.20812](https://arxiv.org/abs/2502.20812)).
- **A demonstration, not a product.** This is a from-scratch proof that an
  AI-memory engine can be made provably correct and self-healing, in the hardest
  fault environment that could be modeled. It is not a claim to a market and not a
  replacement for Postgres. The radiation story is the proving ground; the rigor is
  the substance.
