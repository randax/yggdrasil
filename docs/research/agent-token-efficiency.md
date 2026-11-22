# Agent token efficiency: why our payloads lose to grep-and-read

Status: research note · Date: 2026-11-23 · Ticket: #123 (map #121) ·
Yggdrasil baseline: commit `c4bec0c` on `main`

> Scope. Why Yggdrasil's Verb payloads cost more bytes than grep-and-read
> (`docs/EVAL.md` records this for every scenario). It also covers what
> comparable tools do to keep agent context small and decisive, and which
> experiments the EVAL harness should run next. Claims about Yggdrasil cite
> repository paths at the commit above. External claims cite their primary
> source inline. **[unverified]** marks claims I could not confirm against a
> primary source, and **[reconstructed]** marks byte counts I computed from
> the wire types instead of measuring against a running server.

## 1. Summary

1. **Payload shape is only part of the problem; the yardstick is the other
   part.** The EVAL baseline counts whole-file bytes
   (`crates/yg-cli/tests/orientation_efficiency.rs`, `baseline_bytes`). A real
   grep-and-read agent first sees `grep -n` output, which is line-oriented and
   much smaller. On the fixture, `grep -rn 'RenderReport('` prints 206 bytes
   and `grep -rn 'func RenderReport('` prints 85, against Verb totals of 1,395
   and 694 bytes (§2). We lose by about 2× to the file-read baseline and by
   about 7–8× to the grep output an agent actually reads.
2. **Our payloads carry identity and no answer.** `node`, `neighbors` and
   `search` return ids, kinds, names, paths, edge metadata and (for search) an
   HTML snippet. They never return a signature or a source line
   (`crates/yg-verbs/src/lib.rs`, `NodeView`, `GraphEdge`). Grep output *is*
   source lines. After a Verb call an agent usually still has to read the
   file, so today Verb bytes add to file reads instead of replacing them.
3. **The payloads are highly redundant.** Every external id embeds the full
   repo qualifier (`VerbId::external`). Each node repeats its name, path and
   kind, all derivable from its id. Each edge repeats two full ids. Search
   hits carry the qualifier a second time as `repo`. In a reconstructed
   `neighbors` answer for two callers, the qualifier appears six times: 19% of
   the bytes with `github.com/acme/widgets`, and 40% with a macOS temp-path
   qualifier like the fixture's **[reconstructed]**.
4. **Nothing is budgeted or ranked for an agent.** `neighbors` pages are
   ordered by `(depth, external id)`, which is alphabetical and not by
   relevance. They are capped by node and edge *counts* (100 nodes, up to
   10,000 edges), never by bytes or tokens (`crates/yg-verbs/src/lib.rs`,
   constants and `neighbors`). Graphify and Aider both cut to a token budget,
   with seeds or highest-ranked items first (§4.1, §4.2).
5. **Transports add more on top.** MCP results carry the payload twice, as
   text and as `structuredContent` (`crates/yg-api/src/mcp.rs`,
   `mcp_tool_result`). EVAL deliberately excludes that duplication. The
   Skill's examples all pass `--json`
   (`crates/yg-cli/skills/yggdrasil-navigation/SKILL.md`), so CLI agents skip
   the more compact human rendering the CLI already has
   (`crates/yg-cli/src/main.rs`, `neighbors`).
6. **Others converge on five techniques:**
   - a compact text or tree rendering;
   - stable short labels instead of opaque ids;
   - seed-first ranked output cut to a token budget, with an explicit
     truncation notice;
   - tiered detail (ids, then skeletons, then full code);
   - a few higher-level tools rather than many primitive ones.

   Graphify's code shows all of these (§4.1). Anthropic's tool-writing guide
   and the SWE-agent, Agentless and LocAgent papers back them with
   measurements (§4.6, §4.7).
7. **Graph calls beat file reads on relationship questions over large or
   unfamiliar code:** enclosing callers, transitive impact, cross-repo
   questions, and common names where grep is noisy. They lose on small repos
   and exact-string questions, and whenever the payload lacks the content
   that ends the search (§5.6). The current fixture tests only the losing
   case.
8. The ranked experiments in §6 start with fixing the yardstick, then move to
   cheap wire-shape changes. The expensive agent-in-the-loop evaluation comes
   last.

## 2. What EVAL measures, and what it misses

`docs/EVAL.md` defines `verb_bytes` as the raw REST body length of each Verb
response. The baseline "find[s] tracked fixture files with a scenario's literal
match, deduplicate[s] them, then count[s] each matched file's full bytes once",
and "excludes grep's own output and request overhead" (`docs/EVAL.md`,
"Fixed scenarios"). I reproduced the fixture from
`crates/yg-cli/tests/orientation_efficiency.rs` and emulated
`grep -rn <literal>` over it. The file-read numbers match the recorded table
exactly, which confirms the reproduction:

