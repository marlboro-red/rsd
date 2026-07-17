# rsd — Implementation Plan

Companion to `DESIGN.md` (v3). Phases are strictly ordered by the risk register
(§16): the convergence kernel first, storage/commit machinery second, everything
ambitious on top. Each task has **exact success criteria** — a task is done when its
criteria pass in CI, not before. Status markers: `[ ]` todo · `[~]` in progress ·
`[x]` done.

Mapping to design tiers: Phases 0–5 ≈ T0, Phases 6–7 ≈ T1, Phase 8 ≈ T2.

---

## Phase 0 — Workspace scaffold

**P0.1 — Cargo workspace [x]**
- `cargo build --workspace` and `cargo test --workspace` succeed on macOS arm64.
- Crates: `rsd-catalog`, `rsd-ingest`, `rsd-fsevents`, `rsd-testkit`, `rsd-daemon`
  (further crates added by the phase that needs them).
- Workspace-level lints: `unsafe_code` allowed only in `rsd-fsevents` (FFI).

**P0.2 — CI gate script [x]**
- `scripts/ci.sh` runs fmt-check, clippy (deny warnings), and all tests; exits
  non-zero on any failure. (Becomes the hook for the permanent convergence gate.)

## Phase 1 — Convergence kernel (design §5, §6.7, spike 1)

Goal: a catalog that provably converges to filesystem truth via bootstrap scan,
scoped reconciliation, and FSEvents-driven incremental updates. No journal, no
extraction, no query engine yet — correctness of observation only.

**P1.1 — Catalog: FsObject/Entry store on redb [x]**
- Two-entity model: objects (identity: dev+ino+birthtime evidence) and entries
  (path → object, many-to-one), `by_path` and `by_fileid` indexes.
- Success criteria:
  - Unit tests green: create/update/rename/remove; hard-link add/unlink leaves
    sibling entries and object intact; last-entry removal removes object;
    reopen-from-disk persistence.
  - Property test: 1,000 randomized op sequences (upsert/rename/remove/hardlink)
    maintain invariants — no dangling entry→object refs, `by_path` and `by_fileid`
    exactly mirror entries/objects, object entry-lists match entries table.

**P1.2 — Testkit: tree generator, mutator, convergence oracle [x]**
- `gen_tree` (seeded, nested dirs/files/symlinks), `mutate` (seeded random
  create/write/delete/rename/mkdir/hardlink storms), `fs_listing`,
  `assert_converged(catalog, root)` comparing exact (path, kind, ino, size) sets.
- Success criteria: deterministic under fixed seed; used by every later test.

**P1.3 — Scan-based reconciliation [x]**
- Bootstrap scan and scoped rescan (single-level and recursive) via readdir-diff
  against the catalog; symlinks recorded as links, never followed.
- Success criteria:
  - Bootstrap of a generated 3,000-node tree → `assert_converged` passes.
  - 1,000 random mutations, then one recursive rescan → converged.
  - Scoped rescan touches only the requested subtree (verified by op counters).
  - Full suite < 60s in CI.

**P1.4 — Coalescer with structural backpressure [x]**
- Bounded input channel; per-path debounce (500ms quiet, 5s cap); dedup; overflow
  degrades to scoped rescan markers (P4: queue memory O(directories)).
- Success criteria (pure state-machine tests with synthetic clock, no real FS needed):
  - N events on one path within quiet window → exactly one WorkItem.
  - Continuous events on one path → WorkItem within 5s cap.
  - Channel overflow → rescan marker set, no event loss unaccounted, no unbounded
    growth (asserted on internal map size).

**P1.5 — FSEvents wrapper [x]**
- Safe wrapper over `FSEventStreamCreate` with `kFSEventStreamCreateFlagFileEvents`:
  event IDs, flag decoding (`MustScanSubDirs`, `EventIdsWrapped`, rename hints),
  callback → bounded channel handoff, clean start/stop, `sinceWhen` resume support.
- Success criteria (live, macOS): create/modify/rename/delete under a watched
  tempdir each produce decoded events with correct paths within 5s; stream stops
  cleanly (no leaked runloop thread — asserted via join with timeout).

**P1.6 — End-to-end convergence harness (the permanent CI gate) [x]**
- Pipeline: FSEvents → coalescer → applier (lstat-resolving work items → catalog).
  `lstat` is the truth-resolver: work items say *look here*, never *believe this*.
