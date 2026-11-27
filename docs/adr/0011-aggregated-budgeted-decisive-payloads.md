# Verb payloads are aggregated, budgeted and decisive

Status: accepted. Refines the response shape of [ADR 0003](0003-verb-contract-not-query-language.md): the Verb set is unchanged, and this ADR fixes what each Verb returns and how much.

The navigation claim is that an agent orients through bounded Verb responses instead of reading files. Today it does not hold. The [token-efficiency research](https://github.com/randax/yggdrasil/blob/d0a86b57a2ef094d629188caa4ba1dac20ac0748/docs/research/agent-token-efficiency.md) found that payloads carry identity and no answer, so an agent reads the file anyway and Verb bytes add to file reads instead of replacing them. The [Shard profile](https://github.com/randax/yggdrasil/blob/563e9b4c3b6a42e9583452055150a2154173b0c3/docs/research/shard-profile.md) found where the bytes go:

- the default `neighbors` page from etcd's top Symbol is 1.44 MB, about 360k tokens, holding 4,356 edges but only 395 distinct `(src, dst, kind)` triples;
- 94% of CALLS edges sit at call sites with more than one same-named candidate;
- node-id strings are 22–54% of payload bytes, and the cross-repo `search` cursor is 6 KB of a 12.8 KB response.

Every peer that does better converges on the same few techniques: compact rendering, short labels, ranked output cut to a budget with a stated truncation, tiered detail, and a few higher-level calls. This ADR adopts them.

## The rules

### Edges are aggregated

- `neighbors` returns one edge per `(src, dst, kind)`. An edge that stands for several call sites carries `sites: n` and the first few (about 3), each with its line number and the line's text. `detail: sites` returns every site, paged.
- Page and budget limits count aggregated edges.

### Ambiguity is ranked and counted, never dropped

- At a call site with several same-named candidates (ADR 0006), a payload returns the best few (about 3), ranked by confidence and then locality (same package, imported scope, same repo), and states `+N other candidates` with their highest confidence. `detail: candidates` returns all of them.
- The Shard still stores every candidate, and `impact` still counts them in its reach, marked as low-confidence. Only what a payload shows by default changes.

### Identity is stated once

- A response states its repo context once, and ids inside it are repo-relative (`sym:server/etcdserver/api/rafthttp/stream.go#streamWriter.run`). A cross-repo response groups results by repo.
- Fields an id already encodes are not repeated: no separate `name`, `path` or `kind` beside an id that carries them, no `SearchHit.repo`, no null cursors. A line number is kept, because ids do not carry it.
- Cursors are short opaque handles kept on the server with a time to live, bound to the caller's effective-view fingerprint (ADR 0008), never self-describing blobs.

### Payloads carry the text that ends the search

- Every Symbol a Verb returns (from `node`, `neighbors` or `search`) carries its declaration line and line number, for example `func (w *streamWriter) run()`. `detail: doc` adds the first sentence of its doc comment.
- Call sites carry their line text (above). Search snippets are plain text with line numbers, never HTML-escaped fragments.
- The Shard therefore keeps each Symbol's declaration line and each call site's line text; how is decided with the rest of the Shard layout.

### Every list is budgeted and ranked

- `neighbors` and `search` take a `budget` in tokens: 4,000 by default, 20,000 at most. The server estimates tokens from the rendering it returns.
- The seed comes first and is never cut. After it, results are ordered by hop, then Provenance and confidence, then a penalty for high-degree nodes, then id.
- At depth 2 and beyond, a node above a degree threshold is not expanded. It is listed as a hub with its edge count and the call that would expand it, so a deep traversal never returns hundreds of nodes with no edges.
- Truncation is always stated: how many nodes and edges were cut, and the budget that would include them all. The cursor continues in the same order.

### One call can describe a Symbol

`node` takes `include: [callers, callees, tests, exports, owners, history, memories]` and returns a card: the Symbol's signature and doc summary, then each requested section, ranked and cut under one shared budget, each with its own "+N more" and the call that fetches the rest. Sections whose data arrives in a later milestone (owners and Memories in M2) are accepted and empty until then. Memories in a card obey the same Audience and visibility rules as `recall`.

### MCP gets one compact rendering

- MCP tools declare no `outputSchema` and return a single text rendering, line- and tree-oriented, with the repo stated once, signatures and call sites inline and the truncation notice last. There is no `structuredContent` copy.
- REST and `yg … --json` keep the typed JSON for programs. The CLI's human output is the same rendering as MCP's, and the Skill's examples drop `--json`.
- The rendering and the JSON are two views of one typed response, produced at the transport boundary.

### Order of work

These wire changes land first in M1, before `callers`, `callees`, `impact` and `map`, so those Verbs are born in this shape. The defect "neighbors direction filter drops callers on hub nodes" is fixed before them, since every caller measurement depends on it. The map's milestone step may move the slot, not the order.

### Measuring it

`docs/EVAL.md` fixes the yardstick and sets two tiers of targets: deterministic checks in CI, and an agent-in-the-loop run at release on the Shard profile's corpus, shared with Ask's eval (ADR 0010).

## Considered options

- **One edge per call site, with a smaller page.** No wire change, and the elevenfold redundancy on hubs reaches every agent.
- **Aggregated edges with no sites.** Smallest, but "where exactly" needs the file read this ADR exists to remove.
- **Every ambiguous candidate, or a confidence floor.** The first is where most bytes go; the second silently drops real edges, which ADR 0006 chose not to do.
- **Full ids with only duplicated fields dropped, or compressed self-describing cursors.** Both keep kilobytes the profile measured.
- **Signatures on `node` only, or no content.** Traversals stay identity-only, and grep keeps winning.
- **A byte budget in id order, or lower count caps.** The relevant callers can still fall past the cut, or one hub page still floods the context.
- **Both text and `structuredContent` over MCP, or JSON only.** A client that forwards both pays twice; JSON punctuation costs tokens no model needs.
- **A separate `explain` Verb.** Duplicates `node` against ADR 0003's small Verb set.
- **A deterministic eval only, or an agent-in-the-loop eval only.** The first never measures accuracy per token; the second is too slow and noisy to guard every change.

## Consequences

- Every Verb response changes shape, breaking every client; all of them are ours (the repository's breaking-changes default).
- The server ranks and budgets each response and holds cursor state.
- The Shard grows by declaration lines and call-site line text, small next to the CALLS-edge bytes the profile measured; "Storage and indexing direction" owns the layout.
- Two renderings (MCP text and JSON) must stay in step; both come from one typed response.
- Ask (ADR 0010) and the PR impact report consume the same shapes and budgets.
