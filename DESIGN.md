# rsd — Design Document

**A reactive, semantic, time-traveling index of everything on your machine.**

Status: **v3 (final)** · 2026-07-15 · target platform: macOS 14+ (arm64 primary, x86_64 supported)
Lineage: v2 → `DESIGN_CODEX_CRITIQUE.md` → `DESIGN_FABLE_REBUTTAL.md` → this document.

**Drafting rule, binding on every future edit:** every guarantee in this document names
its *authority* (which store or mechanism backs it), its *failure mode*, and its *test*.
A claim that can't do all three is a target, and is labeled as one.

---

## 0. Thesis

Spotlight was a landmark in 2005 and has barely moved since. It offers typed metadata
predicates, scoped search, and live-updating queries via `NSMetadataQuery` — and stops
there: a twenty-year-old predicate model over current-state metadata, an unsandboxed
and famously crashy importer ABI, index recovery that is silent and unobservable
(failure mode: quietly re-crawl the disk for hours), no semantic retrieval, no history,
no provenance, and no surface through which the OS's most valuable dataset — everything
you've ever worked on — can be used by anything smarter than a search box.

rsd's thesis is that the local filesystem should behave like a **live, queryable,
versioned database with a semantic understanding of its contents** — and that in 2026,
with local embedding models running on the Neural Engine, agents speaking MCP, and Rust
giving us fearless concurrency over memory-mapped indexes, this is buildable by a small
team on one machine's resources.

We are not building a better mds. We are building the substrate mds should have become:

1. **Reactive** — standing queries are incrementally-maintained views over a committed
   delta stream, with a published contract for which query classes get exact
   point-incremental maintenance and which get bounded repair (§9). Smart folders,
   agent triggers, and dashboards are the same primitive.
2. **Semantic** — documents are chunked, embedded on-device, and searchable by meaning.
   Hybrid lexical+vector retrieval with fusion ranking is the default free-text path.
3. **Temporal** — metadata history is guaranteed; historical content search is a
   storage policy you can buy with retention (§13). *"My Downloads folder as of last
   Tuesday"* is a query, with honest availability semantics.
4. **Provenance-aware** — rsd records where things came from as *evidence-carrying
   facts* (which process wrote it, which URL it claims to be from, what shares its
   content) and derives lineage as explicit, confidence-scored inference.
5. **Total in ambition, honest in state** — screenshots are OCR'd, audio transcribed,
   code parsed into symbols, archives indexed through. Unindexed *by neglect* is a
   bug; unindexable *by policy or physics* (encryption, cloud placeholders, resource
   budgets) is a labeled, queryable state.
6. **A platform with a permission model** — a typed IPC API with real caller
   authorization (§11), an `mdfind`-compatible CLI, a Rust/C client library, and an
   MCP server so local agents can query, subscribe, and ground themselves in your
   corpus — under scopes you granted. Extraction is extensible via a sandboxed WASM
   plugin ABI: the mdimporter model reinvented with capability security.

Everything is local. Nothing leaves the machine. That is not a limitation — it is the
product.

### The gap, stated accurately

| Dimension | Spotlight (mds) | rsd |
|---|---|---|
| Change detection | FSEvents | FSEvents file-level events as baseline; optional EndpointSecurity tier adds process attribution; **all sources treated as hints — reconciliation is the correctness mechanism** |
| Worker model | mdworker process stampedes, unsandboxed third-party importers in shared processes | Fixed sandboxed pools, fd-only capability grants, per-importer isolation, WASM plugins |
| Index durability | Corruption → silent full re-crawl | Checksummed planes with a published failure matrix; **no plausible single failure escalates beyond scoped repair** (§6.8); full reconciliation is rare, observable, cancellable |
| Retrieval | Boolean predicates + content relevance | Hybrid BM25 + ANN + graph, RRF fusion, local learned reranker, inspectable ranking |
| Live queries | `NSMetadataQuery`: initial gather + batched live updates, predicate model only | Delta-stream views with published maintenance classes, p99 < 10ms notify for exact classes, semantic alerts, live aggregates |
| History | None | Bitemporal catalog (guaranteed); historical content search within retention (§13) |
| Extensibility | `.mdimporter` CFPlugIns | WASM component ABI + legacy mdimporter compat shims |
| Caller security | Same-user processes query freely | Trust-tiered authorization: code-signed first-party clients, explicit scope capabilities for everyone else (§11) |
| AI surface | None | Local MCP server: search / snippets / subscribe / provenance / history, scope-gated |

---

## 1. Authorities: who is the truth about what

Four authorities, kept strictly distinct. Most of v2's contradictions came from
blurring them; every guarantee below cites one.

| Authority | Owns | Never claims |
|---|---|---|
| **The filesystem** | Current state of files. External, independently mutating, reports changes through lossy channels. | — |
| **The journal** (`rsd-log`) | *Accepted indexing transitions*: what rsd durably observed and decided to apply, in order (LSNs). | The content of files; completeness of observation |
| **Projections** (catalog, lexical, vector, graph) | Fast answers about current state. Caches of the journal + CAES; each carries its applied watermark. | Truth independent of the journal |
| **CAES** (content-addressed extraction store) | Retained extraction records: the durable text/attrs/chunks/references derived from content, versioned by extractor and model. | Bytes it was never given; records past retention |

Consequences:

- **Convergence** (catalog ⇄ filesystem) is the job of event ingestion *plus
  reconciliation* (§7.5) — never of journal replay, which can only reproduce what was
  observed.
- **Recovery** (projections ⇄ journal + CAES) is replay: any projection can be rebuilt
  scoped or whole from the journal and CAES without touching the disk's files, at
  CAES-retention fidelity.
- **History** (§13) is journal ordering + catalog history + CAES retention — three
  different guarantees at three different prices, never conflated.

---

## 2. Design pillars

- **P1 — Convergence over completeness.** The catalog must converge to filesystem
  truth under any event stream: dropped events, coalesced events, event-ID wraparound,
  reboots, `kill -9` at any instruction. Authority: reconciliation subsystem.
  Failure mode: bounded staleness until the anti-entropy pass covers the divergent
  scope. Test: the convergence harness (§16, spike 1) is a permanent CI gate.
- **P2 — The journal orders; projections cache; CAES retains.** Every projection is
  rebuildable — scoped or whole — from journal + CAES, independently and online.
  Corruption of any single plane never cascades (§6.8).
- **P3 — Untrusted bytes never touch the daemon.** All content parsing happens in
  sandboxed workers holding a read-only fd and a scratch dir. Blast radius of hostile
  input: one worker, one request, one quarantined file.
- **P4 — Backpressure is structural.** Every queue is bounded; overload degrades
  granularity (per-file → per-directory rescan markers), never memory. Worst-case
  queue memory is O(directories).
