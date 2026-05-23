# Capstone — Relational Database from Scratch (Rust)

CSE 499 senior project. Build a real disk-based relational database engine in Rust with ACID guarantees, WAL+ARIES recovery, MVCC, SQL parser, cost-based planner, and query executor.

**Owner:** Nathan Luckock — `nathanluckock@gmail.com`
**Repo:** https://github.com/Nathan7108/capstone
**Budget:** ~120 hrs over 13 weeks (W1-13)

---

## How to work in this repo (rules for Claude)

1. **Feature branch per task → PR → merge.** Never push directly to `main`. Branch names: `feat/<sprint>-<short-slug>`, `fix/<short-slug>`, `docs/<short-slug>`.
2. **Every commit ships with a `Design notes:` section.** Brief, but real. What we picked, what we rejected, why. This is Nathan's live-defense ammo when his prof asks "why a fanout of 128?"
3. **Checkpoint before architectural decisions.** Don't silently pick between e.g. B+ tree vs LSM, MVCC snapshot vs lock-based, ARIES vs simpler shadow-paging. Surface the tradeoff, get Nathan to call it, then implement.
4. **Tests first or alongside, not after.** Every module gets unit tests. The WAL + recovery layer gets a crash-recovery torture test that's run in CI.
5. **No silent scope creep.** If a task grows beyond its issue description, comment on the issue, don't just expand the diff.
6. **No dependencies for the graded must-haves.** No `sqlparser-rs`, no `sled`, no `rocksdb`. We're building the engine from scratch — that's the entire point. Standard library + `tracing` + `clap` + `serde` are fine for plumbing. Anything storage/parser/planner-related is hand-written.

---

## Architecture (target)

```
   ┌──────────────────────────────────────────────────────┐
   │                       CLI (psql-like)                │
   │                  crate: rustdb-cli                   │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │          SQL parser  →  planner  →  executor         │
   │         crates: sql, planner, executor               │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │     transaction manager + MVCC + lock manager        │
   │              crate: txn                              │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │           WAL  +  ARIES recovery manager             │
   │              crate: wal                              │
   └───────────────────────────┬──────────────────────────┘
                               │
   ┌───────────────────────────▼──────────────────────────┐
   │    buffer pool  +  page manager  +  B+ tree          │
   │              crate: storage                          │
   └──────────────────────────────────────────────────────┘
```

All crates live in a single Cargo workspace.

---

## State log (running progress)

> **Update this section as a running heartbeat.** Top entry = most recent. Keep terse — one line per shipped task.

- _(Sprint 1, 2026-05-22)_ Storage I — file manager landed (issue #2): `PageId`, `PAGE_SIZE = 8192`, `FileManager` with `open` / `allocate_page` / `read_page` / `write_page` / `fsync`. 8/8 unit tests green. CI gates clean.
- _(pre-Sprint-1, 2026-05-22)_ Bootstrap (PR #1, merged): Rust 1.95.0 installed, `.claude/skills/db-debug.md` added, `.mcp.json` registers GitHub MCP server, CLAUDE.md + design.md + sprints.md drafted, 8-crate workspace, Sprint 1 milestone + 5 issues filed (#2–#6).

---

## Page format (canonical, update as it changes)

> Single source of truth for the on-disk layout.

- Page size: **8192 bytes (8 KiB).** Defined as `rustdb_storage::PAGE_SIZE`.
- Page ID: **`u64`, 0-indexed**, byte offset = `page_id * PAGE_SIZE`. See `rustdb_storage::PageId`.
- Page header layout: **24 bytes** — see [docs/design.md](docs/design.md#slotted-page-format-heap-tables). Implementation lands in Sprint 1 issue #3.

---

## Invariants

- **WAL ordering:** No dirty page is flushed to disk before its log record is fsync'd.
- **Pin/unpin balance:** Every `PageGuard` drop unpins exactly once. No bare `pin`/`unpin` calls in user code.
- **No torn writes:** All page writes are full-page or recoverable from WAL.
- **MVCC snapshot stability:** A transaction's read snapshot doesn't shift mid-transaction.

---

## Reference material (read these, don't just cite them)

- ARIES paper (Mohan et al., 1992) — for WAL + recovery
- CMU 15-445 lectures (Pavlo) — for storage, buffer pool, indexes, joins
- CMU 15-721 lectures — for MVCC, query optimization
- "Database Internals" (Petrov) — pragmatic reference for layout choices

---

## Local scripts / Make targets

> Fill in as we add them. Goal: `cargo test`, `cargo bench`, `cargo run --bin rustdb` should all just work.
