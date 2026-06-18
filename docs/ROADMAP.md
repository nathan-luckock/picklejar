<div align="center">

# picklejar roadmap

From a crash-proven vector engine to flight-certifiable AI memory.

[Overview](../README.md) &nbsp;·&nbsp; [Design](design.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Build log](sprints.md)

</div>

---

## The thesis

For hardware you cannot physically service, the only acceptable form of
reliability is **proven before deployment**. You cannot attach a debugger to a
satellite. You cannot swap a corrupted drive in orbit. You cannot ship a patch
and reboot when the link is down for nine hours. So the data layer for those
places is not valuable because it is fast or because it does vector search.
Plenty of things do that. It is valuable because it can hand you a
**machine-checkable, regenerable proof that it survives the exact fault model of
its deployment environment**, before you ever fly it, including the guarantee
that no tenant's data is ever silently corrupted or leaked.

That proof artifact is the product. Everything in this roadmap builds toward it.

## The bet (why this is worth real money to an orbital operator)

A company like Starcloud is putting GPUs in orbit to run inference. Those models
have memory: embeddings, retrieval corpora, KV cache, agent state, per-customer
context. That memory has to live in a store that:

1. **Self-heals without ground intervention.** Radiation flips bits. Pages rot.
   Nobody is coming. The store must detect and repair corruption on its own, or
   fail safe rather than return a wrong answer.
2. **Never silently corrupts or leaks.** A corrupted embedding that is returned
   as if it were fine is worse than a crash. A tenant reading another tenant's
   memory is a contract and security breach. Both must be impossible, not
   unlikely.
