# Response to the Codex critique of DESIGN.md

Status: v1 · 2026-07-15 · companion to `DESIGN.md` (v2) and `DESIGN_CODEX_CRITIQUE.md`

## Summary of verdicts

I went through all 15 numbered critiques plus the scope-decision list and the T0
reframing with the intent to defend the design where the critique is wrong. The honest
result: **this is a high-quality review and most of it stands.** The critique's central
diagnosis — that the document repeatedly promotes desirable goals into universal
invariants before the data model supports them — is correct, and its proposed
four-authority separation (filesystem truth / durable journal / current projections /
retained historical content) is adopted verbatim as the fix.

What I *reject* is the implied retreat. Nearly every blocking issue is a flaw in the
**claims**, not in the **mechanisms** — in several cases the mechanism the critique
demands is already in the design and the surrounding rhetoric contradicted it. The
resolution is to make the guarantees layered and precise, not to shrink the ambition.
The leapfrog tier (semantic, temporal, provenance, WASM, MCP) survives intact; three
things genuinely change shape (authorization, file identity, and a new durable
extraction store), and one absolute ("never re-crawl") is demoted to what it always
should have been: a measured engineering target.

| # | Critique | Verdict |
|---|---|---|
| 1 | Log-as-truth contradicts convergence | **Accept** — reframe authorities; mechanisms already existed, claims overreached |
| 2 | Log schema can't rebuild planes | **Accept** — resolved by adding a durable extraction store (CAES) |
| 3 | Temporal search unsupported by storage | **Accept** — split into four layered temporal guarantees |
| 4 | `LOCAL_PEERCRED` can't enforce visibility | **Accept — most severe finding.** Authorization redesigned for T0 |
| 5 | Identity model wrong for hard links, TOCTOU | **Accept** — two-entity model, fd-pinned identity, commit revalidation |
| 6 | Live views only work for a subset | **Accept core; refute one framing** — maintainability matrix added; semantic-alert threshold semantics defended as intentional |
| 7 | Contradictory semantic commit timelines | **Accept** — dual watermark `(lsn, semantic_gen)` |
| 8 | FSEvents/ES descriptions too absolute | **Accept** — all sources become hints; reconciliation is the correctness mechanism |
| 9 | Provenance confuses equality/attribution/lineage | **Accept enthusiastically** — evidence-carrying edges make the graph better |
| 10 | Extraction cache under-keyed | **Accept** — content-derived vs. instance-derived fact split |
| 11 | "Not indexable is a bug" ignores hostile inputs | **Partial** — limits and status codes adopted; the slogan survives, reworded |
| 12 | Commit/recovery needs a real state machine | **Accept** — required spec work, promoted into the doc |
| 13 | Perf targets aren't acceptance criteria | **Partial** — benchmark matrix adopted; design-to targets retained |
| 14 | "Every valid mdfind predicate" is brittle | **Accept** — versioned grammar + compat corpus + explicit unsupported errors |
| 15 | Spotlight polemic contains factual overreach | **Partial** — factual errors conceded and fixed; the thesis stands |
| — | Missing scope decisions | **Accept** — preliminary T0 positions given below |
| — | Recommended T0 reframing | **Accept ~90%** — with the ambition tiers explicitly preserved |

---

## Blocking issues

### 1. "The log is the truth" vs. convergence — **Accept**

The critique is right, and the internal contradiction is real: a journal of *observed*
mutations cannot simultaneously be (a) the sole source of truth, (b) a guarantee of
convergence to an independently mutating filesystem that reports changes through a
lossy channel, and (c) immune to needing a crawl. If FSEvents drops an event, the
missing state is absent from the log and from every projection derived from it; replay
faithfully reproduces the gap.

