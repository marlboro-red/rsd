# rsd — End-to-End Performance & Usability Audit

**Date:** 2026-07-16 · **Scope:** all 17 workspace crates, the RSD.app Swift palette,
CLI/MCP surfaces, scripts, CI, and docs · **Method:** full read of every source file
(~11.6k lines) by four parallel subsystem reviewers (storage/indexing, search/ranking,
daemon/worker/IPC, user-facing surfaces) plus a cross-cutting pass; every headline
finding was verified against the code, and findings that could not be verified are
marked *(uncertain)*. **No code was changed** — this document is the only artifact.

Findings are numbered `A-n` (architecture/cross-cutting), `S-n` (storage & indexing),
`Q-n` (query/search/ranking), `D-n` (daemon/worker/IPC), `U-n` (user-facing).
Duplicated observations across reviewers were merged; the count below is post-dedup.

---

## Executive summary

The architecture (journal + projection planes, sealed workers, CAES) is sound, and
the lexical path genuinely hits its latency targets in the ignored perf smoke. But
the implementation currently has **one structural performance problem that dominates
everything else** — the entire pipeline (extraction, hashing, embedding, live-query
evaluation, commits) runs serially on a single applier thread — and **a handful of
silent-wrong-results usability bugs** that undermine the project's own "honest error,
never silent misinterpretation" charter.

The ten issues that matter most, in rough order of impact:

1. **Everything runs on one thread** — content extraction, 32 MB blake3 hashing,
   worker round trips (up to 10 s each), MiniLM inference, and live-view evaluation
   all execute inline in the applier's commit loop. One slow PDF stalls all indexing;
   backpressure then overflows the FSEvents channel, which triggers a *full-root
   rescan*, which stalls the applier further — a self-amplifying feedback loop.
   (D-1, D-2, D-3, S-7)
2. **The worker "pool" provides zero parallelism** — `Pool::request(&mut self)` is a
   synchronous send→recv round trip; pool size 2 just alternates which idle process
   serves the next serial request. Bootstrap extracts one file at a time on one core,
   versus the DESIGN.md §3 target of N ≈ cores/2. (D-2, verified)
3. **Semantic search is a brute-force full-table scan** — every query deserializes
   every document's chunk vectors from redb before computing dot products (~1.5 GB of
   decode/alloc per query at the module's own 1M-chunk design scale). (Q-1)
4. **Embedding runs synchronously under the query mutex inside a write txn** — with
   the real MiniLM embedder wired in, committing a batch of large documents holds the
   `Mutex<VectorPlane>` for seconds-to-minutes of CPU BERT inference, during which
   every palette search hangs (no spinner, 60 s default URLSession timeout). (Q-3, U-5)
5. **`!=` silently behaves as `==` on text and symbol predicates** — the comparison
   operator is pattern-matched away (`Expr::Cmp { attr: TextContent, .. }`); a user
   excluding matches gets exactly the matches. Verified in both `eval` and `eval_live`.
   (Q-5)
6. **The CLI/MCP surfaces cannot run while the daemon is running, and the README says
   otherwise** — both open the same redb files with a write handle; the second opener
   errors (rsdfind) or panics (`rsd-mcp`, seen by an agent as a broken pipe). Worse, a
   mistyped `--state` path silently *creates a fresh empty index* and returns zero
   hits with exit 0. (U-2, U-3)
7. **One unreadable directory aborts an entire rescan** — a single `EACCES` discards
   all already-resolved changes for the subtree with only a daemon-log warning;
   thousands of readable files silently never get indexed. (S-2)
8. **Journal grows without bound and is fully re-read+checksummed on every daemon
   start** — no compaction/segment-deletion exists (DESIGN.md §6.1 promises it), every
   rescan journals an `Upsert` per live file even when nothing changed, and
   `Journal::open` re-hashes every byte of every segment (sealed manifests are
   ignored). Startup cost grows with the life of the install. (S-3, S-4, S-10)
9. **The first-run experience looks broken** — a new user gets a palette answering
   from a partial index with "Nothing for X" for the entire home-dir bootstrap
   (potentially hours), with no indexing-progress indication anywhere, plus a
   per-keystroke flicker of a false "rsd isn't running" banner caused by cancelled
   in-flight requests being treated as daemon death. (U-1, U-7)
10. **None of the DESIGN.md §15 performance targets are gated** — both perf tests are
    `#[ignore]`d and CI runs plain `cargo test --workspace`, so sub-millisecond
    search and commit-throughput claims can regress silently. (A-1)

**Post-dedup totals:** 15 high, 33 medium, 25 low across 73 findings.

---

## A. Cross-cutting / architecture

