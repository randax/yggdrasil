# Identity: OIDC login, passkey step-up and Forge team sync

Status: research note · Date: 2026-11-23 · Ticket: #128 (map #121) · Yggdrasil baseline: commit `c4bec0c` on `main`

> Scope. This note covers the building blocks for human login to the web GUI and
> CLI, for the step-up factor that RFC 0001 §5.1 requires on human publishing
> acts, and for syncing Team membership from Forge teams and groups. It lays out
> options with their costs and does not choose between them. Every external
> claim cites a primary source by number (see [Sources](#sources)). Claims about
> Yggdrasil cite repository paths at the commit above. **Unverified** marks a
> claim the sources consulted did not settle. *Inference* marks reasoning of my
> own that no source states.

Settled before this note (map #121): a web GUI with OIDC/SSO login and passkey
step-up; Teams whose membership can sync from Forge teams and groups; one
Organization per deployment.

## 1. Summary

1. **Only GitHub is not OIDC.** GitLab, Google, Okta, Entra ID and Forgejo are
   OIDC providers. GitHub user login is plain OAuth 2.0: no ID token, so identity
   comes from `GET /user` with the access token [12]. A login layer therefore
   needs a generic OIDC path and a GitHub-specific OAuth path.
2. **The stable keys differ per provider, and none is an email.** OIDC fixes
   `(iss, sub)` as the only stable identifier and forbids using `email` or
   `preferred_username` as one [1 §5.7]. Entra's `sub` is pairwise per
   application, so the cross-app key there is `oid` with `tid` [35]. GitHub's
   key is the numeric user `id`. Logins change, and a released login can be
   claimed by anyone [21][22].
3. **The Rust stack exists and is maintained.** `openidconnect` 4.1.0 and
   `oauth2` 5.0.0 cover the protocols. For sessions on the repo's axum 0.8 and
   sqlx 0.9 there are `tower-sessions` 0.15, `tower-sessions-sqlx-store` 0.16 and
   `axum-login` 0.19. `webauthn-rs` 0.6.2 is SUSE-audited, and its passkey flows
   hard-code `UserVerificationPolicy::Required`. `passkey-rs` is an
   authenticator/client library, not a relying-party verifier [47–51].
4. **WebAuthn is bound to a web origin, and the CLI is not one.** The CLI's
   step-up options are a browser hand-off to Yggdrasil's own origin (works with
   synced passkeys); direct CTAP2 to a USB or NFC key (roaming keys only); or
   TOTP. IdP-delegated step-up (RFC 9470) is not bound to a single act [6], so
   on its own it does not meet §5.1's "authorises that single act".
5. **The device grant opens a hole in credential separation.** RFC 8628's
   remote-phishing risk [3 §5.4] fits Yggdrasil exactly. An agent can start a
   device login and ask its human to approve it. The human token then lands in
   the agent's environment, which RFC 0001 §8 assumes never happens. If the
   approval page is the step-up ceremony itself, this gets harder.
6. **Forge membership webhooks are uneven.**
   - GitHub has `membership`, `organization` and `team` events but never retries
     failed deliveries [18][19].
   - GitLab has member events only on Premium and Ultimate group webhooks, or in
     self-managed system hooks [30][31].
   - Forgejo/Codeberg has no membership event among its 41 hook types, so it can
     only be polled [43].

   A poll reconciler is therefore required anyway. Webhooks can only speed it
   up, which matches RFC 0001 §3 ("poll is truth, webhooks are latency").
7. **The Forges disagree on nesting.** GitHub REST team listings include
   child-team members; GraphQL can ask for `IMMEDIATE` only [14][17]. GitLab
   splits direct from inherited and shared membership (`/members` vs.
   `/members/all`) [29]. Forgejo teams are flat [45]. "Member of Team X" needs
   one definition that Yggdrasil fixes itself.
8. **SCIM is a different direction.** With SCIM, the IdP pushes Users and
   Groups into Yggdrasil [9][10]. That provisioning and deprovisioning signal is
   faster than any Forge poll. The cost is a new authenticated inbound surface.
   Entra cannot provision nested groups through SCIM [37], and Okta deactivates
   users rather than deleting them [41].
9. **Teams as a visibility boundary make sync lag access lag.** This reopens
   ADR 0001: a removal that has not synced yet is a leak. Team membership is
   authority state, so it falls under the restore gate (RFC 0001 §5.1) like
   tokens and grants.

## 2. Starting point in the repo

- **Tokens.**
  - `member_tokens(id, member text, token_hash, created_at, last_used_at,
    revoked_at, expires_at)`, with the hash stored at rest
    (`crates/yg-control/migrations/0010_member_tokens.sql`, `0017_member_token_expiry.sql`).
  - `issue_member_token` mints `ygm_…` tokens for a `MemberName` with an
    optional lifetime (`crates/yg-control/src/lib.rs`).
  - There is no `members` table yet. The RFC's `members(id, name, kind,
    can_promote, is_admin)` and `tokens` sketch (RFC 0001 §2) is not built.
- **Auth middleware.** `crates/yg-api/src/auth.rs` authenticates bearer tokens
  on every request. It hashes the token with SHA-256, rate-limits failures with
  a negative cache and recognises the bootstrap token without the database. The
  HTTP stack is axum 0.8.9, reqwest 0.13.4 and sqlx 0.9.0 on Postgres
  (`crates/yg-api/Cargo.toml`).
- **Requirements already written.**
  - RFC 0001 §8 says "OIDC post-v1" and requires a separate agent token for
    `yg mcp` (`YG_AGENT_TOKEN`).
  - §5.1 requires a factor on human `promote`, accepting a proposal and
    affirming a shared Memory. The factor must be "never stored on the agent's
    machine" and authorises "that single act", either as a WebAuthn/passkey
    touch or a TOTP code. It is "enrolled for that Member when an Admin issues
    the human's token". These acts are refused over MCP.
  - A human Admin with step-up is required to:
    - create a human Member and bind its first factor;
    - issue a token for an existing Member;
    - replace the factor of an Admin or of any Member who holds the promote
      right.
  - Authenticator enrolments and revocations are authority revisions that the
    restore gate re-checks.
- **Glossary.** A Member is "an authenticated human or agent authorized to query
  an Index Server". A Contributor is "a person appearing in synced repositories'
  history or forge artifacts, identity-merged across forges". An Admin operates
  the deployment (`CONTEXT.md`). RFC 0001 §2 sketches Contributor identities as
  `identities(contributor_id, kind email|forge_account, value,
  merge_provenance)`.
- **ADR 0001** ("Org-visible access model") says query-time ACLs were rejected
  because they need "per-user identity mapping and permission sync across three
  forges". This note is about exactly those pieces.

## 3. Login options

### 3.1 Provider facts

| Provider | Protocol for user login | Stable user key | Groups at login | Device grant (RFC 8628) |
|---|---|---|---|---|
| GitHub | OAuth 2.0 only; identity via `GET /user` [12] | numeric `id` (`login` changes; `GET /user/{account_id}` takes "their durable user ID") [21] | none in the token; call the API | yes; must be enabled per app, otherwise `device_flow_disabled` [12] |
| GitLab | OIDC [27] | `sub` = "The ID of the user" [27] | `groups_direct` in the ID token; `groups` and `https://gitlab.org/claims/groups/{owner,maintainer,developer}` only in userinfo [27] | yes; GA in 17.9 (flag removed) [28] |
| Google | OIDC [33] | `sub`: "unique among all Google Accounts and never reused" [33] | none; restrict tenants with the `hd` claim, not the `hd` request parameter [33] | yes, but only for `openid email profile` (plus a few Drive and YouTube scopes), with a separate "TVs and Limited Input devices" client [34] |
| Entra ID | OIDC [35] | `oid` + `tid`; `sub` is pairwise per application ID [35] | `groups` (optional claim); above 200 in a JWT the token carries an overage pointer to Graph instead [35] | yes; no `verification_uri_complete` [36] |
| Okta | OIDC [39] | `sub` (per OIDC) | a `groups` claim configured on the org or custom authorization server [39] | yes, native app type only [40] |
| Forgejo / Codeberg | OAuth2 + OIDC discovery [42] | `sub` = numeric user ID (`fmt.Sprint(grant.UserID)`) [44] | `groups` scope yields `org` and `org:team` entries [44] | **not mentioned**; the docs say only the authorization code grant (with PKCE) is supported [42] |

Further protocol constraints:

- **PKCE and redirect URIs.** RFC 9700 requires PKCE for public clients and
  recommends it for confidential ones. Redirect URIs need exact matching, and
  mix-up defence is required for clients that talk to more than one
  authorization server [5 §2.1, §4.4.2]. Yggdrasil configured with several IdPs
  is such a client.
- **GitHub PKCE** is S256 only [12].
- **GitHub token lifetimes.** GitHub App user tokens expire after 8 h; refresh
  tokens last 6 months. "A GitHub App user access token only has permissions
  that both the user and the app have" [13].

### 3.2 Options

**L1. Generic OIDC plus a GitHub OAuth adapter, in-process.** One OIDC code
path covers GitLab, Google, Okta, Entra and Forgejo; GitHub gets a second path.

- *Cost:* two flows, a per-provider claim mapper (Entra `oid`/`tid` and
  overage; Google `hd`; GitLab userinfo-only groups) and mix-up defence.
- *Libraries:* `openidconnect` 4.1.0 (released 2026-10-31) provides discovery,
  PKCE, nonce checks, typestate clients, `AdditionalClaims` for custom claims
  and RP-initiated logout types. It also has device-grant types; the
  `exchange_device_code` call shape is **unverified** [50]. It builds on
  `oauth2` 5.0.0 (released 2025-01-21), which serves the GitHub path too [47].

**L2. Forge-as-IdP only.** Login goes through the deployment's Forge (GitHub
OAuth, GitLab OIDC or Forgejo OIDC). The logged-in identity *is* the Forge
account, so mapping it to team membership and Contributors is direct.

