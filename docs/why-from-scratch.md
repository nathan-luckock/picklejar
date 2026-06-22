# Why a database from scratch, and not Postgres + pgvector?

The honest version, because the project only survives scrutiny if this answer
does.

## The objection is fair

Postgres with `pgvector` and row-level security already gives you vector
similarity search and engine-enforced per-tenant isolation, today, in production.
Oracle 23ai ships vector search too. If your environment is a normal data center
with operators, spare nodes, and backups, **you should use Postgres.** picklejar
is not trying to beat Postgres at being Postgres, and pretending otherwise would
be the fastest way to lose the argument.

## What changes the answer: the environment removes every assumption Postgres makes

Postgres, and every mature database, is built for a *serviceable* environment.
Its durability and availability story quietly assumes, underneath:

- a human (a DBA) who can intervene,
- spare nodes to fail over to,
- backups you can restore from,
- a technician who can swap a failed disk.

An orbital or remote-edge data center removes all four. No DBA, no hot spare you
can reach, no restore, no disk swap. The data layer there has a different and
harder job: **keep committed data intact and tenant-isolated through faults that
nobody is around to repair, and prove it survives them.**

## Three things that job needs, that bolting onto Postgres cannot give you

### 1. Self-healing under silent corruption, not just detection

Postgres can optionally checksum pages (`data_checksums`) and will *error* on a
bad one. It does not repair it, does not erasure-code it, has no radiation fault
model, and assumes you restore from a backup or fail over to a replica. On an
unreachable node, "error and stop" is a dead node. picklejar detects, logs, and
repairs from software redundancy, so the node heals itself instead of waiting for
a human who is never coming.

### 2. Mass-efficient redundancy

In space the binding constraint is launched mass: every kilogram to orbit costs
thousands of dollars, permanently. The traditional way to survive radiation is
heavy hardware: radiation-hardened chips (low density, decades behind on
capacity) and triple-redundant drives (three times the mass for the same usable
bytes). picklejar moves the redundancy into software with erasure coding: survive
`M` simultaneous failures at roughly `M/K` storage overhead instead of `+200%`.
That lets an operator launch cheap, light, dense commodity storage and let the
engine tolerate its failures. "More usable memory per kilogram launched" is then
a property of the storage engine, not an add-on a normal database can provide.

### 3. Reliability proven by deterministic simulation, not asserted

Postgres reliability is empirical and battle-tested, but it is not *replayable*:
you cannot re-run the exact fault sequence that broke it. picklejar's recovery
and isolation are exercised by a deterministic, seed-replayable fault simulator
(the FoundationDB and TigerBeetle philosophy), now parameterized by an orbital
radiation model. A failure is one `u64` seed you replay exactly. That guarantee
requires a fully controlled, deterministic I/O path, which you do not have on top
of Postgres.

## Why *from scratch*, specifically

Each of those three requires owning the entire I/O path: how pages and metadata
are written, checksummed, made redundant, and recovered, and how time and
randomness flow so a run is deterministic. You cannot bolt "never serve a
silently corrupted vector, self-heal it, and prove it by replay" onto Postgres,
because its storage layer, its catalog, its `fsync` assumptions, and its recovery
are neither built for it nor under your control. The proof *is* the product, and
the proof requires the control.

## What is genuinely new, and what is not

- **Not new on its own:** vector search plus row-level isolation (Postgres,
  Oracle 23ai). Deterministic simulation testing (FoundationDB, TigerBeetle).
  Erasure coding (every object store).
- **Uncontested in combination:** a single from-scratch engine that is
  reliability infrastructure for AI memory, self-healing under a modeled fault
  environment with mass-efficient software redundancy, whose durability and
  isolation are proven by deterministic replay *and exhaustive model-checking*,
  aimed at a single unreachable node rather than a serviced cluster. The
  reliability testing of vector databases is still posed as an open problem in the
  literature ([arXiv 2502.20812](https://arxiv.org/abs/2502.20812)).

The honesty that makes the claim survive scrutiny: picklejar is not replacing
Postgres. It is building the thing Postgres's own assumptions prevent it from
being.