- Success criteria:
  - Live test: bootstrap a tree, run pipeline, apply 500 random mutations while
    watching, quiesce → `assert_converged` with **zero full rescans** (counters).
  - Overflow test: flood events past channel capacity → converges via scoped
    rescan path.
  - Hard-link and rename storms converge (identity preserved via `by_fileid`).
- `rsd-daemon watch <root>` smoke binary: bootstrap + live convergence + stats line.

## Phase 2 — Journal, CAES, commit state machine (design §6.1–6.2, §7.3–7.4, spike 2)

**P2.1 — Journal (`rsd-log`) [x]**
- Append-only segmented log, per-record blake3 checksum, LSN allocation, segment
  seal with membership manifest, replay iterator, corrupt-record detection.
- Success: unit tests incl. torn-write simulation (truncate mid-record → clean
  detection, replay stops at last valid record); fuzz decode never panics.
- Correctness pass: sync-on-append active segments persist a durable path-scope
  manifest. Mid-segment corruption quarantines the original bytes, reconstructs
  the LSN range with repair placeholders, and appends current filesystem truth for
  only the affected paths; content is re-keyed through the normal pinned-fd path.

**P2.2 — Source-cursor fencing [x]**
- Cursor persisted only after derived records are journaled (CursorStore,
  fsync-tmp + rename + parent-directory fsync; corrupt-reads-as-None so the
  failure direction is re-delivery).
- Note: proven end-to-end via the synthetic-source crash harness; wiring the
  daemon's FSEvents `sinceWhen` resume now ships. The coalescer attaches a cursor
  only when its pending set empties, and the FIFO applier persists it after every
  earlier derived commit. Missing/corrupt cursors capture a pre-bootstrap event ID,
  so changes during bootstrap replay after the watcher starts.
- Success: kill-restart test — events delivered between journal and cursor-advance
  are re-delivered and idempotently re-applied; zero lost transitions across 100
  randomized kill points.

**P2.3 — CAES v1 [x]**
- Content-addressed store for extraction records (text+attrs placeholder schema),
  keyed `(content_hash, extractor_id+version, hints_hash, abi_version)`; checksums;
  retention stub (unlimited).
- Success: round-trip, corrupt-record detection, dedup hit on identical content
  under two paths (copy indexes with zero extraction calls — counter-verified).

**P2.4 — Commit state machine + idempotent apply [x]**
- Single committer: journal-before-apply, CAES-before-planes, per-plane watermarks;
  catalog is the first projection. Catalog replay streams in bounded windows;
  lagged lexical/vector projections rebuild from current catalog identities + CAES
  without filesystem reads, avoiding historical path-reuse ambiguity.
- Success criteria (the crash-injection gate, permanent in CI):
  - At least 500 randomized `kill -9`s (child-process harness) during mixed
    Upsert/RemovePath/SetContent storms. Survivor and fresh catalog, lexical, and
    vector planes must equal the never-crashed reference/current content set;
    fresh content planes read only journal-derived catalog state + CAES.
  - Double-apply of any batch is a no-op (idempotency property test).

## Phase 3 — Extraction fabric v1 (design §10, P3 pillar)

**P3.1 — Worker protocol + sandboxed pool [x]**
- `rsd-worker` binary: fd-passing over UDS (`SCM_RIGHTS`), postcard framing,
  Seatbelt profile (no fs, no net, no exec), timeout/kill/respawn, crash quarantine
  with queryable reason. Retry counts are persisted in CAES, so restarting does not
  reset hostile content's quarantine budget.
- Success: hostile-input test (worker that segfaults/hangs on cue) → daemon
  unaffected, file quarantined after N retries, pool self-heals; sandbox denies
  demonstrated by a probe worker (open("/etc/passwd") fails).
- Shipped variation: instead of a host-side Seatbelt profile, the worker
  self-seals with `sandbox_init("(deny default)")` after startup — tighter than
  any external profile (no dyld carve-outs needed), verified by probe + control
  group.

**P3.2 — Native extractors: text + source [x]**
- Encoding detection, plain text, tree-sitter symbols for an initial language set
  (shipped: Rust, Python, JavaScript, Go, C; TypeScript/C++ grammars are
  mechanical follow-ups); extraction limit contract enforced (input/output/time
  budgets, partial results, typed status codes).
- Success: golden-file tests per format; limit tests (oversize input → clean
  `ResourceBudgetExceeded` partial, not OOM); status codes land as queryable
  catalog attributes.

**P3.3 — Ingest integration [x]**
- Dispatcher routes Extract items through CAES-check → worker → committer;
  `AttrsOnly` path (rename/chmod) provably skips extraction.