| # | Sev | Cat | Finding |
|---|-----|-----|---------|
| A-1 | high | perf | Perf targets ungated: `crates/rsd-daemon/tests/perf.rs:44,60` are `#[ignore]`d; `scripts/ci.sh` runs plain `cargo test --workspace`, so no DESIGN.md §15 number (p50 < 1 ms lexical, commit throughput, etc.) is enforced. A regression in any hot path lands green. |
| A-2 | high | perf | Single-applier-thread pipeline coupling (umbrella for D-1/D-2/D-3/S-7): metadata commits, content extraction, embedding, and live evaluation share one thread and back up into the FSEvents channel; overflow recovery is a full-root rescan executed on that same thread. The design's freshness targets (§7.5: save→searchable p50 < 200 ms) are unreachable whenever any nontrivial extraction is in flight. |
| A-3 | med | usab | The design/docs promise several things the code doesn't do yet: journal compaction (§6.1), async semantic timeline (§7.3), lexical commit cadence "~1000 docs or ~2 s" (§6.4), `rsdfind` mdfind-flag compatibility and `--ask` (§8.3), MCP `answer` tool. DESIGN.md labels these targets, but README/DESIGN cross-references will send users hunting for flags and behaviors that don't exist. `DIVERGENCES.md` documents grammar gaps but not the phrase-search gap (Q-14). |
| A-4 | low | usab | `.github/workflows/ci.yml` builds the Swift app only in debug in the gate job; release-job app breakage (build-app.sh path) is only caught on push to main. |

---

## S. Storage & indexing pipeline
*(rsd-catalog, rsd-log, rsd-caes, rsd-ingest, rsd-fsevents)*

### High

- **S-1 · perf · `crates/rsd-catalog/src/lib.rs:559-572`** — `children()` materializes
  the **entire subtree** to list immediate children: it calls `subtree_paths(dir)`
  (allocating a `String` per descendant) then filters to depth 1. Verified. Consumed
  once per directory by `resolve_dir` (`rsd-ingest/src/scan.rs:164`), so a recursive
  rescan of an N-entry tree does O(N × depth) work, and rescanning one shallow
  directory near the root of a 1M-file tree allocates ~1M strings to diff a handful of
  children. The range scan could stop at the first grandchild per child.
- **S-2 · usab · `crates/rsd-ingest/src/scan.rs:128,143`** — any non-`NotFound` I/O
  error (one `EACCES` directory, one racing lstat failure) aborts the whole rescan and
  drops all already-resolved changes; the daemon logs a warn (`rsd-daemon/src/lib.rs:230`)
  and moves on. One permission-denied folder ⇒ the entire subtree is silently never
  indexed, with no per-path skip, retry, or user-visible report.
- **S-3 · perf · `crates/rsd-log/src/lib.rs:216-256`** — `Journal::open` re-reads and
  blake3-checksums **every byte of every segment** on every daemon start. Sealed
  segments carry `.seal` manifests with `first_lsn`/`last_lsn` (lib.rs:69-75) that are
  ignored; only the last unsealed segment needs scanning. Multi-GB journal ⇒
  seconds-to-minutes of read+hash before watching begins.
- **S-4 · perf · `crates/rsd-log` (whole crate) + `scan.rs:150-159`** — unbounded
  journal growth: no segment deletion/compaction API exists (DESIGN.md §6.1 promises
  "old segments compact into catalog history"), while `resolve_dir` emits an `Upsert`
  for every live child unconditionally, even when the catalog already holds identical
  stats. Every rescan of an unchanged 10k-entry directory journals 10k records; disk
  grows without bound and S-3 makes startup pay for it.
- **S-5 · perf · `crates/rsd-catalog/src/lib.rs:635-651` + `rsd-daemon/src/lib.rs:254-267`** —
  `sweep_orphans` is a full objects-table scan (deserializing every record) inside a
  write transaction, invoked **every 200 ms idle tick**. On a multi-million-object
  catalog this burns CPU/page cache continuously while idle — directly against the
  §15 idle-battery target — and an event arriving mid-sweep waits out the scan on the
  single applier thread. Needs an orphan index or in-memory list.
- **S-6 · perf · `crates/rsd-caes/src/lib.rs:288-291`** — `Indexer::index_file` does
  `std::fs::read(path)` — whole file into memory, no size cap — before hashing.
  A 20 GB video allocates 20 GB; a few large files can OOM the daemon. blake3 hashes
  streams; extraction should be gated by size/type budget before any read. *(Note:
  the daemon's own dispatch path caps at 32 MB — see D-9 — so this bites library/CLI
  callers of `rsd-caes` directly, and the cap discrepancy itself is confusing.)*
- **S-7 · perf · `crates/rsd-fsevents/src/lib.rs:220-225` + `rsd-daemon/src/lib.rs:236-248`** —
  on channel overflow (> 8192 queued events, e.g. one big `git checkout`) the callback
  drops the event and sets only a boolean — the path is discarded — so the only sound
  recovery is a whole-root recursive rescan, which the daemon performs. The rescan
  occupies the applier while more events queue, so overflow can recur and re-trigger
  itself. Retaining the deepest common ancestor of shed events would scope recovery.
- **S-8 · perf · `crates/rsd-ingest/src/scan.rs:205-246` + `rsd-log/src/lib.rs:311-329`** —
  a recursive rescan resolves the entire tree into one in-memory `Vec<Change>` before
  anything is applied; the committer then serializes the whole batch into a single
  scratch buffer. For a 1M-file tree that's roughly two full copies of every path+stat
  in RAM, one giant redb transaction, and zero durability until the end — a crash at
  99 % restarts from zero. (Bootstrap proper chunks at 1024 — `rsd-daemon/src/lib.rs:447` —
  but `resolve_and_commit` for overflow/RescanRecursive work items does not; see D-5.)

### Medium

