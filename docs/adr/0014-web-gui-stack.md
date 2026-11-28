# The web GUI is a Next.js backend-for-frontend behind the Rust server, with ceremonies on their own origin

Status: accepted. Amends [ADR 0005](0005-object-storage-native-shards.md) (the minimum deployment gains a web image and a second hostname), [ADR 0008](0008-team-granted-repository-visibility.md) (how the GUI surface is authenticated), [ADR 0009](0009-human-login-agent-binding-forge-sync.md) (where ceremonies run) and [ADR 0013](0013-pr-impact-report.md) (the report route for anonymous readers and rebuilds), and records how the web GUI meets [ADR 0003](0003-verb-contract-not-query-language.md) and [ADR 0010](0010-ask-agent-and-chat-adapters.md).

The web GUI has four surfaces (admin console, graph browser, Memory workspace, web Ask), hosts every identity ceremony ADR 0009 defines, and shows the full PR impact report. The Index Server is a Rust axum server whose Verbs must stay identical across CLI, MCP and REST, and whose request and response types already derive `schemars` schemas. The project's frontend default is Next.js with tRPC and Tailwind; tRPC needs a TypeScript server. This ADR keeps that default for the product surfaces and confines the TypeScript server to presentation. Identity, consent, sessions and authorisation stay in Rust; the Node server never holds a credential that outlives a request, and no code it serves can obtain a passkey signature.

## The decisions

### Ceremonies run on their own origin, served only by Rust

A deployment has two canonical HTTPS origins, both served by Rust:

- the **app origin** (for example `yg.example`), where Rust proxies the Next.js product surfaces and serves REST, MCP and webhooks;
- the **ceremony origin** (for example `auth.yg.example`), which Rust serves alone and never proxies.

