<div align="center">

# picklejar quickstart

From zero to storing and recalling AI memories in five minutes.

[Overview](../README.md) &nbsp;·&nbsp; [Features](FEATURES.md) &nbsp;·&nbsp; [Gallery](gallery.md)

</div>

---

picklejar is an AI-memory database that speaks the PostgreSQL wire protocol, so
any Postgres client or driver connects to it. You run one server and talk to it
with `psql`, a driver, or the Python client.

## 1. Run the server

With Docker (build locally, or pull the published image once a release is cut):

```bash
docker build -t picklejar .                                  # local build
# docker pull ghcr.io/nathan-luckock/picklejar               # published image
docker run -p 5433:5433 -v picklejar-data:/data picklejar
```

Or straight from the repo:

```bash
cargo run --release --bin picklejar-pg -- --database mem.db --port 5433
```

It listens on port 5433 with trust auth (no password) by default. Set
`--password <pw>` to require SCRAM-SHA-256, and `--host 0.0.0.0` to accept
connections from other machines.

## 2. Talk to it with any Postgres client

```bash
psql -h 127.0.0.1 -p 5433 -U postgres
```

```sql
CREATE TABLE memories (id SERIAL PRIMARY KEY, tenant TEXT, content TEXT, embedding VECTOR(3));
INSERT INTO memories (tenant, content, embedding) VALUES ('acme', 'the sky is blue', '[0.1,0.2,0.9]');
SELECT content FROM memories WHERE tenant = 'acme' ORDER BY embedding <-> '[0.1,0.2,0.8]' LIMIT 5;
```

A real driver works the same way (here `psycopg`, but any does):

```python
import psycopg
conn = psycopg.connect(host="127.0.0.1", port=5433, user="postgres", autocommit=True)
conn.execute("INSERT INTO memories (tenant, content, embedding) VALUES (%s, %s, %s) RETURNING id",
             ("acme", "fire is hot", "[0.9,0.1,0.1]")).fetchone()
```

## 3. Or use the Python client

```bash
pip install picklejar      # see sdk/python
```

```python
from picklejar import MemoryStore

mem = MemoryStore(host="127.0.0.1", port=5433, dim=3).ensure_schema()
mem.store("acme", [0.1, 0.2, 0.9], content="the sky is blue", metadata={"src": "doc1"})
mem.store("acme", [0.9, 0.1, 0.1], content="fire is hot")

for m in mem.recall("acme", [0.1, 0.2, 0.8], k=5):
    print(m.id, m.content, m.distance)

mem.forget("acme", id=2)
```

Each memory is tagged with a `tenant`, and every recall is fenced to that
tenant's own rows. For isolation enforced by the engine rather than the client,
add a row-level-security policy (see [the memory-layer section of the
README](../README.md#the-memory-layer)); similarity search then runs through the
same fence.

## Run a replicated cluster (multi-node)

For hardware that can vanish or get cut off, run picklejar as an
availability-first cluster. Start three nodes, each pointed at the others:

```bash
pjnode --id 0 --port 7500 --peer 1@127.0.0.1:7501 --peer 2@127.0.0.1:7502 &
pjnode --id 1 --port 7501 --peer 0@127.0.0.1:7500 --peer 2@127.0.0.1:7502 &
pjnode --id 2 --port 7502 --peer 0@127.0.0.1:7500 --peer 1@127.0.0.1:7501 &
```

Store and recall vector memories across the cluster with the `pjctl` client:

```bash
NODES="--node 0@127.0.0.1:7500 --node 1@127.0.0.1:7501 --node 2@127.0.0.1:7502"
pjctl $NODES store 1 0.1,0.2,0.9 "the sky is blue"
pjctl $NODES store 2 0.9,0.1,0.1 "fire is hot"
pjctl $NODES recall 0.1,0.2,0.82 2     # distributed nearest-neighbor
```

Writes go to a key's replicas; recall is a scatter-gather nearest-neighbor
across the nodes. The cluster stays available through a network partition and
reconciles itself on heal (run `repdemo` to watch that, `repsim` to prove it at
scale).

## What you just used

The server is the from-scratch picklejar engine: SQL, MVCC, write-ahead logging
with crash recovery, a cost-based planner, row-level security, and a native
`VECTOR` type with an HNSW index, all behind the real PostgreSQL wire protocol.
Its durability is backed by 1,000,000 deterministic crash simulations; see the
[README](../README.md) for the full proof story.
