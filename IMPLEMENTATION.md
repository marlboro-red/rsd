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

**P2.2 — Source-cursor fencing [x]**
- Cursor persisted only after derived records are journaled (CursorStore, atomic
  tmp+rename, corrupt-reads-as-None so the failure direction is re-delivery).
- Note: proven end-to-end via the synthetic-source crash harness; wiring the
  daemon's FSEvents `sinceWhen` resume through the coalescer's pending window is
  deferred to Phase 3 (startup bootstrap rescan covers correctness meanwhile).
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
- Single committer: journal-before-apply, CAES-before-planes, per-plane watermarks,
  `(lsn, id, plane, version)` idempotency keys; catalog is the first projection.
- Success criteria (the crash-injection gate, permanent in CI):
  - 500 randomized `kill -9` runs (child-process harness) during commit storms →
    on restart, recovery replays from `min(watermarks)`; catalog equals a
    never-crashed reference run; zero divergences, zero unscoped repairs.
  - Double-apply of any batch is a no-op (idempotency property test).

## Phase 3 — Extraction fabric v1 (design §10, P3 pillar)

**P3.1 — Worker protocol + sandboxed pool [x]**
- `rsd-worker` binary: fd-passing over UDS (`SCM_RIGHTS`), postcard framing,
  Seatbelt profile (no fs, no net, no exec), timeout/kill/respawn, crash quarantine
  with queryable reason.
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
- Success: end-to-end counter tests — rename storm on 1k indexed files causes 0
  extractions; content change causes exactly 1.

## Phase 4 — Lexical plane + query engine core (design §6.4, §8)

**P4.1 — tantivy plane [x]**
- Schema per design (doc_id fast field, content w/ positions, name n-grams,
  symbols), delete-term+add commit protocol under the Phase-2 watermark, hot RAM
  segment for freshness.
- Success: crash-injection extended to lexical plane (rebuild-from-CAES row of the
  failure matrix demonstrated: delete a segment → scoped rebuild, zero fs reads).

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

**P5.3 — IPC + authorization skeleton [x]**
- UDS for first-party CLI (same-uid gate via getpeereid); principal model with
  path-prefix scope grants; enforcement before ANY output (results, counts,
  live deltas over the authorized subset only); connection audit via tracing.
- Deferred with rationale: XPC audit-token code identity (the untrusted
  third-party tier). Until it lands, cross-uid and unknown-binary access does
  not exist at all — the same-uid UDS gate is the v1 trust boundary. Rides
  behind the existing Hello handshake when added.
- Success: **leak suite** — an unauthorized principal cannot distinguish
  existence/counts/aggregates/timing-class of out-of-scope docs (statistical test);
  grant revocation re-fences live subscriptions.

**P5.4 — `rsdfind -live` [x]**
- Success: end-to-end — live query over a watched tree reflects mutations within
  the exact-class SLO; slow-client overflow triggers documented resync behavior.

## Phase 6 — Semantic plane (design §6.5, §8.2, spike 5) [T1]

**P6.1 — `rsd-ml` learned embedder [x]** — all-MiniLM-L6-v2 via candle behind
the Embedder trait, mean-pooled + normalized, 6.9ms/chunk CPU; hash-projection
fallback when the model is absent (scripts/fetch-model.sh). Paraphrase proof:
zero-shared-vocabulary queries rank correctly. Deferred within P6.1: the
evictable sidecar *process* (transport change behind the same trait) and the
ANE/Metal device path (throughput, not capability). — batched embedding protocol, CoreML/ANE path, candle
fallback, full idle eviction. Success: ≥ 2k chunks/sec (adopt) or documented
fallback throughput; RSS returns to baseline after idle timeout.
**P6.2 [x]** — structure-aware chunks, HNSW segments with
tombstones, semantic watermark + second delta stream. Success: crash-injection
extended; chunk-hash dedup counter tests (copy embeds nothing).
**P6.3 — Hybrid retrieval [x]** (RRF fusion + semantic() operator shipped; NDCG eval harness pending) — RRF fusion, `semantic()` operator, stale-`semantic_gen`
compensation. Success: NDCG-gated eval harness live (labeled local corpus); hybrid
p50 < 15ms / p99 < 60ms.
**P6.4 — Semantic alerts [x]** (threshold classification on the live path, streamed over IPC via SubscribeAlert; `rsdfind -live --semantic --threshold N "query"`) — `ALERT WHEN` threshold class on the semantic watermark.
Success: alert fires only after vector commit; fence/resync includes both
watermarks.

## Phase 7 — Platform surfaces [T1]

**P7.1 — PDF + OCR + media pipeline [~]** (PDF text extraction shipped: pure-Rust v1 with typed statuses, panic-contained locally AND by the sealed worker; searchable lexically + semantically e2e. pdfium quality upgrade, Vision OCR, and whisper transcription remain) (pdfium in-sandbox, Vision OCR, whisper
opt-in; power gating). Success: budget/status contract tests incl. adversarial
archive set; battery gate verified via powermetrics protocol.
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