- **P5 — The user's machine is the user's.** CPU/IO/battery/thermal budgets are
  scheduler inputs. Heavy work (embeddings, OCR, transcription) trickles and is
  gated on power state; interactive work preempts. No telemetry. No network.
- **P6 — Callers see only what they were granted.** Authorization is enforced before
  candidate generation, not by filtering final results (§11). Authority: the scope
  grant store + XPC audit tokens. Test: the leak suite (counts/aggregates/timing/
  deltas must be computed over authorized subsets only).

---

## 3. Process model

```
                                   ┌───────────────────────────────────────────────┐
  FSEvents (file-level) ─────────▶ │                 rsd  (daemon)                  │
                                   │                                                │
  EndpointSecurity ──────────────▶ │  ┌──────────┐  ┌───────────┐  ┌────────────┐  │
  (rsd-sentinel sysext, optional)  │  │ ingest + │─▶│ scheduler │─▶│ dispatcher │  │
                                   │  │ reconcile│  │ (budgeted)│  └─────┬──────┘  │
                                   │  └──────────┘  └───────────┘        │ fd pass │
                                   │        ▲                            ▼          │
                                   │  ┌─────┴─────┐   ┌───────────────────────┐    │
                                   │  │ committer │◀──│   result assembler    │    │
                                   │  └─────┬─────┘   └───────────────────────┘    │
                                   │        │ delta streams (doc, semantic)         │
                                   │        ▼                                       │
                                   │  ┌───────────────┐    ┌────────────────────┐  │
                                   │  │ live-view     │    │  query engine      │  │
                                   │  │ engine        │    │  (RQL / NL / hybrid)│  │
                                   │  └──────┬────────┘    └──────────┬─────────┘  │
                                   │         └───────────┬────────────┘            │
                                   │              authorization layer               │
                                   │                     ▼                          │
                                   │   XPC (identity-bearing) · UDS (first-party)  │
                                   │              · MCP server                     │
                                   └──────┬──────────────┬──────────────┬──────────┘
                                          │              │              │
                              ┌───────────▼───┐  ┌───────▼────────┐  ┌──▼──────────────┐
                              │ rsd-worker ×N │  │ rsd-wasm-host  │  │ rsd-compat ×1/  │
                              │ native Rust,  │  │ ×M  wasmtime,  │  │ importer  ObjC, │
                              │ tightest      │  │ WASM extractor │  │ loads one       │
                              │ Seatbelt      │  │ plugins        │  │ .mdimporter     │
                              └───────────────┘  └────────────────┘  └─────────────────┘
                              ┌───────────────┐  ┌────────────────┐
                              │ rsd-ml        │  │ rsd-media      │
                              │ embeddings/   │  │ OCR (Vision),  │
                              │ rerank on ANE │  │ ASR (whisper)  │
                              └───────────────┘  └────────────────┘
```

- **`rsd`** — the daemon (launchd agent, one per user). Owns the journal, CAES, all
  projections, the single committer, query + live-view engines, the authorization
  layer, and the IPC/MCP servers. Never parses untrusted content. Target idle RSS:
  < 150 MB (design target; hardened by the benchmark matrix, §15).
- **`rsd-sentinel`** — *optional* EndpointSecurity system extension. When present, it
  adds high-fidelity per-file events with **process attribution** (which pid/binary
  wrote/renamed/deleted what), feeding the provenance plane and reducing rescan
  frequency. Like every source, it is a **hint stream with gap detection** (sequence
  numbers, extension-restart epochs) — never a correctness dependency. Distribution
  requires Apple's restricted ES entitlement plus user approval and FDA; this is
  tracked as a *product risk*, not a coding task (§16). Absent sentinel, provenance
  degrades to xattr heuristics (`kMDItemWhereFroms`, quarantine data) and rsd runs
  pure FSEvents.
- **`rsd-worker`** (pool, N ≈ physical cores/2) — native Rust extractors. Sandbox: no
  filesystem, no network, no fork/exec; input is a read-only fd over `SCM_RIGHTS`,
  output a typed extraction record. Per-request timeout, memory rlimit,
  kill-and-respawn; repeated failures quarantine the *file* with a recorded,
  queryable reason.
- **`rsd-wasm-host`** (pool, M) — wasmtime hosting third-party extractor plugins
  compiled to WASM components: fuel-metered, memory-capped, capability-scoped (a
  plugin cannot express "open a file"). Host-side container handling (decompression,
  archive walking) runs under the same budget contract as guests (§10.1). This is the
  mdimporter successor we want the ecosystem to adopt: publish an extractor as a
  `.wasm`, installable by drag-and-drop, unable to harm the host.
- **`rsd-compat`** — one process per loaded `.mdimporter` bundle (ObjC/`objc2`,
  CFPlugIn + runloop). One bundle per process → per-importer blacklisting, zero
  shared fate. Looser sandbox by necessity (read-only FS, no network). A bridge, not
  a destination.
- **`rsd-ml`** — embedding model, reranker, NL-query encoder; quantized models via
  CoreML (ANE) with candle/Metal fallback. Separate process so model memory is fully
  evictable at idle and an ANE/Metal fault can't take the daemon down.
- **`rsd-media`** — Vision OCR for images/scans/text-layer-less PDF pages; whisper.cpp
  for audio/video transcription with timestamps. Lowest priority, power-gated,
  scope-configurable, A/V transcription opt-in.
