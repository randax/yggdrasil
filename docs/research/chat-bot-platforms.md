# Chat bot platforms: Slack app model and Waddle bot integration

Status: research note · Date: 2026-11-23 · Ticket: #127 (map #121) ·
Yggdrasil baseline: commit `c4bec0c` on `main` · Waddle baseline: local
checkout `~/projects/waddle` at commit `fee238c72`

> Question. What does it take to ship the Ask bot on Slack and on Waddle, and
> what could a shared chat-adapter contract abstract over both? Settled
> premise (map #121): Ask is a shipped agent. It is an MCP client of the Index
> Server and composes cited answers with an LLM the Admin has opted in to.
> Chat platforms are surfaces only. The bot answers when invoked and can
> capture a Memory ("remember this"), but it never ingests channel history.
>
> Sourcing. Slack claims cite Slack's official developer docs (docs.slack.dev)
> and Slack's API Terms, as read on 2026-11-23. Waddle claims cite repository
> paths at the commit above; that repo is the primary source for its own
> behaviour. **Unverified** marks a claim I could not confirm against the
> owning source. **Inference** marks my own reasoning.

## 1. Summary

1. **Slack: use Socket Mode, with one app per customer created from a
   manifest.** Socket Mode makes only outbound WebSocket connections, so a
   self-hosted Yggdrasil behind a firewall needs no public Request URL.
   Socket Mode apps cannot be listed in the Slack Marketplace, so Yggdrasil
   ships a manifest and each customer creates its own app. That fits the
   one-Organization-per-deployment model anyway. A per-customer app is also
   what Slack's API Terms exempt ("created for use only by a single third
   party"), which keeps it clear of the 2025 limits on non-Marketplace apps
   (inference about how Slack classifies such apps; §2.9).
2. **Slack: invocation is a mention, a DM, a message shortcut or a slash
   command, but slash commands do not work in threads.** A threaded follow-up
   has to come from a mention or a DM. The best "remember this" gesture is a
   *message shortcut*. It hands the app the message the user picked, so the
   bot needs no history scope. One catch: an app cannot reply into the
   thread the shortcut came from.
3. **Slack: Block Kit carries cited answers well.** Use `section`/`markdown`
   for the answer, `context` for citations and trust markers, and
   `context_actions` for feedback. Native streaming (`chat.startStream`)
   covers long answers. The main limits are 50 blocks per message, 3,000
   characters per section and 1 message per second per channel.
4. **Waddle has a real bot framework today, but it is a server-side WASM
   extension framework, not an external bot API.** Bots are server-spoken
   identities at `<plugin>@extensions.<domain>/bot`. They are loaded by OCI
   digest from operator config and talk through a typed WIT contract
   (`waddle:extension@3.0.0`). The shipped `ai-chatbot` extension is a close
   template for Ask: `/ai` command, LLM call over outbound HTTPS, private or
   in-channel answers.
5. **Waddle has no external-bot path.** There are no XEP-0114 components, no
   bot accounts, no DMs to bots ("Bots do not accept messages") and no
   mention trigger wired to bots. Slash commands do not carry the thread
   they were typed in. There is no link block in the declarative UI. The
   only async inbound path is an HMAC-signed provider webhook, and that path
   cannot reply to an arbitrary thread. Each of these is a Waddle change.
   §3.8 lists them.
6. **The transports point in opposite directions.** On Slack, Ask dials out
   and receives events. On Waddle, Waddle's extension host calls *Ask* over
   HTTPS, within a 45 s invocation budget. A shared adapter contract should
   abstract the *conversation semantics*: invocation, threaded reply, cited
   answer, identity, capture and notification. It should not abstract the
   transport. Ask needs one small inbound HTTP API that the Waddle extension
   calls, and a Slack adapter that runs inside Ask.
7. **Identity needs an explicit link, keyed per platform.** The key is a Slack
   `(enterprise_id?, team_id, user_id)` or a Waddle bare JID, linked to a
   Member through a web-GUI OIDC login. Email matching is at most a hint.
   This choice also settles who authors a chat-captured Memory, which is an
   open design question for #134 (§4.4).
8. **Audience is a new risk.** An in-channel answer is visible to everyone in
   the channel. That can include Slack guests, Slack Connect external
   organizations, or Waddle room members who are not Yggdrasil Members. Once
   Teams reopen ADR 0001, it can also include Members outside a Team. The
   adapter must report audience facts, and Ask must fall back to a private
   reply when the audience is wider than the answer's visibility.

## 2. Slack

### 2.1 Transport: Events API over HTTP vs. Socket Mode

| | Events API (HTTP) | Socket Mode |
|---|---|---|
| Delivery | Slack POSTs JSON to a public HTTPS Request URL; app must return 2xx "within three seconds" or the attempt fails; retried 3 times (near-immediate, 1 min, 5 min) with `x-slack-retry-num` / `x-slack-retry-reason` headers ([Events API](https://docs.slack.dev/apis/events-api/)) | App calls `apps.connections.open` with an app-level `xapp-` token, gets a WebSocket URL, connects; "replaces the need for Slack to dispatch to a Request URL" ([Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)) |
| Verification | Signed secrets; the `token` field is deprecated ([Events API](https://docs.slack.dev/apis/events-api/)) | None needed per event; the socket is already authenticated ([Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)) |
| Ack | 2xx within 3 s | Echo each event's `envelope_id`; no deadline stated on the Socket Mode page ([Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)). Slash commands and shortcuts still require an ack within 3000 ms ([slash commands](https://docs.slack.dev/interactivity/implementing-slash-commands), [shortcuts](https://docs.slack.dev/interactivity/implementing-shortcuts)) |
| Connections | n/a | "up to 10" concurrent WebSockets; Slack may send any payload to any of them; connections refresh "once every few hours", with a `disconnect` warning about 10 s before ([Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)) |
| Marketplace | Allowed | "Apps using Socket Mode are _not_ currently allowed in the public Slack Marketplace" ([Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode)) |
| Event volume cap | 30,000 deliveries per workspace per app per 60 min, then `app_rate_limited` events each minute ([Events API](https://docs.slack.dev/apis/events-api/)) | Same Events API (inference: the page states the cap for event deliveries in general) |

`apps.connections.open` itself lists "No scopes required" and is Tier 3
([reference](https://docs.slack.dev/reference/methods/apps.connections.open)).
App-level tokens "do not expire by default". Rotation is opt-in and cannot be
turned off once enabled; rotated access tokens expire after 12 hours
([tokens](https://docs.slack.dev/authentication/tokens)). *Unverified:* that
the app-level token must carry the `connections:write` scope. That is
common practice, but neither page I read states it.

**Fit for Yggdrasil (inference).** Socket Mode matches a self-hosted server.
It makes outbound connections only, needs no inbound firewall rule, TLS
certificate or public DNS, and handles per-event verification itself.
Running two or more connections covers rolling restarts. The cost is that
Ask must keep a reconnecting WebSocket client and deduplicate events, because
any connection may receive any payload. Marketplace exclusion costs nothing
here, because a per-customer app is not listed anyway (§2.6).

### 2.2 Invocation surfaces

- **Mention in a channel.** The `app_mention` event needs `app_mentions.read`.
  The app receives it only if it is a member of the conversation or is
  invited by the mention. DMs to the app are *not* sent as `app_mention`; the
  app subscribes to `message.im` for those
  ([app_mention](https://docs.slack.dev/reference/events/app_mention)). The
  reference's example event shows `user`, `text`, `ts`, `channel` and
  `event_ts`. *Unverified on that page:* that `thread_ts` is present when the
  mention is inside a thread. It is needed to reply in-thread and to read the
  parent for "remember this" (§2.3).
- **Direct message.** Subscribe to `message.im`; reading it needs `im:history`
  (the scope list for `conversations.replies` includes `im:history`
  ([reference](https://docs.slack.dev/reference/methods/conversations.replies));
  *unverified:* the exact scope listed on the `message.im` event page).
  A DM thread is the natural place for a private, multi-turn Ask.
- **Slash command** (e.g. `/ask`). The payload carries `user_id`, `team_id`,
  `channel_id`, `enterprise_id`/`enterprise_name` in Enterprise orgs,
  `response_url` and `trigger_id`. The default response is ephemeral;
  `in_channel` shares the response and the command. Custom slash commands
  "cannot, however, be invoked in message threads"
  ([slash commands](https://docs.slack.dev/interactivity/implementing-slash-commands)).
  `response_url` can be used "up to 5 times within 30 minutes"
  ([handling user interaction](https://docs.slack.dev/interactivity/handling-user-interaction)),
  so a slow LLM answer can still go back through it, or through
  `chat.postMessage`.
- **Message shortcut** ("Remember this in Yggdrasil"). It needs the `commands`
  scope and an ack within 3 s. The payload includes the chosen message's
  text, `message_ts` and channel. The app "must follow up with a modal to
  confirm any action". An app answering a shortcut on a threaded message
  "cannot currently publish a message back to that thread"
  ([shortcuts](https://docs.slack.dev/interactivity/implementing-shortcuts)).
  *Inference:* this is the best capture gesture. The user picks exactly one
  message, a modal confirms the Memory text and its Anchors, and no history
  scope is needed. The bot can confirm in the modal or with an ephemeral
  message instead of a thread reply.
- **Agent / assistant container.** Slack offers "a split-view container, top
  navigation entry point, app threads, text streaming, and suggested prompts"
  for AI apps ([AI overview](https://docs.slack.dev/ai/)). In the manifest,
  "New apps can only use `agent_view`"; `assistant_view` is the older form
  ([manifest reference](https://docs.slack.dev/reference/app-manifest)).
  Marketplace guidance says the agent container needs a paid Slack plan, and
  that the app must still work without one
  ([Marketplace guidelines](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements)).
  *Unverified:* the scopes and events for `agent_view`. The quickstart I read
  does not list them ([agent quickstart](https://docs.slack.dev/ai/agent-quickstart)).
  *Inference:* treat the container as an optional extra over mention and DM,
  not a requirement.

### 2.3 Replies and threads

- **Thread replies.** `chat.postMessage` with `thread_ts` set to the
  *parent's* ts ("Avoid using a reply's `ts` value"). `reply_broadcast` should
  be used "sparingly" ([chat.postMessage](https://docs.slack.dev/reference/methods/chat.postMessage)).
- **Fallback text.** When blocks are sent, `text` is the notification fallback
  and what screen readers read. Keep it near 4,000 characters; beyond 40,000
  it is truncated. `markdown_text` (up to 12,000 characters) cannot be
  combined with `blocks` or `text`
  ([chat.postMessage](https://docs.slack.dev/reference/methods/chat.postMessage)).
- **Posting scopes.** `chat:write`. New apps cannot post to every public
  channel without `chat:write.public`. Impersonating a user (custom
  username/icon) needs `chat:write.customize`
  ([chat.postMessage](https://docs.slack.dev/reference/methods/chat.postMessage)).
  Ask should post as itself.
- **Streaming.** `chat.startStream` → `chat.appendStream` → `chat.stopStream`
  (`chat:write`, Tier 2). Top-level streaming works only in channels "where
  the whole channel is one session"; elsewhere a `thread_ts` is needed. A
  stream lasts at most 10 minutes. `chat.stopStream` can attach up to 50
  more blocks, for example the citations block
  ([chat.startStream](https://docs.slack.dev/reference/methods/chat.startStream)).
- **Reading the thread for "remember this" said as a reply.** That needs
  `conversations.replies` and `*:history` scopes (Tier 3, 50+/min as listed)
  ([reference](https://docs.slack.dev/reference/methods/conversations.replies)).
  *Inference:* reading the single parent message at the user's explicit
  request fits "surface, not ingestion". The capture flow should still prefer
  the message shortcut, so the bot needs no history scopes at all.

### 2.4 Block Kit for cited answers

- Limits: up to 50 blocks per message (100 in modals and the Home tab)
  ([blocks](https://docs.slack.dev/reference/block-kit/blocks)). Section text
  up to 3,000 characters; up to 10 `fields`, each up to 2,000
  ([section](https://docs.slack.dev/reference/block-kit/blocks/section-block)).
  A context block holds up to 10 image or text elements
  ([context](https://docs.slack.dev/reference/block-kit/blocks/context-block)).
  `context_actions` holds up to 5 feedback or icon buttons, in messages only
  ([context_actions](https://docs.slack.dev/reference/block-kit/blocks/context-actions-block)).
- The block catalogue now also lists `markdown`, `rich_text`, `table`,
  `data_table`, `card`, `carousel`, `alert`, `container`, `plan` and
  `task_card` ([blocks](https://docs.slack.dev/reference/block-kit/blocks)).
  I did not read per-block limits beyond those quoted above.
- *Inference, a starting layout for the presentation prototype the map
  defers:*
  1. A `section` or `markdown` block with the answer and inline `[n]`
     markers.
  2. One `context` block per citation group. Each holds a file or symbol
     link to the web GUI or forge, with a provenance marker (precise,
     syntactic or asserted) and a Memory trust marker (private, shared or
     stale).
  3. An abstention notice in an `alert` or plain `section` when Ask declines
     to answer.
  4. `context_actions` with feedback, "Remember…" and "Open in Yggdrasil".

  With 50 blocks there is room for about 10–20 citations, so the Ask
  contract should cap citations per answer. It should link out to the web
  GUI for the full list.

### 2.5 Minimal scope set (inference, from the sections above)

| Need | Scope |
|---|---|
| Receive mentions | `app_mentions.read` |
| DMs with the bot | `im:history` (+ `message.im` event); `im:write` to open a DM (*unverified*, not read) |
| Post answers | `chat:write` |
| `/ask`, message shortcut | `commands` |
| Org-ready install | `team:read` (first step to org-readiness, [Enterprise](https://docs.slack.dev/enterprise/developing-for-enterprise-orgs)) |
| Identity hint (optional) | `users:read`, `users:read.email` ([users.info](https://docs.slack.dev/reference/methods/users.info)) |
| Audience check (optional) | `channels:read`/`groups:read` for `conversations.info` (*unverified*, not read) |

Deliberately absent: `channels:history`, `groups:history` and
`mpim:history`. The bot never reads channel history.

### 2.6 Distribution for a self-hosted product

- **Manifests.** An app can be created by pasting a YAML or JSON manifest
  into the app config UI. Slack documents a shareable link:
  `https://api.slack.com/apps?new_app=1&manifest_yaml=<url-encoded>`
  (or `manifest_json`). `apps.manifest.create` does the same through the API
  with a 12-hour app configuration token
  ([manifests](https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests)).
  The relevant fields are `settings.socket_mode_enabled`,
  `settings.org_deploy_enabled`, `settings.event_subscriptions.bot_events`,
  `settings.interactivity.is_enabled`, `features.bot_user`,
  `features.slash_commands`, `oauth_config.scopes.bot` and
  `settings.token_rotation_enabled`
  ([manifest reference](https://docs.slack.dev/reference/app-manifest)).
- **Model (inference).** `yg` generates the manifest, or the admin console
  shows the link. The Admin creates the app in their own workspace or org,
  installs it, and pastes the bot token (`xoxb-`) and app-level token
  (`xapp-`) into Yggdrasil's secret store. No Yggdrasil-operated Slack app
  exists, and no OAuth redirect to a vendor is needed. One app maps to one
  Organization, which matches "one Organization per deployment" (map #121).
- **Terms.** The API Terms (effective 2025-10-10) say that an app used
  outside its own organization must not "use API Data to train a large
  language model" or "bulk export Slack message and file data". Caching must
  be "limited and temporary" and deleted promptly. Commercial-distribution
  restrictions "do not apply if your Application and any products or services
  connected to it were created for use only by a single third party"
  ([API Terms](https://slack.com/terms-of-service/api)). *Inference:* a
  customer-created app is internal to that customer. Even if Slack saw it as
  Yggdrasil's, Ask already obeys the data rules: it does no training, no
  history reads and no persistent message copies, apart from Memories the
  user explicitly asks for.

### 2.7 Enterprise orgs (formerly "Enterprise Grid")

Slack's docs now say "Enterprise organization" or "Enterprise org"
([Enterprise](https://docs.slack.dev/enterprise/)).

- **Org-ready apps.** To install across an org, an app must be org-ready:
  add `team:read` and opt in to Org Level Apps
  (`settings.org_deploy_enabled: true`). An Org Admin reviews the install.
  After an org-wide install, the app is in *no* workspace until the admin
  adds workspaces.
- **Install response and events.** `oauth.v2.access` returns `enterprise
  {id,name}` and `is_enterprise_install`. Events carry `enterprise_id` and an
  `is_enterprise_install` authorization flag. Store `enterprise_id` with the
  workspace data.
- **Single-workspace apps.** An app locked to one workspace "cannot be
  installed across an Enterprise org"
  ([developing for Enterprise orgs](https://docs.slack.dev/enterprise/developing-for-enterprise-orgs)).
- **User IDs.** An Enterprise user ID (`U…` or `W…`) is stable across the
  org's workspaces ([Enterprise](https://docs.slack.dev/enterprise/)).

*Unverified:* whether Socket Mode works with org-ready apps. The pages I read
do not say. *Inference:* key chat identity on `(enterprise_id, user_id)` when
`enterprise_id` is present and on `(team_id, user_id)` otherwise. Install
records must hold several workspaces per Organization.

### 2.8 Rate limits

- **Web API tiers** are per method, per workspace, per minute: Tier 1 is 1+,
  Tier 2 is 20+, Tier 3 is 50+ and Tier 4 is 100+. Some methods have special
  limits. On a 429, wait for `Retry-After`. The wait applies only to that
  method in that workspace
  ([rate limits](https://docs.slack.dev/apis/web-api/rate-limits)).
- **`chat.postMessage`**: about 1 message per second per channel, plus a
  workspace-wide limit. Bursts above this are not guaranteed to be stored
  ([rate limits](https://docs.slack.dev/apis/web-api/rate-limits),
  [chat.postMessage](https://docs.slack.dev/reference/methods/chat.postMessage)).
  Streaming appends are Tier 2
  ([chat.startStream](https://docs.slack.dev/reference/methods/chat.startStream)).
- **Implication (inference).** Send one message per answer, not one per
  citation. Notifications such as stale-Memory digests need a per-channel
  outbound queue.

### 2.9 Marketplace, AI-app policy and 2025–2026 changes

- **2025-05-29.** For commercially distributed apps outside the Marketplace,
  `conversations.history` and `conversations.replies` dropped to 1 request
  per minute and 15 objects per request. Apps created on or after that date
  were affected at once. "Internal customer-built applications are not
  impacted" and keep 50+/min with up to 1,000 objects. Existing installs
  were initially exempt. The updated API Terms applied at once to new apps
  and from 2025-06-30 to older ones
  ([changelog 2025-05-29](https://docs.slack.dev/changelog/2025/05/29/rate-limit-changes-for-non-marketplace-apps),
  [clarification 2025-06-03](https://docs.slack.dev/changelog/2025/06/03/rate-limits-clarity)).
  *Unverified:* the extension of these limits to existing installs on
  2026-03-03. Search snippets attribute that date to a Slack changelog page I
  did not read, and the live method page still lists Tier 3.
  *Inference:* this barely matters to Ask, which avoids history reads.
- **Marketplace AI rules.** Do not "use Slack data to train Large Language
  Models". Disclose the model, retention and residency. Add a disclaimer
  about inaccurate outputs. Keep humans in the loop for consequential
  decisions. Agent apps must show a thread status while working and must not
  store Slack data ("zero-copy"). Apps with broad scopes get enhanced review
  ([Marketplace guidelines](https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements)).
  Ask is not listed, but these are sound defaults for it.
- **2026-06: Slackbot MCP Client.** A Slack app can declare `mcp_servers`
  (`url`, `auth_type` = `no_auth`, `slack_identity_auth`, `manual_auth` or
  `dynamic_client_registration`). Slackbot then calls the server's tools
  over Streamable HTTP and must get a result within 60 s. With
  `slack_identity_auth`, Slack signs the request and passes `user_id`,
  `team_id` and `enterprise_id` in `_meta.slack`. A user can have up to 5
  active servers
  ([Slackbot MCP Client](https://docs.slack.dev/ai/slackbot-mcp-client/);
  dated by its [changelog path](https://docs.slack.dev/changelog/2026/06/18/slackbot-mcp-client/)).
  *Inference:* this is a real alternative surface, but it conflicts with two
  settled points:
  - the Index Server's MCP endpoint would have to be publicly reachable;
  - Slack's LLM, not the Admin-opted-in provider, would compose the answers.

  It also sidesteps Ask's citation and abstention behaviour. It is worth one
  line in the design as an optional mode for deployments that accept those
  costs, not as the Ask path.

### 2.10 Manifest sketch (inference; field names from the manifest reference)

```yaml
display_information: { name: Yggdrasil }
features:
  bot_user: { display_name: yggdrasil }
  slash_commands:
    - { command: /ask, description: Ask Yggdrasil about the code, usage_hint: "how does X work?" }
  # message shortcut "Remember this" lives under features.shortcuts (not read on the reference page; unverified shape)
oauth_config:
  scopes:
    bot: [app_mentions.read, im:history, chat:write, commands, team:read]
settings:
  socket_mode_enabled: true
  org_deploy_enabled: true
  interactivity: { is_enabled: true }
  event_subscriptions:
    bot_events: [app_mention, message.im]
```

## 3. Waddle

### 3.1 What Waddle is, as far as a bot cares

Waddle is the user's own chat product: a Rust XMPP server, native SwiftUI
apps and a web client (`README.md`, `server/`). The server advertises the
user-visible capabilities listed in `server/capabilities.toml`:

- room messaging (XEP-0045, XEP-0359);
- direct messages with carbons (XEP-0280);
- MAM history (XEP-0313);
- threads and replies (XEP-0201 and XEP-0461, "beta");
- reactions (XEP-0444);
- push (XEP-0357);
- file sharing;
- spaces and administration through Ad-Hoc Commands (XEP-0050, XEP-0503).

Federation is "unsupported" for all of these. The server also implements
XEP-0372 References, XEP-0393 Message Styling, XEP-0394 Message Markup,
XEP-0513 Explicit Mentions and XEP-0317 Hats (`docs/xep-conformance-audit.md`;
`server/crates/waddle-xmpp/src/xep/`). User login is OIDC or OAuth2 at the
HTTP layer (`server/crates/waddle-server/src/auth/providers.rs`). XMPP SASL
supports PLAIN (with a session token), SCRAM-SHA-256 and OAUTHBEARER
(`server/crates/waddle-xmpp/src/auth/mod.rs`;
`server/docs/adrs/0015-dual-authentication.md`).

### 3.2 The integration surface today: server-side WASM extensions

Waddle's bot and integration API is the **extension framework**:

- **Runtime.** An extension is a WebAssembly component built against the WIT
  world `waddle:extension@3.0.0` (`server/wit/waddle-extension.wit`). The
  server runs it in-process. Production modules load from OCI by immutable
  digest; dev modules load from a local path. Configuration lives in the
  operator env var `WADDLE_EXTENSIONS_JSON`, and secrets can be mounted from
  files (`configSecretFiles`) (`server/crates/waddle-extensions/src/config.rs`).
  The planned XEP-0050 `admin:install` / `admin:configure` flow in the
  protocol spec has no implementation I could find: only the `invoke` node
  appears in code (grep over `server/crates`). Installing therefore means
  editing config and restarting.
- **Grants.** Default deny. The runtime grants only the intersection of the
  manifest's capabilities and the operator's `capabilityGrants`
  (`config.rs`). The capability enum includes:
  - `commands`, `message-enrich`, `message-observe`, `launch`;
  - `host-message-send`, `host-mam-read`, `host-members-read`,
    `host-presence-read`, `host-roster-read`, `host-channels-read`,
    `host-spaces-read`;
  - `outbound-http-request`, `pubsub-publish`, `ui-declarative`
    (`waddle-extension.wit`, `enum extension-capability`).
- **Events in.** `room-message-observe`, `message-hook` (body, links,
  thread-id and reply-to of an accepted message), `command` (XEP-0050),
  `launch` (a button on an earlier enrichment) and `provider-webhook`
  (`variant extension-event`).
- **Effects and host calls out.**
  - Effects: `enrich-message`, `publish-room-result`, `publish-pubsub`,
    `command-form`, `host-warning`.
  - Host tools: `send-message` (MUC or direct, with `thread-id`, `reply-to`,
    `markup` and an extension envelope), `query-mam`, `list-room-members`,
    `get-presence` and others.
  - Runtime: `http-request` (GET/POST) and `get-config`
    (`interface host-tools`, `interface runtime`).
- **Template.** `server/extensions/ai-chatbot` is a shipped LLM bot. It
  registers the `/ai` command (`composer-prefix: "ai"`, inline field
  `prompt`) and calls an OpenRouter-compatible provider through
  `http-request`. It answers either privately, as an XEP-0050 command result,
  or in the channel ("Post to channel"). Its bot hat label is "Bot"
  (`src/manifest.rs`, `src/command.rs`, `src/constants.rs`). Its system
  prompt already treats tool results as "untrusted data"
  (`src/constants.rs`).
- **Webhook template.** `server/extensions/github` receives provider webhooks
  and posts to configured channels (`src/extension.rs`).

### 3.3 Bot identity and authentication

- **A bot has no account.** "A bot has no account; the server speaks for it."
  Its JID is `<plugin>@<extensions domain>/bot`, with disco identity
  `client/bot` and the server-assigned XEP-0317 Bot hat. It joins a room only
  while posting (`server/crates/waddle-server/src/server/extension_bot.rs`).
  Room bots are listed on the disco node `urn:waddle:room:bots:0`
  (`docs/xep-conformance-audit.md`, XEP-0030 row).
- **Authentication is operator trust.** The digest-pinned artifact plus the
  operator's grants are the credential. Sends are fenced by a durable,
  revocable per-plugin send grant (`extension_grants` table) and per-room
  provider grants, derived from config at startup
  (`docs/superpowers/plans/2026-09-13-extension-ingress-identity-1753.md`,
  D2). Direct sends also require a roster subscription with the requester
  (same plan, "Facts").
- **How Ask would authenticate (inference).** Waddle trusts the extension
  because the operator installed it. Yggdrasil must authenticate the
  *extension's calls to Ask*. The extension's config would carry a
  Yggdrasil credential, mounted as a secret file, and outbound HTTP is
  limited to the operator-listed `allowedHttpOrigins`.
- **XMPP clients are the other option.** SCRAM-SHA-256 or OAUTHBEARER would
  let an *external* process log in as an ordinary user account and act as a
  bot (§3.9, option B). Nothing marks such an account as a bot: there are no
  bot hats, no command registration and no enrichment UI. Waddle has no bot
  or service account type (grep for `bot_account`/`service_account`). The
  historical RFC-0014 bot-account design (`server/docs/rfcs/0014-bot-framework.md`)
  is superseded by the extension spec, according to its own status note.

### 3.4 Invocation: what triggers a bot today

- **Slash commands.** These work. A command descriptor can declare a
  `composer-prefix`, so `/ask <text>` appears in composer autocomplete and
  fills `inline-field` (`waddle-extension.wit`, `record command-descriptor`).
  But `command-invocation` carries only `waddle-id`, `room`, `requester`,
  node, session, action and form fields. It has **no thread-id and no source
  stanza** (`record command-invocation`), so `/ask` typed inside a thread
  cannot answer in that thread.
- **Launch buttons.** These work. A `launch-invocation` carries
  `source-stanza-id` through `launch-context`, so follow-up buttons on an
  answer, such as "Remember this" or "Ask a follow-up", know their message.
- **Mention.** Not wired. The protocol spec lists an XEP-0513 mention of the
  bot JID as a trigger (`docs/superpowers/plans/2026-04-27-waddle-extension-xmpp-protocol.md`,
  "Bot Trigger Rules"). In code, `ai-chatbot` handles `MessageHook` as a
  no-op (`src/command.rs`) and only strips a leading `@waddle` from command
  prompts (`src/prompt.rs`). I found no mention trigger in
  `waddle-extensions` or the extension host adapter (grep for "mention").
  The only way to react to a mention today is `message-observe`. That hands
  the extension *every* accepted message body for which it holds the grant
  (`server/crates/waddle-extensions/src/manager/message_processing.rs`), and
  observers cannot post effects other than `host-warning` (same file).
  *Inference:* watching every message just to spot mentions is ingestion in
  all but storage. Ask should not use it.
- **DM to the bot.** Refused. "Nothing on the extensions domain accepts
  messages: bots are not DM peers" → `service-unavailable`, "Bots do not
  accept messages." (`server/crates/waddle-server/src/server/routes/websocket/handlers/message.rs`).
  The spec's "DM to bot" trigger is unimplemented.
- **Durable room observation.** This is an explicit per-module opt-in
  (`roomObservation`) that processes every accepted room message
  asynchronously (`docs/extensions/room-observations.md`). It is the opposite
  of "never ingests", so Ask must not enable it.

### 3.5 Replies, threads and rich messages

- **Threads.** Threads are RFC 6121 `<thread/>` sub-conversations in a MUC.
  A Waddle thread inbox extends XEP-0430 (`docs/rfc/thread-inbox.md`).
  Replies are XEP-0461. `send-message-request` takes `thread-id` and
  `reply-to`, so a bot *can* answer in a thread. The `/ask` gap is that the
  thread id never arrives (§3.4).
- **Inline markup.** `message-markup-kind` has only `blockquote`
  (`waddle-extension.wit`). XEP-0394 is implemented server-side, but bots
  get only that one span kind. XEP-0372 references (for example linking a
  symbol name to a URL) are not exposed to extensions.
- **Rich answers are framework enrichments.** An `<extensions
  xmlns='urn:waddle:extension:1'>` envelope holds `ui-view`s made of `text`
  blocks (styles `body`, `heading`, `muted`, `code`), `image`, `action`
  (launch button), `form` (XEP-0004) and `list-block`. List items have
  label, description, image and launch-id (`waddle-extension.wit`). Clients
  render only server-validated declarative descriptors: no HTML, JS, CSS or
  iframes (`docs/extensions/sample-plugin-contracts.md`, "Shared Contract").
  There is **no hyperlink block or field**. A citation can be a `list-item`
  whose launch opens a detail form, or a bare URL in body text. *Unverified:*
  whether clients autolink bare URLs in bot bodies. Link previews exist
  (`xep_waddle_link_preview.rs`), but I did not trace client autolinking.
- **Private vs. public answers.** A command can return a `command-form`
  result to the requester only, which plays the role of Slack's ephemeral
  reply. Alternatively it can `send-message` to the room
  (`ai-chatbot/src/command.rs`).

### 3.6 Calling Yggdrasil from a Waddle extension

The limits come from `server/crates/waddle-extensions/src/config/limits.rs`
and `runtime/http.rs`:

- **Invocation budget.** 45 s per command (`EXTENSION_COMMAND_TIMEOUT`,
  `manager.rs`). Each HTTP request is bounded by `http_timeout_ms`, default
  30 s, and there are at most 32 requests per invocation.
- **HTTP shape.** HTTPS only, with no redirects. The origin must appear in
  `allowedHttpOrigins`. Requests are up to 256 KiB and responses up to 1 MiB
  (configurable to 4 MiB). Bodies must be UTF-8.
- **Response headers are not exposed.** `http-response` is `{status, body}`.
  *Inference:* an extension therefore cannot hold an MCP Streamable-HTTP
  session, which needs the `Mcp-Session-Id` response header, and it could
  only buffer an SSE body.

*Inference:* the extension should not be an MCP client of the Index Server.
It should make one bounded request to Ask, such as `POST /ask` returning a
typed answer, and render the result. Ask stays the only MCP client. A long
Ask run can exceed 45 s. With no async callback into a thread (§3.7), the
first version needs an Ask-side deadline (for example 35 s) and an honest
"took too long — open in web" fallback.

### 3.7 Inbound notifications into Waddle

`POST /webhooks/providers/{provider_id}/{plugin_id}` accepts HMAC-SHA256
signed payloads. The secret, event header, delivery header and plugin come
from `WADDLE_PROVIDER_<ID>_WEBHOOK_*` env vars. The payload reaches the
plugin as a typed `provider-webhook` event, and the route keeps a delivery
ledger for duplicates
(`server/crates/waddle-server/src/server/routes/extension_webhooks.rs`).
Provider webhooks have no requester. They may post only to the module's
`providerRoomGrants` rooms, and direct sends are denied
(`config.rs`; ingress-identity plan, "Facts").

*Inference:* this fits Team notifications such as "3 Memories went stale in
`#payments`" posted to a fixed, operator-granted room. It does not fit
delivering a late Ask answer into an arbitrary thread or DM.

### 3.8 Gaps: what Waddle would need to add

| # | Gap | Evidence | Needed for |
|---|---|---|---|
| W1 | `command-invocation` lacks `thread-id` and source stanza | WIT `record command-invocation` | `/ask` in a thread |
| W2 | No mention trigger for bots (XEP-0513 → targeted `message-hook` for that bot only) | §3.4 | `@yggdrasil …` without observing everything |
| W3 | Bots refuse DMs | `handlers/message.rs` | private multi-turn Ask |
| W4 | No link in declarative UI (`text-block`/`list-item` have no URL), only `blockquote` markup, no references | WIT | clickable citations |
| W5 | No async reply path to a requester or thread after the invocation returns (provider webhooks are room-granted, requester-less) | §3.7 | answers longer than 45 s, streaming-like progress |
| W6 | `http-response` hides headers | WIT `record http-response` | only if the extension ever speaks MCP directly |
| W7 | Requester is a JID only; the user's OIDC `(iss, sub)` is not exposed to extensions | WIT `record command-invocation`; OIDC claims stored server-side (`routes/auth/implementation.rs`) | automatic identity mapping when Waddle and Yggdrasil share an IdP |
| W8 | Install/configure is env config plus restart; `admin:install` unimplemented | §3.2 | Admin UX parity with Slack's "create from manifest" |

W1, W2 and W4 are the minimum for a credible Ask in Waddle. W3 and W5 make
it pleasant.

### 3.9 Ways to build the Waddle bot

- **A. Yggdrasil WASM extension (recommended; inference).** A Rust
  `yggdrasil-ask` component modelled on `ai-chatbot`.
  - Capabilities: `commands`, `launch`, `ui-declarative`,
    `outbound-http-request`, `host-message-send` and, after W2,
    mention-targeted hooks.
  - Behaviour: it calls Ask over HTTPS, renders enrichment citations, and
    handles "Remember" as a launch on the answer or on any message.
  - Pros: native bot identity and hat, composer autocomplete, Waddle-rendered
    UI, and no new protocol.
  - Cons: written against Waddle's WIT (version churn: the world is at
    `@3.0.0`). Waddle must reach Ask over HTTPS. It needs W1, W2 and W4.
- **B. External XMPP client on a normal user account.** Ask logs in with
  SCRAM or OAUTHBEARER, joins rooms and parses mentions from XEP-0513.
  - Pros: no Waddle changes for basic use, and it works with other XMPP
    servers too.
  - Cons: it is not a bot to Waddle (no hat, commands or enrichment UI). It
    would have to read every message in joined rooms to spot mentions, which
    is ingestion-shaped. It also bypasses the operator grant model.
- **C. XEP-0114 external component.** Waddle has none. The only references
  are comments about future externalisation (push service, Jingle). It is
  absent from the conformance audit (grep for `XEP-0114`; `xeps/xep-0114.xml`
  is only the spec text). It would mean a new transport plus routing, and
  bots would still lack the extension framework's UI. *Inference:* not worth
  it while A exists.

## 4. A shared chat-adapter contract

### 4.1 Where things run (inference)

| | Slack | Waddle |
|---|---|---|
| Who opens the connection | Ask → Slack (Socket Mode WebSocket, Web API calls) | Waddle extension host → Ask (HTTPS) |
| Adapter code lives in | Ask (Rust Slack adapter) | Waddle WASM extension (thin) + Ask's inbound chat API |
| Reply path | Ask calls `chat.postMessage` / streams | Extension returns effects or calls `send-message` within the same invocation |
| Deadline | ack ≤ 3 s, answer whenever (stream ≤ 10 min) | whole answer ≤ 45 s |

So the shared contract sits *inside Ask*, below transport. The Slack adapter
turns Socket Mode envelopes into contract values. The Waddle extension sends
contract values, already typed, to Ask's inbound endpoint and renders the
typed response. Following Yggdrasil's typed-payload rule, the contract is
typed Rust values with no strings carrying structure. Serialisation happens
only at the Slack WebSocket or Web API and the Waddle HTTPS boundary.

### 4.2 What the contract abstracts

| Concern | Contract value (sketch) | Slack | Waddle |
|---|---|---|---|
| Inbound question | `Invocation { origin: ChatOrigin, actor: ChatActor, question: Question, trigger: Trigger }` with `Trigger::{Mention, DirectMessage, Command, FollowUp(AnswerRef)}` | `app_mention`, `message.im`, `/ask`, block action | mention hook (W2), DM (W3), `command`, `launch` |
| Location | `ChatOrigin { platform, conversation: ConversationRef, thread: Option<ThreadRef> }` | `(team/enterprise, channel, thread_ts)` | `(waddle-id, room, thread-id)` (thread needs W1 for commands) |
| Who asked | `ChatActor { platform_user: PlatformUserKey }` | `(enterprise_id?, team_id, user_id)` | bare JID |
| Audience | `Audience::{Private, Conversation { external_members: bool, non_member_count: Option<u32> }}` | ephemeral / DM vs channel; Slack Connect/guests (*unverified* fields) | command-form result vs room; `list-room-members` |
| Answer | `CitedAnswer { body: AnswerBody, citations: Vec<Citation>, abstention: Option<Abstention>, trust: Vec<TrustMarker> }` | Block Kit (§2.4) | enrichment `ui-view` (§3.5) |
| Citation | `Citation { target: CitationTarget::{Symbol, File, DocSection, Memory}, link: WebLink, provenance, memory_state }` | `context` block link | `list-item` + launch (link needs W4) |
| Reply placement | `ReplyTo::{SameThread, NewThreadUnder(MessageRef), Private}` | `thread_ts`, ephemeral, DM | `thread-id`/`reply-to`, command result |
| Progress | `Progress::{Working, Streaming(Chunk), Done}` | thread status / `chat.*Stream` | none in-invocation (W5) |
| Capture | `CaptureRequest { source: CapturedMessage { text, author: ChatActor, at: MessageRef }, proposed: MemoryDraft }` → confirm step → `remember` | message shortcut + modal | launch on a message + XEP-0004 form |
| Notification | `Notice { audience: NoticeTarget::{Conversation, Member}, kind: NoticeKind }` | `chat.postMessage` to a configured channel / DM | provider webhook → granted room |
| Feedback | `Feedback { answer: AnswerRef, verdict }` | `context_actions` feedback buttons | launch buttons |

The contract does **not** abstract transport, install and secrets (Slack
tokens vs Waddle operator config) or rendering. Each adapter owns a renderer
from `CitedAnswer` to its native format, so citation caps and fallbacks stay
platform-specific.

### 4.3 Invariants the contract enforces (inference)

- **No ingestion.** An adapter only produces `Invocation`, `CaptureRequest`
  and `Feedback` values triggered by a user. No adapter subscribes to
  general message events: Slack gets no `channels:history`, Waddle gets no
  `message-observe` or `roomObservation`. A capture holds exactly the one
  message the user chose.
- **Answer visibility ≤ audience.** If the audience includes non-Members or
  external organizations, or later, once Teams are a visibility boundary,
  Members outside the answer's Teams, the adapter downgrades to `Private`.
  Ask's answer composer must therefore receive the audience as input. It
  cannot be decided at render time.
- **Capture always confirms.** Slack requires a modal for shortcuts. Waddle
  uses a form. Either way the user sees the Memory text and its Anchors
  before `remember` runs. That also keeps a prompt-injected answer from
  writing Memories by itself.

### 4.4 Identity mapping from chat user to Member

- **Recommended (inference).** An explicit link, keyed by `PlatformUserKey`.
  1. On first use, the bot replies privately with a one-time link to the web
     GUI.
  2. The user logs in with OIDC (SSO, decided in #121).
  3. Yggdrasil stores `PlatformUserKey → Member`.

  The link is revocable from the admin console.
- **Email as a hint only.** Slack `users.info` gives email with
  `users:read.email`
  ([users.info](https://docs.slack.dev/reference/methods/users.info)), and
  Sign in with Slack ID tokens carry `https://slack.com/user_id`, `team_id`
  and `email` ([Sign in with Slack](https://docs.slack.dev/authentication/sign-in-with-slack/)).
  Neither proves that the person controls the Yggdrasil Member. Waddle
  exposes no claims to extensions at all (W7).
- **Unlinked users.** They get either nothing or a public-only, Memory-free
  answer, as an Admin policy. Under ADR 0001, every Member sees all indexed
  content, so an unlinked chat user must not get answers from it.
- **Authorship, an open question for #134.** ADR 0007 needs per-Member
  identity for Memories and refuses step-up acts over MCP. Ask is an MCP
  client, so a chat-captured Memory can be at most a private, agent-path
  write. Is its author the human Member (Ask holds a delegated agent token
  per linked Member) or the Ask agent Member, with the chat user recorded as
  the requester? The first option keeps "private until promoted" meaning
  "private to the human". The second needs a new on-behalf-of attribution.
  Either way, promotion stays in the web GUI with passkey step-up. Chat
  cannot carry the step-up factor.

### 4.5 Notifications (feeds the map's "notification plumbing")

- **Slack.** Ask already holds a bot token, so a notice is a
  `chat.postMessage` to a channel configured per Team, or a DM to a linked
  Member. Throttle it to the ~1 msg/s per-channel limit and send digests.
- **Waddle.** Ask POSTs an HMAC-signed provider webhook to
  `/webhooks/providers/yggdrasil/yggdrasil-ask`. The extension posts to an
  operator-granted room. DMs to Members are impossible on this path (§3.7)
  unless Waddle adds a grant for requester-less direct sends.
- **Contract.** `Notice` with `NoticeTarget::Conversation` is portable.
  `NoticeTarget::Member` is Slack-only until Waddle closes that gap.

### 4.6 Open questions for the design tickets

1. **Memory authorship.** Who authors a chat-captured Memory: a delegated
   per-Member agent token, or the Ask agent with an on-behalf-of record
   (§4.4)?
2. **Audience on Slack.** Which Slack fields identify Slack Connect and guest
   audiences reliably? `conversations.info` flags are *unverified* here.
3. **Waddle changes.** Is the user willing to land W1–W5 in Waddle? They are
   Waddle work, tracked in Waddle's repo, not Yggdrasil's.
4. **Slackbot MCP Client.** Should it be offered at all, given public MCP
   reachability and Slack-side answer composition (§2.9)?

## 5. Sources

**Slack (docs.slack.dev and slack.com, read 2026-11-23):**

- Events and connections:
  https://docs.slack.dev/apis/events-api/;
  https://docs.slack.dev/apis/events-api/using-socket-mode;
  https://docs.slack.dev/reference/methods/apps.connections.open;
  https://docs.slack.dev/authentication/tokens;
  https://docs.slack.dev/reference/events/app_mention.
- Interactivity:
  https://docs.slack.dev/interactivity/implementing-slash-commands;
  https://docs.slack.dev/interactivity/handling-user-interaction;
  https://docs.slack.dev/interactivity/implementing-shortcuts.
- Messages and Block Kit:
  https://docs.slack.dev/reference/methods/chat.postMessage;
  https://docs.slack.dev/reference/methods/chat.startStream;
  https://docs.slack.dev/reference/methods/conversations.replies;
  https://docs.slack.dev/reference/block-kit/blocks;
  https://docs.slack.dev/reference/block-kit/blocks/section-block;
  https://docs.slack.dev/reference/block-kit/blocks/context-block;
  https://docs.slack.dev/reference/block-kit/blocks/context-actions-block.
- Rate limits and changelog:
  https://docs.slack.dev/apis/web-api/rate-limits;
  https://docs.slack.dev/changelog/2025/05/29/rate-limit-changes-for-non-marketplace-apps;
  https://docs.slack.dev/changelog/2025/06/03/rate-limits-clarity.
- Manifests and Enterprise:
  https://docs.slack.dev/app-manifests/configuring-apps-with-app-manifests;
  https://docs.slack.dev/reference/app-manifest;
  https://docs.slack.dev/enterprise/;
  https://docs.slack.dev/enterprise/developing-for-enterprise-orgs.
- AI, Marketplace and terms:
  https://docs.slack.dev/ai/;
  https://docs.slack.dev/ai/agent-quickstart;
  https://docs.slack.dev/ai/slackbot-mcp-client/;
  https://docs.slack.dev/slack-marketplace/slack-marketplace-app-guidelines-and-requirements;
  https://slack.com/terms-of-service/api.
- Identity:
  https://docs.slack.dev/authentication/sign-in-with-slack/;
  https://docs.slack.dev/reference/methods/users.info.

**Waddle (local repo at `fee238c72`):**

- Product and protocol inventory: `README.md`; `CONTEXT.md`;
  `server/capabilities.toml`; `docs/xep-conformance-audit.md`.
- Extension contract and design: `server/wit/waddle-extension.wit`;
  `docs/extensions/sample-plugin-contracts.md`;
  `docs/extensions/room-observations.md`;
  `docs/superpowers/plans/2026-04-27-waddle-extension-xmpp-protocol.md`;
  `docs/superpowers/plans/2026-09-13-extension-ingress-identity-1753.md`;
  `server/docs/rfcs/0014-bot-framework.md` (historical);
  `docs/rfc/thread-inbox.md`.
- Extension runtime: `server/crates/waddle-extensions/src/{config.rs,config/limits.rs,manager.rs,manager/message_processing.rs,runtime/http.rs,actor.rs}`.
- Server: `server/crates/waddle-server/src/server/{extension_bot.rs,routes/extension_webhooks.rs,routes/websocket/handlers/message.rs,routes/auth/implementation.rs}`;
  `server/crates/waddle-server/src/auth/providers.rs`.
- XMPP auth: `server/crates/waddle-xmpp/src/auth/mod.rs`;
  `server/docs/adrs/0015-dual-authentication.md`.
- Shipped extensions: `server/extensions/ai-chatbot/src/*`;
  `server/extensions/github/src/extension.rs`.

**Yggdrasil:** `CONTEXT.md` (Member, Memory, Verb, Promotion);
`docs/adr/0001-org-visible-access-model.md`;
`docs/adr/0007-code-anchored-team-brain.md`; map issue #121.
