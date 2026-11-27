# Verb payloads are aggregated, budgeted and decisive

Status: accepted. Refines the response shape of [ADR 0003](0003-verb-contract-not-query-language.md): the Verb set is unchanged, and this ADR fixes what each Verb returns and how much. Replaces [ADR 0008](0008-team-granted-repository-visibility.md)'s signed cursor with a server-held handle that binds the same things.

The navigation claim is that an agent orients through bounded Verb responses instead of reading files. Today it does not hold. The [token-efficiency research](https://github.com/randax/yggdrasil/blob/d0a86b57a2ef094d629188caa4ba1dac20ac0748/docs/research/agent-token-efficiency.md) found that payloads carry identity and no answer, so an agent reads the file anyway and Verb bytes add to file reads instead of replacing them. The [Shard profile](https://github.com/randax/yggdrasil/blob/563e9b4c3b6a42e9583452055150a2154173b0c3/docs/research/shard-profile.md) found where the bytes go:

- the default `neighbors` page from etcd's top Symbol is 1.44 MB, about 360k tokens, holding 4,356 edges but only 395 distinct `(src, dst, kind)` triples;
- 94% of CALLS edges sit at call sites with more than one same-named candidate;
- node-id strings are 22–54% of payload bytes, and the cross-repo `search` cursor is 6 KB of a 12.8 KB response.

Every peer that does better converges on the same few techniques: compact rendering, short labels, ranked output cut to a budget with a stated truncation, tiered detail, and a few higher-level calls. This ADR adopts them.

## The rules

### Everything is computed on the caller's view

Every count, `sites: n`, `+N` figure, degree, hub decision, ranking penalty, candidate denominator, traversal work cap, early-stop decision and budget estimate below is computed after the caller's effective view has been applied (their view, intersected with any reader ceiling and egress set, ADR 0008 and ADR 0010), never from a total precomputed in the Shard. A node whose visible degree is under the hub threshold is expanded normally. Traversal work caps count only edges and sites in the effective view, so hidden edges can never move where a traversal stops. The same query run with and without an extra hidden repo that has edges into the result must render byte for byte the same once cursor handles are replaced by a placeholder, and must return the same items across all pages; that is a Tier 1 check, including a scenario whose hidden edges alone would exceed the work cap.

### Edges are aggregated

- `neighbors` returns one edge per `(src, dst, kind)`. Distinct call sites are distinct `(line, column)` locations (ADR 0006). An edge that stands for several sites carries `sites: n` and the first few (about 3), each with its line number, column, line text, Provenance and confidence. `detail: sites` returns every site, paged under the budget.
- Provenance and confidence filters in the request apply to the evidence first, including evidence with no location (an `IMPLEMENTS` edge inferred from method sets). The surviving located evidence is then grouped by site, so a site seen by both the precise and the syntactic pass is one site, shown with its strongest surviving observation (at a site the precise pass resolved, the Shard keeps the precise evidence plus only the syntactic candidates declared in files the build never indexed, [ADR 0012](0012-shard-layout-and-incremental-indexing.md)). An aggregated edge's Provenance and confidence are those of its strongest site, and it states how many of its sites are at each Provenance ("5 sites: 2 precise, 3 syntactic"), which always sums to `sites: n`. An edge supported only by evidence with no location is kept, with `sites: 0` and the Provenance and confidence of that evidence.

### Ambiguity is ranked and counted, never dropped

ADR 0006 keeps every candidate of an ambiguous call site. How a payload shows them depends on direction:

- **Outbound** (what does this call?): the response is still a list of aggregated edges. For each site, the best few candidate destinations (about 3) are chosen, ranked by confidence and then locality (same package, imported scope, same repo); an edge is shown if it is among the best at any of its sites, and its `sites: n` counts all its sites. `+N other candidates`, with their highest confidence, is reported once per source node. Suppressed candidates are not traversed at depth 2 and beyond, and the notice says how many candidate destinations were not expanded. The per-site choice is made before edges are grouped: all of a source's sites and their candidates are read, the best are chosen per site, and only then are edges grouped.
- **Inbound** (who calls this?): a site whose candidates include the seed always appears, never suppressed, marked "1 of N candidates at this site" with its confidence. Low-confidence callers rank lower; they never vanish.
- **Both directions** (the default): each expansion applies both rules and takes the union; an edge admitted from either end appears once, and `+N other candidates` counts what is still suppressed after the union.
- Candidates not shown are counted separately from budget-cut results, and are reached only through `detail: candidates`, which has its own cursor. The Shard still stores every candidate, and `impact` still counts them in its reach, marked as low-confidence.

### Identity is stated once

- A response states its repo context once, and ids inside it are repo-relative. A relative id has an empty repo segment (`sym::server/etcdserver/api/rafthttp/stream.go#streamWriter.run`), a form no fully qualified id (`sym:<repo>:<path>#<name>`) can take, so the two never collide. A cross-repo response is ordered by global rank, and restates the repo context at the start of each run of consecutive results from one repo. Any id outside the current context, including the far end of an edge that crosses repos, is always written fully qualified.
- Every Verb accepts both forms on input: a fully qualified id, or a relative id together with a `repo` parameter. Every id in a response, used as input with its stated context, resolves to the same node; that is a Tier 1 check.
- Fields an id already encodes are not repeated: no separate `name`, `path` or `kind` beside an id that carries them, no `SearchHit.repo`, no null cursors. A line number is kept, because ids do not carry it.

### Payloads carry the text that ends the search

- Every Symbol a Verb returns (from `node`, `neighbors` or `search`) carries its declaration line and line number, for example `func (w *streamWriter) run()`. `detail: doc` adds the first sentence of its doc comment.
- Call sites carry their line text (above). Search snippets are plain text with line numbers, never HTML-escaped fragments.
- Text longer than a fixed cap (for example 400 characters, as with a generated declaration) is clipped, marked as clipped and given its source location.
- The Shard therefore keeps each Symbol's declaration line and each call site's line text; how is decided with the rest of the Shard layout.

### Every list is budgeted and ranked

- `node`, `neighbors`, `search`, `history` and every `detail:` page take a `budget` in tokens: 4,000 by default, 20,000 at most, and at least 500; below that the request is refused with a typed error. `budget` replaces `limit` on all of them.
- The budget is spent against one **cost model** on the typed response, independent of transport: a fixed cost per node, per edge and per site, plus the length of every character it will render, ids, repo headers and Provenance included, divided by four. The cut is made once, before rendering, so MCP and REST return the same items and the same cursor position. Room is always kept for the cursor and the truncation notice. Tier 1 calibrates the model: the MCP rendering, counted with a pinned offline tokenizer, stays within 1.1× the budget on every scenario.
- The seed comes first and is never cut; if its own text would exceed the budget, that text is clipped as above. Each Verb has its own order: a traversal by hop, then Provenance and confidence, then a penalty for high-degree nodes, then qualified id; `search` by normalised relevance (ADR 0008), then qualified id; `history` by commit time, newest first, then id. A cursor resumes its Verb's order.
- Discovery runs in batches bounded by the work cap. The pageable unit is a complete aggregated edge, never emitted before all its sites are seen. Inbound, batch boundaries fall only between `(src, dst, kind)` groups. Outbound and in both directions, they fall only between source nodes: the cap is checked between sources and may be overshot to finish the current one. The Shard keeps each group's evidence, and all of one source's evidence, contiguous in traversal order; how is part of the Shard layout. Results are ranked and paged within each batch, the order across batches is the order of discovery, which the notice states, and the cursor, once a batch is exhausted, resumes discovery where it stopped against the pinned revisions, so even one Symbol's callers beyond the cap are all reachable. Repeated continuations enumerate every visible item exactly once; that is a Tier 1 check. The notice says when more discovery remains.
- At depth 2 and beyond, a node whose visible degree is above a threshold is not expanded. It is listed as a hub with its visible edge count and the call that would expand it, so a deep traversal never returns hundreds of nodes with no edges.
- Truncation is always stated: how many nodes and edges were cut, exactly or as "at least N" when the traversal stopped early, and the budget that would include them all, but only when that budget is within the maximum; otherwise the notice points to the cursor.
- Every successful continuation returns at least one item, so paging always advances. If the next item will not fit the budget asked for, the request is refused with a typed insufficient-budget error that keeps the cursor valid and states the smallest budget that fits. Every item has a representation that fits the maximum budget: when an item, with its headers and notices, would not, its ids are replaced by short references, bound to the cursor's state and resolvable by `node` losslessly for as long as that state lives, final page included. A short reference is never published: anything that cites a result, such as Ask's retrieval log (ADR 0010), records the full id, which the server returns on request for every short reference.

### Cursors

- A cursor is a short opaque handle to state kept in Postgres, so any stateless query node can resume it. The state binds the requester (the issuing Member, or for Ask's public fallback the adapter installation and platform user key, with the fallback re-checked as still enabled on every continuation), the fingerprint of their effective view (ADR 0008), the Shard revisions the first page read (for a cross-repo request, the publish sequence it read at, [ADR 0012](0012-shard-layout-and-incremental-indexing.md)), and the request's shape (Verb, seed, direction, kinds, depth, detail and filters). It expires after an hour.
- Presented by another Member, after the view has changed, or with a different request shape, it is refused with the same expired-cursor error ADR 0008 defines. `budget` may change between pages, because the order is fixed by the pinned shape and revisions.
- This replaces ADR 0008's signed, self-describing cursor with a handle that binds the same things and more.

### One call can describe a Symbol

- `node` takes `include` and returns a card: the Symbol's signature and doc summary, then each requested section, ranked and cut under one shared budget. Every requested section is guaranteed a minimum share of the budget, divided by twice the number of sections requested; the remainder goes by rank, and each section ends with its own "+N more" and the call that fetches the rest.
- The sections are defined by edges and Verbs: `callers` and `callees` (CALLS) and `history` (the `history` Verb) exist today. `tests` (a `TESTS` edge kind) and `exports` (an `EXPORTS` edge kind) are decided in M1 by the parity decision on the map. `owners` (CODEOWNERS) arrives in M2. Memories are never part of the card, because derived Verbs never return Memories (ADR 0007): from M2 the card ends with a pointer such as "3 Memories anchored here (1 stale): `recall anchor=<id>`", whose count is computed under `recall`'s Audience and visibility rules and never cached across Members. A section is not advertised in the tool schema until its data exists; requesting it earlier is refused with a typed unsupported-section error, never answered with an empty section that would read as "none".

### MCP gets one compact rendering

- MCP tools declare no `outputSchema` and return a single text rendering, line- and tree-oriented, with the repo stated once, signatures and call sites inline and the truncation notice last. There is no `structuredContent` copy.
- REST and `yg … --json` keep the typed JSON for programs. The CLI's human output is the same rendering as MCP's, and the Skill's examples drop `--json`.
- The rendering and the JSON are two views of one typed, already budgeted response, produced at the transport boundary.

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
- The server ranks and budgets each response, and holds cursor state in Postgres, where every query node can reach it.
- The Shard grows by declaration lines and call-site line text, small next to the CALLS-edge bytes the profile measured; "Storage and indexing direction" owns the layout.
- Two renderings (MCP text and JSON) must stay in step; both come from one typed response.
- Ask (ADR 0010) and the PR impact report consume the same shapes and budgets.