- **`rsdfind`**, **`rsdctl`** — CLI clients. `rsdfind` is flag-compatible with
  `mdfind` (`-onlyin`, `-live`, `-count`, `-attr`, `-name`, `-0`). `rsdctl` manages
  scopes, grants, plugins, retention, and diagnostics: `doctor`, `top` (live view of
  what's being indexed and why), `ranking explain`, `rebuild --plane`, `grants`.

Why processes, not tasks: async solves *scheduling*; it does not provide fault
isolation, privilege separation, or memory eviction. Parsing untrusted bytes, running
third-party code, and holding multi-hundred-MB models are the three things that must
sit behind a process boundary. The tokio queue's job is to feed **fixed** pools
through bounded channels — the anti-stampede design.

---

## 4. Crate breakdown

Cargo workspace; one concern per crate; the daemon is thin glue.

| Crate | Responsibility |
|---|---|
| `rsd-fsevents` | Safe FSEvents FFI with `kFSEventStreamCreateFlagFileEvents`: event-ID journaling (`sinceWhen` resume), flag decoding (`MustScanSubDirs`, `EventIdsWrapped`, item-rename hints), bounded handoff from the C callback thread |
| `rsd-sentinel-proto` | Wire protocol shared with the ES extension; event schema with process attribution, sequence numbers, restart epochs |
| `rsd-ingest` | Coalescer, scheduler, budgeted dispatcher, bootstrap crawler, **reconciliation subsystem** (scoped scans, anti-entropy audit) |
| `rsd-log` | The journal: append-only, segmented, per-record checksummed, source-cursor fencing |
| `rsd-caes` | Content-addressed extraction store: durable, checksummed extraction records + embedding cache, retention policy, projection-version bookkeeping |
| `rsd-catalog` | FsObject/Entry catalog over redb: identity, attributes, bitemporal history, path/inode/hash indexes |
| `rsd-lexical` | tantivy plane: schema, commit protocol, segment lifecycle, single-doc membership matcher |
| `rsd-vector` | Semantic plane: HNSW segments with tombstones, PQ storage tier, semantic watermark, per-segment model tags |
| `rsd-graph` | Provenance plane: evidence-carrying typed edges, inference rules, traversal operators |
| `rsd-extract` | `Extractor` trait, content sniffing, extraction limit contract, native extractors: text/source (tree-sitter symbols), PDF (pdfium), HTML/XML/Markdown, archives, mail, media metadata |
| `rsd-wasm-abi` | WIT component interface, host bindings, fuel/memory/output budget policy |
| `rsd-compat` | mdimporter bridge: CFPlugIn loading, schema.xml typing, CFDictionary→typed attrs |
| `rsd-ml-proto` / `rsd-ml` | Embedding/rerank sidecar and batched protocol |
| `rsd-query` | RQL: versioned grammar, typed AST, planner with statistics, executor, `EXPLAIN` |
| `rsd-live` | Live views: trigger index, per-class maintenance engines (§9), dual-watermark fencing, resync protocol |
| `rsd-authz` | Principal model, scope grants, capability store, audit log |
| `rsd-ipc` | XPC (audit-token identity) + UDS (first-party) servers, postcard framing, subscription multiplexing |
| `rsd-mcp` | MCP server: search / snippets / subscribe / provenance / history tools, scope-gated |
| `rsd-daemon`, `rsd-cli` | Binaries |

---

## 5. Identity model

Two entities, because a file is not a path:

- **`FsObject`** — the content-bearing node. Identity: volume identity + file ID +
  *generation evidence* (birthtime where the volume provides it reliably; otherwise a
  first-seen fingerprint of size/mtime/content-hash). Owns: content hash, chunk
  manifest, extraction state, index membership, content-derived attributes.
- **`Entry`** — a directory entry: path → FsObject, **many-to-one**. Owns:
  path-derived attributes (name, parent, extension-derived hints), scope membership.

Rules that fall out:

- **Hard links**: one FsObject, several Entries. Unlinking one Entry never touches the
  FsObject or its siblings; renaming one link renames nothing else. Temporal history
  records Entry events and FsObject events separately.
- **Renames/moves** are Entry mutations; content planes are untouched.
- **Symlinks** are first-class Entries with link semantics — indexed as links, never
  silently resolved into their targets.
- **APFS clones** are distinct FsObjects sharing content identity through `by_hash`
  (extraction and embeddings shared via CAES; identity not shared).
- **Content identity** is separate from both: blake3 whole-file hash + FastCDC chunk
  hashes. A copied file reuses extraction, embeddings, and OCR verbatim (for
  content-derived facts only — §10.2); "find copies of this" is an index lookup.
- **TOCTOU discipline**: an event names a path, but identity is pinned by `fstat` on
  the *opened fd* at dispatch. Extraction results carry pinned identity + content
  hash; at commit, the committer revalidates (does an Entry still reference this
  FsObject? has a later-LSN event superseded this work?) and stale results are
  discarded or rescheduled. Races become retries, never wrong commits.
- **Case-folding and Unicode normalization** (NFD/NFC) are handled at the Entry layer
  with volume-aware canonicalization; collisions are explicit convergence-test cases.

---

## 6. Storage

### 6.1 The journal (`rsd-log`)

Append-only, segmented, per-record blake3-checksummed, fsync-batched. Records
*accepted indexing transitions*:

```
LogRecord { lsn, wall_time, source: FsEvents|Sentinel|Scan|AntiEntropy|Repair,
            object: FsObjectId, entry: Option<EntryId>,
            change: ObjectCreated|ContentChanged|EntryAdded|EntryRenamed{from,to}
                   |EntryRemoved|ObjectRemoved|AttrsChanged,
            actor: Option<ProcessIdentity>,          // sentinel tier
            content_hash, evidence: EventId|ScanGeneration }
```

- **Source-cursor fencing**: FSEvents event IDs advance durably only after the
  coalescer reaches a fence and all earlier FIFO work (including content commits)
  is journaled. Startup resumes from that cursor; on a new/corrupt cursor it captures
  a pre-bootstrap event ID and replays from there after reconciliation. ES sequence
  fencing remains a future sentinel-tier concern.
- The journal orders and describes transitions; it does **not** contain content
  (that's CAES) and does **not** claim complete observation of the filesystem
  (that's reconciliation's job).
- Retention: old segments compact into catalog history (§6.3). The journal's rebuild
  role is bounded by this; CAES carries content-rebuild duty independently.

### 6.2 CAES — the content-addressed extraction store (`rsd-caes`)

The durable home of everything derived from content:

- **Extraction records**: canonical text, typed attributes, chunk boundaries,
  extracted references, extractor identity + version, limits/status metadata.
  Keyed by `(content_hash, extractor_id+version, canonical_hints_hash,
  host_abi_version)` — see §10.2 for the content/instance fact split.
- **Embedding cache**: chunk vectors keyed additionally by embedding model revision.
- Checksummed, versioned, scrubbed like every plane.
- **Retention is a user policy and is the temporal-search knob** (§13): a record
  retained is an old document version you can still search and snippet; a record
  expired is an honest `ContentVersionUnavailable`.
- **Projection versions**: extractor/tokenizer/schema/model upgrades create a new
  projection version. Watermarks are `(lsn, projection_version)`. Upgrades reindex in
  the background behind a generation; mixed-version vector segments stay queryable
  via per-segment model tags until rebuilt. Replay after an upgrade is a *new
  projection*, not "deterministic re-derivation" — the distinction governs rollback
  and historical semantics.

### 6.3 Catalog plane (`rsd-catalog`, on redb)

- `objects`: `FsObjectId → { volume_id, file_id, generation_evidence, content_hash,
  chunk_manifest, size, mtime, indexed_gen, semantic_gen, attrs (typed kMDItem* map,
  postcard) }` with an internal per-record CRC above redb's page checksums.
- `entries`: `EntryId → { path, parent, FsObjectId, entry_attrs }`.
- `by_path`: prefix-ordered `path → EntryId` (scoped queries = range scans).
- `by_fileid`: `(volume_id, file_id) → FsObjectId`.
- `by_hash`: `content_hash → [FsObjectId]` (copies, clones, dedup).
- `attr_idx`: `(attr_id, order-preserving value, FsObjectId)` for the hot attribute
  set; **adaptive** — the query engine promotes attributes that recur in slow
  filters, building the index online.
- `history`: `(id, valid_from_lsn) → Diff` for both Entries and FsObjects — the
  bitemporal layer. Compaction: fine-grained history compacts to end-of-local-day
  states; `AS OF` inside a compacted interval resolves to the nearest retained
  boundary **and says so** (`resolution: day`). No silent approximation.
- `snapshots`: rsd LSN ↔ APFS snapshot name mapping, when snapshots exist.

Per-volume index directories (`~/Library/Application Support/rsd/index/<volume-uuid>/`):
an external drive carries its own state and resumes from its own cursors.

### 6.4 Lexical plane (`rsd-lexical`, on tantivy)

Fields: `doc_id` (u64 fast field, indexed for delete-by-term), `content` (positions
enabled — results deep-link to byte ranges), `name` (edge n-grams for as-you-type),
`symbols` (tree-sitter identifiers, camelCase/snake_case-aware tokenization),
`title`/`authors` (rank-boosted), `transcript` (OCR/ASR text, separately weighted).
Chunk boundaries stored as positional markers: a hit maps to *page 12, paragraph 3*.

Commit cadence: batched (~1000 docs or ~2s) with a RAM-resident hot segment so
freshness never waits on disk merges; merge policy tuned for 24/7 small commits.

### 6.5 Semantic plane (`rsd-vector`)

- **Chunking**: structure-aware first (extractors emit semantic boundaries — headings,
  functions, slides, mail parts), FastCDC fallback. Chunks keyed by chunk content
  hash → an edited paragraph re-embeds one chunk, not a document; copies re-embed
  nothing.
- **Embeddings target**: quantized small model (int8, ~50–150M params) via
  CoreML/ANE, candle+Metal fallback, generated asynchronously behind lexical
  commit. Shipping today is deliberately simpler: embedding runs synchronously
  inside commit into a redb exact-scan projection with one semantic watermark.
  The second timeline and two-delta-stream protocol remain targets (§7.3).
- **ANN**: per-segment HNSW with tombstones and background rebuilds, mirroring
  tantivy's segment lifecycle; PQ tier for the long tail, full-precision cache for
  the hot set; per-segment model tags for mixed-version periods.

### 6.6 Graph plane (`rsd-graph`)

**Facts, claims, and inferences are distinct**, every edge carries
`{source, evidence, observed_at, confidence}`:

```
# facts (observed)
SameContentAs(a ⇄ b)            # content-hash equality — says nothing about direction
LastWrittenBy(obj → process)    # sentinel observation — mutator, not necessarily author
References(obj → obj|url)       # extracted link/import/include
ExtractedFrom(member → archive)

# claims (recorded assertions, spoofable)
ClaimsDownloadedFrom(obj → url) # quarantine/WhereFroms xattrs

# inferences (derived, confidence-scored, evidence chain attached)
CopiedFrom(b → a)     # e.g. SameContentAs + sentinel-observed read(a)+write(b) by one process + temporal order
DerivedFrom(b → a)    # weaker conjunctions; always probabilistic
```

RQL exposes confidence (`DERIVED FROM x MIN CONFIDENCE 0.8`); MCP provenance
responses include the evidence chain so agents can cite *why* an edge exists. A
lineage graph that fabricates confident edges is worse than none — this plane never
presents inference as observation.

### 6.7 Reconciliation (part of `rsd-ingest`) — the convergence authority

- **Event-driven scoped scans**: `MustScanSubDirs`, queue overflow, sentinel gap
  detection, cursor discontinuities → readdir-diff of the affected scope against the
  catalog.
- **Anti-entropy audit**: continuous, lowest-priority, directory-mtime-pruned walk
  comparing catalog to filesystem — catches eventless divergence (bugs, offline
  mutations, volume moved between machines). Budgeted like all bulk work.
- **Full-volume reconciliation**: the honest last resort. Rare, **observable**
  (`rsdctl doctor` shows what triggered it), cancellable, rate-limited, and scoped to
  a volume. It exists; the engineering program's job is to make it never fire in
  practice (§6.8 test).

### 6.8 Failure matrix — the "no cascading loss" contract

Guarantee: **no plausible single failure escalates beyond scoped repair.** Authority:
this matrix. Test: crash/corruption-injection CI (spikes 1–2) exercises every row.

| Failure | Detection | Blast radius | Repair path |
|---|---|---|---|
| Crash mid-commit (any instruction) | Watermark divergence at startup | Docs in flight | Catalog replays its journal suffix; a lagged lexical/vector plane rebuilds current object identities from catalog + CAES, avoiding path-reuse ambiguity and filesystem reads |
| Journal segment corrupt | Record checksums on open/replay; only EOF-truncated frames are tail-repaired. Durable active/sealed scope manifests allow the corrupt bytes to be quarantined, preserve the LSN range with repair placeholders, then append current filesystem truth for affected paths | That manifest's paths; corrupt bytes retained for diagnosis | Automatic scoped repair ships for segments carrying a durable scope manifest; legacy active segments without one still fail closed |
| Lexical/vector segment corrupt | Segment checksums / scrubber | That segment's docs | Drop segment; rebuild from CAES records (segment manifests record membership); no filesystem reads needed within retention |
| Catalog page damaged, redb recovers to prior root | redb MVCC recovery | Since-prior-root delta | Replay journal delta |
| Catalog page damaged beyond redb recovery | Scrubber / read failure | Catalog plane | Rebuild skeleton from lexical stored fields + journal + CAES; close residual gap via anti-entropy scan of affected scopes |
| CAES record corrupt | Record checksums | One extraction record | Re-extract from current file if unchanged; else mark `ContentVersionUnavailable{Corrupt}` |
| Whole-index loss | — | Everything | Full bootstrap: the one case that legitimately crawls |

Notes: redb's damaged-page behavior is a **verification obligation** (spike 2), not an
assumption — transaction crash recovery is not arbitrary-page repair, and the matrix
row above depends only on the rebuild path, not on redb heroics. The scrubber
continuously walks all planes at idle priority.

---

## 7. Ingest: event → committed → notified

### 7.1 Sources and coalescing

1. FSEvents (file-level flags) and sentinel push into a *bounded* ring from their
   callback threads. Overflow → per-subtree rescan flag (same path as
   `MustScanSubDirs`): overload degrades granularity, never memory (P4).
2. **Coalescer**: per-path debounce (500ms quiet, 5s cap so an endlessly-appending
   file still lands), create+delete annihilation, rename-hint pairing via
   `by_fileid` (exact with sentinel, heuristic with FSEvents — mismatches caught by
   reconciliation), directory events expanded by readdir-diff. Emits
   `WorkItem { pinned identity, action: Extract | AttrsOnly | Remove | RescanDir,
   priority }`. `AttrsOnly` is load-bearing: renames/chmods never re-extract.

### 7.2 Scheduling and dispatch

3. **Scheduler**: priority heap with *budgets* — interactive scope (recently
   user-touched paths, foreground-app documents) preempts; bulk work consumes a
   CPU/IO token bucket that shrinks on battery/thermal pressure and pauses under
   user CPU contention. In-flight dedup. Saturation collapses file items into parent
   `RescanDir` markers.
4. **Dispatcher**: semaphore-capped; CAES consulted *first* (known
   content-hash+key → skip extraction); route by sniffed type to native / WASM /
   compat / media workers; pass fd; `fstat`-pin identity (§5).

### 7.3 Commit — shipping synchronous state machine and async target

```
Observed → Journaled → ExtractionDurable(CAES) → CatalogApplied
        → LexicalApplied → GraphApplied → (async) SemanticApplied
```

The diagram and two-stream rules below are the target. Shipping today has one
committer thread executing journal → catalog → lexical → vector → live hook
synchronously. Catalog, lexical, and vector keep independent watermarks; restart
streams the catalog suffix in bounded batches and rebuilds a lagged disposable
content plane from current catalog identities + CAES. There is no `SemanticDelta`,
`ALLOW STALE`, or query fence at `min(watermarks)` yet.

Ordering rules that do ship:

- Journal append is durable **before** any projection applies (journal-before-apply).
- CAES write is durable **before** dependent plane writes reference it.
- Source cursors advance only past fully-journaled events (§6.1).
- Old attribute state and the *previous* CAES record reference are captured into the
  commit delta **before** overwrite/tombstone — leave-events in live views evaluate
  against real old state (§9).
- **Two delta streams (target)**: `DocDelta { lsn, (id, old, new, caes_refs) }` at
  catalog+lexical commit; `SemanticDelta { lsn, semantic_gen, chunks }` when the
  vector batch durably commits. A semantic alert cannot fire before its vector
  exists, so it fences on the semantic watermark — stated, not hidden.
- **Read-fence target**: queries read behind `min(plane watermarks)` by default; callers may opt into
  `ALLOW STALE(plane)` for freshness-tolerant reads.

### 7.4 Idempotency

Every projection apply is keyed by `(lsn, id, plane, projection_version)` and is
safely re-runnable: delete-before-add within a keyed apply, replays after uncertain
commits converge to the same state. This is what makes cursor re-delivery (§6.1) and
crash replay (§6.8) safe. Randomized kill injection covers journal/catalog and
content-plane convergence; byte corruption tests currently cover journal segment
detection and manifest-scoped automatic repair; legacy segments without a scope
manifest still fail closed rather than guessing.

### 7.5 Freshness targets

Save → lexically searchable: **p50 < 200ms, p99 < 2s**. Commit → exact-class live
notification: **p99 < 10ms**. Semantic coverage: trickle — seconds on AC typical,
minutes budgeted on battery, always visible via `semantic_gen`. (Design targets;
hardened by §15.)

Bootstrap = the same pipeline seeded with `RescanDir(scope)` at minimum priority.

---

## 8. Query system

### 8.1 RQL — versioned grammar, honest compatibility

RQL implements a **versioned, documented grammar** covering the practically-used
Spotlight predicate surface (`c`/`d`/`w` modifiers, `$time.*`, `InRange`, wildcards,
type coercions) plus rsd's extensions. Compatibility posture:

- A published compatibility corpus is differential-tested against real `mdfind` — as
  a *discrepancy-finder*; no equivalence proof over an opaque implementation is
  claimed.
- Known divergences are documented; unsupported constructs fail with
  `UnsupportedPredicate`, never silent misinterpretation.
- `rsdfind` is flag-compatible with `mdfind`.

Extensions (each mapping to a specific plane and guarantee):

```
# semantic and hybrid (vector plane; one-shot = top-k semantics)
semantic("the contract where we agreed to net-60 payment terms")
kMDItemTextContent == "*invoice*"cd && semantic("payment terms dispute") weight 0.7

# temporal (catalog history + CAES; guarantees per §13)
AS OF 2026-07-08T09:00 : kMDItemFSSize > 100mb && InTree("~/Downloads")
CHANGED SINCE yesterday WHERE kMDItemContentType == "public.swift-source"
DIFF ~/Projects/foo BETWEEN 2026-06-01 AND now

# provenance (graph plane; confidence exposed)
DERIVED FROM path("~/Papers/attention.pdf") DEPTH 2 MIN CONFIDENCE 0.8
WRITTEN BY app("com.apple.Preview")          # LastWrittenBy facts
origin(doc) REACHES url("github.com/*")      # claims + facts, labeled

# structure and state
symbols:"parse_predicate" lang:rust
kRSDIndexState == "encrypted"                # unindexable-by-policy is queryable
SNIPPETS 3                                   # matched chunks with byte ranges
```

Typed AST; planner with real statistics (attr_idx cardinality estimates); `EXPLAIN`;
selectivity chooses the driving plane, the others filter.

### 8.2 Hybrid retrieval and ranking

Default free-text path: BM25 top-k ∪ ANN top-k → reciprocal rank fusion → optional
cross-encoder rerank of top ~50 in `rsd-ml` (explicit "deep" queries or ambiguous
scores). Final ranking blends relevance, **frecency** (local open/activation signals
reported by consenting clients), and scope affinity. A small learned ranker
(logistic over ~20 features) trains on-device from click-through; weights inspectable
via `rsdctl ranking explain`. Docs with stale `semantic_gen` are scored
lexical-only — compensated, not hidden.

### 8.3 Natural-language frontend

`rsdfind --ask "that tax pdf I downloaded around march"` → local model compiles NL to
RQL (semantic clause + date range + type hints), **shows the compiled query**, then
executes. MCP `answer` goes one step further: retrieve → rerank → return grounded
chunks with byte-range citations. rsd never generates prose; it is the retrieval
substrate.

---

## 9. Live views — published maintenance classes

A standing query is a view over the delta streams, maintained per its **class**. The
class contract is public API, not internal detail:

| View class | Maintenance | Delivered semantics |
|---|---|---|
| Attribute predicates; boolean/unranked lexical membership; simple aggregates (COUNT/SUM/GROUP BY) | **Exact point-incremental** via trigger index | Exact membership deltas, p99 < 10ms post-commit |
| Semantic alerts (`ALERT WHEN similarity(q, chunk) > θ`) | Threshold match on committed vectors | Threshold semantics on the **semantic watermark** (seconds-scale on AC) |
| Ranked / hybrid top-k views | **Bounded top-k repair**: maintain top-k + margin buffer; repair from index on exhaustion; periodic re-query fence | Eventually-exact top-k, bounded staleness, fence LSNs exposed |
| Graph-traversal views | Dependency-aware frontier invalidation; re-query on high-fanout edge changes | Exact after invalidation window |
| Clock-relative predicates (`$time.now(...)`) | Scheduled re-evaluation at predicate-derived boundaries | Exact at ticks |
| Everything else; high-fanout events (grant changes, projection upgrades) | `Resync{fence}` | Client re-fetches |

Mechanics:

- **Trigger index**: at registration, extract the view's attribute/term footprint →
  attribute→views and term→views maps (Rete-flavored, purpose-built). Only views
  whose footprint intersects a delta are evaluated.
- **Old-state evidence**: deltas carry the previous CAES record reference (retained at
  least until all subscribers consume the delta), so leave-events evaluate against
  real old text — never against an already-deleted lexical doc.
- **Single-doc matcher**: text-membership clauses evaluate against a one-doc RAM index
  sharing the production tokenizer. Claim, precisely: **bit-identical tokenization
  and boolean membership** with the on-disk index — property-tested. Scoring parity
  is explicitly *not* claimed (BM25 depends on corpus statistics); scoring lives in
  the ranked-view class.
- **Semantic alerts are threshold semantics by design**, not degraded top-k: a
  standing alert asks "is this new thing similar enough?", which is classification —
  a top-k standing query would let a new document retroactively displace an old
  alert, which is nonsense for notifications. One-shot `semantic()` = top-k; standing
  `ALERT WHEN` = threshold. Two operators, two contracts.
- **Client protocol**: subscribe → initial result set fenced to
  `(lsn, semantic_gen)` → batched diffs `{fence, enters, leaves, updates}`. Slow
  client → bounded buffer → `Resync`. The daemon never queues unboundedly for anyone.
- Frecency/permission-driven reordering flows through the ranked class
  (repair/fence), not through file deltas. Grant changes re-fence affected
  subscriptions (§11).

---

## 10. Extraction fabric

### 10.1 The extraction contract (applies to native, WASM, compat, media)

Every extraction runs under explicit budgets: input bytes, decompressed bytes,
compression ratio, recursion depth, archive member count, output text/chunk/reference
volume, wall time, scratch space. Host-side container handling (decompression,
archive walking) is budgeted identically — WASM fuel does not cover host work, so the
host meters itself. Results are one of: complete, **partial** (first-class, with
what/why), or a typed status: `EncryptedContent`, `PasswordRequired`,
`CloudPlaceholder`, `ResourceBudgetExceeded`, `Unsupported`, `Corrupt`,
`QuarantinedAfterCrashes`. **Status is a queryable attribute** (`kRSDIndexState`) —
the index of what couldn't be indexed is itself search surface.

### 10.2 Cache discipline: content facts vs. instance facts

Extraction output is split:

- **Content-derived facts** (text, chunks, references, content attrs): cached in CAES
  under `(content_hash, extractor_id+version, canonical_hints_hash,
  host_abi_version)`; shared across copies/clones/links.
- **Instance-derived facts** (path/name/xattr/archive-member-derived attributes):
  computed per Entry, never cached across identities.

Chunking parameters, OCR language settings, and embedding model revisions join their
respective cache keys.

### 10.3 Extractors

- **Native (`rsd-extract`)**: plain text + source (encoding detection; tree-sitter
  symbols for the top ~20 languages), PDF (pdfium — the one C++ dependency worth
  taking, contained by the fd-only sandbox; pages without text layers → OCR),
  HTML/XML/Markdown (link extraction feeds the graph), archives (indexed *through*,
  members as FsObjects with `ExtractedFrom` — T1+), mail, EXIF/media metadata.
- **WASM plugins (`rsd-wasm-abi`)**: WIT interface
  `extract(stream, hints) → { text_chunks[], attrs[], references[], boundaries[],
  status }`; fuel-metered, memory-capped, no ambient capabilities; output cached per
  §10.2. SDK + template repo: an extractor for your niche format should be a weekend
  project that cannot harm the host.
- **mdimporter compat (`rsd-compat`)**: per-bundle processes, schema.xml-driven
  typing, crash-quota blacklisting. Bridge tier; never load-bearing for core formats.
- **Media (`rsd-media`)**: Vision OCR (screenshots are the highest-value
  undersearched corpus on most Macs); whisper.cpp transcription with timestamps —
  results deep-link to a moment. Power-gated, opt-in per scope for A/V.

---

## 11. Security and authorization (T0 architecture)

Unix peer credentials establish *user* identity only — they cannot see a caller's App
Sandbox extensions, TCC grants, or code identity. rsd therefore never infers
visibility; it grants it explicitly.

- **Principals and trust tiers**:
  - *First-party trusted clients* (`rsdfind`, `rsdctl`, future GUI): verified by
    code-signing identity over **XPC audit tokens**; receive the user's full index.
  - *Third-party clients*: **nothing by default.** Access = explicit, user-approved,
    revocable **scope capabilities** ("app X may query ~/Documents/Contracts"),
    stored in `rsd-authz`, listable and revocable via `rsdctl grants`.
  - *MCP surface*: its own principal with user-configured scopes; off by default for
    non-first-party agents; every grant visible and auditable.
- **Transport**: XPC is the identity-bearing surface (T0). UDS remains for the
  same-product CLI during development.
- **Shipping status (2026-07 correctness pass)**: UDS scope evaluation is
  component-boundary-safe and deny-by-default, including unknown principals and
  explicit empty grants. Its `Hello` principal remains caller-asserted, so the
  daemon configures no UDS grants; verified first-party identity, persistent grant
  management, and dynamic revocation remain T0 targets. Lexical documents carry
  non-stored component-ancestor scope terms, catalog enumeration is scope-first,
  and the exact-scan semantic plane filters oids before ranking. Lexical and catalog
  RQL counts use uncapped counting paths over the authorized candidate set; ranked
  semantic predicates reject exact-count requests because membership has no
  threshold. Aggregates and statistical timing tests remain unfinished. The
  token-authenticated loopback UI surface carries one explicit startup scope
  (currently unrestricted for the first-party app); search, hybrid fusion, and
  live subscriptions enforce it through the same query/live engines. Status counts
  are scope-derived, and restricted scopes cannot read global metrics.
- **Enforcement-point target**: scope filters constrain **candidate generation**, not final
  results — counts, aggregates, group-bys, rank positions, snippets, and live-view
  deltas are computed over the authorized subset only. Provenance traversal clips at
  scope boundaries: edges into unauthorized documents are invisible, not
  redacted-but-countable.
- **Dynamics**: grants are path-rooted; directory-wide permission changes are
  grant-level events; standing subscriptions affected by a grant change are re-fenced
  with `Resync`.
- **Audit**: privileged queries and grant changes are locally logged.
- **Deliverables**: a threat-model document (same-user hostile process; over-curious
  sandboxed app; prompt-injected agent on MCP) and a **leak-test suite** (aggregates/
  timing/deltas over unauthorized docs) are T0 gates alongside crash injection.

Index-at-rest: T0 relies on FileVault + `0700` index directory. Index-level sealing
while the session is locked is a T2 item with real key management, not a checkbox.

---

## 12. Surfaces

- **IPC**: XPC (identity-bearing) + UDS (first-party dev path), length-prefixed
  postcard frames, stream-multiplexed subscriptions.
- **MCP server (`rsd-mcp`)**: tools — `search` (RQL or NL), `snippets` (grounded
  chunks + byte ranges), `subscribe` (standing views per §9 classes), `provenance`
  (edges with evidence chains), `history` (AS OF / DIFF with availability labels).
  Scope-gated per §11. Local agents get a private, cited, real-time view of exactly
  the corpus the user granted.
- **CLI**: `rsdfind` (mdfind-flag-compatible + RQL + `--ask`), `rsdctl` (scopes,
  grants, plugins, retention, `doctor`, `top`, `ranking explain`,
  `rebuild --plane`).
- **Library**: `librsd` (Rust + C ABI).

---

## 13. Temporal guarantees — four capabilities, four prices

| Capability | Backing authority | Guarantee |
|---|---|---|
| Metadata history: `AS OF`, `CHANGED SINCE`, `DIFF` over attrs/paths/existence | Catalog `history` | **Guaranteed** within history retention (default: 90 days fine-grained, 2 years daily; compaction resolution surfaced in responses). T2 |
| Historical extracted content (old snippets, old doc versions) | CAES retention | Guaranteed **within CAES retention**; else explicit `ContentVersionUnavailable { RetentionExpired \| NeverCaptured \| Corrupt }` |
| Historical full-text/semantic *search* (query old corpus states; rank deleted docs by old content) | Catalog history (candidates) + CAES (evaluation) | **Bounded**: documented candidate-generation + replay, cost ∝ candidate set. Not a version-aware inverted index in T2 |
| Historical raw bytes | APFS/Time Machine snapshots | **Opportunistic only** — availability always exposed, never promised |

What "falls out of the log" is ordering and change detection. Historical *content* is
a storage policy the user buys with CAES retention — the API never blurs the two.
Retention is configurable to zero for the paranoid; every temporal answer carries its
resolution and availability labels.

---

## 14. Scope decisions (ratified)

- **Default roots & consent**: nothing indexed without onboarding consent. Default
  offer: `~` minus exclusions. TCC-protected folders and FDA requested explicitly,
  per-scope, with explanation; declining excludes cleanly.
- **Cloud placeholders (File Provider)**: metadata-only by default; never trigger
  downloads implicitly; `kRSDIndexState == "cloud-placeholder"`; per-scope opt-in
  download-on-index.
- **Network volumes**: out of T0 (no FSEvents fidelity, weak identity). Future:
  polling + anti-entropy-only mode.
- **Packages/bundles**: indexed as trees, presented as single results by default,
  expandable.
- **Archive members**: first-class FsObjects in T1+; identity = (archive FsObject,
  member path, member content hash); archive edit = member-set diff.
- **Default exclusions (deny by default, configurable)**: keychains, browser profile
  databases/cookies, `~/.ssh` and credential material, ungran­ted app containers,
  `.git/objects` and dependency trees (`node_modules`, `target`, `.venv`) — the last
  group excluded from *content extraction* but present in the catalog so scoped
  queries still see the files exist.
- **Migrations**: owned by `rsd-daemon` at startup; versioned manifest per plane;
  projection-version machinery (§6.2) is the upgrade path; failed migration → keep
  serving the old generation read-only while rebuilding. Never brick the index.

---

## 15. Performance: design targets → benchmark matrix

The numbers below are **design targets** — the sizes the architecture is engineered
against. They harden into acceptance criteria when the benchmark matrix (spike 7)
lands: named corpora (monorepo source tree; office/PDF corpus; photo-heavy library;
media library; adversarial archive set), named hardware tiers, defined cache states,
concurrency, query mixes, and background-load conditions. Index-size reporting is
**absolute bytes per document/chunk** alongside corpus ratios — percentage-of-corpus
alone misleads on small-file corpora, and vector size scales with chunk count, not
source bytes.

| Metric | Target |
|---|---|
| Cold bootstrap, 1M-file mixed corpus (lexical + catalog) | < 25 min on current-gen M-series laptop, machine fully usable throughout |
| Semantic coverage of same corpus | < 8 h trickle on AC, no user-noticeable impact |
| Save → lexically searchable | p50 < 200ms, p99 < 2s |
| Commit → exact-class live notification | p99 < 10ms |
| Semantic delta latency (AC power) | p50 < 5s |
| Lexical query | p50 < 1ms, p99 < 10ms |
| Hybrid query (fusion, no rerank) | p50 < 15ms, p99 < 60ms |
| Daemon idle RSS | < 150 MB (ML sidecar fully evicted at idle) |
| Idle battery cost | < 0.5%/day, measured by powermetrics protocol over a defined 24h idle scenario |
| Crash/corruption-injection suite | 100% convergence; zero unscoped-repair escalations |
| Authorization leak suite | zero information flow to unauthorized principals (counts, aggregates, timing classes, deltas) |

---

## 16. Risk register and prototype sequence

Ordered by how much of the design dies if the bet fails; each spike has a
kill/adopt criterion.

1. **Convergence kernel (weeks 1–3).** Coalescer + catalog + reconciliation vs. a
   stress harness (monorepo checkouts, `npm install`, mass renames, hard-link
   matrices, unlink-while-open, NFC/NFD collisions, `kill -9` storms); assert
   convergence by full-tree comparison. *No kill criterion — must work; the harness
   is a permanent CI gate.*
2. **Journal + CAES + multi-plane recovery (weeks 2–4).** Prove the §7.3 state
   machine and §6.8 matrix with randomized kill/corruption injection. Includes the
   **redb damaged-page verification obligation**: confirm recovery behavior matches
   the matrix row or route it entirely through plane rebuild.
3. **Authorization architecture (weeks 3–5).** XPC audit-token identity, scope
   capabilities, candidate-generation enforcement, leak-test suite. *T0 gate: no
   third-party access ships before this passes.*
4. **Single-doc matcher fidelity (weeks 3–5).** Property-test bit-identical
   tokenization + boolean membership against the on-disk index across the modifier
   surface. Scoring parity out of scope by design.
5. **ANE embedding throughput (weeks 4–6).** Target ≥ 2k chunks/sec batched, int8,
   sidecar fully evictable. *Fallback: Metal/candle at lower throughput — trickle
   window widens, design unchanged.*
6. **EndpointSecurity tier (weeks 5–8).** Dev-signed sysext: fidelity, gap detection,
   attribution quality, CPU cost. **Product risk, tracked separately:** the
   restricted entitlement may never be granted for broad distribution — which is why
   nothing correctness-critical sits on sentinel. *Fallback: FSEvents-only;
   provenance degrades to claims-tier.*
7. **Benchmark matrix (from week 6, permanent).** Named corpora/hardware; converts
   §15 targets into acceptance criteria; NDCG-gated ranking changes (labeled
   local-corpus query set) — retrieval quality moves on evidence, not vibes.
8. **WASM extractor ABI (weeks 6–9).** Port a real extractor (EPUB) to WIT; adopt if
   within 2× native throughput for text-heavy formats.
9. **Ranked-view repair engine (weeks 7–10).** Bounded top-k maintenance under churn:
   measure repair frequency/cost on realistic delta streams; tune margin buffer.
10. **mdimporter compat (last, weeks 10+).** CFPlugIn loading, runloop expectations,
    x86_64-only bundles (Rosetta worker slice). Isolated and last: nothing depends
    on it.
11. **Segment churn (continuous).** Merge-policy tuning for tantivy + HNSW under 24/7
    small commits; hot-RAM segment keeps freshness decoupled from merge cadence.

Spikes 1–4 are the foundation bets. 5, 6, 8 are the leapfrog bets, each with a
graceful fallback. Nothing in the ambitious tier sits underneath the foundation:
boldness never taxes correctness.

---

## 17. Roadmap tiers

- **T0 — Trustworthy kernel.** Explicit user-approved scopes; events as acceleration
  + reconciliation as correctness; crash-consistent catalog + lexical projections
  with bounded-staleness watermarks; versioned RQL grammar (documented Spotlight
  subset + extensions); exact-class live views with published classes and resync;
  authorization architecture with first-party/capability tiers; `rsdfind`/`rsdctl`;
  full crawl rare, observable, cancellable. Already a materially better foundation
  than mds: observable, repairable, no stampedes, honest live queries.
- **T1 — Semantic + total recall.** Vector plane + hybrid ranking, semantic alerts,
  OCR, NL frontend, MCP server, WASM plugin SDK, archive members. The leapfrog
  release.
- **T2 — Time & lineage.** Bitemporal queries with availability labels, CAES-backed
  historical content search, APFS snapshot integration, sentinel provenance
  (facts/claims/inferences), `DIFF`/`DERIVED FROM`, transcription, index-at-rest
  sealing.
- **T3 — Frontier.** Learned ranker maturity; federated multi-device search (E2E
  encrypted index sync between the user's own machines); agent-native workflows
  (standing views driving automations); speculative pre-extraction of predicted-hot
  files.

---

*The one-line test for every future feature: does it make the machine's memory of
itself more complete, more current, or more askable — with a guarantee that names its
authority, its failure mode, and its test? If not, it doesn't belong in rsd.*

---

## 18. Observability — the daemon's account of itself (shipped: metric plane)

Unobservable-by-neglect is a bug, the corollary of §0's unindexed-by-neglect.
Three planes, distinct authorities (mirroring §1): **events** (the narrative —
`tracing`), **metrics** (quantities over time — `rsd-metrics`), **spans** (one
item's causal timeline). This is a *projection of the §7.3 commit state
machine*, not a parallel set of print statements — it hangs off transitions
that already exist.

### 18.1 What shipped (v0.3+)

- **`rsd-metrics`**: counters, gauges, fixed-bucket histograms (log-spaced
  0.1ms→60s, constant memory, percentiles on read). **Cardinality-safe by
  construction**: every metric is a named field or an enum-indexed array —
  there is no dynamic `String→metric` map, so a per-path/per-query label (the
  O(files) growth P4 forbids) *cannot be expressed*. Paths and queries live in
  the event plane only.
- **Inline stage timing on the applier thread.** Today a live item's whole
  journey (resolve → commit(Upsert) → sealed-worker extract → commit(SetContent))
  is synchronous on one thread, so timing needs no cross-thread keyed table.
  `index_latency_ms` is recorded for **Probe (single live-edit) items only** —
  bulk bootstrap scans are coarse-grained (§18.6), so directory-sized samples
  never pollute the freshness histogram. This makes the §7.5 freshness target
  *measured, not asserted*.
- **Real-today metrics wired**: `files_indexed`, `caes_hits/misses` (dedup
  effectiveness), `commits`, `extract_ms`, `commit_ms`, `index_latency_ms`,
  `full_rescans` (the convergence canary), `quarantines`,
  `extraction_failures{status}`, `journal_replays`, `catalog_entries`,
  `bootstrap_dirs/done`.
- **`/api/metrics`** JSON snapshot + the **RSD.app Activity HUD** (1Hz, pure
  reader): files indexed, live latency p50/p99, commit latency, dedup rate,
  bootstrap progress, and a green/orange convergence light off `full_rescans`.
- **Loopback-secret gate (§18.5.2, done first as its own fix).** The HTTP
  surface — `/api/search` included — was reachable by any web page via
  `ACAO:*` on `127.0.0.1`. Now every request requires a token from a 0600 file
  the native app reads and a web page cannot; `ACAO:*` removed. Token generation
  fails closed if OS entropy is unavailable, and verification compares the fixed
  32-byte token in constant time before route dispatch (including SSE routes).
- **Flood test**: cardinality stays flat and percentiles stay finite under 1M
  samples.

### 18.2 Deferred, and honestly why

Metrics that measure the *aspirational async* pipeline read trivial until it
lands, so they are intentionally **not** shown as if meaningful yet:
`semantic_delta` / `semantic_gen_lag` / `embed_queue_depth` (embedding is
currently synchronous *inside* commit — which is itself why `commit_ms` p50 is
~7ms, a cost the HUD now makes visible); per-**sidecar** `rss_bytes` (the ML
embedder is in-process, not yet a separate process). `worker_crashes` needs
plumbing through the `ContentSource` trait. Structured JSONL event sink with
sampling, and `rsdctl top`/`doctor`, are the next slices. Each lights up when
its underlying subsystem (async embedder, processization) lands — the metric
plane is the seam, already in place.
