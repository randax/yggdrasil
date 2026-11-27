# The PR impact report: syntactic head revisions, one revocable comment, a pure function

Status: accepted. Builds on [ADR 0008](0008-team-granted-repository-visibility.md) (the narrow PR comment, the GUI report behind a link, recompute on narrowing), [ADR 0011](0011-aggregated-budgeted-decisive-payloads.md) (budgeted, aggregated shapes) and [ADR 0012](0012-shard-layout-and-incremental-indexing.md) (complete revisions, the blob cache, publish sequences). Moves CR-head indexing from a post-v1 question (RFC 0001 §11 Q6) into the plan, and makes Yggdrasil write to Forges for the first time.

The report answers one reviewer question: what breaks if this Change Request merges now. It is deterministic and graph-derived, with no model opinion. The [forge research](https://github.com/randax/yggdrasil/blob/5fcd7d45db11042a5f49cf2f396b317575f97d9f/docs/research/pr-impact-forge-apis.md) found that most of it needs no head data: callers, owners and Memory staleness come from base Shards plus the changed Symbols. Head data matters for edges and Symbols the CR adds, and for agents reviewing the CR. ADR 0012 makes a head cheap: with the per-blob cache only changed files are parsed.

## The decisions

### Each CR head is a complete syntactic revision, never current

A CR head is built as a complete Shard revision of the head commit, exactly as ADR 0012 builds the default branch, and published under a per-CR pointer instead of the repo's pointer. It is never made current, so it never runs Memory staleness checks and writes no cross-repo rows: other repos never see a head's references. Its outbound edges into other repos resolve per view at read time like any revision's.

Heads get no precise pass: it would cost about 4 worker-minutes per push (around 67 worker-hours a day at 1,000 pushes, a fifth of the fleet budget) and would run build code on every push. A head does reuse ADR 0012's stored precise output for every Precise Root whose input hash the CR leaves unchanged, so untouched roots stay precise and only changed roots are syntactic. Every edge carries its Provenance, so the report never overstates.

### Which CRs, and the limits

- Every open CR that targets the default branch of an admitted repo gets a head revision, drafts and fork CRs included. Fork CRs are safe: nothing on a head runs code.
- Only a CR's newest push is built; a later push cancels a pending build. Head builds queue below default-branch jobs, so they never cost the syntactic freshness target.
- A repo keeps head revisions for at most its 50 most recently pushed CRs; a CR idle for 30 days loses its head revision until its next push; a CR changing more than 5,000 files gets a report marked as truncated. An Admin can change all three.
- A head revision belongs to its base repo and inherits its Visibility. It is deleted when the CR closes or merges, after ADR 0012's retention window.

### What the report is a function of

The report is a pure function of the head revision, the merge-base commit, one ADR 0012 publish sequence, and the reader's ceiling (for the comment) or effective view (for the GUI report):

- **What changed** is head against merge-base. The merge-base's per-file facts come from the blob cache and are built in memory, so the report shows only the CR's own changes, never what the default branch gained since it branched.
- **Who is affected** (callers, dependents, owners, tests, Memories) comes from every repo's current default-branch revision at that publish sequence.

Identical inputs give an identical report, so the pipeline is snapshot-tested on fixture repos and an unchanged result is never reposted. The comment states the head sha and an "as of" time.

The report is recomputed on every push; on ADR 0008 narrowing; and on drift, when the publish sequence has moved, at most hourly per CR and only if the result changed.

### What the report contains

Each changed Symbol is classified as added, removed, signature-changed or body-only-changed. That needs a second per-node hash: alongside ADR 0007's fingerprint of the normalised defining span, which stays the staleness fingerprint, each Symbol gets an interface hash of its normalised declaration without the body. Both come from the tree-sitter parse in both passes.

Sections, in order:

1. **Summary:** counts of Symbols added, removed, signature-changed and body-only-changed, and of repos, owners and tests affected.
2. **Changed exported API:** `EXPORTS` Symbols whose signature changed or that were removed, first because they break callers.
3. **Cross-repo callers and dependents:** grouped by repo, ranked by caller count and then Provenance, aggregated as ADR 0011 requires (a precise caller says so; an ambiguous one shows "1 of N"), with signature-change impact ahead of body-only impact.
4. **Affected tests** per repo, through `TESTS` edges.
5. **Owners and Teams** of the affected code, by name. The comment never @-mentions anyone: mentions notify irrevocably and would fire on every push to a busy library. Reaching people is the notification plumbing's job.
6. **A truncation notice and the GUI link.**

The comment applies this to ADR 0008's narrow ceiling (Forge-public and Open repos plus the CR's own repo, never Memories), with a default budget of about 1,500 tokens and truncation always stated. The GUI report applies it to the signed-in Member's own effective view at read time, and adds the Memories the CR would make stale (anchors whose fingerprint differs between merge-base and head, or whose Symbol the CR removes) and the head graph to browse.

