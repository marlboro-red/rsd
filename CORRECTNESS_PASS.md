# rsd — Correctness & Honesty Pass

Status: plan · 2026-07-17 · consolidates two independent code audits
(catalog/log/daemon-core deep dive + full-surface review). Supersedes feature
work until the Critical and High items land.

## Why this exists

Two reviewers, working independently, converged on the same conclusion: the
storage kernel (journal, catalog identity, metrics cardinality, worker seal) is
genuinely solid, but a meaningful set of DESIGN.md's *guarantees* are library
code exercised only by tests, or don't exist at all — while the docs and commit
messages presented them as shipped. By the design's own binding drafting rule
("every guarantee names its authority, failure mode, and test"), several of
those are **targets mislabeled as done**.

This pass has two jobs, in priority order:

1. **Fix the security and durability defects** that make real guarantees false
   (authorization, crash recovery, content identity, the loopback token).
2. **Reconcile the claims** — make DESIGN.md, IMPLEMENTATION.md, and the gate
   names describe what the code actually does; downgrade the rest to labeled
   targets.

A guiding rule for the whole pass: **a fix isn't done until a gate would catch
its regression.** Most of these defects survived precisely because the gate that
should have caught them tests something narrower than its name.

---

## The through-line: gates narrower than their names

This is the root cause, and fixing it is the highest-leverage work here.

- The **500-kill crash gate** drives `crash-child`, which exercises
  `Journal::append` + `Catalog::apply_changes` + `CursorStore` over `synth`
  ops. `synth` emits only `Upsert`/`RemovePath` — `commit.rs` literally has
  `unreachable!("synth emits no SetContent")`. So the gate tests **the catalog
  plane only**: no content, no CAES, no lexical/vector plane, no FSEvents, no
  restart of the real pipeline. This is why C-RECOVER and C-HASH below went
  unseen.
- The **convergence gate** calls `bring_up(..., None, None, None, None, ...)`
  — every plane and the live engine are `None`, and `trickle_bootstrap` is
  `false` (the daemon runs `true`). It gates catalog⇄filesystem convergence,
  never across a restart.
- The **"leak suite"** is one scoped principal, one count, one query, one live
  event, with a *non-empty* grant. It never tests the empty-grant escalation,
  the unknown-principal default, or prefix-boundary confusion — which is
  exactly where the authorization holes live.
- **Silent CI skips**: the OCR/embed/transcribe gate tests `return` cleanly
  when their helper env var is unset; a Swift toolchain hiccup converts three
  gates into no-ops with a green check. `perf.rs` targets are `#[ignore]`, so
  no §15 latency number is enforced anywhere.

**Do this first, because everything else regresses invisibly without it:**
give `synth` a `SetContent` arm and assert lexical+vector equality (not just
`catalog.listing()`) after the kill storm; add a restart case to the
convergence gate; and make the helper-gated tests **fail loudly** when CI just
built the helper but the env var is missing.

---

## Critical (security / durability — a stated guarantee is currently false)