- *Cost:* there is no corporate IdP gate. A personal GitHub account outlives
  employment, so offboarding means leaving the Forge org, not the IdP.
- *Cost:* a deployment with Forge connections to several Forges needs a login
  per Forge, or a primary one.

**L3. External broker.** Yggdrasil speaks OIDC to one broker, which federates
GitHub, SAML and the rest. Brokers include Dex, Keycloak, Authentik and Kanidm;
none of their docs were checked for this note.

- *Benefit:* Yggdrasil keeps a single OIDC client.
- *Cost:* the self-hosted deployment gains another service to run.
- *Cost:* the broker's `sub` becomes the identity key. It wraps the upstream
  identity, so Forge-account mapping needs the upstream claims passed through.
  How each broker exposes them is **unverified**.

**L4. SAML.** Not researched. Entra and Okta both offer OIDC [35][39], and
GitHub's SAML is for its own orgs, not for relying parties. *Inference:* L1
covers the same IdPs without a SAML stack.

L1 and L2 can coexist: an IdP login with a separate *Forge account link* step
(§6.2).

## 4. Sessions: web GUI and CLI

### 4.1 Web GUI

| Option | How | Cost |
|---|---|---|
| **W1. Server-side session, Postgres store** | `tower-sessions` 0.15 + `tower-sessions-sqlx-store` 0.16, which depends on sqlx `^0.9.0`, matching the repo; `axum-login` 0.19 for the login/logout lifecycle [47] | Session rows are authority state (see the restore-gate note below). One DB read per request, as bearer tokens already pay (`auth.rs`). |
| **W2. Stateless signed or encrypted cookie** | Session claims in the cookie, verified per request | No server-side revocation: OWASP says "deleting a cookie alone does not invalidate a stolen copy" and requires server-side invalidation [46]. Conflicts with instant revocation and the restore gate. |
| **W3. Sessions as short-lived `member_tokens` rows** | The web login mints a token row of a distinct kind; the cookie carries it | Reuses the existing hash, revoke and expiry path and the auth middleware. It needs a token-kind column and CSRF handling, since cookies are ambient credentials. |

