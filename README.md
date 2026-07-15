# rsd

**A reactive, semantic index of everything on your machine.** A ground-up
replacement for macOS Spotlight's indexing daemon: crash-proof by
construction, sandboxed extraction, sub-millisecond search, on-device
semantic understanding, live standing queries, an agent surface (MCP), and a
native search palette. Nothing ever leaves your Mac.

## Quick start

```bash
./scripts/fetch-model.sh                      # optional: the learned embedder (~90MB, local)
cargo build --release && ./scripts/build-app.sh

./target/release/rsd-daemon watch ~/Documents # index + watch a folder
open dist/RSD.app                             # then ⌥Space from anywhere
```

CLI: `rsdfind --state <state-dir> [--semantic|--hybrid] "query"`, plus
`-live` standing queries and `-live --semantic` alerts.
Agents: `rsd-mcp --state <state-dir>` (MCP over stdio: search + grounded snippets).

## Why it's different

- **Never silently re-indexes**: an event-sourced journal + checksummed
  projection planes; any plane rebuilds from the journal without touching
  your files. Enforced by a 500-SIGKILL crash gate in CI.
- **Hostile files can't hurt it**: extraction runs in deny-default sealed
  worker processes that can only read the fd they're handed.
- **Finds meaning, not just words**: local MiniLM embeddings, hybrid
  rank fusion, and provenance glyphs showing *why* each result matched.
- **Live**: standing queries stream enters/leaves; semantic alerts turn
  "watch for anything like an invoice" into a system notification.

Design: [DESIGN.md](DESIGN.md) · plan: [IMPLEMENTATION.md](IMPLEMENTATION.md)
· query language: [DIVERGENCES.md](DIVERGENCES.md)
