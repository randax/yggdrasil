# Parity matrix: Yggdrasil vs Graphify, Sourcegraph, Copilot Memory and Unblocked

Status: research note · Date: 2026-11-23 · Ticket: #122 (map #121) ·
Yggdrasil baseline: commit `c4bec0c` on `main`

> Scope. This note compares agent-facing graph and Memory capabilities only:
> MCP tools and Verbs, search modes, language coverage, cross-repo graphs,
> freshness, MCP auth and Memory semantics. It builds on
> [team-brain-landscape.md](team-brain-landscape.md) (2026-11-22). Rows taken
> from that note without a fresh fetch cite it as **[L §n]**. Everything else
> was re-checked on 2026-11-23 against first-party docs, source or this
> repository. Citation keys such as [GC-cap] resolve in §5.

## 1. Answer in brief

- **Graphify Cloud is the parity bar, and it ships today what Yggdrasil plans
  for M1–M2:** callers, impact, trace, tests-for, imports/exports, file
  ranking, cross-repo combined graphs, OAuth MCP and Memory [GC-cap]
  [GC-mcp]. Yggdrasil has only four Verbs: `search`, `node`, `neighbors` and
  `history` ([`Verb::ALL`][Y-verbs]).
- **Graphify Cloud's Memory is closer to ADR 0007 than ADR 0007 says.**
  Graphify's Memory page documents three things:
  - Facts are "grounded when anchored to a code node that still resolves".
  - A stale flag fires when "the code this refers to changed after it was
    saved".
  - Agent memories "start as suggestions" that Ask uses only "after someone
    keeps" them, and memories "don't expire with age" [GC-mem].

  ADR 0007 calls Graphify Cloud's Memory internals "undocumented". That is now
  out of date. What Graphify still lacks:
  - a precise graph;
  - a documented change-detection mechanism;
  - a private-to-shared scope ladder;
  - an audit or erase log;
  - self-hosting outside Enterprise early access.
- **Three agent-facing tools have no Yggdrasil plan at all:** a tests-for
  lookup, community detection and regex search. Two more are only partly
  covered: question-to-files ranking (until M3 semantic search) and export
  edges.
- **Semantic retrieval is the most widespread capability Yggdrasil lacks.**
  Every peer except Graphify OSS has some form of it:
  - Sourcegraph `nls_search` [SG-mcp];
  - Copilot's semantic code-search index [GH-idx];
  - Unblocked's vector store [L §5.2];
  - Graphify Cloud's "semantic and keyword" Memory recall and
    `graphify_rank_files` [GC-mem] [GC-cap].
- **MCP auth is a gap against every hosted peer.** Graphify Cloud uses OAuth
  with PKCE, Sourcegraph OAuth with Dynamic Client Registration, GitHub's MCP
  server OAuth or a PAT, and Unblocked a browser OAuth sign-in. Yggdrasil has
  bearer Member tokens and puts OIDC post-v1 ([RFC 0001 §8][Y-rfc]).
- **Language breadth is the starkest numeric gap.** Yggdrasil parses 6
  languages syntactically and plans precise indexing for 4 language families
  in M1. Graphify parses about 40 languages (OSS) or 63 grammars (Cloud), and
  Sourcegraph covers 40 search-based plus 8 precise indexer families
  [SG-nav] [SG-precise].
- **Where Yggdrasil still leads or will lead:**
  - provenance and confidence on every edge, with ambiguity kept;
  - git history in the graph (shipped);
  - transitive, repo-grouped `impact` (M1), where Graphify Cloud's impact
    tool returns direct callers and callees only [GC-cap];
  - compiler-precise edges (M1), which neither Graphify product has;
  - code-driven Memory staleness tied to the Shard swap, with a human gate,
    step-up and audit (M2).

## 2. The matrix