### C-AUTHZ-1 — Authorization is default-allow
`ipc.rs` `AuthzStore::scopes()` returns `None` for any principal not in the
map; `None` means *unrestricted*. The daemon builds an empty
`AuthzStore::default()` for both the UDS and HTTP surfaces. The `Hello
{ principal }` string is caller-asserted and unverified. Net: any same-uid
process gets the whole index by naming an unknown principal. DESIGN §11 opens
by stating same-uid is insufficient and promises "nothing by default"; §16
spike 3 makes this a T0 gate ("no third-party access ships before this
passes"). The shipped posture is the inverse.

**Fix:** default-deny. An unlisted principal gets no access. Introduce a single
`Scope` type (below) so "unrestricted" is an explicit, auditable grant, not the
absence of a map entry.

### C-AUTHZ-2 — A deny-all grant becomes full access on the live path
`Some(vec![])` (explicit revoke-everything) means *no access* for queries
(`authorized()`'s `.any()` over `[]` is false) but *unrestricted* in the live
engine (`granted()` = `grants.is_empty() || …`). `ipc.rs` passes
`grants.clone().unwrap_or_default()` into `subscribe`/`subscribe_alert`,
collapsing `Some(vec![])` → `vec![]`. So granting a principal nothing yields
zero query results and a **live feed of every enter/leave with full paths
across the entire index**. Two crates assign opposite meanings to `vec![]`.

**Fix (shared with C-AUTHZ-1):** replace `Option<Vec<String>>` end-to-end with
```rust
enum Scope { Unrestricted, Paths(Vec<PathPrefix>) }  // empty Paths = deny
```
so the type makes the sentinel collision unrepresentable. `rsd-live`'s
`granted()` and `ipc`'s `authorized()` must consume the same type.

### C-AUTHZ-3 — Prefix grants have no path-boundary check
`path.starts_with(grant)` authorizes `/Users/x/Documents-private/` under a
grant for `/Users/x/Documents`. §11 says grants are "path-rooted." The one
test that exists dodges this by granting a trailing-slash path.

**Fix:** compare on path components, or require the grant to end in `/` and
match `path == grant || path.starts_with(grant_with_sep)`. Test sibling-prefix
confusion explicitly.

### C-RECOVER — Crash recovery never restores the lexical or vector planes
`Committer::recover()` replays the journal into the **catalog only**. Both
planes carry their own watermarks and idempotent, watermark-guarded `apply` —
nothing drives them past a crash. `rsd_lexical::rebuild()` exists and is dead
code outside its own test. And the gap is **permanent**: on restart Gate 1 in
the dispatcher skips re-extraction because the catalog already has
`index_state.is_some()` at unchanged size/mtime, so no `SetContent` is emitted
and the plane is never refilled. A crash between the catalog txn and the
tantivy commit drops those docs from search forever. Steady-state plane-apply
errors are logged `"… plane lags, rebuildable"` and dropped — nothing rebuilds
them. DESIGN §6.8 row 1 promises "replay from `min(watermarks)`"; `min()`
appears nowhere.

**Fix:**
- `recover()` (and `bring_up`) must replay from `min(catalog, lexical, vector)`
  watermarks and apply to each plane past *its own* watermark, sourcing content
  from CAES (not the filesystem).
- On plane-apply error in `commit()`, mark the plane degraded and schedule a
  rebuild — do not advance as if it succeeded.
- Switch the daemon's catalog open to `open_catalog_resilient` (today only the
  crash-child uses it; `main.rs` uses the non-resilient open and simply fails
  to start on the exact failure the resilient path was written for).
- **Gate:** the extended crash gate (above) must reconstruct a fresh lexical +
  vector plane from journal+CAES and assert search equality — not just catalog
  equality.

### C-HASH / C-TOCTOU — Content identity is forgeable
`hash_file` hashes only the first `max_input_bytes` (32 MiB) and the result is
stored as `content_hash` everywhere; size is **not** in the CAES key
(`hints_hash` is only extension+truncated+processor). Consequences:
- An append-only file > 32 MB freezes its indexed content at first sight
  forever (Gate 1 notices the mtime change, re-hashes the same prefix, hits
  CAES, re-commits stale text).
- `B = A[..32MiB] || anything` inherits `A`'s extracted text — a controllable
  poisoning primitive.

Compounding it, §5's TOCTOU discipline (fstat-pinned identity, commit-time
revalidation) is **entirely absent**: the file is opened twice independently
(once to hash, once in the worker to extract), nothing fstats either fd or
compares against the resolved `file_id`, and `commit()` revalidates nothing.
A file rewritten between hash and extract stores B's text under hash(A),
durably, keyed for every future copy of A. `Change::SetContent` applies by
path with no identity check.

**Fix:**
- Hash the **whole file** (streaming, no cap) — or, if a cap is kept for
  pathological files, put `(full_size, mtime)` in the CAES key and rename the
  field to `content_prefix_hash` so no caller mistakes it for whole-file
  identity.
