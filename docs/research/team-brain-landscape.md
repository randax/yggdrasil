# Team-brain landscape: Yggdrasil, Graphify, and the field

Status: research note · Date: 2026-11-22 · Yggdrasil baseline: commit `5ca6d17` on `main`

> Scope. "Team brain" here means a shared, always-current knowledge layer for a
> (software) team that both people and AI agents can query: code, plus the
> decisions, tickets, discussions and memory around it. This note compares
> Yggdrasil against Graphify (open source and Cloud) and the leading products in
> three adjacent categories. Every substantive external claim is cited inline;
> claims about Yggdrasil cite repository paths at the commit above.

## 1. Summary

The products called "team brains" differ on four axes:

1. **How knowledge is derived.** Three families:
   - **Deterministic code structure.** Yggdrasil, Graphify and Sourcegraph
     parse code into symbols and edges with no model involved. Sourcegraph is
     compiler-precise today; Yggdrasil and Graphify are syntactic
     (tree-sitter).
   - **Embedded text chunks, ranked by hybrid search.** Glean, Onyx,
     Unblocked and Augment.
   - **LLM-extracted facts.** Zep/Graphiti, mem0 and Cognee.

   The families trade precision for breadth. Structural tools can say "these 14
   call sites, with confidence", but know nothing about the Slack thread that
   explains them. Answer engines know about the thread but cannot enumerate
   call sites.
2. **Breadth of sources.**
   - Code only: Sourcegraph, Graphify OSS, Yggdrasil M0.
   - Code plus forge data: Greptile, Copilot Spaces, Yggdrasil's planned M2.
   - Code plus everything a team writes (chat, docs, tickets, incidents):
     Unblocked, Glean, Rovo.
   - Conversations only: mem0, Zep.
3. **Write path.** Some products are read-only derived indexes: Yggdrasil
   today, Sourcegraph and Onyx. In others, agents or people can add knowledge:
   Unblocked Shared Memories, GitHub Copilot Memory, Graphify Cloud
   `remember`, Glean Agent Memory (beta), Zep and mem0. The trust models
   differ sharply. Copilot re-checks code citations and expires memories after
   90 days; Unblocked verifies against sources or attributes to a person;
   Graphiti's reference server has no authentication at all. The field moved
   fastest on this axis in 2026.
4. **Deployment and access model.**
   - Local file: Graphify OSS.
   - Self-hosted server: Yggdrasil, Onyx, Sourcegraph, Greptile.
   - SaaS: most of the rest.
   - Separately, every enterprise product mirrors source-system permissions;
     Yggdrasil deliberately does not.

Where each one sits:

- **Yggdrasil**
  - Shipped: a self-hosted, open-source (AGPL), server-side code graph with
    git history and per-edge provenance. It has a full-text index and only
    four Verbs. It has no semantic search, no precise edges yet, no PRs,
    issues, chat or docs, no write path and no permission mirroring.
  - Designed: a precise, cross-repository code, history and forge graph, with
    agent Memories anchored to code. Memories are glossary-only, not
    scheduled.
- **Graphify OSS** is a hugely adopted (≈152k stars) local tool: a syntactic
  graph over code plus LLM-extracted docs and media, shared by committing a
  JSON file.
- **Graphify Cloud** has, since July 2026, shipped much of Yggdrasil's M1–M2
  surface (callers, impact, cross-repo graphs, Contributors, GitLab) plus
  Memory and PR review. It remains syntactic and closed source. Its Jira
  connector is "not available yet".
- **Sourcegraph** is the precise, permission-aware, organisation-scale code
  index. It is closed, from $16K a year, operationally heavy, and has no
  memory or non-code sources.
- **Unblocked** is the closest thing to a full engineering team brain: code,
  PRs, chat, docs and tickets, with cited answers and verified shared
  memories. It has no structural code graph, and its permission mirroring is
  Enterprise-only.
- **Glean, Onyx and Atlassian Rovo** are company-wide search and answer layers
  in which code is a minor source.
- **Zep/Graphiti and mem0** are memory engines for agents rather than team
  knowledge layers. They do not touch code.

The single most differentiated idea in Yggdrasil's documents is **agent
Memories stored in the same provenance-ranked graph as compiler-derived code
edges, anchored to Symbols so they go stale when the code changes** (§9.4). No
surveyed product does exactly this. It is also, as of this commit, unscheduled.

## 2. Yggdrasil

### 2.1 What it is

Yggdrasil describes itself as "a self-hosted **Index Server** for serving a
repository Knowledge Graph to **AI agents over MCP** and **humans over a CLI**"
([README.md](../../README.md)). The Index Server is deployed once per
organization, syncs repositories from **Forges** (git hosting platforms), and
answers graph queries through a fixed set of **Verbs** — named graph operations
that are exposed identically as CLI subcommands, MCP tools and HTTP endpoints
([CONTEXT.md](../../CONTEXT.md)). The project is AGPL-3.0, written in Rust, and
positions itself as "the Codeberg of code intelligence, not the next SaaS"
([docs/PRD.md](../PRD.md), "License & development").

### 2.2 Shipped today (M0, verified in the repository)

The README separates shipped behaviour from roadmap explicitly, and the code
agrees with it:

- **Sync.** GitHub repositories are discovered and polled, plus a generic
  git-URL adapter (`crates/yg-sync/src/forge/github.rs`,
  `crates/yg-sync/src/forge/git_generic.rs`). Poll scheduling, ETag conditional
  requests, rate-limit budgeting shared through the control plane, and
  discovery rules (public/internal repos auto-included, private repos opt-in)
  are implemented ([TODO-ISSUES.md](../../TODO-ISSUES.md) items #28, #41, #48;
  [ADR 0001](../adr/0001-org-visible-access-model.md)). There is no webhook
  receiver yet ([README.md](../../README.md): "webhook acceleration" is M1–M3).
- **Extraction.** A tree-sitter *syntactic* pass with language packs for Go,
  Java, JavaScript, TypeScript, Python and Rust (`crates/yg-index/src/pass/`,
  grammar crates in `crates/yg-index/Cargo.toml`), plus git-history
  extraction. The shipped graph vocabulary is five node kinds — `File`,
  `Symbol`, `Package`, `Commit`, `Contributor` — and seven edge kinds —
  `DEFINES`, `CALLS`, `IMPORTS`, `EXTENDS`, `IMPLEMENTS`, `TOUCHES`, `AUTHORED`
  (`crates/yg-shard/src/lib.rs`, `NodeKind` and `EdgeKind`). The RFC's full
  schema (12 node kinds, 15 edge kinds including `OWNS`, `MENTIONS`, `CLOSES`,
  `DEPENDS_ON`) is therefore mostly roadmap ([RFC 0001 §5](../rfc/0001-yggdrasil-v1.md)).
- **Provenance and confidence on every edge.** Edges carry a provenance
  (`precise | syntactic | extracted | inferred`) and a confidence in [0, 1].
  Ambiguous syntactic name resolution is recorded as *spread confidence* —
  N candidate targets each get 0.9/N — rather than silently picking a winner;
  name-only `IMPLEMENTS` matches are capped at 0.5; edges carry a
  `path:line:byte-column` location ([ADR 0006](../adr/0006-syntactic-ambiguity-spread-confidence.md)).
  Today only `syntactic` (and history-derived) edges are produced; `precise`
  arrives with M1.