The security floor whichever option is chosen [46]:

- session IDs of at least 64 bits of entropy;
- `Secure`, `HttpOnly` and an explicit `SameSite` (`Strict` preferred);
- the `__Host-` prefix (`Secure`, no `Domain`, `Path=/`);
- a new ID "after any privilege level change";
- idle timeouts of 2–5 min for high-value apps and 15–30 min for low-risk
  ones, and an absolute timeout of 4–8 h;
- invalidation on the server.

`axum-login` already calls `session.cycle_id()` on login ("Session-fixation
mitigation") and `flush()` on logout [51].

IdP-driven logout:

- OIDC Back-Channel Logout 1.0 (Final) lets the IdP POST a signed
  `logout_token` carrying `sub` and/or `sid`. The relying party "clears any
  state associated with the identified session(s)" [2].
- This is the only standard push signal for "kill this session now" short of
  SCIM.
- Which of the five IdPs send it was **unverified**. GitHub, being OAuth only,
  cannot.

*Inference (restore gate):* sessions in Postgres roll back with a restore just
as tokens do. A snapshot taken before a logout or a revocation brings the
session back. If sessions count as credentials, the §5.1 gate has to revoke
every session on restore, or derive their validity from the authority-revision
log. Forcing everyone to log in again is the cheap answer.

### 4.2 CLI

| Option | How | Cost |
|---|---|---|
| **C1. Admin-issued token (status quo)** | `yg admin token issue`; the human pastes `ygm_…` | No login UX. Every Member creation needs an Admin, which §5.1 already requires for human Members. |
| **C2. IdP device grant, then exchange** | The CLI runs RFC 8628 against the IdP and sends the ID token to Yggdrasil, which mints a `ygm_` token | Per-IdP quirks: GitHub needs device flow enabled; Google needs a separate client type and limits scopes [34]; Okta needs a native app [40]; Forgejo has no device grant [42]. Yggdrasil must accept ID tokens minted for a different client ID, or share one, which complicates audience checks. |
| **C3. Yggdrasil-hosted device grant** | Yggdrasil is the RFC 8628 authorization server. The CLI gets `user_code`/`verification_uri` from Yggdrasil; the human opens the web GUI (already OIDC-logged-in) and approves; Yggdrasil mints the `ygm_` token | Provider-independent and reuses the web login. Yggdrasil must implement the RFC 8628 endpoint, user-code rate limiting (§5.1) and the display of the code for the human to compare (§5.4) [3]. |
| **C4. Loopback redirect (RFC 8252)** | The CLI listens on `http://127.0.0.1:{port}` and opens the browser to Yggdrasil's login, which redirects back with a code (PKCE) | No code typing. Only works when the browser runs on the same host as the CLI; RFC 8252 requires the AS to accept any loopback port and forbids embedded user-agents [4 §7.3, §8.12]. |

**Credential separation (applies to C2–C4).** RFC 8628 §5.4 describes an
attacker who starts the flow and sends the victim the code [3]. In Yggdrasil the
"attacker" can be the human's own coding agent. If the agent runs `yg login` and
asks the human to approve, a human CLI token lands in the agent's environment.
RFC 0001 §8 assumes that does not happen.

Options:

- Make device approval a step-up ceremony (§5).
- Show the requesting client and host on the approval page.
- Have the CLI mint only tokens that cannot publish, keeping publishing gated
  by per-act step-up (already the case).
- Keep agent tokens Admin-issued only.

*Inference:* per-act step-up is what still protects publishing, because a
stolen human CLI token alone cannot promote.

**Agent Members** never log in through OIDC. Their tokens stay Admin-issued,
which §5.1 already requires as "issuing a token for any existing Member".

## 5. Step-up: enrolment and enforcement

### 5.1 What WebAuthn guarantees

- **Status.** WebAuthn Level 3 is a W3C Recommendation dated 25 August 2026
  [11].
- **Flags.** "The UP flag SHALL be set if and only if the authenticator
  performed a test of user presence"; "The UV flag SHALL be set if and only if
  the authenticator performed user verification" [11 §6.1].
- **Required UV.** With `userVerification: "required"` the RP "will fail the
  overall ceremony if the response does not have the UV flag set" [11 §5.8.6].
  The default is `preferred` [11 §5.1.9].
- **Clone detection.** If either counter is non-zero and the new `signCount` is
  less than or equal to the stored one, "a cloned authenticator may exist"
  [11 §6.1.1].
- **Backup flags.** Backup eligibility (BE) is a permanent property of a
  credential, set at creation. Backup state (BS) "can change over time" [11
  §4]. BE=1 means a synced passkey.
- **Origin binding.** The RP ID must equal, or be a registrable suffix of, the
  caller's effective domain [11 §5.1.10.1]. A browser will only run the ceremony
  for Yggdrasil's own web origin.
- **Mapping to §5.1.** A passkey assertion with UV is a human-gesture factor
  (biometric or PIN) over a fresh server challenge. *Inference:* binding it to
  "that single act" comes from server-side state, not from WebAuthn. The server
  issues a challenge for one pending act (Memory id, scope, `expected_head`),
  keeps the ceremony state server-side and consumes it once.

TOTP is the fallback the RFC allows: 30 s steps, at most one step of network
delay, and "The verifier MUST NOT accept the second attempt of the OTP" [8
§5.2]. It is phishable and its seed sits in an authenticator app, but it works
in any terminal.

### 5.2 Rust libraries

| Crate | Role | Status (crates.io, 2026-11-23) [47] |
|---|---|---|
| `webauthn-rs` 0.6.2 | RP-side registration and verification | Released 2026-10-31; repo pushed 2026-11-05. Not archived. "This library has passed a security audit performed by SUSE product security" (README) [48]. |
| `webauthn-authenticator-rs` 0.6.2 | Client side: CTAP2 to USB and NFC keys | Same project and release |
| `ctap-hid-fido2` 3.6.1 | Client side: CTAP2 over HID | Released 2026-11-02 |
| `passkey` / `passkey-types` 0.6.0 (1Password) | Authenticator (CTAP2) and WebAuthn client | Released 2026-10-01. The README says Relying Party interaction is "not within the scope of these libraries" [49], so it is useful for software authenticators in tests, not for server-side verification. |
| `totp-rs` 6.0.0 | TOTP | Released 2026-08-06 |

`webauthn-rs` specifics, from source and docs [48]:

- **UV.** `start_passkey_registration` and `start_passkey_authentication` set
  `UserVerificationPolicy::Required` (`webauthn-rs/src/lib.rs`), and the docs
  say "A passkey will always enforce user-verification". Security-key flows
  only enforce UV opportunistically.
- **Hardware-only passkeys.** `start_attested_passkey_registration` requests
  direct attestation against a supplied CA list and sets
  `reject_synchronised_authenticators(true)`. This is the option for "hardware,
  non-synced only", for example for Admins.
- **Server-side state.** Ceremony state "MUST be persisted on the server", and
  its serialisation is disabled by default so it cannot be parked in a cookie
  (`danger-allow-state-serialisation` re-enables it).
- **Counter.** The caller must check the counter and should update the stored
  credential after authentication.

### 5.3 Enforcement options for the CLI and REST step-up

| Option | Flow | Works with | Cost |
|---|---|---|---|
| **S1. Browser hand-off** | `yg promote` gets a pending-act id and a URL on Yggdrasil's origin. The human opens it, sees the act (Memory, scope, expected head), touches the passkey; the server completes that act. The CLI polls or gets a one-time act grant. | Synced passkeys, platform authenticators, roaming keys | Needs the web GUI and a browser. Two-device use mirrors C3. |
| **S2. Direct CTAP2 from the CLI** | The CLI talks to a USB or NFC key with `webauthn-authenticator-rs` or `ctap-hid-fido2` and posts the assertion with the REST request. | Roaming security keys only | No platform or synced passkeys. *Inference:* the server cannot rely on browser origin enforcement, because the CLI writes `clientDataJSON` itself. The authenticator still checks the RP ID hash. Needs a separate credential registered for that RP ID. |
| **S3. TOTP on the REST request** | The CLI prompts and sends the code with the act | Any terminal | Phishable; an agent that can see the screen or the seed can pass. Exactly what §5.1 names as the alternative. |
| **S4. IdP-delegated step-up (RFC 9470)** | The resource server returns `insufficient_user_authentication` with `acr_values`/`max_age`; the client re-authenticates at the IdP [6] | Whatever the IdP enforces; `amr` values such as `hwk`, `mfa`, `user` [7] | RFC 9470 "does not recommend any specific token-caching strategy", so the elevated token is not tied to one act [6 §2]. Trust is delegated to the IdP's `acr`/`amr`, and GitHub has no ID token. It does not meet "authorises that single act" without extra binding. |

### 5.4 Enrolment options

RFC 0001 §5.1 already says that creating a human Member and binding its first
factor needs a human Admin with step-up, apart from the bootstrap Admin and the
restore gate.

- **E1. Admin-issued enrolment link.** The Admin, with step-up, creates the
  Member and gets a one-time, short-lived link. The human logs in with OIDC,
  opens the link and registers a passkey. The link is bound to the Member, so
  an OIDC identity cannot claim someone else's link.
  - *Cost:* the link is a bearer secret in transit.
- **E2. JIT Member on first OIDC login.** The first login creates a pending
  Member who can't publish until an Admin approves with step-up and the human
  enrols a factor.
  - *Cost:* amends the §5.1 rule that creating a human Member is an Admin act.
    The pending state has to be invisible to authorisation.
- **E3. SCIM- or Team-sync-provisioned Members.** IdP or Forge provisioning
  creates pending Members, as in E2.
  - *Cost:* provisioning would then create authority, which §5.1 reserves for a
    human Admin acting with step-up.

Common follow-ups:

- **Multiple credentials.** WebAuthn allows several per Member;
  `exclude_credentials` prevents duplicates [48].
- **Replacing a lost factor** needs Admin step-up for Admins and promote-right
  holders (§5.1).
- **Synced vs. hardware passkeys.** A synced passkey (BE=1) lives in a cloud
  keychain that may also be unlocked on the machine where the agent runs. UV
  still needs a biometric or PIN gesture. *Inference:* the residual risk is an
  agent with computer-use driving the OS prompt. Admins could be required to use
  attested, non-synced keys (`reject_synchronised_authenticators`).
- **Restore gate.** Enrolments are authority revisions (§5.1). After a restore,
  "a Member whose current enrollment cannot be established must enrol afresh".

## 6. Forge team sync

### 6.1 Per-Forge mechanics

**GitHub**

- **List members.** `GET /orgs/{org}/teams/{team_slug}/members`; "Team members
  will include the members of child teams"; `role` is
  `member|maintainer|all`; `per_page` max 100; OAuth tokens need `read:org`
  [14].
- **Membership state.** Per user it is `active` or `pending`, and pending lasts
  until the invitation is accepted [14].
- **GraphQL.** `Team.members(membership: IMMEDIATE|CHILD_TEAM|ALL)` with `ALL`
  as the default. Teams expose `parentTeam`, `childTeams`, `databaseId` and
  `slug` [17].
- **Webhooks** (org, business and App; "Members" read permission) [18]:
  - `membership` `added` and `removed`, with scope "Currently, can only be
    team";
  - `organization` `member_added`, `member_removed`, `member_invited` and
    `renamed`;
  - `team` `created`, `deleted` and `edited` (name, description or
    visibility).
  - Whether `edited` covers a parent change, and whether `membership` fires for
    child-team inheritance, is **unverified**.
- **Delivery.** "GitHub does not automatically redeliver failed webhook
  deliveries" [19]. Redelivery reaches back only 3 days [20].
- **Rate limits.**
  - REST: 5,000/h for a user token; 5,000–12,500/h for an App installation
    (15,000 on Enterprise Cloud) [15].
  - GraphQL: 5,000 points/h for a user; installations 5,000 + 50 per
    repository or user beyond 20, capped at 12,500 [16].
  - Secondary limits: 100 concurrent requests and 900 REST points/min [15].
  - A 304 on a conditional request "does not count against your primary rate
    limit" when authorised [23].
- **IdP-linked teams.** Team synchronization with Entra and Okta groups needs
  Enterprise Cloud with SAML SSO and "is not a user provisioning service" [24].
  Enterprise Managed Users get usernames "by normalizing an identifier provided
  by your IdP" and are SCIM-provisioned [25].
- **SAML link.** Org owners can map IdP NameID to GitHub login through GraphQL
  `organization.samlIdentityProvider.externalIdentities { samlIdentity { nameId }
  scimIdentity { username } user { login } }` [26].

**GitLab**

- **List members.** `GET /groups/:id/members` returns "only direct members".
  `/members/all` includes "inherited members, invited users, and permissions
  through ancestor groups" and shared-group members, and keeps the highest
  `access_level` [29].
- **Access levels.** 10 Guest, 15 Planner, 20 Reporter, 25 Security Manager,
  30 Developer, 40 Maintainer and 50 Owner (5 Minimal Access on
  Premium/Ultimate) [29].
- **Owner-only identities.** `group_saml_identity` (`extern_uid`, `provider`)
  is only visible to group owners [29].
- **Member webhooks.** `user_add_to_group`, `user_update_for_group` and
  `user_remove_from_group` are **Premium and Ultimate, group webhooks only**.
  Subgroup create and destroy events exist, but nothing fires when a subgroup is
  transferred [30].
- **System hooks.** Self-managed and Dedicated have `user_add_to_group`,
  `user_remove_from_group`, `user_update_for_group`, `user_rename`,
  `user_destroy` and `group_rename` on all tiers, but need instance admin
  access [31].
- **Rate limits.** GitLab.com allows 2,000 authenticated API requests per user
  per minute. Listing all project members is limited to 200/min; there is no
  group-members-specific limit [32]. Self-managed limits are set by the
  instance admin.
- **At login.** The ID token carries `groups_direct` [27], which gives a
  snapshot of direct group paths at each login.
- **Unverified:** webhook retry and auto-disable behaviour.

**Forgejo / Codeberg** (API 17.0.0+gitea-1.22.0 on codeberg.org) [45]

- **Endpoints.** `GET /orgs/{org}/teams`, `GET /teams/{id}/members`,
  `GET /teams/{id}/members/{username}`, `GET /orgs/{org}/members`,
  `GET /user/teams`.
- **Teams are flat.** The `Team` schema has no parent field. Users carry
  `id` (int64) and `login`.
- **No membership webhooks.** The 41 `HookEventType` constants cover create,
  delete, push, issues, pull requests, wiki, repository, release, package and
  workflow runs, with no membership, team or org-member event [43]. Sync is
  **poll only**.
- **At login.** The OIDC `groups` claim lists `org` and `org:team` (lowercased
  team name). It is filtered to public memberships only in some scope modes
  [44].
- **Unverified:** Forgejo/Codeberg API rate limits.

**SCIM 2.0** (the IdP pushes to Yggdrasil)

- **Identifiers.** `id` is issued by the service provider and "MUST be a
  stable, non-reassignable identifier". `externalId` is "always issued by the
  provisioning client". `active` is the admin status. `Group.members[].value` is
  a resource id, and `$ref` may point to a User or Group, so nesting is possible
  [9].
- **Membership changes.** They arrive as atomic `PATCH` add/remove on
  `members[value eq "…"]` [10 §3.5.2].
- **Auth.** SCIM defines none of its own; bearer tokens are allowed with TLS
  [10 §2].
- **Entra.**
  - Runs an initial cycle, then "back-to-back incremental cycles … at intervals
    defined in the tutorial specific to each application". The frequently
    quoted 40-minute figure was **unverified** for non-gallery apps.
  - It "can't read or provision users in nested groups".
  - Out-of-scope users are disabled with `active=false`; hard delete follows
    30 days after soft delete.
  - It authenticates with a long-lived bearer token or an Entra-issued OAuth
    token [37][38].
- **Okta** deprovisions with `active=false` and does not send a delete when a
  deactivated profile is deleted [41].
- **Rust.** `scim_v2` 1.0.0 (2026-09-20) exists [47]. Whether it provides more
  than types was **unverified**; the endpoint would be hand-written on axum
  either way.

### 6.2 Sync mechanics options

| Option | Latency | Cost |
|---|---|---|
| **T1. Poll reconciler per Forge connection** | Poll interval | Required for Forgejo; the only fully reliable path on GitHub (no retries) and GitLab Free. Fits the existing discovery and poll loops and the `forge_rate_budgets` (`crates/yg-control/migrations/0016_forge_rate_budgets.sql`). |
| **T2. T1 plus webhooks** | Seconds, when delivered | Extra receivers (GitHub `membership`/`organization`/`team`; GitLab Premium group hooks or self-managed system hooks). Webhooks only trigger a re-poll of the team, never apply a delta blindly, matching RFC 0001 §3. |
| **T3. Login-time claims** | Changes apply at next login | Free for GitLab (`groups_direct`), Forgejo (`groups`), Okta and Entra (`groups`, with overage). Nothing for GitHub or Google. Stale until next login, and never sees removals of users who don't log in. *Inference:* fit only as a supplement. |
| **T4. SCIM inbound** | IdP cycle | New authenticated endpoint and bearer-token lifecycle. Syncs IdP groups, not Forge teams; Entra won't do nested groups. The fastest deprovisioning signal for a user (`active=false`). |

*Worked budget (derived, not from a source):* a GitHub org with 200 teams of
≤100 members each costs 200 requests per full pass at `per_page=100`. Every 15
minutes that is 800 requests/h, inside a 5,000/h user or installation budget,
and most of it becomes free 304s when `If-None-Match` is used [15][23]. The
binding limit is the secondary 900 points/min if a pass is not spread out [15].

Semantics Yggdrasil has to fix whichever source it uses:

- **Nesting.** Choose direct-only or inherited membership. GitHub REST gives
  inherited; GraphQL `IMMEDIATE` gives direct. GitLab `/members` gives direct;
  `/members/all` adds inherited and shared. Forgejo is always flat.
- **Pending invitations.** GitHub `pending` and GitLab `awaiting` are not
  members.
- **Team identity.** Key Teams by the Forge's numeric team or group id (GitHub
  `id`/`databaseId`, GitLab group `id`, Forgejo team `id`), not by slug or path.
  GitHub `team.edited` and GitLab `group_rename` show those change.