What I'll note in partial defense: the *mechanisms* the critique demands were already
present — `MustScanSubDirs` handling, readdir-diff expansion against the catalog,
overflow-to-rescan-marker degradation, and Pillar P1 itself names convergence under
dropped events as the top invariant with a crash-injection CI gate. The architecture
never actually relied on the log discovering missed events. The **claims** (P2's "the
log is the truth", §4.7's "never re-crawl" as an invariant) contradicted the
architecture's own correctness story. In a design doc, the claims *are* the contract,
so this is a legitimate blocking finding, not a nitpick.

**Resolution (adopted into v3):**

- **Authorities, restated.** The filesystem is the external authority on current
  state. The log is the authority on *accepted indexing transitions* — what rsd has
  durably decided it observed and applied. Projections are caches of the log.
  Retained content (see #2) is the authority on historical bytes/extractions.
- **Cursor fencing.** A source cursor (FSEvents event ID, ES sequence number) advances
  durably only after the work derived from it is committed. Crash → re-deliver, never
  acknowledge-and-lose. Idempotent application makes re-delivery safe (see #12).
- **Reconciliation is a first-class subsystem**, not a fallback: (a) event-driven
  scoped scans on `MustScanSubDirs` / overflow / gap detection; (b) a continuous
  low-priority **anti-entropy audit** that walks the catalog against the filesystem
  (directory-mtime-pruned) to catch eventless divergence — bugs, offline mutations,
  clock issues; (c) full-volume reconciliation as an honest, observable, cancellable,
  rate-limited last resort (`rsdctl` shows why it triggered).
- **The re-crawl claim, demoted and made falsifiable:** "no plausible single failure
  (crash at any instruction, loss of any one plane, any detected corruption) escalates
  beyond scoped repair; full reconciliation is a monitored rare event with an explicit
  trigger list, not a routine recovery path." That is the true differentiator vs. mds
  — whose failure mode is *silent* full re-crawl — and it's provable in CI.
- The failure matrix the critique requests (crash consistency / detected bit rot /
  lost log ranges / lost catalog pages / whole-index loss, each with detection,
  blast radius, and repair path) becomes a required table in v3.

Retention weakening P2 is also conceded — resolved structurally by #2, where the
rebuild source for content planes stops being the log at all.

### 2. The log schema cannot rebuild the planes — **Accept, resolved by adding a store the design needed anyway**

Correct as charged: `LogRecord` carries hashes and evidence, not content; a hash is not
the bytes; re-extraction during replay requires bytes that may no longer exist; and the
extraction cache, as written, was doing authoritative work (recovery, replay speed,
dedup) without being specified as durable, checksummed, versioned, or retained.

The critique offers two coherent models. I adopt **both halves deliberately**:

- The log is renamed to what it is: a **filesystem observation and transition
  journal**. It never claims to contain content.
- The extraction cache is promoted to a first-class plane: the **content-addressed
  extraction store (CAES)** — durable, checksummed, keyed as specified in #10, storing
  canonical extraction records (text, typed attrs, chunk boundaries, references,
  extractor identity + version). CAES is the rebuild source for lexical/vector/graph
  planes and — this is the important convergence with critique #3 — **it is also the
  storage that makes historical content search real** rather than rhetorical. One new
  component discharges two blocking findings. Embeddings are cached alongside, keyed
  additionally by model revision.
- **Projection versioning**, adopted as specified: extractor/tokenizer/schema/model
  upgrades are new projection versions with explicit reindex-behind-a-generation
  semantics, not "deterministic replay." Watermarks become
  `(lsn, projection_version)` pairs. Mixed-version vector segments are queryable with
  per-segment model tags until the background upgrade completes.
- CAES retention is a user policy and is *the* knob that determines temporal-search
  depth (see #3). "Bytes for a historical hash no longer exist" is a defined API
  state: `ContentVersionUnavailable { reason: RetentionExpired | NeverCaptured }`.

### 3. Advertised temporal search outruns the storage — **Accept**

The critique correctly separates four things the doc blurred. v3 states them as
distinct, individually-priced guarantees:

| Temporal capability | Backing store | Guarantee |
|---|---|---|
| Metadata history (`AS OF`, `CHANGED SINCE`, `DIFF` over attrs/paths/existence) | catalog `history` table | **Guaranteed** within retention policy — T2 |
| Historical content retrieval (old bytes) | APFS/TM snapshots | **Opportunistic, availability-exposed** — never promised, always labeled |
| Historical extracted text (old snippets, old versions of a doc) | CAES retained extraction records | Guaranteed **within CAES retention**, explicit `ContentVersionUnavailable` otherwise |
| Historical full-text/semantic *search* (query old corpus states, rank deleted docs by old content) | candidate generation from catalog history + evaluation over CAES records | **Bounded**: supported via documented candidate-generation + replay, cost proportional to candidate set; not a version-aware inverted index in T2 |

Compaction semantics get specified: fine-grained history compacts to
end-of-local-day state; `AS OF` a time inside a compacted interval resolves to the
nearest retained boundary **and the response says so** (`resolution: day`). No silent
approximation.

The thesis section's "time travel falls out of the log" is rewritten — what falls out
of the log is *ordering and change detection*; historical content is a storage policy
you buy with CAES retention. Honest, and still far beyond anything shipping.

### 4. `LOCAL_PEERCRED` cannot enforce the visibility policy — **Accept. The most important finding in the review.**

No rebuttal. Same-UID peer credentials establish *user* identity, not the caller's App
Sandbox extensions, TCC grants, code identity, or security-scoped URL access. A
broad-access index daemon answering sandboxed callers on POSIX-mode checks is a
textbook confused deputy, and result-set filtering alone leaks through counts,
aggregates, rank positions, timing, snippets, and subscription deltas. P6 as written
("enforced at query time" via UDS creds) was unimplementable on macOS.

**Resolution — authorization becomes T0 architecture:**

- **Principal model with trust tiers.** (a) *First-party trusted clients* (`rsdfind`,
  `rsdctl`, future GUI) — verified by code-signing identity over **XPC with audit
  tokens**, receive the user's full index. (b) *Third-party clients* — receive
  **nothing by default**; access is granted as explicit, user-approved, revocable
  **scope capabilities** ("app X may query ~/Documents/Contracts"), stored and
  auditable, checked before candidate generation. (c) *MCP surface* — its own
  principal with user-configured scopes, off by default outside first-party agents;
  every grant visible in `rsdctl`.
- UDS remains the transport for the trusted CLI tier during development; **the
  identity-bearing surface is XPC** (audit token → code-signing identity). The earlier
  "XPC later, maybe" stance is reversed: XPC is T0 for any caller that isn't the
  same-product CLI.
- **Visibility applies before aggregation and ranking**, not after: scope filters
  constrain candidate generation itself, so counts, group-bys, rank positions, and
  live-view deltas are computed over the authorized subset only. Provenance traversal
  clips at scope boundaries (edges into unauthorized docs are invisible, not
  redacted-but-countable).
- Directory-level permission/ACL/TCC changes: scope grants are path-rooted, so
  subtree-wide changes are grant-level events, not per-document attribute rewrites;
  standing subscriptions are re-fenced (`Resync`) on any grant change affecting them.
- A threat model document (same-user hostile process; over-curious sandboxed app;
  prompt-injected agent on the MCP surface) becomes a T0 deliverable alongside the
  crash-injection gate.

This is *less* magical than the original claim and *more* ambitious operationally: rsd
becomes the thing that finally gives local search an actual permission model, instead
of pretending one can be inferred.

### 5. Identity: hard links, birthtime, TOCTOU — **Accept**

All three sub-findings are correct. One `DocRecord` with one `path` field cannot
represent a file with two hard links; birthtime is evidence, not a guarantee (and is
unreliable on some volumes); and the event→open→extract→commit window is a real race.

**Resolution — the two-entity model, adopted:**

- **`FsObject`** — the content-bearing node: volume identity + file ID + generation
  evidence (birthtime where reliable, else first-seen fingerprint: size/mtime/hash).
  Owns content hash, extraction state, index membership.
- **`Entry`** — a directory entry: path → FsObject, many-to-one. Owns path-derived
  attributes (name, parent, path-scoped visibility). Unlinking one of two hard links
  removes an Entry; the FsObject and its other Entry are untouched. Temporal history
  records Entry events and FsObject events separately.
- **Symlinks are first-class Entries with their own policy** (indexed as links,
  never silently resolved into their targets), not transparent aliases.
- **TOCTOU discipline:** identity is pinned by `fstat` on the *opened fd* at dispatch;
  extraction results carry that pinned identity + content hash; at commit the
  committer revalidates (does an Entry still reference this FsObject? has a
  later-LSN event superseded this work?) and stale results are discarded or
  rescheduled. This is a cheap check that turns a race into a retry.
- Convergence tests gain required cases: hard-link create/unlink matrices,
  unlink-while-open, rename storms across links, case-folding and Unicode
  normalization (NFD/NFC) collisions, APFS clones (which share content identity but
  are distinct FsObjects — see scope answers below).

### 6. Live views only work for a query subset — **Accept the core; refute one framing**

The critique's list is correct and the doc's "a standing query is a materialized view"
universalism was wrong in exactly the ways enumerated: top-k membership is non-local,
BM25 depends on corpus statistics (so "bit-identical scoring" from a one-doc index was
an overclaim — conceded specifically), transitive graph queries have unbounded delta
fan-in, `NOT`/broad predicates lack finite trigger footprints, time-relative
predicates change truth value without any commit, and deletion needs old-state text
evidence that the shown delta didn't carry.

**The maintainability matrix is adopted as the public, documented contract:**

| View class | Maintenance | Semantics delivered |
|---|---|---|
| Attribute predicates, boolean/unranked lexical membership, simple aggregates (COUNT/SUM/GROUP BY) | Exact point-incremental | Exact membership deltas, p99 < 10ms post-commit |
| Semantic alerts | Threshold match on committed vectors | Threshold semantics, delivered on semantic watermark (see #7) |
| Ranked / hybrid top-k views | Bounded top-k repair: maintain top-k + margin buffer; repair from index on buffer exhaustion; periodic re-query fence | Eventually-exact top-k, bounded staleness, explicit fence LSNs |
| Graph-traversal views | Dependency-aware invalidation of affected frontier; re-query on high-fanout edge changes | Exact after invalidation window |
| Clock-relative predicates (`$time.now`-style) | Scheduled re-evaluation at predicate-derived boundaries | Exact at evaluation ticks |
| Anything else / high-fanout events (grant changes, projection upgrades) | `Resync` | Client re-fetches from a fence |

Old-state evidence: the commit delta carries a reference into CAES for the *previous*
extraction record (retained at least until all subscriptions have consumed the delta),
so leave-events are evaluated against real old text, not against an already-deleted
lexical doc. Frecency/permission-driven re-ranking is classified as view-class
"ranked" and flows through repair/fence, not through file deltas.

**The refuted framing:** the critique describes semantic standing queries as top-k ANN
search "redefined" into per-document threshold matching, implying inconsistency. That
was not a workaround; it is the deliberately chosen semantics, and I'll defend it: a
standing alert ("notify me about anything resembling a resignation letter") is
*intrinsically* a threshold/classification question — "is this new thing similar
enough?" — not a ranking question over a corpus. Top-k semantics for a standing query
would mean a new document displaces an old alert retroactively, which is nonsense for
notifications. One-shot semantic *search* is top-k; standing semantic *alerts* are
threshold. v3 names these as two distinct operators (`semantic()` in queries,
`ALERT WHEN similarity(...) > θ` in subscriptions) so the distinction is contractual
rather than accidental. The critique is right, however, that the doc presented them as
the same thing — that's fixed.

The single-doc matcher survives with a narrowed, testable claim: **bit-identical
tokenization and boolean membership**, property-tested against the on-disk index;
scoring parity is explicitly out of scope for it.

### 7. Two contradictory semantic commit timelines — **Accept**

Correct: §4.5 (async, minutes), §5 (enqueue at commit), and §7 ("already embedded on
the ingest path") cannot describe one timeline, and a semantic alert cannot fire
before its vector exists. This was an internal inconsistency, full stop.

**Resolution:** a second durable commit with its own watermark. Every doc/chunk state
is `(catalog_lsn, semantic_gen)`. The commit pipeline emits **two delta streams**:
the document delta at lexical/catalog commit (p99 < 10ms target applies *here*, to
attribute/lexical/aggregate views), and a semantic delta when the vector batch
durably commits (target: seconds on AC, minutes budgeted on battery — surfaced, not
hidden). Semantic alert subscriptions fence and resync on the semantic watermark.
Hybrid one-shot queries already handled the lag via `semantic_gen` compensation; now
subscriptions do too.

---

## Major risks

### 8. FSEvents/ES absolutism — **Accept**

Conceded on both counts. The comparison table's "dir-granularity" ignored
`kFSEventStreamCreateFlagFileEvents` (which rsd should and now does request —
accepting the higher event volume, which the bounded-queue design absorbs by
construction), and item-rename events are indeed unpaired hints requiring inode
reconciliation with races. ES "exact per-file events" becomes "high-fidelity events
with process attribution, treated — like every source — as a hint stream with gap
detection (sequence numbers, extension restart epochs), backed by reconciliation."
The ES distribution dependency (restricted entitlement, user approval, FDA) is
elevated from "coding spike" to a **product risk** in the risk register: the sentinel
tier ships dev-signed for development and is treated as a feature flag that may never
be broadly distributable — which is precisely why nothing correctness-critical sits
on it.

### 9. Provenance epistemics — **Accept enthusiastically**

This is the critique at its best, and it makes the feature stronger rather than
smaller. Hash equality ≠ copying ≠ direction; a writing process is a mutator, not an
author; quarantine xattrs are removable claims. A lineage graph that returns
confident-looking false edges is worse than no graph — it would poison the MCP
surface with fabricated provenance for agents to repeat.

**Adopted:** the graph stores **observed facts with evidence, source, time, and
confidence**; edge vocabulary split accordingly — `SameContentAs` (fact: hash
equality), `LastWrittenBy` (fact: ES observation), `ClaimsDownloadedFrom` (claim:
xattr), `References` (fact: extracted link). `CopiedFrom` / `DerivedFrom` become
*inferences* computed from conjunctions (same content + ES-observed
read-of-A-write-of-B by one process + temporal order ⇒ high-confidence `CopiedFrom`),
always carrying their evidence chain. RQL exposes confidence
(`DERIVED FROM x MIN CONFIDENCE 0.8`), and MCP provenance responses include evidence,
so an agent can cite *why* the edge exists.

### 10. Extraction cache under-keyed — **Accept**

Correct, and the WIT signature's own `hints` parameter proved it. Adopted: extraction
output is split into **content-derived facts** (keyed by
`(content_hash, extractor_id+version, canonical_hints_hash, host_abi_version)`) and
**instance-derived facts** (path/name/xattr/archive-member-derived attributes,
computed per Entry, never cached across identities). Chunking parameters, OCR
language settings, and embedding model revisions join their respective cache keys.
"Reuses extraction verbatim" is scoped to content-derived facts only.

### 11. "Not indexable is a bug" — **Partial accept**

The hostile-input catalog is accepted wholesale into the extraction contract: caps on
input bytes, decompressed bytes, compression ratio, recursion depth, member count,
output text/chunk/reference volume, wall time, and scratch space; partial extraction
as a first-class result; typed status/reason codes (`EncryptedContent`,
`PasswordRequired`, `CloudPlaceholder`, `ResourceBudgetExceeded`, `Unsupported`,
`Corrupt`) that are **queryable attributes** — `kRSDIndexState == "encrypted"` is
itself useful search surface. The critique is also right that WASM fuel doesn't bound
host-side decompression; container walking happens under the same budgeted, sandboxed
worker discipline as parsing.

The refuted part is the reading of the slogan. "Not indexable is a bug, not a
category" was aimed at *neglect* — screenshots, audio, code symbols left unindexed
because nobody bothered — not at a denial that encrypted bytes exist. v3 rewords it:
**"unindexed by neglect is a bug; unindexable by policy or physics is a labeled,
queryable state."** The ambition (OCR everything, transcribe everything the user
permits) is unchanged.

### 12. Multi-plane commit needs a real state machine — **Accept**

Agreed — §5 step 5 was a sentence where a specification must be. The questions the
critique lists (durability order of extraction output vs. log append, cursor
advancement, idempotent replay, cross-plane read fences, delete-before-add safety
under replay, vector watermark meaning with outstanding embedding work) are exactly
the spec. v3 gains a **per-LSN projection state machine**:
`Observed → Journaled → ExtractionDurable(CAES) → CatalogApplied → LexicalApplied →
GraphApplied → [async] SemanticApplied`, with: journal-before-apply ordering; CAES
write durable before dependent plane writes; idempotency keys
`(lsn, doc_id, plane, projection_version)` making every apply safely re-runnable;
source cursors advancing only past fully-journaled events (#1); and queries reading
behind a fence of `min(plane watermarks)` by default with an opt-in
`ALLOW STALE(plane)` for freshness-tolerant callers. The redb previous-root claim is
downgraded to a **verification obligation** in spike #2 (transaction crash recovery ≠
arbitrary reachable-page repair — if scrubbing finds damage redb can't serve, the
repair path is catalog-plane rebuild per the ladder, and that path gets tested, not
assumed).

### 13. Performance targets — **Partial accept**

Adopted: the **benchmark matrix** (named corpora: monorepo source tree, office/PDF
corpus, photo library, media library, adversarial archive set; named hardware tiers;
defined cache states, concurrency, query mixes, background-load conditions), reporting
absolute bytes-per-doc/chunk alongside ratios. Conceded outright: percentage-of-corpus
index-size targets are misleading for small-file corpora (per-doc overhead dominates)
and vector size scales with chunk count, not source bytes; and "unmeasurable
(< 0.5%/day)" was self-contradictory — it becomes a measurement protocol
(powermetrics delta over a defined 24h idle scenario, threshold < 0.5%).

Retained, mildly refuted: the critique says the numbers "are not yet acceptance
criteria," which is true today — but publishing design-to targets *before* a baseline
exists is intentional discipline, not overreach; they are the numbers the architecture
is sized against and they become acceptance criteria the day the benchmark matrix
lands. v3 labels the table "design targets, hardened into acceptance criteria by the
benchmark matrix (spike #7)."

### 14. mdfind compatibility promise — **Accept**

The doc did contradict itself (crate table: "Spotlight-compatible subset"; §6.1:
"strict superset… identical semantics"), and exact equivalence with an opaque,
locale-sensitive, importer-dependent implementation is unprovable. Adopted: a
**versioned supported grammar** covering the practically-used predicate surface, a
published compatibility corpus (differential-tested against real `mdfind` — as a
discrepancy-finder, with no equivalence claim), documented known divergences, and a
clean `UnsupportedPredicate` error rather than silent misinterpretation. `rsdfind`
remains flag-compatible.

### 15. The Spotlight polemic — **Partial accept**

Conceded and corrected: `NSMetadataQuery` does provide an initial-gather + live-update
phase with result-change notifications; "re-poll-ish" was wrong, and the comparison
table is fixed to say what's true — live updates exist but are batched at
notification-interval granularity, limited to Spotlight's attribute/predicate model,
with no semantic, temporal, provenance, or aggregate-view capability. Also conceded:
"strictly better mds," "crash-proof," and "most capable ever" are claims that belong
*after* measurement; v3 replaces them with the falsifiable versions (the failure-matrix
and benchmark commitments).

Retained: the thesis. Accurate comparison strengthens rather than weakens it — a
twenty-year-old predicate model over current-state metadata, an unsandboxed crashy
importer ABI, unobservable index recovery, and zero agent surface is a fair
characterization of the gap rsd exists to close, and none of the critique's
corrections touch it.

---

## Scope decisions (preliminary T0 positions)

The critique is right that these change identity/convergence/security/scheduling and
must be answered, not deferred. Positions to be ratified in v3:

- **Default roots & consent:** nothing indexed without onboarding consent. Default
  offer: `~` minus exclusions. TCC-protected folders and FDA requested explicitly with
  per-scope explanation; declining a scope excludes it cleanly.
- **Cloud placeholders (File Provider):** metadata-only by default; never trigger
  downloads implicitly; `kRSDIndexState == "cloud-placeholder"`; opt-in
  download-on-index per scope.
- **Network volumes:** out of T0 entirely (no FSEvents fidelity, weak identity
  guarantees). Revisit with a polling+anti-entropy-only mode later.
- **APFS clones:** distinct FsObjects sharing content identity via `by_hash` /
  `SameContentAs` — extraction and embeddings shared, identity not.
- **Hard links/symlinks/case/normalization:** per critique #5, specified in the
  two-entity model and convergence test matrix. Packages/bundles: indexed as trees,
  presented as single results by default (`kMDItemContentTypeTree`-style), expandable.
- **Archive members:** first-class FsObjects with `ExtractedFrom` Entries in T1+;
  identity = (archive FsObject, member path, member content hash); archive edit =
  member-set diff.
- **Deliberate exclusions (default deny, configurable):** keychains, browser profile
  databases and cookies, `~/.ssh` and credential material, app container private data
  requiring TCC the user hasn't granted, `.git/objects` and dependency trees
  (`node_modules`, `target`, `.venv`) — the last group excluded from *content*
  extraction but present in the catalog so scoped queries still see the files exist.
- **Encryption at rest:** T0 relies on FileVault + `0700` index directory +
  `NSFileProtection` where applicable; index-level encryption (sealed while session
  locked) is a T2 item with real key-management design, not a checkbox.
- **Migrations:** owned by `rsd-daemon` at startup; versioned manifest per plane;
  projection-version machinery from #2 doubles as the upgrade path; failed migration
  = keep serving old generation read-only while rebuilding, never brick the index.

## The T0 reframing — **Accept ~90%**

The critique's narrowed T0 (approved scopes; events as acceleration + reconciliation
as correctness; crash-consistent catalog+lexical with bounded-staleness watermarks;
documented predicate subset; documented live-maintainability classes; metadata-history
first; trusted-client-only IPC; full crawl rare-observable-not-impossible) is accepted
nearly verbatim — it is what T0 should have said, and as the critique itself notes,
it gives up rhetoric, not product.

The 10% I keep against the grain of the review's tone: **the ambition tiers are not
descoped, and the doc will not read like an apology.** T1–T3 (semantic, WASM, MCP,
temporal, provenance) remain the point of the project; what this revision changes is
that each ambitious capability now states *exactly* what it guarantees, on which
store, under which failure modes — which makes the bold claims stronger, because they
become falsifiable instead of aspirational. The critique demonstrated that the fastest
way to lose a leapfrog project is to let its guarantees outrun its data model. The
fix is better data models, not smaller goals.

## Errata acknowledged (meta)

Pattern conceded: the v2 doc repeatedly converted engineering targets into universal
invariants ("never," "any," "bit-identical," "every," "exact"). Six of the seven
blocking findings trace to that single habit. v3 adopts a drafting rule: **every
guarantee names its authority, its failure mode, and its test.** Claims that can't,
get demoted to targets.

Actions: DESIGN.md v3 will fold in — the four-authority model (#1), CAES plane and
projection versioning (#2), layered temporal guarantees (#3), the T0 authorization
architecture (#4), the FsObject/Entry identity model (#5), the live-view
maintainability matrix (#6), dual watermarks (#7), evidence-carrying provenance (#9),
split cache keying (#10), the extraction limit contract (#11), the commit state
machine (#12), the benchmark matrix framing (#13), the versioned grammar (#14), and
the corrected comparison table (#15). Spike list gains: authorization threat model
(new, T0), redb damaged-page behavior verification (spike #2), ES distribution risk
(product risk register).