- Open the file **once**, `fstat` the fd, and pass that one fd to both the
  hasher and the worker (the worker protocol already passes an fd — thread it
  through instead of re-opening by path).
- `commit()` revalidates the pinned `(file_id, generation)` before applying
  `SetContent`; on mismatch, discard and reschedule (the "races become retries"
  the design promises).

### C-TOKEN — Loopback token: silent all-zero key + non-constant-time compare
`gen_token()`'s comment claims a "time-mixed fallback if `/dev/urandom` fails";
**no such fallback exists**. The error from `open`/`read_exact` is discarded
with `let _ =`, so on any failure the token is a constant
`"000…0"` and the daemon starts and logs success. The compare is
`presented != token` — length check + early-exit memcmp, not constant-time;
over loopback with unlimited un-rate-limited attempts, byte-at-a-time timing
recovery of a 32-hex token is feasible, and §15's leak suite explicitly
promises "zero information flow … (timing classes)."

**Fix:** use `getentropy(2)` (or fail closed and refuse to start the HTTP
surface if entropy is unavailable); delete the false comment; compare with a
constant-time equality. *(Credit: the token gate is correctly placed before the
route match, so the SSE endpoints are genuinely covered — verify a regression
test pins that.)*

---

## High (correctness — silent divergence, panics, or resource unboundedness)

### H-APPLIER-SUPERVISION — A poisoned mutex freezes indexing, canary stays green
The applier is the single writer and `.lock().unwrap()`s several mutexes per
commit (vector plane, live engine, stats). One panic while any is held poisons
it; every subsequent commit panics; the applier thread dies. Nothing supervises
it — the handle is only `join()`ed at shutdown. The daemon keeps serving stale
queries, the status line keeps printing, and `full_rescans` (the §18.1
"convergence canary") stays at 0 while convergence has stopped entirely.
Per-connection query handlers then panic on the same poisoned lock.

**Fix:** supervise the applier (detect `is_finished`, restart or raise a loud
`health.applier_down` metric/flag); use poison-recovering locks
(`.lock().unwrap_or_else(|e| e.into_inner())`) on the commit path; add a
health signal the HUD and `doctor` read.

### H-SIDECAR-DEADLINE — The embedder request path has no timeout; failure poisons vectors
The recent handshake fix bounded `spawn`'s READY read but **not**
`one_shot`'s steady-state `read_exact`. A helper that accepts the write and
never answers blocks `embed()` → `VectorPlane::apply` → `commit()` → the applier
thread, forever. Separately, the `vec![0.0; dim]` degradation stores a **real
null embedding** in the plane and advances `semantic_gen`, silently poisoning
semantic search and permanently disabling any alert whose query embeds to zero;
and a respawn that returns a different `dim` is discarded (`Ok((p, _))`) →
"dim mismatch" on every call thereafter.

**Fix:** read deadline on `one_shot` (respawn on timeout); make the
zero-vector path a typed, counted, **non-persisted** failure (skip the write,
leave `semantic_gen` unadvanced, retry later); honor the respawned helper's
reported dim.

### H-PROCESSOR-VERSION — Stale extractions served forever after a processor change
The CAES key's `extractor_id`/`extractor_version` are hardcoded to the
**native** extractor's constants regardless of which processor ran; the only
real discriminator is the constant `processor_tag` ("wasm"/"ocr"/"stt") folded
into `hints_hash`. Upgrade a WASM plugin, swap plugins for the same extension,
or change OCR language — the key is unchanged and every previously-seen file
serves the old output forever, with no invalidation path. §10.2 requires
`(content_hash, extractor_id+version, canonical_hints_hash, host_abi_version)`
and names "OCR language settings, embedding model revisions join their
respective cache keys."

**Fix:** each `ContentSource` reports its real `(id, version)`; use those in the
CAES key instead of the native constants. Route once and derive the key from
that single decision (see H-ROUTE-DUP).