- **Roles.** GitHub maintainer, GitLab access level and Forgejo team permission
  could feed review routing. Whether they map to anything is a design question.
- **Disagreements.** When an IdP group and a Forge team disagree, one source
  must be named authoritative per Team, or Teams can mix sources and take the
  union.

## 7. Identity mapping

### 7.1 Entities and keys

| Identity | Stable key | Source of truth |
|---|---|---|
| OIDC subject | `(iss, sub)` [1 §5.7]; for Entra prefer `(tid, oid)` because `sub` is pairwise per app ID [35] | IdP |
| Forge account | `(forge_id, numeric user id)`; GitHub `login` is renameable and reusable [21][22]; GitLab `sub` and Forgejo `sub` are the numeric user id [27][44] | Forge |
| Member | Yggdrasil id; human or agent; holds grants, tokens and factors | Yggdrasil (Admin acts) |
| Contributor | Yggdrasil id, merged from `identities(kind email\|forge_account, value)` (RFC 0001 §2) | Derived from history and Forge artifacts; merges tagged and reversible (PRD Risks) |

Ways to link a Member to a Forge account (and so to Teams and Contributors):

- **M1. Forge login is the identity** (L2). Proof of ownership is built in.
- **M2. Explicit link flow.** After IdP login, the human completes an OAuth
  login with the Forge, and Yggdrasil stores the Forge user id.
  - *Cost:* one more OAuth client per Forge connection. Proves control at link
    time only.
