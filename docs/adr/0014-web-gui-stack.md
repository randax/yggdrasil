# The web GUI is a Next.js backend-for-frontend behind the Rust server, which keeps every ceremony

Status: accepted. Amends [ADR 0005](0005-object-storage-native-shards.md) (the minimum deployment gains a web image), [ADR 0008](0008-team-granted-repository-visibility.md) (how the GUI surface is authenticated), [ADR 0009](0009-human-login-agent-binding-forge-sync.md) (where ceremony pages render) and [ADR 0013](0013-pr-impact-report.md) (the report route for anonymous readers and rebuilds), and records how the web GUI meets [ADR 0003](0003-verb-contract-not-query-language.md) and [ADR 0010](0010-ask-agent-and-chat-adapters.md).

The web GUI has four surfaces (admin console, graph browser, Memory workspace, web Ask), hosts every identity ceremony ADR 0009 defines, and shows the full PR impact report. The Index Server is a Rust axum server whose Verbs must stay identical across CLI, MCP and REST, and whose request and response types already derive `schemars` schemas. The project's frontend default is Next.js with tRPC and Tailwind; tRPC needs a TypeScript server. This ADR keeps that default for the product surfaces and confines the TypeScript server to presentation. Identity, consent, sessions and authorisation stay implemented once, in Rust, and the Node server never holds a credential that outlives a request.

## The decisions

### Rust renders the ceremony pages itself

