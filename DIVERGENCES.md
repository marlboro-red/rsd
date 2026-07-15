# RQL v1 — Spotlight compatibility posture (DESIGN.md §8.1)

RQL implements a versioned, documented subset of the Spotlight predicate
grammar. Anything outside it fails with `UnsupportedPredicate` — never silent
misinterpretation. Known divergences from `mdfind` in grammar v1:

| Area | mdfind | rsd v1 |
|---|---|---|
| Attributes | full kMDItem* schema incl. importer-defined | kMDItemFSName/DisplayName, FSSize, ContentModificationDate/FSContentChangeDate, TextContent + kRSDIndexState, kRSDSymbols |
| `w` modifier | word-boundary matching | UnsupportedPredicate |
| `d` modifier | diacritic folding | accepted, folding not yet applied (documented gap) |
| `$time.*` | now/today/yesterday/this{Week,Month,Year} ± arithmetic | `$time.now` and `$time.now(±N)` only |
| Text wildcards | `"*foo*"` substring-within-word semantics | token/word membership (default-tokenizer terms); `*` stripped |
| Bare query | Spotlight relevance ranking + content+name match | content-field BM25 hits only |
| Value coercions | locale-sensitive dates/strings | numbers, quoted strings, `$time.now` only |

Differential corpus runs against a live `mdfind` (discrepancy-finder, not an
equivalence proof) are queued for the Phase-5 hardening pass: Spotlight will
not deterministically index throwaway fixture trees in CI, so the harness
needs a persistent pre-indexed fixture on the bench machine.
