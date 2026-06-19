---
name: yggdrasil-navigation
description: Navigate an organization codebase through yggdrasil's Knowledge Graph before falling back to local file reads.
---

# Yggdrasil Navigation

Use this Skill when you need to orient in a repository or answer questions about
code structure, ownership, history, dependencies, or forge artifacts from a
yggdrasil Index Server.

## Server/Verb version

Written for yggdrasil M0 and the Verb contract in RFC 0001 §7.

Available in M0: `search`, `node`, `neighbors`, and `history`.

Expected in M1: `map`, `callers`, `callees`, and `impact`.

## Knowledge Graph vs reading files

Use the Knowledge Graph first for org-truth:

- finding Symbols, Doc Sections, and files across synced repositories
- following edges between Symbols, files, Dependencies, Contributors, and docs
- checking history and ownership context before editing
- narrowing a large codebase before opening local files

Read files with local tools when you need working-tree truth:

- uncommitted edits, generated files, and local branches
- exact formatting or full source needed for an edit
- build output, test output, or runtime state from the current checkout

## Division of truth

Yggdrasil answers org-truth at the default branch as synced by the Index Server.
The agent's local tools own working-tree truth, including uncommitted edits and
the active branch.

When the two disagree, explain the split: "the Knowledge Graph shows default
branch state; the working tree currently differs here."

## Search-first orientation

Start with `search` for the concept, Symbol, file, or phrase. The map Verb arrives in M1; until then, search is the orientation step.

Good first searches:

- a Symbol or type name mentioned by the user
- a product term from a Doc Section or issue
- an error string, route, command, or configuration key
- a repository qualifier plus a focused term when the organization is large

After search returns a likely node, use `node` for its edge summary, then
`neighbors` to traverse the nearby graph.

## Provenance trust rules

Every edge carries Provenance and confidence.

- Trust `precise` edges for compiler-grade call/reference answers.
- Treat `syntactic` edges as strong orientation, then verify before making
  high-risk edits.
- Use `extracted` edges for deterministic non-code facts such as manifests,
  Forge Artifacts, and identity matches.
- Treat `inferred` edges as hypotheses and seek confirmation.

Precise answers can still be stale if the Index Server has not synced the
latest forge state. Syntactic answers can be incomplete or ambiguous. Prefer
answers that cite node ids, edge kinds, Provenance, and the local files you
verified afterward.

## Verb cookbook

Use `search` when you need a starting point.

```sh
yg search "rate limit" --repo github.com/acme/widgets --json
```

Use `node` when you have an id and need the node's summary.

```sh
yg node file:github.com/acme/widgets:src/main.rs --json
```

Use `neighbors` when you need connected Symbols, Doc Sections, Dependencies, or
ownership context.

```sh
yg neighbors sym:github.com/acme/widgets:src/main.rs#handle --depth 2 --json
```

Use `history` when the question depends on change timing or contributors.

```sh
yg history file:github.com/acme/widgets:src/main.rs --since 2026-01-01 --json
```

For M1 and later, use `map` before `search` when entering an unfamiliar repo.

```sh
yg map github.com/acme/widgets --json
```

## Failure etiquette

When the graph thins out, say so and fall back gracefully.

- If `search` misses, broaden the query or remove filters once.
- If `node` cannot find an id, re-run `search` and check whether the repo is
  synced.
- If `neighbors` returns few edges, inspect Provenance and confidence before
  drawing conclusions.
- If the needed Verb is not available in this server version, name the missing
  Verb and use the nearest available route.
- If local files disagree with graph results, report default-branch graph truth
  separately from working-tree truth.

Do not pretend graph silence proves absence. Say what was checked, what the
Index Server could answer, and what you verified locally.
