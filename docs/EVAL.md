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
  evidence that proves it: the enclosing declarations and line ranges. A
  scenario passes when the answer meets its recall and precision thresholds,
  stated per scenario.
- **`grep_lines_bytes`**: what the scenario's literal `grep -rn` prints
  (`path:line:text`), which is what a grep-and-read agent actually sees.
- **`followup_read_bytes`**: computed mechanically, the same way for both
  arms: the bytes of the gold evidence ranges that are not already present
  verbatim in what that arm has received. `total_bytes` is everything the arm
  received, every page included, plus its follow-up bytes. `verb_calls`
  counts every page.
- **Tokens**: the budget invariant is measured on the MCP rendering with
  ADR 0011's cost model. Model-exact tokens are counted with the provider's
  token-counting endpoint when an API key is present; bytes stay the CI
  metric for totals.
- **Mid-size tier**: pinned repos from the Shard profile's corpus
  (`docs/research/shard-profile/corpus.tsv` on `research/shard-profile`). The
  four-file fixture stays as the small tier.
- **Scenarios**: callers on a common name (where grep is noisy) and on a
  unique name (where grep is exact), hub `neighbors`, `impact`, a cross-repo
  question, locate definition, and the `node` Symbol card. A hidden-repo
  variant of each runs too (below).

### Tier 1: deterministic, in CI

- Every scenario meets its gold thresholds.
- Every response is within its budget, measured as above: a hard invariant,
  not a percentile.
- Each scenario renders byte for byte the same with and without an extra
  hidden repo that has edges into its results (ADR 0011, ADR 0008).
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
- On relationship tasks, the Verb arm uses at least 30% fewer input tokens
  and at least 20% fewer tool calls, at equal or better accuracy.
- On exact-string tasks, it is no more than 10% worse on tokens and tool
  calls, and its accuracy is no more than 5 points lower.
- Each comparison is reported with a paired bootstrap 95% confidence
  interval, and a target counts as met only when the interval clears it.
- The corpus and harness are shared with Ask's eval. This tier is not run in
  CI.
