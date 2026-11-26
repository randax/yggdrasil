# Humans log in through OIDC, agents are bound by their principal's passkey, and Forge sync fails closed

Status: accepted. Amends RFC 0001 §5.1 and §8; supplies the identity rules that ADR 0008 depends on.

RFC 0001 deferred OIDC to after v1, and every Member today holds a token an Admin issued and pasted. ADR 0008 makes Team membership a visibility boundary, and the web GUI needs a login. Both need an answer to who a person is, how they reach the CLI and their agents, and how their Forge accounts and Teams stay true. The building blocks and their costs are in the [research note](https://github.com/randax/yggdrasil/blob/ba63df882d7cab45a9a40d975e801d84e5169d6e/docs/research/identity-login-team-sync.md).

Three ideas run through the rules below:

- **Authority comes from a stepped-up human act.** A Forge, an IdP or a config file may *inform* who someone is and what Teams they are in. None of them can create a Yggdrasil role, and every widening is an Admin act with step-up, a reason, audit and a notice (ADR 0008).
- **The factor never reaches the agent.** Every ceremony that hands out or uses a credential is a passkey touch on Yggdrasil's own web origin, on a page that says what is being approved.
- **Freshness is stated and fails closed.** Anything derived from a Forge or an IdP has a maximum age. When it cannot be reconfirmed, it lapses and access narrows.

## The rules

### Login

- A human logs in through a **Login Provider**: a generic OIDC issuer (a corporate IdP, GitLab or Forgejo) or GitHub's OAuth, which has no ID token and is served by a separate adapter. An Admin chooses which configured providers may be used for login. A deployment may allow only its IdP and keep Forges for linking alone.
- A login identity is keyed by `(issuer, sub)`. Entra is keyed by `(tid, oid)`, because its `sub` changes per application. GitHub is keyed by its numeric user id. An email or a login name is never a key.
- Several issuers make Yggdrasil a client of several authorization servers, so every flow uses PKCE, exact redirect URIs and mix-up defence (RFC 9700).

### Who becomes a Member

- An Admin, acting with step-up, writes **Member Admission Rules**. A rule matches one of:
  - a group asserted by a login-allowed IdP;
  - an email domain asserted as verified by a login-allowed IdP (Google's `hd` claim, never its request parameter);
  - active membership of a connected Forge Namespace, checked through the Forge connection's service credential.
- A login that matches a rule becomes a Member at once and enrols a passkey before doing anything else. A login that matches no rule becomes **pending**: it grants nothing, Admins are notified, and it expires after 14 days unless an Admin approves it with step-up.
- A rule never grants the promote right beyond RFC 0001's default for human Members, never grants the Admin role, and may only use a login-allowed provider. A rule that would match any account of a provider (all of GitHub, every Google account) is rejected.
- Every login re-checks the rules against fresh claims. A Member admitted through a Forge Namespace rule is reconfirmed by Forge sync, under the freshness rules below.

This amends RFC 0001 §5.1, where creating a human Member was an Admin act for each person. The Admin's step-up now authorises the rule, and the rule is the audited authority act.

### The first Admin and lockout

- The bootstrap credential (`YG_BOOTSTRAP_TOKEN`, held in memory and never stored) runs `yg admin bootstrap` on the server host. It configures at least one Login Provider and names the first Admin by provider identity, then prints a single-use enrolment link that expires in about 15 minutes. The named person logs in, opens the link and enrols two hardware keys (below).
- After that the bootstrap credential can create an Admin only while no Admin has an enrolled factor, or while the restore gate is closed. Every such use is audited and sends a notice to every Admin. It keeps its other RFC 0001 roles unchanged.

### Step-up

- Every step-up act from the CLI or REST is a **browser hand-off**. The client creates a pending act and gets a URL on Yggdrasil's origin. The page shows the exact act: for a promote, the Memory, its Audience and the shared head it expects to displace. The human touches a passkey with user verification, and the server completes that act and consumes the challenge once. The CLI polls for the outcome. Ceremony state stays on the server.
- TOTP is no longer a factor: an agent that can see the screen or read the seed can pass it. Direct CTAP2 from the CLI is not offered now and can be added later without changing the rest.
- **Members** may use any passkey with user verification, synced ones included. **Admin** step-ups need an attested, non-synced hardware key, and every Admin enrols two. Admin step-ups cover widening, Break-glass Sessions, issuing Service Agent tokens, writing Member Admission Rules, moving Forge Links and lockout recovery.
- A Member adds and removes their own passkeys by stepping up with one they still hold. A Member who has lost every passkey gets a fresh single-use enrolment link from an Admin acting with step-up, since every human Member holds the promote right by default (RFC 0001 §5.1). A lost last Admin key is lockout, handled above.

### Credentials

Every credential is a `member_tokens` row of one of four kinds, so one hash, expiry, revoke and audit path, and one restore-gate rule, cover them all.

| Kind | How it is issued | Lifetime | Accepted on |
|---|---|---|---|
| Web session | OIDC login in the browser; carried in a `__Host-` cookie, `HttpOnly`, `Secure`, `SameSite=Strict`, with CSRF protection | 30 min idle, 8 h absolute; the id is rotated on login and on step-up | the web GUI |
| Human CLI | `yg login`, a device grant Yggdrasil hosts (RFC 8628) | about 12 h, no refresh | REST and CLI; refused over MCP |
| Personal Agent | `yg mcp login`, the same device grant requesting an agent token | 90 days by default; the maximum is set by an Admin | MCP and REST, as that agent Member |
| Service Agent | an Admin acting with step-up | set by the Admin | MCP and REST, under its reader ceiling (ADR 0008) |

- **Device-grant approval is a step-up ceremony.** The human opens the code page in the web GUI, where they are logged in, and the page shows the user code to compare, the requesting host and client, and the token kind in plain words: "human CLI session, not for agents" or "Personal Agent token for <name>, bound to you". Approving takes a passkey touch. User codes are rate-limited.
- **A Personal Agent is bound by the approver.** The Member who approves the grant becomes its principal, and the binding never changes. There is no path that hands a Personal Agent token to a human to paste; a remote sandbox uses the same grant, because the human types the code into their own browser.
- `yg mcp` and the MCP endpoint accept only agent tokens. A leaked human CLI token still cannot publish, because every publishing act needs its own step-up.
- Web sessions and human CLI tokens end on logout, revoke, expiry, the Member's admission lapsing, an IdP back-channel logout where the IdP sends one, or a restore, which revokes all of them and makes everyone log in again.

### Offboarding through the IdP

- A Member admitted through an IdP rule must log in successfully within the **Liveness Window**: 7 days by default, set by an Admin. Otherwise their Membership is suspended, and so is every Personal Agent bound to them, until they log in again and the rules still match. Suspension is a narrowing and needs no step-up.
- An IdP may also push deprovisioning through an optional SCIM endpoint. `active=false` suspends the Member and their Personal Agents. The endpoint only suspends: it creates no Member and grants nothing, which stays with the admission rules. Its bearer token is a Service-Agent-like credential an Admin issues with step-up.

### Forge Links

- A **Forge Link** binds a Member to a Forge account, keyed by the Forge and its numeric user id, never by login, so a renamed or reclaimed login cannot inherit it. It is made either by the Member completing that Forge's OAuth (logging in through a Forge also makes the link), or by an Admin with step-up, a reason, audit and a notice (ADR 0008). It is never inferred from Contributor merges or email.
- A Member may hold many Forge Links; their synced Team memberships are the union across them. A Forge account links to at most one Member. A second Member who proves control of a linked account is refused and told to ask an Admin. An Admin may move the link with step-up, which narrows the old holder and widens the new one; both are told.
- Each sync reconfirms that a linked account still exists. A deleted account's link lapses.
- A Member is linked to Contributors only through their Forge Links (`identities.kind = forge_account`), never through a commit email, which anyone can write into git.

### Forge sync

- **Credential.** Each Forge connection syncs with its own service credential, never a person's:
  - GitHub: a GitHub App installation with Members read, Metadata and Contents read, whose user OAuth also serves the Forge Link step;
  - GitLab: a group access token or service account at the top-level group, with `read_api`;
  - Forgejo and Codeberg: a dedicated bot account in the org with a read-scoped token.

  Only teams the credential can see are synced. The admin console lists teams it cannot see, so a hidden team fails visibly. Replacing a connection's credential is an Admin act with step-up; if the new credential sees more, the change is a widening and sends a notice.
- **Who a synced Team contains.** A Member is in a synced Team when one of their Forge Links is an *active* member of the Forge team in the Forge's own effective view (GitHub's listing including child teams; GitLab's `/members/all`, including inherited and shared-group members; Forgejo's flat teams), with a role that carries code read: GitHub member or above, GitLab Reporter or above, Forgejo a team with Code read. Pending invitations and Guest, Planner and Minimal Access roles never count. This is the same filter ADR 0008 applies to Forge-derived repo grants.
- **Unlinked accounts.** A Forge account in a team with no linked Member is held as an unclaimed entry and takes effect at the first sync after a Member links it. Bot and machine accounts never become Members, since only a human links an account. Agents get Teams from their principal (Personal Agents) or from an Admin (Service Agents).
- **Freshness.** Forge-derived state is re-polled every 5 minutes and lapses when not reconfirmed within 30 minutes. That covers Team memberships, repo grants, repo visibility on the Forge, Forge Links and Forge Namespace admission rules. An Admin may set the interval between 5 and 60 minutes and the maximum age between 3× the interval and 24 hours. Webhooks, where a Forge offers them, only trigger an early re-poll of the affected team or repo; they are never applied as deltas. A lapse caused by an outage sends a notice to Admins. Explicit Admin grants and memberships of local Teams never lapse.
- **Converting a local Team to a synced one** replaces its membership with the Forge's. Members the conversion adds are a widening and send a notice.
- **Roles.** No Forge role maps to a Yggdrasil role. Forge org owners do not become Admins, and Forge team maintainers do not become Team Maintainers; an Admin appoints both with step-up. Forge roles feed only the code-read filter.

### Restore

The new tables (login identities, Member Admission Rules, pending logins, Forge Links, passkeys and their ceremony state, and every credential kind) are authority revisions under the restore gate (RFC 0001 §5.1). On restore, every web session and human CLI token is revoked, and every Forge-derived membership, grant, link and admission is treated as unconfirmed until the next successful sync, so it fails closed.

## Considered options

- **The Forge as the only IdP.** The login is the link, which is simple. But a personal GitHub account outlives employment, so there is no offboarding gate, and a deployment with several Forges needs a login per Forge.
- **An external broker** (Dex, Keycloak, Authentik). One OIDC client inside Yggdrasil, but another service for every self-hosted deployment, and its `sub` wraps the upstream identity so a Forge Link still needs a separate proof.
- **Invitation-only or approval-only Membership.** Strictest and closest to RFC 0001 as written, but an Admin acts for every hire, and offboarding is never automatic.
- **Declaring the first Admin in the deploy config.** No token step, but a config edit would mint an Admin silently.
- **IdP device grants exchanged for Yggdrasil tokens.** Per-provider quirks, Forgejo has none, and the approval happens at the IdP, where Yggdrasil cannot make it a step-up ceremony that names the token kind.
- **Admin-issued or pasted Personal Agent tokens.** An Admin acting for every agent does not scale, and pasting puts a bearer secret on the clipboard; the device grant puts it straight into the agent's environment.
- **TOTP or direct CTAP2 for CLI step-up.** TOTP is the factor most exposed to an agent, and neither shows the human the act being approved.
- **Hardware keys for everyone, or for no one.** For everyone works against admission by rule; for no one leaves widening and Break-glass behind a synced passkey an agent with computer use might drive.
- **A separate session store, or stateless cookies.** A second store doubles the revoke and restore story; stateless cookies cannot be revoked at all.
- **Hourly polling with a day's maximum age.** Cheap and outage-tolerant, but a removed employee keeps Restricted access for up to an hour, and a stale grant lingers for a day.
- **Direct membership only, or excluding GitLab shared groups.** Both contradict what people see on their Forge, and direct-only contradicts expanding nested teams.
- **An Admin's personal token for sync.** Quicker setup, but what everyone sees would then depend on one person's access, secret teams included.
- **Latest proof wins for a Forge Link.** Smoother for a rebuilt Member, but a borrowed Forge session would silently move someone's Team access.
- **Shorter agent tokens instead of liveness.** A daily passkey ceremony per agent, and the gap still lasts a day.
- **Mapping Forge roles to Yggdrasil roles.** Less setup, but Forge administration would then mint Yggdrasil authority without step-up or a notice.

## Consequences

- The `members` table RFC 0001 §2 sketched is finally needed, together with login identities, Member Admission Rules, pending logins, Forge Links, passkeys and a `kind` column on `member_tokens`.
- Yggdrasil implements an RFC 8628 device-authorization endpoint, a WebAuthn relying party (`webauthn-rs`, with attestation for Admin keys), an OIDC client and a GitHub OAuth adapter, and optionally a SCIM endpoint.
- Every step-up act needs a browser, and every Admin needs two hardware keys.
- IdP-admitted humans must log in at least once per Liveness Window, or their agents stop.
- Forge sync becomes a polling loop over teams and permissions per connection, within each Forge's rate budget, and its outages narrow Visibility.
- RFC 0001 still says "OIDC post-v1" (§8), names TOTP as a factor and makes every human Member an Admin act (§5.1), and sketches `tokens` without kinds (§2). Those sections are rewritten to this ADR in the map's RFC consolidation step; until then this ADR wins where they disagree.
