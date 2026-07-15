# Codex critique of `DESIGN.md`

## Overall assessment

This is a strong product vision and a better-than-average early architecture document. It identifies real weaknesses in desktop search, makes sensible use of process boundaries, treats overload and crash testing as design inputs, and gives ambitious features an explicit prototype order. The author clearly understands retrieval systems and has thought about operational behavior rather than drawing only a box diagram.

The document is not yet an implementable correctness specification, however. Several of its strongest guarantees do not follow from the storage model it describes, and a few are internally contradictory. In particular:

- a lossy observation log cannot be both the sole source of truth and a guarantee of convergence to an independently changing filesystem;
- the current-only lexical/vector planes and attribute-only history cannot answer the advertised historical content queries;
- Unix peer credentials cannot establish a caller's macOS sandbox and TCC visibility;
- the proposed point-update live-view algorithm does not preserve the semantics of ranked, graph, temporal, or all text queries;
- `(device, inode, birthtime) -> one document with one path` does not model hard links correctly.

Those are architectural issues, not matters of taste. They should be resolved before the crate layout or roadmap is treated as a build plan. The core product remains viable if its guarantees and T0 scope are narrowed.

## What the design gets right

### Process isolation follows actual fault boundaries

Separating native parsing, legacy importers, WASM plugins, media processing, and ML inference is well motivated. The distinction between concurrency and fault/privilege isolation is exactly right. Fixed worker pools, per-request limits, kill-and-respawn behavior, and per-importer failure isolation are all sound starting points.

### Backpressure and subscriber recovery are designed in

Bounded queues, collapsing fine-grained work into rescan markers, and replacing an overflowing subscriber buffer with a resync fence are good patterns. They convert overload into explicit staleness or extra I/O rather than unbounded memory use. This is one of the most credible parts of the proposal.

### Lexical freshness is correctly separated from semantic completeness

Making lexical indexing the fast path while embeddings, OCR, and transcription trickle in is the right product tradeoff. Tracking semantic generation separately is also better than pretending the whole index has one atomic freshness state.

### The risk-first prototype sequence is unusually good

Convergence, multi-plane crash recovery, live-query fidelity, embedding throughput, and Endpoint Security feasibility are appropriate spikes. Crash injection and retrieval evaluation being permanent gates rather than late polish is a serious strength.

### Observability and explainability are first-class

`doctor`, `top`, rebuild controls, query `EXPLAIN`, visible NL-to-RQL compilation, and ranking explanations would make the system substantially easier to trust than an opaque background indexer.

### The extraction extension model is promising

A WASM component boundary with explicit inputs, fuel, and memory limits is a good direction. It is materially safer than loading third-party native bundles into a shared process, even though the document overstates the absolute safety and determinism it provides.

## Blocking correctness and security concerns

### 1. “The log is the truth” conflicts with convergence to filesystem truth

The design says the catalog must converge under “any event stream,” including dropped events, while also saying the change log is the truth and corruption never causes a crawl. These cannot all be invariants.

