# Org-visible access model for the Index Server

The Index Server is deployed per organization; an admin chooses which repos get indexed, and every authenticated user/agent can query everything indexed. We deliberately do not mirror forge-side repo ACLs at query time — that requires per-user identity mapping and permission sync across three forges (Sourcegraph-grade complexity) for little v1 value. Sensitive repos are kept private by not indexing them; revisit when per-user identity is introduced.

Repo discovery reinforces this: connected forge orgs are auto-discovered and auto-indexed, but private repos are explicit opt-in (discovered-but-not-indexed until an Admin includes them by rule), so a newly created private repo can never silently become org-visible.
