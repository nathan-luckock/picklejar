# Examples

## `agent_memory.py` — durable, tenant-isolated agent memory

A small end-to-end demo of picklejar as an AI agent's memory: store facts as
embeddings, recall the relevant ones by meaning, keep tenants isolated, and
forget on request.

```bash
# 1. start a server (one terminal)
cargo run --release --bin picklejar-pg -- --database agentmem.db --port 5433

# 2. install the client and run the demo (another terminal)
pip install ./sdk/python
python examples/agent_memory.py
```

What it shows:
- **store / recall** — agent memories go in as embeddings; a query recalls the
  nearest ones (the engine's `VECTOR` + distance search).
- **tenant isolation** — two tenants share the store, and a recall only ever
  returns the caller's own memories, even when another tenant's vector is nearer.
- **forget** — a memory is deleted and no longer recalled.

The `embed()` in the script is a tiny collision-free bag-of-words stand-in so the
demo needs no model or API key; in a real agent you pass embeddings from a real
model and the picklejar calls are identical.

Run against a different port with `PICKLEJAR_PORT=5470 python examples/agent_memory.py`,
and use a fresh database file to repeat cleanly.
