# Org-visible access model for the Index Server

The Index Server is deployed per organization; an Admin chooses which repos get indexed, and every Member — human or agent — can query everything indexed. We deliberately do not mirror forge-side repo ACLs at query time — that requires per-user identity mapping and permission sync across three forges (Sourcegraph-grade complexity) for little v1 value. Sensitive repos are kept private by not indexing them; revisit when per-user identity is introduced.

Repo discovery reinforces this: connected forge orgs are auto-discovered and auto-indexed, but private repos are explicit opt-in (discovered-but-not-indexed until an Admin includes them by rule), so a newly created private repo can never silently become org-visible.

Discovery rule precedence is deterministic. Rules are glob patterns over repo slugs (`org/repo`); the longest matching pattern wins, and the newest rule wins ties. Public and internal repos default to `included`; private repos default to `discovered` and ignore rules unless that rule explicitly applies to private repos, so private indexing always requires an Admin's private opt-in rule.