- Correctness pass: content identity hashes the whole file even when extraction is
  budget-truncated. The dispatcher opens once, validates that fd against the
  catalog generation, hashes and extracts through the same fd for native, WASM,
  OCR, and transcription processors, re-fstats afterward, and revalidates again
  immediately before the SetContent journal append. Races are discarded for a
  later watcher/bootstrap retry.
- Success: end-to-end counter tests — rename storm on 1k indexed files causes 0
  extractions; content change causes exactly 1.

## Phase 4 — Lexical plane + query engine core (design §6.4, §8)

**P4.1 — tantivy plane [x]**
- Schema per design (doc_id fast field, content w/ positions, name n-grams,
  symbols), delete-term+add commit protocol under the Phase-2 watermark, hot RAM
  segment for freshness.
- Success: crash-injection covers the lexical and vector planes, including
  deletion/invalidation and fresh rebuild from catalog + CAES with zero fs reads.

**P4.2 — RQL v1: versioned grammar, parser, planner, executor [x]**
- Attribute predicates (typed comparisons, `c`/`d` modifiers, `$time.*`,
  `InRange`), text predicates, boolean composition, `-onlyin` scoping;
  `UnsupportedPredicate` for everything else; `EXPLAIN`.
- Success: grammar corpus tests green; DIVERGENCES.md documents the v1
  compatibility posture. Differential corpus runs against live mdfind deferred
  to the Phase-5 hardening pass (Spotlight won't deterministically index CI
  fixture trees; needs a persistent pre-indexed bench fixture).

**P4.3 — `rsdfind` one-shot [x]**
- Success: `-onlyin`, `-name`, `-count`, `-0`, `--explain` on daemon state
  (`-attr` lands with the attribute store expansion); lexical query MEASURED at
  p50 = 219µs / p99 = 354µs on the 100k-doc corpus — 4.5×/28× inside target.
  Phase-4 note: rsdfind reads a quiesced state dir; live-daemon IPC is P5.

## Phase 5 — Live views, IPC, authorization (design §9, §11, spikes 3–4)

**P5.1 — Delta stream + exact-class views [x]**
- DocDelta with old-state evidence; trigger index; exact point-incremental
  maintenance for attribute/membership/aggregate views; resync protocol.
- Success: property test — for 10k random mutation streams, incremental view state
  == from-scratch query at every fence; notify latency p99 < 10ms post-commit on
  bench hardware.

**P5.2 — Single-doc matcher [x]**
- Success: property test — bit-identical tokenization + boolean membership vs.
  on-disk index across the modifier surface (phrases, wildcards, `c`/`d`);
  documented exclusion: scoring.

**P5.3 — IPC + authorization skeleton [partial]**
- UDS same-uid gate via getpeereid; one explicit `Scope` type shared by query and
  live paths; unknown principals and empty grants deny all; path grants compare
  components rather than string prefixes. The leak regression covers results,
  counts, initial subscription state, and live deltas.
- The `Hello` principal is still caller-asserted. The shipped daemon therefore
  configures no UDS grants; XPC audit-token identity, persistent/user-visible grant
  management, dynamic revocation re-fencing, aggregates, and statistical timing
  tests remain T0 targets. Lexical and catalog RQL counts are exact above the hit
  cap; ranked semantic predicates reject exact-count requests as undefined.
- Lexical authorization is enforced inside Tantivy candidate generation through
  exact component-ancestor terms refreshed on rename/unlink/hard-link changes.
  Catalog scans enumerate only intersected grants, and semantic exact-scan filters
  unauthorized oids before ranking. A limit=1 regression proves unauthorized rank
  positions cannot consume the scoped lexical budget. Mixed lexical predicates use
  per-object index membership rather than a capped precomputed oid set.