- **S-9 · perf · `crates/rsd-log/src/lib.rs:380-404`** — `replay(from)` scans and
  checksums every segment including those entirely below `from`; segments are named by
  `first_lsn`, so replay-from-near-tip (the common recovery case) could seek to the
  containing segment instead of re-reading the whole journal.
- **S-10 · perf · `crates/rsd-catalog/src/lib.rs:322-354`** — `apply_stat_in`
  unconditionally rewrites the object record and re-inserts the `by_path` row even
  when nothing changed. Combined with S-4's unconditional Upserts, a no-op rescan
  rewrites every touched B-tree page. An identity early-out would make verification
  rescans read-only.
- **S-11 · perf · `crates/rsd-caes/src/lib.rs:169-195`** — `Store::put`/`evict` open
  and commit their own redb transaction per record; no batch API. Bulk content
  indexing pays a full B-tree commit per small file while the catalog side batches.
- **S-12 · perf/usab · `crates/rsd-ingest/src/scan.rs:116-176,272-274`** — bootstrap
  scanning is single-threaded serial readdir+lstat with **no progress reporting** of
  any kind (`ScanStats` only on completion). Large home dir ⇒ full serial-I/O wall
  time with nothing visible until the end. Feeds U-1.
- **S-13 · usab · `crates/rsd-ingest/src/scan.rs:68-89`** — the exclusion list is
  hardcoded and matches any path component by bare name, silently: any user folder
  named `Library`, `Caches`, or `.build` anywhere is invisible to search with no
  diagnostic and no opt-out short of recompiling.
- **S-14 · perf · `crates/rsd-catalog/src/lib.rs:465-477`** — `remove_subtree`
  collects victim paths under a read txn then deletes in a separate write txn:
  O(subtree) strings in memory, and paths created between the txns linger stale.

### Low

- **S-15 · perf · `crates/rsd-ingest/src/coalesce.rs:118-136,160-189`** — the pump
  wakes every 20 ms even when idle and `due()` linearly scans the whole pending map
  (up to 65,536 entries ⇒ ~3.3M entry-visits/sec during a storm); a min-deadline heap
  fixes both. *(Lower confidence, flagged by reviewer:)* the burst-absorb loop
  (`while let Ok(ev) = rx.try_recv()`) never calls `due()` while events keep arriving,
  so sustained streams may starve emission past the `max_delay` cap.
- **S-16 · perf/usab · `crates/rsd-log/src/lib.rs:462-471`** — `CursorStore::set` is
  tmp+rename with no fsync; after power loss the cursor can read back as `None`,
  which is safe but forces the S-7 whole-root rescan for every unclean shutdown.
  One `sync_data` before rename avoids it.
- **S-17 · usab/perf · `crates/rsd-catalog/src/lib.rs:407-411,444-446`** —
  `Catalog::open` defaults to `Durability::Immediate`, so the ergonomic per-item
  `apply_stat` costs a txn+fsync per file — a foot-gun for library callers (the daemon
  avoids it; nothing steers others to `apply_stats`).
- **S-18 · perf · `crates/rsd-log/src/lib.rs:102,224-227,327,350-359`** — per-segment
  path-set tracking allocates an owned `String` per record on the append hot path and
  rebuilds (then discards) the set for sealed segments during open; inflates startup
  memory for small-record segments.

---

## Q. Query, search & ranking
*(rsd-lexical, rsd-query, rsd-vector, rsd-ml, rsd-live)*

### High

- **Q-1 · perf · `crates/rsd-vector/src/lib.rs:286-317`** — every semantic search
  brute-force scans the entire redb table and postcard-deserializes **every**
  document's chunk vectors (fresh `Vec<f32>` per chunk) before any dot product. At the
  module's own stated scale (~1M chunks × 384 dims) that is ~1.5 GB of decode+alloc
  per query. The header comment prices only the dot products ("a few ms"), not the
  dominant deserialization. No cached matrix, no pruning, no top-k pushdown (full sort
  at :290-316). The hybrid free-text path always calls this.
- **Q-2 · perf · `crates/rsd-live/src/lib.rs:225-268`** — live evaluation is
  O(views × deltas) with a CAES fetch + full-document retokenization **inside the
  innermost loop**: `text_of` (redb get + full `ExtractionRecord` decode) runs once
  per view per delta, and `matches_text` re-tokenizes the whole document per text
  predicate per view (each token cloned, :52). 50 standing queries × one changed 1 MB
  file = 50+ decodes and tokenizations that could be computed once per delta.
- **Q-3 · perf · `crates/rsd-vector/src/lib.rs:214-271` + `rsd-daemon/src/commit.rs:98-106` + `ipc.rs:225`** —
  `VectorPlane::apply` embeds all chunks synchronously **inside an open redb write
  txn while holding the plane `Mutex` shared with the query path**. The comment
  justifies this for the "μs-fast" hash embedder, but `main.rs:94-107` wires MiniLM
  when the model is present: a batch of large documents holds the mutex for
  seconds-to-minutes of BERT inference, stalling every query and subsequent commit.
  DESIGN.md §7.3 explicitly calls for an asynchronous semantic timeline.
- **Q-4 · perf · `crates/rsd-live/src/lib.rs:253-266` + `commit.rs:107-108`** —
  semantic Alert views run a **full MiniLM forward pass per chunk, per view, per
  delta, synchronously on the commit path**, duplicating the embeddings
  `VectorPlane::apply` just computed for the same commit (embeddings are not shared
  between alert views either). N saved alerts ⇒ ~(1+N)× embedding cost on every
  changed file; directly conflicts with the p99 < 10 ms live-notify target.