### Where it is posted

One comment per CR, plus an advisory commit status, on every Forge:

- **The comment** is updated in place while the result is no narrower. When access narrows (ADR 0008), it is deleted and reposted, or replaced by a short "report withdrawn; see the GUI" note if the CR is closed. The control plane records which repos each live comment drew on, for open and closed CRs alike, so narrowing finds every comment it affects.
- **The commit status** always reports success or neutral, says how many repos and owners are affected, and links to the GUI report. It never fails, so it never gates a merge: a gate could only weigh impact inside the narrow ceiling, and a gate that silently ignores Restricted impact would mislead.
- **The GUI link** has the same shape (`/cr/<forge>/<repo>/<n>`) whether or not anything lies beyond the ceiling, so neither the comment nor its link reveals hidden impact.
- **A repo's Forge Visibility** (public, internal, private) is read at sync and checked again before each post; if the Forge cannot answer, the repo is treated as not public.

The reporter is a Service Agent per Forge installation, with its ceiling enforced by the server (ADR 0008, [ADR 0009](0009-human-login-agent-binding-forge-sync.md)). It writes with:

- **GitHub:** a GitHub App with Issues or Pull requests write and Commit statuses write, using one-hour installation tokens. The same App delivers webhooks.
- **GitLab:** a project or group access token, which creates a bot user (Premium or higher on GitLab.com), or a dedicated bot user's token on self-managed Free.
- **Forgejo and Codeberg:** a bot user's token, limited to the repos it serves.

The Forge seam gains its first write path and app-token refresh. Writes per installation are queued under the Forge's secondary rate limits (GitHub allows about 500 content-generating writes an hour); coalescing pushes and posting only changed results keep a busy organisation inside them.

### Head revisions in the Verb contract

Every graph Verb (`node`, `neighbors`, `search`, `callers`, `callees`, `impact`, `map`, `history`, and later `path`, `deps`, `dependents`, `owners`) accepts `revision: cr:<n>` for the CR's repo, so an agent reviewing a CR can browse its head graph. The caller must have the base repo in view. Cross-repo hops from a head read the other repos' current revisions at the same publish sequence, so `impact` on a head means "what breaks if this merges now". Responses state the head sha, and ADR 0011's cursors bind the head revision like any other.

Memory Verbs ignore heads. `remember` anchors only to Symbols in the default-branch revision and refuses a `cr:` anchor with a typed error; ADR 0007's anchor model is unchanged. The Concept doc gains a short "reviewing a CR" method that starts from `impact` on `cr:<n>`.

## Considered options

- **A change summary per head, with no queryable head graph.** Cheapest, but an agent reviewing a CR could not ask what the CR's new code calls.
- **The precise pass on same-repo heads.** Precise impact for changed code, at a fifth of the fleet budget every day, and build code run for everyone with write access.
- **A delta Shard over the base revision.** ADR 0012 rejected read-time layer merging.
- **Opt-in per repo, or skipping drafts and forks.** Most repos would never get the report, and fork CRs are the outside contributions whose impact maintainers least expect.
- **A GitHub check run with line annotations instead of the comment.** Two report shapes, and whether a check run's output can be fully withdrawn on narrowing is unverified.
- **Merge gating on impact rules.** A gate can only see the narrow ceiling, so it would ignore exactly the Restricted impact that matters.
- **Counts and a link only, or @-mentioning owners.** The first gives outside readers nothing; the second pings irrevocably on every push.
- **Diffing head against the current default branch.** Changes merged since the CR branched would show as reverted by the CR.
- **Computing impact against the revisions current when the CR branched.** Those revisions are deleted after the retention window, and the report would describe callers that may no longer exist.
- **Pending Memory anchors on head Symbols, activated on merge.** Review-time Memories, at the cost of a new anchor state, merge-time mapping and orphan handling, redesigning ADR 0007's anchors.

## Consequences

- The PRD drops "indexing non-default branches or CR heads" from its non-goals, amends G3, and narrows "Forges keep their jobs": Yggdrasil writes one comment and one advisory status per CR, and still does no code review, hosting or CI.
- The report depends on M1 (`impact`, `callers`, stable Symbol IDs, fingerprints, `TESTS` and `EXPORTS` edges) and on owners and CODEOWNERS (M2), and needs the GitLab and Codeberg adapters for those Forges. Where it lands is part of milestone resequencing.
- Head revisions add one syntactic build per CR push, roughly 3 worker-hours a day at 1,000 pushes, and storage of one tuned Shard per live head.
