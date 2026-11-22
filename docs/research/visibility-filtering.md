# Query-time visibility filtering in code search and graph systems

Status: research note · Date: 2026-11-23 · Ticket: [#124](https://github.com/randax/yggdrasil/issues/124) (map [#121](https://github.com/randax/yggdrasil/issues/121)) · Yggdrasil baseline: commit `c4bec0c` on `main`

> **Question.** How do multi-user code search and graph systems enforce per-repo
> or per-group visibility at query time? What does that imply for Yggdrasil's
> per-repo, per-revision Shards, cross-repo traversal, search merges, Memory
> recall and the Ask agent, now that Teams get a visibility boundary and
> reopen [ADR 0001](../adr/0001-org-visible-access-model.md)?
>
> **Method.** Primary sources only: vendor documentation, source code pinned to
> a commit, and the original papers. Source code was read at these heads on
> 2026-11-23: `sourcegraph/sourcegraph-public-snapshot@c864f15` (archived
> 2024-09, the last public Sourcegraph source), `sourcegraph/zoekt@b55ce72`,
> `onyx-dot-app/onyx@2582a29` and `quickwit-oss/tantivy@0aa896f`. Claims about
> Yggdrasil cite repository paths at `c4bec0c`. Anything not confirmed in a
> primary source is marked **(unverified)**; my own reasoning is marked
> **(analysis)**. This note lays out options and costs. It does not decide.

## 1. Summary

1. **Every surveyed system pre-filters at the coarsest unit it can, and some
   also post-filter.** Sourcegraph narrows Zoekt and the code-intel SQL to
   the repos a user may read before searching, then post-filters only
   sub-repo (path) permissions ([§3.1](#31-sourcegraph-and-zoekt)). GitHub
   code search adds permission clauses to the rewritten query and then
   filters "to double-check permissions" after merging the shards
   ([§3.2](#32-github-code-search)). Onyx pre-filters on a per-chunk ACL
   field and post-filters only one source (Salesforce)
   ([§3.4](#34-onyx)). The pure post-filter is the minority pattern,
   reserved for permissions the index cannot express.
2. **A post-filter over a shared index leaks through ranking and statistics.**
   Büttcher and Clarke showed in 2005 that post-filtered BM25 lets a user
   recover term counts in unreadable files from scores, or even from result
   order alone, and rebuild file contents with phrase queries. Their fix was
   to apply the restriction inside query processing, so the statistics come
   only from visible files ([§3.8](#38-büttcher-and-clarke-fast-05)).
   Elasticsearch document-level security still has this property today:
   "scores are computed without taking the role query into account", and
   aggregations can count inaccessible documents
   ([§3.5](#35-elasticsearch-document-level-security)). tantivy computes BM25
   statistics over the whole index too
   ([§3.6](#36-lucene-and-tantivy)).
3. **Per-shard visibility avoids that whole class of leak.** In a system where
   each shard is one repo and the unit of visibility is the repo, a
   pre-filter on the shard set means hidden repos contribute nothing to
   scores, counts or timing. Zoekt prunes whole shards by repo set before
   searching them ([§3.1](#31-sourcegraph-and-zoekt)). Yggdrasil already has
   this shape: one tantivy index per repo, BM25 inside each repo, and
   max-normalisation before the merge ([§5.1](#51-what-yggdrasil-already-does)).
4. **Per-document ACLs live in the index; per-repo ACLs live beside it.**
   Onyx and Glean attach an ACL to every document or chunk and keep it
   current by syncing permissions into the index
   ([§3.3](#33-glean), [§3.4](#34-onyx)). Sourcegraph keeps repo permissions
   in Postgres (`user_repo_permissions`) and passes repo IDs to the search
   engine ([§3.1](#31-sourcegraph-and-zoekt)). With immutable Shards, only the
   second shape changes visibility without re-indexing.
5. **Existence is hidden by making "forbidden" look like "absent".** GitHub
   returns 404 rather than 403 "to avoid confirming the existence of private
   repositories". Sourcegraph's repo lookup returns `RepoNotFoundErr` for a
   repo the user cannot see, and its documentation says other users will not
   be shown "the existence of" a private repo. Sourcegraph's cross-repo
   reference *counts* apply the same authz condition as the rows
   ([§4.3](#43-counts-and-existence-leaks)).
6. **Cross-reference leakage is handled by dropping, not redacting.** In
   Sourcegraph's precise code navigation, cross-repo reference lookups join
   the authz condition into both the count and the page. A reference into, or
   from, a repo you cannot see simply does not exist for you. Neo4j instead
   offers a split: `TRAVERSE` "enables the specified entities to be found",
   while `READ` covers their properties. This permits "you may know a node
   exists but not read it" ([§4.5](#45-cross-reference-and-edge-leakage)).
7. **Caches are keyed by permission state.** Zanzibar avoids "reusing results
   evaluated from a different snapshot by encoding snapshot timestamps in
   cache keys". GitHub caches "some access control data" in Redis.
   Elasticsearch caches per-role-query `BitSet`s. Sourcegraph materialises
   per-user permissions and accepts sync lag ("in the worst case the lag time
   is as long as the time it takes to completely sync all user or repository
   permissions") ([§4.4](#44-caching-and-freshness)).
8. **For Yggdrasil the hard parts are traversal and Ask, not search.**
   Restricting the search fan-out set to the caller's visible repos is a
   small change to an existing pre-filter. Cross-repo traversal (`callers`,
   `impact`, `deps`, `path`), the planned PR impact report, Memory recall
   with multi-repo Anchors, and an Ask agent answering into a shared Slack
   channel each need an explicit rule for what a hidden endpoint looks like.
   [§5](#5-applying-this-to-yggdrasil) lays out the options with their costs.

## 2. Terms used here

- **Pre-filter:** the visibility predicate is part of query evaluation. Either
  the candidate set (shards, repo IDs, posting lists) is narrowed before
  scoring, or a non-scoring filter clause is ANDed into the query. Hidden
  items never reach ranking, counting or collection.
- **Post-filter:** the query runs over everything, and hidden results are
  removed from the output afterwards. Pagination has to over-fetch, and
  anything computed before the filter (scores, statistics, totals, facets,
  timing) can reflect hidden data.
- **Per-shard (per-repo) ACL:** visibility is a property of a whole index
  unit, stored outside it, and applied by choosing which units to open.
- **Per-document ACL:** each document or chunk carries its allowed principals
  as an indexed field, matched against the caller's principals at query time.

## 3. How each system does it

### 3.1 Sourcegraph and Zoekt

- **Granularity.** Sourcegraph "can be configured to enforce the same access
  to repositories and underlying source files as your code host". Path-level
  ("file-level") permissions are listed only for Perforce. Enforcement is
  "across the product for all the use cases that need to read data from a
  repository"
  ([docs: permissions](https://sourcegraph.com/docs/admin/permissions)).
- **Where permissions live.** Permissions are synced from the code host into
  Sourcegraph's database, either user-centric ("a list of repositories that a
  specific user has read access to") or repo-centric ("a list of users that
  can access a specific repository"). They are refreshed on a schedule, with
  webhooks recommended because they make "lag time much smaller"
  ([docs: permission syncing](https://sourcegraph.com/docs/admin/permissions/syncing)).
- **The repo-level predicate is SQL.** `authzQuery` in
  [`internal/database/repos_perm.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/database/repos_perm.go)
  ORs three conditions:
  - "Unrestricted repos are visible to all users" (`user_repo_permissions` row
    with `user_id IS NULL`);
  - `NOT repo.private`, or a repo from an external service marked
    `unrestricted`;
  - an `EXISTS` on `user_repo_permissions` for the current user.

  Internal actors and, by default, site admins bypass it:
  `params.BypassAuthz = true`, unless `authz.enforceForSiteAdmins` is set
  (same file;
  [docs](https://sourcegraph.com/docs/admin/permissions): "By default,
  administrators bypass all of the permissions checks"). The repo store adds
  it to every listing ("🚨 SECURITY: Enforce repository permissions"). A
  `Get` that the authz condition filters out returns `RepoNotFoundErr`,
  the same as a missing repo
  ([`internal/database/repos.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/database/repos.go)).
- **Search is pre-filtered at the repo level.** For global searches,
  Sourcegraph builds a Zoekt query that ANDs the user's query with an OR of
  `RcOnlyPublic` (Zoekt's per-repo "public" config bit) and an explicit
  `BranchesRepos` set of the user's private repo IDs. The code calls this
  ensuring "appropriate repo privacy scopes"
  ([`internal/search/zoekt/zoekt_global.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/search/zoekt/zoekt_global.go)).
- **Zoekt itself has no users.** Its shards are files that "can contain one
  repository or, after shard merging, multiple repositories in a compound
  shard". Queries are partially evaluated per shard, so a shard for the wrong
  repo becomes FALSE and is skipped
  ([`doc/design.md`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/doc/design.md)).
  `selectRepoSet` drops shards that contain none of the repos in a
  `RepoSet`, `RepoIDs` or `BranchesRepos` clause before any search work
  happens. A compound shard containing some hidden repos still has to be
  searched with the filter clause inside it
  ([`search/shards.go`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/search/shards.go),
  `doSelectRepoSet`).
- **Sub-repo (path) permissions are a post-filter.** `subRepoPermsFilterJob`
  wraps the search job and filters the *streamed* results with
  `applySubRepoFiltering`. On a permission-check error, matches are omitted
  ("If an error occurs while checking sub-repo perms, we omit it from the
  results")
  ([`internal/search/job/jobutil/sub_repo_perms_job.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/search/job/jobutil/sub_repo_perms_job.go)).
  Code navigation applies `authz.FilterActorPath` to each returned location
  ([`internal/codeintel/codenav/service.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/service.go)).
- **Cross-repo code intelligence joins authz into the query.**
  `GetVisibleUploadsMatchingMonikers` finds indexes in *other* repos that
  reference a moniker. It passes the same `authzConds` into both the
  `COUNT(distinct r.dump_id)` query and the paged row query.
  `GetCompletedUploadsWithDefinitionsForMonikers`, which resolves a moniker
  to its defining repo, also carries the authz condition
  ([`internal/codeintel/uploads/internal/store/uploads.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/uploads/internal/store/uploads.go)).
- **Initial sync is fail-closed.** During the first sync, "users will
  gradually see more and more search results from repositories they have
  access to"
  ([docs: permission syncing](https://sourcegraph.com/docs/admin/permissions/syncing)).

### 3.2 GitHub code search

From the engineering post on Blackbird
([Clem, 2023-02-06](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/)):

- The index is "sharded by Git blob SHA", not by repo. Every shard therefore
  holds content from many repos, public and private.
- Query rewriting is the pre-filter: it involves "tagging on extra clauses
  for permissions and scopes", and the worked example ANDs the user's query
  with `Or(RepoIDs(...), PublicRepo())`. The rewrite "ensures that I'll get
  results from public repositories or any private repositories that I have
  access to".
- After the shards return, the service aggregates the results, re-sorts them
  by score, filters them "(to double-check permissions)" and returns the
  top 100. This is a defence-in-depth post-filter on top of the pre-filter.
- "We use Redis to manage quotas and cache some access control data."
- The post does not say whether ranking statistics are computed per shard
  over all content or only over the permitted set **(unverified)**. Given
  blob-SHA sharding, per-user statistics are unlikely **(analysis)**.
- On the REST API, "GitHub uses a `404 Not Found` response instead of a
  `403 Forbidden` response … to avoid confirming the existence of private
  repositories"
  ([docs: troubleshooting the REST API](https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api)).

### 3.3 Glean

- **Per-document ACLs pushed into the index.** Documents carry a `permissions`
  object with `allowedUsers`, `allowedGroups` and `allowAnonymousAccess`
  (anyone signed into Glean). `allowAllDatasourceUsersAccess` covers all
  users indexed for that datasource. "Users need to be indexed before being
  referenced". Permissions "are processed asynchronously", so "there might
  be a small delay before documents are visible"
  ([developer docs: permissions](https://developers.glean.com/api-info/indexing/documents/permissions)).
- **Enforced at query time, on every surface.** "Glean evaluates the
  signed-in user's identity against those mirrored permissions before
  returning any results". The same enforcement covers search, AI answers
  ("responses only from content the querying user is authorized to
  access"), citations, and agents, which "execute with the identity and
  permissions of the signed-in user who triggered the run"
  ([docs: security principles](https://docs.glean.com/security/security-principles)).
- Whether Glean filters before or after ranking, and what statistics its
  ranking uses, is not documented **(unverified)**.

### 3.4 Onyx

- **Per-chunk ACL, pre-filtered in the search engine.** `ExternalAccess`
  holds `external_user_emails`, `external_user_group_ids` and `is_public`
  ([`backend/onyx/access/models.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/access/models.py)).
  At query time `build_access_filters_for_user` turns the user's ACL entries
  into a filter
  ([`context/search/preprocessing/access_filters.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/context/search/preprocessing/access_filters.py)).
  The filter is placed in OpenSearch `bool.filter` clauses, which do not
  score, including inside hybrid (lexical + vector) search
  ([`document_index/opensearch/search.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/document_index/opensearch/search.py)).
- **ACLs kept fresh by periodic sync into the index.** Each source has a
  `doc_sync_frequency` and a `group_sync_frequency`. GitHub has both
  ([`ee/onyx/external_permissions/sync_params.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/ee/onyx/external_permissions/sync_params.py)).
- **A post-filter where the index cannot express the rule.** "Post-query
  censoring" runs for Salesforce only. Chunks are checked against the source
  after retrieval, with a fail-closed rule: when the censor errors, Onyx is
  "throwing out all chunks for this source and continuing"
  ([`ee/onyx/external_permissions/post_query_censoring.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/ee/onyx/external_permissions/post_query_censoring.py)).

### 3.5 Elasticsearch document-level security

- A role's DLS query "restricts which documents a user can read". Multiple
  roles' DLS queries are ORed, and a role that grants the index without DLS
  disables DLS for that user. Templated role queries can read `_user`
  metadata, such as `{{_user.metadata.group_id}}`. Elastic suggests
  splitting documents into separate indices when both documents and fields
  need restricting
  ([docs: document and field level security](https://www.elastic.co/docs/deploy-manage/users-roles/cluster-or-deployment-auth/controlling-access-at-document-field-level)).
- **Documented leaks** ([docs: security limitations](https://www.elastic.co/docs/deploy-manage/security/limitations)):
  - "Document level security doesn't affect global index statistics that
    relevancy scoring uses. This means that scores are computed without
    taking the role query into account."
  - It is "still possible to write search requests that return aggregate
    information about the entire index", including to "count how many
    inaccessible documents contain a given term".
  - Suggesters are ignored, and the terms enum API returns nothing under
    DLS.
- **Caching.** DLS uses cached Lucene `BitSet`s, controlled by
  `xpack.security.dls.bitset.cache.ttl` (default `2h`) and `.size`
  (default 10% of heap)
  ([docs: security settings](https://www.elastic.co/docs/reference/elasticsearch/configuration-reference/security-settings)).
  The request cache is disabled when the role query is templated with a
  stored script
  ([DLS docs](https://www.elastic.co/docs/deploy-manage/users-roles/cluster-or-deployment-auth/controlling-access-at-document-field-level)).

### 3.6 Lucene and tantivy

- **Lucene.** `BooleanClause.Occur.FILTER` is "Like MUST except that these
  clauses do not participate in scoring"
  ([javadoc](https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/search/BooleanClause.Occur.html)).
  That is the standard place for an ACL term clause. It removes the clause's
  own score contribution, but the other clauses' IDF still comes from the
  whole index. That is the Elasticsearch behaviour above, since
  Elasticsearch is built on Lucene **(analysis)**.
- **tantivy.** It has no ACL feature. The building blocks are:
  - a `BooleanQuery` with a `TermQuery` or `TermSetQuery` clause on an ACL
    field, optionally wrapped in `ConstScoreQuery` so it adds no score
    ([`src/query/mod.rs`](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/src/query/mod.rs));
  - `FilterCollector`, which "filters docs using a fast field value and a
    predicate" during collection
    ([docs.rs, tantivy 0.27.0](https://docs.rs/tantivy/latest/tantivy/collector/struct.FilterCollector.html)).

  BM25 statistics come from `Bm25StatisticsProvider`, which the `Searcher`
  implements by summing `max_doc` and token counts over all segment readers.
  So a filter does not change IDF. The trait is public, and its own docs say
  "you can also create your own to adjust the statistics"
  ([`src/query/bm25.rs`](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/src/query/bm25.rs)).
- **Postgres full-text search**, which Yggdrasil's `recall` uses:
  "the ranking functions do not use any global information"
  ([docs: textsearch controls](https://www.postgresql.org/docs/current/textsearch-controls.html)).
  A `WHERE` visibility predicate therefore cannot leak through `ts_rank`
  scores **(analysis)**.

### 3.7 Graph databases and authorisation services

- **Neo4j fine-grained access control** separates finding from reading:
  - `TRAVERSE` "enables the specified entities to be found";
  - `READ` "enables the specified properties of the found entities to be
    read";
  - `MATCH` combines both
    ([docs: read privileges](https://neo4j.com/docs/operations-manual/current/authentication-authorization/privileges-reads/)).

  Neo4j's tutorial grants a role traversal of every node with only partial
  `READ`. Hidden properties then come back as `NULL`, and "users cannot tell
  the difference between data that does not exist and data that is hidden"
  ([tutorial](https://neo4j.com/docs/operations-manual/current/tutorial/access-control/)).

  Neo4j's documented limitations
  ([docs: limitations](https://neo4j.com/docs/operations-manual/current/authentication-authorization/limitations/)):
  - Counts cannot use the precomputed count store under restrictions,
    because "it must make sure that only traversable nodes" are counted.
  - Full-text indexes over-exclude, because "the security model cannot
    introspect the Lucene query".
  - Labels of an accessible node are visible even when the role may not
    traverse them.
  - "A DENY rule fails open when its criteria are not met".
- **Zanzibar** (Pang et al.,
  [USENIX ATC '19](https://www.usenix.org/system/files/atc19-pang.pdf)).
  §2.2 defines the "new enemy" problem, in two forms: "Neglecting ACL update
  order" and "Misapplying old ACL to new content". The fix is external
  consistency plus snapshot reads with bounded staleness, carried by the
  *zookie* ("an opaque byte sequence encoding a globally meaningful
  timestamp"). §3.2.5 on caching says: "We avoid reusing results evaluated
  from a different snapshot by encoding snapshot timestamps in cache keys",
  with timestamps quantised to "one or ten seconds" so that cache entries
  are shared. §3.2.4 describes Leopard, a denormalised set index for deeply
  nested groups.
- **SpiceDB** documents both list-filtering patterns explicitly
  ([docs: protecting a list endpoint](https://authzed.com/docs/spicedb/modeling/protecting-a-list-endpoint)):
  - Pre-filter with `LookupResources` "if the number of resources that a
    user has access to is small (e.g. less than 10,000 resources)".
  - Post-filter with `CheckBulkPermissions`: "fetch a page of resources from
    your database, and then call CheckBulkPermissions", iterating "until we
    have a full page of accessible results". Cursor pagination is preferred
    over limit-offset.
  - A materialised local permission table, which "provides the greatest
    scalability of the three options".

### 3.8 Büttcher and Clarke (FAST '05)

"A Security Model for Full-Text File System Search in Multi-User
Environments" ([USENIX FAST '05](https://www.usenix.org/conference/fast-05/security-model-full-text-file-system-search-multi-user-environments);
[full text](https://www.usenix.org/legacy/events/fast05/tech/full_papers/buettcher/buettcher_html/index.html)):

- §5 attacks a post-filtered BM25 index, which ranks with system-wide
  statistics and then drops unreadable files:
  - From returned scores, a user can work out "the total number of files in
    the file system containing the term" (§5.1).
  - Without scores, the user gets "an approximation of |F_T*|, or even its
    exact value, from the order in which matching files are returned"
    (§5.2).
  - With phrase queries "it is possible to use the search system to obtain
    the whole content of a file" (§5.3).
- §6 ("Query Integration") intersects every posting list with the user's
  searchable files before query processing. Then "all statistics necessary
  to perform BM25 relevance ranking can be generated from the user-specific
  posting lists", and results cannot "depend on the content of files that
  are not searchable by" the user.
- §7 and §9 report the cost: the integrated design is slower when most
  files are searchable, by up to about 75% before the §8 optimisations.

## 4. Approaches compared

### 4.1 Who uses which

| System | Unit of visibility | Pre-filter | Post-filter | ACL stored | Scoring stats |
|---|---|---|---|---|---|
| Sourcegraph + Zoekt | repo; path for Perforce | repo IDs + public bit into Zoekt; SQL `authzConds` for code intel | sub-repo path checks on streamed results and code-nav locations | Postgres `user_repo_permissions`, synced | per Zoekt shard **(unverified)** |
| GitHub code search | repo | permission clauses in query rewrite | "double-check permissions" after merge | Redis cache of "some access control data" | blob-sharded, so mixed-visibility **(analysis)** |
| Glean | document | yes, "before returning any results" | not documented | per-document `permissions` in index | not documented |
| Onyx | chunk | OpenSearch `bool.filter` on ACL | Salesforce censoring only | per-chunk ACL, periodic sync | index-wide **(analysis)** |
| Elasticsearch DLS | document (role query) | role query as filter | — | role query + document fields | index-wide, documented leak |
| Neo4j | label / relationship type / property | privilege checks during traversal | — | role privileges | n/a |
| Büttcher & Clarke §6 | file | posting-list intersection | — (§5 is the insecure post-filter) | file permissions | visible-only |
| **Yggdrasil today** | repo, all-or-nothing admission | fan-out set = included repos | — | `repos.discovery_state` | per-repo tantivy index |

### 4.2 Pre-filter versus post-filter

- **Pre-filter costs**
  - The caller's visible set must be known and passed in at query time:
    Sourcegraph's private repo ID list, Onyx's ACL list.
  - Large sets make queries heavy. SpiceDB's 10,000-resource rule of thumb
    is the documented threshold for that pattern.
  - Per-document pre-filters need the ACL in the index.
- **Pre-filter benefits.** Pagination is exact, and with per-unit statistics
  nothing about hidden data reaches scores or counts.
- **Post-filter costs**
  - Pages must be over-fetched and looped (SpiceDB).
  - Totals are wrong or leaky.
  - Under index-wide statistics, scores and order leak (Büttcher & Clarke
    §5; Elasticsearch limitations).
- **Post-filter benefits.** It works for rules the index cannot express, such
  as path rules or live source checks. That is exactly where Sourcegraph and
  Onyx use it.
- **Both.** GitHub pre-filters and then post-checks before returning. The
  post-check guards against a stale or buggy pre-filter; it is not the
  primary control.

### 4.3 Counts and existence leaks

The ways hidden data has been shown to leak:

- relevance scores and result order (Büttcher & Clarke);
- aggregations, term counts and suggesters (Elasticsearch);
- totals computed before filtering (avoided by Sourcegraph's authz'd
  `COUNT`);
- forbidden-vs-missing error codes (avoided by GitHub's 404 and
  Sourcegraph's `RepoNotFoundErr`);
- labels on visible nodes (Neo4j);
- counts served from a precomputed store (Neo4j disables it under
  restriction).

Zoekt's `Stats` carry `ShardsScanned`, `ShardFilesConsidered` and similar
fields
([`api.go`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/api.go)).
Whether Sourcegraph exposes these per-query stats to end users is
**(unverified)**. If a system surfaces such internals, they need the same
treatment **(analysis)**.

### 4.4 Caching and freshness

- **Materialised permission tables**
  - Sourcegraph `user_repo_permissions`: worst-case lag is a full sync.
  - GitHub: Redis.
  - SpiceDB Materialize.
  - Onyx: per-source sync frequencies.

  These trade freshness for query cost. Revocation latency is bounded by
  the sync interval unless webhooks drive it.
- **Snapshot-keyed caches.** Zanzibar puts the evaluation snapshot timestamp
  in every cache key, and quantises it so entries are shared. A cache can
  then never serve a result computed under a different ACL state.
- **Filter caches.** Elasticsearch caches `BitSet`s per role query and
  disables the request cache for script-templated role queries.
- **Ordering ("new enemy").** A revoke followed by new content must not
  expose the new content to the revoked user (Zanzibar §2.2). Any result
  cache keyed only by query text is wrong under visibility **(analysis)**.

### 4.5 Cross-reference and edge leakage

A visible caller pointing into a hidden repo, or a hidden caller pointing
into a visible one, exposes a symbol name, a location, a count or a repo
slug. The surveyed systems show three treatments:

1. **Drop.** The edge does not exist for the caller, and counts exclude it.
   Sourcegraph joins authz into both the count and the rows of its
   cross-repo reference and definition lookups (§3.1).
2. **Find-but-not-read.** Neo4j's `TRAVERSE` without `READ`: the endpoint's
   existence is visible, its properties are `NULL`, and hidden reads as
   absent.
3. **Redacted aggregate**, such as "3 callers in repos you cannot see". This
   was found in none of the surveyed systems' documentation
   **(unverified as a practice)**. It is a deliberate existence and count
   disclosure.

The caller's own Shard is a separate matter. A visible repo's code that
*calls into* a hidden package already names that package and symbol in its
own source. Showing that reference leaks nothing the caller could not read.
What leaks is *resolving* it to the hidden repo's definition, location, body
or other callers **(analysis)**.

### 4.6 Fail-closed defaults and admin bypass

- **Fail-closed:**
  - Sourcegraph omits matches when a sub-repo check errors.
  - Onyx drops a whole source's chunks when its censor fails.
  - Sourcegraph shows nothing private until the first sync lands.
- **The counter-example:** Neo4j `DENY` rules "fail open" when their
  criteria do not match.
- **Admin bypass:** Sourcegraph lets site admins bypass authz by default.
  It is configurable.

## 5. Applying this to Yggdrasil

### 5.1 What Yggdrasil already does

- **Admission is the only visibility rule, and it is per repo.** ADR 0001:
  every Member sees everything indexed, and "sensitive repos are kept
  private by not indexing them". `ControlPlane::verb_target` resolves a
  qualifier only `WHERE … r.discovery_state = 'included'`
  ([`crates/yg-control/src/lib.rs`](../../crates/yg-control/src/lib.rs)).
- **Search is already a per-shard pre-filter.** With no `repos` filter, the
  fan-out set is "every indexed repo whose current Shard is at the caller's
  revision suffix", resolved per query (same file, `indexed_search_targets`
  doc). Each repo has its own tantivy segment in its Shard (RFC 0001 §6).
  Scores are BM25 *inside* each repo, then divided by that repo's maximum
  before a deterministic merge (RFC 0001 §7;
  [`crates/yg-verbs/src/search/fanout.rs`](../../crates/yg-verbs/src/search/fanout.rs)
  `normalize_repo_scores`). No hidden repo can influence a visible repo's
  IDF, its normalisation, or the merge order. This is the Büttcher & Clarke
  §6 property, obtained structurally **(analysis)**.
- **Search cursors pin the target set and re-check it on resume.** A search
  cursor carries the `(repo_id, qualifier, revision)` fan-out set
  ([`crates/yg-verbs/src/search/types.rs`](../../crates/yg-verbs/src/search/types.rs)).
  On resume, the API resolver re-runs `verb_target` for each pinned repo and
  refuses one that is no longer visible
  ([`crates/yg-api/src/verbs.rs`](../../crates/yg-api/src/verbs.rs),
  `resolve_fts` with `ResumedFromCursor`). The graph-Shard `resolve` used by
  pinned `neighbors` and `history` cursors also calls `verb_target` first
  and checks the pinned repo id (same file). Cursors are HMAC-signed
  ([`crates/yg-verbs/src/cursor.rs`](../../crates/yg-verbs/src/cursor.rs)).
  The cursor payload carries no Member identity; I found none in
  `cursor.rs` **(analysis)**.
- **Errors distinguish two cases.** "no indexed repository matches" is
  returned for an unknown *or* non-included repo. "is registered but not yet
  indexed" is returned for an included repo with no Shard yet
  ([`fanout.rs`](../../crates/yg-verbs/src/search/fanout.rs),
  `map_named_target_error`). That is harmless under ADR 0001. Under Teams it
  would be an existence oracle unless visibility is checked first
  **(analysis)**.
- **Memory rules already encode several of the patterns above** (RFC 0001
  §5.1):
  - recall only "while every repo it anchors to is indexed";
  - "An inaccessible target is refused with the same error as an unknown
    one";
  - hidden supersession links "are omitted without placeholder or count";
  - proposal reviewers see the expected head only if every anchored repo is
    admitted;
  - recall ranks on Postgres FTS, never merged into tantivy BM25.
- **Cross-repo edges are planned, not built.** `xrepo_edges(from_repo,
  to_repo, via_package_id, kind)` in Postgres "bound all cross-repo query
  fan-out" (RFC 0001 §2, §4). `callers`, `impact`, `deps`, `dependents` and
  `path` are planned for M1 and M2 (RFC 0001 §7).

So Teams change one thing at the root: today's single global visible set
("included repos") becomes a **per-Member visible set**. Every place that
today asks "is this repo admitted?" must instead ask "is it visible to this
caller?" **(analysis)**.

### 5.2 Unit of visibility and where the ACL lives

| Option | What it is | Costs | Benefits |
|---|---|---|---|
| **A. Repo-level, control plane** (Sourcegraph pattern) | A Team-to-repo grant table in Postgres. The visible set is computed per Member and applied by choosing Shards. | Per-repo granularity only. A sensitive directory in a shared repo is unsupported. The visible-set computation must be cached and invalidated on Team and grant changes. | No re-index on a visibility change, so it fits immutable Shards (ADR 0005). Per-repo scoring stays leak-free. It reuses the existing fan-out, `verb_target` and Memory admission seams. |
| **B. Per-document ACL in the Shard** (Onyx and Glean pattern) | ACL terms on each tantivy doc and graph node. | Every visibility change rewrites Shards, against immutability and the "pointer swap only on new code" model. Within-repo IDF would include hidden docs unless a custom `Bm25StatisticsProvider` is written (§3.6). Graph traversal needs per-node checks. | Path-level rules become possible. |
| **C. A plus a path post-filter** (Sourcegraph sub-repo pattern) | Repo-level pre-filter, plus a path predicate applied to results and traversal steps. | Over-fetching and pagination loops (SpiceDB). Within-repo scores and order can leak hidden-path content (Büttcher & Clarke §5). Every Verb needs the hook. | Handles CODEOWNERS-style "this directory is restricted" without re-indexing. |
| **D. Split restricted paths into their own Shard** | A monorepo sub-tree becomes its own index unit, a generalisation of RFC 0001 Q4. | Changes Shard identity and the cross-repo join model. Edges between the halves become "cross-repo". | Restores the per-shard property for sub-repo rules. Elastic gives similar advice: "split documents into separate indices". |

The map's Notes put **membership synced from Forge teams and groups** in
scope. That brings in Sourcegraph's sync-lag problem regardless of option:
revocation is only as fast as the sync loop or webhooks (§4.4).

### 5.3 Per-revision Shards

- Visibility is a property of the repo (or Team grant), not the revision. If
  the ACL lives outside the Shard (options A, C), a repo's visibility can
  change without any Shard work, and every revision follows it at once
  **(analysis)**.
- Pinned revisions in `neighbors`, `history` and search cursors let a walk
  continue on an old revision after a pointer swap (`cursor.rs` docs). The
  existing resume re-check (§5.1) is the right shape, and every pinned path
  already runs it through `verb_target`. Under Teams, that one function
  would have to check the *Member's* visibility, not just admission.
- **Options for binding cursors to identity:**
  - Bind the cursor to the Member: add the Member id to the signed payload.
    This stops a cursor minted by one Member from being replayed by another.
  - Rely only on re-checking: cheaper, but the pinned target list inside a
    cursor is itself a disclosure if a cursor ever leaks. It is
    authenticated, not encrypted **(analysis)**.

### 5.4 Search fan-out and merges

- **Pre-filtering the fan-out set** to the caller's visible repos keeps every
  current property: per-repo BM25, per-repo max-normalisation and a
  deterministic merge. The open cost is size. With 5000 repos, the visible
  set can be thousands of IDs. That is the SpiceDB and Sourcegraph regime,
  so the set should be resolved in Postgres as a join, not shipped around
  as a list **(analysis)**.
- **Global tiers reintroduce the leak.** RFC 0001 Q5 contemplates "a small
  global 'top symbols' index in the control plane" and a "coarse global
  tier" for vectors. Any shared index across repos with different
  visibility has the Elasticsearch, Lucene and tantivy property:
  index-wide statistics and post-filter leakage. The choices are:
  - per-visibility-class global indices (one per Team, or per distinct
    grant set);
  - statistics-free ranking in the global tier;
  - visible-only statistics via a custom `Bm25StatisticsProvider` (§3.6).

  This also applies to compound-shard style packing of small repos (Zoekt's
  compound shards) if it is ever considered **(analysis)**.
- **Semantic search at M3.** Per-repo ANN segments fit the pre-filter. A
  global ANN index with a post-filter has the over-fetch problem, and ANN
  libraries differ in filtered-search support. That was not researched
  here **(unverified)**.
- **Counts.** `search` returns hits plus a cursor, with no total today. A
  total, if ever added, must be computed over the visible set only
  (Sourcegraph `COUNT` with `authzConds`) **(analysis)**.
- **Errors.** Visibility must be checked before the
  `UnknownRepo`/`NotIndexed` distinction, so a hidden repo yields exactly
  the unknown-repo response. That is the GitHub and Sourcegraph pattern.

### 5.5 Cross-repo traversal

This affects `callers`, `callees`, `impact`, `deps`, `dependents`, `path`,
`neighbors` across `xrepo_edges`, `owners` (Teams own files), and the PR
impact report. The options, from §4.5:

| Option | Behaviour at a hidden endpoint | Costs |
|---|---|---|
| **T1. Drop** (Sourcegraph) | Edges into or out of a hidden repo vanish. Counts and `impact` closures exclude them. `path` never routes through a hidden repo. | `impact` and the PR impact report understate blast radius for anyone who cannot see every dependent. A Team changing a shared library cannot learn that a hidden repo breaks. Silent incompleteness also conflicts with the Skill's "graph thin ⇒ fall back gracefully" etiquette, because the agent cannot tell "none" from "hidden". |
| **T2. Find-but-not-read** (Neo4j `TRAVERSE`) | The hidden repo appears as an opaque node (repo id or slug, maybe owning Team). Symbols, locations, bodies and its further edges are withheld. | Repo existence and the dependency relation are disclosed. Owners and Teams can be notified without disclosing code. A rule is needed for whether traversal may continue *through* the opaque node (A → hidden B → visible C): continuing discloses that B links A and C. |
| **T3. Redacted aggregate** | "n callers in repos you cannot see", with no identities. | Discloses counts, which is the leak Neo4j and Elasticsearch document. Repeated queries can narrow it down (Büttcher & Clarke §5.2 style) **(analysis)**. Not found as a documented practice. |
| **T4. Per-Verb policy** | For example: navigation Verbs drop (T1), while `impact` and the PR report use T2 or T3, or run under a deployment-level privileged identity. | More rules to document and test. The deterministic PR impact report becomes visibility-dependent, so two reviewers see different reports. |

Further points **(analysis)**:

- **The control-plane edge table is a pre-filter point.** `xrepo_edges`
  joins can apply the visibility predicate in SQL, as Sourcegraph's
  `referenceIDsQuery` does. Both `from_repo` and `to_repo` must be visible
  under T1.
- **Package facts leak too.** `PROVIDES Repo → Package` and
  `DEPENDS_ON Repo → Package/Repo` reveal that a hidden repo publishes or
  consumes a package. Under T1 the moniker resolves to "package not
  indexed", the same response as a genuinely external package.
- **Node IDs are content-derived strings** such as `repo:github.com/acme/api`
  (RFC 0001 §5). A guessed ID for a hidden node must give the same error as
  a nonexistent one, for every Verb that takes an `id`.
- **`owners` and Team nodes.** If Teams are graph nodes (`OWNS Team →
  File/Dir`), a Team's membership and ownership may themselves need a
  visibility rule. That is not settled anywhere yet.

### 5.6 Memory recall

The existing rules (§5.1) generalise directly if "indexed" becomes "visible
to the caller" **(analysis)**:

- **Recall predicate.** A Memory is recallable by a Member only if *every*
  anchored repo is visible to that Member. This is the conservative AND
  already used for admission, and it keeps "A Memory must anchor every repo
  whose non-public content its body discloses" meaningful.
- **Promotion attestation changes meaning.** "Promoting a Memory attests
  that its content is fit for org-wide visibility" would become "fit for
  every Member who can see all its anchored repos". A Team-scoped Memory
  (one of the four Team powers in #121) adds a second axis: *scope* says
  where it is recalled, and the Team boundary says who may see it. Whether
  Team scope is an extra AND on top of anchor visibility, or replaces it, is
  open.
- **Proposals and reviewers.** A named reviewer must be able to see every
  anchored repo, or the proposal discloses content. Reviewer selection, and
  review routing to Teams, must be visibility-checked at proposal time and
  again at decision time. The latter is like the existing "reviewer's grant
  to be current at that moment" rule.
- **Supersession across visibility.** A correction anchored to more repos
  than its predecessor can be invisible to someone who sees the predecessor.
  The current rule (omit hidden links "without placeholder or count") already
  covers the display. Whether the hidden successor *displaces* the visible
  predecessor for that Member needs a rule. One option is per-caller heads,
  matching "For each caller, `recall` returns the newest Memory in each
  chain branch that the caller can see".
- **Ranking.** Postgres `ts_rank` uses no global statistics (§3.6), so a
  `WHERE` visibility predicate is a leak-free pre-filter for full-text
  `recall`. The same is not true if M3 semantic recall uses a shared ANN
  index (§5.4).
- **Revocation latency and the restore gate.** Team membership and Team
  grants are authority. They would join the authority-revision entries that
  RFC 0001 §5.1 already logs ("every authority change … every change in a
  repo's effective admission"), so a Postgres restore cannot silently
  re-grant visibility.

### 5.7 The Ask agent

Ask is an MCP client of the Index Server (map #121 Notes). Glean's
documented model is the reference point: agents "execute with the identity
and permissions of the signed-in user who triggered the run", and answers
are generated "only from content the querying user is authorized to access".
Yggdrasil-specific options **(analysis)**:

| Option | Identity Ask queries with | Costs |
|---|---|---|
| **K1. Asker's identity** (Glean model) | A per-asker token, or a delegated credential bound to the asker for that run. | In Slack or Waddle the *answer is posted to a channel*, so everyone in the channel reads content gathered under the asker's visibility. That leaks to channel members who lack it. It needs a mapping from chat identity to Member. |
| **K2. Audience intersection** | Query with the intersection of the visible sets of everyone who can read the reply: channel members, or the DM pair. | Needs channel membership from the surface at answer time, and membership can change after posting. The narrowest answers. |
| **K3. Surface-bound Team** | Each channel is bound by an Admin to a Team, and Ask runs with that Team's visibility. | Admin configuration per channel. It relies on the Admin binding matching who is actually in the channel. |
| **K4. Asker identity, private reply** | As K1, but restricted answers go only to the asker (DM or ephemeral message), with a public "answered privately" note. | Surface-dependent UX. Ephemeral messages are a Slack feature, and whether Waddle has an equivalent was not checked **(unverified)**. |

In every option:

- **Citations must be re-checked at render time.** This is GitHub's
  "double-check permissions" step. Cited node IDs and Memory IDs must still
  be visible to the reader when the answer is shown or reopened.
- **Answer and embedding caches must be keyed by the visible set and a
  permission epoch.** A cache keyed by query text alone would replay one
  Member's answer to another (Zanzibar §3.2.5). Shard content caches keyed
  by checksum stay safe, because visibility is checked before a Shard is
  opened.
- **The LLM context is the leak surface.** Anything retrieved under one
  identity must not persist into a later turn for a different audience,
  such as a thread with new participants.

### 5.8 Caching and freshness in Yggdrasil

- **Content caches are visibility-independent.** The NVMe Shard cache is
  keyed by segment checksum (RFC 0001 §6). As long as visibility is
  enforced before a Shard is resolved, the cache needs no change
  **(analysis)**.
- **The visible-set cache is new.** Each request needs "repos visible to
  Member M". The options:
  - compute it per request with a Postgres join (simplest, always fresh);
  - materialise it per Member (Sourcegraph `user_repo_permissions`,
    SpiceDB Materialize), invalidated on Team, grant and admission changes;
  - quantised snapshot keys (Zanzibar).

  The costs are query load against write amplification on membership
  changes.
- **Forge-synced membership has a lag floor.** Revocation in the Forge takes
  effect only at the next sync or webhook. Sourcegraph's worst case is a
  full sync cycle. Whether Yggdrasil states a bound, or offers an immediate
  local revoke that overrides sync, is a design choice.

### 5.9 Admin bypass

Sourcegraph's default lets site admins see everything, with an opt-in to
enforce. Yggdrasil's Admin today is the deployment operator, and the
bootstrap token already cannot run normal Verbs (RFC 0001 §8). The options
**(analysis)**:

- Admins see all repos.
- Admins see only what their Teams grant, plus an audited, step-up "view
  as" or break-glass.
- Admins manage grants but cannot query restricted content at all.

## 6. Options at a glance

| Decision | Options | Cheapest | Most protective |
|---|---|---|---|
| Unit of visibility | A repo-level · B per-doc in Shard · C A + path post-filter · D split Shards | A | A or D |
| Search | pre-filter fan-out (existing shape) · global tier with visible-only stats | pre-filter | pre-filter |
| Cursor binding | re-check only · re-check + Member-bound | re-check only | both |
| Cross-repo edges | T1 drop · T2 opaque node · T3 redacted count · T4 per-Verb | T1 | T1 (but `impact` is incomplete) |
| Memory | AND over anchors (existing shape) · plus Team-scope AND | AND | both |
| Ask identity | K1 asker · K2 audience intersection · K3 bound Team · K4 private reply | K1 | K2 |
| Visible-set cache | per-request join · materialised · snapshot-keyed | per-request | per-request |
| Admin | bypass · enforce + break-glass · no content access | bypass | no content access |

## 7. Unverified and open

- Whether GitHub code search, Glean or Sourcegraph/Zoekt compute ranking
  statistics over visible content only. None of their docs say so.
- Whether Sourcegraph surfaces Zoekt's per-query `Stats` to end users.
- Filtered-ANN support in the M3 candidate libraries (usearch, hnsw_rs,
  lance).
- Whether Waddle supports ephemeral or private replies (Ask option K4).
- Sourcegraph source after 2024-09 is private. Its current behaviour may
  differ from the archived snapshot cited here.
- The "redacted aggregate" treatment of hidden edges (T3) was not found as a
  documented practice in any surveyed system.

## Sources

- Sourcegraph docs: [permissions](https://sourcegraph.com/docs/admin/permissions), [permission syncing](https://sourcegraph.com/docs/admin/permissions/syncing)
- Sourcegraph source (`sourcegraph-public-snapshot@c864f15`): [`repos_perm.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/database/repos_perm.go), [`repos.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/database/repos.go), [`zoekt_global.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/search/zoekt/zoekt_global.go), [`sub_repo_perms_job.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/search/job/jobutil/sub_repo_perms_job.go), [`codenav/service.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/service.go), [`uploads/internal/store/uploads.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/uploads/internal/store/uploads.go)
- Zoekt (`b55ce72`): [`doc/design.md`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/doc/design.md), [`search/shards.go`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/search/shards.go), [`api.go`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/api.go), [`query/query.go`](https://github.com/sourcegraph/zoekt/blob/b55ce727df8cfbed76b8a1420445fd0cf93f3256/query/query.go)
- GitHub: [The technology behind GitHub's new code search](https://github.blog/engineering/architecture-optimization/the-technology-behind-githubs-new-code-search/) (2023-02-06); [Troubleshooting the REST API](https://docs.github.com/en/rest/using-the-rest-api/troubleshooting-the-rest-api)
- Glean: [Indexing API permissions](https://developers.glean.com/api-info/indexing/documents/permissions); [Security principles](https://docs.glean.com/security/security-principles)
- Onyx (`2582a29`): [`access/models.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/access/models.py), [`access_filters.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/context/search/preprocessing/access_filters.py), [`opensearch/search.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/onyx/document_index/opensearch/search.py), [`sync_params.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/ee/onyx/external_permissions/sync_params.py), [`post_query_censoring.py`](https://github.com/onyx-dot-app/onyx/blob/2582a29ef5a52ffff360777b02c47ebff48478ff/backend/ee/onyx/external_permissions/post_query_censoring.py)
- Elasticsearch: [Document and field level security](https://www.elastic.co/docs/deploy-manage/users-roles/cluster-or-deployment-auth/controlling-access-at-document-field-level); [Security limitations](https://www.elastic.co/docs/deploy-manage/security/limitations); [Security settings](https://www.elastic.co/docs/reference/elasticsearch/configuration-reference/security-settings)
- Lucene: [`BooleanClause.Occur`](https://lucene.apache.org/core/9_11_0/core/org/apache/lucene/search/BooleanClause.Occur.html)
- tantivy (`0aa896f`): [`src/query/bm25.rs`](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/src/query/bm25.rs), [`src/query/mod.rs`](https://github.com/quickwit-oss/tantivy/blob/0aa896fe29417107ad0bbab502c1a9fd1077f272/src/query/mod.rs); [`FilterCollector` (docs.rs)](https://docs.rs/tantivy/latest/tantivy/collector/struct.FilterCollector.html)
- PostgreSQL: [Controlling text search, ranking](https://www.postgresql.org/docs/current/textsearch-controls.html)
- Neo4j: [Read privileges](https://neo4j.com/docs/operations-manual/current/authentication-authorization/privileges-reads/); [Limitations](https://neo4j.com/docs/operations-manual/current/authentication-authorization/limitations/); [Access control tutorial](https://neo4j.com/docs/operations-manual/current/tutorial/access-control/)
- Pang et al., [Zanzibar: Google's Consistent, Global Authorization System](https://www.usenix.org/system/files/atc19-pang.pdf), USENIX ATC '19
- AuthZed: [Protecting a list endpoint](https://authzed.com/docs/spicedb/modeling/protecting-a-list-endpoint)
- Büttcher & Clarke, [A Security Model for Full-Text File System Search in Multi-User Environments](https://www.usenix.org/legacy/events/fast05/tech/full_papers/buettcher/buettcher_html/index.html), USENIX FAST '05
