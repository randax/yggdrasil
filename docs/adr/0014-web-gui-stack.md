# The web GUI is a Next.js backend-for-frontend behind the Rust server, which keeps every ceremony

Status: accepted. Amends [ADR 0005](0005-object-storage-native-shards.md) (the minimum deployment gains a web image) and records how the web GUI meets [ADR 0003](0003-verb-contract-not-query-language.md), [ADR 0008](0008-team-granted-repository-visibility.md) and [ADR 0009](0009-human-login-agent-binding-forge-sync.md).

The web GUI has four surfaces (admin console, graph browser, Memory workspace, web Ask), hosts every identity ceremony ADR 0009 defines, and shows the full PR impact report ([ADR 0013](0013-pr-impact-report.md)). The Index Server is a Rust axum server whose Verbs must stay identical across CLI, MCP and REST, and whose request and response types already derive `schemars` schemas. The project's frontend default is Next.js with tRPC and Tailwind; tRPC needs a TypeScript server. This ADR keeps that default and confines the TypeScript server to presentation, so identity and authorisation stay implemented once, in Rust.

## The decisions

### A Next.js server is a presentation-only backend-for-frontend

The GUI is a Next.js application (App Router) running as its own server. Its tRPC procedures call the Rust API and may compose several calls for one page and shape the results into view models. They hold no authorisation, visibility, budget or business rule: every such decision is made by Rust on each call. A bug in the BFF can therefore render something badly but cannot show something the Member may not see.

The contract is generated, never hand-written. Rust publishes an OpenAPI document generated from the same typed definitions behind REST, MCP and the CLI (an axum OpenAPI layer such as `utoipa` or `aide`, over the existing `schemars` types), covering Verbs, admin endpoints and ceremony endpoints; `yg openapi` emits it. A TypeScript client is generated from it at build time, and CI regenerates it and fails on any difference.

### Rust owns every ceremony and session

OIDC and the GitHub adapter, `webauthn-rs`, step-up hand-offs, device grants, enrolment and recovery links, and `member_tokens` sessions stay in the Index Server, exactly as ADR 0009 specifies. Rust sets the `__Host-` session cookie (`HttpOnly`, `Secure`, `SameSite=Strict`, CSRF-protected). Next.js renders every page, ceremony pages included, but each ceremony's state lives in Rust and every ceremony action is a call to a Rust endpoint.

The BFF stores no Member credential. The browser sends the session cookie with each request; the BFF forwards it on its server-side calls to Rust, so a compromised BFF can act only for Members whose requests are in flight, never with standing access.

### Rust is the front door; Next.js ships as its own image

- One origin, served by Rust. axum serves the ceremony and identity endpoints, REST (`/v1`), MCP and Forge webhooks itself, and reverse-proxies everything else (pages, `/api/trpc`, static assets) to the Next.js server. Security headers, CSP, CSRF checks and rate limits are applied by Rust for every response. Agents and the CLI never pass through Node, so a GUI outage never affects them.
- The Next.js app is built with pnpm in standalone mode as a separate image, `yg-web`, versioned and released in lockstep with `yg`.
- The minimum deployment becomes `yg` (role modes) + `yg-web` + Postgres + an S3-compatible store, amending ADR 0005 and PRD G14. `yg-web` is stateless and can be scaled or restarted independently.

### The BFF proves it is the GUI surface with a credential on an internal listener

Rust opens a second listener, bound to the internal network only, for calls from the BFF. Each call carries an Admin-provisioned GUI-surface credential, stored like other deployment secrets and rotatable with an overlap window, plus the Member's forwarded session cookie. A call is authorised only with both: the credential attests the surface (ADR 0008's adapter credential, here for the GUI, whose ceiling for a signed-in Member is their own effective view), and the session decides who is acting. The public listener refuses the GUI credential. A leaked credential alone reads nothing, and network placement alone attests nothing. Mutual TLS can be added later as an option without changing this model.

### Rendering never shares Member data across requests