- **M3. IdP-side linkage.** GitHub SAML `externalIdentities` (NameID to login)
  [26], GitLab `group_saml_identity.extern_uid` [29], EMU usernames derived from
  the IdP [25]. These are authoritative where they exist.
  - *Cost:* owner-level Forge credentials and an Enterprise or Premium tier.
- **M4. Match by email.** Ruled out by the sources:
  - OIDC "MUST NOT" use `email` as a unique identifier [1 §5.7].
  - Entra's `email` "isn't guaranteed to be correct and is mutable … Never use
    it for authorization" [35].
  - Google says "Don't use the `email` field as a unique identifier" [33].
  - GitLab only returns `email` for users with a public email [27].
  - At most it could *suggest* a link that the human confirms with M2.

Member to Contributor: *inference:* link through the Member's verified Forge
account to the Contributor identity of `kind = forge_account`, never through
commit email. A commit email is a claim anyone can write into git.

### 7.2 Hard edge cases

1. **Renamed and reclaimed GitHub login.** "After changing your username, your
   old username becomes available for anyone else to claim" [22]. Every stored
   reference, webhook payload and team list must be keyed by numeric id.
   Mapping by login hands the old person's Member to whoever registers the
   name.
2. **Reassigned email.** Entra warns that an email can be "given … to a
   previous, no longer present employee" [35]. Email-based linking would give
   the new hire the old employee's Member, grants and private Memories.