### H-ROUTE-DUP — Routing precedence is duplicated in two places that must agree
`route()` and the `processor` derivation feeding the CAES key are independent
copies of the wasm > ocr > media > default chain; a comment admits the coupling.
If they drift, the key names one processor while another produces the bytes →
wrong text cached under a mislabeled key, silently and durably. (Adding
`media` this session required editing both — it happened to be correct.)

**Fix:** `route()` returns `(source, ProcessorKey)`; the CAES key derives from
that one return. Delete the parallel derivation.

### H-REMOVE-PLANES — Deletions never leave the lexical/vector planes
`LexicalPlane::apply` handles only `SetContent`; `RemovePath` is ignored.
Deleted files' docs remain in tantivy indefinitely — masked at query time by
catalog re-resolution, but they consume the top-k budget (a query whose top-k
are all deleted returns nothing), the index grows unbounded, and the plane is
not "a projection of current state" (§1).

**Fix:** `apply` deletes the doc on `RemovePath` and on content-invalidating
`Upsert`. Add a test asserting a deleted file's oid leaves the plane.

### H-JOURNAL-MIDROT — One flipped bit in the active segment bricks startup
`Journal::open` truncates the active segment to the first invalid byte on any
corruption ("torn tail expected"). But `scan_segment` stops at the first bad
byte *anywhere*, so mid-segment bit-rot discards every valid record after it and
rewinds `next_lsn`. Then `recover()` finds `catalog_watermark > journal_max` and
returns an `Invariant` error → the daemon **refuses to start**. §6.8 promises
"no plausible single failure escalates beyond scoped repair"; one bit here
escalates to a non-booting daemon with no repair path and no matrix row.

**Fix:** distinguish torn tail (truncation at an EOF-adjacent offset) from
mid-segment rot (hard error → scoped repair, like sealed segments). Add the
matrix row and a corruption-injection test.

### H-BOUNDED-MEMORY — Several O(n) growths violate P4
- `recover()` drains the entire replay tail into one `Vec` before chunked apply
  — O(journal) RAM on a long-offline restart.
- `ContentIndexer::failures` (`[u8;32] → u32`) only shrinks on success/quarantine
  — O(distinct-failing-content).
- `/api/search?limit=…` is unclamped → a full scan + huge `Vec::with_capacity`.
- Pre-auth unbounded header read + unbounded thread-per-connection in both
  `start_ipc` and `start_http`; SSE connections pin a thread with no cap.

**Fix:** stream `recover()` in bounded windows; cap/evict `failures`; clamp
`limit`; cap header bytes *before* the auth check and bound concurrent
connections.

---

## Medium (real bugs, narrower blast radius)

- **M-SNIPPET-PANIC** — `snippet()` computes an offset in the lowercased string
  and slices the original; lowercasing isn't length-preserving (e.g. `İ`), so
  `start > end` panics and kills the connection thread. Search the original
  string case-insensitively instead of mapping indices across two strings.
- **M-TOKENIZER** — the single-doc matcher's query side is `split_whitespace`,
  not the tantivy analyzer; §9's "bit-identical tokenization, property-tested"
  is false (the on-disk plane shares the same half-applied mistake, so they
  agree *by shared bug*). Run query terms through the same `TextAnalyzer`; add
  the promised property test (§16 spike 4).
- **M-STARTUP-EXPECT** — `main.rs` `.expect("vector plane")` panics daemon
  startup on a corrupt vector store — opposite of the disposable-projection
  posture. Open resiliently; rebuild on failure.
- **M-HTTP-AUTHZ-FILTER** — `/api/search` performs no scope filtering at all
  (unlike the UDS `run_query`). Today the HTTP surface is first-party-only, but
  once scopes exist this is a hole. Enforce the same authz on both surfaces.
- **M-AUTHZ-POSTFILTER** — authorization is a post-filter over a 10k-limited
  result set, not candidate-generation enforcement (§11). A scoped principal's
  results are silently truncated by unauthorized docs consuming the budget;
  `Count` is wrong above 10k; latency scales with the whole corpus (a timing
  channel on total size). Constrain candidate generation to the granted scope.
