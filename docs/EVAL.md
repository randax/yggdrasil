# Orientation-efficiency evaluation

Yggdrasil's navigation claim is that an agent can orient through bounded Verb
responses instead of placing whole source files in its context. Payload
efficiency is not answer quality, so this evaluation reports both byte counts
and fixed correctness checks.

## Response-size metric

`/metrics` exposes
`yggdrasil_verb_response_size_bytes_{bucket,sum,count}` with the typed `verb`
label (`node`, `neighbors`, `search`, or `history`). Every label is registered
at process startup, including zero-observation series.

An observation is the compact JSON byte length of a successful typed Verb
payload after the engine has formed its final response DTO. This is the common
payload that both REST and MCP consume. JSON object key sorting changes order,
not length, so it is also the REST response-body length. The measurement
excludes HTTP status and headers, transport-shaped error bodies, JSON-RPC and
MCP tool-result envelopes, and MCP's duplication of the payload as text and
structured content. Those transport costs are excluded because they are not
comparable per Verb in MCP batches. Dividing bytes by four is a rough token
estimate only; it is not emitted as a second metric.

## Fixed scenarios

The ignored `orientation_efficiency` integration test creates and indexes the
same small Go repository on every run, then evaluates:

1. `find_callers`: `search` for `RenderReport`, then inbound `CALLS`
   `neighbors`; both `BuildSummary` and `Preview` must appear.
2. `locate_definition`: `search` for `RenderReport`, then `node`; the name and
   `internal/report/render.go` path must match.
3. `summarize_module_dependencies`: `search` for the file containing `fmt`,
   then outbound `IMPORTS` `neighbors`; both `fmt` and `strings` must appear.

For every scenario the harness emits `verb_bytes`, `verb_calls`,
`baseline_bytes`, and `correct` in one JSON array. `verb_bytes` is the sum of
the raw REST response bodies read before JSON parsing. The baseline models a
simple grep-plus-read workflow: find tracked fixture files with a scenario's
literal match, deduplicate them, then count each matched file's full bytes
once. It excludes grep's own output and request overhead:

- callers: every Go file containing `RenderReport(`;
- definition: every file containing `func RenderReport(`;
- dependencies: files under `internal/report/` containing `import (`.

This baseline is intentionally small and auditable, not a claim about every
possible grep strategy. Verb payloads include the fixture's absolute temporary
repository path in node IDs, so their totals can vary slightly with the host's
temporary-directory path length; baseline file bytes do not vary.

## Run

Start the same services used by the other end-to-end suites, then run only the
ignored target with output capture disabled:

```sh
docker compose up -d --wait postgres rustfs
docker compose run --rm rustfs-init
YG_EVAL_OUTPUT=target/orientation-efficiency.json \
  RUSTC_WRAPPER= cargo test -p yg-cli --test orientation_efficiency --locked -- --ignored
```

`target/orientation-efficiency.json` is the standalone machine-readable JSON
array. If `YG_EVAL_OUTPUT` is omitted, the test prints the same array to stdout;
add `--nocapture` to the test arguments to see it among Cargo's test-runner
output. The test fails if any `correct` value is false. It deliberately does
not fail merely because a Verb payload is larger than its baseline: that
discrepancy is an economic finding, not an answer-quality failure.

## Recorded baseline (2026-07-17, dev compose stack)

| scenario | verb bytes | calls | baseline bytes | correct | Verb payload smaller? |
| --- | ---: | ---: | ---: | :---: | :---: |
| `find_callers` | 1395 | 2 | 729 | yes | no |
| `locate_definition` | 694 | 2 | 244 | yes | no |
| `summarize_module_dependencies` | 1472 | 2 | 244 | yes | no |

**Finding: no scenario used less response payload than the file-read
baseline on this fixture.** Recorded per the acceptance criteria rather
than hidden. Two structural reasons, both expected to invert on
realistic corpora, and both now measurable:

- The fixture repository is a handful of files totalling a few hundred
  bytes, so "read the matching files" is nearly free. The baseline cost
  scales with file size and match count; the Verb response is bounded
  regardless of how large the matched files grow.
- Every node id embeds the repository qualifier (here a long temporary
  checkout path), which dominates these small payloads.

Regressions to watch: `verb_bytes` growing across releases for the same
scenarios, or `verb_calls` increasing. Re-run the command above and
compare against this table.

## Targets (ADR 0011)

These are the decided targets for the reworked yardstick. The harness changes
below land with ADR 0011's payload changes, at the start of M1. The
percentages are opening targets, to be tightened after the first baseline run.

### Yardstick

- **Gold answers**: for the mid-size tier, each scenario's expected answer
  comes from a precise resolver (SCIP, or `go/types` for Go), with the
  evidence that proves it: the enclosing declarations and line ranges.