3. **Entra pairwise `sub`.** Re-registering the app or adding a second client
   ID changes every `sub` [35]. Keyed on `sub`, all links break; keyed on
   `oid`/`tid`, they survive. Guests get a different `oid` per tenant and
   "should [be treated] as if they're a brand new user" [35].
4. **Google personal accounts.** Without an `hd` check, any Gmail account can
   log in. The `hd` request parameter is only a UI hint [33].
5. **The IdP offboards but the Forge account lives on.** A personal GitHub
   account stays in org teams until someone removes it. If Team membership comes
   from the Forge but login comes from the IdP, the person cannot log in yet
   still appears in Teams. This matters for review routing (proposals routed to
   a departed reviewer) and for anything Team visibility grants to outstanding
   tokens.
6. **The Forge offboards but the IdP lives on.** The reverse: removal from the
   GitHub org but not the IdP. With the Forge as the source, the person loses
   Team membership at the next poll and keeps Member access.
7. **Live credentials after deprovisioning.** A web session and a `ygm_` CLI
   token outlive an IdP disable unless something revokes them. Signals are
   back-channel logout [2], SCIM `active=false` [37][41] or an expiry on every
   human token (`expires_at` exists in 0017).
8. **One person, several Forge accounts** (work and personal, or GitHub plus
   GitLab). One Member has many Forge links. Team membership is the union across
   links, and Contributor merges must not fuse two people who share a machine
   email.
