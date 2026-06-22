# Examples

## `agent_memory.py`: durable, tenant-isolated agent memory

An infrastructure agent's memory at scale: write hundreds of operational
memories per tenant, recall the relevant few by vector similarity (with the
latency printed), stay tenant-isolated across the whole corpus, survive a
reconnect, and forget on request.

```bash
# 1. start a server (one terminal)
cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433

# 2. install the client and run the demo (another terminal)
pip install ./sdk/python
python examples/agent_memory.py
```

What it shows (with timings, over ~800 memories):
- **store / recall at scale**: hundreds of memories go in as embeddings; a query
  recalls the nearest few (the engine's `VECTOR` + distance search) and prints
  the latency.
- **tenant isolation**: two tenants share the store, and a recall only ever
  returns the caller's own memories, even across the full corpus.
- **durability**: the connection is dropped and reopened, and the memory is
  still there (it is on disk via the write-ahead log).
- **forget**: a memory is deleted and no longer recalled.

The `embed()` in the script is a tiny collision-free bag-of-words stand-in so the
demo needs no model or API key; in a real agent you pass embeddings from a real
model and the picklejar calls are identical.

Run against a different port with `PICKLEJAR_PORT=5470 python examples/agent_memory.py`,
and use a fresh database file to repeat cleanly.