- **Answer keys**, fixed per scenario and applied to both arms: distinct
  caller (or callee) ids for "who calls this?"; `(revision, path, line,
  column)` for "where is it called?"; the precise resolver's edge set for the
  seed in hub `neighbors`; the precise transitive caller set to the stated
  depth for `impact`; and each Symbol card section the scenario requests,
  scored separately, with `history` graded against the commits Git reports
  at the pinned revision for the `history` Verb's scope. Each
  arm's output is mapped to keys and deduplicated; evidence-span coverage is
  scored separately as `followup_read_bytes`.
- **The answer an arm gives** is defined the same way for both arms: its
  results are consumed in the order returned, page by page, until recall over
  the answer key reaches 0.9 or the results run out. Everything received up
  to that point counts toward `total_bytes` and `verb_calls`. Grep-and-read is
  held to the same recall floor.
- **The grep arm's procedure** is fixed per scenario and uses only bytes
  that arm received: for a multi-step question such as `impact`, it greps the
  target name, reads the enclosing declarations of each match, then greps
  each enclosing name it found, to the scenario's depth. Every search and
  read is charged. Gold only scores the answer and decides when to stop.
- **Staged gates.** A scenario is enabled when its Verb and the Provenance it
  depends on exist. Recall 0.9 gates from then on. Precision at the stopping
  point is reported from the start and becomes a gate (at least 0.5) once
  precise edges exist for that scenario's language. The common-name callers
  scenario is a method call with at least 20 same-named candidates, so the
  ambiguity rules are always exercised.
- **`grep_lines_bytes`**: what the scenario's literal `grep -rn` prints
  (`path:line:text`), which is what a grep-and-read agent actually sees.
- **`followup_read_bytes`**: computed mechanically, the same way for both
  arms. Coverage is keyed by pinned repo revision, path and source byte
  interval, and only the intersection of delivered spans with gold spans is
  credited. The follow-up is the union of the uncovered gold bytes per file,
  so an identical line from another file never counts. `total_bytes` is
  everything the arm received plus its follow-up bytes.
- **Tokens**: the budget is enforced with ADR 0011's cost model and calibrated
  against the MCP rendering, counted with a pinned offline tokenizer.
  Model-exact tokens are counted with the provider's token-counting endpoint
  when an API key is present; bytes stay the CI metric for totals.
- **Mid-size tier**: pinned repos from the Shard profile's corpus
  (`docs/research/shard-profile/corpus.tsv` on `research/shard-profile`). The
  four-file fixture stays as the small tier.
- **Scenarios**: callers on a common name (where grep is noisy) and on a
  unique name (where grep is exact), hub `neighbors`, `impact`, a cross-repo
  question, locate definition, and the `node` Symbol card. A hidden-repo
  variant of each runs too (below).

### Tier 1: deterministic, in CI

- Every enabled scenario passes its gates as staged above.
- Every response is within its budget by the cost model, as a hard invariant,
  and its MCP rendering is within 1.1× the budget by the pinned tokenizer.
- Each scenario renders byte for byte the same, cursor handles replaced by a
  placeholder, with and without an extra hidden repo that has edges into its
  results, and returns the same items across all pages (ADR 0011, ADR 0008).
  One such scenario's hidden edges alone exceed the traversal work cap.
- Every id in every response, used as input with its stated repo context,
  resolves to the same node.
- Every paged scenario's cursor is drained to exhaustion, and the ordered
  sequence of items, before any deduplication, equals an independently
  enumerated expected sequence: nothing lost, nothing repeated.
- On relationship scenarios at the mid-size tier (callers, hub `neighbors`,
  `impact`, cross-repo, the Symbol card), the Verb arm's `total_bytes` is no
  more than grep-and-read's.
- On exact-string scenarios (locate definition, callers on a unique name),
  the Verb arm's `total_bytes` is at most 1.5× grep's, since grep is built for
  exactly that.

### Tier 2: agent in the loop, at release

- A fixed model runs natural-language tasks on the profile corpus, at least
  20 relationship tasks and at least 20 exact-string tasks, in two arms: grep
  and read only, and Verbs plus grep and read. Each task runs 5 times.
- A task's answer is graded against gold citations, as in Ask's eval
  (ADR 0010). Input tokens are the cumulative uncached input across the
  task's turns.
- On relationship tasks, the Verb arm uses at least 30% fewer input tokens and
  at least 20% fewer tool calls, and is non-inferior on accuracy: no more than
  5 points lower.
- On exact-string tasks, it is no more than 10% worse on tokens and tool
  calls, and again no more than 5 points lower on accuracy.
- Each comparison is a paired bootstrap 95% confidence interval that resamples
  tasks, keeping each task's runs together. Costs are compared as the ratio of
  the Verb arm's mean to the grep arm's, and a cost target is met when the
  interval's upper bound is within it: at most 0.70 for relationship tokens,
  0.80 for relationship tool calls, and 1.10 for exact-string tokens and tool
  calls. An accuracy target is met when the interval's lower bound on
  Verb-minus-grep accuracy is above −5 points.
- The corpus and harness are shared with Ask's eval. This tier is not run in
  CI.