9. **One Forge account claimed by two Members.** This needs a uniqueness
   constraint on `(forge_id, forge_user_id)`, and a takeover policy when a
   second Member proves ownership.
10. **Forge users who never log in.** Team lists contain people with no Member.
    They can be ignored, held as unlinked placeholders that a later login
    claims, or pre-created as pending Members (E3).
11. **Agent Members in Teams.** Agents have no OIDC or Forge identity. Team
    membership for an agent, needed for Memory scope and review routing, must be
    assigned by hand or inherited from a sponsoring human. Inheritance would
    cross the agent/human line that RFC 0001 §8 draws.
12. **Bots and Apps in Forge teams.** GitHub machine users and GitLab bot users
    appear in member lists. A sync must not mint human Members for them.
13. **Restore gate.** Forge links, Team memberships, sessions and enrolments
    restored from a snapshot can predate a removal. *Inference:* Team membership
    used as a visibility boundary must be re-fetched from the Forge before
    Member traffic reopens, as the RFC already requires for repo admission. A
    Forge that cannot be reached should fail closed for the Teams it sources.
14. **Fail-open vs. fail-closed on sync errors.** A Forge outage or rate-limit
    exhaustion means stale membership. For visibility, stale means over-sharing
    on removals; for review routing, it means stale reviewers. These two Team
    powers may want different failure modes.
15. **Hidden and secret teams.** GitHub: "the team must be visible to the
    authenticated user" [14]. Forgejo `groups` can be limited to public
    memberships [44]. The sync credential's own visibility bounds what syncs.
    Using an Admin's personal token makes sync depend on that one person.
16. **Restructures and transfers.** A GitLab subgroup transfer fires no removal
    event [30], and a GitHub parent-team change may not appear in
    `team.edited` (**unverified**). Only a full re-poll catches inherited
    membership shifts.

## 8. Option bundles and their costs

These are illustrative combinations, not recommendations.

| Bundle | Login | CLI | Step-up | Team sync | Main costs |
|---|---|---|---|---|---|
| **A. Minimal** | L1 (OIDC + GitHub OAuth) | C1 (Admin-issued) plus C3 later | S1 (browser hand-off, `webauthn-rs`) + S3 (TOTP) | T1 poll | Web-only passkeys; manual Forge links (M2); poll latency |
| **B. Forge-native** | L2 | C3 | S1 | T1 + T3 (claims) + T2 webhooks | No IdP offboarding gate; per-Forge login if several Forges |
| **C. Enterprise** | L1 | C3 | S1 with attested keys for Admins + S2 for key users | T1 + T2 + T4 SCIM | SCIM endpoint and tokens; IdP-vs-Forge authority rules; most code |

Pieces every bundle shares:

- **Tables.** The RFC's `members` table (not yet built); OIDC identities;
  Forge-account links; WebAuthn credentials and ceremony state; TOTP seeds,
  encrypted; sessions.
- **Restore gate.** All of the above are authority-revision entries for the gate
  (RFC 0001 §5.1).
- **Device grant.** An RFC 8628 endpoint, if C3 is chosen.

## 9. Unverified and open

- `openidconnect` device-grant call shape (`exchange_device_code`), and whether
  `oauth2` 5's bundled reqwest version duplicates the repo's reqwest 0.13.4 (a
  custom `AsyncHttpClient` would avoid it).
- Okta's group-count limit in the `groups` claim (not in [39]).
- Entra's incremental provisioning interval for non-gallery SCIM apps (the
  40-minute figure is not in [37] or [38]).
- Whether GitHub `team.edited` reports parent changes, and whether `membership`
  fires for inherited child-team membership.
- GitLab webhook retry and auto-disable behaviour.
- Forgejo/Codeberg API rate limits.
- Which IdPs send OIDC Back-Channel Logout.
- Google Workspace group membership (needs the Admin SDK or Cloud Identity
  Groups API; not checked).
- The upstream-claim pass-through of brokers (Dex, Keycloak, Authentik, Kanidm).
- How far `scim_v2` 1.0.0 goes beyond types.

## Sources

Specifications

1. OpenID Connect Core 1.0 — §2 (`sub`, `acr`, `amr`), §3.1.2.1 (`max_age`), §5.7 (claim stability), §8 (subject types). https://openid.net/specs/openid-connect-core-1_0.html
2. OpenID Connect Back-Channel Logout 1.0 (Final, errata 1, 2023-12-15). https://openid.net/specs/openid-connect-backchannel-1_0.html
3. RFC 8628, OAuth 2.0 Device Authorization Grant. https://www.rfc-editor.org/rfc/rfc8628.html
4. RFC 8252, OAuth 2.0 for Native Apps. https://www.rfc-editor.org/rfc/rfc8252.html
5. RFC 9700, Best Current Practice for OAuth 2.0 Security. https://www.rfc-editor.org/rfc/rfc9700.html
6. RFC 9470, OAuth 2.0 Step Up Authentication Challenge Protocol. https://www.rfc-editor.org/rfc/rfc9470.html
7. RFC 8176, Authentication Method Reference Values. https://www.rfc-editor.org/rfc/rfc8176.html
8. RFC 6238, TOTP. https://www.rfc-editor.org/rfc/rfc6238.html
9. RFC 7643, SCIM Core Schema. https://www.rfc-editor.org/rfc/rfc7643.html
10. RFC 7644, SCIM Protocol. https://www.rfc-editor.org/rfc/rfc7644.html
11. W3C Web Authentication Level 3 (Recommendation, 2026-08-25). https://www.w3.org/TR/webauthn-3/

