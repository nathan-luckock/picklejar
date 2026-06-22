"""Agent memory, end to end, on picklejar.

A tiny but real example of what picklejar is for: giving an AI agent a durable,
tenant-isolated memory it can write to and recall from by meaning.

Run a server first, in another terminal:

    cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433

Then run this (set PICKLEJAR_PORT to use a different port):

    pip install ./sdk/python      # or: pip install picklejar
    python examples/agent_memory.py

The `embed()` here is a tiny collision-free bag-of-words stand-in over the demo's
own vocabulary, so the example needs no model or API key and recall is clean. In
a real agent you would pass embeddings from a real model (OpenAI,
sentence-transformers, ...); the picklejar calls are exactly the same.
"""

from __future__ import annotations

import math
import os
import sys

# Use the SDK straight from this repo without installing it.
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from picklejar import MemoryStore  # noqa: E402

ALICE_MEMORIES = [
    "Sarah prefers dark mode in every app",
    "Sarah is allergic to peanuts",
    "Sarah is in the US Pacific timezone",
    "the deploy script is at scripts/deploy.sh",
]
BOB_MEMORIES = [
    "Bob's project runs on Postgres",
    "Bob skips meetings before noon",
]

# A fixed vocabulary over the corpus: one dimension per word, so embeddings never
# collide and recall is pure word overlap. (A real model replaces this.)
_VOCAB = {
    word: i
    for i, word in enumerate(
        sorted({w for text in ALICE_MEMORIES + BOB_MEMORIES for w in text.lower().split()})
    )
}
DIM = len(_VOCAB)


def embed(text: str) -> list[float]:
    """Bag-of-words over the fixed vocabulary, L2-normalized. Words outside the
    vocabulary (most of a free-form query) are ignored."""
    vec = [0.0] * DIM
    for word in text.lower().split():
        if word in _VOCAB:
            vec[_VOCAB[word]] += 1.0
    norm = math.sqrt(sum(x * x for x in vec)) or 1.0
    return [x / norm for x in vec]


def main() -> None:
    host = os.environ.get("PICKLEJAR_HOST", "127.0.0.1")
    port = int(os.environ.get("PICKLEJAR_PORT", "5433"))
    try:
        mem = MemoryStore(host=host, port=port, table="agent_memory", dim=DIM)
        mem.ensure_schema()
    except Exception as exc:  # noqa: BLE001
        print(f"could not connect to picklejar on {host}:{port}: {exc}")
        print("start it with: cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433")
        raise SystemExit(1)

    # (Run against a fresh database, e.g. delete agentmem.db, to repeat cleanly.)

    for i, fact in enumerate(ALICE_MEMORIES):
        mem.store("alice", embed(fact), content=fact, metadata={"source": f"chat#{i}"})
    for fact in BOB_MEMORIES:
        mem.store("bob", embed(fact), content=fact)

    print("=== Alice's agent recalls what it knows about Sarah ===")
    for m in mem.recall("alice", embed("what do we know about Sarah"), k=3):
        print(f"  [{m.distance:6.3f}] {m.content}   {m.metadata or ''}")

    print("\n=== A sharper query: is Sarah allergic to anything ===")
    for m in mem.recall("alice", embed("is Sarah allergic to anything"), k=2):
        print(f"  [{m.distance:6.3f}] {m.content}")

    print("\n=== Tenant fence: Bob's agent recalls the SAME query ===")
    print("    (it must never see Sarah's memories)")
    hits = mem.recall("bob", embed("what do we know about Sarah"), k=3)
    for m in hits:
        print(f"  [{m.distance:6.3f}] {m.content}")
    leaked = [m for m in hits if "Sarah" in m.content]
    print(f"\n  Sarah's memories leaked to Bob: {len(leaked)}  (expected 0)")

    print("\n=== Forgetting a memory ===")
    before = mem.recall("alice", embed("is Sarah allergic to anything"), k=1)
    if before:
        target = before[0]
        print(f"  forgetting: {target.content}")
        mem.forget("alice", id=target.id)
        after = mem.recall("alice", embed("is Sarah allergic to anything"), k=1)
        gone = all(m.id != target.id for m in after)
        print(f"  still recalled? {'no, it is gone' if gone else 'yes (unexpected)'}")

    mem.close()


if __name__ == "__main__":
    main()