3. **Comes with a flight-assurance proof.** Before a space company runs software
   in orbit, a mission-assurance review signs off on it. A data layer that ships
   with a reproducible report ("under N upsets per day at this orbit, across S
   seeds, zero data lost, zero silent corruption, zero tenant leakage") is the
   thing that review board needs and cannot currently get off the shelf.

Nobody sells that. Vector databases sell features and recall. Space-storage
products so far are archival, not queryable AI memory, and none ship a
fault-model-parameterized reliability proof. The wedge is the proof, on a real,
queryable, isolated AI-memory engine.

**Honest framing of the money:** the capstone produces the working reference
engine and the novel proof methodology. That is genuine IP and a credible
partnership or acquisition pitch. A literal multi-million-dollar sale is a
business-development outcome built on top of this, not a deliverable of the code.
This roadmap is how you build the thing that makes that conversation possible.

## Where we are today

The relational engine is complete and crash-proven (storage, WAL + ARIES
recovery, MVCC, cost-based planner, roles, row-level security, Postgres wire),
verified by 100,000 deterministic crash simulations. On top of it sits a working
AI-memory layer: a `VECTOR(n)` type, four distance operators, brute-force KNN,
row-level-security-filtered similarity (a tenant's search can only rank its own
vectors), an HNSW index (four metrics, insert/search/delete, durable), and the
`vecsim` simulator that proves durability and isolation together under crash.

Much of this roadmap is now built. Shipped so far:

- **Recall oracle on realistic data (1C/1D):** clustered, near-duplicate, and
  unit-norm distributions, with recall@k held as a CI gate against the exact
  brute-force oracle. Building it found a real recall bug (0.47 on clustered
  data), fixed by the HNSW heuristic neighbor selection, which took it to 0.98.
- **Metamorphic oracle (2D):** self-retrieval, monotonic insertion, deletion
  consistency, and recall monotonicity, the answer to the oracle problem.
- **Corruption detection (2B):** every page and every serialized index carries a
  CRC32 verified on read, so a flipped bit is refused, not served. (The page
  checksum machinery existed but was unwired; this closed that gap, verified by a
  3000-seed crash sweep.)
- **Self-healing (2C):** a redundant index reconstructs itself from its second
  copy when one is corrupted past its checksum, with no intervention.
- **Radiation framing (3A)** and the **regenerable reliability certificate (3B):**
  `vecert` emits a content-hashed report of all of the above, framed in a named
  orbit's upset rate.

The remaining gaps:

- The HNSW index is **not yet wired into the query path** (1A), so the search a
  SQL `ORDER BY <-> LIMIT` runs is still exact brute force, not the index.
- The corruption faults are injected into the serialized artifacts, **not yet
  into the live crash simulator** (the deeper half of 2A) or surfaced through the
  certificate at scale.
- The core recovery and isolation invariants are simulation-tested but **not yet
  model-checked** (3C).

## The arc

| Horizon | Goal | Outcome |
|---|---|---|
| **1. Close the gaps** | Make every current claim fully real | The approximate search path is wired in and fault-tested; recall is measured on realistic data with an oracle |
| **2. Solve the real problem** | Answer the open problem in VDBMS reliability testing | A space fault model (corruption, not just crash), end-to-end corruption detection, self-healing, and a metamorphic oracle for approximate search |
| **3. Above and beyond** | Do what nobody has done | Radiation-rate-parameterized simulation, a regenerable flight-reliability certificate, and the core invariants model-checked, not just sampled |

Each horizon is a sequence of concrete, deterministically-testable steps. Every
step states the **invariant** it establishes and the **proof** that backs it,
because in this project a feature does not exist until a seed can reproduce its
correctness.

---

## Horizon 1: Close the gaps (high-leverage, do this first)

This horizon turns "we test a vector memory layer" into "we test the real
approximate search path under fault, on realistic data." It is the highest
leverage work because it makes claims you are already making fully defensible,
and it is mostly engineering on foundations that exist.

### 1A. Wire HNSW into the planner

Make `ORDER BY embedding <-> :q LIMIT k` use the index instead of a linear scan.

- **Build:** detect the nearest-neighbor query shape in the planner; maintain a
  per-(table, column) HNSW cached on the `Database`, invalidated on writes to
  that table; serve the top-k from it.
- **Safety, non-negotiable:** the index path activates only when no row-level
  security policy applies to the session for that table. When a policy is in
  effect, fall back to the exact path, which already filters before it takes the
  top-k. This makes a cross-tenant leak through the index **structurally
  impossible** rather than merely unlikely. Default the index off behind a
  session toggle so exact results stay the default and approximate search is an
  explicit opt-in.
- **Invariant:** with RLS active, the index path is never taken, so isolation is
  identical to today. With RLS inactive, the index result is a recall-bounded
  approximation of the exact result, verified below.
- **Proof:** a differential test comparing the index path against exact brute
  force over many seeds, asserting recall at or above a threshold.

### 1B. Run `vecsim` against the approximate path

Today `vecsim` exercises the exact path. Extend it to optionally route through
the index.

- **Build:** a flag that makes the simulated workload's reads use the HNSW path.
- **Oracle becomes:** after a crash and recovery, every committed embedding is
  present (durability), each tenant still sees only its own (isolation), and the
  approximate search recovers at least recall R of the true neighbors.
- **Why it matters:** this is the single most important credibility move. It
  retires the "you only fault-test brute force" critique and makes "we test real
  ANN behavior under crash" true.

### 1C. Realistic and adversarial vector distributions

Replace uniform-random vectors with distributions that actually stress search.

- **Build:** generators for clustered data (Gaussian mixtures), near-duplicates,
  antipodal pairs, normalized unit vectors, and realistic dimensionality (384,
  768, 1536). All seeded and deterministic.
- **Why it matters:** recall on uniform random data is easy and uninformative.
  Recall on clustered, near-duplicate data is where ANN indexes actually fail,
  and where the literature says testing is hard.

### 1D. The recall oracle as a CI gate

Turn approximate-search quality into a tracked, regressible property.

- **Build:** a `difftest`-style harness that runs identical KNN queries through
  the exact and approximate paths and computes recall@k, over a seed sweep,
  across the distributions from 1C. A recall regression fails the build.
- **Invariant:** recall@k stays at or above the committed threshold for each
  distribution class.
- **Why it matters:** this is a concrete, working answer to one half of the
  literature's "oracle problem." You cannot know the exact right approximate
  answer in general, but exact brute force is a sound oracle for recall, and you
  have made it a gate, not a vibe.

**Horizon 1 deliverable:** index-accelerated, RLS-safe similarity search, fault-
tested on the approximate path over realistic data, with recall held as a CI
invariant. Every claim in the README becomes literally true and defensible.

---

## Horizon 2: Solve the real problem (the open research gap)

The 2030 testing-roadmap paper calls VDBMS reliability testing an open problem:
test-input generation, the oracle problem for approximate results, and fault
behavior. Horizon 1 answers input generation and the recall oracle. Horizon 2
answers the hard part: **correctness and survival under the fault model that
actually matters in space, with a real oracle for approximate search.** This is
the part that is genuinely beyond what shipped vector databases do.

### 2A. A space fault model, not just crash

Extend the deterministic fault disk from "crash loses un-fsynced writes" to the
full taxonomy of faults that dominate when hardware cannot be serviced.

- **Build, all seeded and reproducible:**
  - **Single-event upsets (bit flips):** flip random bits in pages, in buffer-
    pool frames, in WAL records, and in index nodes, at a parameterized rate.
  - **Silent data corruption:** a write lands with altered bytes, with no error
    reported by the device.
  - **Torn writes:** only part of a page reaches durable storage.
  - **Misdirected and lost writes:** a write lands at the wrong location, or is
    acknowledged but never persisted.
  - **Clock skew and brownout:** time jumps, and power is lost mid-operation.
- **Why it matters:** in low Earth orbit, bit flips and silent corruption are
  not edge cases, they are the steady state. A reliability story that only
  models clean crashes is testing the wrong thing for this market.

### 2B. End-to-end corruption detection (never return a wrong answer)

Make silent corruption impossible to return.

- **Build:** extend the existing page CRC32 to cover WAL records, index nodes,
  and the vector payload itself with end-to-end checksums verified on read, so a
  flipped bit anywhere is caught before the value leaves the engine.
- **Invariant (the headline):** **the engine never returns a silently corrupted
  embedding.** Every injected flip is either repaired from redundancy (2C) or
  surfaced as a detected fault. It is never served as if it were correct.
- **Proof:** under the 2A fault injector, across a seed sweep, assert that every
  corruption is detected, and that no query returns a value that differs from the
  last committed value without raising a detected-corruption signal.

### 2C. Redundancy and self-healing (no one is coming)

Detection is not enough when nobody can replace the bad hardware. The store must
reconstruct lost or corrupted data on its own.

- **Build:** erasure coding (Reed-Solomon parity) over page groups, or, as a
  simpler first cut, a replicated and checksummed WAL plus periodic full-page
  snapshots, so a corrupted or lost page is reconstructible. Add a **scrubber:**
  a background pass that re-verifies checksums and repairs from redundancy before
  corruption accumulates past the recoverable threshold.
- **Invariant:** the store survives up to t corrupted or lost pages per stripe
  with zero data loss and zero downtime, and the scrubber keeps the corruption
  count under t between scrubs.
- **Why it matters:** this is the literal "self-heals without ground
  intervention" requirement. It is the difference between a database and a
  database that can fly.

### 2D. A metamorphic oracle for approximate search

This is the direct answer to the hardest part of the open problem. You cannot
know the exact correct approximate result, but you know **relations that must
always hold**, and you can test those.

- **Build a metamorphic test suite.** Each relation is checked over random data,
  and again under the 2A fault injector:
  - **Self-retrieval:** a query equal to a stored vector returns that vector
    first.
  - **Monotonic insertion:** inserting a point strictly closer than the current
    k-th neighbor must change the top-k to include it.
  - **Deletion consistency:** a removed vector never appears in any result.
  - **Order invariance:** the set of exact nearest neighbors does not depend on
    insertion order.
  - **Scale invariance:** scaling every vector by a positive constant preserves
    the cosine ranking.
  - **Metric axioms:** distances stay non-negative, symmetric, zero only on
    self, and obey the triangle inequality (already proven; fold into the suite).
- **Invariant:** every metamorphic relation holds, including under injected
  faults (which is where a corrupted index would betray itself).
- **Why it matters:** metamorphic testing is the accepted research answer to the
  oracle problem for systems whose exact output you cannot predict. Applying it
  to ANN search **under a fault model** is the frontier. This is the part you can
  honestly call "beyond what anyone has shipped."

**Horizon 2 deliverable:** a vector AI-memory engine that detects every
corruption, repairs from redundancy without intervention, never returns a wrong
answer, and is proven correct against a metamorphic oracle under a realistic
space fault model. This is the genuine contribution to the open problem.

---

## Horizon 3: Above and beyond (plant the flag)

Horizon 2 makes it correct under the right faults. Horizon 3 makes it
**certifiable**, and ties the proof to the real environment, which is what turns
a strong capstone into something a space company takes seriously.

### 3A. Radiation-realistic fault rates

Parameterize the simulator by real upset rates instead of arbitrary ones.

- **Build:** drive the bit-flip injector from published single-event-upset rates
  for a named orbit (upsets per megabit per day at, say, a 550 km
  sun-synchronous orbit, from standard environment models). Run at one, ten, and
  one hundred times the expected rate and report the survivable rate.
- **The claim it earns:** "at the expected upset rate for this orbit, and at one
  hundred times that rate, across S seeds, zero committed embeddings were lost,
  zero silent corruptions were returned, and zero tenant leakage occurred." That
  is a sentence a mission-assurance engineer respects, because it is in their
  units.

### 3B. The flight-reliability certificate (the crown jewel)

Make the proof a regenerable, signed artifact.

- **Build:** a release step that runs the full fault-model sweep and emits a
  structured, reproducible report: the fault model and its rates, the seed range,
  every invariant checked (no data lost, no silent corruption returned, no tenant
  leakage, recall at or above threshold, every metamorphic relation held), and
  the results, with a content hash so the report is tamper-evident and exactly
  reproducible from the same commit and seed range.
- **Why it matters:** this is the deliverable a flight-software review board
  cannot get anywhere else. It is the product's center of gravity. "Here is a
  proof, regenerate it yourself, that this memory layer survives your
  environment" is the pitch.

### 3C. Model-check the core invariants

Deterministic simulation samples the state space. Model checking covers a bounded
state space exhaustively. Do both, the way the most serious systems do.

- **Build:** a lightweight formal model of the recovery and isolation invariants
  (in TLA+, or in Rust with a state-exploration library), model-checked so that,
  over a bounded model, recovery never loses a committed write and isolation
  never leaks, for all interleavings, not just sampled ones.
- **Why it matters:** "simulation-tested at 100k seeds and the recovery and
  isolation invariants are model-checked" is the belt-and-suspenders standard the
  most rigorous databases aspire to, and almost nothing in the AI-memory space
  does it.

### 3D. Partition and power tolerance

For intermittent ground links and constrained power, prove behavior under the
remaining space realities.

- **Build:** a partition model (the ground link is down for a bounded interval;
  the node serves locally and reconciles on reconnect with bounded staleness) and
  a brownout model (power is lost mid-operation, with partial fsync), both
  deterministic.
- **Invariant:** correct service and reconciliation under partition, and full
  recovery after brownout.

**Horizon 3 deliverable:** a reliability story stated in the operator's own
units (orbit, upset rate), backed by a regenerable signed certificate and
model-checked invariants. This is the artifact that makes the commercial
conversation real.

---

## The product an orbital operator buys

Not "a database." A **flight-certifiable, self-healing, isolated AI-memory
appliance** with these properties, none of which is currently available off the
shelf:

- **Drop-in:** speaks the PostgreSQL wire protocol, so an existing RAG, agent, or
  embedding stack connects with no code change.
- **Tiny and dependency-free:** single-threaded, no external database, parser,
  wire, or crypto crates, runs on the constrained single-board compute that flies.
- **Self-healing:** detects and repairs corruption with no ground intervention,
  and fails safe rather than returning a wrong answer.
- **Isolated by construction:** the engine, not the application, guarantees one
  tenant can never read or corrupt another's memory, and that guarantee is proven
  to survive faults.
- **Certified before launch:** ships with a regenerable proof that it survives
  the deployment environment's fault model, which is the artifact mission
  assurance signs.

The wedge is the **memory and retrieval store for orbital inference**: the place
embeddings, RAG corpora, KV cache, and agent state live when the model runs
somewhere nobody can reach. It is valuable because it replaces an in-house build
and de-risks the data layer of a flight system, and because the proof artifact is
the long pole in any mission-assurance review.

## Honest reality check

This roadmap is ambitious on purpose, and the value framing above is the
aspiration. Here is the line between what the capstone realistically delivers and
what is company-scale work, so nothing here gets overstated.

| Achievable as the capstone | Company-scale, beyond it |
|---|---|
| Horizon 1 in full | Production erasure coding at petabyte scale |
| Horizon 2: fault model, detection, metamorphic oracle, a first cut of redundancy and scrubbing | Hardware-in-the-loop and radiation-beam testing |
| Horizon 3: radiation-rate-parameterized simulation and the certificate generator; a first model-checked invariant | A real flight-software certification (a DO-178C-style process) and distributed multi-node operation |

The capstone deliverable you actually pitch is the **working reference engine plus
the novel, regenerable, fault-model-parameterized reliability-proof methodology**,
demonstrated end to end. That methodology, deterministic-simulation-plus-
metamorphic-testing of a self-healing, isolated AI-memory layer against a space
fault model, is the part that is genuinely new, and it is the IP. The flight
contract is a later chapter; this builds the thing that earns the meeting.

## Sequencing: the first three moves

Do these in order. Each is independently shippable and each makes the next one
possible.

1. **Wire HNSW into the planner and point `vecsim` at the approximate path**
   (1A, 1B). Closes the biggest honest gap. After this, "we fault-test real
   approximate search, with isolation, under crash" is simply true.
2. **Build the space fault model and end-to-end corruption detection** (2A, 2B).
   This is the most differentiating single step and the literal space
   requirement: never return a silently corrupted answer, and prove it under bit
   flips and silent corruption. This is where the project stops looking like a
   good database project and starts looking like flight software.
3. **Add the metamorphic oracle and the certificate generator** (2D, 3B). The
   oracle answers the field's open problem; the certificate turns the proof into
   the artifact you demo and pitch. Together they are the "nobody has done this"
   claim, made concrete.

Everything after that, redundancy and scrubbing (2C), radiation rates (3A), model
checking (3C), and partition and power tolerance (3D), deepens the moat and the
certificate. But the first three moves are what take this from a crash-proven
vector engine to a credible, novel, fault-proven AI-memory layer for hardware
nobody can reach.