- **M-QUARANTINE-VOLATILE** — the failure counter is in-memory and resets each
  restart, so a worker-crashing file gets fresh retries every boot. Persist the
  quarantine decision (it already lands in CAES on quarantine — gate retries on
  that).
- **M-DIR-FSYNC** — atomic `write tmp → rename` never fsyncs the parent dir, so
  the rename isn't durable across power loss (seal manifest, cursor, token). The
  "written atomically" comments overstate it.

---

## Documentation honesty (cheap; the drafting rule requires them)

DESIGN.md presents these as shipped; they are targets. Relabel with the
authority/failure-mode/test triple, or mark "target — not yet wired":

- **§7.3** "per-LSN projection state machine" + "two delta streams" — `commit()`
  is a flat synchronous function (journal→catalog→lexical→vector→hook); no
  states, no `SemanticDelta`, no async second timeline, no `min()` read fence,
  no `ALLOW STALE`. §18.2 already models the right honesty for the sync-embed
  admission — apply it here.
- **§6.5** "embeddings generated **asynchronously** behind lexical commit" —
  they're synchronous inside commit (this is *why* `commit_ms` p50 ≈ 7ms).
- **§6.1** cursor fencing — `CursorStore` is real but wired only into the
  crash-child; the daemon watches with `since: None` (SINCE_NOW) and recovers
  downtime changes only via the (trickle-paced) bootstrap walk. "An acknowledged
  event is never lost" is untrue in the shipping daemon.
- **§6.8** "crash/**corruption**-injection CI" — no bytes are ever corrupted;
  it's a kill-injection gate. Add real corruption injection or rename the claim.
- **§11** — note enforcement is unconfigured in the shipped binary and the
  surface is default-allow until C-AUTHZ lands.
- **§9** "bit-identical tokenization … property-tested" — see M-TOKENIZER.
- **§15** — the perf targets are `#[ignore]`d; no latency number is gated. Either
  wire one release-mode perf assertion or label the table "design targets,
  not yet gated."
- **Convergence §5** promises NFC/NFD normalization convergence cases; the
  testkit has none.

---

## Suggested execution order

1. **Extend the gates first** (crash gate → content+planes+restart; convergence
   gate → a restart case; leak suite → empty grant, unknown principal, prefix
   boundary; make helper-gated tests fail-loud). Nothing below is trustworthy
   until a regression would be caught.
2. **C-AUTHZ-1/2/3** — the `Scope` enum refactor closes all three at once; the
   extended leak suite proves it.
3. **C-RECOVER** + **H-REMOVE-PLANES** + **H-JOURNAL-MIDROT** — the durability
   cluster; the extended crash gate proves it.
4. **C-HASH / C-TOCTOU** — whole-file hash + single-fstat'd-fd + commit-time
   revalidation.
5. **C-TOKEN** — small, self-contained, high-value.
6. **H-APPLIER-SUPERVISION** + **H-SIDECAR-DEADLINE** — the "silent freeze"
   cluster.
7. **H-PROCESSOR-VERSION** + **H-ROUTE-DUP** — one refactor.
8. **H-BOUNDED-MEMORY**, then the Medium items.
9. **Documentation honesty pass** — do this *as* each fix lands, so DESIGN.md
   converges to truth rather than drifting again.

## What is genuinely solid (don't touch)

Both reviews independently credited: the journal's framing / torn-tail /
sealed-vs-active asymmetry / arbitrary-bytes fuzz; the catalog identity model
(inode-reuse via birthtime, orphan-grace rename pairing, `check_invariants` as a
mirror oracle); `rsd-metrics`'s structural cardinality safety; the sealed worker
(deny-default seal, fd-only, refuses to run unsealed, probe + control group);
and the crash gate's mutual-reconstructability check *for the catalog plane*.
The kernel is trustworthy — the failures are in the wiring above it and in the
claims about it.
