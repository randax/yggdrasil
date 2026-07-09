# Yggdrasil

An organization-deployed server that syncs git repositories from forges and serves a queryable knowledge graph of them, for AI agents (via MCP) and humans (via CLI).

## Language

**Index Server**:
The long-running yggdrasil service, deployed once per organization, that syncs repositories and answers graph queries.
_Avoid_: Indexer, daemon, backend

**Forge**:
A git hosting platform that yggdrasil syncs from: GitHub, GitLab, or Codeberg.
_Avoid_: Provider, platform, host

**Knowledge Graph**:
The queryable graph of entities and relationships that yggdrasil extracts from synced repositories.
_Avoid_: Index, database, map

**Symbol**:
A named code entity (function, method, class, type, constant) extracted from a repository's source, with its location.
_Avoid_: Node, definition, identifier

**Dependency**:
A manifest-declared requirement of one repository on a package, resolved where possible to another indexed repository.
_Avoid_: Link, reference

**Change Request**:
A proposed set of commits under review on a forge — GitHub/Codeberg "pull request", GitLab "merge request".
_Avoid_: PR, MR (forge-specific)

**Forge Artifact**:
Data synced from a forge's API rather than from git: change requests, issues, reviews.
_Avoid_: Metadata

**Provenance**:
How a graph edge was derived: precise (compiler-grade indexer), syntactic (heuristic parsing), extracted (deterministically derived from non-code sources: manifests, forge data, identity matches), inferred (heuristic guess, reversible), or asserted (claimed by an agent as a Memory — lowest trust). Every edge carries it, with a confidence tag.
_Avoid_: Source, origin (both overloaded in this domain)

**Doc Section**:
A heading-delimited unit of a markdown document, addressable as a graph node with edges to the code it mentions.

**Concept**:
An entity extracted semantically (by an LLM provider) from docs or media, as opposed to structurally extracted entities.

**Contributor**:
A person appearing in synced repositories' history or forge artifacts, identity-merged across forges.
_Avoid_: User, author, committer (git-mechanical terms)

**Member**:
An authenticated human or agent authorized to query an Index Server.
_Avoid_: User, client

**Admin**:
The person who operates an Index Server deployment: configures forges, chooses what gets indexed, wires up providers.
_Avoid_: Operator, owner

**Sync**:
The continuous process by which an Index Server converges with forge reality: git data via clone/fetch (poll + optional webhook), forge artifacts via API.
_Avoid_: Mirror, import, crawl

**Verb**:
A named graph operation in the public query contract (e.g. callers, impact, owners), exposed identically via CLI, MCP, and HTTP.
_Avoid_: Endpoint, command, tool (interface-specific words for the same thing)

**Skill**:
The hand-written navigation-method document shipped and versioned with yggdrasil, teaching agents how to traverse the Knowledge Graph well.
_Avoid_: Prompt, instructions

**Map**:
The generated per-repo orientation returned by the map verb: entry points, landmark symbols, doc index, owners. Always derived fresh from the graph.
_Avoid_: Overview, summary, report

**Shard**:
The immutable per-repo index artifact (graph, full-text, vectors) produced by indexing and swapped atomically on re-index.
_Avoid_: Index file, snapshot

**Memory**:
A unit of knowledge an agent asserts about a repository — a fact it learned, a decision, a gotcha — stored as a graph node and recallable in later sessions. Agent-authored and revisable, carrying asserted provenance; not extracted from source. Contrast the Knowledge Graph, which is derived and immutable.
_Avoid_: Note, fact, observation (these name parts of a Memory, not the whole)

**Semantic Memory**:
A Memory holding durable knowledge that compounds: a fact, decision, convention, or gotcha about a repository. Persists and is reinforced or superseded — never expired by time.

**Episodic Memory**:
A Memory holding task- or session-scoped working state — what an agent was doing and where it left off — meant to be resumed after compaction and then archived or expired.

**Anchor**:
An edge from a Memory to the code entity it concerns (a Symbol, file, or other graph node), making the Memory recallable from that entity and detectably stale when the entity changes.
_Avoid_: Reference, mention (a memory→memory connection is a link, not an Anchor)

**Promotion**:
Moving a Memory to wider reach — up the scope ladder (session → repo → org) or from a private overlay into the shared pool — so knowledge learned narrowly becomes available more broadly.
_Avoid_: Publish, share, merge
