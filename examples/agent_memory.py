"""Agent memory at scale on picklejar.

An infrastructure agent that remembers operational facts: it writes hundreds of
memories per tenant, recalls the relevant few by vector similarity, stays
tenant-isolated across the whole corpus, survives a reconnect, and forgets on
request. Each step prints what the engine did and how long it took.

Run a server first, in another terminal:

    cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433

Then (set PICKLEJAR_PORT to use a different port):

    pip install ./sdk/python
    python examples/agent_memory.py

The `embed()` here is a collision-free bag-of-words stand-in over the corpus
vocabulary, so the demo needs no model or API key; in a real agent you pass
embeddings from a real model and the picklejar calls are identical.
"""

from __future__ import annotations

import math
import os
import random
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "sdk", "python"))

from picklejar import MemoryStore  # noqa: E402

SERVICES = [
    "auth-api", "billing-svc", "search-api", "api-gateway", "ingest-worker",
    "scheduler", "payments", "notifier", "user-store", "feature-flags",
]
REGIONS = ["us-west-2", "us-east-1", "eu-central-1", "ap-south-1"]
TOPICS = ["latency", "throughput", "errors", "deploy", "rollback", "migration", "quota", "cache"]
ACTIONS = ["spiked", "recovered", "degraded", "scaled", "restarted", "throttled", "paged"]

# A few specific facts we will later recall out of the noise.
NEEDLES = [
    "auth-api p99 latency spiked to 240ms before the cache deploy",
    "migration 0042 must run before 0043 on user-store",
    "payments runs in us-east-1 not us-west-2",
]
NOISE_PER_TENANT = 400


def _corpus_words() -> set[str]:
    words = {w for n in NEEDLES for w in n.lower().split()}
    words |= set(SERVICES) | set(REGIONS) | set(TOPICS) | set(ACTIONS)
    words |= {"p99", "240ms", "0042", "0043", "in", "before", "the", "to", "not", "must", "run"}
    return words


_VOCAB = {w: i for i, w in enumerate(sorted(_corpus_words()))}
DIM = len(_VOCAB)


def embed(text: str) -> list[float]:
    vec = [0.0] * DIM
    for word in text.lower().split():
        if word in _VOCAB:
            vec[_VOCAB[word]] += 1.0
    norm = math.sqrt(sum(x * x for x in vec)) or 1.0
    return [x / norm for x in vec]


def noise(rng: random.Random) -> str:
    return f"{rng.choice(SERVICES)} {rng.choice(TOPICS)} {rng.choice(ACTIONS)} in {rng.choice(REGIONS)}"


def connect() -> MemoryStore:
    host = os.environ.get("PICKLEJAR_HOST", "127.0.0.1")
    port = int(os.environ.get("PICKLEJAR_PORT", "5433"))
    try:
        return MemoryStore(host=host, port=port, table="agent_memory", dim=DIM).ensure_schema()
    except Exception as exc:  # noqa: BLE001
        print(f"could not connect to picklejar on {host}:{port}: {exc}")
        print("start it: cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433")
        raise SystemExit(1)


def main() -> None:
    mem = connect()
    print(f"embedding dim = {DIM}; loading {NOISE_PER_TENANT * 2 + len(NEEDLES)} memories...")

    rng = random.Random(7)
    t0 = time.perf_counter()
    for fact in NEEDLES:
        mem.store("team-sre", embed(fact), content=fact)
    for _ in range(NOISE_PER_TENANT):
        mem.store("team-sre", embed(noise(rng)), content=noise(rng))
    for _ in range(NOISE_PER_TENANT):
        mem.store("team-data", embed(noise(rng)), content=noise(rng))
    load_ms = (time.perf_counter() - t0) * 1000
    print(f"loaded in {load_ms:.0f}ms\n")

    query = "what was the auth-api p99 latency"
    print(f"query: {query!r}")
    t0 = time.perf_counter()
    hits = mem.recall("team-sre", embed(query), k=3)
    ms = (time.perf_counter() - t0) * 1000
    print(f"recalled top-3 of {NOISE_PER_TENANT + len(NEEDLES)} team-sre memories in {ms:.1f}ms:")
    for m in hits:
        print(f"  [{m.distance:6.3f}] {m.content}")

    print("\ntenant isolation across the whole corpus:")
    leak = [m for m in mem.recall("team-data", embed(query), k=5) if "auth-api p99" in m.content]
    print(f"  team-data recall leaked team-sre's needle: {len(leak)}  (expected 0)")

    print("\ndurability: drop the connection, reconnect, recall again:")
    mem.close()
    mem = connect()
    again = mem.recall("team-sre", embed(query), k=1)
    survived = bool(again) and "auth-api p99" in again[0].content
    print(f"  needle still recalled after reconnect: {survived}")

    print("\nforget: delete the needle, confirm it is gone:")
    if again:
        mem.forget("team-sre", id=again[0].id)
        post = mem.recall("team-sre", embed(query), k=1)
        gone = not post or "auth-api p99" not in post[0].content
        print(f"  needle gone: {gone}")

    mem.close()


if __name__ == "__main__":
    main()