Pages are React Server Components rendered per request, calling Rust through tRPC's server-side caller with the forwarded cookie. Every route that touches Member data is dynamic (`force-dynamic`) and every fetch to Rust is `no-store`, so Next.js's data cache and full-route cache never hold Member data; only build artefacts are cached. A lint rule enforces this, and a CI test renders the same pages for two Members with different views and asserts that neither sees the other's data. Rust remains the only layer that caches derived results, keyed by effective-view fingerprint (ADR 0008).

In the browser, tRPC with TanStack Query drives interactive views (graph browsing, Memory editing, admin forms); its cache is per tab and cleared on logout. Ask answers and long report computations stream through tRPC subscriptions over server-sent events, relayed from Rust.

### Toolkit

- Tailwind with shadcn/ui components (Radix primitives, copied into the repo).
- The graph browser renders with Sigma.js on graphology (WebGL), with layout in a web worker, so ADR 0011's neighbourhoods and hubs stay responsive. Every graph view has an equivalent list view, because a canvas alone is not accessible.
- Code snippets and line text are highlighted with Shiki on the server.
- The target is WCAG 2.2 AA, checked with axe in CI. Ceremony pages are fully usable by keyboard and screen reader.

### Repo layout and tests

The GUI lives in this repo as a `web/` pnpm workspace: `web/app` (the Next.js app), `web/api-client` (generated from `yg openapi`) and `web/ui` (shared components, if they outgrow the app). CI adds lint, typecheck and Vitest unit tests, the OpenAPI drift check, axe checks, the `yg-web` image build, and Playwright end-to-end tests against the real compose stack (`yg`, `yg-web`, Postgres, RustFS). The end-to-end suite covers every identity ceremony with the browser's virtual WebAuthn authenticator and a mock OIDC provider, the two-Member caching test, and refusal of the GUI credential on the public listener.

## Considered options

- **A Next.js static export embedded in the `yg` binary, with a generated client and no tRPC.** One binary and no Node at runtime, but no server rendering, an awkward fallback for dynamic routes, and a departure from the tRPC default.
- **A Rust-rendered UI (askama with htmx, or Leptos in WASM).** One language, but a much smaller UI ecosystem for the graph browser and Memory workspace, and a lower bar on UI quality.
- **Next.js owning login (Auth.js) and exchanging for a Rust token.** Idiomatic for Next.js, but ADR 0009's login rules would be implemented twice, identity would split across two servers, and the BFF would become a token store whose compromise gives standing access.
- **A BFF-side session mapped to stored Rust tokens.** A second session system to keep in step with ADR 0009, and a credential store in the BFF.
- **Next.js as the front door, or an external ingress routing paths.** The first puts Node in the path of every agent and MCP stream; the second makes every self-hosting Admin maintain routing rules whose mistakes can expose ceremony endpoints.
- **Hand-written TypeScript types, or the BFF calling Rust over MCP.** The first drifts silently; the second puts admin and ceremony endpoints into the agent protocol and gives the GUI agent-oriented shapes.
- **Server rendering with Next.js caching keyed per session.** One missed cache key would leak one Member's view to another.
- **React Flow, or a full component framework (Mantine, MUI).** React Flow slows past a few hundred DOM nodes, where hubs live; a full framework's styling system competes with Tailwind.
- **Mutual TLS or network placement alone for BFF trust.** mTLS asks every Admin to run a certificate authority; network placement alone lets anything on the network act as the GUI surface.
- **A separate repo, or end-to-end tests against a mocked API.** The first allows version skew and splits the drift check; the second hides exactly the ceremony, forwarding and caching behaviour where a BFF fails dangerously.

## Consequences

- Every deployment runs two application images. The reference compose file and the Helm chart gain `yg-web`, and the Rust server gains the internal listener, the reverse proxy and `yg openapi`.
- The OpenAPI document becomes part of the public contract alongside the Verb table: adding an endpoint means its typed definition, and the generated client follows.
- Where the GUI lands in the milestones, and which surfaces ship first, is part of milestone resequencing; the page inventory is the information-architecture ticket's.
