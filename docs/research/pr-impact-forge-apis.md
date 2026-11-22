# PR impact report: forge check APIs and the cost of indexing CR heads

Status: research note · Date: 2026-11-23 · Ticket: #129 (map #121) · Yggdrasil
baseline: commit `c4bec0c` on `main`

> Scope. The decision already taken is a deterministic, graph-derived impact
> report on each Change Request (CR): cross-repo callers and dependents,
> affected owners and Teams, and the Memories the change would make stale. No
> LLM opinion. This note covers three things the design needs: where each Forge
> lets a bot put that report and how the bot authenticates; how other tools
> compute diff impact; and what indexing CR heads would cost under ADR 0005.
> It does not choose between the options.
>
> Sourcing. External claims link to primary sources: forge API docs, vendor
> docs and source code at pinned commits. All were retrieved on 2026-11-23.
> Yggdrasil claims cite repository paths at the baseline commit. Anything not
> confirmed against a primary source is marked **unverified**. Cost figures that
> rest on assumptions are labelled as such.

## 1. Summary

- **Every Forge offers the same floor: a commit status plus a CR comment.**
  Only GitHub has a rich, app-only Checks API, with Markdown output, line
  annotations and buttons. GitLab adds external status checks, but only on
  Ultimate, and they fail after two minutes pending. Forgejo/Codeberg has
  commit statuses and comments only.
- **Auth models differ sharply.**
  - GitHub Checks require a GitHub App: a JWT exchanged for one-hour
    installation tokens.
  - GitLab uses a project or group access token, which creates a bot user.
    On GitLab.com that needs Premium or Ultimate.
  - Forgejo uses an ordinary access token, optionally limited to specific
    repositories, for a dedicated (bot) user.
  - Yggdrasil's Forge seam today knows only a static bearer token
    (`crates/yg-sync/src/forge.rs`).
- **Graphify does not index PR heads, in either product.**
  - The open-source `get_pr_impact` matches the PR's changed file *paths*
    against the local graph and counts nodes and communities.
  - The hosted review "compare[s] the pull request's changes against the
    indexed default branch". Its cross-repo section is advisory.
- **Products that do index changes do it as a stacked delta over a base
  index, not as a fresh full index.** Examples: Meta's Glean (stacked immutable
  databases, "diff sketches" for code review), CodeQL's 2026 incremental PR
  analysis, and Sourcegraph's position translation from the nearest indexed
  commit.
- **For Yggdrasil, most of the report needs no head Shard.**
  - Cross-repo callers and dependents of changed symbols live in other repos'
    default-branch Shards and in `xrepo_edges`.
  - Memory staleness is a fingerprint comparison. Fingerprints come from the
    tree-sitter span in both passes (RFC 0001 §5), so a syntactic parse of the
    changed files at the head is enough to predict it.
  - Head indexing matters mainly for edges the CR *adds*, and for precise
    resolution of changed code.
- **The options range from no head parse to a full Shard per head.** The four
  options in §5 span about two orders of magnitude in compute and storage. The
  deciding variables are pushes to open CRs per day, and whether the precise
  (SCIP) pass runs on heads, which means running build code from fork PRs.

## 2. Forge surfaces

### 2.1 GitHub

**Checks API (check runs).**

- Write access is app-only: "Write permission for the REST API to interact
  with checks is only available to GitHub Apps. OAuth apps and authenticated
  users can view check runs and check suites, but they are not able to create
  them" ([Checks guide](https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-checks)).
  The fine-grained PAT permissions page does not list
  `POST /repos/{owner}/{repo}/check-runs` under any permission
  ([fine-grained PAT permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens)).
