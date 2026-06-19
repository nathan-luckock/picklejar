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
row-level security, and the PostgreSQL wire protocol, verified by 100,000
deterministic crash simulations and differentially tested against SQLite.

On top of it sits the AI-memory layer and its full reliability story, all shipped:

- **Vector memory.** A `VECTOR(n)` type, four distance operators, brute-force
  KNN, an HNSW index (four metrics, durable), and row-level-security-filtered
  similarity, so a tenant's search can only ever rank its own vectors. The index
  is reachable from SQL through a cached, write-invalidated, RLS-safe path, about
  150x faster on a warm query than the exact scan.
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
- **Backup, point-in-time recovery, and a physical standby replica.**
- **Exhaustive model-checking.** From-scratch bounded model checkers prove the
  write-ahead-logging ordering invariant (no page change is ever durable ahead of
  its log record) and the MVCC snapshot read-stability invariant over every
  reachable interleaving, each with a deliberately buggy variant that yields a
  concrete counterexample so the proof is not vacuous.
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
crash sampling at 100,000 seeds, an independent SQLite oracle, and exhaustive
model-checking of the core invariants.

## The frontier

What is genuinely still ahead, stated honestly:

- **Forward-replay point-in-time recovery.** Today the engine restores from a
  consistent snapshot, not by replaying the log forward over a base image. Rolling
  a base forward to an arbitrary LSN additionally needs the MVCC watermark and the
  version pages to advance in step with the replayed heap, which the snapshot model
  sidesteps and which is the real work here. The catalog and isolation snapshots
  are already LSN-reconstructable (the scan accepts a bound), so schema and policy
  as-of-a-point come for free once that forward-replay path exists.
- **Partition tolerance.** Meaningful only once there is a multi-node replica to
  diverge from, so it is folded into the replication path rather than built ahead
  of it: the link is down for a bounded interval, the node serves locally, and it
  reconciles on reconnect with bounded staleness.
- **Deeper fault coverage.** Per-part, shielding-aware upset rates that only a
  specific flight build can supply, and torn, misdirected, and lost-write models
  beyond the current bit-flip injector.
- **Drift-adaptive vector quantization (a research direction, optional).**
  Production vector indexes lose recall as embeddings drift and lean on a full
  reindex to recover it. Holding recall flat under drift, at a fixed memory budget
  and with the self-healing and model-checking guarantees intact, is an open
  systems problem and the one place this engine could make a novel, benchmarked
  contribution rather than a from-scratch re-implementation of solved art.

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
