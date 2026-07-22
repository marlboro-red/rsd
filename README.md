# rsd

**A reactive, semantic index of everything on your machine.** A ground-up
replacement for macOS Spotlight's indexing daemon: crash-proof by
construction, sandboxed extraction, on-device semantic understanding, live
standing queries, an agent surface (MCP), and a native search palette.
Nothing ever leaves your Mac.

Latency targets are in [DESIGN.md §15](DESIGN.md) and are labeled as targets —
they are benchmarked (`cargo test -p rsd-daemon --release -- --ignored`), not
gated in CI.

## Quick start

```bash
./scripts/fetch-model.sh                      # optional: the learned embedder (~90MB, local)
cargo build --release && ./scripts/build-app.sh

./target/release/rsd-daemon watch ~/Documents # index + watch a folder
open dist/RSD.app                             # then ⌥Space from anywhere
```

### Searching

```bash
rsdfind --state <state-dir> '"quarterly invoice"'   # RQL: bare args are RQL
rsdfind --state <state-dir> --hybrid  "quarterly invoice"   # natural language
rsdfind --state <state-dir> --semantic "a bill I need to pay"
rsdfind --state <state-dir> -live '"invoice"'               # standing query
rsdfind --state <state-dir> -live --semantic "invoice"      # threshold alert
```

Bare arguments are [RQL](DIVERGENCES.md) — quote a phrase as `'"like this"'`,
or use `kMDItemFSName == "*.pdf"c`. `--hybrid` and `--semantic` take plain
language instead.

Queries go to the running daemon over `<state-dir>/rsd.sock`; the client
proves first-party authority with the loopback token in `<state-dir>/http.token`
(0600). With no daemon listening, `rsdfind` reads the state dir directly and
says so on stderr — `--offline` forces that path and fails rather than falling
back. The catalog is a single-writer store, so the direct path only works on a
stopped index.

### Agents

`rsd-mcp --state <state-dir> --scope <allowed-root>` (repeat `--scope` for more
roots). Trusted first-party use must opt in explicitly with `--unrestricted`.
MCP runs over stdio with search + grounded snippets, and requires a running
daemon. `--scope` is sent to the daemon and enforced there during candidate
generation, so a bug in the MCP process cannot widen it.

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