- `POST /repos/{owner}/{repo}/check-runs` requires `name` and `head_sha`
  ([Check runs](https://docs.github.com/en/rest/checks/runs)). The run's
  output has a required `title` and `summary`. `summary` and `text` are
  Markdown, at most 65,535 characters each.
- Annotations:
  - at most 50 per request, appended across further updates;
  - fields `path`, `start_line`, `end_line`, `annotation_level` (`notice`,
    `warning` or `failure`) and `message`.
- `conclusion` is required to complete a run. Only GitHub can set `stale`.
  `details_url` links out to the integrator's own page, and `external_id`
  carries the integrator's reference.
- Up to three `actions` buttons fire the `check_run.requested_action` webhook
  ([Check runs](https://docs.github.com/en/rest/checks/runs)).
- Permission: "Checks", write
  ([App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)).
  `check_suite.requested` and `check_run.rerequested` are delivered only to
  apps with Checks write
  ([webhook events](https://docs.github.com/en/webhooks/webhook-events-and-payloads)).
- Statuses gate merges through protected branches
  ([Checks guide](https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-checks)).
  **Unverified:** how check runs created by an app installed on the base repo
  show up on PRs from forks. Fork PRs are a known edge case for the
  `pull_requests` array in check payloads; not confirmed here.

**Commit statuses.**

- `POST /repos/{owner}/{repo}/statuses/{sha}` takes `state`
  (`error|failure|pending|success`), `target_url`, `description` and
  `context`. There is "a limit of 1000 statuses per sha and context within a
  repository"
  ([Commit statuses](https://docs.github.com/en/rest/commits/statuses)).
- Statuses are binary pass/fail plus a link, so the report itself would live
  at `target_url`.
- Any of these can write them: users with push access, the OAuth `repo:status`
  scope ("without also granting access to repository code"), a fine-grained PAT
  with "Commit statuses: write", or a GitHub App with the same permission
  ([Commit statuses](https://docs.github.com/en/rest/commits/statuses);
  [fine-grained PAT permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens);
  [App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)).

**PR comments and reviews.**

- PR conversation comments use the issues endpoint
  (`POST /repos/{owner}/{repo}/issues/{issue_number}/comments`, editable with
  `PATCH .../issues/comments/{comment_id}`). It needs Issues or Pull requests
  write.
- Line-anchored review comments and reviews use
  `POST .../pulls/{pull_number}/comments` and `.../reviews`, which need Pull
  requests write.
- Reading the CR's file list (`GET .../pulls/{n}/files`) needs Pull requests
  read. `GET .../compare/{basehead}` needs Contents read
  ([App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)).
- The file list returns "a maximum of 3000 files"
  ([Pulls](https://docs.github.com/en/rest/pulls/pulls)). Large CRs therefore
  need a git-side diff, not the API.

**Getting the head.**

- `git fetch origin pull/ID/head:BRANCH`. "The remote `refs/pull/` namespace
  is read-only", and PR commits "are available in a repository even before the
  pull request is merged"
  ([Checking out PRs locally](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/checking-out-pull-requests-locally)).
  A worker could therefore fetch heads into the existing bare cache without a
  second clone.
- `merge_commit_sha` holds a test merge commit that "is not added to the base
  branch or the head branch". `mergeable` is computed in the background and can
  be `null` ([Pulls](https://docs.github.com/en/rest/pulls/pulls)).

**Events.**

- `pull_request` actions include `opened`, `reopened`, `ready_for_review`,
  `closed` and `synchronize` ("A pull request's head branch was updated"). A
  GitHub App needs Pull requests read to subscribe.
- Deliveries are signed in `X-Hub-Signature-256` (HMAC-SHA256 of the body)
  when a secret is set
  ([webhook events](https://docs.github.com/en/webhooks/webhook-events-and-payloads)).

**App auth.**

- The app signs a JWT with RS256. Its `exp` must be "no more than 10 minutes
  into the future", and `iat` should be set 60 s in the past for clock drift
  ([JWT](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app)).
- It exchanges the JWT at `POST /app/installations/{id}/access_tokens` for an
  installation token, which "will expire after 1 hour". The token can be
  narrowed to listed repositories (up to 500) and to a subset of permissions
  ([installation token](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)).

**Rate limits.**

- Installations get at least 5,000 requests/hour. Outside Enterprise Cloud
  this grows by 50 per repository or user beyond 20, up to 12,500.
  Enterprise Cloud installations get 15,000. PATs get 5,000
  ([rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)).
- Secondary limits: "no more than 80 content-generating requests per minute"
  and "no more than 500 content-generating requests per hour". They can change
  without notice.
- With one check-run create plus one update per push, that is about 250 CR
  pushes per hour per writer before throttling.
- **Unverified:** whether the secondary limit counts per installation or per
  app, and whether `PATCH` of an existing comment counts as content-generating.

### 2.2 GitLab

**Commit status.**

- `POST /projects/:id/statuses/:sha` takes `state`
  (`pending|running|success|failed|canceled|skipped`) plus `name`/`context`,
  `ref`, `target_url`, `description` (each at most 255 characters), `coverage`
  and `pipeline_id`. It "adds or updates" a status, represented as a job in an
  `external` stage.
- If no suitable pipeline exists, GitLab creates one with
  `CI_PIPELINE_SOURCE: external`. A `409` means a concurrent update for that
  SHA/ref, so retry. For MRs, "target the commit in the merge request's source
  branch" ([Commits API](https://docs.gitlab.com/api/commits/)).
- "Create commit status" and "Update commit status" need the Developer role
  ([permissions](https://docs.gitlab.com/user/permissions/)).
- The description cap of 255 characters means the report body must live at
  `target_url` or in a note.

**External status checks.**

- These are Ultimate only (GitLab.com, Self-Managed and Dedicated). GitLab
  sends the MR webhook payload, with an `external_approval_rule` object, to
  the external service when the MR is updated or code is pushed. The service
  answers through the status-check API with `passed` or `failed`.
- A response for an outdated commit gets `409 Conflict`. Merges are blocked
  only if the project enables **Status checks must succeed**.
- "Status checks fail if they stay in the pending state for more than two
  minutes." Authentication uses an HMAC shared secret that cannot be viewed
  after creation
  ([status checks](https://docs.gitlab.com/user/project/merge_requests/status_checks/)).
- The two-minute timeout is a hard latency budget if this surface is used.

**MR notes and threads.**

- `POST /projects/:id/merge_requests/:iid/discussions` creates a thread.
  Diff-anchored threads take a `position` with `base_sha`, `start_sha` and
  `head_sha`, taken from `/merge_requests/:iid/versions`, plus
  `new_path`/`new_line` or `old_path`/`old_line`. Plain notes use the Notes API
  ([Discussions](https://docs.gitlab.com/api/discussions/)).
- "Comment and add suggestions" on an MR needs the Reporter role
  ([permissions](https://docs.gitlab.com/user/permissions/)).
- The MR versions API also provides the base, start and head triple that the
  diff calculation needs.

**Getting the head.**

- `refs/merge-requests/:iid/head` exists for every MR. It "is deleted 14 days
  after a merge request is closed or merged"
  ([MR troubleshooting](https://docs.gitlab.com/user/project/merge_requests/merge_request_troubleshooting/)).

**Events.**

- Merge request hook actions: `open`, `close`, `reopen`, `update`,
  `approval`, `approved`, `unapproval`, `unapproved` and `merge`. New commits
  show up as `object_attributes.oldrev` on `update`
  ([webhook events](https://docs.gitlab.com/user/project/integrations/webhook_events/)).
- The legacy secret is sent "as plain text in the `X-Gitlab-Token` HTTP
  header" and is "not recommended for new webhooks".
- Since GitLab 19.0 (GA in 19.1), a signing token adds `webhook-id`,
  `webhook-timestamp` and `webhook-signature` (`v1,` + base64 HMAC-SHA256 over
  `{message_id}.{timestamp}.{body}`). Four consecutive failures disable the
  hook temporarily, and 40 disable it permanently
  ([webhooks](https://docs.gitlab.com/user/project/integrations/webhooks/)).
- This bears on RFC 0001 §11 Q7 (webhook signature schemes).

**Bot auth.**

- A project access token "creates a bot user", a non-billable project member,
  with a role assigned at creation. Expiry defaults to 365 days, and admins
  can change the maximum on 17.6+.
- On GitLab.com it needs Premium or Ultimate. On Self-Managed and Dedicated it
  is available with any license
  ([project access tokens](https://docs.gitlab.com/user/project/settings/project_access_tokens/)).
- Group access tokens are the group-wide analogue. **Unverified:** their
  tier gating and role details were not read for this note.
- GitLab has no GitHub-App-style installation model in the sources read.

### 2.3 Forgejo / Codeberg

Codeberg serves Forgejo; its API reports version `17.0.0-121-39320c0+gitea-1.22.0`
([Codeberg swagger](https://codeberg.org/swagger.v1.json)).

**Commit status.**

- `POST /repos/{owner}/{repo}/statuses/{sha}` takes `CreateStatusOption`:
  `state`, `target_url`, `description` and `context`. States are "pending",
  "success", "error", "failure", "warning" or "skipped"
  ([Codeberg swagger](https://codeberg.org/swagger.v1.json)).
- The route needs a token and repository *code write*
  (`reqRepoWriter(unit.TypeCode)`), inside the repository token-scope group
  ([routers/api/v1/api.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/routers/api/v1/api.go)).
- There is no Checks-style API. No check-run or check-suite path exists in the
  swagger.

**Comments and reviews.**

- Issue/PR comments use `POST /repos/{owner}/{repo}/issues/{index}/comments`,
  with `PATCH .../issues/comments/{id}` to edit. These sit under the *issue*
  token scope.
- Reviews use `POST .../pulls/{index}/reviews` (`CreatePullReviewOptions`:
  `body`, `event`, `commit_id`, `comments[]`). Each comment takes `path`,
  `new_position`/`old_position` and `extra_lines_count`.
- Changed files come from `GET .../pulls/{index}/files`
  ([Codeberg swagger](https://codeberg.org/swagger.v1.json);
  [api.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/routers/api/v1/api.go)).

**Getting the head.**

- `refs/pull/<index>/head`
  ([models/issues/pull.go `GetGitRefName`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/models/issues/pull.go);
  [modules/git/ref.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/git/ref.go)).

**Events.**

- The event type is `pull_request_sync`, with action `synchronized`
  ([modules/webhook/type.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/webhook/type.go);
  [modules/structs/hook.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/structs/hook.go)).
- Deliveries carry `X-Forgejo-Signature` (and `X-Gitea-Signature`), the hex
  HMAC-SHA256 of the body
  ([services/webhook/shared/payloader.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/services/webhook/shared/payloader.go)).

**Bot auth.**

- Access tokens have read/write scopes per category (`repository`, `issue`,
  …). Posting both a status and a comment therefore needs `write:repository`
  and `write:issue`.
- A token can be limited to "Specific repositories". Only the repository and
  issue scopes are allowed with that option
  ([token scopes](https://forgejo.org/docs/latest/user/token-scope/)).
- Forgejo has a bot user type (`UserTypeBot`,
  [models/user/user.go](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/models/user/user.go)),
  but no app or installation model in the API.
- **Unverified:** whether Codeberg lets users create bot-type accounts. On
  Codeberg the bot is in practice a normal account.

### 2.4 Comparison

| | GitHub | GitLab | Forgejo/Codeberg |
|---|---|---|---|
| Rich report on the CR's checks surface | Check run: Markdown summary up to 65,535 chars, ≤50 annotations per call, buttons | No (commit-status description ≤255 chars) | No (status `description` only) |
| Pass/fail signal with link | Commit status or check-run conclusion | Commit status (external pipeline job); external status check (Ultimate, 2-min pending timeout) | Commit status (incl. `warning`) |
| Comment surface | Issue comment (editable), review comments | MR note or discussion thread (diff position) | Issue comment (editable), review comments |
| Bot identity | GitHub App (JWT → 1 h installation token) for Checks; PAT or app for statuses and comments | Project/group access token → bot user (GitLab.com: Premium+) | User or bot account + scoped PAT (optionally repo-limited) |
| Min. permission | Checks: write; Commit statuses: write; Issues or Pull requests: write | Developer (status), Reporter (comment) | Code write + `write:repository` (status); `write:issue` (comment) |
| Head ref | `refs/pull/N/head` (read-only) | `refs/merge-requests/IID/head` (deleted 14 d after close) | `refs/pull/N/head` |
| CR-updated event | `pull_request.synchronize` | MR hook `update` with `oldrev` | `pull_request_sync` / `synchronized` |
| Webhook auth | HMAC-SHA256 hex (`X-Hub-Signature-256`) | Plaintext `X-Gitlab-Token`; HMAC signing token since 19.0 | HMAC-SHA256 hex (`X-Forgejo-Signature`) |

**Relevance to Yggdrasil.**

- The current Forge seam reads one static token per Forge from an env var.
  It presents that token as a bearer (`github.rs` `build_poll_request`) and as
  git basic-auth (`Forge::git_auth`)
  ([crates/yg-sync/src/forge.rs](../../crates/yg-sync/src/forge.rs);
  [crates/yg-sync/src/forge/github.rs](../../crates/yg-sync/src/forge/github.rs)).
- The seam has no write path and no app-token refresh. GitLab and Codeberg
  adapters do not exist yet; they are planned for M1 and M2 (PRD Milestones).
- The report is also the first time Yggdrasil writes to a Forge, which the
  PRD non-goals currently frame as "Forges keep their jobs".

## 3. How others compute diff impact

### 3.1 Graphify (open source)

Sources pinned at
[`3671f31`](https://github.com/Graphify-Labs/graphify/tree/3671f31124fdb1de4ec4e485b1bcd7059726a5f3)
on the default branch `v8`.

- **Changed files.** `fetch_pr_files` shells out to
  `gh pr diff <n> --name-only` with a 30 s timeout. A failure yields an empty
  list
  ([prs.py L277–291](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/prs.py#L277-L291)).
  Auth is whatever `gh` is logged in as. Graphify reads PRs and "prints them",
  with no writes to GitHub
  ([docs: pull requests](https://docs.graphify.com/guides/pull-requests)).
- **Impact.**
  - `compute_pr_impact(files, G)` indexes graph nodes by `source_file`. It
    path-matches each changed file, allowing suffix matches at path
    boundaries.
  - It returns `(communities_touched, nodes_affected)`: the Leiden
    communities of all nodes in matched files, and their count. There is no
    edge traversal and no hunk-to-symbol mapping
    ([prs.py L296–333](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/prs.py#L296-L333)).
  - `blast_radius` is the string "N nodes / C communities"
    ([prs.py L89–95](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/prs.py#L89-L95)).
- **MCP tool.** `get_pr_impact` calls `gh pr view`, then `fetch_pr_files`,
  then `compute_pr_impact`. It prints CI state, review state, the node and
  community counts, and up to 20 files
  ([serve.py L3357–3385](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/serve.py#L3357-L3385)).
- **Which graph.** "Impact comes from your local graph. Run `graphify update .`
  on the base branch first"
  ([docs: pull requests](https://docs.graphify.com/guides/pull-requests)). The
  head is never parsed.
- **Conflicts.** `--conflicts` groups open PRs that share a community, which
  the docs call "a review signal, not a guarantee"
  ([docs](https://docs.graphify.com/guides/pull-requests)).
- **Ranking.** `--triage` asks an LLM to rank PRs by `blast_radius`
  ([prs.py L642ff](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/prs.py#L642)).
  That is out of scope for us.
- **Real reverse traversal exists but is not used for PRs.**
  `affected_nodes` walks reverse edges breadth-first to `depth=2` by default.
  It follows a relation set (`calls`, `references`, `imports`, `inherits`,
  `implements`, `defines`, …) and expands overridden members
  ([affected.py L12–41, L369–460](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/affected.py#L12-L41)).
  `prs.py` and `get_pr_impact` never call it.

### 3.2 Graphify (hosted)

- **Delivery.**
  - A review runs when a PR is opened, reopened or marked ready.
    `@graphify review` re-runs it after new commits.
  - It posts "a review comment from the Graphify app, and a check run named
    Graphify on the reviewed commit". Formal verification posts a separate
    check run on every push
    ([docs: reviews](https://docs.graphify.com/platform/reviews.md)).
  - The verification check is advisory: "`success` only when every changed
    function is Proven same, otherwise `neutral`, never `failure`"
    ([verification checks](https://docs.graphify.com/platform/verification-check-results.md)).
- **No head index.** "Compare the pull request's changes against the indexed
  default branch"; "Nothing from the pull request's branch is saved as an
  index" ([docs: reviews](https://docs.graphify.com/platform/reviews.md)).
- **Cross-repo.**
  - Repos joined in a combined graph get a "Cross-repo impact" section listing
    callers and dependents in peer repos. It is advisory: "It never changes
    whether the Graphify check passes or fails"
    ([docs: reviews](https://docs.graphify.com/platform/reviews.md)).
  - **Unverified:** the algorithm behind that section. It is not documented
    beyond "reviews read the graph, so findings cover callers beyond the diff"
    ([blog: blast radius](https://graphify.com/blog/code-change-blast-radius.md)).
- **Pre-change impact.** The MCP tool `graphify_impact` "returns direct
  connections only"
  ([docs: check impact](https://docs.graphify.com/concepts/check-impact)).
- The LLM merge-confidence review itself is out of scope (#121).

### 3.3 Sourcegraph

- **Nearest index.** Precise navigation resolves queries "using data from
  older commits when data on the requested commit is not yet available". It
  precomputes the nearest uploads per commit and later restricted the search
  to ancestors
  ([blog: commit graph part 2, 2021-06-03](https://sourcegraph.com/blog/optimizing-a-code-intelligence-commit-graph-part-2)).
- **Position translation.**
  - `GitTreeTranslator` maps SCIP positions and ranges between the indexed
    commit and the requested one using git diff hunks.
  - `TranslatePosition` and `TranslateRange` "return None if the given
    position is on a line that was removed or modified between from and to".
  - Hunks are cached per `(from, to, path)`
    ([gittree_translator.go L22–37](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/gittree_translator.go#L22-L37),
    public snapshot, last commit 2024-08-22).
  - This is the "overlay a diff on the base index" pattern in a shipping
    product. Changed lines simply lose precise data.
- **Fallback.** Search-based navigation is the fallback "when precise
  navigation is not available". Code navigation is offered "on diffs in your
  code review tool, via integrations"
  ([precise code navigation](https://sourcegraph.com/docs/code-navigation/precise-code-navigation.md)).
- No documented diff-impact or blast-radius report was found for Sourcegraph.
  **Unverified** whether one exists in current products.

### 3.4 Others

- **Meta Glean (stacked databases).**
  - "The goal is to be able to index the changes in *O(changes)* rather than
    *O(repository)*."
  - `glean create --incremental <old> --exclude A,B,C` stacks a new DB on a
    base and hides the listed units. Fact ownership is propagated so that no
    visible fact dangles.
  - Overheads: about 7% DB size and 2–3% indexing (Python). Typical queries
    cost under 10% more, but search-heavy queries can be about 3x slower.
    Incremental *derivation* is only O(increment) for some queries
    ([Glean blog: incremental, 2022-12-01](https://glean.software/blog/incremental/)).
  - Meta runs "indexing … on diffs" to produce a "diff sketch" for code review
    (Phabricator). In practice the bound is "O(fanout)", for example a
    changed C++ header
    ([Meta engineering, 2024-12-19](https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/)).
- **GitHub CodeQL incremental PR analysis.**
  - CodeQL generates "a CodeQL database to represent your new or changed code
    introduced in pull requests" and combines it "with a cached database for
    your entire codebase".
  - Speedups over more than 100k repos range from 4% (C#, short scans) to 70%
    (JS/TS and Python, scans over 7 minutes). It applies to the default query
    suite and build-mode-none languages
    ([changelog 2026-03-24](https://github.blog/changelog/2026-03-24-faster-incremental-analysis-with-codeql-in-pull-requests/)).
- **Nx affected.**
  - Nx diffs `base` (by default "the latest successful commit on the `main`
    branch") against `head`, and selects tasks "whose inputs include a changed
    file". "Every push to a pull request is compared with the same base
    commit", so each push selects the same tasks again.
  - The legacy mode also takes every dependent project
    ([Nx affected](https://nx.dev/docs/features/ci-features/affected)).
  - This is deterministic impact from a base-side project graph plus a file
    diff, with no head indexing.
- **Zoekt (multi-branch FTS).** Each file blob in a shard carries a
  branch bitmask, so "we can index many similar branches of a repository with
  little space overhead"
  ([zoekt design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)).
  This is a dedup precedent if head text ever has to be searchable.

**Pattern.**

- Shipping tools either skip head indexing and map the diff onto a base
  index (Graphify, Nx, Sourcegraph's translator), or build a delta layer
  stacked on a cached base (Glean, CodeQL).
- None of the sources read builds a full fresh index per PR push as the
  primary design.

## 4. What the report needs from the graph

Yggdrasil facts, at `c4bec0c`:

- **Shard model.** A Shard is an immutable per-repo artifact: `graph.sqlite`,
  a tantivy FTS directory and, later, an ANN segment. Publishing swaps a
  pointer in `shards`, and GC runs after a grace window (RFC 0001 §6; ADR
  0005).
  - A revision id is the commit SHA plus a pass and schema suffix
    (`yg_shard::syntactic_revision`).
  - The M0 syntactic pass parses the whole worktree
    (`yg_index::syntactic_pass(root)`). RFC 0001 §4 describes "changed files
    (full parse on first index)" as the target, so incremental parsing is
    design intent, not current code.
- **Staleness.** On publish, anchors are compared with the *new* revision's
  fingerprints, in the same transaction as the pointer swap (RFC 0001 §5.1).
  The fingerprint is the hash of the normalized defining span, which comes
  "from the tree-sitter parse in both passes" (RFC 0001 §5).
- **Cross-repo.** Cross-repo edges come from package facts resolved into
  `xrepo_edges` and from SCIP monikers (RFC 0001 §4).
- **Verbs.** The relevant Verbs are planned but not served: `impact`,
  `callers` (M1), and `owners`, `dependents`, `deps` (M2) (RFC 0001 §7).

Mapping the report's three parts onto these facts:

| Report part | Inputs | Needs the head parsed? |
|---|---|---|
| Changed Symbols | Diff hunks (merge-base…head) mapped onto Symbol spans | Base side: no. A hunk-to-base-span mapping, Sourcegraph-style, finds modified and removed Symbols. New Symbols and signature changes need the head parsed syntactically |
| Cross-repo callers / dependents | Changed Symbols → other repos' default-branch Shards via `xrepo_edges` and monikers | No. The callers live on default branches. Only callers the CR itself *adds* need the head |
| Affected owners / Teams | Changed paths × CODEOWNERS (base, or head if CODEOWNERS changed) → Teams | Only if CODEOWNERS is in the diff (a file read, not a parse) |
| Memories made stale | Anchors on changed Symbols and Files: base fingerprint vs head fingerprint | A syntactic parse of the changed files at the head gives exact fingerprints (same algorithm as publish). Without a parse, "span intersects hunk" is an over-approximation, since normalization can absorb whitespace-only edits |

Two further inputs are not cost items but bear on all the options:

- **Which base.** The CR's merge-base, the current base-branch head, or
  GitHub's test merge commit. The default branch moves while a CR is open, so
  a report computed against a fixed Shard revision can drift. GitHub's
  `merge_commit_sha` and GitLab's versions triple
  (`base_commit_sha`/`start_commit_sha`/`head_commit_sha`) are the
  forge-provided anchors (§2).
- **Visibility.** Under ADR 0001 the graph is org-visible. A report posted on
  a *public* repo's CR could name private-repo callers, symbols or Teams to
  anyone who can read that CR, including outside contributors on fork PRs.
  Team visibility boundaries are being reopened in #121. This is a constraint
  the design must answer, whichever indexing option is chosen.

## 5. CR-head indexing options under ADR 0005

All four options use the same event path:

1. A CR-updated event arrives (§2) through a webhook or the poll loop.
2. The worker fetches the head ref into the existing bare cache (RFC 0001
   §3).
3. It diffs the merge-base against the head, computes the report and posts it.

The options differ in what they parse and what they persist.

### Option D: diff-only, no head parse (Graphify, Nx, Sourcegraph-translator style)

- **Method.** Map hunks onto base-revision Symbol spans. Report the cross-repo
  callers, dependents and owners of those Symbols, and the Memories anchored
  to them, as "may go stale".
- **Compute.** A git diff plus Shard and control-plane reads. No parse. Cost
  is O(diff), roughly seconds.
- **Storage.** None beyond the report.
- **Latency.** Fetch plus diff plus queries, which fits inside GitLab's
  two-minute external-status-check window.
- **Gaps.**
  - Blind to Symbols and call edges the CR adds.
  - Staleness is over-approximated.
  - Renames show as remove plus add.

### Option C: syntactic-only head parse, ephemeral (nothing published)

- **Method.** Tree-sitter-parse only the changed files at the head, in
  memory. Compute head fingerprints and the Symbol-level diff (added, removed,
  changed), plus the call and import edges in changed files. Join with base
  Shards and `xrepo_edges`. Discard the result.
- **Compute.** O(changed files) parse. The M0 pass caps files at 1 MiB
  (`MAX_SOURCE_FILE_BYTES`). **Not measured:** per-file parse time.
- **Storage.** None. The report could be cached by `(base revision, head sha)`.
- **Latency.** Comparable to D plus a parse.
- **Precision.**
  - Memory staleness is exact, because it uses the same fingerprint
    algorithm as publish.
  - Callers of changed Symbols carry whatever Provenance the base has
    (precise where SCIP ran).
  - New edges are heuristic.
- **Resolution fanout.** The syntactic pass resolves calls through repo-wide
  symbol tables (`crates/yg-index/src/resolve.rs`). A change in one file can
  alter resolution in unchanged files. This is Glean's "O(fanout)" problem in
  miniature, and option C ignores it.
- **Execution.** No code execution, so fork PRs are safe to process.
- **Limits.** No graph-diff browsing Verbs over the head.

### Option B: delta or overlay Shard on the base revision (Glean and CodeQL style)

- **Method.** Persist a small immutable delta artifact under object storage,
  for example under `shards/<repo-id>/<base-revision>+cr/<head-sha>/`. The
  artifact holds the changed files' nodes, edges, FTS docs and tombstones for
  removed nodes. Query nodes compose base and delta at read time, so Verbs
  (`impact`, `callers`, `search`) can answer "as of this CR".
- **Compute.** As option C, plus artifact write, plus optional re-resolution
  of fanout files.
- **Storage.** O(changed files) per head push, kept until the CR closes or
  merges. The GitLab head ref disappears 14 days after close, which gives a
  natural GC trigger.
- **Base drift.** A delta is pinned to one base revision. When the default
  branch advances, the delta is either rebased (recomputed against the new
  base) or served against a GC-protected old base.
- **Complexity.**
  - A read-time merge in the Verb engine. The design resolves one current
    Shard per repo through the `shards` pointer (RFC 0001 §6), and nothing
    composes two Shards today.
  - Delta-aware Shard GC.
  - Tombstone semantics for FTS segments.
  - Glean reports up to about 3x slowdown on search-heavy queries over stacks
    ([Glean blog](https://glean.software/blog/incremental/)).
- **Precise pass.** An optional precise delta needs a SCIP run on the head.
  The SCIP indexers in the RFC run per project and build dependencies, so a
  "changed files only" precise pass is not generally available.
  **Unverified:** per-indexer incremental modes. In practice precise stays a
  full-head job, as in option A.

### Option A: full Shard per CR head

- **Method.** Run the normal pipeline on the head commit (syntactic, and
  optionally precise). Publish an ordinary Shard revision that is never
  pointer-swapped in as the repo's current Shard. Diff the base and head
  graphs.
- **Compute.**
  - Syntactic: a whole-worktree parse per push, exactly what M0 does per
    default-branch change. **Not measured:** per-repo syntactic time. The PRD
    target is only "minutes".
  - Precise: about 4 worker-minutes average per run, with cached dependencies
    (RFC 0001 §4; PRD full-org budget of about 5000 × 4 worker-minutes,
    roughly 333 worker-hours).
- **Storage.** A full Shard per pushed head, for the CR's life. Content
  dedup, for example Zoekt-style branch masks, does not exist in the current
  format (RFC 0001 §6).
- **Latency.** Syntactic within the PRD's freshness envelope (p50 ≤ 8 min by
  poll, ≤ 3 min by webhook). Precise "when its build lands", within the hour.
  It cannot meet GitLab's two-minute pending window, so on GitLab it must use
  commit status, not external status checks.
- **Security.** The precise pass runs repo-controlled build code (PRD Risks).
  On fork PRs of public repos, anyone who can open a PR can trigger it. The
  sandbox is the same, but the set of people who can trigger it widens.
- **Gains.** It is the only option that gives a full precise graph diff and
  head-side Verbs without a read-time merge.

### Illustrative cost

This calculation is **assumption-driven**. `P` is pushes to open CRs per
day, organisation-wide. The PRD fixes no value; 500 Members × 2 pushes per
workday gives `P ≈ 1000`.

| Option | Worker time per push | Daily worker time at P = 1000 | Persistent storage |
|---|---|---|---|
| D | ~seconds (diff + reads) | well under 1 worker-hour (**estimate**) | none |
| C | seconds + parse of changed files (**not measured**) | ~1 worker-hour or less (**estimate**) | none / report cache |
| B (syntactic delta) | C + artifact write | ~C | O(changed files) × open heads |
| A (syntactic only) | full-repo parse (**not measured**) | P × full parse | full Shard × open heads |
| A (+ precise) | ≈ 4 min avg (RFC) | ≈ 4000 worker-min ≈ 67 worker-hours/day ≈ 20% of the 333 worker-hour full-org budget, every day | full Shard × open heads |

Forge write budget for every option on GitHub: two content-generating writes
per push against a secondary limit of 500 per hour (§2.1). That allows about
250 pushes per hour per writer. An org with bursty pushes may need batching or
a single updated comment. **Unverified:** whether edits count against the
limit.

## 6. Open points for the design ticket (not decided here)

- Surface per Forge:
  - GitHub: a check run (requires a GitHub App, a new auth mode in the Forge
    seam) or a status plus comment (works with the current token model).
  - GitLab: commit status plus note, or Ultimate-only external status checks.
  - Forgejo: status plus comment only.
- Whether the report is advisory only (Graphify hosted never fails its
  cross-repo check) or can gate merges.
- Which head-indexing option, and whether the precise pass ever runs on
  heads, especially from forks.
- Which base the report diffs against, and how base drift is handled.
- How the report respects visibility when posted on a public CR (§4).
- Dependencies on unbuilt pieces:
  - M1: `impact` and `callers`, plus stable Symbol IDs and fingerprints;
  - M2: `owners` and `dependents`, Forge Artifacts and CODEOWNERS, plus the
    GitLab and Codeberg adapters;
  - M3: the webhook accelerator.
- The PRD changes needed: removing "Indexing non-default branches or CR
  heads" from the non-goals, amending G3, and updating RFC 0001 §11 Q6.

## Sources

GitHub:
- [Check runs](https://docs.github.com/en/rest/checks/runs)
- [Checks guide](https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-checks)
- [Commit statuses](https://docs.github.com/en/rest/commits/statuses)
- [Pulls](https://docs.github.com/en/rest/pulls/pulls)
- [App permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-github-apps)
- [Fine-grained PAT permissions](https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens)
- [JWT](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-json-web-token-jwt-for-a-github-app)
- [Installation token](https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-an-installation-access-token-for-a-github-app)
- [Rate limits](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api)
- [Webhook events](https://docs.github.com/en/webhooks/webhook-events-and-payloads)
- [Checking out PRs locally](https://docs.github.com/en/pull-requests/collaborating-with-pull-requests/reviewing-changes-in-pull-requests/checking-out-pull-requests-locally)
- [CodeQL incremental changelog](https://github.blog/changelog/2026-03-24-faster-incremental-analysis-with-codeql-in-pull-requests/)

GitLab:
- [Commits API](https://docs.gitlab.com/api/commits/)
- [External status checks](https://docs.gitlab.com/user/project/merge_requests/status_checks/)
- [Discussions API](https://docs.gitlab.com/api/discussions/)
- [Permissions](https://docs.gitlab.com/user/permissions/)
- [Project access tokens](https://docs.gitlab.com/user/project/settings/project_access_tokens/)
- [Webhooks](https://docs.gitlab.com/user/project/integrations/webhooks/)
- [Webhook events](https://docs.gitlab.com/user/project/integrations/webhook_events/)
- [MR troubleshooting](https://docs.gitlab.com/user/project/merge_requests/merge_request_troubleshooting/)

Forgejo/Codeberg:
- [Codeberg swagger](https://codeberg.org/swagger.v1.json)
- [Token scopes](https://forgejo.org/docs/latest/user/token-scope/)
- Forgejo source on the `forgejo` branch:
  - [`routers/api/v1/api.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/routers/api/v1/api.go)
  - [`services/webhook/shared/payloader.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/services/webhook/shared/payloader.go)
  - [`modules/webhook/type.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/webhook/type.go)
  - [`modules/structs/hook.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/structs/hook.go)
  - [`modules/git/ref.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/git/ref.go)
  - [`models/issues/pull.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/models/issues/pull.go)
  - [`models/user/user.go`](https://codeberg.org/forgejo/forgejo/src/branch/forgejo/models/user/user.go)

Graphify:
- Source at `3671f31`:
  - [`graphify/prs.py`](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/prs.py)
  - [`graphify/serve.py`](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/serve.py)
  - [`graphify/affected.py`](https://github.com/Graphify-Labs/graphify/blob/3671f31124fdb1de4ec4e485b1bcd7059726a5f3/graphify/affected.py)
- Docs:
  - [pull requests](https://docs.graphify.com/guides/pull-requests)
  - [reviews](https://docs.graphify.com/platform/reviews.md)
  - [verification checks](https://docs.graphify.com/platform/verification-check-results.md)
  - [check impact](https://docs.graphify.com/concepts/check-impact)
- [Blog: blast radius](https://graphify.com/blog/code-change-blast-radius.md)

Sourcegraph and others:
- Sourcegraph:
  - [`gittree_translator.go`](https://github.com/sourcegraph/sourcegraph-public-snapshot/blob/c864f15af264f0f456a6d8a83290b5c940715349/internal/codeintel/codenav/gittree_translator.go)
  - [commit graph blog](https://sourcegraph.com/blog/optimizing-a-code-intelligence-commit-graph-part-2)
  - [precise code navigation](https://sourcegraph.com/docs/code-navigation/precise-code-navigation.md)
  - [zoekt design](https://github.com/sourcegraph/zoekt/blob/main/doc/design.md)
- Glean:
  - [incremental indexing blog](https://glean.software/blog/incremental/)
  - [Meta engineering post](https://engineering.fb.com/2024/12/19/developer-tools/glean-open-source-code-indexing/)
- [Nx affected](https://nx.dev/docs/features/ci-features/affected)
