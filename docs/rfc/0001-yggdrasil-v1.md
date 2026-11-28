# RFC 0001 — Yggdrasil v1 Technical Design

Status: Proposed · Date: 2026-06-10 · Author: Øyvind Randa
Scope: full v1 design (milestones M0–M3). Requirements live in the [PRD](../PRD.md); component overview in [ARCHITECTURE](../ARCHITECTURE.md); vocabulary in [CONTEXT.md](../../CONTEXT.md). Comments welcome on everything; **Open questions** at the end are genuinely open.

## 1. Summary

A Rust workspace producing one binary, `yg`, that is simultaneously the server (`yg serve --role=api|worker|all`), the Admin/Member CLI, and a stdio MCP proxy (`yg mcp`). Stateless API/query nodes and indexing workers around Postgres (control plane) and S3-compatible object storage (immutable per-repo Shards). The v1 contract targets twelve graph Verbs plus four Memory Verbs (ADR 0007) over REST + MCP; M0 currently serves four.

## 2. Control plane schema (Postgres, sketch)

```sql
forges(id, kind /* github|gitlab|codeberg */, base_url, token_ref, write_token_ref?, world_readable bool,
       rate_budget)                                 -- write credential and attestation: ADR 0013
repos(id, forge_id, remote_id, slug, visibility, default_branch, indexed bool,
      discovery_state /* discovered|included|excluded */, last_synced_commit, …)
rules(id, forge_id, pattern, action /* include|exclude */, applies_to_private bool)
jobs(id, kind, repo_id, state, lease_until, attempts, run_after, finished_at)
shards(repo_id, revision, manifest_key /* per upload incarnation */, commit_sha,
       provenance_level /* weakest across Precise Roots */, published_at,
       valid_from_seq, valid_to_seq)                -- pointer history by publish sequence (ADR 0012)
cr_heads(repo_id, cr_number, head_sha, generation, state /* pending|live|not_resident|too_large|closed|ineligible */,
         revision, manifest_key, merge_base, lease_until)          -- per-CR pointer (ADR 0013)
impact_comments(repo_id, cr_number, forge_comment_id, drew_on_repos /* union since last delete */,
                authority_generation, last_visible_change_at)     -- ADR 0013
members(id, name, kind /* human|agent */, can_promote bool, is_admin bool)
tokens(id, member_id, hash, created_at, last_used_at, revoked_at)
packages(id, ecosystem, name)                       -- npm, crates, go modules…
repo_packages(repo_id, package_id, version_range, relation /* provides|depends */)
xrepo_edges(from_repo, to_repo, via_package_id, kind)
xrepo_exports(key /* import binding | version-free moniker */, provider_repo, revision,
              denotes /* symbol_id | forwarded binding */, valid_from_seq, valid_to_seq)
xrepo_refs(key /* import binding | version-free moniker */, from_repo, revision, kind,
           symbols_any, symbols_precise, symbols_syntactic, valid_from_seq, valid_to_seq)
                                                    -- written in the pointer-swap transaction with
                                                    -- repo_packages/xrepo_edges, which carry the same
                                                    -- sequence range; kept for retention (ADR 0012)
contributors(id, display_name)
identities(contributor_id, kind /* email|forge_account */, value, merge_provenance)
memories(id, member_id, kind /* semantic|episodic */, scope /* session|repo|org */,
         session_id, state /* private|proposed|shared|displaced|retracted|erased */,
         unverified bool, body, supersedes, stale_reason, expires_at,
         created_at)                                    -- M2, ADR 0007
anchors(memory_id, node_id, repo_id, forge_id, remote_id, node_kind, path, name, fingerprint)
affirmations(memory_id, actor_member_id, fingerprints, at)      -- latest is the staleness baseline
proposals(id, memory_id, reviewer_member_id, scope,
          expected_head_id /* null = none */, state, created_at)
memory_log_outbox(commit_id, chain_id, entry, sent_at)  -- displacements and reinstatements awaiting the log;
                                                       -- chain_id = id of the chain's root Memory
memory_events(memory_id, actor_member_id, action, at)            -- audit, body-free
-- retractions, erasures, displacements and reinstatements are appended to an object-storage log (§5.1)
```

Memories (§5.1) are the only knowledge in the control plane that cannot be rebuilt from git and the Forges. Staleness is independent of lifecycle: a Memory in any state is stale while `stale_reason` is non-null. A Memory's body and anchors (targets and original fingerprints) are immutable once written. Only its lifecycle state and `stale_reason` change, and affirmations are appended as separate rows; every content change is a new Memory linked through `supersedes`, so a Memory id *is* a version and a supersession chain is its history. Memory ids are random UUIDs generated at write time, never a database sequence, so they are never reused across restore histories; replay ignores an entry whose Memory is absent from the restored database rather than guessing a target.

Job queue = `SELECT … FOR UPDATE SKIP LOCKED` with leases; priorities favor incremental over backfill. No Redis/Kafka in v1 — Postgres is enough at 5000 repos and removing a dependency beats theoretical throughput.

## 3. Sync protocol