Key for the Yggdrasil columns: **have** = shipped in M0 and verified in the
repository; **partial** = shipped, but covering only part of the capability;
**planned (M#)** = in the PRD or RFC milestones; **gap** = neither shipped nor
planned. "Map #121" marks a direction set on the wayfinder map that is not yet
in a milestone. The competitor columns describe documented behaviour, and "—"
means none was found in that product's docs.

### 2.1 Graph tools and Verbs

| Capability | Yggdrasil shipped (M0) | Yggdrasil planned | Graphify OSS | Graphify Cloud | Sourcegraph | Copilot Memory + Spaces | Unblocked |
|---|---|---|---|---|---|---|---|
| Find a symbol or definition | **have**: `search` over Symbol names, plus `node` with fuzzy-id candidates [Y-verbs] [L §2.2] | precise ids **planned (M1)** | `get_node`, `query_graph` [GO-mcp] | `graphify_find`, `graphify_node` [GC-cap] | `go_to_definition` [SG-mcp] | none structural; `search_code` in GitHub MCP [GH-mcp] | — (answers via `context_research`) [UB-mcp] |
| One-hop neighbourhood | **have**: `neighbors`, depth ≤ 3, edge-kind filter, hub caps [L §2.2] | — | `get_neighbors` [GO-mcp] | `graphify_node`, `graphify_file_neighbors` [GC-cap] | — | — | — |
| Callers and callees | **partial**: inbound `CALLS` through `neighbors`, syntactic only [L §9.2] | `callers`, `callees` **planned (M1)** [Y-rfc] | `get_neighbors` filtered by relation; CLI `affected` [GO-mcp] [L §3.4] | `graphify_callers`: resolved edges by default, `strict_calls=false` adds inferred ones [GC-cap] | `find_references` [SG-mcp] | — | — |
| Impact / blast radius | **gap** | `impact` **planned (M1)**: transitive, Symbols→Files→Repos, provenance-annotated [Y-rfc]; PR impact report (map #121) | CLI `affected`; `get_pr_impact` [GO-mcp] [L §3.4] | `graphify_impact`: *direct* callers and callees, with an optional `include_risk` hotspot ranking [GC-cap] | — | — | — |
| Trace / path A→B | **gap** | `path` **planned (M2)**: shortest paths with edge provenance [Y-rfc] | `shortest_path` [GO-mcp] | `graphify_trace`: directed call paths, may fall back to a "semantically similar node" [GC-cap] | — | — | — |
| Tests for a symbol or file | **gap** | **gap** | — | `graphify_tests_for` (lists tests, doesn't run them) [GC-cap] | — | — | — |
| Rank files for a question | **gap** | `map` **planned (M1)** (centrality, not question-driven); semantic `search` **planned (M3)** | `query_graph` (lexical seeds) [GO-mcp] | `graphify_rank_files` [GC-cap] | `code_finder` (agentic), `nls_search` [SG-mcp] | semantic code search [GH-idx] | `context_research` [UB-mcp] |
| Imports / exports | **partial**: `IMPORTS` edges through `neighbors`; no export edges [Y-shard] | package-level `deps`/`dependents` **planned (M2)**; file exports **gap** | `imports` relation [L §3.3] | `graphify_imports_exports` [GC-cap] | — | — | — |
| Communities / god nodes | **gap** | god nodes: `map` landmark Symbols by centrality **planned (M1)**; communities **gap** | `get_community`, `god_nodes` (Leiden) [GO-mcp] [L §3.2] | not in hosted tool docs (unverified) [GC-cap] | — | — | — |
| Repo orientation | **gap** | `map` **planned (M1)** [Y-rfc] | `GRAPH_REPORT.md`, `graph_stats` [L §3.1] | `graph_stats`, Wiki [GC-cap] [L §4.1] | — (`deepsearch`) | curated Spaces [GH-spaces] | — |
| History / contributors | **have**: `history` (commits touching a file or a Symbol's defining file) [L §2.2] | identity-merged Contributors, `owners` **planned (M2)** | — (records only the built-at commit) [L §3.3] | Contributors connector [L §4.1] | `commit_search`, `diff_search`, `compare_revisions`, `get_contributor_repos` [SG-mcp] | through GitHub MCP, not Memory [GH-mcp] | expert graph from PRs and commits [L §5.2] |
| Ownership | **gap** | `owners` (CODEOWNERS) **planned (M2)**; Teams (map #121) | — | unverified | removed in 7.0 (Own) [L §5.1] | — | expert graph [L §5.2] |
| Change Requests | **gap** | Forge Artifacts **planned (M2)**; CR-head indexing and PR impact report (map #121) | `list_prs`, `get_pr_impact`, `triage_prs` (live via `gh`) [GO-mcp] | `review_status`, PR review [GC-cap] | `diff_search`, `compare_revisions` [SG-mcp] | PRs and issues as Space sources [GH-spaces] | PRs, issues [L §5.2] |
| Per-edge provenance | **have**: `precise\|syntactic\|extracted\|inferred` plus confidence and `path:line:col`; ambiguity kept with spread confidence [L §2.2] | `precise` **planned (M1)** | `EXTRACTED\|INFERRED\|AMBIGUOUS` plus score; ambiguous calls dropped [L §3.2] | same tags [L §3.4] | precise vs search-based, per result [SG-nav] | — | document citations [L §5.2] |
| Synthesised answer | **gap**, by design | Ask as a shipped agent over the Verbs (map #121), not a Verb | — (no LLM at query time) [L §3.4] | Ask with citations, abstains [L §4.1] | `deepsearch` [SG-mcp] | Copilot chat over Spaces [GH-spaces] | `context_research` with citations [UB-mcp] |

### 2.2 Search modes

| Capability | Yggdrasil shipped (M0) | Yggdrasil planned | Graphify OSS | Graphify Cloud | Sourcegraph | Copilot Memory + Spaces | Unblocked |
|---|---|---|---|---|---|---|---|
| Lexical / full-text | **have**: BM25 (tantivy) over Symbol names and File contents, normalised per repo before the cross-repo merge [Y-fts] [Y-rfc] | full text over docs, commits and artifacts: G10 goal, no milestone [Y-prd] | IDF scoring over labels, paths and rationale; no full-text over file bodies [L §3.4] | `search`/`fetch`, `graphify_find` (symbol-name match); modes undocumented [GC-cap] | `keyword_search` (Zoekt trigram) [SG-mcp] | `search_code` (GitHub code search with qualifiers) [GH-mcp] | undisclosed |
| Regex | **gap** | **gap** | — | undocumented | yes (regex and keyword search) [L §5.1] | regex in GitHub-intelligence code search [GH-intel] | undisclosed |
| Semantic / hybrid | **gap**; `search` rejects `semantic` and `hybrid` although the MCP schema enum lists them [Y-search] | **planned (M3)**: opt-in, local embeddings by default [Y-prd] | none: "No embeddings, no vector store" [L §3.4] | semantic plus keyword Memory recall [GC-mem]; semantic fallback in `trace` [GC-cap] | `nls_search` [SG-mcp] | semantic code-search index [GH-idx] | vectors plus graph plus relational retrieval [L §5.2] |
| Commit / diff search | **partial**: `history` by file or Symbol; commit messages not full-text indexed [Y-fts] | G10 goal, no milestone [Y-prd] | — | — | `commit_search`, `diff_search` [SG-mcp] | through GitHub MCP | PR and commit history [L §5.2] |

### 2.3 Language coverage

| Capability | Yggdrasil shipped (M0) | Yggdrasil planned | Graphify OSS | Graphify Cloud | Sourcegraph | Copilot Memory + Spaces | Unblocked |
|---|---|---|---|---|---|---|---|
| Syntactic parsing | **have**: 6 languages (Go, Java, JavaScript, TypeScript, Python, Rust) [Y-grammars] | no further languages planned | ~40 (33 tree-sitter grammars plus 7 built-in parsers) [L §3.2] [GO-lang] | 63 grammars [L §4.1] | 40 languages, search-based [SG-nav] | n/a (semantic index, language-agnostic) | undisclosed [L §10] |
| Compiler-precise | **gap** | SCIP for Go, TS/JS, Python, Rust **planned (M1)** [Y-prd]; Java, C/C++, C#, Ruby, Kotlin **gap** | SCIP ingester is an unwired skeleton [L §3.2] | none documented | GA: Go, TS/JS, C/C++/CUDA, Java/Kotlin/Scala, Rust, Python, Ruby, C#/VB [SG-precise] | — | — |

### 2.4 Cross-repo, freshness, MCP auth and visibility

| Capability | Yggdrasil shipped (M0) | Yggdrasil planned | Graphify OSS | Graphify Cloud | Sourcegraph | Copilot Memory + Spaces | Unblocked |
|---|---|---|---|---|---|---|---|
| Org-wide query | **have**: `search` fans out across every indexed repo [Y-rfc] | — | manual `merge-graphs` / global graph [L §3.5] | combined graphs of 1–100 repos, queryable once a merge is promoted [GC-mcp] [L §4.1] | org-scale [L §5.1] | Spaces bundle repos; code search "across related projects" [GH-spaces] [GH-intel] | org-wide [L §5.2] |
| Cross-repo edges | **gap** | via manifests and SCIP monikers **planned (M2)** [Y-prd] | merge passes add cross-repo calls and `same_type_as` [L §3.5] | `calls`, `same_type_as` [L §4.1] | precise "cross-repository navigation" [SG-nav] | — | — |
| Freshness | **have**: poll every 5 min by default; target p50 ≤ 8 min to syntactic layers [Y-prd] | precise layer when its build lands **planned (M1)**; webhooks, p50 ≤ 3 min **planned (M3)** | local hooks and watch; content-hash incremental [L §3.5] | every push to the default branch, under a daily build cap [L §4.1] | continuous; precise needs CI upload or auto-indexing [SG-precise] | Space sources auto-update; index refreshed "within seconds" of a new chat; Memory citations checked against the current branch [GH-spaces] [GH-idx] [GH-mem] | webhooks for PRs and conversations; code at least every 8 h [L §5.2] |
| Branch scope | default branch | CR heads post-v1, pulled forward by map #121 | working tree | default branch only [L §4.1] | default branch for search [L §5.1] | current branch for Memory checks [GH-mem] | default branch [L §5.2] |
| MCP transport | **have**: Streamable HTTP plus `yg mcp` stdio proxy [Y-rfc] | — | stdio or HTTP [L §3.4] | Streamable HTTP at `api.graphify.com/mcp` [GO-mcp] | remote MCP [SG-mcp] | remote or local GitHub MCP server [GH-mcp] | CLI-run MCP server; transport unspecified [UB-mcp] |
| MCP auth (OAuth) | **gap**: bearer Member tokens (Admin-issued, hashed, expiring, revocable) [Y-rfc] | OIDC post-v1 [Y-rfc]; OIDC/SSO web login (map #121), MCP OAuth not specified | API key over HTTP; OAuth "a deliberate follow-up" [L §3.4] | OAuth with PKCE, public clients only, rotating refresh tokens, scope `graphify:query`, 240 req/min per IP [GC-mcp] | OAuth 2.0 with DCR (Admin can disable) or pre-registered clients, plus access tokens [SG-mcp] | OAuth or PAT bearer [GH-mcp] | browser OAuth sign-in; Okta through Claude managed auth [UB-mcp] |
| Per-user visibility | org-visible by design [L §2.2] | Teams as a visibility boundary (map #121, reopens ADR 0001) | whoever holds the file or key [L §3.5] | workspace roles [L §4.3] | mirrors code-host permissions [L §5.1] | repository access [GH-mem] | Data Shield, Enterprise-only [L §5.2] |

### 2.5 Memory semantics

| Capability | Yggdrasil shipped (M0) | Yggdrasil planned | Graphify OSS | Graphify Cloud | Sourcegraph | Copilot Memory + Spaces | Unblocked |
|---|---|---|---|---|---|---|---|
| Write path | **gap** | `remember` (create, supersede, affirm) by human or agent Members **planned (M2)** [Y-rfc] | `graphify save-result` writes Markdown files to `graphify-out/memory/` [GO-mem] | `remember` over MCP or HTTP; humans add in the app; automatic review, fix and merge records [GC-mem] [GC-cap] | none [SG-mcp] | Copilot agents create memories (`store_memory`, root agent only in the CLI); users prompt or bulk-import; no SDK API [GH-mem] | learned from corrections in shared conversations; no MCP write tool [UB-mem] [UB-mcp] |
| Read path | **gap** | `recall` by anchor, scope or full-text query, separate from derived Verbs **planned (M2)** [Y-prd] | lesson hints in query and explain output [GO-mem] | `recall`, with paths and symbols as ranking hints, not filters [GC-cap] | — | applied implicitly across Copilot surfaces; no MCP or API read [GH-mem] | applied in Unblocked answers; MCP read undocumented [UB-mem] |
| Anchoring to code | **gap** | Anchors to Symbol, File, Repo or Doc Section, with a content fingerprint **planned (M2)** [Y-ctx] | `reflect` overlay marks graph nodes preferred, tentative or contested [GO-mem] | "anchored to a code node that still resolves" [GC-mem] | — | citations to code [GH-mem] | — |
| Staleness | **gap** | flagged stale in the Shard-pointer-swap transaction when a fingerprint changes or a node disappears **planned (M2)** [Y-adr7] | signal weight halves every 30 days [GO-mem] | flags "code … changed after it was saved"; mechanism undocumented [GC-mem] | — | citations re-checked against the current branch before each use, at Copilot's judgment [GH-mem] | conflicts weighed against sources [UB-mem] |
| Expiry | n/a | Semantic: never; Episodic: session-bound **planned (M2)** [Y-ctx] | decay only [GO-mem] | "don't expire with age" [GC-mem] | — | 90 days by default, configurable 7–180; reset on upvote [GH-mem] | not stated [UB-mem] |
| Sharing gate | n/a | private on write; Promotion audited; an agent's promote becomes a proposal to a human reviewer; step-up for human publishing acts **planned (M2)** [Y-adr7] | commit the files [GO-mem] | agent memories are "suggestions" until someone keeps them; conflicting versions are used by nobody [GC-mem] | — | repository facts shared with collaborators; writing needs write access [GH-mem] | shared org-wide at once; DMs and private channels excluded [UB-mem] |
| Scopes | n/a | session → repo → org [Y-ctx] | per checkout | workspace [GC-mem] | — | repository facts plus user preferences [GH-mem] | organisation [UB-mem] |
| Trust labelling | n/a | `asserted` trust, author, staleness; bodies returned as untrusted data [Y-adr7] | "verify before relying" [GO-mem] | "A code link does not establish that the claim is correct" [GC-mem] | — | votes; "not a guarantee" [GH-mem] | verified vs attributed to a person [UB-mem] |
| Removal and audit | n/a | `retract`, Admin erase, append-only object-storage log **planned (M2)** [Y-adr7] | delete the files | removal is restorable and history kept; no export [GC-mem] | — | deleted in repository settings (write access) [GH-mem] | anyone corrects or removes it in a shared channel [UB-mem] |

## 3. Gaps

Each item is a Yggdrasil gap against at least one peer, with the most
consequential first.

1. **MCP OAuth.** Graphify Cloud, Sourcegraph, GitHub and Unblocked all offer
   OAuth to MCP clients. Yggdrasil requires a bearer token and puts OIDC
   post-v1. Map #121's OIDC web login does not yet cover MCP.
2. **Semantic and question-driven retrieval.** This is M3 and covers
   `rank_files`-style queries, semantic `search` and fuzzy `recall`. Every
   hosted peer has it today.
3. **Callers, impact, path and trace.** These are planned for M1–M2, and
   Graphify Cloud ships them now. Its `graphify_impact` is one hop, so
   Yggdrasil's planned transitive, repo-grouped `impact` is a real
   differentiator once it lands.
4. **Language breadth.** Yggdrasil parses 6 languages syntactically against
   about 40 (Graphify OSS, Sourcegraph) or 63 (Graphify Cloud). Its M1
   precise list leaves out Java, which it already parses syntactically, along
   with C/C++, C#, Ruby and Kotlin, all of which Sourcegraph covers precisely.
5. **Cross-repo edges.** These are M2; Graphify Cloud and Sourcegraph ship
   them now.
6. **Tests-for: unplanned.** Only Graphify Cloud has it. A `TESTS` edge or a
   test-file heuristic would be needed.
7. **Communities: unplanned.** Graphify OSS has Leiden communities. The M1
   `map` covers god nodes, not clusters.
8. **Regex search: unplanned.** Sourcegraph and GitHub code search have it.
9. **Export edges and commit-message full text: unplanned or unscheduled.**
   G10 lists commits in the full-text core but gives no milestone.
10. **Push-driven freshness.** This is M3 (webhooks). Graphify Cloud rebuilds
    on every push, and Unblocked uses webhooks for PRs.
11. **Hygiene.** The MCP `search` schema advertises `semantic` and `hybrid`
    modes that the Verb rejects [Y-search]. By the repo rule on advertised
    features, the enum should list only `lexical` until M3.

Not gaps, but relevant to the positioning in ADR 0007:

- Graphify Cloud's documented Memory already has anchors, a staleness flag, a
  human keep gate and no time expiry [GC-mem].
- What Yggdrasil keeps as separation:
  - fingerprint-based staleness applied transactionally with the Shard swap;
  - anchors to a precise graph;
  - private-to-shared Promotion with step-up;
  - retraction and erase logging;
  - self-hosting and AGPL.

## 4. Unverified claims

- **Graphify Cloud, open questions:**
  - Its full MCP tool list: the docs say to trust `tools/list`, which needs a
    signed-in session [GC-mcp].
  - Whether communities or god nodes are exposed through hosted tools.
  - The search mode behind `search`.
  - How `graphify_trace`'s "semantically similar" fallback retrieves.
  - Whether staleness compares content or only checks that a node resolves.
  - Which plans include Memory.
- **Graphify Cloud, carried from [L] without a re-fetch:**
  - 63 grammars;
  - rebuild on default-branch pushes only, under a daily cap;
  - the 1–100 repo limit for combined graphs.
- **Graphify OSS, carried from [L] without a re-fetch:** HTTP MCP auth by API
  key (from `serve.py`) and the "~40 languages" count. The current languages
  page gives no total and lists about 30 code languages plus formats
  [GO-lang].
- **Sourcegraph:**
  - Freshness specifics for the default-branch search index.
  - Whether precise navigation is cross-repo for every listed indexer; the
    navigation page says so only in general [SG-nav].
  - Amp's current use of Sourcegraph context was not assessed: Amp has been
    an independent company since 2025-12, and Cody reaches end of life on
    2027-01-31 [L §5.1].
- **Copilot:**
  - Whether Memory is reachable over MCP (no docs found).
  - The release status of the "Code Search" capability in GitHub
    intelligence [GH-intel].
  - The retrieval method Spaces use over attached repos.
- **Unblocked:**
  - The exact MCP auth and transport.
  - Whether MCP clients can read Shared Memories.
  - How code is indexed and for which languages.
  - The plan tier for Shared Memories.
- **Yggdrasil:** the depth of syntactic resolution outside Go was not
  audited [L §10].
- **All vendors:** every vendor capability above is as documented by the
  vendor. None was exercised against a live account.

## 5. Sources

Accessed 2026-11-23 unless marked [L].

- [L]: [team-brain-landscape.md](team-brain-landscape.md), sections as cited (accessed 2026-11-22).
- [Y-verbs]: `crates/yg-verbs/src/lib.rs` (`Verb::ALL`, line 101; `SEARCH_MODE_VALUES`, line 80).
- [Y-search]: `crates/yg-verbs/src/search/mod.rs` (mode gate: "only \"lexical\" is supported").
- [Y-fts]: `crates/yg-shard/src/fts.rs` (the full-text segment indexes Symbol and File nodes).
- [Y-shard]: `crates/yg-shard/src/lib.rs` (`NodeKind`, `EdgeKind`).
- [Y-grammars]: `crates/yg-index/Cargo.toml`; `crates/yg-index/src/pass/`.
- [Y-rfc]: [RFC 0001](../rfc/0001-yggdrasil-v1.md) §7 Verb table, §8 auth, §12 rollout.
- [Y-prd]: [PRD](../PRD.md) goals G2, G9, G10, G15–G17, and milestones.
- [Y-adr7]: [ADR 0007](../adr/0007-code-anchored-team-brain.md).
- [Y-ctx]: [CONTEXT.md](../../CONTEXT.md) (Memory, Anchor, Promotion).
- [GO-mcp]: https://docs.graphify.com/open-source/features/mcp-tools.md
- [GO-lang]: https://docs.graphify.com/open-source/features/languages.md
- [GO-mem]: https://docs.graphify.com/open-source/features/work-memory.md
- [GC-cap]: https://docs.graphify.com/agents/capabilities
- [GC-mcp]: https://docs.graphify.com/platform/graphify-mcp
- [GC-mem]: https://docs.graphify.com/platform/memory.md
- [SG-mcp]: https://sourcegraph.com/docs/api/mcp
- [SG-nav]: https://sourcegraph.com/docs/code-navigation
- [SG-precise]: https://sourcegraph.com/docs/code-navigation/precise-code-navigation
- [GH-mem]: https://docs.github.com/en/copilot/concepts/agents/copilot-memory
- [GH-spaces]: https://docs.github.com/en/copilot/concepts/context/spaces
- [GH-idx]: https://docs.github.com/en/copilot/concepts/context/repository-indexing
- [GH-intel]: https://github.blog/changelog/2026-10-28-github-intelligence-puts-your-code-context-and-history-to-work
- [GH-mcp]: https://github.com/github/github-mcp-server (README at `9a72d8d`, 2026-11-20: `search_code`, `get_copilot_space`, `list_copilot_spaces`, OAuth/PAT)
- [UB-mcp]: https://docs.getunblocked.com/unblocked-mcp/mcp-overview
- [UB-mem]: https://docs.getunblocked.com/unblocked-chat/shared-memories