The ceremony origin's host is the WebAuthn relying-party id, and the app origin's host is neither that host nor a subdomain of it. A browser lets a page request a passkey signature only for its own domain or a parent of it, so no script on the app origin (a compromised `yg-web`, a malicious npm dependency, ordinary XSS) can ever obtain a signature. Every ceremony runs there: OIDC login and passkey assertion, passkey enrolment, enrolment and recovery links, step-up confirmation (ADR 0009's browser hand-off), device-grant approval, and the confirmation steps of Chat-account Binding and held-capture acceptance (ADR 0010). Links in notices, chat and email point at it directly.

Ceremony pages are server-rendered HTML from Rust templates, styled with the GUI's Tailwind design tokens. The ceremony origin has its own host-only `__Host-` cookie, accepts only top-level navigations and form posts that carry a token from the page that issued them, refuses service-worker scripts, and sends a strict CSP (`script-src 'self'`, one hand-written script for the WebAuthn call), `frame-ancestors 'none'`, `Cross-Origin-Opener-Policy: same-origin`, `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. Rust supplies every detail the human approves. Login, step-up, device grants, bootstrap and recovery work while `yg-web` is down.

The two origins share one `member_tokens` session (ADR 0009):

- **Login.** After OIDC and a passkey assertion on the ceremony origin, Rust issues a single-use code, valid for about a minute and bound to that ceremony session, and redirects to the app origin's redemption route, where Rust exchanges it for the app origin's `__Host-` session cookie (`Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`). An enrolment-only session (ADR 0009) never leaves the ceremony origin: no code is issued until a passkey has been asserted.
- **Step-up.** A GUI action creates the pending act through Rust; the browser navigates top-level to the ceremony origin, which shows the act from server state, takes the passkey touch, completes the act, and redirects back to a validated app-origin path.
- **Logout and rotation** apply to both cookies; logging out on the app origin also ends the ceremony session.

Only Rust sets, rotates or clears either cookie. On the app origin the session cookie is read only by Rust's front door; REST (`/v1`) and MCP accept bearer tokens only. Requests the GUI marks as background (refetches) and stream heartbeats do not count as activity for ADR 0009's idle timeout.

### A Next.js server is a presentation-only backend-for-frontend

The product surfaces are a Next.js application (App Router) running as its own server. Its tRPC procedures call the Rust API and may compose several calls for one page and shape the results into view models. They hold no authorisation, visibility, budget or business rule: every such decision is made by Rust on each call. Server Actions are disabled; tRPC is the only path from the browser to the BFF, queries are side-effect free, and every mutation is a tRPC POST. The BFF keeps no module-level cache of anything derived from Rust and builds its API client per request.

The contract is generated, never hand-written. Rust publishes OpenAPI documents generated from the same typed definitions behind REST, MCP and the CLI (an axum OpenAPI layer such as `utoipa` or `aide`, over the existing `schemars` types): a public document for `/v1`, and an internal one for the endpoints only the BFF may call, which also declares, for each tRPC mutation, the Rust write operations it may invoke. Stream events are typed Rust enums whose schemas appear in the documents. `yg openapi` emits both. A TypeScript client is generated from them at build time, and CI regenerates it and fails on any difference.

### Rust is the front door, and the BFF never sees the session

- **Origins.** Rust uses the configured origins, never a request's `Host`, for WebAuthn, OIDC redirect URIs, hand-off URLs and origin checks, and trusts forwarding headers only from configured ingress addresses.
- **Routing on the app origin.** Rust serves the session-redemption and logout routes, `/healthz`, REST (`/v1/*`), MCP and Forge webhooks itself; those namespaces are never proxied, even for paths that do not exist there. Each route class has its own authentication: bearer tokens for REST and MCP, per-Forge verification for webhooks, the session for proxied GUI paths. Versioned static build assets are proxied without a session (no image optimiser or other dynamic handler is exposed). Every other path is proxied only for a fully authenticated session; a navigation without one gets the same redirect to the ceremony origin's login whatever the path, carrying a return path that Rust later accepts only if it is a same-origin path.
- **Links from other sites.** A `SameSite=Strict` cookie is not sent on navigations from another site, such as a report link on a Forge or a citation in Slack. For a top-level document navigation to a GUI page route that arrives cross-site without a cookie, Rust returns a data-free page that re-navigates to the same URL from the same site. The bounce never applies to `/api/trpc`, streams or any Rust route.
- **What crosses to Node.** Before proxying, Rust validates the session, removes `Cookie`, `Authorization` and every inbound forwarding or framework-internal header (such as `x-middleware-subrequest`), and adds an assertion it signs with a key held only in that node's memory. The assertion names the node, the request, the Member, the session and its generation, the client address and user agent the node observed, and the Member's current effective-view epoch, and carries one capability:
  - **read**, for every page render, query and stream;
  - **write**, only for a tRPC mutation (a POST whose `Origin` is the app origin, or, without `Origin`, whose `Sec-Fetch-Site` is same-origin and `Sec-Fetch-Mode` is not navigate; a request with neither is refused), limited to the Rust write operations the internal contract declares for that mutation, each usable once.

  The assertion is valid only while its request is open on the node that issued it, and at most a minute.
- **What comes back.** Rust passes through only an allow-list of response headers from `yg-web` (never `Set-Cookie` or `Service-Worker-Allowed`), refuses service-worker script requests, stamps `Cache-Control: no-store` on every response other than versioned static assets, sends `Referrer-Policy: same-origin`, adds the effective-view epoch, and sets the CSP: it mints a per-request nonce, passes it to Next.js for the scripts and styles Next.js emits, and allows workers from its own origin. Shiki emits classes, not inline styles; inline `style` attributes, which component libraries use for positioning and which cannot run script, are allowed (`style-src-attr 'unsafe-inline'`).
- **Isolation.** Agents and the CLI never pass through Node. Proxying to `yg-web` uses its own bounded connection pool, and streams a separate one with caps per Member and per session and their own timeouts, so a stalled `yg-web` or a flood of streams cannot exhaust the API's capacity. When `yg-web` is unavailable, Rust serves a plain "GUI unavailable" page and never replays a mutation whose outcome is unknown. Readiness reports the core server and the GUI separately.

A compromised `yg-web` can therefore act only inside requests Members are making: during a request it can read anything in that Member's effective view (including an active Break-glass Session) and render or exfiltrate it, and during a validated mutation it can perform only the write operations that mutation declares. It cannot replay a session, act after a request ends, obtain a passkey signature or approve a ceremony.

### The BFF authenticates as the GUI surface on an internal listener

The BFF calls Rust only on an internal listener, never the public one, and only on the node that issued the request's assertion: each node's internal address comes from the deployment's configuration, never from input, and calls never follow a redirect with credentials attached. That node knows when the request ends, so it refuses the assertion afterwards; if the node is gone, the call fails. Each call carries the assertion and a GUI-surface credential. The credential is a deployment secret outside `member_tokens` (like the cursor secret), provisioned at deploy and rotatable with an overlap window; an Admin can revoke it at once, without overlap, through a flag every node checks within seconds, which also ends open streams. It authenticates the surface; it is not a reader ceiling. A request through the GUI is the signed-in Member's own query, as ADR 0008 says of the full impact report, so Rust applies the Member's effective view and nothing else. The public listener refuses the credential, and the internal listener refuses a call without both a valid credential and a live assertion; the one exception is a credential-only compatibility endpoint that returns the server's contract hash.

Both hops between Rust and Node (Rust proxying to `yg-web`, and `yg-web` calling the internal listener) run over TLS when they cross hosts, or Unix sockets with file permissions on one host, and `yg-web` is reachable only from Rust. Mutual TLS can be added later without changing the model.

Every internal call carries the generated client's contract hash, and Rust refuses a mismatch with a typed error. `yg-web` uses the compatibility endpoint for readiness. `yg` and `yg-web` are upgraded together; while a rolling upgrade mixes versions, a request whose proxying node and `yg-web` disagree gets an "upgrading" page instead of a mismatched call. A browser holding an older build is told to reload.

### Rendering never shares Member data

Pages are React Server Components rendered per request, calling Rust through tRPC's server-side caller with the request's assertion. Every route that touches Member data is dynamic and every fetch to Rust is `no-store`; Rust's `no-store` header covers HTML, React payloads and tRPC responses for every intermediary. A lint rule enforces the route settings, and CI renders pages for two Members with different views, sequentially and interleaved, and asserts neither sees the other's data. Rust remains the only layer that caches derived results, keyed by effective-view fingerprint (ADR 0008).

In the browser, tRPC with TanStack Query drives interactive views, and link prefetching is off for routes that show Member data. Client state is bound to the session and the view. When a response carries a different effective-view epoch (a Team grant removed, a Break-glass Session ended) or the session-ended response (logout, expiry, revocation, suspension, recovery), the client clears TanStack Query and the router cache and tells every other tab through a broadcast channel; logout also sends `Clear-Site-Data`. Before showing retained data again after history navigation or a tab returning to view, the client checks the current epoch with Rust.

### Streams

Ask answers and long report computations stream through tRPC subscriptions over server-sent events, relayed from Rust. A stream is started by a mutation that returns a single-use stream id bound to the session; the subscription attaches to that id, and a reconnect re-attaches to the same work rather than starting it again. Streams are read-only, carry heartbeats, cancel the work upstream when the browser leaves, and end when the session ends or the effective-view epoch changes. A report stream for `/cr` passes ADR 0013's visibility check before sending headers, progress or job ids.

Web Ask keeps ADR 0010's separation. Starting a question is the mutation above: Rust mints a grant for that one question, bound to that request, and forwards the question to the Ask role itself; the BFF never holds the grant, and the session and assertion never reach the Ask role. Only progress events and answer content that has passed ADR 0010's citation and link checks cross the relay, never raw model tokens.

### Toolkit

- Tailwind with shadcn/ui components (Radix primitives, copied into the repo); the Rust ceremony pages share the same design tokens.
- The graph browser renders with Sigma.js on graphology (WebGL), with layout in a web worker, so ADR 0011's neighbourhoods and hubs stay responsive. Every graph view has an equivalent list view, because a canvas alone is not accessible.
- Code snippets and line text are highlighted with Shiki on the server, per request.
- The target is WCAG 2.2 AA, checked with axe in CI on both the Next.js pages and the Rust ceremony pages, which are fully usable by keyboard and screen reader.

### Repo layout and tests

The GUI lives in this repo as a `web/` pnpm workspace: `web/app` (the Next.js app), `web/api-client` (generated from `yg openapi`) and `web/ui` (shared components, if they outgrow the app); the ceremony templates live with the Rust server. CI adds lint, typecheck and Vitest unit tests, the OpenAPI drift check, axe checks, the `yg-web` image build, and Playwright end-to-end tests against the real compose stack (`yg` on both origins, `yg-web`, Postgres, RustFS). The end-to-end suite covers every ceremony with the browser's virtual WebAuthn authenticator and a mock OIDC provider; that a script on the app origin cannot request a signature for the ceremony origin; the login code hand-off; the cross-site bounce; the two-Member caching tests and the view-epoch purge; header and cookie stripping at the proxy; refusal of the GUI credential on the public listener, of a write with a read assertion, of a write outside its mutation's declared operations, and of an assertion after its request ended; the contract mismatch; and login and step-up with `yg-web` stopped.

## Considered options

- **A Next.js static export embedded in the `yg` binary, with a generated client and no tRPC.** One binary and no Node at runtime, but no server rendering, an awkward fallback for dynamic routes, and a departure from the tRPC default.
- **A Rust-rendered UI for everything (askama with htmx, or Leptos in WASM).** One language, but a much smaller UI ecosystem for the graph browser and Memory workspace, and a lower bar on UI quality. Rust renders only the ceremony pages.
- **Next.js rendering the ceremony pages.** One UI codebase, but a compromised GUI build could show one act while a passkey signs another, and nothing could be approved while `yg-web` is down.
- **Rust-rendered ceremony pages on the app origin.** Any script on the app origin shares the relying-party id, so it could fetch a pending act's challenge and request the signature itself; hardening narrows this but cannot close it.
- **A separate origin only for Admin step-ups.** It protects the highest-stakes acts but leaves Member acts exposed and doubles the ceremony paths.
- **A Rust-served confirmation frame inside Next.js pages.** WebAuthn in frames needs permissions-policy work, and a compromised parent can still overlay the frame.
- **Forwarding the session cookie to the BFF.** A compromised BFF could replay a session for its whole lifetime.
- **Next.js owning login (Auth.js), or a BFF-side session mapped to stored Rust tokens.** ADR 0009's rules implemented twice, and a credential store in the BFF.
- **Next.js as the front door, or an external ingress routing paths.** The first puts Node in the path of every agent and MCP stream; the second makes every self-hosting Admin maintain routing rules whose mistakes can expose ceremony endpoints.
- **Hand-written TypeScript types, or the BFF calling Rust over MCP.** The first drifts silently; the second puts admin and ceremony endpoints into the agent protocol.
- **Server rendering with Next.js caching keyed per session.** One missed cache key would leak one Member's view to another.
- **React Flow, or a full component framework (Mantine, MUI).** React Flow slows past a few hundred DOM nodes, where hubs live; a full framework's styling system competes with Tailwind.
- **Mutual TLS required now, or network placement alone for BFF trust.** mTLS asks every Admin to run a certificate authority; network placement alone lets anything on the network act as the GUI surface.
- **A separate repo, or end-to-end tests against a mocked API.** The first allows version skew and splits the drift check; the second hides exactly the ceremony, proxy and caching behaviour where a BFF fails dangerously.

## Consequences

- Every deployment runs two application images and serves two hostnames, each with DNS and TLS. The reference compose file and the Helm chart gain `yg-web` and the ceremony hostname; the Rust server gains the ceremony templates, the proxy with its assertion, header, cache and CSP rules, the internal listener and `yg openapi`. RFC 0001 §8's single authentication rule becomes the per-route-class rules above.
- The OpenAPI documents become part of the contract alongside the Verb table: adding an endpoint means its typed definition, and the generated client follows.
- The ADR 0013 report route redirects a reader without a session to login like every GUI path, and keeps its identical not-found for signed-in Members who cannot see the repo or the CR; rebuilding a non-resident head is an explicit action on the page, never a side effect of opening it.
- Where the GUI lands in the milestones, and which surfaces ship first, is part of milestone resequencing; the page inventory is the information-architecture ticket's.