Rust serves a small set of pages directly, never through Next.js, under `/auth/*`: login and passkey assertion, passkey enrolment, enrolment and recovery links, step-up confirmation (ADR 0009's browser hand-off), device-grant approval, logout, and the confirmation steps of Chat-account Binding and held-capture acceptance (ADR 0010). They are plain server-rendered HTML from Rust templates, styled with the same Tailwind design tokens, with their own strict CSP (`script-src 'self'`, one hand-written script for the WebAuthn call, `frame-ancestors 'none'`) and no Next.js code. Rust supplies every detail the human approves, so a compromised Node server or npm dependency can never show one act while a passkey signs another. Login, step-up, device grants, bootstrap and recovery keep working when `yg-web` is down. Everything else is rendered by Next.js.

OIDC and the GitHub adapter, `webauthn-rs`, step-up, device grants, enrolment and recovery, and `member_tokens` sessions stay in the Index Server exactly as ADR 0009 specifies. Only Rust sets, rotates or clears the `__Host-` session cookie (`Secure`, `HttpOnly`, `SameSite=Strict`, `Path=/`, host-only), and only on `/auth/*` responses. The cookie is accepted only by `/auth/*` and by the front door's proxy; REST (`/v1`) and MCP accept bearer tokens only. Requests the GUI marks as background (refetches) and stream heartbeats do not count as activity for ADR 0009's idle timeout.

### A Next.js server is a presentation-only backend-for-frontend

The product surfaces are a Next.js application (App Router) running as its own server. Its tRPC procedures call the Rust API and may compose several calls for one page and shape the results into view models. They hold no authorisation, visibility, budget or business rule: every such decision is made by Rust on each call. Server Actions are disabled; tRPC is the only path from the browser to the BFF, queries are side-effect free, and every mutation is a tRPC POST. The BFF keeps no module-level cache of anything derived from Rust and builds its API client per request.

The contract is generated, never hand-written. Rust publishes OpenAPI documents generated from the same typed definitions behind REST, MCP and the CLI (an axum OpenAPI layer such as `utoipa` or `aide`, over the existing `schemars` types): a public document for `/v1`, and an internal one for the endpoints only the BFF may call. Stream events are typed Rust enums whose schemas appear in the documents. `yg openapi` emits both. A TypeScript client is generated from them at build time, and CI regenerates it and fails on any difference.

### Rust is the front door, and the BFF never sees the session

- **One origin.** A deployment configures one canonical external HTTPS origin and its WebAuthn relying-party id. Rust uses that origin, never a request's `Host`, for WebAuthn, OIDC redirect URIs, hand-off URLs and origin checks, and trusts forwarding headers only from configured ingress addresses.
- **Routing.** Rust serves `/auth/*`, `/healthz`, REST (`/v1/*`), MCP and Forge webhooks itself; those namespaces are never proxied, even for paths that do not exist there. Static build assets and the login shell are proxied to Next.js without a session. Any other path is proxied only for a valid session; a navigation without one gets the same redirect to login whatever the path, so the redirect reveals nothing about what exists.
- **Links from other sites.** A `SameSite=Strict` cookie is not sent on navigations from another site, such as a report link on a Forge or a citation in Slack. Rust answers a cross-site navigation that arrives without a cookie with a data-free page that re-navigates to the same URL from the same site, which then carries the cookie.
- **What crosses to Node.** Before proxying, Rust validates the session cookie, removes the `Cookie` and `Authorization` headers and every inbound forwarding or framework-internal header (such as `x-middleware-subrequest`), and adds a short-lived assertion it signs. The assertion is bound to that one request: its id, the Member, the session and its generation, the client address and user agent Rust observed, and an operation class. The class is read-only unless the request is an unsafe method or a stream start that passed Rust's origin check (`Origin` and `Sec-Fetch-Site` same-origin), in which case it may also carry writes. The assertion expires when its request ends, or within a minute.
- **What comes back.** Rust strips any `Set-Cookie` from `yg-web` responses, stamps `Cache-Control: no-store` on every proxied response other than versioned static assets, and sets the CSP: it mints a per-request nonce, passes it to Next.js on the proxied request for the scripts and styles Next.js emits, and allows workers from its own origin.
- **Agents and the CLI never pass through Node.** Requests from `yg-web` are proxied over a separate, bounded connection pool with timeouts, so a stalled `yg-web` cannot exhaust capacity for the API; when `yg-web` is unavailable, Rust serves a plain "GUI unavailable" page and never replays a mutation whose outcome is unknown. Readiness reports the core server and the GUI separately.

A compromised `yg-web` can therefore act only within requests Members are making, with the authority Rust granted each one, and only until each request ends; it can render or exfiltrate what those requests return, which is the residual risk of running a presentation server, but it cannot replay a session, approve a ceremony or write without a validated request.

### The BFF authenticates as the GUI surface on an internal listener

The BFF calls Rust only on a second listener reachable from the internal network, never on the public one. Each call carries the request's assertion and a GUI-surface credential. The credential is a deployment secret outside `member_tokens` (like the cursor secret), provisioned at deploy, rotatable with an overlap window, and revocable at once without overlap if compromised, which also ends open streams. It authenticates the surface; it is not a reader ceiling. A request through the GUI is the signed-in Member's own query, as ADR 0008 says of the full impact report, so Rust applies the Member's effective view and nothing else. The public listener refuses the credential, and the internal listener refuses calls without both a valid credential and a live assertion. Calls go only to the configured Rust upstream, never to a destination taken from input, and never follow a redirect with credentials attached. The internal hop runs over TLS when it crosses hosts, or a Unix socket with file permissions when both run on one host. Mutual TLS can be added later without changing the model.

Every internal call carries the contract hash of the generated client, and Rust refuses a mismatch with a typed error. `yg-web` reports itself not ready until its contract matches the server's, so a rolling upgrade or a partial rollback serves an "upgrading" page instead of mismatched calls; `yg` is upgraded first. A browser holding an older build is told to reload.

### Rendering never shares Member data

Pages are React Server Components rendered per request, calling Rust through tRPC's server-side caller with the request's assertion. Every route that touches Member data is dynamic and every fetch to Rust is `no-store`; Rust's `no-store` header covers HTML, React payloads and tRPC responses for every intermediary. A lint rule enforces the route settings, and CI renders pages for two Members with different views, sequentially and interleaved, and asserts neither sees the other's data. Rust remains the only layer that caches derived results, keyed by effective-view fingerprint (ADR 0008).

In the browser, tRPC with TanStack Query drives interactive views, and link prefetching is off for routes that show Member data. Client state is bound to the session: when any request returns the session-ended response (logout, expiry, revocation, suspension, recovery), the client clears TanStack Query and the router cache and tells every other tab through a broadcast channel; logout also sends `Clear-Site-Data`. After a change of identity or view, cached data is refetched before it is shown again.

### Streams

Ask answers and long report computations stream through tRPC subscriptions over server-sent events, relayed from Rust. Streams carry heartbeats; closing the browser cancels the work upstream; Rust ends a stream when its session ends or the Member's effective view changes, and a report stream for `/cr` passes ADR 0013's visibility check before sending headers, progress or job ids.

Web Ask keeps ADR 0010's separation. The BFF asks the Rust API to mint the short-lived per-session Ask grant for the signed-in Member, and the question goes to the Ask role with that grant; the session and its assertion never reach the Ask role. Only progress events and answer content that has passed ADR 0010's citation and link checks cross the relay, never raw model tokens.

### Toolkit

- Tailwind with shadcn/ui components (Radix primitives, copied into the repo); the Rust ceremony pages share the same design tokens.
- The graph browser renders with Sigma.js on graphology (WebGL), with layout in a web worker, so ADR 0011's neighbourhoods and hubs stay responsive. Every graph view has an equivalent list view, because a canvas alone is not accessible.
- Code snippets and line text are highlighted with Shiki on the server, per request.
- The target is WCAG 2.2 AA, checked with axe in CI on both the Next.js pages and the Rust ceremony pages, which are fully usable by keyboard and screen reader.

### Repo layout and tests

The GUI lives in this repo as a `web/` pnpm workspace: `web/app` (the Next.js app), `web/api-client` (generated from `yg openapi`) and `web/ui` (shared components, if they outgrow the app); the ceremony templates live with the Rust server. CI adds lint, typecheck and Vitest unit tests, the OpenAPI drift check, axe checks, the `yg-web` image build, and Playwright end-to-end tests against the real compose stack (`yg`, `yg-web`, Postgres, RustFS). The end-to-end suite covers every ceremony with the browser's virtual WebAuthn authenticator and a mock OIDC provider, the cross-site link bounce, the two-Member caching tests, cookie and header stripping at the proxy, refusal of the GUI credential on the public listener, refusal of a write with a read-only assertion, the contract mismatch, and login and step-up with `yg-web` stopped.

## Considered options

- **A Next.js static export embedded in the `yg` binary, with a generated client and no tRPC.** One binary and no Node at runtime, but no server rendering, an awkward fallback for dynamic routes, and a departure from the tRPC default.
- **A Rust-rendered UI for everything (askama with htmx, or Leptos in WASM).** One language, but a much smaller UI ecosystem for the graph browser and Memory workspace, and a lower bar on UI quality. Rust renders only the ceremony pages.
- **Next.js rendering the ceremony pages.** One UI codebase, but a compromised GUI build could show one act while a passkey signs another, and nothing could be approved while `yg-web` is down.
- **A Rust-served confirmation frame inside Next.js pages.** It looks unified, but WebAuthn in frames needs permissions-policy work and a compromised parent can still overlay the frame.
- **Forwarding the session cookie to the BFF.** Simpler, but a compromised BFF could replay a session for its whole lifetime.
- **Next.js owning login (Auth.js), or a BFF-side session mapped to stored Rust tokens.** ADR 0009's rules implemented twice, and a credential store in the BFF.
- **Next.js as the front door, or an external ingress routing paths.** The first puts Node in the path of every agent and MCP stream; the second makes every self-hosting Admin maintain routing rules whose mistakes can expose ceremony endpoints.
- **Hand-written TypeScript types, or the BFF calling Rust over MCP.** The first drifts silently; the second puts admin and ceremony endpoints into the agent protocol.
- **Server rendering with Next.js caching keyed per session.** One missed cache key would leak one Member's view to another.
- **React Flow, or a full component framework (Mantine, MUI).** React Flow slows past a few hundred DOM nodes, where hubs live; a full framework's styling system competes with Tailwind.
- **Mutual TLS required now, or network placement alone for BFF trust.** mTLS asks every Admin to run a certificate authority; network placement alone lets anything on the network act as the GUI surface.
- **A separate repo, or end-to-end tests against a mocked API.** The first allows version skew and splits the drift check; the second hides exactly the ceremony, proxy and caching behaviour where a BFF fails dangerously.

## Consequences

- Every deployment runs two application images. The reference compose file and the Helm chart gain `yg-web`; the Rust server gains the ceremony templates, the proxy with its assertion, header, cache and CSP rules, the internal listener and `yg openapi`. RFC 0001 §8's rule that every route but `/healthz` needs a token becomes the routing rules above.
- The OpenAPI documents become part of the contract alongside the Verb table: adding an endpoint means its typed definition, and the generated client follows.
- The ADR 0013 report route redirects a reader without a session to login like every GUI path, and keeps its identical not-found for signed-in Members who cannot see the repo or the CR; rebuilding a non-resident head is an explicit action on the page, never a side effect of opening it.
- Where the GUI lands in the milestones, and which surfaces ship first, is part of milestone resequencing; the page inventory is the information-architecture ticket's.