- **Discovery loop** (per Forge connection, ~hourly + on demand): list orgs/groups → upsert `repos` with `discovery_state`. Public/internal → `included` unless an exclude rule matches; private → `discovered` until an include rule with `applies_to_private` covers it (ADR 0001). Rule precedence is longest matching glob first, then newest rule for equal-length matches.
- **Poll loop** (default 5 min/repo, jittered, conditional requests): compare branch head via cheap API call; on change enqueue `fetch`. Rate budgeting per Forge token; backoff on 429/abuse signals.
- **Webhook receiver** (optional accelerator): push/CR/issue events → enqueue the same jobs. Secrets per Forge; idempotent with poll (poll is truth, webhooks are latency).
- **Fetch job**: incremental fetch into a persistent, bare, blobless partial clone on the worker (full commit and tree history; blobs fetched on demand for the tree being indexed; `--depth` per-repo override) (ADR 0012). Forge Artifacts synced via API cursors (updated-since), normalized to common shapes (Change Request, Issue, Review). For each open CR targeting the default branch, the head ref (`refs/pull/N/head`, `refs/merge-requests/IID/head`) is fetched into the same mirror, newest push only (ADR 0013).

## 4. Indexing pipeline

Two-pass per change (ADR 0002):

**Pass 1 — syntactic (target: minutes).** tree-sitter parse of every file whose blob misses the per-blob extraction cache (keyed by blob id, language, and extractor, grammar and schema versions; ADR 0012): Symbols, heuristic call/import/extends edges with confidence; markdown → Doc Sections + `MENTIONS` edges (lexical match against symbol names, tagged); CODEOWNERS → `OWNS` edges; `git log <previous>..<new>` delta merged with the previous revision's unfiltered commit/path facts, projected onto the current tree at compose (full walk after a force-push or rewrite) → commits, `TOUCHES`/`AUTHORED` edges; manifest parse → `packages`/`repo_packages`/`xrepo_edges`. Every candidate at an ambiguous call site is stored; calls through imported names are kept as external evidence keyed by import binding, and exported bindings are recorded. Publishes a complete Shard immediately, using the stored precise output of every Precise Root whose input hash still matches. Every job records per-stage timings.

**Pass 2 — precise (target: same hour).** Language detection → SCIP indexer matrix (M1: scip-go, scip-typescript, scip-python, rust-analyzer/scip; M2+: scip-java family, scip-clang, scip-ruby, scip-dotnet). Runs in a sandbox: container, read-only source, egress allowlist (package registries), no credentials, CPU/mem/time limits, per-language dependency caches (e.g. shared module/crate caches) to hit the ~4-min average. Builds run per Precise Root (the unit each ecosystem builds) whenever the current revision has no stored output matching the root's input hash (its files, manifest and lockfile, workspace-level build inputs, sandbox image, indexer version, and in-repo dependency roots' hashes, with cycles hashed as one unit). Raw SCIP output is stored by its own digest and found by the root's input hash; precise evidence is derived from it per schema and ingest version → a compose job publishes a new revision of the desired head in which precise evidence replaces syntactic evidence per call site, in files the root owns and its build indexed. Publication holds a per-repo lease and compare-and-swaps on the base revision and the desired-head generation. Infrastructure failures retry until success; deterministic failures retry daily or on Admin request, the root staying syntactic and surfaced per root in admin status (ADR 0012).

**CR heads.** Each eligible CR head is built as a complete syntactic revision (changed files parsed, the rest from the blob cache, unchanged Precise Roots reusing stored precise output) under a per-CR pointer, never current: no Memory staleness checks, no cross-repo rows. Each CR pointer has its own generation and lease; the poll loop lists open CRs per repo independently of default-branch changes. Head builds queue below default-branch jobs, with CRs by human authors with write access or a linked Member first, bot accounts next and others last; limits are 50 live heads per repo (30 reserved for those human authors, at most 10 for bot accounts), 30 days idle and 5,000 changed files, all Admin-configurable. Each head carries a change set and its own export, reference and package facts. The report's graph result is the `impact` Verb on the head seeded with the change set, deterministic in its inputs, recomputed on push, on narrowing, when the merge-base moves and hourly on drift when a dependency changed; the reporter, a built-in server principal, posts one revocable comment per CR and, on GitHub, an advisory status with fixed content (ADR 0013).