| scenario | Verb bytes (recorded) | file-read baseline (recorded) | `grep -n` output (computed) | does grep output alone answer it? |
| --- | ---: | ---: | ---: | --- |
| `find_callers` | 1395 | 729 | 206 | **No.** Grep shows the call lines but not the enclosing functions `BuildSummary` and `Preview`, which the check requires, so the agent must read more. |
| `locate_definition` | 694 | 244 | 85 | Yes: path, line and signature. |
| `summarize_module_dependencies` | 1472 | 244 | 29 (the `import (…)` block) | Yes. |

Three consequences:

- The baseline is too **generous to grep** in one way: it charges whole files
  where an agent would read grep lines. It is too **harsh** in another: real
  agents often read more than the matched files, and grep output on large
  repos with common names is noisy. Neither effect shows on a four-file
  fixture.
- The comparison is not like for like. Grep output carries **content**
  (source lines). Verb output carries **structure** (ids and edges). Only
  `find_callers` exercises something grep cannot do directly: name the
  enclosing caller.
- The metric counts bytes, not tokens. The EVAL note itself calls bytes÷4
  "a rough token estimate only". Graphify uses about 3 characters per token
  (`serve.py`, `_subgraph_to_text`). The two ratios differ for JSON
  punctuation and base64 cursors. Anthropic's token-counting endpoint is free
  (rate-limited) and gives model-exact counts, though its docs call the count
  "an **estimate**"
  ([docs: token counting](https://platform.claude.com/docs/en/build-with-claude/token-counting)).

## 3. Diagnosis: why our payloads are large

All of these follow from the wire types in `crates/yg-verbs`.

### 3.1 Identity, not answers

`NodeView` is `{id, kind, name?, path?}`. `NodeResponse` adds an edge summary
of `{kind, count, provenance: {level: count}}` per direction
(`crates/yg-verbs/src/lib.rs`). `GraphEdge` is
`{src, dst, kind, provenance, confidence, location?}`. No Verb returns a
signature, a docstring or the text of a call site. Even the `location`
(`path:line:col`) of a CALLS edge forces a file read to see the line. Every
tool in §4 that beats file reads returns *some* source text: Aider's
"critical lines of code for each definition", Agentless's skeletons,
LocAgent's preview and full-code tiers, and Graphify's `get_section`.

### 3.2 The repo qualifier is repeated in every id

External ids are `kind:<qualifier>:<local>`, made globally unique by splicing
in the repo qualifier (`crates/yg-verbs/src/lib.rs`, module docs and
`VerbId::qualify`). Every `NodeView.id`, `GraphEdge.src` and `GraphEdge.dst`
is serialized in this external form (`impl Serialize for VerbId`). A
single-repo `neighbors` page therefore repeats one constant string N + 2E
times, where N is the number of nodes and E the number of edges. For the fixture the qualifier
is the checkout's absolute temporary path (`Harness::qualifier` in
`crates/yg-cli/tests/common/mod.rs`), which `docs/EVAL.md` already names as
dominating "these small payloads".

### 3.3 Fields derivable from the id are repeated

A Symbol id `sym:<q>:<path>#<name>` already encodes kind (`sym`), path and
name, yet `NodeView` serializes `kind`, `name` and `path` alongside it.
`SearchHit` adds `repo`, the same qualifier that is already inside its `id`
(`crates/yg-verbs/src/search/types.rs`; the unit test
`typed_hits_keep_the_existing_wire_scalars` shows it). The `neighbors` edge
list also repeats the origin id as `dst` (or `src`) on every edge, although
the caller supplied it.

### 3.4 Search adds HTML snippets and a large cursor

- Snippets come from Tantivy's `Snippet::to_html()`
  (`crates/yg-shard/src/fts.rs`, `snippets_for`). This wraps matches in
  `<b>…</b>` and runs `htmlescape::encode_minimal` over the fragment, so every
  `"`, `<`, `>` and `&` in code becomes a 4–6-byte entity. Examples are Go
  string literals, Rust generics and `->`. The fragment is Tantivy's default
  of 150 characters, since `set_max_num_chars` is never called (tantivy
  0.26.1, `src/snippet/mod.rs`, `DEFAULT_MAX_NUM_CHARS = 150`). JSON then
  escapes newlines and tabs on top. For a short symbol the snippet can be as
  large as the whole definition.
- `next_cursor` is always serialized, as `null` when the result set is
  exhausted (`SearchWireResponse`). When present, it is base64url of a JSON
  `SearchCursor`, which holds the query, kinds, mode, offset and **every
  pinned target's qualifier and revision**, plus an HMAC
  (`crates/yg-verbs/src/cursor.rs`). Its size grows with the number of repos
  searched.

### 3.5 Bounds are counts, ordering is alphabetical

The defaults are 100 nodes per `neighbors` page, a maximum of 1,000 nodes,
1,000 edges per node, 10,000 edges per page and 10,000 edges examined
(`crates/yg-verbs/src/lib.rs`, constants). Page order is the traversal order
`(depth, external id)` (`neighbors`, the `order` vector and
`window_start`), so within a depth, nodes sort by id string. A hub at depth 1
can fill page one with alphabetically early but irrelevant neighbors. The
`truncated` flag says a cap was hit, but not how much was cut or how to
narrow the query. The ambiguity response is better: it is ranked by
confidence and bounded by `MAX_ADDRESS_CANDIDATES`
(`crates/yg-verbs/src/fuzzy.rs`).

### 3.6 Two round trips minimum

Every scenario is `search`, then `node` or `neighbors`, and each call costs
the model a tool-use block whose arguments repeat a full id. EVAL does not
count that either. The planned `callers`, `callees` and `map` Verbs (RFC 0001
§7) would collapse the common two-step paths into one call.

### 3.7 Transport overhead outside EVAL

- **MCP duplication.** `mcp_tool_result` returns
  `{"content":[{"type":"text","text":<canonical JSON>}],"structuredContent":<same JSON>}`
  (`crates/yg-api/src/mcp.rs`). The MCP spec only says a tool returning
  structured content "SHOULD also return the serialized JSON in a TextContent
  block" for backwards compatibility
  ([MCP spec 2025-11-25: tools](https://modelcontextprotocol.io/specification/2025-11-25/server/tools)).
  Whether a given client puts both copies into the model's context is
  client-specific. **[unverified]** I found no first-party statement of what
  Claude Code, Cursor or Codex forward to the model when both are present.
- **Skill steers agents to JSON.** Every cookbook line in the Skill ends in
  `--json`. The CLI's human output is already a terser line format
  (`Kind  id  (name)` and `src -[KIND prov conf]-> dst  @ loc`; see
  `crates/yg-cli/src/main.rs`, `neighbors`), though it still prints full ids.

### 3.8 Byte accounting (reconstructed)

I rebuilt the payloads from the DTOs for the `find_callers` neighbors answer,
which has two inbound CALLS edges at confidence `0.9` (`SYNTACTIC_MATCH`)
**[reconstructed]**. The snippet text is approximate and was not
HTML-escaped, so real search bytes are somewhat higher. The script is not
committed; the arithmetic follows from the field list.

| rendering of the same 2-caller answer | bytes (q = `github.com/acme/widgets`) | vs today |
| --- | ---: | ---: |
| today's `NeighborsResponse` JSON | 708 | n/a |
| today's JSON, fixture-style 66-byte temp-path qualifier | 966 | +36% |
| repo stated once, repo-relative ids | 597 | −16% |
| origin-relative edge list (`{node, kind, provenance, confidence, location}`), repo once, no derivable fields | 378 | −47% |
| line-oriented text (repo header, then `name  path:line:col  provenance` per caller) | 159 | −78% |

These reconstructions roughly match the recorded totals in `docs/EVAL.md`.
For example, `search` (one hit) plus `node` comes to 660–798 bytes depending
on qualifier length, against 694 recorded. The remaining gap comes from the
exact snippet and the number of search hits, which I could not observe
without running the stack.

## 4. What others do

### 4.1 Graphify (`graphify/serve.py`, `Graphify-Labs/graphify@3671f31`)

The background is in `docs/research/team-brain-landscape.md` §3.4 and §3.6.
Reading `serve.py` directly
([GH](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/serve.py)):

- **Text, not JSON.** `call_tool` returns only `types.TextContent`, with no
  `structuredContent`. Nodes render as one line each:
  `NODE <label> [src=<file> loc=<line> community=<name>]`. Edges are
  `EDGE <label> --<relation> [<confidence>]--> <label> at=<file>:<line>`.
  Edges name their endpoints by **label**, not by id (`_subgraph_to_text`).
- **Token budget on every traversal tool.** `query_graph`, `get_neighbors`
  and `get_community` take `token_budget` (default 2000). The budget is
  converted at "approx 3 chars/token" (`char_budget = token_budget * 3`) and
  cut at a line boundary (`_subgraph_to_text`, `_cut_lines_to_budget`).
  `query_graph` defaults to BFS at depth 3.
- **Seed-first, distance-ranked ordering.** Seeds render first and are never
  cut. The other nodes sort by hop distance from the seeds, then by degree
  descending, then by id. When query scores exist, the output is
  "file-first": files rank by their best-scoring node. The stated reason is
  "so the node that answers the query… survives the budget cut instead of
  being pushed past it by incidental high-degree hubs" (`_subgraph_to_text`
  comments).
- **Hub suppression.** BFS and DFS do not expand *through* nodes whose degree
  is at or above the 99th percentile (floored at 50), unless the node is a
  seed (`_bfs`, `_dfs`).
- **Explicit, actionable truncation notices.** When the budget cuts nodes,
  the answer starts with
  `[!] TRUNCATED: showing N of M nodes … raise the token budget to <exact
  number> … or narrow the query (e.g. context_filter=['call'] …)`. When all
  nodes fit but edges overflow, the answer instead says it is complete and
  over budget, and that raising the budget will not shrink it. The code
  comments record why: a false truncation warning "teaches an agent to
  distrust a complete answer and burn follow-up narrowing calls" (#2601), and
  silent overflow let queries "quietly cost 4-6x" the budget (#2784).
- **Raw content only on request.** `get_node` returns source file, location,
  type, community and degree, with attributes capped at 1,000 characters.
  `get_section` returns a Markdown section's text up to `max_chars` (default
  8,000) and is described as a replacement for "opening the .md file". There
  is no general source-code retrieval tool.
- **Benchmarks.** Graphify's own token benchmark compares canned query
  answers with a word-count *estimate* of the corpus. The figures range from
  about 1× on small corpora to "71.5x fewer tokens" on a 52-file corpus. All
  of these are self-reported (landscape §3.6 and the
  [token benchmark docs](https://docs.graphify.com/open-source/features/token-benchmark)).
  The ~1× small-corpus figure agrees with our fixture result.

### 4.2 Aider repo map (`aider/repomap.py`, `Aider-AI/aider@5dc9490`)

- **Content:** "a list of the files in the repo, along with the key symbols
  which are defined in each file", including "the critical lines of code for
  each definition". The reason given is that "sending whole files is a bulky
  way to send code context"
  ([Aider blog, 2023-10-22](https://aider.chat/2023/10/22/repomap.html);
  [docs: repo map](https://aider.chat/docs/repomap.html)).
- **Ranking** (source,
  [GH](https://github.com/Aider-AI/aider/blob/main/aider/repomap.py),
  `get_ranked_tags`):
  - The graph has one node per file and an edge from referencer to definer
    per identifier.
  - Edge weight is multiplied by ×10 if the identifier was mentioned in the
    chat, ×10 for long snake, kebab or camel names (8 or more characters),
    ×0.1 for `_private` names, ×0.1 if more than 5 files define the name, and
    ×50 if the referencer is a file already in the chat.
  - Reference counts are dampened with `sqrt`.
  - Personalized PageRank (`nx.pagerank(..., personalization=…)`) runs over
    the graph, and each file's rank is distributed over its out-edges to rank
    `(file, identifier)` definitions.
- **Budget fitting:**
  - `--map-tokens` defaults to 1,024 (`map_tokens=1024`), and the docs say
    "defaults to 1k tokens".
  - With no files in the chat, the budget expands by `map_mul_no_files=8`.
  - A binary search over how many ranked tags to include renders the tree
    each time and counts tokens. It accepts within 15% of the budget
    (`get_ranked_tags_map_uncached`, `ok_err = 0.15`).
  - Lines are truncated to 100 characters "in case we get minified js"
    (`to_tree`).
  - Token counting samples about 1% of lines for long texts
    (`token_count`).

### 4.3 Sourcegraph Cody

- **Two stages, recommender-style:** "we also distinguish between two stages
  in this process: retrieving context items and ranking them". The ranking
  stage picks "only items above a certain threshold… e.g. a relevance score
  or a token budget". The retrievers include "keyword search, semantic
  search" and "code graph analysis"
  ([Hartman et al., RecSys '24, arXiv:2408.05344](https://arxiv.org/html/2408.05344v1)).
- **Budget by snippet length.** Cody "takes the first N snippets returned (N
  being a function of the length of each snippet)", ranked with "an adapted
  form of the BM25 ranking function alongside other signals". For enterprise
  context, Sourcegraph dropped embeddings over third-party data exposure,
  admin burden and the cost at 100k+ repositories
  ([Sourcegraph blog, 2024-02-15](https://sourcegraph.com/blog/how-cody-understands-your-codebase)).
- Context: Cody reaches end of life on 2027-01-31 (landscape §5.1), but the
  ranking lesson stands.

### 4.4 Cursor

- **Semantic search plus grep beats grep alone.** Cursor reports "12.5%
  higher accuracy in answering questions (6.5%–23.5% depending on the
  model)" on its internal Cursor Context Bench. In an online A/B test, code
  retention rose 0.3% overall and 2.6% "on large codebases with 1,000 files
  or more". Its conclusion: "the combination of these two leads to the best
  outcomes"
  ([Cursor blog, 2025-11-06](https://cursor.com/blog/semsearch)). The
  benefit grows with codebase size. This is vendor data and has not been
  replicated.
- **Large outputs go to files, not truncation.** "Tool calls can dramatically
  increase the context window by returning a large JSON response… we instead
  write the output to a file and give the agent the ability to read it",
  because truncation "can lead to data loss". The headline 46.9% reduction is
  in *total agent tokens for runs that called an MCP tool*. It came from
  syncing **tool descriptions** to a folder, not from shrinking results
  ([Cursor blog, 2026-01-06](https://cursor.com/blog/dynamic-context-discovery)).

### 4.5 Augment Context Engine

Augment reports "70%+" improvement in agent quality across Claude Code,
Cursor and Codex on 300 Elasticsearch PRs (900 attempts). It also says
"MCP-enabled runs consistently required fewer tool calls and conversation
turns". It publishes no per-run token figures, no scoring method, and no
description of what the tool returns
([Augment blog, 2026-02-06](https://augmentcode.com/blog/context-engine-mcp-now-live)).
Its retrieval returns file paths and line ranges (landscape §5.3). Treat the
quality claim as unverifiable. Product status after the Harness acquisition is
uncertain (landscape §5.3).

### 4.6 Anthropic's tool guidance and Claude Code's limits

From [Writing effective tools for agents (2025-09-11)](https://www.anthropic.com/engineering/writing-tools-for-agents):

- Tools should "return only high signal information" and avoid "low-level
  technical identifiers (for example: `uuid`, `256px_image_url`,
  `mime_type`)". Resolving opaque ids to "semantically meaningful and
  interpretable language… significantly improves Claude's precision in
  retrieval tasks by reducing hallucinations".
- A `response_format` enum with "concise" and "detailed" values: in their
  Slack example the detailed response is 206 tokens and the concise one 72,
  about ⅓.
- "Some combination of pagination, range selection, filtering, and/or
  truncation". When truncating, "steer agents with helpful instructions".
- Tools can "consolidate functionality, handling potentially multiple
  discrete operations"; overlapping tools "distract agents".
- On format: "XML, JSON, or Markdown… can have an impact on evaluation
  performance… there is no one-size-fits-all solution". Choose by
  evaluation.

From [Effective context engineering (2025-09-29)](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents):

- The goal is "the smallest possible set of high-signal tokens".
- Recall degrades as context grows ("context rot"; there is an "attention
  budget").
- The just-in-time pattern keeps "lightweight identifiers (file paths, stored
  queries, web links, etc.)". Claude Code uses "glob and grep… just-in-time",
  "effectively bypassing the issues of stale indexing and complex syntax
  trees". This is the baseline we compete against.

Claude Code's MCP limits
([docs: MCP output limits](https://code.claude.com/docs/en/mcp)):

- It warns above 10,000 tokens per MCP tool output and defaults to a
  25,000-token maximum (`MAX_MCP_OUTPUT_TOKENS`).
- It saves text results longer than 50,000 characters to a file, which the
  agent then has to read.
- Servers may raise the per-tool threshold with
  `_meta["anthropic/maxResultSizeChars"]`.

Our 10,000-edge page cap can exceed all of these.

Supporting evidence on long context: performance is "often highest when
relevant information occurs at the beginning or end", and it "significantly
degrades" in the middle
([Liu et al., arXiv:2307.03172](https://arxiv.org/abs/2307.03172)). Across 18
models, "performance varies significantly as input length changes, even on
simple tasks", and related-but-wrong distractors hurt more as length grows
([Chroma, Context Rot, 2025-07-14](https://www.trychroma.com/research/context-rot)).
Redundant ids are mostly not distractors, but long alphabetical neighbor lists
are.

### 4.7 Papers on agent code navigation

- **SWE-agent** ([Yang et al., arXiv:2405.15793](https://arxiv.org/html/2405.15793)):
  - Design principle: "Environment feedback should be informative but
    concise."
  - Search commands "return at most 50 results". Past that limit they return
    nothing and suggest a narrower query.
  - Table 3 ablations on SWE-bench Lite:
    - summarized search scores 18.0% and iterative search 12.0%, lower than
      no search at all (15.7%), because agents "look through every match
      exhaustively";
    - a 100-line file window scores 18.0%, against 14.3% for 30 lines and
      12.7% for the full file.
  - Both too little and too much context hurt.
- **Agentless** ([Xia et al., arXiv:2407.01489](https://arxiv.org/html/2407.01489)):
  - Localization is hierarchical: a repo tree, then files, then a skeleton of
    each file, then edit locations.
  - The skeleton holds class and function headers, fields, method signatures
    and module and class comments. The paper calls it "a much more concise
    representation" than the whole file.
  - Cost: $0.70 and about 78k tokens per SWE-bench Lite issue.
- **LocAgent** ([Chen et al., ACL 2025, arXiv:2503.09089](https://arxiv.org/html/2503.09089)).
  This is the closest analogue to our Verbs. It has three tools:
  - `SearchEntity` (keyword or BM25) returns three detail levels: full code
    when there are 3 or fewer matches, a skeleton "preview" for large files,
    and "fold" (ids only) for more than 3 matches. The paper says this
    "effectively prevents lengthy code context and reduces noise".
  - `TraverseGraph` is a type-aware BFS whose output is a tree using
    indentation for distance. In Table 9, tree and JSON tie on
    function-level Acc@10 (41.18) and beat flat "row" formats (38.24). The
    sample is only 37 instances, so the margins are small.
  - `RetrieveEntity`.

  Table 6 ablations (fine-tuned Qwen-7B, Function Acc@10, full system
  71.53):

  | removed | Function Acc@10 |
  | --- | ---: |
  | `SearchEntity` | 53.28 |
  | BM25 index | 60.22 |
  | `TraverseGraph` | 66.06 |
  | `RetrieveEntity` | 69.34 |

  So **search carries most of the value, and traversal adds about 5 points.**

## 5. Synthesis by technique

### 5.1 Payload shape

Every tool that agents use heavily renders **lines or trees of
human-meaningful labels**: Graphify's NODE and EDGE lines, Aider's file:symbol
tree, LocAgent's indented tree, and grep's `path:line:text`. None repeats a
globally unique id per field. Anthropic's guide says to prefer meaningful
names over opaque identifiers. The LocAgent table shows JSON *can* match a
tree in accuracy, so the format matters less than **what is repeated**.
Our levers:

- state the repo once per response;
- use repo-relative ids;
- omit fields that can be derived from the id;
- make edge lists relative to the origin.

Together these cut the reconstructed answer by about 47% (§3.8).

### 5.2 Ranking

The ranking signals used are:

- distance from the seeds, then degree (Graphify);
- personalized PageRank over the reference graph with identifier-quality
  weights (Aider);
- BM25 plus learned signals (Cody);
- normalized BM25 for our `search` (RFC 0001 §7), which is fine.

`neighbors` has **no** relevance order. Within a depth it is alphabetical,
which is the gap to close. Usable signals already in the Shard are:

- provenance and confidence (precise before syntactic before inferred);
- edge kind;
- node degree;
- for search-seeded traversals, the search score.

### 5.3 Truncation

Graphify's lesson, recorded in its own bug history, is that truncation must
be **explicit, quantified and actionable**. It should state how many items
were cut, the exact budget that would fit them all, and how to narrow. A
false "truncated" warning on a complete answer costs follow-up calls. Our
`truncated: bool` meets none of these. Cursor's alternative is to spill large
results to a file instead of truncating. Claude Code applies the same
behaviour to MCP results over 50k characters, so we get it for free there.

### 5.4 Token budgets

Graphify (default 2,000 tokens, 3 characters per token) and Aider (default
1,024 tokens, ×8 with no files in chat, ±15%) let the **caller choose a
budget**. The server fills it with the highest-ranked content. Cody selects
by "a relevance score or a token budget". We expose only `limit`, in nodes.
A byte or token budget is the natural Verb parameter, and it keeps transports
identical because the engine forms the final DTO
(`crates/yg-verbs/src/engine.rs`).

### 5.5 Summaries vs raw nodes

The winning pattern is **tiered detail**:

- ids or labels when there are many matches (LocAgent "fold", our
  ambiguity candidates);
- a skeleton or signature when there are a few (LocAgent "preview",
  Agentless, Aider's "critical lines");
- full code only when there are three or fewer matches, or on request
  (LocAgent "full code", Graphify `get_section`).

SWE-agent's window ablation shows that full content is *worse* than a bounded
window. We sit at the "fold" tier for everything, which is why a file read
always follows. The cheapest decisive upgrade is the **signature line for
definitions and the source line for call sites**. Both are already
addressable through `location` and the stored node data. This is
**[unverified]**: I have not confirmed which fields the Shard stores beyond
the FTS `body`.

### 5.6 When graph calls beat file reads

Evidence and mechanism point the same way.

**Graph wins:**

- *enclosing-symbol* questions ("who calls X" means the calling function,
  which grep cannot name without a read; see `find_callers` in §2);
- transitive questions at depth 2 or more, and impact questions, where grep
  must iterate, which SWE-agent shows agents do badly;
- cross-repo questions;
- common or overloaded names, where grep is noisy (Aider down-weights
  identifiers defined in more than 5 files for the same reason);
- large codebases (Cursor's gain is largest at 1,000+ files).

**Grep wins:**

- exact-string and single-file questions;
- tiny repos (Graphify reports ~1× on small corpora; our fixture);
- any case where the graph payload lacks the content that ends the search.

LocAgent's ablation also says that **search quality matters more than
traversal**, so `search` payload efficiency deserves equal attention.

## 6. Candidate experiments for the EVAL harness (ranked)

The ranking weighs expected information against cost. Each item can be one
ticket. Every experiment keeps `correct` as a gate: a smaller payload that
fails a check is a regression.

| # | Experiment | Hypothesis | Metric(s) | Cost |
| --- | --- | --- | --- | --- |
| 1 | **Fix the yardstick.** (a) Add a `grep_lines_bytes` baseline: the `path:line:text` output of the scenario's literal grep. (b) Add a mid-size pinned Go fixture with hundreds of files and common names such as `New` and `String`, alongside the toy one. (c) Optionally record tokens via `/v1/messages/count_tokens` when an API key is present. | On the toy fixture, Verbs lose to both baselines. On the mid-size fixture, Verbs beat `baseline_bytes` on `find_callers`, and `grep_lines_bytes` grows with name collisions. | Per scenario: `verb_bytes`, `baseline_bytes`, `grep_lines_bytes`, optional `*_tokens`, `correct` | Low; harness only |
| 2 | **Repo-once, relative ids.** A response states its repo context once, and ids inside it are repo-relative. Cross-repo responses (search fan-out) group hits by repo. This is a breaking wire change, which is the project default. | `neighbors` and `node` bytes fall 15–20% at `github.com/org/repo` qualifiers and about a third at the fixture's temp-path qualifier | `verb_bytes`, with `correct` unchanged | Low–medium; touches `VerbId` serialization and the cursors |
| 3 | **Drop derivable and duplicate fields.** Remove `name`, `path` and `kind` where the id encodes them (or keep those and drop the id), `SearchHit.repo`, `next_cursor: null`, and origin-redundant edge endpoints. | A further ~35% off `neighbors` and about 15% off `search` (§3.8 reconstruction: 597 → 378 bytes) | `verb_bytes` | Low |
| 4 | **Decisive payloads: add the source line.** `node` returns the definition's signature line. `neighbors` CALLS edges return the call-site line text. Search snippets become plain text with a line number instead of HTML-escaped fragments. | Eliminates the follow-up read for `locate_definition` and `find_callers`. The total, Verb bytes plus required reads, falls below `baseline_bytes` on the mid-size fixture. | New `followup_read_bytes` (bytes an agent must still read to verify the answer) and `total_bytes = verb + followup` | Medium; needs the Shard to keep line text **[unverified]** |
| 5 | **Token or byte budget on `neighbors` and `search`.** Add a `budget` parameter. Seeds come first and are never cut. Order by hop, then provenance and confidence, then degree, then id. Truncation states the counts cut and the exact budget needed (Graphify's pattern). | Payloads stay bounded: p95 ≤ budget on a hub-node scenario, with the expected nodes still present | Max and p95 `verb_bytes` on a new `hub_neighbors` scenario; rank of expected node; `correct` | Medium |
| 6 | **Relevance-ordered `neighbors`, with hub suppression at depth 2 and beyond.** | The expected node moves from an alphabetical position to the top 5 on large fan-out traversals | Mean reciprocal rank of expected nodes; bytes until the first expected node | Medium |
| 7 | **Text rendering for MCP and CLI agents.** The MCP `content` text becomes a compact line or tree rendering, and `structuredContent` stays for programmatic clients. The Skill drops `--json` from its examples. | MCP tool-result text is 60–80% smaller than canonical JSON (§3.8: 708 → 159 bytes) with no accuracy loss | New `mcp_result_bytes` (the whole `tools/call` result) per scenario; accuracy needs #9 | Low to build; validating accuracy needs #9 |
| 8 | **Fused Verbs: `callers`, `callees` and `map` (planned M1).** | `verb_calls` falls from 2 to 1 for caller and callee scenarios, with fewer bytes because no search hop is needed | `verb_calls`, `verb_bytes` | Already planned; the harness only adds scenarios |
| 9 | **Agent-in-the-loop evaluation.** A fixed model runs each scenario as a natural-language task with (a) only grep and read and (b) Verbs plus grep and read, as in Cursor Context Bench and SWE-agent's ablations. | Arm (b) uses fewer total input tokens and tool calls on the mid-size fixture at equal or better accuracy, and gains nothing on the toy fixture | Total input tokens (API `usage`), tool calls, task accuracy, wall time | High; needs an API budget and nondeterminism handling, so run it rarely and not in CI |

The first order of work is #1, because it makes every later number
meaningful. #2, #3 and #7 are cheap shape changes measurable in one run. #4
is the one most likely to flip the economics, because it removes the
follow-up read. #9 is the only experiment that measures what actually
matters, agent accuracy per token, and should validate the winners of #2–#7.

## 7. Open questions and unverified items

- **[unverified]** Whether Claude Code, Cursor and Codex send both the MCP
  `content` text and `structuredContent` to the model. This decides whether
  our MCP cost is 1× or 2× the payload.
- **[reconstructed]** The §3.8 byte counts come from the DTO shapes, with an
  approximate snippet and no HTML escaping. They are not measured. Re-run the
  EVAL to get exact per-call numbers. That needs the compose stack, which I
  did not start.
- **[unverified]** Whether the Shard stores per-line text (needed for #4)
  beyond the FTS `body` field.
- Graphify's, Augment's and Cursor's figures are self-reported and
  unreplicated. LocAgent's format comparison uses 37 instances.
- A side finding outside this ticket: the `search` tool's input schema puts
  `anyOf` at the root (`crates/yg-verbs/src/lib.rs`, `input_schema`). Claude
  Code flattens such schemas and prepends a sentence to the description
  ([docs: root-level combinators](https://code.claude.com/docs/en/mcp)). That
  is harmless but adds tool-definition tokens.

## 8. Sources

Repository, at `c4bec0c`:

- `docs/EVAL.md`
- `crates/yg-cli/tests/orientation_efficiency.rs`
- `crates/yg-cli/tests/common/mod.rs`
- `crates/yg-verbs/src/lib.rs`
- `crates/yg-verbs/src/engine.rs`
- `crates/yg-verbs/src/search/types.rs`
- `crates/yg-verbs/src/cursor.rs`
- `crates/yg-verbs/src/fuzzy.rs`
- `crates/yg-shard/src/fts.rs`
- `crates/yg-shard/src/lib.rs`
- `crates/yg-api/src/mcp.rs`
- `crates/yg-cli/src/main.rs`
- `crates/yg-cli/skills/yggdrasil-navigation/SKILL.md`
- `docs/rfc/0001-yggdrasil-v1.md` §7
- `docs/research/team-brain-landscape.md` §3.4, §3.6, §5.1, §5.3
- tantivy 0.26.1 `src/snippet/mod.rs`, from the Cargo registry and pinned in
  `Cargo.lock`

External:

- Graphify `graphify/serve.py` @ `3671f31` (2026-11-21):
  <https://github.com/Graphify-Labs/graphify/blob/v8/graphify/serve.py>
- Graphify token benchmark docs:
  <https://docs.graphify.com/open-source/features/token-benchmark>
- Aider `aider/repomap.py` @ `5dc9490` (2026-05-22):
  <https://github.com/Aider-AI/aider/blob/main/aider/repomap.py>
- Aider repo map docs: <https://aider.chat/docs/repomap.html>
- Aider blog (2023-10-22): <https://aider.chat/2023/10/22/repomap.html>
- Hartman et al., "AI-assisted Coding with Cody", RecSys '24:
  <https://arxiv.org/html/2408.05344v1>
- Sourcegraph, "How Cody understands your codebase" (2024-02-15):
  <https://sourcegraph.com/blog/how-cody-understands-your-codebase>
- Cursor, "Improving agent with semantic search" (2025-11-06):
  <https://cursor.com/blog/semsearch>
- Cursor, "Dynamic context discovery" (2026-01-06):
  <https://cursor.com/blog/dynamic-context-discovery>
- Augment, "Context Engine MCP now live" (2026-02-06):
  <https://augmentcode.com/blog/context-engine-mcp-now-live>
- Anthropic, "Writing effective tools for agents" (2025-09-11):
  <https://www.anthropic.com/engineering/writing-tools-for-agents>
- Anthropic, "Effective context engineering for AI agents" (2025-09-29):
  <https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents>
- Claude Code MCP docs (output limits; root-level combinators):
  <https://code.claude.com/docs/en/mcp>
- Claude token counting docs:
  <https://platform.claude.com/docs/en/build-with-claude/token-counting>
- MCP specification 2025-11-25, Tools:
  <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>
- Yang et al., "SWE-agent": <https://arxiv.org/html/2405.15793>
- Xia et al., "Agentless": <https://arxiv.org/html/2407.01489>
- Chen et al., "LocAgent": <https://arxiv.org/html/2503.09089>
- Liu et al., "Lost in the Middle": <https://arxiv.org/abs/2307.03172>
- Chroma, "Context Rot" (2025-07-14):
  <https://www.trychroma.com/research/context-rot>