- **Q-5 · usab · `crates/rsd-query/src/lib.rs:575-598,604-615,653` (+ `eval_live` :709-718)** —
  **`!=` silently behaves as `==` on text and symbol predicates.** Verified: the
  `Expr::Cmp { attr: TextContent, .. }` arms of `collect_text_sets`/`bind_text_sets`
  pattern-match the operator away and reduce the predicate to bare set membership;
  `eval` only tests `contains`. `kMDItemTextContent != "invoice"` returns exactly the
  invoices. Silent wrong results, against the crate's own "honest error" charter.
- **Q-6 · usab · `crates/rsd-query/src/lib.rs:261-273`** — a misspelled builtin
  silently degrades to full-text search: only `kMD`/`kRSD`-prefixed words error, so
  `inRange(kMDItemFSSize,10,100)` or `Semantic("x")` (wrong case) become content
  searches for the literal token — empty/nonsense results, no diagnostic.
- **Q-7 · usab · `crates/rsd-vector/src/lib.rs:294-298` + `rsdfind.rs:108-113`** —
  embedder-ID mismatch is skipped silently: vectors indexed by the daemon with MiniLM
  are all dropped when the CLI falls back to `HashEmbedder` (model dir missing), so
  `rsdfind --semantic` returns consistently zero hits with no warning or skipped-count.
  Pairs with U-6 (partial model download ⇒ permanent silent fallback).
- **Q-8 · perf · `crates/rsd-query/src/lib.rs:490-516` + `rsd-catalog/src/lib.rs:596-632`** —
  the mixed-predicate path materializes the **entire catalog** via `listing()`
  (decoding every object into a BTreeMap), then calls `get_by_path` per entry — a
  fresh redb read txn and a second decode of the same record. `kMDItemFSSize > 1000`
  on a 1M-file catalog ⇒ ~1M read transactions + two full decodes each, before
  evaluating anything. One read txn iterated once removes almost all of it.

### Medium

- **Q-9 · usab · `crates/rsd-vector/src/lib.rs:182`** — no read-only open for the
  vector plane (`Database::create` = exclusive write handle), so CLI semantic search
  fails whenever the daemon runs; `rsdfind` swallows the error (`.ok()`, rsdfind.rs:198)
  and then reports the misleading "query needs a vector plane… but none is open".
  The lexical plane solved exactly this with `LexicalReader`. *(Feeds U-2.)*
- **Q-10 · perf · `crates/rsd-query/src/lib.rs:398-413,659-676`** — `glob_match`
  allocates two fresh `String`s per call (even case-sensitive) and uses naive
  backtracking that is exponential for multi-star patterns; invoked once per catalog
  entry per name predicate ⇒ 2N allocations per query and `*a*a*a*a*` blowups.
- **Q-11 · perf/usab · `crates/rsd-query/src/lib.rs:559-598`** — text predicates in
  mixed queries fetch top-65,536 docs into a HashSet per predicate (common word ⇒
  large TopDocs heap + stored-field fetches every query) and **silently truncate**
  beyond that: `AND`/`NOT` combinations miss valid results with no signal.
- **Q-12 · perf · `crates/rsd-live/src/lib.rs:229-236,303-315`** — dead subscribers
  are only detected on the next send attempt, so a rarely-firing view whose client
  vanished leaks forever and keeps paying full per-delta evaluation (CAES fetch,
  tokenization, alert embedding) indefinitely. *(Related: D-14, no UDS heartbeat.)*
- **Q-13 · usab · `crates/rsd-query/src/lib.rs:708` + `rsd-daemon/src/ipc.rs:158-173`** —
  `eval_live` returns `false` for `Semantic` predicates: a standing query containing
  `semantic(...)` never matches after seeding — and any touch of a seeded member emits
  a spurious **Leave** even though the content still matches. No subscribe-time error
  says semantic isn't supported in live views.
- **Q-14 · usab · `crates/rsd-query/src/lib.rs:471,560,582,767`** — phrase search is
  implemented in the lexical plane (`PhraseQuery`, rsd-lexical :199-203) but every
  call site passes `phrase=false`, so quoted `"foo bar"` matches both words anywhere.
  Undocumented in DIVERGENCES.md; users will assume quotes mean phrases.
- **Q-15 · perf · `crates/rsd-ml/src/lib.rs:31,83-116`** — no batching and a single
  global `Mutex` over model+tokenizer serialize all embedding to one core,
  batch-of-1 forwards (the comment concedes the lock exists only for tokenizer
  state). Gates semantic indexing throughput (Q-3) and alert latency (Q-4).
- **Q-16 · perf/usab · `crates/rsd-ml/src/lib.rs:75,89-90` + `rsd-vector/src/lib.rs:137-157`** —
  the whole chunk is WordPiece-tokenized before manual truncation to 256 tokens
  (wasted CPU), and the chunker never splits oversized paragraphs: minified JS, logs,
  or any blank-line-free document becomes one giant chunk, so everything past ~256
  tokens is invisible to semantic search with no indication.
