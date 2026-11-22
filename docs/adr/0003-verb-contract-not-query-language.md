Status: extended by ADR-0007 (adds the Memory Verbs remember, recall, promote, retract, and one transport-parity exception for human step-up acts over MCP)

# The query contract is a verb set, not a query language

The public API is a curated set of graph verbs (search, map, node, neighbors, path, callers, callees, deps, dependents, owners, history, impact) exposed identically as CLI subcommands, MCP tools, and HTTP endpoints. We deliberately expose no graph query language: agents write unreliable Cypher against schemas they don't know, a freeform-query MCP tool is a footgun, and a public query dialect would weld us to whatever database speaks it. Bounded verbs keep storage swappable and every query optimizable. If verbs prove insufficient, an escape hatch can be added later — the reverse migration (retiring a public query language) would be impossible.