GitHub

12. Authorizing OAuth apps. https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps
13. Generating a user access token for a GitHub App. https://docs.github.com/en/apps/creating-github-apps/authenticating-with-a-github-app/generating-a-user-access-token-for-a-github-app
14. REST: Team members. https://docs.github.com/en/rest/teams/members
15. Rate limits for the REST API. https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api
16. Rate limits and query limits for the GraphQL API. https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api
17. GraphQL reference: Teams. https://docs.github.com/en/graphql/reference/teams
18. Webhook events and payloads. https://docs.github.com/en/webhooks/webhook-events-and-payloads
19. Handling failed webhook deliveries. https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries
20. Redelivering webhooks. https://docs.github.com/en/webhooks/testing-and-troubleshooting-webhooks/redelivering-webhooks
21. REST: Users. https://docs.github.com/en/rest/users/users
22. Username changes. https://docs.github.com/en/account-and-profile/concepts/username-changes
23. Best practices for using the REST API. https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api
24. Managing team synchronization for your organization. https://docs.github.com/en/enterprise-cloud@latest/organizations/managing-saml-single-sign-on-for-your-organization/managing-team-synchronization-for-your-organization
25. About Enterprise Managed Users. https://docs.github.com/en/enterprise-cloud@latest/admin/managing-iam/understanding-iam-for-enterprises/about-enterprise-managed-users
26. Troubleshooting identity and access management for your organization (externalIdentities query). https://docs.github.com/en/enterprise-cloud@latest/organizations/managing-saml-single-sign-on-for-your-organization/troubleshooting-identity-and-access-management-for-your-organization

GitLab

27. GitLab as an OpenID Connect identity provider. https://docs.gitlab.com/integration/openid_connect_provider/
28. OAuth 2.0 identity provider API. https://docs.gitlab.com/api/oauth2/
29. Group and project members API. https://docs.gitlab.com/api/members/
30. Webhook events. https://docs.gitlab.com/user/project/integrations/webhook_events/
31. System hooks. https://docs.gitlab.com/administration/system_hooks/
32. GitLab.com-specific rate limits. https://docs.gitlab.com/user/gitlab_com/rate_limits/

Google, Microsoft, Okta

33. Google: OpenID Connect. https://developers.google.com/identity/openid-connect/openid-connect
34. Google: OAuth 2.0 for TV and limited-input device applications. https://developers.google.com/identity/protocols/oauth2/limited-input-device
35. Entra: ID token claims reference. https://learn.microsoft.com/en-us/entra/identity-platform/id-token-claims-reference
36. Entra: OAuth 2.0 device authorization grant. https://learn.microsoft.com/en-us/entra/identity-platform/v2-oauth2-device-code
37. Entra: How application provisioning works. https://learn.microsoft.com/en-us/entra/identity/app-provisioning/how-provisioning-works
38. Entra: Develop a SCIM endpoint. https://learn.microsoft.com/en-us/entra/identity/app-provisioning/use-scim-to-provision-users-and-groups
39. Okta: Customize tokens with a groups claim. https://developer.okta.com/docs/guides/customize-tokens-groups-claim/main/
40. Okta: Device Authorization grant. https://developer.okta.com/docs/guides/device-authorization-grant/main/
41. Okta: Understanding SCIM. https://developer.okta.com/docs/concepts/scim/

Forgejo / Codeberg

42. Forgejo: OAuth2 provider. https://forgejo.org/docs/latest/user/oauth2-provider/
43. Forgejo source: `modules/webhook/type.go`. https://codeberg.org/forgejo/forgejo/src/branch/forgejo/modules/webhook/type.go
44. Forgejo source: `routers/web/auth/oauth.go` (`getOAuthGroupsForUser`, ID token `Subject`). https://codeberg.org/forgejo/forgejo/src/branch/forgejo/routers/web/auth/oauth.go
45. Codeberg OpenAPI spec (`/orgs/{org}/teams`, `/teams/{id}/members`, `Team` and `User` schemas). https://codeberg.org/swagger.v1.json

Libraries and guidance

46. OWASP Session Management Cheat Sheet. https://cheatsheetseries.owasp.org/cheatsheets/Session_Management_Cheat_Sheet.html
47. crates.io API, queried 2026-11-23: `openidconnect` 4.1.0, `oauth2` 5.0.0, `webauthn-rs` 0.6.2, `webauthn-authenticator-rs` 0.6.2, `passkey` 0.6.0, `ctap-hid-fido2` 3.6.1, `totp-rs` 6.0.0, `tower-sessions` 0.15.0, `tower-sessions-sqlx-store` 0.16.0 (sqlx ^0.9.0), `axum-login` 0.19.0 (axum ^0.8.1), `scim_v2` 1.0.0, `jsonwebtoken` 12.0.0. https://crates.io/api/v1/crates/{name}
48. webauthn-rs: docs (https://docs.rs/webauthn-rs/latest/webauthn_rs/, https://docs.rs/webauthn-rs/latest/webauthn_rs/struct.Webauthn.html), README and `webauthn-rs/src/lib.rs` at https://github.com/kanidm/webauthn-rs
49. passkey-rs README. https://github.com/1Password/passkey-rs
50. openidconnect docs. https://docs.rs/openidconnect/latest/openidconnect/
51. axum-login docs (https://docs.rs/axum-login/latest/axum_login/) and `axum-login/src/session.rs` (`cycle_id`, `flush`) at https://github.com/maxcountryman/axum-login