- **Q-17 · perf · `crates/rsd-daemon/src/ipc.rs:225-235`** — `run_query` holds the
  `VectorPlane` mutex for the entire query even for queries that never touch vectors;
  all concurrent clients plus the commit path serialize on one lock. `search` takes
  `&self`; a RwLock or read-only handle removes the contention.

### Low

- **Q-18 · usab · `crates/rsd-query/src/lib.rs:658-676`** — relational operators on
  string attributes silently act as equality (`kMDItemFSName < "b.txt"` ⇒ glob-match)
  instead of returning `Unsupported`.
- **Q-19 · usab · `crates/rsd-query/src/lib.rs:291-323`** — no string-escape
  mechanism; a query term containing `"` is inexpressible and yields an unrelated
  "trailing input" error.
- **Q-20 · perf · `crates/rsd-query/src/lib.rs:497-499`** — scope-prefix `format!`
  allocates per catalog entry inside the scan loop; hoistable.
- **Q-21 · perf · `crates/rsd-query/src/lib.rs:457,476,777`** — path resolution calls
  `get_object` per hit (fresh read txn + full decode each); up to 10k txns per query.
- **Q-22 · perf · `crates/rsd-lexical/src/lib.rs:260-280`** — `rebuild()` materializes
  the entire journal into a Vec then clones every `Change` per chunk — double peak
  memory on a multi-million-record journal (repair path only).
- **Q-23 · perf · `crates/rsd-vector/src/lib.rs:117-122`** — `HashEmbedder` allocates
  a String per token and a `format!` per bigram on the indexing hot path.

---

## D. Daemon, workers, IPC & extraction
*(rsd-daemon, rsd-ipc, rsd-worker, rsd-sandbox, rsd-extract)*

### High

- **D-1 · perf · `crates/rsd-daemon/src/lib.rs:219-224` + `dispatch.rs:99-116`** —
  content extraction runs inline on the single applier thread: per file, a full
  blake3 hash read (≤ 32 MB, dispatch.rs:70-84), a blocking worker round trip (up to
  the 10 s pool timeout), a second journal-fsync'd commit, a tantivy commit, and
  embedding. One slow/hung file stalls all metadata commits; the bounded work channel
  backs up into the FSEvents channel ⇒ overflow ⇒ full rescans (S-7) — compounding.
- **D-2 · perf · `crates/rsd-worker/src/lib.rs:256-282` (+ `PoolConfig::default`
  size=2, :169-178)** — **the worker pool provides zero parallelism.** Verified:
  `request(&mut self)` does a fully synchronous send→recv round trip, so at most one
  request is in flight regardless of pool size; round-robin only alternates which idle
  worker serves the next serial request. DESIGN.md §3 specifies N ≈ physical cores/2.
- **D-3 · perf · `crates/rsd-daemon/src/commit.rs:87-111` + `rsd-lexical/src/lib.rs:157-161`** —
  per-work-item commit granularity: each changed file typically costs one metadata
  commit (journal append + fsync; `sync_on_append` defaults true), a second
  `SetContent` commit (second fsync), and a full tantivy `prepare_commit`/`commit`
  per dirty batch. DESIGN.md §6.4 specifies "~1000 docs or ~2 s" lexical cadence; the
  implementation can do a segment commit + 2 fsyncs per file during npm/build churn.
- **D-4 · perf** — *(merged into Q-3: synchronous embedding under the vector mutex on
  the commit path — flagged independently by three reviewers.)*
