//! On-demand, compose-backed orientation-efficiency evaluation.
//!
//! Run with the command documented in `docs/EVAL.md`. The ignored test emits
//! one JSON array containing one typed record per fixed scenario.

mod common;

use common::{Harness, TEST_TOKEN, git};
use serde::Serialize;
use serde_json::{Value, json};

const FIXTURE: &[(&str, &str)] = &[
    ("go.mod", "module example.com/orientation\n\ngo 1.22\n"),
    (
        "internal/report/render.go",
        r#"package report

import (
	"fmt"
	"strings"
)

// RenderReport formats a title and its ordered lines for terminal output.
func RenderReport(title string, lines []string) string {
	return fmt.Sprintf("%s: %s", title, strings.Join(lines, ", "))
}
"#,
    ),
    (
        "summary.go",
        r#"package orientation

import "example.com/orientation/internal/report"

// BuildSummary renders the completed work for a task.
func BuildSummary(items []string) string {
	return report.RenderReport("completed", items)
}
"#,
    ),
    (
        "preview.go",
        r#"package orientation

import "example.com/orientation/internal/report"

// Preview renders a bounded sample before the full task is run.
func Preview(items []string) string {
	if len(items) > 2 {
		items = items[:2]
	}
	return report.RenderReport("preview", items)
}
"#,
    ),
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    FindCallers,
    LocateDefinition,
    SummarizeModuleDependencies,
}

#[derive(Debug, Serialize)]
struct EvalRecord {
    scenario: Scenario,
    verb_bytes: usize,
    verb_calls: usize,
    baseline_bytes: usize,
    correct: bool,
}

struct MeasuredResponse {
    body: Value,
    bytes: usize,
}

#[tokio::test]
#[ignore = "requires the dev compose stack; run via docs/EVAL.md"]
async fn orientation_efficiency() {
    let harness = Harness::boot_with(FIXTURE).await;
    harness.add_repo().await;
    harness.sync_and_index().await;

    let records = vec![
        find_callers(&harness).await,
        locate_definition(&harness).await,
        summarize_module_dependencies(&harness).await,
    ];

    emit_records(&records);
    assert!(
        records.iter().all(|record| record.correct),
        "every fixed scenario must return the expected graph facts"
    );
}

fn emit_records(records: &[EvalRecord]) {
    if let Some(path) = std::env::var_os("YG_EVAL_OUTPUT") {
        let file = std::fs::File::create(&path).unwrap_or_else(|error| {
            panic!("create eval output {}: {error}", path.to_string_lossy())
        });
        serde_json::to_writer(file, records).expect("eval records serialize to the output file");
    } else {
        println!(
            "{}",
            serde_json::to_string(records).expect("eval records serialize")
        );
    }
}

async fn find_callers(harness: &Harness) -> EvalRecord {
    let search = verb(
        harness,
        "search",
        json!({"query": "RenderReport", "kinds": ["Symbol"]}),
    )
    .await;
    let id = hit_id(&search.body, "RenderReport", "internal/report/render.go");
    let neighbors = verb(
        harness,
        "neighbors",
        json!({"id": id, "direction": "in", "edge_kinds": ["CALLS"]}),
    )
    .await;
    let names = node_names(&neighbors.body);

    EvalRecord {
        scenario: Scenario::FindCallers,
        verb_bytes: search.bytes + neighbors.bytes,
        verb_calls: 2,
        baseline_bytes: baseline_bytes(&harness.repo_dir, |path, source| {
            path.ends_with(".go") && source.contains("RenderReport(")
        }),
        correct: names.contains(&"BuildSummary") && names.contains(&"Preview"),
    }
}

async fn locate_definition(harness: &Harness) -> EvalRecord {
    let search = verb(
        harness,
        "search",
        json!({"query": "RenderReport", "kinds": ["Symbol"]}),
    )
    .await;
    let id = hit_id(&search.body, "RenderReport", "internal/report/render.go");
    let node = verb(harness, "node", json!({"id": id})).await;
    let resolved = &node.body["node"];

    EvalRecord {
        scenario: Scenario::LocateDefinition,
        verb_bytes: search.bytes + node.bytes,
        verb_calls: 2,
        baseline_bytes: baseline_bytes(&harness.repo_dir, |_, source| {
            source.contains("func RenderReport(")
        }),
        correct: resolved["name"] == "RenderReport"
            && resolved["path"] == "internal/report/render.go",
    }
}

async fn summarize_module_dependencies(harness: &Harness) -> EvalRecord {
    let search = verb(
        harness,
        "search",
        json!({"query": "fmt", "kinds": ["File"]}),
    )
    .await;
    let id = hit_id(&search.body, "render.go", "internal/report/render.go");
    let neighbors = verb(
        harness,
        "neighbors",
        json!({"id": id, "direction": "out", "edge_kinds": ["IMPORTS"]}),
    )
    .await;
    let names = node_names(&neighbors.body);

    EvalRecord {
        scenario: Scenario::SummarizeModuleDependencies,
        verb_bytes: search.bytes + neighbors.bytes,
        verb_calls: 2,
        baseline_bytes: baseline_bytes(&harness.repo_dir, |path, source| {
            path.starts_with("internal/report/") && source.contains("import (")
        }),
        correct: names.contains(&"fmt") && names.contains(&"strings"),
    }
}

async fn verb(harness: &Harness, name: &str, request: Value) -> MeasuredResponse {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/verbs/{name}", harness.base))
        .bearer_auth(TEST_TOKEN)
        .json(&request)
        .send()
        .await
        .expect("Verb request succeeds");
    let status = response.status();
    let bytes = response.bytes().await.expect("Verb body is readable");
    assert!(
        status.is_success(),
        "Verb {name} returned {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let body = serde_json::from_slice(&bytes).expect("Verb body is JSON");
    MeasuredResponse {
        body,
        bytes: bytes.len(),
    }
}

fn hit_id<'a>(body: &'a Value, name: &str, path: &str) -> &'a str {
    body["hits"]
        .as_array()
        .expect("search response has hits")
        .iter()
        .find(|hit| hit["name"] == name && hit["path"] == path)
        .and_then(|hit| hit["id"].as_str())
        .unwrap_or_else(|| panic!("search did not find {name} at {path}: {body}"))
}

fn node_names(body: &Value) -> Vec<&str> {
    body["nodes"]
        .as_array()
        .expect("neighbors response has nodes")
        .iter()
        .filter_map(|node| node["name"].as_str())
        .collect()
}

fn baseline_bytes(root: &std::path::Path, matches: impl Fn(&str, &str) -> bool) -> usize {
    let mut total = 0;
    for path in git(root, &["ls-files"]).lines() {
        let bytes = std::fs::read(root.join(path)).expect("tracked fixture file is readable");
        let source = std::str::from_utf8(&bytes).expect("fixture source is UTF-8");
        if matches(path, source) {
            total += bytes.len();
        }
    }
    total
}