- Both UDS and loopback HTTP listeners cap active connection threads; excess peers
  are rejected. Pre-auth handshakes time out, IPC frames and HTTP headers are
  bounded, and search limits are clamped. The HTTP listener receives an explicit
  startup scope (unrestricted for today's first-party app) and applies it to
  lexical, semantic, hybrid, and live-view execution. Status counts are scoped;
  the global metrics endpoint requires unrestricted scope.

**P5.4 — `rsdfind -live` [x]**
- Success: end-to-end — live query over a watched tree reflects mutations within
  the exact-class SLO; slow-client overflow triggers documented resync behavior.

## Phase 6 — Semantic plane (design §6.5, §8.2, spike 5) [T1]

**P6.1 — learned embedder + ANE sidecar [partial]** — MiniLM via candle (CPU,
6.9ms/chunk) AND the ANE sidecar: rsd-embed runs Apple's NLContextualEmbedding
(512-dim, Neural-Engine transformer, no model files shipped) as a separate
evictable process behind the Embedder trait; the daemon respawns it
transparently if it dies. READY and steady-state reads are deadline-bounded;
respawned dimensions are honored; invalid/zero vectors are rejected without
advancing the vector watermark. Chain: ANE sidecar > in-process MiniLM > hash.
Batching, typed fallible embedding at the trait boundary, and idle eviction remain.
**P6.2 [partial]** — structure-aware chunks in a redb exact-scan projection with
a synchronous semantic watermark. HNSW segments, a second async delta stream, and
chunk-hash dedup remain targets. Crash injection now covers vector rebuild and
deletion/invalidation.
**P6.3 — Hybrid retrieval [x]** (RRF fusion + semantic() operator shipped; NDCG eval harness pending) — RRF fusion, `semantic()` operator, stale-`semantic_gen`
compensation. Success: NDCG-gated eval harness live (labeled local corpus); hybrid
p50 < 15ms / p99 < 60ms.
**P6.4 — Semantic alerts [x]** (threshold classification on the live path, streamed over IPC via SubscribeAlert; `rsdfind -live --semantic --threshold N "query"`) — `ALERT WHEN` threshold class on the semantic watermark.
Success: alert fires only after vector commit; fence/resync includes both
watermarks.

## Phase 7 — Platform surfaces [T1]

**P7.1 — PDF + OCR + media pipeline [x]** — PDF text extraction; Vision OCR
(screenshots searchable by pixel-text, gate-tested); **whisper A/V
transcription** (rsd-transcribe: whisper.cpp + symphonia decode, separate
process, headless, no auth prompts — chosen over Apple Speech which requires
interactive TCC and hangs for a background daemon). Opt-in per design
(RSD_TRANSCRIBE=1 + fetched model). pdfium quality upgrade remains. (pdfium in-sandbox, Vision OCR, whisper
opt-in; power gating). Success: budget/status contract tests incl. adversarial
archive set; battery gate verified via powermetrics protocol.
- Processor routing is decided once and returns the real CAES identity. OCR
  settings, transcription model revision, and the full WASM module hash enter the
  cache key; a canonical CAES alias preserves projection rebuild compatibility.
**P7.2 — WASM extractor ABI** (WIT interface, fuel/memory/output budgets, EPUB
reference plugin). Success: within 2× native throughput on text-heavy formats;
hostile-plugin suite (infinite loop, alloc bomb, output flood) all contained.
**P7.3 — MCP server [x]** (rsd_search lexical/semantic/hybrid/rql + rsd_snippets with byte offsets, stdio JSON-RPC) (search/snippets/subscribe/provenance/history, scope-gated).
Success: leak suite passes against MCP principal; agent round-trip demo with
byte-range citations.
**P7.4 — mdimporter compat** (per-bundle processes, crash quotas). Success: top-10
common third-party importers run or are cleanly blacklisted; daemon uptime
unaffected by importer crash storm.

**P7.5 — RSD.app native UI [x]** (added mid-T1 by request) — SwiftUI search
palette over a localhost JSON API (`/api/search`, `/api/status`; 127.0.0.1
only, first-party surface): search-as-you-type across hybrid/exact/meaning
modes, grounded snippets, file icons, ↩ open / ⌘↩ reveal, latency readout.
`scripts/build-app.sh` → dist/RSD.app.

## Phase 8 — Time & lineage [T2]

**P8.1 — Bitemporal history** — `history` table, compaction with surfaced
resolution, `AS OF`/`CHANGED SINCE`/`DIFF`. Success: temporal answers carry
resolution + availability labels; compaction property tests.
**P8.2 — CAES retention + historical content search** — retention policy engine,
`ContentVersionUnavailable` states, candidate-generation + replay search. Success:
documented cost bounds hold on bench corpus.
**P8.3 — Sentinel + provenance** — ES sysext (dev-signed), facts/claims/inferences
with evidence chains, `DERIVED FROM ... MIN CONFIDENCE`. Success: gap-detection
tests; inference precision eval on a scripted copy/export scenario suite.

---

## Working agreements

- Every phase lands with its tests in CI; the convergence harness (P1.6), crash
  injection (P2.4), and leak suite (P5.3) are permanent gates that later phases must
  keep green.
- Benchmark-matrix entries accrete from P4.3 onward; design targets in DESIGN.md §15
  harden into acceptance criteria as their corpus lands.
- No plane ships without its row of the failure matrix (§6.8) demonstrated by test.