- **D-5 · perf · `crates/rsd-daemon/src/lib.rs:234-248`** — overflow self-heal and
  `RescanRecursive` work items commit the entire resolved subtree as **one unchunked
  batch** (`resolve_and_commit` passes the full vec straight to `commit()`; bootstrap
  chunks at 1024 but this path doesn't). Giant Vec + one enormous journal append +
  one massive redb txn, exactly when the system is already behind. *(Same root
  observation as S-8.)*

### Medium

- **D-6 · perf · `crates/rsd-daemon/src/http.rs:140`** — `LexicalReader::open()` per
  `/api/search` request (connection-per-request server): a fresh tantivy
  Index+reader rebuild (mmap, fd churn) on **every palette keystroke**. The IPC path
  reuses one per connection; HTTP reuses nothing.
- **D-7 · perf · `crates/rsd-daemon/src/http.rs:279`** — `snippet()` lowercases the
  entire extracted text (up to the 2 MB budget) of every hit per request: up to
  limit × 2 MB of allocation/scanning per keystroke. Same pattern in rsd-mcp.rs:174.
- **D-8 · perf · `crates/rsd-daemon/src/ipc.rs:159-170`** — the `Subscribe` handler
  holds the LiveEngine mutex across the full initial `run_query` (possibly semantic ⇒
  vector mutex too); the committer's `on_commit` needs the same lock, so one expensive
  subscription stalls the whole commit pipeline for its duration (head-of-line, not
  deadlock).
- **D-9 · perf · `crates/rsd-worker/src/bin/rsd-worker.rs:43-47` + `dispatch.rs:136-137`** —
  every novel file is read end-to-end **twice**: daemon streams ≤ 32 MB through blake3
  for the CAES key, then the worker `read_to_end`s the same bytes into a fresh Vec —
  even for content `sniff()` will classify as Binary from the first 8 KB and discard.
- **D-10 · perf · `crates/rsd-extract/src/lib.rs:135-167` + `source.rs:127-141`** —
  `parse_timeout_ms` is wired only to tree-sitter; `pdf_extract::extract_text_from_mem`
  has no cooperative timeout or output budget, so a pathological PDF burns the full
  10 s host kill-timeout, costs a worker kill+respawn, and repeats up to 3× before
  quarantine — each stall landing on the applier thread (D-1).
- **D-11 · usab/perf · `crates/rsd-daemon/src/commit.rs:128-136,65-66`** — `recover()`
  buffers the whole pending journal backlog into one `Vec<LogRecord>` (memory ∝
  downtime), and replays **only into the catalog** — the reviewer could not find code
  replaying into the lexical/vector planes despite the plane-watermark comment
  claiming it *(uncertain: flagged with that caveat)*. If real, a crash between
  journal append and plane apply leaves search silently missing content until the
  file next changes.
- **D-12 · usab · `crates/rsd-daemon/src/lib.rs:277-303`** — `open_catalog_resilient`
  treats **any** open failure as corruption and deletes the catalog: a lock held by an
  already-running daemon, transient EPERM, or disk-full destroys a healthy projection
  and forces full journal replay *(uncertain how redb surfaces a cross-process lock;
  both Err and panic paths delete)*. Compounded by D-13 (no single-instance guard).
- **D-13 · usab · `crates/rsd-daemon/src/main.rs` + `ipc.rs:49`** — no single-instance
  guard (no pidfile/flock): double-starting on the same state dir relies on a raw redb
  error, and `start_ipc` unconditionally unlinks `rsd.sock`, so a second instance that
  gets that far silently breaks the first daemon's IPC for all clients.
- **D-14 · usab · `crates/rsd-daemon/src/ipc.rs:180-215`** — UDS subscription streams
  have no heartbeat (SSE has `: ping`, http.rs:251): a vanished client on a quiet
  view leaks the connection thread + LiveEngine view/channel indefinitely (Q-12).
- **D-15 · usab/perf · `crates/rsd-daemon/src/lib.rs:95-101`** — the FSEvents
  since-token exists in `WatchConfig` but is never persisted/reused (`since: None`
  always), so every daemon restart pays a full trickle re-walk of the root (at
  8 ms/dir ≈ 13 min per 100k dirs; ~1.7 h at the 60 ms battery pace), during which
  results for unvisited directories are stale.
- **D-16 · perf/usab · `crates/rsd-daemon/src/http.rs:100-111,34` + `ipc.rs:58`** —
  HTTP handler has no read timeout and no size limit on request line/headers
  (`read_line` grows unboundedly); both HTTP and IPC are uncapped thread-per-connection,
  so a buggy local client can pin threads indefinitely.

### Low

- **D-17 · perf/usab · `crates/rsd-daemon/src/ipc.rs:118-127,217-243`** — `count_only`
  still materializes up to 10,000 full `Hit`s just to return a length; and grant
  filtering runs **after** the engine's 10k cap, so a scoped principal can get 0 hits
  (or a wrong count) even when matches exist beyond the cap, with no truncation signal.
- **D-18 · perf · `crates/rsd-daemon/src/dispatch.rs:159-161`** — second `caes.get`
  per miss just to test `Corrupt`.
- **D-19 · perf · `crates/rsd-daemon/src/dispatch.rs:65,170-190`** — the `failures`
  map only evicts on success or quarantine; once-failed never-seen-again content
  accumulates in daemon memory forever.
- **D-20 · usab · `crates/rsd-daemon/src/main.rs:163-185`** — stats line to stderr
  every 5 s forever with no quiet flag (launchd log noise); `t0`/`boot` computed then
  discarded so bootstrap duration is never reported; the loop never notices dead
  pipeline threads — a panicked applier leaves a daemon that looks alive but indexes
  nothing.
- **D-21 · usab · `crates/rsd-daemon/src/main.rs:15-18,151-162`** — any CLI parse
  problem prints the same generic usage; no `--help`/`--version`; HTTP port 5871 is
  hardcoded (no flag) and a bind conflict surfaces as a raw "Address already in use"
  with no mention of the port or a likely second instance. *(Also U-16: whichever
  daemon owns 5871 is the one the palette silently searches.)*
- **D-22 · perf · `crates/rsd-daemon/src/lib.rs:308-322`** — shells out to
  `/usr/bin/pmset` every 64 directories (~2 process spawns/sec at the 8 ms pace)
  during bootstrap; cache the answer or use IOKit.
- **D-23 · noted (security-adjacent, outside requested scope) · `http.rs:90`** —
  `Access-Control-Allow-Origin: *` on the localhost API lets any webpage in the
  user's browser query the full file index (paths + snippets) via fetch to
  127.0.0.1:5871. Not a perf/UX item but too consequential to omit.

---

## U. User-facing surfaces
*(RSD.app, rsdfind, rsd-mcp, scripts, docs, first-run)*

### High

- **U-1 · usab · first-run end-to-end (`Daemon.swift:44`, `RSDApp.swift:185-188`)** —
  a brand-new user gets a silent, seemingly broken palette for the entire home-dir
  bootstrap: the palette answers from a partial index and shows "Nothing for X" —
  indistinguishable from failure. `/api/status` exposes counts and the daemon tracks
  `bootstrap_done` (main.rs:179), but the app surfaces no indexing progress or
  "indexing…" state anywhere. *(Feeds on S-12's lack of scan progress.)*
- **U-2 · usab · `rsdfind.rs:192`, `rsd-mcp.rs:42-49` (+ `rsd-catalog/src/lib.rs:414`)** —
  the CLI and MCP server **cannot run while the daemon is running** (redb exclusive
  write handle; the open path even begins a write txn to create tables), but README
  lines 19-21 advertise them side by side with no caveat. rsdfind fails with a raw
  "cannot open catalog"; `rsd-mcp` dies on `.expect("catalog")` — an MCP client sees
  a broken pipe. *(Root cause shared with Q-9; rsdfind's own source header admits
  "run against a quiesced state dir".)*
- **U-3 · usab · `rsdfind.rs:192`, `rsd-mcp.rs:42`** — a mistyped `--state` path
  silently **creates a brand-new empty index** (redb `create`, plus `LexicalReader::open`
  does `create_dir_all`) and returns "no results" with exit 0, littering empty
  databases. Agents querying rsd-mcp against a stale dir get zero hits for everything
  with no diagnostic.
- **U-4 · usab · `app/Sources/RSD/RSDApp.swift:144-148,185`** — fast typing flashes a
  false "rsd isn't running" banner per keystroke: cancelling the in-flight task makes
  `URLSession.data` throw `URLError.cancelled`, which the generic `catch` treats as
  daemon death (`daemonUp = false`), hiding all results until the next response. No
  `Task.isCancelled`/`CancellationError` check exists in the catch path.
- **U-5 · perf · `RSDApp.swift` + `http.rs:141` + `commit.rs:99-104`** — palette
  hybrid searches block on the same `Mutex<VectorPlane>` the indexer holds while
  embedding committed content (Q-3): during bootstrap or a large save, a keystroke
  can hang for the full embedding batch — and the app has no spinner and no timeout
  override (60 s default), so the user sees frozen stale results.
- **U-6 · usab · `scripts/fetch-model.sh:8-11`** — an interrupted model download
  leaves a partial file treated as complete forever (`[ -f ]` guard, no `-C -`, no
  checksum, no `--remove-on-error`). `MiniLmEmbedder::load` then fails and every
  surface silently falls back to the hash embedder (one stderr line, which the app
  redirects to a log file) — permanently junk "meaning" results with no visible
  indication. *(Pairs with Q-7's silent embedder-mismatch skip.)*

### Medium

- **U-7 · usab · `app/Sources/RSD/Daemon.swift:20-31,49`** — the daemon is
  health-checked exactly once at launch; spawn failures are swallowed (`try? p.run()`);
  no respawn. If the daemon crashes later the palette shows "rsd isn't running —
  rsd-daemon watch <folder>" — a misleading instruction since the app owns and
  bundles the daemon. `responds()` counts **any** HTTP response on 127.0.0.1:5871
  (even a 404 from an unrelated server) as "daemon up".
- **U-8 · usab · `Daemon.swift:42-51,59-62`** — orphan daemon on abnormal app exit:
  no termination handler, no parent-death linkage; `stop()` runs only on clean
  terminate. Force-quit ⇒ the daemon keeps indexing the whole home dir indefinitely,
  and no `rsd-daemon stop`/`status` subcommand exists to find or kill it.
- **U-9 · usab · `RSDApp.swift:126-131` + `http.rs:59-61`** — queries containing `+`
  are silently corrupted: `URLComponents.queryItems` doesn't percent-encode `+`, and
  the server decodes `+` as space, so "c++" searches for "c  ". Wrong/empty results
  with no hint.
- **U-10 · perf · `RSDApp.swift:312`** — `NSWorkspace.shared.icon(forFile:)` called
  synchronously on the main thread inside `HitRow.body`, uncached; every arrow-key
  selection change re-renders all rows ⇒ repeated disk-touching icon lookups on the
  UI thread; visible jank at 40 results on slow/network volumes.
- **U-11 · usab · `rsdfind.rs:206-226`** — `--hybrid` silently ignores `-onlyin`,
  `-name`, `-0`, and `--explain`, and `-count` is capped at the hardcoded 100 —
  un-scoped, un-filtered, silently truncated results with no warning.
- **U-12 · usab · `rsdfind.rs:147`** — unknown flags — including `--help`, `-h`, and
  typos — are silently swallowed into the query text: `rsdfind --state d --help`
  content-searches the literal "--help". No help output exists beyond the usage line.
- **U-13 · usab · `README.md:19-21` vs `main.rs:47-54`** — README requires
  `--state <state-dir>` but never says where the daemon puts it; the default is the
  non-obvious sibling `<parent>/.rsd-state-<rootname>` while the app uses
  `~/Library/Application Support/rsd`. No `RSD_STATE` env fallback. A quick-start
  user must read source to construct the CLI invocation.
- **U-14 · usab · `app/Sources/RSD/Alerts.swift:101-107,36`** — ⌘S standing alerts
  give zero feedback if notification permission is denied/unresolved: `deliver` is
  gated on `authorized` (set asynchronously), so the "Watching for…" confirmation and
  all alerts are silently dropped; the palette shows no in-UI confirmation for ⌘S.
- **U-15 · perf · `rsd-live/src/lib.rs:246-247,273-279` + `http.rs:191-197` + `RSDApp.swift:100-103`** —
  each palette connection registers a match-all standing view whose members set
  accumulates an entry per indexed file per client, `on_commit` does a CAES text
  lookup per delta even for attribute-only predicates, and the app re-runs the
  current search on every SSE tick — a continuous ~10 queries/sec storm against the
  contended daemon during bulk indexing.
- **U-16 · usab · `main.rs:151-161`** — *(see D-21)* hardcoded port 5871 + silent
  wrong-daemon adoption: the palette searches whichever daemon owns the port, which
  can be a different index (different root/state) than the user expects.
- **U-17 · perf · `rsdfind.rs:197-198`** — every rsdfind invocation loads the ~90 MB
  MiniLM model (tokenizer JSON parse + safetensors load) even for pure name/attribute
  queries that never touch vectors; the embedder is only needed for
  `--semantic`/`--hybrid`. Fixed startup latency on every CLI call.

### Low

- **U-18 · usab · `RSDApp.swift:136-137,178-180`** — spring animation on every result
  update + live-refresh re-queries during indexing ⇒ palette height and row order
  visibly bounce as results stream in.
- **U-19 · usab · `Summon.swift:49-75,144-147`** — ⌥Space is hardcoded (no rebinding;
  collides with non-breaking-space on many international layouts and other launchers),
  and `showPalette()` re-centers the window on every summon, discarding repositioning.
- **U-20 · usab · `RSDApp.swift:144-145`** — server-side 400s fail `SearchResponse`
  decoding, so the error text is discarded and shown as "Nothing for X".
- **U-21 · usab · `rsd-mcp.rs:59-62,109`** — malformed JSON lines are dropped with no
  JSON-RPC error (strict clients time out); unknown methods return `{}` instead of
  -32601.
- **U-22 · usab · `rsd-mcp.rs:174-180`, `http.rs:280-281`** — snippets anchor to the
  *first query token* only; for semantic/hybrid hits where that token doesn't occur,
  the excerpt is just the start of the file — a plausible-looking but ungrounded
  "citation" exactly where grounding matters most.
- **U-23 · usab · `rsd-mcp.rs:133,141`, `rsdfind.rs:162,167`, `http.rs`** — a `"` in
  the query is interpolated raw into RQL (parse error) on CLI/MCP but stripped on
  HTTP — inconsistent behavior across surfaces for the same query.
- **U-24 · usab · `rsdfind.rs:29-37,76-92`** — `-live` handshake failures `.unwrap()`
  (Rust backtrace instead of a message); daemon shutdown mid-stream exits 0 silently.
  (CPU-wise `-live` is event-driven over UDS — no polling — that part is fine.)
- **U-25 · usab/perf · `Daemon.swift:53-57` + `main.rs:166-184`** — the app truncates
  daemon.log on every launch (destroying the previous crash's evidence) while the
  daemon appends a stats line every 5 s with no rotation — unbounded within a session,
  amnesiac across sessions.
- **U-26 · usab · DESIGN.md §8.1/§8.3 vs implementation** — *(see A-3)* mdfind flag
  compatibility, `--ask`, and MCP `answer` don't exist yet; labeled targets, but
  cross-references will mislead.
- **U-27 · usab · `scripts/build-app.sh:28`** — `codesign … || true` with stderr
  discarded: signing failures are silent; the app is ad-hoc signed regardless and
  first-launch Gatekeeper friction is undocumented.
- **U-28 · usab · `Alerts.swift:29` *(uncertain)*** — alert threshold hardcoded at
  0.35 with no tuning UI; with the hash-embedder fallback active the score
  distribution is likely uncalibrated to that threshold (spam or silence — direction
  unverified; the silent substitution and missing control are verified).

---

## Recommended priorities (if/when fixes are commissioned)

1. **Decouple the pipeline** (A-2 umbrella): move extraction+embedding off the
   applier thread onto a real concurrent worker pool (D-1, D-2, Q-3, Q-4), batch
   commits per DESIGN §6.4 cadence (D-3), and chunk rescan commits (S-8/D-5). This
   single workstream addresses the worst latency, throughput, and overflow-feedback
   problems at once.
2. **Fix the silent-wrong-results bugs** (small, high leverage): `!=` on text/symbol
   predicates (Q-5), misspelled-builtin fallthrough (Q-6), `+`-in-query corruption
   (U-9), spurious Leave events for semantic live views (Q-13), grant-after-truncation
   (D-17), quoted-phrase semantics documented or implemented (Q-14).
3. **Make the CLI/daemon coexist and fail loudly** (U-2, U-3, Q-9): read-only plane
   opens, an "is this an rsd state dir?" check instead of implicit create, a
   single-instance guard + friendlier startup errors (D-12, D-13, D-21).
4. **First-run visibility** (U-1, S-12, U-4, U-6): indexing-progress state in the
   palette, cancellation-aware error handling, checksummed resumable model fetch.
5. **Storage hygiene**: journal compaction + seal-manifest-trusting open (S-3, S-4),
   orphan index instead of 200 ms full sweeps (S-5), no-op-rescan early-outs (S-10).
6. **Gate the numbers** (A-1): run the perf smokes in CI (release, `--ignored`) with
   generous thresholds so regressions of 10× get caught even if 10 % ones don't.
