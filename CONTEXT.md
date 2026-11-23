# Yggdrasil

An Organization-deployed server that syncs git repositories from forges and serves a queryable knowledge graph of them, for AI agents (via MCP) and humans (via CLI).

## Language

**Index Server**:
The long-running yggdrasil service, deployed once per Organization, that syncs repositories and answers graph queries.
_Avoid_: Indexer, daemon, backend

**Forge**:
A git hosting platform that yggdrasil syncs from: GitHub, GitLab, or Codeberg.
_Avoid_: Provider, platform, host

**Organization**:
The single entity an Index Server deployment serves. It owns every Forge connection, Member, Team and repository in the deployment, and it is the widest audience a Memory can be promoted to. Separate companies run separate deployments.
_Avoid_: Org (also used for a Forge Namespace), tenant, workspace, company

**Forge Namespace**:
A Forge's own grouping of repositories: a GitHub organization, a GitLab group or subgroup, or a Codeberg organization. One Organization connects any number of Forge Namespaces, across Forges.
_Avoid_: Org, group (forge-specific words)

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
How a graph edge was derived: precise (compiler-grade indexer), syntactic (heuristic parsing), extracted (deterministically derived from non-code sources: manifests, forge data, identity matches), or inferred (heuristic guess, reversible). Every edge carries it, with a confidence tag. Memories carry a separate control-plane trust level, asserted, which is lower than all of these.
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

**Personal Agent**:
An agent Member bound to one human Member, its principal. It belongs to exactly its principal's Teams, never sees more than its principal, and loses access when its principal does.
_Avoid_: Bot, assistant, delegate

**Service Agent**:
An agent Member that acts for no particular human, such as the Ask bot, a CI job or the impact reporter. An Admin assigns its Teams explicitly, and it does not hold the promote right by default.
_Avoid_: Bot, system user, robot account

**Team**:
A named group of Members within the Organization, and an audience for shared Memories: a Memory promoted to a Team is recalled only by that Team's Members. A Member may belong to several Teams, and a Memory's audience may include several Teams, each added by its own Promotion. Teams are flat: a Team has no parent, and its Members are exactly the Members it lists. A Team decides *who* sees a Memory; it never changes *where* the Memory is recalled, which is its scope.
_Avoid_: Group, squad, workspace

**Owning Team**:
A Team responsible for a piece of code. Ownership is derived from the code where possible: a CODEOWNERS entry naming a Forge team that is synced as a Team. Where no such entry exists, ownership is assigned by a stewardship rule. Ownership routes notices and impact reports, but it never decides who may see the code.
_Avoid_: Steward, maintainer team, code owner (a code owner may be an individual Contributor)

**Team Maintainer**:
A human Member, named by an Admin, who manages one Team. A Team Maintainer adds and removes its human Members and grants or revokes that Team's promote right. The role has no power over other Teams, the Organization-wide right, Service Agents or Admin grants.
_Avoid_: Team admin, team owner, lead

**Admin**:
The person who operates an Index Server deployment: configures forges, chooses what gets indexed, wires up providers.
_Avoid_: Operator, owner

**Sync**:
The continuous process by which an Index Server converges with forge reality: git data via clone/fetch (poll + optional webhook), forge artifacts via API.
_Avoid_: Mirror, import, crawl

**Verb**:
A named operation in the public contract — a graph query (e.g. callers, impact, owners) or a Memory operation (remember, recall, promote, retract) — exposed identically via CLI, MCP, and HTTP, except that step-up acts (a human Member's promote, accept and shared affirm, and an Admin's chain-wide retract) are refused over MCP (ADR 0007).
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
A unit of knowledge a Member (human or agent) asserts about a repository — a fact it learned, a decision, a gotcha — stored as a control-plane record anchored to graph nodes and recallable in later sessions. Member-authored and revisable, carrying asserted trust; not extracted from source. Contrast the Knowledge Graph, which is derived and immutable.
_Avoid_: Note, fact, observation (these name parts of a Memory, not the whole)

**Semantic Memory**:
A Memory holding durable knowledge that compounds: a fact, decision, convention, or gotcha about a repository. Persists and is reinforced or superseded — never expired by time.

**Episodic Memory**:
A Memory holding task- or session-scoped working state — what an agent was doing and where it left off — meant to be resumed after compaction and then archived or expired.

**Anchor**:
A link from a Memory to the graph node it concerns (a Symbol, File, Repo, or — once Doc Sections land — Doc Section), carrying that node's content fingerprint (none for Repo anchors), making the Memory recallable from that node and detectably stale when it changes.
_Avoid_: Reference, mention (a memory→memory connection is a link, not an Anchor)

**Scope**:
Where a Memory is recalled, set by its Anchors: session (Episodic only), repo (every Anchor in one repository) or cross-repo (Anchors span repositories). Scope never decides who sees a Memory; its audience does.
_Avoid_: Visibility, reach, org scope

**Audience**:
Who may recall a Memory: only its author (the private overlay), one or more Teams, or the whole Organization. Audience is independent of Scope.
_Avoid_: Visibility (reserved for which repositories a Member may see), sharing level

**Proposal**:
A request to promote a Memory into an audience whose promote right its author lacks. It waits in that audience's review queue, where every holder of the right can see it, until one of them accepts or rejects it.
_Avoid_: Request, submission, pull request

**Promotion**:
Widening a Memory's reach. Its audience can grow from its author's private overlay to one or more Teams, then to the whole Organization. Its scope can also widen from repo to cross-repo. Promotion makes knowledge learned narrowly available to more Members. The promote right is held per audience: for each Team, and for the Organization as a whole. A Member without the right for the target audience can only propose, and a holder of that right decides. Episodic (session-scoped) Memory is never promoted; a lesson worth keeping from a session is written as a new Semantic Memory.
_Avoid_: Publish, share, merge
