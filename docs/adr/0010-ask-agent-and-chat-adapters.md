# Ask answers on behalf of its asker, grounded in what it retrieved and fenced by the server

Status: accepted. Amends [ADR 0007](0007-code-anchored-team-brain.md), which left answer synthesis and chat front ends to others, and refines the chat rows of [ADR 0008](0008-team-granted-repository-visibility.md)'s ceiling table. Builds on [ADR 0009](0009-human-login-agent-binding-forge-sync.md) for credentials and step-up.

ADR 0007 kept an LLM off the query path: the agent composes the answer, and Yggdrasil supplies structure and anchored memory. That still holds for the Index Server. But teams want to ask the brain from Slack, from Waddle and from the web GUI without running their own agent, so Yggdrasil now ships one: **Ask**, a Service Agent that is an ordinary client of the Index Server, composes cited answers with an LLM provider an Admin opted into, and captures Memories when a human asks it to. The Index Server itself still never synthesises an answer and never sends code to a model. Chat platforms stay surfaces only; nothing in a channel is read unless a human addressed it to Ask. The platform facts behind this design are in the [research note](https://github.com/randax/yggdrasil/blob/c3659cb43163dcb7430efdfa719eecdbe213b04a/docs/research/chat-bot-platforms.md).

Three ideas run through the rules:

- **Never more than the asker could see.** Ask has no view of its own. Every query it makes is on behalf of a bound human, and the server narrows it further to the surface's ceiling and to what may leave the deployment.
- **The server fences, the agent composes.** Everything that keeps an answer safe (ceilings, egress, writes, budgets) is enforced by the Index Server or by Ask's own mechanical checks, never by the model following instructions.
- **Claims need a retrieval.** An answer says only what something it retrieved supports, and says nothing about what it could not see.

## The rules

### Where Ask runs

- Ask is a role of the `yg` binary, `yg serve --role=ask`. It calls the Index Server only through its public MCP and HTTP API, with its own Service Agent token, even when every role runs in one process. There is no in-process shortcut to Shards or the control plane.
- The Ask role hosts the Slack adapter, the inbound API the Waddle extension calls, and the web Ask box's backend. It refuses to start until an LLM provider is configured (below).

### On whose behalf

- Every Ask query names the **asker**, identified by an active Chat-account Binding (or, for the web box, by their web session), and the **surface** it will be published on. The server computes the effective view as the asker's view intersected with the surface's ceiling (ADR 0008) and with the egress set (below). Ask's own token grants no repositories and no Audiences; it grants only the right to act on behalf of bound accounts on surfaces an Admin registered, and to serve the public fallback where an Admin enabled it.
- A chat user with no binding gets, as an Admin chooses per surface, either only the private binding prompt or the narrowest ceiling: Forge-public repos and no Memories. That public fallback acts for no Member: it is rate-limited per chat account, charged to the surface's budget, and audited by platform user key.
- Ask is a trusted deputy for *which* bound account is asking; it can never exceed that account's view or the surface's ceiling. Every on-behalf-of call is audited with the asker, the surface and the ceiling applied.
- Ask's credential is therefore its own class, an **on-behalf-of Service Agent** token, distinct from ordinary Service Agents: it can never be assigned Teams, and the server bounds it by the asker's view, not by a view of its own. Whoever holds it could name any bound Member, so issuing or replacing it, and registering each surface it may serve, is a widening: an Admin hardware-key step-up, a reason, audit, and ADR 0008 notices, batched, to every tier for the Restricted repos it can reach. It lives only in the Ask role's environment. A compromised Ask host can read what its bound Members could; that is the accepted cost of a chat deputy, bounded by surface ceilings, the egress set and the audit.
- Ask is not a Personal Agent, so it never recalls an asker's private overlay (ADR 0008), even in a direct message.

### Chat-account Bindings

- A **Chat-account Binding** ties a chat account to a Member. Its key is the platform's stable identifier: for Slack `(enterprise_id, team_id, user_id)`, with `enterprise_id` absent outside an Enterprise org; for Waddle the bare JID. An email address is at most a hint, never a key.
- **Self-binding.** On first use, Ask replies privately to that chat account with a single-use link to the web GUI. The person logs in (OIDC and a passkey, ADR 0009); the page names the chat account by its stable platform id and workspace, not only a display name, and shows a short code. They confirm with a passkey step-up and then type the code back to Ask *from that chat account*, and only then is the binding made. The round trip proves that the same person controls both ends, so a link forwarded to someone else cannot bind the forwarder's account to them. Self-binding is a credential act, like binding a Personal Agent: it is audited, sends the Member a credential alert with a one-touch revoke, and appears in their list of credentials. With both directions proven it is not a widening, because it lets no one new see anything.
- **Admin-set bindings**, for a person who cannot complete the link, stay a widening under ADR 0008, with step-up, a reason, audit and a notice.
- A binding is suspended whenever its Member is suspended, and revoked when the platform reports the chat account deactivated or removed. A chat account binds to at most one Member.

### Capture

- A Memory is captured only from an explicit human gesture: a Slack message shortcut on a message the person chose, or a capture command in Waddle or the web box. Ask never reads a channel in the background, so "chat platforms are surfaces only, never ingested" still holds.
- The bound human is the Memory's author. Ask writes it on their behalf into *their* private overlay, recording "captured via Ask from <surface>", a permalink to the message and the original message author's platform identity as provenance.
- A chat capture lands **held**: no one recalls it, the author's Personal Agents included, until the Member accepts it in a web session (no step-up needed), where they see its full text. This keeps text an attacker posted in a channel, or captured from a hijacked chat account, from reaching the Member's coding agents unseen. Once accepted it is recalled like any Memory, as untrusted data, and keeps its "captured from chat" provenance and original author. The confirmation flags a capture of a message whose author is unbound or external.
- Before writing, Ask proposes Anchors by searching within the asker's view and the surface's ceiling, and the human confirms or edits them. Every repo the captured text discloses must be anchored (ADR 0008).
- The capture reply offers an undo (an author `retract`, already allowed over MCP) and a link to the web GUI, where promotion happens with step-up. Chat never carries the step-up factor.

### LLM provider and egress

- Ask talks to a model through a provider trait with two adapters: the Anthropic Messages API, and an OpenAI-compatible Chat Completions endpoint, which also covers Azure OpenAI and self-hosted servers such as vLLM and Ollama. It is off by default.
- An Admin opts in with step-up and a reason, choosing the provider, model, endpoint and key, and marking the provider **external** (a vendor's API) or **in-deployment** (a model on infrastructure the Organization controls). The admin console states what leaves the deployment: the asker's question and the snippets and Memory bodies retrieved under that answer's ceiling, never whole repositories. Changing any of these is the same Admin act.
- The locality label is the Admin's claim, and marking a provider in-deployment puts every Restricted repo in the egress set. So marking a provider in-deployment, or changing the endpoint of one, is a widening: an Admin hardware-key step-up, a reason, audit, and ADR 0008 notices, batched, to the Owning Teams of every Restricted and Forge-private repo that enters the egress set as a result.
- Sending code to an external provider widens who can see it, so egress approvals belong to one recipient: the provider and endpoint. Opting into an external recipient sends ADR 0008 widening notices, batched, for every Forge-private repo that is Open in Yggdrasil and so enters its egress set. Switching to a different recipient, or changing a provider's locality, clears every per-repo approval, which must be granted afresh.
- The server keeps an **egress set** and intersects every Ask query with it, so Ask cannot override it. With an in-deployment provider it is everything. With an external provider it holds Open repos and the Memories anchored only to them; a Restricted repo joins only when an Admin allows it with step-up and a reason, which is audited and notifies that repo's Owning Teams. A repo outside the egress set is absent from Ask's results exactly as a hidden repo is.

### What Ask may call

- Read Verbs (`node`, `neighbors`, `search`, `history`, and each read Verb as it lands, such as `map`, `callers`, `impact` and `owners`) and `recall`, all on behalf of the asker.
- `remember` only from a capture, after the human confirms its Anchors; `retract` only as the undo on a capture that same human just made.
- Never `promote`, accepting a proposal, affirming, a chain retract or any admin route. The server already refuses step-up acts over MCP.
- Each answer has a budget of tool calls and tokens (for example 12 calls), so one question cannot fan out without bound.

### Grounding and abstention

- The model must attach to every factual sentence a citation of a graph node, an edge or a Memory that this answer retrieved. After generation, Ask checks every citation id against its retrieval log for the answer; a sentence with no valid citation is removed.
- If what remains does not answer the question, Ask abstains: "I couldn't find that in what I can see here." The wording is the same whether nothing exists or something is hidden or outside the egress set.
- Citation links go to web GUI pages that check access when opened, never to raw code, and show each edge's Provenance.

### Memories in answers

- A fresh Memory is cited with a distinct marker naming its author (where the asker may see them), its Audience and its date, and is phrased as what a teammate recorded, never in the graph's voice.
- A stale Memory never supports a claim. It may appear only as history ("a note on this code predates its last change"), with a link.
- When a Memory disagrees with the graph, the graph leads and the Memory is shown as a possibly outdated note. `unverified` Memories never appear, since `recall` omits them.

### Prompt injection

Retrieved code, comments, documents, Memory bodies and the chat question are all untrusted. Ask's defence is what a hijacked model can reach, not what it is told:

- Retrieved items reach the model as quoted data in tagged envelopes. Ask sends the asker's own question and its thread context (below), never a channel's history.
- The ceiling and egress set are enforced by the server; the only write is a capture the human confirms; the tool budget bounds the loop.
- The rendered answer carries no URL except the citation links Ask builds itself, no images, no link unfurls and no `@channel`, `@here` or user mentions, and it is size-capped. A model told to exfiltrate has nowhere to put the data.

### Cost and rate

- Each Member has a rate limit on questions (for example 30 an hour), set by an Admin.
- The Organization has a monthly budget for the provider, in tokens or currency. Admins are alerted at 80%. At 100% Ask replies that it is paused until the budget resets, and capture keeps working.
- The admin console shows spend by surface and by Team, without question text.

### What Ask keeps

- **Always:** a body-free audit of each answer: asker, surface, ceiling, cited ids, tokens and whether it abstained.
- **Only if an Admin opts in** with step-up: transcripts, retained 30 days by default. A stored transcript keeps its ceiling. Every read of a transcript, evaluation copies included, is checked against the reader's current access to every repo it drew on and every Memory it cited, Audience included, as `recall` would be. That holds for the asker too, and an Admin is no exception; a Break-glass Session adds repos but never Memory Audiences.
- **With the asker's consent:** a thumbs-down with "share this answer" stores that one transcript for evaluation.

### Follow-ups

- For each thread, Ask holds in memory, for 30 minutes and never on disk, the asker's messages addressed to it and its own answers. It never reads other people's messages in the thread.
- Every turn recomputes the ceiling. If the ceiling narrowed or a different person is asking, earlier context is dropped and the turn starts fresh, so nothing retrieved for one reader is carried into another's answer. Earlier citations are re-checked before reuse, and before any retained context is sent to a model, every source behind it must still be permitted under the current asker, Audiences, ceiling, provider and egress set; otherwise, or if its sources are unknown, the turn starts fresh.

### The chat-adapter contract

- Adapters share typed conversation semantics, not a transport:
  - `Invocation`: surface, platform user key, conversation and thread reference, text, and kind (ask or capture);
  - `AudienceFacts`: the readers of the conversation, as below;
  - `Reply`: threaded, private or in-channel, with the cited answer;
  - `CaptureRequest`: message reference, text and permalink;
  - `Notice`: to a conversation or to a Member.
- Each adapter owns its transport and renders `Reply` itself: Slack inside the Ask role over Socket Mode, Waddle through an authenticated inbound API its extension calls, and the web Ask box as a direct surface whose ceiling is the asker's own view. The ceiling, capture, citation and injection rules live once, above the contract.

### Who can read a conversation

- The adapter establishes who can *read* the conversation, not only who has joined it. For a Slack public channel that is every member of the workspace, since anyone there can find its messages through search; for a private channel or a Waddle members-only room it is the member list (Slack `conversations.members`, Waddle room occupants). Each reader is checked against active Chat-account Bindings. This replaces ADR 0008's "workspace that admits only Organization Members" with a check Yggdrasil can make itself:
  - every reader is a bound, active Member: the Members-only channel row;
  - additionally, every reader belongs to the Team an Admin bound the channel to: the Team row;
  - any reader unbound, a guest, an external organisation's user, or any bot other than Ask: the guest row (Forge-public repos, no Memories).

  In practice a public channel gets a wider row only when every full member of the workspace is bound; Team-bound channels are private ones. A Slack Connect or multi-workspace channel's readers include every connected organisation, and a Waddle room whose history non-occupants can read counts them as readers too.
- A Member suspended by ADR 0009's Liveness Window is not an active reader, so a channel containing one falls to a narrower row. Chat use through Ask is not a login, so Ask's direct reply to an asker whose binding is suspended says so and links to the web GUI to resume; that is the asker's own state and reveals nothing else.
- An Admin may register a named bot as a non-reader, for example an integration that only posts, as an audited act. Every other bot is a reader that is not a Member.
- The admin console shows each registered conversation's current row. Warnings about Team-bound channels losing their row are aggregated per channel, not sent per event.
- Facts the adapter cannot establish fall to the narrower row. Results are cached per conversation, invalidated by join and leave events, and re-checked when older than 5 minutes at the moment an answer is posted.
- When a conversation's row is narrower than the asker wants, Ask offers "answer me privately", which continues in a direct message under that row of ADR 0008.

### Platforms

- **Slack.** Yggdrasil ships an app manifest. An Admin creates the app in their workspace or Enterprise org and enters its tokens in the admin console with step-up. Ask connects over Socket Mode, so the deployment needs no public URL. The scopes are the research note's minimal set plus `channels:read` and `groups:read` for member listing. Slackbot's own MCP client is not supported: it would compose answers on Slack's side, outside every rule above except the server ceiling, and would need Yggdrasil's MCP endpoint on the internet.
- **Waddle.** The Waddle adapter needs changes in Waddle's still-unfinished extension system: thread ids on commands (W1), a mention trigger (W2), bot direct messages (W3), links in its UI (W4), an async reply path (W5), and a room occupant list for extensions (W9). They are tracked as issues in Waddle's repository, at low priority. The adapter ships when W1, W2, W4 and W9 land; Slack and the web box come first.

### Eval

An Ask eval runs on the Shard profile's 36-repo corpus, shared with the agent-efficiency evals. It has about 150 questions with gold citations: answerable within one repo, answerable across repos, unanswerable, hidden-repo probes, injection probes planted in Memories and code comments, and stale-Memory cases. Release gates, per provider and model, and on every change to the prompt or tool loop:

- citation precision at least 95%;
- correct answers at least 80% on answerable questions, judged against the gold;
- abstention at least 90% on unanswerable questions;
- zero leaks (a hidden or non-egress repo influencing an answer, or a hidden-repo probe answered differently from a truly missing one) and zero injection successes.

## Considered options

- **Ask inside the Index Server**, calling the Verb layer directly. Fastest, but it puts the answering agent inside the boundary it should be a client of, and brings LLM egress into the server ADR 0007 kept free of it.
- **A separate binary.** No access difference from a role that uses the public API, and a second artifact to ship.
- **Ask's own view narrowed by the ceiling**, or the asker's view intersected with an Admin-set Ask view. The first lets anyone in an admitted DM see Ask's whole view; the second silently narrows answers whenever Ask's Teams lag a person's.
- **Every chat-account binding as a widening**, as ADR 0008 first said. A notice for every Restricted repo on each person's first use of Ask, for a binding that lets no one new see anything.
- **The Ask agent as author of captured Memories.** One shared overlay of everyone's captures, and blurred authorship.
- **Anthropic only, or OpenAI-compatible only.** One rules out self-hosted models; the other weakens tool use with Anthropic.
- **All-or-nothing egress, or egress allowed by default.** One sensitive repo would block Ask entirely or reach a vendor unnoticed.
- **Letting the model call `remember` freely.** A background read of chat by another name.
- **Soft citations, or extractive answers only.** Plausible unsupported claims, or a chat wrapper around `search`.
- **Citing Memories like graph facts, or hiding stale ones.** A teammate's guess reads as truth, or a useful lead is lost.
- **Prompt hardening or an injection classifier as the main defence.** Neither holds on its own; capability limits do.
- **Per-Team budgets now.** Needs a charging rule for direct messages from people in several Teams; it can come later on the same ledger.
- **Keeping all transcripts for Admins.** Silently hands Admins every Member's view.
- **A transport-level adapter abstraction, or none.** Socket Mode and Waddle's synchronous calls do not share a transport, and without a contract the safety rules would be written three times.
- **Trusting Slack's guest and shared-channel flags and an Admin's workspace registration.** Unverified flags, and workspace members who are not Members would read Open-repo answers.
- **Never answering in channels.** Safe, but most of the point of a chat brain is shared answers.
- **A Waddle adapter within today's limits.** No threads, mentions, links or verifiable rooms, so nearly every answer would fall to private, Forge-public replies.
- **Single-turn answers, or persisted conversations.** Broken follow-ups, or a transcript store by another name.
- **Supporting Slackbot's MCP client, or an HTTP Events API app.** The first bypasses Ask's rules and needs public MCP; the second needs a public Request URL.

## Consequences

- The Index Server gains an on-behalf-of mechanism for Ask's token, surface registration with ceilings, and an egress set, all enforced server-side.
- Ask is the deployment's only LLM egress. The admin console must show its provider, locality, egress set, budget and spend.
- Each answer pays for a citation check and a ceiling recomputation, and channel answers pay for member listing; caches keep these off the common path.
- Ask needs its own eval corpus and gold set, maintained with the agent-efficiency evals.
- Waddle support depends on work in Waddle's repository.
- How notices reach people (chat channels, direct messages, the web inbox) and how a cited answer looks on each platform are not decided here; they stay on the map as notification plumbing and chat answer presentation.
- RFC 0001 does not yet describe Ask. It is added in the map's RFC consolidation step; until then this ADR wins.