- **Storage.** Each repository revision is an immutable **Shard** in
  S3-compatible object storage: a SQLite graph segment plus a tantivy
  full-text segment and a manifest, swapped atomically by a pointer in
  Postgres; query nodes cache Shards in memory/NVMe with LRU eviction and
  single-flight cold fetches ([ADR 0005](../adr/0005-object-storage-native-shards.md),
  [RFC 0001 §6](../rfc/0001-yggdrasil-v1.md), TODO-ISSUES #45). Postgres is
  the control plane (repo registry, job queue via `SKIP LOCKED`, Shard
  pointers, Member tokens) — 18 migrations at `crates/yg-control/migrations/`.
- **Query.** Exactly four Verbs: `search` (lexical BM25 per repository,
  normalised by the repository's top score before a deterministic
  cross-repository merge), `node`, `neighbors` (depth ≤ 3, hub fan-out capped
  with truncation markers) and `history` (commits touching a file or a
  Symbol's defining file) ([RFC 0001 §7](../rfc/0001-yggdrasil-v1.md);
  TODO-ISSUES #44, #61). Fuzzy node ids return ranked candidates; cursors are
  signed (TODO-ISSUES #61, #63).
- **Interfaces.** REST (`POST /v1/verbs/<name>`), a hand-rolled MCP server over
  Streamable HTTP, the `yg` CLI (which doubles as a stdio→HTTP MCP proxy), and
  a Claude Code navigation **Skill** installed by `yg skill install` and
  stamped with the Verb contract version ([RFC 0001 §8–9](../rfc/0001-yggdrasil-v1.md);
  `crates/yg-cli/skills/yggdrasil-navigation/SKILL.md`). There is no web UI,
  IDE plugin, chat interface or natural-language answer engine, by design
  ([docs/PRD.md](../PRD.md), "Non-goals").
- **Access control.** Bearer tokens: a bootstrap Admin token from the
  environment plus Admin-issued, hashed, expiring, revocable Member tokens
  (`yg admin token issue`; migrations `0010_member_tokens.sql`,
  `0017_member_token_expiry.sql`), per-token rate limits (TODO-ISSUES #58).
  Authorisation is deliberately **org-visible**: every Member sees everything
  indexed; forge ACLs are not mirrored ([ADR 0001](../adr/0001-org-visible-access-model.md)).
- **Operations.** A container image and single-host Compose profile (one
  `yg serve --role=all` plus Postgres plus RustFS), smoke-tested in CI;
  Prometheus metrics for Verb latency and response size, queue depth, poll lag
  and cache activity ([docs/DEPLOYMENT.md](../DEPLOYMENT.md); README).
- **Evaluation.** An orientation-efficiency harness exists. Its recorded
  baseline (2026-07-17) is honest and unflattering: on a tiny Go fixture,
  **no scenario used fewer bytes than a grep-and-read baseline**, though all
  three answered correctly ([docs/EVAL.md](../EVAL.md)). There is no published
  result on a realistic corpus yet.

Adoption signals: the GitHub repository was created 2026-06-09, has 375 commits
(last on 2026-11-01) and 0 stars at the time of writing (`gh repo view`); it is
pre-release (release packaging, issue #15, was skipped). The Rust workspace is
roughly 50k lines including tests.

### 2.3 Planned (M1–M3 and beyond, from the design documents)

| Milestone | Planned content | Source |
|---|---|---|
| M1 — Precise core | SCIP indexers (Go, TS/JS, Python, Rust) run in sandboxed builds and overlay `precise` edges; `callers`, `callees`, `impact`, `map` Verbs; GitLab | [PRD](../PRD.md) Milestones; [ADR 0002](../adr/0002-precise-indexing-with-syntactic-fallback.md) |
| M2 — The whole graph | History/people layers (identity-merged Contributors), CODEOWNERS ownership, Doc Sections with doc→code `MENTIONS` edges, cross-repo Dependency resolution via package manifests and SCIP monikers, Codeberg, Forge Artifacts (Change Requests, issues, reviews); `path`, `deps`, `dependents`, `owners` Verbs | PRD; [RFC 0001 §4–5, §7](../rfc/0001-yggdrasil-v1.md) |
| M3 — Semantic + scale | Opt-in embedding provider (local by default) with vector segments chunked on Symbol/Doc Section boundaries, hybrid lexical+semantic search; opt-in LLM **Concept** extraction; webhook accelerators; a published 5000-repo certification run | PRD; RFC 0001 §4, §12 |
| Post-v1 | Per-user visibility via forge ACL mirroring; org-local feedback-trained retrieval; opt-in Change Request head indexing; OIDC | PRD "Post-v1 directions"; [ARCHITECTURE](../ARCHITECTURE.md) Security model |

**Memories are vocabulary, not yet a milestone.** CONTEXT.md defines
**Memory** (agent-asserted knowledge stored as a graph node), **Semantic** and
**Episodic Memory**, **Anchor** (an edge from a Memory to the code entity it
concerns, which makes it detectably stale when that entity changes),
**Promotion** (session → repo → org scope ladder) and an `asserted` provenance
level ("lowest trust") ([CONTEXT.md](../../CONTEXT.md)). None of this appears in
the PRD goals, the PRD milestones, the RFC schema (whose provenance enum has
four values, not five) or the code (`Provenance` in `crates/yg-shard/src/lib.rs`
has four variants). As of this commit, Memories are a designed concept with no
milestone, no Verb and no storage plan.

A smaller inconsistency worth knowing: the shipped Skill tells agents to use
the graph for "Doc Sections", "Dependencies", "ownership" and "forge artifacts"
(`crates/yg-cli/skills/yggdrasil-navigation/SKILL.md`), none of which M0
extracts.

## 3. Graphify (open source)

Repository facts below refer to `Graphify-Labs/graphify` on its default branch
`v8` at commit `3671f31` (2026-11-21), which corresponds to the PyPI package
`graphifyy` 0.9.115. "GH:" paths link to
`https://github.com/Graphify-Labs/graphify/blob/v8/<path>`.

### 3.1 What it is

Graphify is a Python library and CLI, driven primarily through an AI-assistant
skill, that turns a folder of code and documents into a knowledge graph on the
local machine. The pipeline is `detect → extract → build → cluster → analyze →
report → export`, passing NetworkX graphs between stages
([GH: ARCHITECTURE.md](https://github.com/Graphify-Labs/graphify/blob/v8/ARCHITECTURE.md)).
Output is `graphify-out/graph.json` (NetworkX node-link JSON,
`schema_version: 1`), an interactive `graph.html` and a `GRAPH_REPORT.md`
([GH: graphify/export.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/export.py)).
It is Apache-2.0 (relicensed from MIT in 0.9.25, 2026-07-22;
[GH: CHANGELOG.md](https://github.com/Graphify-Labs/graphify/blob/v8/CHANGELOG.md),
`LICENSE`, `NOTICE`).

### 3.2 Extraction

- **Code: tree-sitter, local, no LLM.** "Code is parsed with tree-sitter AST:
  deterministic, no LLM, nothing leaves your machine"
  ([GH: README.md](https://github.com/Graphify-Labs/graphify/blob/v8/README.md)).
  About 40 languages: 33 tree-sitter grammars and 7 built-in parsers
  ([GH: docs/how-it-works.md](https://github.com/Graphify-Labs/graphify/blob/v8/docs/how-it-works.md)).
  It also reads SQL DDL, package manifests (`depends_on` edges), MCP configs,
  live Postgres schemas and Terraform.
- **Cross-file resolution is heuristic, not compiler-precise.**
  `symbol_resolution.py` looks a bare callee name up in a global label index
  and emits an edge only when exactly one candidate survives tie-breakers,
  tagged `INFERRED` with confidence 0.85 "because the raw call alone is not
  import proof"; import-guided passes and per-language receiver typing (Java,
  C#, Kotlin, Swift, Go, C++) refine this
  ([GH: graphify/symbol_resolution.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/symbol_resolution.py)).
  The docs say Graphify "only draws an edge when the name resolves to exactly
  one declaration… it leaves the edge out, or marks it as ambiguous"
  ([docs: how Graphify finds dependencies](https://docs.graphify.com/concepts/how-graphify-finds-dependencies)).
  A SCIP ingester exists but is "a skeleton… Not wired to the CLI in this
  phase" (verified directly:
  [GH: graphify/scip_ingest.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/scip_ingest.py)).
  Note the contrast with Yggdrasil's ADR 0006: Graphify *drops* ambiguous
  calls; Yggdrasil *keeps* every candidate with split confidence.
- **Documents and media: LLM extraction.** Markdown, text, HTML, PDFs,
  Office files, images (vision) and audio/video (transcribed locally with
  faster-whisper) go through an LLM pass that produces concept nodes and
  relations such as `semantically_similar_to` and `rationale_for`
  ([GH: graphify/detect.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/detect.py);
  [GH: extraction-spec.md](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/skills/claude/references/extraction-spec.md)).
  Inside an assistant the skill uses the assistant's own subagents; headless
  runs use one of nine backends (Claude, Kimi, Ollama, Gemini, OpenAI and
  compatibles, DeepSeek, Azure, Bedrock, Claude CLI), or none with
  `GRAPHIFY_BACKEND=none` ([GH: graphify/llm.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/llm.py); README "Privacy").
- **Communities.** Leiden clustering groups nodes into communities, optionally
  named by an LLM; "god nodes" (hubs) and "surprising connections" are
  reported in `GRAPH_REPORT.md` (README;
  [docs: incremental updates](https://docs.graphify.com/open-source/features/incremental-updates)).

### 3.3 Graph schema

Nodes require `id`, `label`, `file_type` (`code | document | paper | image |
rationale | concept`) and `source_file`; edges require `source`, `target`,
`relation`, `confidence` (`EXTRACTED | INFERRED | AMBIGUOUS`) and
`source_file`, with an optional numeric `confidence_score` and
`source_location`
([GH: graphify/validate.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/validate.py)).
There is no typed node-kind enum beyond `file_type`, and the relation
vocabulary is open-ended (`calls`, `imports`, `inherits`, `implements`,
`dispatches_to`, `same_type_as`, `depends_on`, framework-specific relations,
plus LLM-produced semantic relations and hyperedges). EXTRACTED edges score
1.0, INFERRED edges use a 0.95–0.55 rubric, AMBIGUOUS 0.1–0.3
(extraction-spec.md; `export.py`). Older docs describe INFERRED as "a
reasonable inference the model made", whereas current docs and the code use it
mainly for deterministic static resolution
([docs: provenance](https://docs.graphify.com/concepts/provenance)) — a
terminology drift a reader should be aware of.

**Not in the graph:** git history, commits and authorship (only the
`built_at_commit` is recorded), Change Requests or issues, chat, tickets or
incidents (agent's grep of `graphify/*.py`; README).

### 3.4 Query and agent integration

- **CLI:** `graphify query "<question>"` (BFS or DFS from seed nodes, cut to a
  token budget, default 2000), `path A B`, `explain X`, `affected X`,
  `god-nodes`. Ambiguous names exit with code 3 and list candidates rather than
  guess (README; `graphify/__main__.py`).
- **Natural-language queries are lexical plus graph traversal, with no LLM and
  no vectors at query time:** the question is tokenised, node labels, paths
  and rationale are scored with IDF weighting and exact/prefix bonuses, seeds
  are picked, then traversed
  ([GH: graphify/serve.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/serve.py),
  `_score_query`, `_query_graph_text`). There is no full-text index over file
  bodies and no embedding code in the package; the README says "No
  embeddings, no vector store".
- **Provenance in answers:** every node line carries `src=<file> loc=L<n>`
  and every edge line its confidence tag and `at=<file>:<line>` (`serve.py`).
- **MCP server (`graphify-mcp`):** stdio by default, optionally Streamable
  HTTP with API-key auth ("OAuth is a deliberate follow-up"). Eleven tools:
  `query_graph`, `get_node`, `get_section`, `get_neighbors`, `get_community`,
  `god_nodes`, `graph_stats`, `shortest_path`, `list_prs`, `get_pr_impact`,
  `triage_prs`, plus report/stats resources (`serve.py` `list_tools()`). The
  docs say "ten tools"
  ([docs: MCP tools](https://docs.graphify.com/open-source/features/mcp-tools)).
  Neo4j and FalkorDB export is available.
- **PR helper:** `graphify prs` lists open PRs via the `gh` CLI with their
  impact on the *local* graph, finds PRs whose touched communities overlap,
  and can rank them with an LLM; it stores nothing
  ([docs: pull requests](https://docs.graphify.com/guides/pull-requests);
  [GH: graphify/prs.py](https://github.com/Graphify-Labs/graphify/blob/v8/graphify/prs.py)).

### 3.5 Freshness and team use

Freshness is local and event-driven: a SHA-256 content-hash cache makes
`graphify update .` re-extract only changed code files with no LLM call;
`graphify watch .` rebuilds code on save (docs changes only set a
`needs_update` flag); `graphify hook install` installs post-commit,
post-checkout, post-merge and post-rewrite hooks plus a graph merge driver
([docs: keep the graph fresh](https://docs.graphify.com/open-source/keep-the-graph-fresh)).
The documented team workflow is to **commit `graphify-out/graph.json` to the
repository**, register a union merge driver in every clone, and rebuild in a
pre-commit hook; the guide warns that the graph "holds names and paths taken
from your source"
([docs: shared team graph](https://docs.graphify.com/cookbooks/shared-team-graph)).
Cross-repo work is possible by merging graph files (`graphify merge-graphs`,
a `~/.graphify/global-graph.json`) with passes that add cross-repo member
calls and `same_type_as` links (`__main__.py`). A shared server means running
`graphify-mcp` over HTTP in Docker yourself
([docs: self-host](https://docs.graphify.com/open-source/self-host)).
There is no access control beyond an optional API key: whoever holds the file
or the key sees all of it.

### 3.6 Benchmarks (self-reported)

All numbers below are Graphify Labs' own and unreplicated.

- **Token reduction.** `graphify benchmark` *estimates* naive corpus tokens
  from word counts and compares them with five canned query answers; the docs
  warn "the naive side is an estimate, not a measured prompt"
  ([docs: token benchmark](https://docs.graphify.com/open-source/features/token-benchmark)).
  Quoted figures range from ~1× on small corpora to "71.5x fewer tokens" on a
  52-file corpus (docs/how-it-works.md).
- **BENCHMARKS.md** reports conversational-memory results (LOCOMO recall@10
  0.497 vs mem0 0.048; LongMemEval-S 76%) and an ERPNext code-QA result
  (82.0% vs 70.8% key-fact coverage for grep/read) on **six questions**
  ([GH: BENCHMARKS.md](https://github.com/Graphify-Labs/graphify/blob/v8/BENCHMARKS.md)).
  The configuration uses a BGE-m3 embedder, hybrid retrieval and a SurrealDB
  engine that do not exist in the open-source package, and the cited harness
  scripts are not in the repository tree.
- **Hosted PR review:** 35.3/50 bugs caught on "Code Review Bench" versus
  Greptile "re-scored with our rule" at 28/50, self-run and self-graded
  ([graphify.com/benchmarks](https://graphify.com/benchmarks), Nov 2026).

### 3.7 Adoption

151,774 stars and 14,584 forks; created 2026-04-03; last push 2026-11-21
(`gh api repos/Graphify-Labs/graphify`, checked 2026-11-22). Over 400
contributors; roughly daily releases; 283 PyPI versions; about 2.16 million
PyPI downloads in the last month
([pypistats](https://pypistats.org/api/packages/graphifyy/recent)). The
company, Graphify Labs, describes itself as a Y Combinator S26 company
([graphify.com/about](https://graphify.com/about)); no funding amount is
published first-party. Homepage figures ("11.4M" downloads, "15.2M
repositories indexed", customer logos) could not be verified.

## 4. Graphify Cloud

Graphify Cloud (app.graphify.com, hosted MCP at `api.graphify.com/mcp`) is the
commercial, hosted product. It indexes with "the same open source engine"
([docs: how it works](https://docs.graphify.com/platform/how-it-works)), but
everything above the engine — cross-repo promotion, Ask, Wiki, Memory, PR
review, verification — is closed source; the public `graphify-mcp` repository
holds only setup documentation.

### 4.1 Shipped, per the first-party changelog

Live 2026-07-21; cross-repo combined graphs and Ask 07-28; hosted MCP with
OAuth and GitLab 07-29; Free and Pro plans 07-31; PR reviews and formal
verification 08-11; Teams 09-02; Slack Ask 10-02; **Memory 10-05**; Wiki 10-08
([docs: changelog highlights](https://docs.graphify.com/changelog/highlights)).

- **Sources:** GitHub and GitLab.com (not self-managed GitLab), Google Drive
  (paid plans), Slack (Q&A surface), Contributors/authorship, Memory notes
  ([docs: connectors](https://docs.graphify.com/platform/connectors);
  [docs: security](https://docs.graphify.com/platform/support/security)).
  Hosted parsing lists 63 grammars.
- **Freshness:** rebuild on every push to the **default branch only**,
  re-processing changed files, capped by a daily build allowance
  ([docs: keep the graph fresh (platform)](https://docs.graphify.com/platform/keep-the-graph-fresh)).
- **Cross-repo:** combined graphs of 1–100 repositories with `calls` and
  `same_type_as` links ([docs: graphs](https://docs.graphify.com/platform/graphs)).
- **Hosted MCP tools** (a different, richer set than the local server):
  `list_repositories`, `query_graph`, `graphify_find`, `graphify_rank_files`,
  `search`, `fetch`, `graphify_node`, `graphify_callers`, `graphify_impact`,
  `graphify_trace`, `graphify_imports_exports`, `graphify_file_neighbors`,
  `graphify_tests_for`, `graph_stats`, **`recall` and `remember`**,
  `review_status`, workspace tools; OAuth with PKCE, 240 requests/minute
  ([docs: agent capabilities](https://docs.graphify.com/agents/capabilities);
  [docs: Graphify MCP](https://docs.graphify.com/platform/graphify-mcp)).
  `graphify_trace` can fall back to a "semantically similar" node, implying
  some similarity retrieval in the hosted product; how it works is not
  documented.
- **Ask** "answers with citations" and "abstains when it has no answer"
  (agent capabilities page).
- **PR review:** automatic reviews with a 1–5 merge-confidence score and a
  learning loop (`@graphify wrong|good|suppress`); **formal verification**
  checks behavioural equivalence of each changed function against its previous
  version (Python, JS/TS, Go, Java, C/C++, Rust, COBOL), advisory only,
  GitHub-only ([docs: verification](https://docs.graphify.com/platform/verification)).
  The README describes this feature differently ("architectural and dependency
  invariants").

### 4.2 Connectors: marketing versus docs

The README advertises "connectors for Sentry, Jira, and more". The pricing page
labels Jira, Confluence, Sentry, Google Drive and Vercel "Early access"
([graphify.com/pricing](https://graphify.com/pricing)). The connectors docs are
more specific: Sentry is "limited early access for a small number of paid
workspaces"; **Jira, Linear, Microsoft Workspace and Confluence are "Not
available yet"** (docs: connectors). As of 2026-11-22, Jira is not shipped.

### 4.3 Pricing, access control, deployment

| Plan | Price (USD, docs updated 2026-11-13) | Notable limits |
|---|---|---|
| Free | $0 | 3 repos, 25k nodes/repo, 10 builds/repo/day, 15 PR reviews/month; pauses after 7 idle days |
| Pro | $15/month or $120/year | Unlimited repos, 100 builds/repo/day, 400 reviews/day |
| Teams (≥2 seats) | $40/seat/month ($28 annual); launch offer $29 ($20 annual) | Shared graphs, 700 reviews/day |
| Enterprise | Custom, **early access** | Self-hosting (BYOC/VPC/on-prem/air-gapped "scoped per deal"), SSO/SAML, GitHub Enterprise |

Source: [docs: pricing](https://docs.graphify.com/platform/support/pricing);
[graphify.com/pricing](https://graphify.com/pricing).

Workspaces have Owner, Admin, Member and Billing roles; SSO is Enterprise-only
([docs: workspaces and members](https://docs.graphify.com/platform/workspaces-and-members)).
There is no documented mirroring of forge repository permissions; visibility
is by workspace. Code is sent to OpenAI and Anthropic as LLM subprocessors; SOC 2
Type II is "in progress" (docs: security). The docs contradict themselves on
API tokens for headless agents ("There is no API key" on
[docs: headless agents](https://docs.graphify.com/agents/headless) versus
workspace tokens on the security page).

## 5. Engineering-context products

These products are built for software teams specifically. They are
Yggdrasil's most direct comparison set.

### 5.1 Sourcegraph (in depth)

- **Knowledge unit and extraction.** A trigram full-text index (Zoekt) over
  "the default branch of every repository", plus two tiers of code
  navigation: search-based ("text search with syntax-level heuristics", about
  40 languages) and **precise**, using "compile-time information" from SCIP
  indexes, which works "across repositories and package dependencies"
  ([docs: architecture](https://sourcegraph.com/docs/admin/architecture);
  [docs: code navigation](https://sourcegraph.com/docs/code-navigation)).
  SCIP — the index format Yggdrasil plans to consume in M1 — now lives in a
  neutral `scip-code` organisation under Apache-2.0 with a steering committee
  that Sourcegraph sponsors ([scip-code.org/governance](https://scip-code.org/governance);
  [GH: scip-code/scip](https://github.com/scip-code/scip), verified).
- **Sources.** Code and git history only (commits, diffs, refs, contributors)
  from GitHub, GitLab, Bitbucket, Azure DevOps, Gerrit and Perforce; no
  tickets, chat or documents
  ([docs: permissions](https://sourcegraph.com/docs/admin/permissions);
  [docs: Deep Search](https://sourcegraph.com/docs/deep-search)).
- **Query model.** Regex and keyword search, semantic search, symbol, commit
  and diff search, plus **Deep Search**, an "agentic code search tool" that
  loops over search and navigation and returns "a detailed list of sources"
  for every answer (docs: Deep Search). Deep Search requires Sourcegraph's model
  provider (no bring-your-own key).
- **MCP.** Sixteen tools including `keyword_search`, `nls_search`,
  `go_to_definition`, `find_references`, `commit_search`, `diff_search`,
  `compare_revisions`, `get_contributor_repos`, `deepsearch`; OAuth with
  dynamic client registration or access tokens
  ([docs: MCP](https://sourcegraph.com/docs/api/mcp)).
- **Permissions.** Mirrors code-host permissions "across the product", rated
  to 40k users and 200k repositories, with webhooks for GitHub and Perforce
  and file-level permissions for Perforce (docs: permissions).
- **Write path.** None for knowledge. Notebooks and Sourcegraph Own (code
  ownership) were removed in 7.0 (2026-02-25)
  ([changelog: 7.0 removals](https://sourcegraph.com/changelog/7-0-removals-deprecations)).
- **Deployment.** Single-tenant cloud, Kubernetes or Docker Compose; services
  include sharded gitserver, zoekt, searcher, worker, two Postgres databases and
  an observability stack (docs: architecture). The heaviest operational
  footprint in this comparison.
- **Commercial and status.** Main repository closed in August 2024 (the
  public snapshot is archived and points to the last Apache-licensed commit;
  [GH: sourcegraph-public-snapshot](https://github.com/sourcegraph/sourcegraph-public-snapshot),
  verified archived); Zoekt, SCIP and `src-cli` remain Apache-2.0. One plan,
  Enterprise, "starting at $16K" per year
  ([sourcegraph.com/pricing](https://sourcegraph.com/pricing)). Cody Free and
  Pro ended 2025-07-23 and Cody reaches end of life 2027-01-31
  ([Sourcegraph blog](https://sourcegraph.com/blog/sunsetting-cody-future-of-code-intelligence));
  Amp became an independent company on 2025-12-02
  ([Sourcegraph blog](https://sourcegraph.com/blog/why-sourcegraph-and-amp-are-becoming-independent-companies)).

### 5.2 Unblocked (in depth)

- **Knowledge unit.** Three stores: vectors for semantic search, a knowledge
  graph of people (merged across identities), PRs, commits, reviews, issues,
  threads, repositories and topics, and relational tables; an "expert graph"
  built from PR and commit activity with time decay
  ([Unblocked blog: inside the context engine, 2026-08-05](https://getunblocked.com/blog/inside-the-unblocked-context-engine/)).
  No AST or compiler-level code graph is described.
- **Sources.** The broadest engineering connector set found: GitHub, GitLab,
  Bitbucket, Azure DevOps; CI systems; Slack and Teams; Confluence, Notion,
  Drive, SharePoint; Jira, Linear, Asana, Zendesk; Datadog, Sentry, PostHog,
  Snowflake; plus arbitrary read-only sources through "Custom MCP"
  ([docs index](https://docs.getunblocked.com/llms.txt);
  [docs: custom MCP](https://docs.getunblocked.com/data-sources/custom-mcp)).
- **Query model and provenance.** Parallel vector, graph and relational
  retrieval ranked by recency, authority, expertise and proximity to the
  asker; "source code represents the ground truth" and conflicts are reported
  rather than silently resolved (engine blog). Agent tools include
  `context-research` (a synthesised answer with citations) and typed searches
  over code, documentation, issues, messages and PRs
  ([docs: CLI query](https://docs.getunblocked.com/unblocked-cli/cli-query);
  [docs: cited sources](https://docs.getunblocked.com/unblocked-chat/cited-sources)).
- **Freshness.** Event-driven via webhooks for PRs and conversations; source
  code is re-indexed from the default branch "at least once every 8 hours"
  ([docs: GitHub](https://docs.getunblocked.com/data-sources/github)).
- **Write path and trust.** **Shared Memories** (2026-08-07) are learned from
  corrections in shared conversations; they are checked against connected
  sources, stored "with attribution to the person" when unverifiable, never
  created from DMs or private channels, treated as "strong hints, not as the
  last word", and editable by anyone in the organisation
  ([docs: shared memories](https://docs.getunblocked.com/unblocked-chat/shared-memories);
  [changelog](https://docs.getunblocked.com/changelog)). The MCP server itself
  exposes no write tools.
- **Permissions.** "Data Shield" restricts answers to what the asker can see in
  the source system, enforced at query time — but it is off by default except
  for Slack, does not cover several sources, and is Enterprise-only
  ([docs: Data Shield](https://docs.getunblocked.com/team-settings/data-shield);
  [pricing](https://getunblocked.com/pricing)).
- **Deployment and commercial.** SaaS; on-premises for Enterprise with no
  published architecture ([docs: security](https://docs.getunblocked.com/security-and-data-handling)).
  Proprietary; $29/user/month annually with pooled usage credits (an MCP call
  costs 2 credits) ([docs: licensing](https://docs.getunblocked.com/billing/how-licensing-works));
  $20M Series A, May 2025 ([Unblocked blog](https://getunblocked.com/blog/series-a/)).
  The homepage claims "62% lower cost, 76% fewer tokens" for Claude Code across
  107 tasks — a vendor claim without published methodology.

### 5.3 Augment Code Context Engine (medium depth)

Augment's Context Engine indexes code into chunks with embeddings from its own
models, served from a per-user index that updates "within a few seconds" of a
change ([Augment blog: real-time index](https://www.augmentcode.com/blog/a-real-time-index-for-your-codebase-secure-personal-scalable)).
Retrieval is semantic and returns file paths and line ranges; there is no
published code graph or precise layer. It is exposed through a local and a
remote MCP server ([docs: MCP](https://docs.augmentcode.com/context-services/mcp/overview))
and the open-source Context Connectors (MIT, index state local or in S3, but
embeddings still held by Augment;
[docs: context connectors](https://docs.augmentcode.com/context-services/context-connectors/how-it-works)).
Access control uses proof of possession: a client must prove it holds a file's
content before retrieving from it. The Cosmos product adds Markdown memory with
an evidence log that promotes a learning only when evidence is strong enough
([docs: experts memory](https://docs.augmentcode.com/cosmos/experts-memory)).
**Status is uncertain:** on 2026-10-08 Harness acquired "Cosmos, Auggie CLI,
the Code Context Engine, and related technology"
([Augment blog](https://augmentcode.com/blog/augment-code-next-chapter);
[Harness press release](https://www.harness.io/press-and-news/harness-acquires-augment-code)),
and neither party has said whether the standalone Context Engine continues.

### 5.4 Greptile (medium depth)

Greptile parses every file into a repository graph of directories, files,
functions, classes and variables with call, import and usage edges
([docs: graph-based context](https://www.greptile.com/docs/how-greptile-works/graph-based-codebase-context)),
and combines it with LLM-summarised, embedded chunks in Postgres/pgvector
([docs: self-hosting architecture](https://www.greptile.com/docs/self-hosting/system-architecture)).
The graph serves **PR review**, not general querying: the MCP tools list PRs,
reviews, past review comments and a substring search over an auto-generated
Knowledge Base, and expose no graph or code-search tool
([docs: MCP tools](https://www.greptile.com/docs/mcp/tools)). It learns rules
from PR comments and reactions, and agents can create "custom context" rules
over MCP ([docs: memory and learning](https://www.greptile.com/docs/how-greptile-works/memory-and-learning);
[docs: custom context](https://www.greptile.com/docs/mcp/custom-context)).
Self-hosting is available (Compose or Kubernetes). It is a review product with
a knowledge layer inside, not a team brain.

### 5.5 Added: GitHub Copilot Spaces and Copilot Memory

Not on the original candidate list, but for teams on GitHub this is the
default shared-context layer and has the most carefully designed agent write
path found in this survey. **Spaces** bundle repositories, files, PRs, issues
and free text; GitHub sources "are automatically updated as they change", and
Spaces are shared with admin/editor/viewer roles and reachable through the
GitHub MCP server ([GitHub docs: Spaces](https://docs.github.com/en/copilot/concepts/context/spaces)).
**Copilot Memory** stores agent-written repository facts "shared with your
collaborators", each with citations to code that are checked against the
current branch before use, with voting and a default 90-day expiry; reading
repository memories requires read access and managing them requires write
access ([GitHub docs: Copilot Memory](https://docs.github.com/en/copilot/concepts/agents/copilot-memory);
[GitHub blog](https://github.blog/ai-and-ml/github-copilot/building-an-agentic-memory-system-for-github-copilot/)).
It is SaaS-only, GitHub-only, and exposes no structural graph queries.

### 5.6 Briefly: Sourcebot, DeepWiki, and others

- **Sourcebot** was the closest open, self-hosted analogue: Zoekt search across
  many code hosts, search-based (ctags) navigation, an MCP server with "Ask"
  and auto-generated wikis
  ([docs: MCP server](https://docs.sourcebot.dev/docs/features/mcp-server)).
  It **went closed source** with v6
  ([Sourcebot blog, 2026-10-08](https://www.sourcebot.dev/blog/sourcebot-is-now-closed-source);
  [PR #1721](https://github.com/sourcebot-dev/sourcebot/pull/1721), verified
  merged 2026-10-10), which matters for Yggdrasil's positioning as an open,
  self-hosted option.
- **Cognition DeepWiki** generates per-repository wikis with source links and
  serves them over MCP; free for public repositories, private ones through
  Devin ([Devin docs: DeepWiki MCP](https://docs.devin.ai/work-with-devin/deepwiki-mcp)).
- **CodeRabbit learnings** (review rules taught in PR comments, optional admin
  approval delay; [docs](https://docs.coderabbit.ai/knowledge-base/learnings)),
  **Potpie** (Apache-2.0 "context graph" daemon over GitHub, Linear, Jira;
  [GH: potpie](https://github.com/potpie-ai/potpie)), **Moderne** (refactoring
  on a lossless semantic tree) and **Swimm** (pivoted to services) were
  reviewed and set aside as narrower than a team brain.
- **Cursor's** codebase indexing is per-editor, with team index reuse
  ([Cursor blog](https://cursor.com/blog/secure-codebase-indexing)); its
  current semantic-search behaviour could not be confirmed from its docs.

## 6. Company-wide knowledge products

These products index a company's documents, chat and tickets through many
connectors, mirror each source's permissions, and answer natural-language
questions with citations. Code is a minor source for most of them.

### 6.1 Glean (in depth)

- **Knowledge unit.** Connector-ingested documents chunked into a hybrid
  index, plus a proprietary "Enterprise Graph" of people, projects, customers
  and products, including per-user graphs inferred from activity
  ([glean.com: Enterprise Graph](https://www.glean.com/product/enterprise-graph)).
  Glean claims "275+" connectors ([glean.com/pricing](https://www.glean.com/pricing)).
- **Code.** The GitHub connector indexes repository metadata, source files,
  commits, READMEs, issues, PRs with reviews, diffs and commit statuses, for
  the **default branch only** ([docs: GitHub connector](https://docs.glean.com/connectors/native/github/about)).
  Code Search (GitHub, GitLab, Bitbucket) uses AST-based chunking, embeddings in
  an ANN index and an OpenSearch lexical index tuned for identifiers, merged
  with popularity, recency and affinity signals; the documentation describes
  **no symbol table or call graph** — code is linked to other content through
  activity such as co-edits and ticket links
  ([docs: how Code Search works](https://docs.glean.com/security/how-code-search-works)).
- **Freshness.** Webhooks "best effort as received", incremental crawls every
  10 minutes, full crawls about every 28 days (GitHub connector docs).
- **Permissions.** Connectors fetch and mirror each source's permission map;
  for GitHub each user must complete OAuth before any GitHub content appears to
  them; MCP queries "enforce user permissions through the Knowledge Graph"
  ([docs: connectors](https://docs.glean.com/connectors/about);
  [docs: MCP](https://docs.glean.com/administration/platform/mcp/about)).
- **Write path.** Agent Memory (beta): Markdown memories consolidated from
  chats, activity and tools, inspectable and editable by users
  ([Glean blog, updated 2026-11-11](https://www.glean.com/blog/memory-enterprise-context)).
  The MCP `read_memory` tool is read-only
  ([release notes 2026-04-08](https://docs.glean.com/release-notes/releases/2026-04-08-april-release)).
- **Agents.** A managed remote MCP server (OAuth 2.1 + PKCE) with Search, Chat,
  Read Document, Code Search and People tools, billed through usage credits
  (docs: MCP).
- **Deployment and commercial.** SaaS, or "Customer Hosted" in the customer's
  GCP or AWS account but still managed by Glean
  ([help: deployment options](https://help.glean.com/en/articles/10093412-glean-deployment-options)).
  Proprietary; **no public pricing**. $150M Series F at $7.2B valuation and
  over $100M ARR, June 2025 ([Glean blog](https://www.glean.com/blog/glean-series-f-announcement)).

### 6.2 Onyx, formerly Danswer (in depth)

- **Knowledge unit and retrieval.** Chunks with embeddings in OpenSearch,
  queried with hybrid keyword plus vector search
  ([GH: OpenSearch index README](https://github.com/onyx-dot-app/onyx/blob/main/backend/onyx/document_index/opensearch/README.md)).
  Vespa was removed on 2026-10-01, in the same PR that deleted a dormant
  knowledge-graph extraction pipeline
  ([PR #15336](https://github.com/onyx-dot-app/onyx/pull/15336), verified
  merged). On top sits "agentic RAG" and Deep Research
  ([GH: onyx](https://github.com/onyx-dot-app/onyx)).
- **Sources.** 56 documented connectors
  ([docs: connectors](https://docs.onyx.app/admins/connectors/overview)). The
  GitHub connector indexes PRs, issues and prose files only; source code is
  "intentionally" excluded
  ([GH: github/connector.py](https://github.com/onyx-dot-app/onyx/blob/main/backend/onyx/connectors/github/connector.py)).
- **Freshness.** Connector refresh every 30 minutes by default; pruning every
  7 days (docs: connectors).
- **Permissions.** Permission sync is documented for 16 connectors (GitHub,
  Slack, Confluence, Jira, Drive, Linear and others), taking "a few minutes to
  an hour" to apply source-side changes (docs: connectors). The code lives under
  `backend/ee/`, i.e. under the Onyx Enterprise License, not MIT
  ([GH: ee external_permissions](https://github.com/onyx-dot-app/onyx/tree/main/backend/ee/onyx/external_permissions)).
- **Write path.** None for agents: knowledge enters only through connectors.
- **Interfaces.** Web and desktop chat, Slack and Discord bots, browser
  extension, embeddable widget, and an MCP server whose
  `search_indexed_documents` tool returns ranked chunks with metadata
  ([docs: MCP server](https://docs.onyx.app/overview/onyx_anywhere/mcp_server)).
- **Operational weight.** The Compose deployment runs an API server, Celery
  background workers, a web server, an MCP server, a webhook server, two model
  servers, Postgres, OpenSearch, Redis, nginx, an object store and a code
  interpreter ([GH: docker-compose.yml](https://github.com/onyx-dot-app/onyx/blob/main/deployment/docker_compose/docker-compose.yml)).
- **Commercial.** MIT core with `ee/` directories under a separate licence
  ([GH: LICENSE](https://github.com/onyx-dot-app/onyx/blob/main/LICENSE)); Cloud
  Business $20/user/month annually, Enterprise custom
  ([onyx.app/pricing](https://www.onyx.app/pricing)); 33.5k stars; $10M seed,
  March 2025 ([Onyx blog](https://onyx.app/blog/seed-round)).

### 6.3 Atlassian Rovo (medium depth)

Rovo is built on Atlassian's **Teamwork Graph**, a "permission-aware map" of
Jira, Confluence and 50+ connected tools; Atlassian says agents can write
discovered relationships and decisions back into it
([Atlassian blog, 2026-07-07](https://www.atlassian.com/blog/company-news/closing-the-ai-context-gap)).
The GitHub connector covers repositories, PRs, issues, commits, branches and
users ([Rovo GitHub connector](https://www.atlassian.com/software/rovo/connectors/github));
whether file contents are indexed is unconfirmed. Connectors are either synced
into the graph or queried live ("Direct", e.g. Slack)
([support: connector types](https://support.atlassian.com/organization-administration/docs/teamwork-graph-connector-types/)).
The Rovo MCP Server is generally available and covers Jira, Confluence,
Compass, Bitbucket, JSM, Loom and Teamwork Graph, respecting "the user's
existing access controls"
([GH: atlassian-mcp-server](https://github.com/atlassian/atlassian-mcp-server)).
Rovo is included in paid Atlassian Cloud plans through per-user credits
([support: Rovo usage limits](https://support.atlassian.com/rovo/docs/rovo-usage-limits/));
it is cloud-only and proprietary. Rovo Dev adds a PR reviewer, CLI and IDE
extension ([support: Rovo Dev](https://support.atlassian.com/rovo/docs/rovo-dev-rollout/)).

### 6.4 Briefly: Notion AI enterprise search and Dust

Notion's enterprise search (Business at $20/member/month and Enterprise,
labelled beta) connects Slack, Teams, Drive, SharePoint, Jira, GitHub, Linear
and others, respects source permissions and cites sources; new content can
take up to 3 hours to appear and only about a year of history is imported
([Notion pricing](https://www.notion.com/pricing);
[help: AI connectors](https://www.notion.com/help/notion-ai-connectors);
[help: enterprise search](https://www.notion.com/help/enterprise-search)).
Dust is an agent platform that syncs "Connections" (GitHub, Notion, Slack,
Drive) and reaches other systems through MCP; plans start at €30/seat/month
([dust.tt pricing](https://dust.tt/home/pricing);
[docs: data sources](https://docs.dust.tt/docs/user-documentation/data-sources/overview)).
Dust's formerly public repository now returns 404, so its current openness
could not be confirmed. Neither is a code-structure product; both are listed
for completeness.

## 7. Shared agent-memory products

These products store what agents and users *learn* — facts extracted from
conversations and documents — rather than indexing a codebase. They are the
closest analogue to Yggdrasil's designed-but-unscheduled Memory layer.

### 7.1 Zep and Graphiti (in depth)

- **Knowledge unit.** A temporal knowledge graph of entities, facts as edges,
  "episodes" (the raw input, kept as provenance) and communities
  ([GH: graphiti](https://github.com/getzep/graphiti)). Edges are
  **bi-temporal**: `valid_at`/`invalid_at` record when a fact was true;
  `created_at`/`expired_at` record when the system learned or retired it
  ([GH: graphiti_core/edges.py](https://github.com/getzep/graphiti/blob/main/graphiti_core/edges.py)).
  Contradicted facts are invalidated, not deleted.
- **Extraction and retrieval.** LLM extraction (OpenAI default; Anthropic,
  Gemini, Groq, local models supported), incremental ingestion, hybrid semantic
  + BM25 + graph-traversal retrieval with graph-distance reranking; every fact
  traces back to its episodes (GH: graphiti).
- **Stores.** Neo4j, FalkorDB or Amazon Neptune; the Kuzu driver was removed on
  2026-11-15 ([PR #2151](https://github.com/getzep/graphiti/pull/2151)).
- **Write path and trust.** Writes are the primary interface (`add_memory`,
  `add_triplet`, delete tools). Graphiti's reference MCP and REST servers have
  "no authentication, no authorization, and no prompt-injection protection"
  (GH: graphiti). Zep's hosted MCP server uses OAuth 2.1 through the customer's
  identity provider, enables writes by default with an admin read-only switch,
  and gates shared graphs by group-based attribute access control
  ([Zep help: MCP server](https://help.getzep.com/memory-mcp-server)).
- **Sources.** No code ingestion and no native connectors; text, JSON,
  messages and documents are pushed in through the API or a bulk-ingest tool
  ([Zep help: enterprise context graphs](https://help.getzep.com/enterprise-context-graphs)).
  Source ACLs are not mirrored.
- **Commercial.** Graphiti is Apache-2.0 (33.0k stars). Zep Community Edition
  was discontinued in April 2025
  ([Zep blog](https://www.getzep.com/blog/announcing-a-new-direction-for-zeps-open-source-strategy/));
  Zep Cloud is managed, BYOK or BYOC, priced by ingestion credits (Flex
  $125/month for 50k credits; [getzep.com/pricing](https://www.getzep.com/pricing)).
- **Benchmarks.** Zep's paper reports 94.8% on DMR versus 93.4% for MemGPT
  ([arXiv 2501.13956](https://arxiv.org/abs/2501.13956)), self-reported. The
  public LoCoMo dispute with mem0 ended with Zep correcting its own figure to
  75.14% ([zep-papers issue #5](https://github.com/getzep/zep-papers/issues/5));
  Letta then reported that a plain filesystem agent scores 74.0% on LoCoMo
  ([Letta blog](https://www.letta.com/blog/benchmarking-ai-agent-memory)) —
  a useful reminder that these conversational benchmarks say little about
  engineering knowledge.

### 7.2 mem0 (in depth)

- **Knowledge unit.** Extracted memory facts stored as vectors, with an entity
  side collection. Since April 2026 the open-source extractor is a single
  LLM pass that only **adds** memories — no UPDATE or DELETE — leaving ranking
  to retrieval (semantic + BM25 + entity match, with temporal ranking)
  ([docs: OSS v2→v3 migration](https://docs.mem0.ai/migration/oss-v2-to-v3)).
  The ADD/UPDATE/DELETE pipeline in mem0's paper is therefore historical.
- **Graph memory left open source.** Graph memory is now platform-only;
  OpenMemory (the local MCP memory server) was removed from the repository on
  2026-07-29 ([PR #6530](https://github.com/mem0ai/mem0/pull/6530), verified
  merged).
- **Sources, trust, permissions.** Conversation and agent messages only; no
  connectors, code or git. Any API-key holder can write; scoping is by user,
  session and agent; there is no source-ACL mirroring
  ([GH: mem0](https://github.com/mem0ai/mem0)).
- **Deployment and commercial.** Self-hosted server is FastAPI + Postgres with
  pgvector ([GH: server/docker-compose.yaml](https://github.com/mem0ai/mem0/blob/main/server/docker-compose.yaml));
  hosted MCP at `mcp.mem0.ai`. Apache-2.0, 69.3k stars; platform pricing
  publishes no dollar figures ([mem0.ai/pricing](https://mem0.ai/pricing));
  Series A announced October 2025 ([mem0.ai/series-a](https://mem0.ai/series-a)).
  README benchmark numbers (LoCoMo 92.5, LongMemEval 94.4) are measured on the
  managed platform, not the open-source package, and are self-reported.

### 7.3 Cognee (medium depth)

Cognee builds a graph plus vectors plus relational provenance, now exposed as
`remember`, `recall`, `improve` and `forget`; it says "code becomes a graph of
symbols and dependencies" and extracts entities without an LLM by default
([GH: cognee](https://github.com/topoteretes/cognee);
[docs: architecture](https://docs.cognee.ai/core-concepts/architecture)).
Defaults are SQLite, LanceDB and a Kuzu fork, with Postgres, Neo4j and others
as alternatives. Its "Company Brain" page claims GitHub, Slack and Jira
ingestion, cited answers, permission carry-over and agent write-back over MCP
([cognee.ai/company-brain](https://www.cognee.ai/company-brain)); no GA or beta
status is stated, so treat it as marketing. Apache-2.0, 33.5k stars, $7.5M seed
(February 2026) ([Cognee blog](https://www.cognee.ai/blog/cognee-news/cognee-raises-seven-million-five-hundred-thousand-dollars-seed)).
Of the memory projects, Cognee's stated direction overlaps most with a team
brain over code.

### 7.4 Briefly: Letta and supermemory

**Letta** (formerly MemGPT) retired its V1 agent server; development moved to
`letta-code`, a TypeScript agent harness with self-edited memory blocks and a
git-backed memory filesystem that "dreams" (consolidates memory in the
background) ([GH: letta](https://github.com/letta-ai/letta);
[docs: memory](https://docs.letta.com/letta-code/memory)). It is a per-agent
memory system with team sharing on a paid tier
([docs: pricing](https://docs.letta.com/letta-code/pricing)), not a team
knowledge layer. **supermemory** offers fact extraction with contradiction
handling, connectors (Drive, Gmail, Notion, GitHub) and an MCP server; the
repository is MIT, but the local engine binary is not open source
([GH: supermemory](https://github.com/supermemoryai/supermemory);
[supermemory.ai/pricing](https://supermemory.ai/pricing)).

### 7.5 Considered and dropped

Microsoft GraphRAG is "largely in maintenance mode" and batch-oriented
([GH: graphrag](https://github.com/microsoft/graphrag)); ByteRover/Cipher was
archived 2026-06-25 ([GH: byterover-cli](https://github.com/campfirein/byterover-cli));
Zep Community Edition and OpenMemory are discontinued. Slack AI and per-user
assistant memory (Claude, ChatGPT projects) are tied to one tool or one person
rather than being agent-queryable team layers, so they were not profiled.
Basic Memory (AGPL-3.0 Markdown-plus-wikilink notes shared between humans and
agents over MCP; [GH: basic-memory](https://github.com/basicmachines-co/basic-memory))
is a small but philosophically interesting example of a human-and-agent
writable memory; it is noted here and not profiled further.

## 8. Comparison tables

The tables compress sections 2–7; the citations are there. "Shipped" means
documented as available to customers or present in released code on
2026-11-22; "beta" and "early access" are marked; roadmap items are marked
*planned*. Yggdrasil appears twice — as shipped (M0) and as designed (v1) — so
that its roadmap is held to the same standard as everyone else's marketing.

### 8.1 Knowledge unit, extraction and sources

The first real divide is how knowledge is derived. Yggdrasil, Graphify and
Sourcegraph extract code structure deterministically (no model in the loop);
Unblocked, Augment, Glean and Onyx embed chunks of text and rank them;
Zep, mem0 and Cognee use an LLM to extract facts. The second divide is breadth:
code-structure tools ignore tickets and chat, while company-wide tools ignore
code structure.

| Product | Knowledge unit | Code extraction | Code history | Change Requests / issues | Chat, docs, tickets, incidents |
|---|---|---|---|---|---|
| Yggdrasil M0 | Typed graph (5 node, 7 edge kinds) + full-text | Syntactic (tree-sitter), 6 languages | Yes (commits, Contributors) | No | No |
| Yggdrasil v1 (planned) | Graph (12 node, 15 edge kinds) + full-text + vectors | SCIP-precise with syntactic fallback | Yes, identity-merged | Yes (M2) | Markdown docs only; LLM Concepts opt-in (M3) |
| Graphify OSS | Untyped graph (open relation vocabulary) + communities | Syntactic (tree-sitter), ~40 languages | No | Live lookup only, not stored | Docs, PDFs, media via LLM |
| Graphify Cloud | Same engine, hosted; plus Memory notes | Syntactic, 63 grammars | Contributors | PR review only | Drive, Slack (Q&A); Jira "not available yet", Sentry early access |
| Sourcegraph | Trigram index + navigation graph | SCIP-precise + search-based | Yes | No | No |
| Unblocked | Vectors + people/PR/issue graph + relational | Not disclosed; no code graph described | Yes (expert graph) | Yes | Yes, broadest set |
| Copilot Spaces + Memory | Curated bundles + agent-written facts | None exposed | Via GitHub | Yes (GitHub) | Free text and uploads |
| Augment Context Engine | Chunks + embeddings | Proprietary models | Claimed, not documented | Via Cosmos triggers | Claimed |
| Greptile | Graph + embedded summaries, review-focused | Unnamed parser | No | Yes (review) | Jira/Confluence fetched per PR |
| Glean | Chunks + embeddings + Enterprise Graph | AST chunking for search; no call graph | Yes | Yes | Yes, 275+ connectors |
| Onyx | Chunks + embeddings (OpenSearch hybrid) | Source code excluded | No | Yes | Yes, 56 connectors |
| Atlassian Rovo | Teamwork Graph | Unconfirmed | Commits (metadata) | Yes | Yes (Atlassian + 50+ connectors) |
| Zep / Graphiti | Bi-temporal fact graph | None | No | No | Pushed text only |
| mem0 | Extracted facts as vectors (+ graph on paid platform) | None | No | No | Conversations only |
| Cognee | Graph + vectors | Symbols and dependencies (claimed) | Unclear | "Company Brain" claim | Claimed |

### 8.2 Retrieval, provenance, scope and freshness

The query models split three ways: **bounded structural operations**
(Yggdrasil's Verbs, Graphify's query/path/explain, Sourcegraph's
navigation tools), **ranked retrieval** (hybrid lexical and vector search), and
**synthesised answers with citations** (Unblocked, Glean, Graphify Cloud Ask,
Sourcegraph Deep Search). Only the structural tools return per-edge confidence;
the answer engines return document-level citations.

| Product | Query model | Per-claim provenance | Cross-repo / org-wide | Freshness |
|---|---|---|---|---|
| Yggdrasil M0 | 4 Verbs; lexical BM25 search | Every edge: provenance + confidence + `path:line:col` | Org-wide search fan-out | Poll (default 5 min), syntactic target p50 ≤ 8 min |
| Yggdrasil v1 (planned) | 12 Verbs incl. callers, impact, owners, path; hybrid search (M3) | Same, plus `precise` | Cross-repo edges via package monikers | Poll + webhooks, p50 ≤ 3 min |
| Graphify OSS | Lexical seed + BFS/DFS, path, explain; no full-text, no vectors | EXTRACTED/INFERRED/AMBIGUOUS + file:line | Manual graph merge | Hooks and watch on the developer's machine |
| Graphify Cloud | Graph tools incl. callers, impact, tests-for; Ask with citations | Same tags | 1–100 repo combined graphs | Every push to default branch, daily cap |
| Sourcegraph | Regex/keyword, semantic, symbol, commit/diff; agentic Deep Search | Precise vs search-based; Deep Search lists sources | Yes, at very large scale | Continuous |
| Unblocked | Hybrid retrieval + synthesised answer | Citations | Yes | Webhooks; code ≥ every 8 h |
| Copilot Spaces + Memory | Copilot chat over curated context | Memory facts cite code, re-checked | Per Space | Auto-updated |
| Augment | Semantic retrieval | File and line | Selected repositories | Seconds (per-user index) |
| Greptile | Review-time graph queries; substring KB search | Review comments | Pro tier | Webhooks |
| Glean | Hybrid search + chat + agents | Citations | Yes | Webhooks + 10-min incremental crawls |
| Onyx | Hybrid search + agentic RAG | Citations | Yes | 30-min connector refresh |
| Zep / Graphiti | Hybrid semantic + BM25 + graph | Fact → source episode | Per graph / group | On write |
| mem0 | Semantic + BM25 + entity | None beyond metadata | Per user/agent/session | On write |

### 8.3 Write path, trust and access control

This is the axis on which the field has moved most in 2026. Unblocked,
Copilot Memory, Graphify Cloud, Glean (beta) and Atlassian all now let agents
or people add knowledge, each with a different trust model. Yggdrasil's
derived graph is read-only; its Memory design is unscheduled.

| Product | Who can write knowledge | Trust handling | Mirrors source permissions? |
|---|---|---|---|
| Yggdrasil M0 | Nobody (derived only) | n/a | No — org-visible by design; private repos opt-in |
| Yggdrasil (designed, unscheduled) | Agents, as Memories | `asserted` provenance (lowest trust), Anchors to code for staleness detection, Promotion up a scope ladder | No (post-v1 direction) |
| Graphify OSS | Anyone who edits the committed `graph.json` | None | No (file or API key) |
| Graphify Cloud | Agents via `remember` | Not documented | Workspace roles only |
| Sourcegraph | Nobody | n/a | Yes, full |
| Unblocked | Org members via corrections; API documents | Verified against sources or attributed; DMs excluded; "strong hints" | Yes, but Enterprise-only and off by default |
| Copilot Memory | Copilot agents | Citations re-checked, voting, 90-day expiry; write access to manage | Yes (repository access) |
| Greptile | Agents create rules; learning from reviews | Rule status (active/suggested) | Not documented |
| Glean | Memory consolidated from activity (beta) | User-editable | Yes, full |
| Onyx | Nobody (connectors only) | n/a | Yes, Enterprise-licensed code |
| Atlassian Rovo | Agents write relationships/decisions (vendor claim) | Not documented | Yes |
| Zep / Graphiti | Any client of the API | Bi-temporal invalidation, episodes as provenance; reference servers have no auth | No; group-based access in Zep Cloud |
| mem0 | Any API-key holder | Add-only, ranking at read time | No |

### 8.4 Deployment, licence, price and maturity

| Product | Deployment | Data stores | Licence | Price (public) | Maturity signal |
|---|---|---|---|---|---|
| Yggdrasil | Self-hosted only | Postgres + S3-compatible store | AGPL-3.0 | Free | Pre-release, 0 stars, nearly all commits from one author |
| Graphify OSS | Local CLI; optional self-run HTTP MCP | Files | Apache-2.0 | Free | 151.8k stars, ~2.2M PyPI downloads/month |
| Graphify Cloud | SaaS; self-host Enterprise early access | Undisclosed | Proprietary | $0 / $15 / $40 per seat | Launched July 2026; YC S26 |
| Sourcegraph | Single-tenant cloud or self-hosted (K8s) | gitserver, Zoekt, 2× Postgres, observability | Proprietary (Zoekt, SCIP Apache-2.0) | From $16K/year | "200+ enterprise teams" |
| Unblocked | SaaS; on-prem Enterprise | Undisclosed | Proprietary | $29/user/month | $20M Series A |
| Copilot Spaces + Memory | SaaS | n/a | Proprietary | Included in Copilot plans | GitHub default |
| Augment | SaaS | GCP (Bigtable, PubSub) | Proprietary; connectors MIT | $20 / $100 flat + usage | Acquired by Harness Oct 2026 |
| Greptile | SaaS or self-hosted | Postgres/pgvector, Hatchet, RabbitMQ | Proprietary | $1/credit | $25M Series A |
| Glean | SaaS or customer cloud (Glean-managed) | Undisclosed | Proprietary | Not public | $7.2B valuation (2025) |
| Onyx | Self-hosted or cloud | Postgres, OpenSearch, Redis, object store, model servers | MIT + enterprise `ee/` | $20/user/month cloud | 33.5k stars |
| Atlassian Rovo | Atlassian Cloud | n/a | Proprietary | Included in paid plans (credits) | Atlassian-scale |
| Zep / Graphiti | Zep SaaS/BYOC; Graphiti self-hosted | Neo4j, FalkorDB or Neptune | Graphiti Apache-2.0 | Zep from $125/month | Graphiti 33.0k stars |
| mem0 | Library, self-hosted server, SaaS | Postgres/pgvector (or 15 vector stores) | Apache-2.0 | Not published | 69.3k stars |
| Cognee | Library or cloud | SQLite + LanceDB + graph DB | Apache-2.0 | $0 / $99 per month | 33.5k stars |

### 8.5 Agent and human interfaces

| Product | MCP | Other agent surfaces | Human interfaces |
|---|---|---|---|
| Yggdrasil M0 | Yes, authenticated Streamable HTTP + stdio proxy; 4 tools | Versioned Claude Code Skill | CLI only |
| Graphify OSS | Yes, stdio or HTTP; 11 tools | Assistant skills for many agents; git hooks | CLI, `graph.html`, report |
| Graphify Cloud | Yes, OAuth; ~20 tools incl. Memory | PR review bot | Web app, Slack |
| Sourcegraph | Yes, 16 tools | — | Web UI, `src` CLI |
| Unblocked | Yes (3 tools via CLI); REST API | Code agent (beta), PR review | Web, Mac app, Slack, Teams |
| Glean | Yes, managed remote | Agents | Web, assistants |
| Onyx | Yes (server and client) | Agents | Web, desktop, Slack, Discord |
| Zep / mem0 | Yes, hosted | SDKs | Dashboards |

## 9. Yggdrasil's position

This section separates what was **verified** (from the repository or a cited
primary source) from what is **inferred** (the author's judgement from those
facts).

### 9.1 What is distinctive (verified)

1. **Provenance and confidence on every edge, with ambiguity kept.** Yggdrasil
   records ambiguous syntactic matches as several edges with split confidence
   and reserves 1.0 for edges witnessed verbatim in source
   ([ADR 0006](../adr/0006-syntactic-ambiguity-spread-confidence.md)). Graphify
   also tags edges (EXTRACTED/INFERRED/AMBIGUOUS with scores) but *drops* calls
   that resolve to more than one candidate (§3.2). Sourcegraph distinguishes
   precise from search-based navigation but not per edge in a queryable graph.
   The answer engines (Unblocked, Glean, Onyx) cite documents, not edges.
2. **A server-side, continuously synced code graph that is open source and
   self-hostable.** Of the products surveyed that index code structure on a
   server: Sourcegraph is closed (since 2024) and from $16K/year; Sourcebot
   closed its source in October 2026; Graphify's hosted product is closed and
   its self-hosting is Enterprise early access; Greptile and Unblocked are
   proprietary. Graphify OSS is open but runs on a developer's machine and
   shares the graph by committing a JSON file. Onyx is open and self-hosted
   but deliberately does not index source code.
3. **Git history in the graph.** Yggdrasil's M0 graph includes `Commit` and
   `Contributor` nodes with `TOUCHES` and `AUTHORED` edges and a `history`
   Verb. Graphify OSS records only the commit it was built at.
4. **A storage design aimed at organisation scale.** Immutable per-repository
   Shards in object storage with a Postgres control plane and stateless query
   nodes ([ADR 0005](../adr/0005-object-storage-native-shards.md)). No other
   open project surveyed publishes a comparable design. This is a design
   claim: the 5000-repository certification is an M3 item and the only
   recorded evaluation is on a tiny fixture ([docs/EVAL.md](../EVAL.md)).
5. **A bounded Verb contract instead of a query language or a chat box**
   ([ADR 0003](../adr/0003-verb-contract-not-query-language.md)), identical
   across REST, MCP and CLI, and versioned with a shipped Skill. Graphify,
   Sourcegraph and Graphiti offer either free-form traversal, Cypher export or
   many loosely-typed tools.
6. **No LLM and no egress by default.** Shared with Graphify's code pass;
   unlike every answer engine surveyed, which sends content to model providers.

### 9.2 Table stakes it lacks today (verified)

Measured against what a "team brain" buyer would expect in late 2026:

- **Anything beyond code and commits.** Every company-wide and
  engineering-context product ingests PRs and issues; most ingest chat, docs
  and tickets. Yggdrasil M0 ingests none; Change Requests and issues are M2;
  chat and tickets are not on the roadmap at all.
- **Semantic retrieval.** Every hybrid product combines lexical and vector
  search; Yggdrasil's is M3 and its provider is undecided
  ([RFC 0001 §9, §11](../rfc/0001-yggdrasil-v1.md)).
- **Compiler-precise edges.** The core promise of ADR 0002 is M1. Today
  Yggdrasil's edges are syntactic, like Graphify's — the PRD's criticism of
  Graphify ("edges that can't be trusted for impact analysis") currently
  applies to Yggdrasil too. Sourcegraph ships precise cross-repository
  navigation now.
- **Callers, impact and orientation.** Graphify Cloud ships `callers`,
  `impact`, `tests_for` and cross-repo combined graphs; Graphify OSS ships
  `path` and `affected`. Yggdrasil's `callers`, `impact`, `map` and `path` are
  M1–M2 (its `neighbors` Verb can answer inbound `CALLS`, as the EVAL
  harness does).
- **Permission mirroring.** Glean, Sourcegraph, Onyx (enterprise code),
  Rovo, Copilot and Unblocked (enterprise) restrict results to what the asker
  can see in the source system. Yggdrasil is org-visible by design
  ([ADR 0001](../adr/0001-org-visible-access-model.md)); for an organisation
  whose private repositories have different audiences, this means either not
  indexing them or running several Index Servers.
- **A write path.** Unblocked, Copilot Memory, Graphify Cloud and Glean (beta)
  all let people or agents add knowledge. Yggdrasil has none, and its Memory
  design is not scheduled (§2.3).
- **Synthesised answers with citations, a web UI, chat integrations, webhooks,
  multiple forges.** Explicit non-goals or later milestones in the PRD; worth
  stating plainly because they are the default experience of the commercial
  products.
- **Evidence.** Graphify, Sourcegraph, Unblocked, Augment, Zep and mem0 all
  publish benchmark numbers (self-reported, often contested). Yggdrasil's only
  recorded result shows Verb payloads *larger* than grep-and-read on a small
  fixture ([docs/EVAL.md](../EVAL.md)). It is honest, but it is not yet an
  argument.

### 9.3 Who overlaps most with the roadmap (verified facts, inferred ranking)

- **Graphify Cloud overlaps most, and has shipped much of it already.** In
  four months it shipped cross-repo combined graphs, callers/impact/trace
  tools, Contributors, GitLab, an OAuth MCP server, Memory (`remember`/`recall`)
  and a wiki (§4.1). What it has not shipped is compiler-precise extraction
  (its SCIP ingester is an unwired skeleton), git-history-as-graph, full-text
  search over file bodies in the open engine, permission mirroring, or
  general-availability self-hosting. *Inference:* if Yggdrasil's M1 precise
  pass lands, precision plus open self-hosting is the clearest technical
  separation from Graphify; without it, the two are both syntactic graphs and
  Graphify has the users.
- **Sourcegraph overlaps on M1–M2 code intelligence** — SCIP, cross-repo
  references, commit and diff search, MCP — and is ahead there. It has
  stepped back from ownership (Own removed) and has no memory or non-code
  sources. *Inference:* Yggdrasil's design is close to "an open,
  agent-first, lighter Sourcegraph plus a history/people/forge graph", and the
  Sourcegraph and Sourcebot licence changes leave that niche less crowded
  than when the PRD was written.
- **Unblocked and Copilot Memory overlap on the M2 forge layer and the Memory
  design.** Unblocked already fuses code, PRs, issues, chat and expertise with
  citations; Copilot Memory already does agent-written, code-cited,
  re-verified, expiring repository facts — the nearest shipped thing to
  Yggdrasil's **Anchor** concept.
- **Zep/Graphiti overlaps conceptually on Memory semantics.** Its bi-temporal
  validity and invalidation-without-deletion is a mature model of
  "superseded" facts, which Yggdrasil's Semantic Memory definition needs
  ([CONTEXT.md](../../CONTEXT.md)) but does not yet specify.

### 9.4 Gaps in the field that the planned layers would fill (inference)

These are judgements, not findings:

1. **Memories anchored to a precise code graph, with staleness detection.**
   No surveyed product stores agent-asserted knowledge as nodes in the *same*
   provenance-ranked graph as compiler-derived edges and marks a memory stale
   when the Symbol it is anchored to changes. Copilot Memory's citation
   re-check is the closest shipped mechanism, but it is GitHub-only, SaaS-only
   and has no graph. Zep has temporal invalidation but no notion of code.
   This is the most differentiated idea in Yggdrasil's design documents — and
   it is currently only glossary text.
2. **Open, self-hosted, cross-repository impact analysis with explicit trust
   levels.** "What breaks across our repos if I change this?" answered with
   precise edges where builds succeed, labelled syntactic edges where they do
   not, and grouped by repository. Sourcegraph answers the reference part
   (closed, costly); Graphify answers it syntactically.
3. **History, ownership and Change Requests linked to Symbols, not just to
   documents.** The answer engines know who touched which PR; the code tools
   know which function calls which. The M2 schema (`AUTHORED`, `TOUCHES`,
   `OWNS`, `PART_OF`, `CLOSES`, `MENTIONS`) joins them in one graph.

Equally, three planned choices point *away* from where buyers are (inference):
the org-visible access model, the absence of chat and ticket sources from the
roadmap, and the absence of an answer-synthesis surface. These are coherent
for "an agent-first code graph" and limiting for "a team brain".

### 9.5 Corrections to Yggdrasil's own documents suggested by this research

- The PRD's description of Graphify ("builds a knowledge graph of one checkout,
  once … no server, no sync, no org view") is out of date: Graphify OSS now has
  watch mode, git hooks, an HTTP MCP server and graph merging, and Graphify
  Cloud syncs on push across up to 100 repositories ([docs/PRD.md](../PRD.md)
  "Problem"; §3–4 above).
- The PRD's Sourcegraph framing should note the closed core, the $16K entry
  price, Cody's end of life and the SCIP governance move (§5.1).
- The shipped Skill advertises Doc Sections, Dependencies, ownership and Forge
  Artifacts, which M0 does not extract (§2.3).
- Memories are defined in CONTEXT.md but absent from the PRD, the RFC
  provenance enum and the milestones (§2.3).

## 10. Open questions and unverified claims

- **Graphify:** the homepage's "11.4M downloads", "15.2M repositories indexed"
  and customer logos; how hosted Memory, Ask and Wiki work internally (closed);
  how `graphify_trace`'s "semantically similar" fallback retrieves; why the
  BENCHMARKS.md configuration (BGE-m3 embeddings, SurrealDB) differs from the
  shipped package and where its harness lives; whether hosted agents can use
  API tokens (docs contradict each other); funding amount.
- **Unblocked:** how source code is indexed (no code graph is described),
  on-premises architecture and data stores, permission-sync latency; the
  "76% fewer tokens" methodology.
- **Augment:** whether the Context Engine MCP and SDK continue as standalone
  products after the Harness acquisition.
- **Greptile:** which parser builds its graph; status of the former public
  codebase Query API (its docs URL now redirects).
- **Sourcegraph:** whether "Enterprise Starter" is still sold (docs and pricing
  disagree).
- **Glean:** ranking internals beyond code search; pricing; Azure hosting.
- **Onyx:** whether permission sync works in the MIT-licensed self-hosted
  edition (docs say "Business, Enterprise, and Cloud"; code sits under `ee/`).
- **Atlassian Rovo:** whether the GitHub connector indexes file contents.
- **Cognee:** whether "Company Brain" is generally available.
- **Dust:** current licence (its former public repository returns 404).
- **mem0:** platform prices (not published).
- **Cursor:** current semantic indexing and team rules behaviour.
- **Yggdrasil:** how deep syntactic resolution goes for languages other than
  Go (ADR 0006 describes Go's rules in detail; the other five language packs
  were not audited line by line); no measured fleet re-index duration.
- **General:** all vendor benchmark figures in this note are self-reported;
  none was independently reproduced. The Zep–mem0 LoCoMo dispute (§7.1) shows
  how far such numbers can move under re-scoring.

## 11. Sources

Accessed 2026-11-22 unless stated. Repository paths for Yggdrasil are relative
to this repository at commit `5ca6d17`.

**Yggdrasil (local):** README.md; CONTEXT.md; docs/PRD.md; docs/ARCHITECTURE.md;
docs/rfc/0001-yggdrasil-v1.md; docs/adr/0001–0006; docs/DEPLOYMENT.md;
docs/EVAL.md; TODO-ISSUES.md; crates/yg-shard/src/lib.rs;
crates/yg-index/src/pass/; crates/yg-index/Cargo.toml;
crates/yg-sync/src/forge/; crates/yg-control/migrations/;
crates/yg-api/src/auth.rs; crates/yg-cli/src/main.rs;
crates/yg-cli/skills/yggdrasil-navigation/SKILL.md.

**Graphify:** repository https://github.com/Graphify-Labs/graphify (branch
`v8`, commit `3671f31`): README.md, ARCHITECTURE.md, BENCHMARKS.md,
CHANGELOG.md, docs/how-it-works.md, graphify/serve.py, symbol_resolution.py,
scip_ingest.py, validate.py, export.py, llm.py, detect.py, prs.py,
__main__.py, skills/claude/references/extraction-spec.md;
https://pypi.org/pypi/graphifyy/json; https://pypistats.org/api/packages/graphifyy/recent;
docs at https://docs.graphify.com — concepts/architecture,
concepts/how-graphify-finds-dependencies, concepts/provenance,
open-source/keep-the-graph-fresh, open-source/self-host,
open-source/features/{incremental-updates, mcp-tools, token-benchmark,
honest-token-savings}, cookbooks/{shared-team-graph, self-host-shared-mcp,
pr-impact-comment-ci}, guides/pull-requests, platform/{how-it-works, graphs,
connectors, verification, graphify-mcp, graphify-vs-local,
keep-the-graph-fresh, workspaces-and-members}, platform/support/{pricing,
security}, agents/{capabilities, headless}, changelog/highlights;
https://graphify.com; https://graphify.com/pricing; https://graphify.com/about;
https://graphify.com/benchmarks; https://graphify.com/blog/graph-memory-benchmark-results.

**Sourcegraph:** https://sourcegraph.com/docs/admin/architecture;
https://sourcegraph.com/docs/code-navigation; https://sourcegraph.com/docs/deep-search;
https://sourcegraph.com/docs/code-search/types/deep-search;
https://sourcegraph.com/docs/api/mcp; https://sourcegraph.com/docs/admin/permissions;
https://sourcegraph.com/docs/admin/deploy; https://sourcegraph.com/changelog;
https://sourcegraph.com/changelog/7-0-removals-deprecations;
https://sourcegraph.com/pricing;
https://sourcegraph.com/blog/changes-to-cody-free-pro-and-enterprise-starter-plans;
https://sourcegraph.com/blog/sunsetting-cody-future-of-code-intelligence;
https://sourcegraph.com/blog/why-sourcegraph-and-amp-are-becoming-independent-companies;
https://github.com/sourcegraph/sourcegraph-public-snapshot;
https://github.com/scip-code/scip; https://scip-code.org/governance.

**Unblocked:** https://getunblocked.com/blog/inside-the-unblocked-context-engine/;
https://docs.getunblocked.com/llms.txt; https://docs.getunblocked.com/data-sources/github;
https://docs.getunblocked.com/data-sources/custom-mcp;
https://docs.getunblocked.com/unblocked-cli/cli-query;
https://docs.getunblocked.com/unblocked-chat/cited-sources;
https://docs.getunblocked.com/unblocked-chat/shared-memories;
https://docs.getunblocked.com/team-settings/data-shield;
https://docs.getunblocked.com/unblocked-mcp/mcp-overview;
https://docs.getunblocked.com/security-and-data-handling;
https://docs.getunblocked.com/billing/how-licensing-works;
https://docs.getunblocked.com/changelog; https://getunblocked.com/pricing;
https://getunblocked.com/blog/series-a/.

**Augment Code:** https://www.augmentcode.com/blog/a-real-time-index-for-your-codebase-secure-personal-scalable;
https://www.augmentcode.com/context-engine;
https://docs.augmentcode.com/context-services/mcp/overview;
https://docs.augmentcode.com/context-services/context-connectors/how-it-works;
https://docs.augmentcode.com/cosmos/experts-memory;
https://www.augmentcode.com/pricing; https://augmentcode.com/blog/augment-code-next-chapter;
https://www.harness.io/press-and-news/harness-acquires-augment-code;
https://github.com/augmentcode/context-connectors.

**Greptile:** https://www.greptile.com/docs/how-greptile-works/graph-based-codebase-context;
https://www.greptile.com/docs/self-hosting/system-architecture;
https://www.greptile.com/docs/self-hosting/deployment-options;
https://www.greptile.com/docs/mcp/tools; https://www.greptile.com/docs/mcp/custom-context;
https://www.greptile.com/docs/how-greptile-works/memory-and-learning;
https://www.greptile.com/pricing; https://www.greptile.com/blog/series-a.

**GitHub Copilot:** https://docs.github.com/en/copilot/concepts/context/spaces;
https://docs.github.com/en/copilot/concepts/agents/copilot-memory;
https://github.blog/ai-and-ml/github-copilot/building-an-agentic-memory-system-for-github-copilot/;
https://docs.github.com/en/copilot/get-started/plans.

**Other engineering tools:** https://docs.sourcebot.dev/docs/features/mcp-server;
https://www.sourcebot.dev/blog/sourcebot-is-now-closed-source;
https://github.com/sourcebot-dev/sourcebot/pull/1721;
https://docs.devin.ai/work-with-devin/deepwiki-mcp;
https://docs.coderabbit.ai/knowledge-base/learnings; https://github.com/potpie-ai/potpie;
https://cursor.com/blog/secure-codebase-indexing.

**Glean:** https://www.glean.com/product/enterprise-graph; https://www.glean.com/pricing;
https://docs.glean.com/connectors/native/github/about;
https://docs.glean.com/security/how-code-search-works; https://docs.glean.com/connectors/about;
https://docs.glean.com/administration/platform/mcp/about;
https://docs.glean.com/release-notes/releases/2026-04-08-april-release;
https://www.glean.com/blog/memory-enterprise-context;
https://help.glean.com/en/articles/10093412-glean-deployment-options;
https://www.glean.com/blog/glean-series-f-announcement.

**Onyx:** https://github.com/onyx-dot-app/onyx; https://github.com/onyx-dot-app/onyx/pull/15336;
https://github.com/onyx-dot-app/onyx/blob/main/backend/onyx/document_index/opensearch/README.md;
https://github.com/onyx-dot-app/onyx/blob/main/backend/onyx/connectors/github/connector.py;
https://github.com/onyx-dot-app/onyx/blob/main/deployment/docker_compose/docker-compose.yml;
https://github.com/onyx-dot-app/onyx/blob/main/LICENSE;
https://docs.onyx.app/admins/connectors/overview;
https://docs.onyx.app/overview/onyx_anywhere/mcp_server; https://www.onyx.app/pricing;
https://onyx.app/blog/seed-round.

**Atlassian, Notion, Dust:** https://www.atlassian.com/blog/company-news/closing-the-ai-context-gap;
https://www.atlassian.com/software/rovo/connectors/github;
https://support.atlassian.com/organization-administration/docs/teamwork-graph-connector-types/;
https://github.com/atlassian/atlassian-mcp-server;
https://support.atlassian.com/rovo/docs/rovo-usage-limits/;
https://support.atlassian.com/rovo/docs/rovo-dev-rollout/;
https://www.notion.com/pricing; https://www.notion.com/help/notion-ai-connectors;
https://www.notion.com/help/enterprise-search; https://dust.tt/home/pricing;
https://docs.dust.tt/docs/user-documentation/data-sources/overview.

**Zep and Graphiti:** https://github.com/getzep/graphiti;
https://github.com/getzep/graphiti/blob/main/graphiti_core/edges.py;
https://github.com/getzep/graphiti/pull/2151; https://help.getzep.com/memory-mcp-server;
https://help.getzep.com/enterprise-context-graphs; https://www.getzep.com/pricing;
https://www.getzep.com/blog/announcing-a-new-direction-for-zeps-open-source-strategy/;
https://arxiv.org/abs/2501.13956; https://github.com/getzep/zep-papers/issues/5;
https://www.letta.com/blog/benchmarking-ai-agent-memory.

**mem0, Cognee, Letta, supermemory, others:** https://github.com/mem0ai/mem0;
https://docs.mem0.ai/migration/oss-v2-to-v3; https://github.com/mem0ai/mem0/pull/6530;
https://github.com/mem0ai/mem0/blob/main/server/docker-compose.yaml;
https://mem0.ai/pricing; https://mem0.ai/series-a; https://arxiv.org/abs/2504.19413;
https://github.com/topoteretes/cognee; https://docs.cognee.ai/core-concepts/architecture;
https://www.cognee.ai/company-brain; https://www.cognee.ai/pricing;
https://www.cognee.ai/blog/cognee-news/cognee-raises-seven-million-five-hundred-thousand-dollars-seed;
https://github.com/letta-ai/letta; https://github.com/letta-ai/letta-code;
https://docs.letta.com/letta-code/memory; https://docs.letta.com/letta-code/pricing;
https://github.com/supermemoryai/supermemory; https://supermemory.ai/pricing;
https://github.com/microsoft/graphrag; https://github.com/campfirein/byterover-cli;
https://github.com/basicmachines-co/basic-memory.