**Cross-repo linking.** A call into another repo is keyed by its import binding (ecosystem, provider-facing import specifier canonicalised through the referencing root's alias tables, imported name path), which both passes derive; only references the precise pass alone can see are keyed by version-free SCIP moniker. Shards resolve external edges per view at read time (ADR 0012). Workers emit `provides`/`depends` package facts, which the control plane resolves to `xrepo_edges` for `deps` and `dependents`, and per-revision `xrepo_exports` and `xrepo_refs` rows, which bound symbol-level cross-repo fan-out (`callers`, `impact`) to the repos that reference the Symbol, filtered to the caller's view and read at one pinned set of revisions (ADR 0012).

**Identity merge.** Same email or .mailmap entry ⇒ merge (provenance `extracted`); same forge account across Forges via profile URLs ⇒ merge (`extracted`); name+similar-email heuristics ⇒ merge (`inferred`, reversible via `identities.merge_provenance`).

**Embeddings (M3).** `embed` jobs are planned to chunk code along Symbol and Doc Section boundaries — the graph gives semantic chunking for free, no line windows — and produce the vector segment for the next Shard revision. The provider API, local model, and inference runtime are deliberately deferred to M3 benchmarking; external API providers remain opt-in. Hybrid lexical+semantic is the deliberate end state: Cursor's semsearch evals report grep+semantic beating either alone (12.5% mean accuracy gain, growing with codebase size), and the effect concentrates at exactly our scale. Scheduled M3 by explicit decision — the core graph proves out first. LLM Concept extraction (M3, opt-in) is planned to follow the selected provider shape for docs → Concept nodes.

## 5. Graph schema

Node kinds: `Repo, File, Symbol, Package, Commit, Contributor, Team, ChangeRequest, Issue, Review, DocSection, Concept`.

Edge kinds (all carry `provenance ∈ {precise, syntactic, extracted, inferred}` + `confidence ∈ [0,1]` + locations where applicable):

```
DEFINES        File → Symbol            CALLS         Symbol → Symbol
REFERENCES     Symbol/File → Symbol     IMPORTS       File → File/Package
EXTENDS        Symbol → Symbol          IMPLEMENTS    Symbol → Symbol
DEPENDS_ON     Repo → Package/Repo      PROVIDES      Repo → Package
TOUCHES        Commit → File            AUTHORED      Contributor → Commit
OWNS           Contributor/Team → File/Dir
MENTIONS       DocSection/Issue/ChangeRequest → Symbol/File
PART_OF        ChangeRequest → Commit   CLOSES        ChangeRequest → Issue
```

Node IDs are stable content-derived strings (`repo:github.com/acme/api`, `sym:<…>`, `doc:<repo>:<path>#<heading-slug>`), so Shard rebuilds don't invalidate references held by agents mid-session. The exact Symbol ID form is settled in M1 under one constraint (ADR 0007): it stays stable across the syntactic and precise passes and across package versions, so a raw, versioned SCIP moniker is an attribute of the Symbol, not its ID. Each node also carries a content fingerprint — the hash of its normalized defining span for a Symbol, the blob hash for a File, the section text hash for a Doc Section (when Doc Sections land in M2). The span and its normalization come from the tree-sitter parse in both passes, so a node's fingerprint is identical across passes for the same blob. Stable Symbol IDs and fingerprints arrive with the M1 schema bump.

### 5.1 Memories (M2, ADR 0007)

Memories are control-plane records, not Shard nodes; derived Verbs never return them. Their trust level is `asserted`, a control-plane type kept separate from the Shard `Provenance` enum (and its `shards.provenance_level` CHECK), which stays four-valued.

- **Anchors.** A Memory has one or more Anchors to a Symbol, File, Repo or (once Doc Sections land) Doc Section. Each Anchor stores the node ID plus kind, path, name, the repo's immutable Forge identity (`forge_id`, `remote_id`) and a fingerprint. Anchorable node references carry the repo's Forge identity, and the writer submits that identity with every Anchor; `remember` refuses an Anchor whose identity does not match the currently admitted repo, for every Anchor kind. The writer also supplies the fingerprint it reviewed; `remember` atomically compares it with the published one and records the new Memory as stale immediately on a mismatch, so a claim about code its author never saw is never presented as current. Commits and Contributors are not anchorable: they never change, so they could never go stale. Repo anchors carry no fingerprint and never go stale; they exist for repo-wide conventions.
- **Staleness.** When a Shard revision is published, the anchors on that repo are compared with the *new* revision only, never with an older Shard that GC may already have deleted. A changed fingerprint sets `stale_reason = changed`; a missing node sets `removed`. This happens in the same Postgres transaction as the `shards` pointer swap, so no reader sees the new code with an unflagged Memory. `stale_reason` also records whether the anchor was precise or syntactic-only when it was checked.
- **Scope and sharing.**
  - Episodic Memory always has `session` scope, and `session` scope is Episodic only. It is bound to a client-supplied `session_id` owned by the writing Member, given an `expires_at`, and never promotable. Its body and search entries are purged within a bounded retention period after expiry (configurable, default 7 days), which releases its quota, and the same cutoff is applied on restore. Every `recall` mode omits it unless the caller supplies the same Member and `session_id` before expiry.
  - `repo` scope requires every anchor to be in that repo.
  - Anchors across repos require `org` scope.
  - A Memory is `private` on write; `promote` moves it to `proposed` or `shared`.
- **Authority.**
  - Only the author may promote, supersede, affirm or retract a private Memory. A `proposed` Memory cannot be affirmed. Every affirmation that would clear staleness, private or shared, uses the same atomic fingerprint check below, so a mismatch leaves the Memory stale through promotion. An inaccessible target is refused with the same error as an unknown one.
  - On a shared Memory, affirming requires the promote right and requires every anchor to resolve in an indexed repo. It atomically checks the fingerprints the actor reviewed against the currently published fingerprints. On a mismatch the Memory stays stale, the attempt is recorded in `memory_events` only, and it must be reviewed against the new revision. On success it appends an `affirmations` row with the actor and those fingerprints, clears staleness and makes those fingerprints the baseline for later staleness checks; the anchor's original fingerprint is kept as evidence of what the claim first described. Without the right, an affirm is recorded in `memory_events` but changes nothing.
  - A superseding Memory may target only the caller's own active Memory or a shared Memory the caller can currently see; an inaccessible target is refused with the same error as an unknown one. The superseding Memory stays private and has no effect on shared recall until it is promoted. For each caller, `recall` returns the newest Memory in each chain branch that the caller can see and that is not retracted, unless the caller asks for history. A private or proposed successor never hides a shared predecessor from other Members; retracting a private or proposed head brings its predecessor back, but a shared Memory displaced by a shared correction moves to `displaced`, which `recall` omits unless `history` is set. Retracting the correction does not bring it back: a Member with the promote right reinstates it to `shared` with `promote`, under the usual publishing checks; a fork returns every head, each marked as superseding the same Memory.
  - Retraction by an author applies only to the named Memory, never to Memories it supersedes, whoever wrote them. Withdrawing a whole supersession chain is Admin-only. Only an Admin may erase. "Admin" in the Authority and Retraction rules means a human Member holding the revocable `is_admin` grant, acting with step-up; every erasure and chain withdrawal records that Member as the actor. Restore-gate and log-recovery operations instead require the bootstrap credential (§8).
  - The promote right is a per-Member grant (`members.can_promote`), defaulting to true for human Members and false for agent Members. An Admin may grant it to an agent Member; that grant is explicit, audited and lets that agent publish without a human gate.
  - During normal operation, each of these requires a human Admin acting with step-up and records the actor, target and change:
    - granting `can_promote` or `is_admin`;
    - creating a human Member (which carries the default promote right) and binding its first factor; bootstrap enrollment is reserved for the first Admin and the restore gate;
    - issuing a token for any existing Member, since a token opens that Member's private overlay;
    - replacing the step-up factor of an Admin or of any Member who holds the promote right;
    - widening private-repo admission (a private include rule or a new Forge connection), since that re-exposes the repo's code and shared Memories to every Member.
  - A Member without the right may still `promote` its own Memory. That opens a proposal with a fresh id, bound to the Memory, the reviewer the author names (who must hold the right), the requested scope and the shared head it expects to displace (possibly none), and marks the Memory `proposed`. The reviewer is shown that expected head only if every repo anchored by both the proposal and the head is currently admitted. Acceptance requires the current shared head to match it exactly and re-checks admission for both atomically; any change, or a hidden head, refuses the transition and needs a fresh proposal. Direct promotion runs the same admission check. While pending, only the author and that reviewer see its body and anchors. Reading or deciding a proposal requires the reviewer's grant to be current at that moment, and revoking a reviewer's promote right cancels every proposal assigned to them, returning each to its author's private overlay. The reviewer accepts or rejects by proposal id; a decision on a proposal that is no longer pending is refused, so an approval can never apply to a later resubmission. Since Memories are immutable, the reviewer always decides on exactly what was proposed, and a superseding Memory needs its own proposal. Accepting moves the Memory into the shared pool. A direct `promote` likewise names the shared head the publisher reviewed (possibly none), and a mismatch needs fresh confirmation. Every transition into shared state — accepting a proposal or promoting directly — also checks atomically that the nearest shared ancestor the Memory would displace is still the current shared head. If another correction won first, the transition is refused and the author must write a fresh correction against the winner, so whoever approves sees what the approval would displace. Rejecting returns it to its author's private overlay. This is how agent-written knowledge, most of what Memories will hold, reaches the team.
  - For a human Member, `promote`, accepting a proposal and affirming a shared Memory also require step-up: the same invocation asks for a factor that is never stored on the agent's machine, and the server then authorises that single act. The factor is a WebAuthn/passkey touch or a TOTP code, enrolled for that Member when an Admin issues the human's token, with the credential bound to that Member. A confirmation prompt alone does not qualify. No step-up credential outlives the act or touches disk. Human publishing acts therefore require the factor on the REST request; the `yg` CLI prompts for it and attaches it. MCP cannot carry the factor, so over MCP these acts, and an Admin's `retract` with `chain` set, are refused — the one deliberate exception to transport parity (ADR 0007). An author's ordinary `retract` keeps parity. This raises the bar for an agent holding a human's ordinary token; it does not make impersonation impossible (PRD Risks).
  - Every transition is written to `memory_events`.
- **Admission.**
  - `remember` rejects anchors to repos that are not indexed.
  - `recall` and `promote` return or accept a Memory only while every repo it anchors to is indexed. Excluding a repo hides its Memories (ADR 0001) without deleting them; re-admitting the repo restores them, stale-checked against its current Shard, but only when the repo's Forge identity still matches the anchor's. A different repository at the same slug is a new repo, and the original Memories stay hidden.
  - A Memory must anchor every repo whose non-public content its body discloses, so excluding that repo hides it. Promoting a Memory attests that its content is fit for org-wide visibility. This is a recorded obligation, not something the server can verify.
- **Retraction and erasure.**
  - Retraction immediately suppresses the retracted Memory from every recall mode; an Admin chain withdrawal suppresses every Memory in the chain.
  - Admin erasure (`yg admin memory erase`) deletes the body and its search index entries but keeps the body-free audit rows.
  - Each retraction and erasure, each displacement of a shared Memory by a shared correction (recording the displaced Memory's id), and each reinstatement of a displaced Memory, is appended to a sequence-numbered, append-only log under a dedicated object-storage prefix, object-locked (WORM) where the store supports it. Each entry claims the next sequence with a create-only conditional put and retries on conflict, so concurrent API nodes never collide. Ordering depends on which way a failure errs. Retractions and erasures append first and update Postgres after, so a half-finished one errs towards suppression. Displacements and reinstatements are decided in one Postgres transaction that checks authorisation, proposal state and the current head together; that commit is the authoritative point. The same transaction writes a `memory_log_outbox` row; any API node appends pending outbox rows to the log, keyed by commit id, and marks them sent, so a crash between commit and append loses nothing. The commit id is a random UUID generated in the decision transaction, never a Postgres sequence or transaction id, so it cannot repeat after a restore. Applying an entry is idempotent on commit id, so a decision already in Postgres is never re-applied. Within a supersession chain, log order equals commit order: a decision on a chain is refused (retryably) while that chain has an unsent outbox row, a durable condition that survives a crash, and appenders send each chain's outbox rows in commit order, so a later decision can neither commit nor log ahead of it. A refused or uncommitted decision therefore never reaches the log, and replay applies only committed decisions, in sequence order. Every log-writing operation is acknowledged only when both writes are done. Every API node continuously applies log entries past the recorded sequence, not only at boot. That prefix is excluded from Shard GC, bucket rebuilds and restores; it is the one object-storage item that is neither rebuildable nor ever rolled back.
  - Postgres records the highest *contiguous* log sequence whose effects it holds durably, so a later entry applied out of order never lets an earlier one be skipped. After a restore, `recall` stays closed until every entry through the log head has been applied. On boot, the API refuses `recall` while the log holds entries past that number and replays them first, across every affected Memory in the chain and every search entry. A Postgres snapshot from before a retraction or erasure therefore cannot republish the body.
  - Any logged transition (retraction, erasure, displacement, reinstatement) is acknowledged only once its log entry is durable in the log's replicated store. If the log itself has to be recovered from a backup, its completeness cannot be proven from the restored data alone, and that covers authority revisions too. Every Member-facing route then closes, graph Verbs included. Before graph access resumes, the bootstrap recovery must either establish current authority from independent evidence, or revoke and reissue every credential whose status cannot be established. Reissue never carries an uncertain grant forward: Members whose continued access cannot be established stay disabled, and uncertain `can_promote` or `is_admin` grants are cleared before any replacement token is issued. In both cases it also re-verifies every repo's admission from the Forge, and a repo whose admission is uncertain stays hidden, with its anchored Memories, until verified. Memory Verbs stay disabled until an Admin also either proves completeness from an independent replica of the log, or discards every restored Memory body and search entry whose withdrawal status cannot be established. In the second case it also starts a new log generation, outside Postgres. The replay cursor is bound to that generation, and its baseline is set before any post-recovery snapshot is taken or Memory Verbs reopen, so sequence numbers never repeat across generations. Any later restore of a snapshot from an earlier generation must repeat the discard before reopening. Every restored Memory that the snapshot shows as shared, but whose current shared, non-displaced status cannot be proven, gets `unverified` set. It is omitted from every ordinary `recall` mode, `history` included, even after the gate reopens. Members with the promote right reach it only through `recall` with `unverified: true`, which returns the body as untrusted data with its Anchors and prior sharing evidence, and only while every anchored repo is admitted, so they can re-affirm it. Restored private drafts and cancelled proposals stay author-only and are never marked unverified. A Member with the promote right clears it by affirming it, or by `promote` on the unverified shared Memory, under the usual publishing checks. Every transition out of `unverified`, affirmation included, also checks atomically the shared head the publisher expects; an omitted or mismatched `expected_head` leaves the Memory unverified. Body-free audit rows are kept.
  - **Restore gate (from M2).** After any Postgres restore, a snapshot can predate an exclusion, a token revocation or a rejection, so:
    - a restore is declared and detected. The documented restore procedure first bumps a deployment generation stored under the log's dedicated object-storage prefix (never rolled back), and an API node refuses Member traffic while the generation Postgres last confirmed differs, so the gate closes even when no revision moved. As a backstop for undeclared restores, every authority change (token issue or revoke, grant, admission rule, authenticator enrollment), every change in a repo's effective admission (including one caused only by a Forge visibility change found by discovery), and every proposal decision or cancellation, commits in Postgres together with a `memory_log_outbox` row and is appended to the object-storage log as an authority-revision entry, like a displacement. Each API node compares the revision Postgres holds with the log's at boot, after any reconnection or failover to a database, before the first Member request on that connection, and again on every pass of its continuous log applier, never per request. On reconnection or failover it also compares the database's contiguous replay position with the log head, and keeps every `recall` mode closed until replay reaches it. The gate closes only when Postgres is *behind* the log, or the revision is missing; a Postgres revision that is ahead only because an outbox row is still unsent is pending, not a restore;
    - while the gate is closed, every Member-facing request, graph Verbs included, stays disabled until an Admin runs `yg admin confirm-restore` to confirm that the current repo admission rules, Member grants and revocations, and step-up authenticator enrollments and revocations are in force; a Member whose current enrollment cannot be established must enrol afresh before publishing again;
    - `confirm-restore` accepts only the bootstrap Admin token from `YG_BOOTSTRAP_TOKEN`, which is held in memory and never stored in Postgres, so a rollback cannot restore the authority to lift the gate;
    - while the gate is closed, only the bootstrap credential may confirm the restore, issue or revoke tokens, change grants, change admission rules, or enrol or revoke step-up authenticators, and it may do nothing else; erasure and chain withdrawal are refused under every credential;
    - replay re-applies logged displacements and reinstatements in order, even when the restored database has no row for the correction;
    - every restored pending proposal is cancelled and returned to its author's private overlay;
    - each restored repo, and every Memory anchored to it, stays hidden until its current Forge identity and visibility have been re-fetched and its admission re-evaluated (ADR 0001), since a snapshot can show a since-privatised repo as public; a repo whose Forge cannot be reached stays hidden;
    - reopening is atomic against the exact admission and grant state the Admin verified, and `yg admin status` reports each step.
  - Deployments still bound backup retention, since old snapshots hold the erased bodies.
- **Untrusted content.** `recall` returns bodies as data marked untrusted. The Skill instructs agents to treat a Memory as a claim to verify against derived Verbs, never as an instruction, and the eval suite includes a promoted Memory containing an injected instruction.
- **Abuse limits.** Writes are rejected when a secret scanner flags the body. Per-Member write-rate and storage quotas count superseded Memories too, and body size, anchor count and `recall` result size are bounded.
- **Telemetry.** Memory Verbs contribute only operational metadata to logs, metrics, traces and any training input: Verb name, outcome, latency and counts. Bodies, free-text `recall` queries and step-up factors are never captured, including for rejected requests; a secret-scanner rejection logs the rule id, never the matched text.
- **Recall search.** Full-text `recall` runs on Postgres full-text search over Memory bodies and ranks separately. It is never merged into `search`'s tantivy BM25 ranking.

## 6. Shard format

```
shards/<repo-id>/<revision>/<incarnation>/
  manifest.json      # commit, pass level, schema version, size + sha256 of every file,
                     # input hash of every Precise Root, embedding model + version
  graph.sqlite       # tuned: integer ids, interned strings, clustered WITHOUT ROWID edges
  fts/…              # tantivy files, merged to one segment; bodies indexed, not stored
  contents/…         # zstd file contents + line offsets: the only copy of source text
  vectors/…          # int8 vectors + mmap ANN graph (when semantic lands; library open, §11 Q2)
  trigram/…          # reserved for regex search after v1
```

Immutable; loose files, no archive; pointer swap in `shards` table; GC deletes one upload incarnation at a time after the retention window. Query nodes download only the files a Verb needs, as parallel byte-range GETs verified by sha256, into an NVMe cache keyed by sha256 that keeps one copy. A publisher holds a per-repo lease, lets holding nodes pre-warm the new revision, then swaps the pointer for every node at once; requests open exactly the revisions their Postgres snapshot names. SQLite survives ADR 0004's supersession as the *graph segment format* — single-file, mmap-friendly, queryable in-process — while object storage + Postgres carry the multi-node story (ADR 0005). Layout, fetch and incremental indexing: ADR 0012.

## 7. Verb contract (ADR 0003)

The v1 contract targets twelve graph Verbs and four Memory Verbs (ADR 0007, §5.1). For the implemented rows, the shared
typed request definitions drive REST (`POST /v1/verbs/<name>`), MCP tools
(`<name>`), and CLI subcommands; planned rows describe milestone intent, not
available endpoints. The served set is defined by `yg_verbs::Verb::ALL` and
`VERB_TOOLS`. Sketch:

| Verb | Status | In | Out |
|---|---|---|---|
| `search` | implemented | query, kinds?, repos?, mode: lexical (semantic/hybrid planned-M3) | ranked node refs + snippets |
| `map` | planned-M1 | repo | entry points, landmark Symbols (centrality), doc index, owners, stats |
| `node` | implemented | id | full node + edge summary |
| `neighbors` | implemented | id, edge_kinds?, direction?, depth≤3 | subgraph |
| `path` | planned-M2 | from, to, max_len | shortest path(s) with edge provenance |
| `callers` | planned-M1 | symbol id, transitive? | call sites with locations |
| `callees` | planned-M1 | symbol id, transitive? | call sites with locations |
| `deps` | planned-M2 | repo\|package\|symbol | dependency closure (xrepo-bounded) |
| `dependents` | planned-M2 | repo\|package\|symbol | dependency closure (xrepo-bounded) |
| `owners` | planned-M2 | file\|dir\|symbol | Contributors/Teams + ownership basis |
| `history` | implemented | file or symbol, since? | commits touching the file or symbol's defining file, Contributors |
| `impact` | planned-M1 | symbol\|file\|CR change set (`{repo, cr}`), depth | affected Symbols→Files→Repos, grouped, provenance-annotated |
| `remember` | planned-M2 | op: create\|supersede\|affirm, body?, kind, scope, session_id?, anchors, target?, expected_head? (required when affirming an unverified Memory; null = no verified head) | Memory id (private to the author unless affirming) |
| `recall` | planned-M2 | anchor id \| scope \| query, session_id?, include_stale?, history?, proposed?, unverified? | Memories with author, scope, lifecycle state, supersession links (only where the caller may see both ends; hidden links are omitted without placeholder or count), `asserted` trust, staleness; bodies marked untrusted. `proposed: true` lists pending proposals awaiting the caller's review, each with its proposal id, Memory, requested scope and expected head; `unverified: true` lists only unverified Memories, for callers with the promote right, so they can be re-affirmed |
| `promote` | planned-M2 | memory id, scope?: repo\|org, expected_head (null = none), reviewer? — or proposal id, decision: accept\|reject | receipt: Memory id, proposal id if any, resulting state and scope (never the body; bodies are read only through `recall`); attributed and audited |
| `retract` | planned-M2 | memory id, chain?: bool (Admin only) | Memory suppressed from every recall mode |

Lexical search ranks with BM25 inside each repository, then divides every
score by that repository's maximum score before the cross-repository merge.
This maps each repository's best hit to `1.0`, preserves its internal score
ratios, and avoids comparing corpus-dependent raw BM25 scales. The final merge
is deterministic: normalized score descending, then repository and node id
ascending.

Graph Verbs accept `revision: {repo, cr: <n>}` to read a CR's head revision of that repo, after the visibility check (`head_unavailable` names why a head cannot be read); cross-repo hops from a head read the other repos' current revisions at the same publish sequence. Memory Verbs read only default-branch revisions, and `remember` refuses a `cr:` anchor (ADR 0013).

Verb additions are minor-version events; removals are major. The Skill is versioned against this table.

## 8. API, auth, clients

- REST + MCP Streamable HTTP on the same port; bearer tokens (`Authorization: Bearer ygt_…`), per-Member, hashed at rest, revocable. OIDC post-v1.
- Operational surface (non-Verb): `GET /healthz` — unauthenticated readiness, a bare `ok`/`error` per dependency (control plane, object storage; failure detail is logged server-side, never returned to anonymous callers); `GET /v1/status` — version and indexed-repo count in the body, uptime in the `x-yggdrasil-uptime-seconds` header (bodies are cache-stable; volatile values ride in headers), behind auth (any valid token, including Member), surfaced as `yg status [--json]`. Every route except `/healthz` requires a token, including unmatched paths. Default listen address `127.0.0.1:7311`. Until Admin-issued Member tokens land (`members`/`tokens`, §2), the server boots from a bootstrap Admin token in `YG_BOOTSTRAP_TOKEN` — held in memory, never stored; the `ygt_` prefix is convention, not enforced. After Member tokens land, it remains as a break-glass Admin credential: normal Verbs refuse it, it can only run admin bootstrap and, while the restore gate is closed, `confirm-restore` and the authority-changing operations listed in §5.1 (tokens, grants, admission rules and step-up authenticators), which no other credential may perform during that time, and deployments keep it out of Postgres backups and agent environments. The richer `yg admin status` (§4) arrives with the admin surface.
- Web GUI (ADR 0014): `yg-web`, a Next.js server behind `yg`, which reverse-proxies pages and `/api/trpc` to it on one origin and serves ceremonies, `/v1`, MCP and webhooks itself. The BFF calls Rust on an internal-only listener with a GUI-surface credential plus the Member's forwarded `__Host-` session cookie; Rust owns every identity ceremony and session. `yg openapi` emits the OpenAPI document generated from the typed definitions, from which the BFF's TypeScript client is generated.
- `yg mcp` proxies stdio↔HTTP for local-only MCP clients, reading `YG_SERVER`/`YG_TOKEN` from config (`~/.config/yg/config.toml`). From M2 it uses a separately issued agent Member token (`YG_AGENT_TOKEN`), never the human's CLI token, and refuses all four Memory Verbs (`remember`, `recall`, `promote`, `retract`) when none is configured; only graph Verbs fall back to the configured token. The human gate on Memory promotion (§5.1) assumes this separation, which the server cannot verify: an agent that can reach a human's token can act as that human, so deployments keep human tokens out of agent environments.
- CLI: `yg <verb> …` for Members; `yg admin forge add|rules|status`, `yg admin token issue|revoke`, `yg admin memory erase`, `yg admin confirm-restore` (M2) for Admins; `--json` everywhere; human output is the default.
- The Skill: markdown shipped in-repo + `yg skill install` (drops into agent skill dirs). Its M0 content covers when to use the graph vs reading files; the division of truth (yggdrasil answers org-truth at the default branch; the agent's local tools own working-tree truth, including uncommitted edits); `search`-first orientation; provenance trust rules; the four implemented Verbs; and failure etiquette (graph thin ⇒ fall back gracefully). It moves to `map`-first orientation when that planned-M1 Verb exists.

## 9. Crate layout

```
crates/
  yg-cli        # binary: subcommands, serve roles, mcp proxy
  yg-api        # REST + hand-rolled MCP server (axum)
  yg-verbs      # verb engine (pure: control-plane + shard reads)
  yg-shard      # shard read/write, cache tier, formats
  yg-control    # postgres models, job queue
  yg-sync       # forge trait + GitHub/generic-git adapters, polling and sync workers
  yg-index      # syntactic indexing, history extraction, Shard publication and reconciliation
web/            # pnpm workspace (ADR 0014)
  app           # Next.js app (yg-web): pages, tRPC BFF
  api-client    # TypeScript client generated from `yg openapi`
  ui            # shared components, if they outgrow the app
```

Key dependencies present today: axum, sqlx, rusqlite (read path), tantivy,
tree-sitter grammars, and object_store. The workspace uses the system `git`
executable for repository operations and implements its MCP transport directly
over axum and serde_json. There is no MCP framework, Rust git implementation,
inference runtime, or ANN library in the current lockfile.

**Decision — implement the M0 MCP surface directly.** The earlier `rmcp` choice
was dropped. The served MCP subset is a small JSON-RPC/Streamable HTTP surface
whose tool inventory and schemas come from `yg-verbs`, so a direct axum +
serde_json implementation keeps the transport tied to the same typed Verb
catalog without another framework dependency. Cost: yggdrasil owns protocol
conformance, negotiation, framing, and error behavior; MCP conformance tests are
therefore part of the contract rather than delegated to a library.

**Decision — shell out to the system git CLI.** The earlier `gitoxide` choice
was dropped rather than retained as a primary implementation with an escape
hatch. Workers invoke non-interactive, locale-pinned git commands with explicit
deadlines. This reuses Git's mature protocol and repository behavior and keeps
one implementation path. Cost: the deployment image must contain git, process
execution is heavier than in-process calls, and command environment, output,
timeouts, and cleanup are yggdrasil's responsibility.

**Decision — defer inference and ANN dependencies to M3 evidence.** The earlier
`ort`/local-ONNX direction and leading ANN candidate are not current dependency
choices. M0 carries no provider implementation, and M3 will select the provider
API, local model, runtime, and vector index from benchmarks. Cost: semantic
search and Concept extraction remain unavailable until that decision and its
implementation land; benefit: the workspace does not freeze a runtime or
artifact format before the graph and retrieval workload can measure it.

## 10. Alternatives considered

Recorded as ADRs: org-visible vs ACL mirroring (0001); precise+fallback vs tree-sitter-only vs precise-only (0002); Verbs vs Cypher/GQL (0003); embedded single-box vs object-storage-native (0004→0005); plus in-question rejections (Kuzu post-shutdown, everything-in-Postgres, specialist cluster zoo, Go and Rust+Go hybrid, webhook-only sync, generated per-repo skills).

## 11. Open questions

1. SCIP language priority beyond M1's four — Java family next, or C/C++ (hardest, big payoff)?
2. ANN library: usearch vs hnsw_rs vs lance — decide at M3 with benchmarks.
3. Build caching strategy for the precise pass (shared registry caches vs sccache-style vs Nix) — biggest lever on worker fleet size.
4. Monorepo sub-projects: storage is settled as one Shard per repo with Precise Roots inside it (ADR 0012); whether `map` groups giant repos by root stays open.
5. Org-wide search tiering, lexical *and* vector: pure fan-out with early termination vs a small global "top symbols" index in the control plane; vector fan-out across thousands of per-repo ANN segments may want repo-scoping hints or a coarse global tier.
6. ~~CR-head indexing~~ — closed by ADR 0013: syntactic head revisions for every eligible CR, no precise pass on heads.
7. Webhook ingress hardening for internet-exposed deployments (signature schemes differ per Forge).
8. Local default embedding model selection: benchmark open code-retrieval models (and chunking-context variants) at M3, together with the ANN-library decision.

### Closed: Shard schema evolution

A `SCHEMA_VERSION` bump changes the deterministic Shard revision suffix. The
read path refuses older-schema artifacts, and each indexing worker checks at
boot for repositories whose current Shard lacks the new suffix, queues one
index job per repository (without duplicating work already in flight), and
publishes a current-schema Shard before swapping the pointer. Schema evolution
therefore means re-indexing the fleet, not migrating a Shard on read. Precise
evidence is re-derived from stored SCIP output rather than rebuilt in sandboxes,
and cursors issued under the old schema expire at the cutover (ADR 0012).

The cost is intentionally explicit: every schema bump requires one full
syntactic rebuild and new Shard publication for each included repository that
already has a synced commit and current Shard, consuming fleet worker time,
Git-cache and object-store I/O, and convergence time. The current implementation
does not establish a measured duration for that operation. For scale context,
the PRD's future full-org budget is approximately 5000 repositories × 4
worker-minutes, or about 333 worker-hours with the precise pass and cached
dependencies; roughly 40 workers make that planning estimate an overnight
operation. If the fleet-wide rebuild cost becomes unacceptable at scale, lazy
migration on read is the escape hatch to evaluate then, not behavior the current
system claims to provide.

## 12. Rollout

M0 ships the tracer (GitHub + syntactic + the search/node/neighbors/history Verbs + CLI/MCP/Skill v0) and is the architecture's proof; M1 adds the precise pass + callers/callees/impact + `map` + GitLab + stable cross-pass Symbol IDs and per-node fingerprints (the ADR 0007 prerequisite); M2 opens with the Memory slice (Member tokens if not yet landed, `remember`/`recall`/`promote`/`retract`, ADR 0007), then completes the graph layers + Codeberg; M3 adds semantic + webhook accelerators + the 5000-repo certification run (synthetic org, published numbers). Each milestone ends with a dogfooding gate: agents must prefer the graph over grep for the navigation tasks it claims.