The log contains mutations *the system believes in*. If FSEvents omits or coalesces a mutation, the missing filesystem state is absent from both the log and every materialized plane. Replaying that log preserves the error; it cannot discover the truth. Apple explicitly requires a scan when events are dropped because there is no way to determine what changed ([FSEvents guide](https://developer.apple.com/library/archive/documentation/Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/UsingtheFSEventsFramework.html)). A silently missed event, an implementation bug, or damage shared by the log and catalog likewise requires comparison with the filesystem.

The corruption ladder has the same problem. If a log segment is corrupt, dropping it erases the only claimed source of truth. The system cannot know the segment's affected documents by reading a segment it cannot trust unless that membership is redundantly stored elsewhere. If the catalog and log agree but are stale, a “targeted stat against catalog” will not visit an unrecorded new path. Directory-mtime pruning is a useful optimization, not a proof that a full reconciliation is never required.

The retention policy further weakens P2: after log segments are compacted into attribute diffs, the append-only log is no longer the complete rebuild source described in §4.2.

**Required change:** make the filesystem the external authority and the durable log the authority for *accepted indexing transitions*. Specify:

- how source event cursors are fenced with durable work so a crash cannot acknowledge uncommitted events;
- which FSEvents flags trigger a scoped or full-volume reconciliation;
- a periodic anti-entropy audit for errors that produce no event;
- an honest full-crawl last resort;
- an explicit failure matrix separating crash consistency, detected bit rot, lost log ranges, lost catalog pages, and whole-index loss.

“Normally repairs only affected scopes” is credible. “Never re-crawl under any event stream or plane corruption” is not.

### 2. The log schema is insufficient to deterministically rebuild the planes

The shown `LogRecord` stores identity, change type, a content hash, and evidence. It does not store extracted text, typed attributes, chunk boundaries, embeddings, graph references, extractor identity, tokenizer/schema version, or model version. A content hash is not content.

Re-extraction during replay only works if the exact bytes are still available. They may not be: the file may have changed again or been deleted. A cache hit only solves this if the extraction cache is itself durable, complete, retained for at least as long as the log, checksummed, versioned, and included in the recovery model. None of that storage is specified. It would effectively be another authoritative plane.

Even with bytes, rebuilding after an extractor, tokenizer, schema, or embedding model upgrade is not deterministic in the ordinary event-sourcing sense. It is a new projection version. This distinction matters for watermarks, mixed-model vector indexes, rollback, and historical query semantics.

**Required change:** choose one of two coherent models:

1. Log canonical enriched extraction records (or references to an immutable content/extraction object store), making downstream planes true projections; or
2. Call the log a filesystem-observation journal and explicitly require current-file reads or cache availability to rebuild content planes.

Also define projection version IDs, upgrade/reindex behavior, cache retention, and what happens when bytes for a historical hash no longer exist.

### 3. The advertised temporal search is not supported by the described storage

The catalog history contains attribute diffs. Tantivy and HNSW are described as current-state indexes with deletes/tombstones. The log does not contain prior text or extracted records. Consequently, the system can plausibly answer historical path/size/type questions, but it cannot in general answer a historical text or semantic query, produce an old snippet, or rank deleted/modified documents by their old content.

APFS/Time Machine snapshots do not fill this gap. They are optional, discontinuous, and subject to retention outside rsd's control. They may allow retrieval of some historical bytes, as the document cautiously says in §4.3, but cannot justify the unconditional thesis-level promise that arbitrary `AS OF` queries “fall out” of the log.

Daily compaction is also underspecified: which state represents a day, how are valid-time intervals rounded, and what semantics does `AS OF 14:30` have after fine history has been compacted?

**Required change:** define separate guarantees for:

- metadata history;
- historical content availability;
- historical lexical/semantic search;
- retrieval of old bytes.

Full historical search needs retained versioned extraction records plus either version-aware indexes or a documented candidate-generation/replay algorithm. If snapshots are merely opportunistic, the API must expose availability and return an explicit “content version unavailable” state.

### 4. `LOCAL_PEERCRED` cannot enforce the stated macOS visibility policy

P6 promises that a result is checked against the caller's UID *and sandbox*. The IPC design authenticates a Unix-domain-socket caller with `LOCAL_PEERCRED`. Same-UID identity is not the same thing as the caller's effective App Sandbox extensions, code-signing identity, TCC grants, app-container access, or user-selected security-scoped URLs.

This creates a direct confused-deputy risk: an index daemon with broad or Full Disk Access could return indexed content to a sandboxed process that could not have read the source file. Checking POSIX mode bits as the user's UID would incorrectly authorize it. macOS applies discretionary permissions, App Sandbox, and other mandatory controls independently ([Apple's sandbox file-access overview](https://developer.apple.com/documentation/security/accessing-files-from-the-macos-app-sandbox)). Protected folders and Full Disk Access are also separate TCC services ([Apple's protected-resource documentation](https://developer.apple.com/documentation/xcode/resetting-access-to-protected-resources-in-macos)).

Filtering only final results is not automatically safe either. Counts, aggregate values, rank changes, errors, timing, provenance edges, subscription deltas, and snippets can all disclose excluded documents. Permission changes on a directory ACL can affect an entire subtree without changing each document's indexed attributes.

**Required change:** treat authorization as a T0 design, not a later XPC shim. At minimum:

- use an IPC mechanism that supplies an audit token/code-signing identity when caller identity matters;
- define explicit user-granted search scopes or unforgeable capabilities and their revocation model;
- apply visibility before aggregates and preferably before ranking/candidate-dependent output;
- define behavior for ACL/TCC/container changes and subscriptions;
- threat-model arbitrary same-user local processes and the MCP surface.

It may be simpler and safer to expose the broad index only to a trusted first-party client and give third parties explicit scoped capabilities. The current claim that the daemon can infer every caller's “actual read access” from UDS credentials is categorically unsupported.

### 5. The file identity model is wrong for hard links and incomplete for filesystem races

`(device, inode, birthtime)` is a reasonable starting key for a filesystem object, but the catalog maps it to one `DocRecord` containing one path. A file with two hard links has one device/inode/birthtime and multiple live paths. Removing one directory entry must not remove the object or its other path; renaming one link must not rename the other. `by_path -> doc_id` does not fix the one-path `DocRecord` or the temporal semantics.

Birth time reduces accidental inode-reuse collisions but should not be presented as a permanent stable identifier without specifying behavior on filesystems that lack reliable birth time, network/removable volumes, restoration, or metadata cloning. Symlinks also need their own identity and policy rather than being silently treated as their targets.

There is an additional time-of-check/time-of-use race. An event names a path; by the time rsd opens it, hashes it, extracts it, and commits, that path may refer to a different inode or a newer generation. An open fd pins one object, but does not prove that the final catalog path still names that object or that a newer event has not superseded the result.

**Required change:** model at least two entities:

- a filesystem object/content-bearing node, identified by volume identity plus file ID and generation evidence;
- a directory entry/path, with a many-to-one relationship to the object.

At dispatch and commit, record `fstat` identity/metadata from the opened fd, revalidate the current path/object generation, and discard or reschedule stale extraction results. Specify hard-link, symlink, unlink-while-open, case-folding, and Unicode-normalization semantics in convergence tests.

### 6. The live-view algorithm only works for a restricted query subset

The document presents every standing RQL query as a point-maintained materialized view, but many advertised query features have non-local dependencies:

- a top-k ranked result can change membership when one document's score changes; the next document may enter even though it had no delta;
- BM25 scores depend on corpus statistics, so a one-document RAM index does not have bit-identical scores to the main index. Tantivy's BM25 weight uses total documents, document frequency, and average field length ([Tantivy BM25 source](https://docs.rs/tantivy/latest/src/tantivy/query/bm25.rs.html));
- a graph edge update can affect many nodes in a transitive traversal;
- `NOT`, wildcards, broad predicates, and dynamic time expressions do not have a simple finite term trigger footprint;
- frecency/click updates and permission changes can reorder or remove results without a file-content delta;
- a deletion or replacement needs old text membership, but the shown delta has one `text_ref` and the old lexical document has already been deleted;
- ANN/RRF/reranked query results are global ranked sets, whereas the semantic subscription is redefined as a per-document threshold query.

The one-document matcher can preserve tokenization and boolean term/phrase membership for a useful subset. It cannot make global scoring or top-k semantics identical merely by sharing tokenizer/scorer configuration.

**Required change:** publish a live-maintainability matrix. For example:

- exact incremental maintenance for attribute predicates, unranked lexical membership, and simple aggregates;
- threshold semantics for semantic alerts, explicitly distinct from ANN top-k search;
- dependency-aware recomputation for graph queries;
- bounded top-k repair or full re-query for ranked/hybrid views;
- scheduled invalidation for clock-relative predicates;
- `Resync` for unsupported or high-fanout changes.

Store old membership/text evidence or evaluate the old document before removing it. “Real live queries” remains valuable without claiming every RQL operator has O(point delta) maintenance.

### 7. Semantic indexing has two contradictory commit timelines

Section 4.5 says embeddings are asynchronous and may lag lexical indexing by up to a minute. Section 5 commits a vector tombstone and merely enqueues new chunks. Section 7 then says new/changed chunks “are already embedded on the ingest path” for semantic standing queries, while the performance table gives a 10 ms commit-to-live-view target.

Those paths cannot all describe the same commit. A semantic alert cannot be evaluated until the embedding exists.

**Required change:** define a second durable semantic commit/delta with its own LSN or `(catalog_lsn, semantic_gen)`. Lexical subscribers can receive the initial document delta; semantic subscribers receive a later enter/leave/update after vectors are committed. Subscription fencing and resync must include both watermarks.

## Major unresolved design risks

### 8. The FSEvents and Endpoint Security descriptions are too absolute

Calling FSEvents simply “directory-granularity” is incomplete: macOS supports a file-level notification flag, albeit with higher event volume ([`kFSEventStreamCreateFlagFileEvents`](https://developer.apple.com/documentation/coreservices/kfseventstreamcreateflagfileevents)). Conversely, item-level rename notification is not an atomic old-path/new-path pair; pairing by inode is a reconciliation heuristic with races, especially around deletion and reuse.

Endpoint Security can improve attribution, but “exact” should not be an invariant until loss, sequence gaps, muting, extension restarts, boot ordering, and event coverage are demonstrated. Distribution is also a product dependency, not merely a coding spike: Apple requires a restricted Endpoint Security entitlement ([entitlement documentation](https://developer.apple.com/documentation/BundleResources/Entitlements/com.apple.developer.endpoint-security.client)), user installation/approval, and Full Disk Access in Apple's sample setup ([installation steps](https://developer.apple.com/documentation/endpointsecurity/monitoring-system-events-with-endpoint-security)).

The baseline should explicitly request file events, treat all event sources as hints with gap detection, and retain reconciliation as the correctness mechanism.

### 9. Provenance claims confuse equality, attribution, and lineage

Equal content hashes establish content equality, not that one file was copied from another, and certainly not the direction of copying. A process observed writing a file is a mutator, not necessarily its author or semantic origin. Download/quarantine xattrs are useful but removable and spoofable. Extracted references indicate a link or import, not derivation.

The graph should distinguish observed facts from inferences and attach evidence, time, and confidence. Safer edge names would include `SameContentAs`, `LastWrittenBy`, `ClaimsDownloadedFrom`, and `References`. `CopiedFrom` or `DerivedFrom` should require stronger evidence or be explicitly probabilistic. Without this, graph traversal will return confident-looking false lineage.

### 10. Content-hash caching is under-keyed

Extraction may depend on filename/extension, declared UTI, xattrs, archive-member path, parser options, locale, and other `hints` explicitly present in the WIT signature. Therefore `(content_hash, extractor_id, extractor_version)` is not always sufficient, and “reuses extraction verbatim” is unsafe for path- or metadata-derived attributes.

Split extraction output into content-derived facts and instance/path-derived facts, or include a canonical hash of every relevant hint and host ABI/policy version in the cache key. Apply the same discipline to chunking, OCR language settings, embedding model revisions, and graph-reference resolution.

### 11. “Not indexable is a bug” ignores legitimate limits and hostile expansion

Encrypted archives, password-protected PDFs, unavailable cloud placeholders, unsupported proprietary formats, corrupt data, and media without enough local resources are valid “not currently indexable” states. Archives also introduce zip bombs, extreme nesting, huge member counts, duplicate paths, symlinks, and recursive container formats. WASM fuel does not cap all host-side output allocation or decompression performed before/around the guest.

The extraction contract needs limits on input bytes, decompressed bytes, compression ratio, recursion depth, member count, output text/chunk/reference count, wall time, and scratch space. Partial extraction and explicit status/reason codes are features, not failures of the product thesis.

### 12. Multi-plane commit/recovery needs a real state machine

“Append log; apply catalog, lexical, vector, graph; advance watermarks; fsync” is not enough to specify crash consistency across engines with independent transactions. Important questions remain:

- Is the log record written before or after extraction output is durable?
- What operation durably advances the FSEvents cursor?
- Are projection writes idempotent when replayed after an uncertain commit?
- Can a query observe catalog generation N with lexical generation N-1, and how is that surfaced?
- Is deletion-before-add safe under every replay boundary?
- What does a vector watermark mean while embedding work is outstanding?

MVCC and checksums do not imply recovery from arbitrary page corruption. In particular, “redb falls back to the previous MVCC root” needs to be proven against the chosen redb version and failure modes; transaction crash recovery is not the same as repairing an arbitrary damaged reachable page.

Specify a per-LSN projection state machine, idempotency keys, read fences, source-cursor checkpointing, and fault-injection points before promising mutual reconstruction.

### 13. Resource and latency targets are not yet acceptance criteria

The numbers are useful hypotheses, but most lack the workload definition needed to pass or fail them. “One million files” says nothing about total bytes, extractable formats, average token count, archive expansion, or hot/cold cache state. Query p99 needs hardware, corpus, concurrency, query mix, candidate `k`, cache state, and background indexing load. “M-series laptop” spans materially different machines.

Percentage-of-corpus index-size targets are particularly misleading. A corpus of many tiny source files can have lexical/catalog per-document overhead larger than 5%; vector size is driven by chunk count and dimension, not source byte size, and can easily exceed 3% for concise text. “Unmeasurable (<0.5%/day)” is also self-contradictory: a numeric bound must be measured with a protocol.

Replace these with a benchmark matrix containing at least small-file source trees, office/PDF corpora, photo-heavy libraries, media, and adversarial archives on named hardware. Report bytes per document/chunk alongside corpus-relative ratios. Keep the current numbers as goals until a baseline exists.

### 14. “Every valid `mdfind` predicate” is an unnecessarily brittle promise

The document first calls RQL a Spotlight-compatible *subset* and later a strict superset in which every valid predicate has identical semantics. Those statements conflict. Exact compatibility includes locale-sensitive modifiers, wildcard/parser edge cases, type coercions, date behavior, scope behavior, and attributes produced by arbitrary importers. Differential tests can find discrepancies but cannot prove equivalence over an opaque implementation.

Define a versioned supported grammar and a compatibility test corpus. `rsdfind` can be flag-compatible while returning a clear unsupported-predicate error. This is more credible than making exact emulation a foundational T0 requirement.

### 15. The opening comparison with Spotlight contains factual overreach

The polemic is lively, but “Spotlight answers one question” and the characterization of its live queries as merely “re-poll-ish” are inaccurate. Spotlight supports typed metadata predicates, scopes, grouping/value lists, content relevance, and a live-update phase. Apple's `NSMetadataQuery` documentation explicitly describes initial gathering followed by live updates and result-change notifications ([Apple documentation](https://developer.apple.com/documentation/foundation/nsmetadataquery)).

This does not invalidate rsd's opportunity: semantic retrieval, inspectable recovery, richer temporal behavior, safer extensibility, and agent-facing APIs can still differentiate it. Accurate comparison would strengthen the case. Claims such as “strictly better mds,” “crash-proof,” and “most capable ever” should wait for measured evidence.

## Missing scope decisions that affect the architecture

These do not each invalidate the design, but T0 needs explicit answers because they change identity, convergence, security, or scheduling:

- What roots are indexed by default, and how does onboarding obtain TCC/FDA consent?
- Are iCloud/File Provider placeholders indexed as metadata only, downloaded on demand, or skipped?
- Are network volumes supported, and if so, without FSEvents and with what identity guarantees?
- How are APFS clones, sparse files, packages/bundles, hard links, symlinks, and case-sensitive volumes represented?
- Are archive members first-class temporal `doc_id`s, and how are their paths/identities derived across archive edits?
- What content is deliberately excluded: secrets, browser profiles, keychains, `.git`, dependency trees, private app containers?
- How are index files encrypted at rest, and what happens while the user session is locked?
- Which component owns schema migrations, compatibility across versions, and rollback after a failed upgrade?

## Recommended reframing of T0

A credible T0 would still be compelling if stated more narrowly:

1. Index explicit user-approved local scopes.
2. Treat FSEvents as an acceleration stream and reconciliation as the correctness source.
3. Provide crash-consistent catalog + lexical projections with bounded-staleness watermarks.
4. Support a documented subset of Spotlight-like attribute/text predicates.
5. Maintain exact live membership for a documented incrementally maintainable subset; resync/re-query the rest.
6. Promise metadata history first, with historical content explicitly availability-dependent.
7. Expose IPC initially only to trusted same-product clients or explicit scope capabilities.
8. Make full crawl rare, observable, cancellable, and scope-limited—not theoretically impossible.

That version gives up some rhetoric, not the core product. It also creates a foundation on which semantic search, WASM extraction, provenance, MCP, and richer temporal indexing can be added without making correctness depend on unproven assumptions.

## Bottom line

The design's strongest ideas are worth keeping: isolated extraction, bounded work, explicit freshness generations, inspectable ranking, live deltas with resync, and recovery-focused testing. Its weakest aspect is the tendency to turn desirable goals into universal invariants before the data model supports them.

The most important revision is to separate four concepts that are currently blurred together:

- filesystem truth;
- the durable observation/commit journal;
- current materialized search projections;
- retained historical content.

Once those have distinct authorities, retention rules, and failure semantics—and once caller authorization and file identity are redesigned—the rest becomes an ambitious engineering program rather than an internally contradictory one.
