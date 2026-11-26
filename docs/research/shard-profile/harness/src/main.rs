//! Per-pass timing harness: replays the index worker's `prepare` stages
//! (crates/yg-index/src/worker.rs) on one bare mirror and commit, timing each
//! stage, and publishes the Shard to a local-filesystem object store so its
//! segments can be inspected. Prints one JSON object on stdout.
//!
//! Usage: shard-profile-harness <bare-mirror> <commit-sha> <out-dir>
//!
//! Deviations from the worker, all forced by visibility:
//! - `extract_tree` is private: the same `git archive <sha>^{tree} | tar -x`
//!   pipeline is re-run here as a blocking shell pipeline.
//! - `merge_history` is `pub(crate)`: its 15-line body is copied verbatim.
//! - `extract_history_for_commit` is `pub(crate)`: the public
//!   `extract_history` wrapper (same body) is timed instead; `git log` alone
//!   is timed separately with the worker's exact arguments.
//! - The segment build split (graph vs FTS) is not observable inside
//!   `prepare_shard`; `build_fts` is timed on its own over a copy of the
//!   search docs, and graph build is reported as the difference (estimate).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use serde_json::{Value, json};
use yg_shard::{Edge, EdgeKind, Graph, NodeKind, SearchDoc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, mirror, commit, out] = args.as_slice() else {
        bail!("usage: shard-profile-harness <bare-mirror> <commit-sha> <out-dir>");
    };
    let report = profile(Path::new(mirror), commit, &PathBuf::from(out)).await?;
    println!("{report}");
    Ok(())
}

async fn profile(mirror: &Path, commit: &str, out: &Path) -> anyhow::Result<Value> {
    let tree = out.join("tree");
    let store_root = out.join("store");
    std::fs::create_dir_all(&tree)?;
    std::fs::create_dir_all(&store_root)?;
    let mut t = Timings::default();

    let start = Instant::now();
    extract_tree(mirror, commit, &tree)?;
    t.record("extract_tree", start);

    let start = Instant::now();
    let log_bytes = git_log_bytes(mirror, commit)?;
    t.record("git_log_only", start);

    let start = Instant::now();
    let history = yg_index::extract_history(mirror, commit).await?;
    t.record("history_walk", start);
    let history_counts = (history.nodes.len(), history.edges.len());

    let start = Instant::now();
    let (mut graph, search_docs) = {
        let dest = tree.clone();
        tokio::task::spawn_blocking(move || yg_index::syntactic_pass(&dest)).await??
    };
    t.record("syntactic_pass", start);
    let syntactic_counts = (graph.nodes.len(), graph.edges.len());

    let start = Instant::now();
    merge_history(&mut graph, history);
    t.record("merge_history", start);

    let kinds = kind_breakdown(&graph);
    let doc_stats = doc_breakdown(&search_docs);

    let docs_copy = copy_docs(&search_docs);
    let start = Instant::now();
    let fts_alone = tokio::task::spawn_blocking(move || yg_shard::build_fts(&docs_copy)).await??;
    t.record("build_fts_alone", start);

    let start = Instant::now();
    let prepared = yg_shard::prepare_shard(graph, search_docs).await?;
    t.record("prepare_shard", start);

    let store: Arc<dyn object_store::ObjectStore> = Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(&store_root)
            .context("opening the local object store")?,
    );
    let start = Instant::now();
    let published = yg_shard::publish_shard(store.as_ref(), 1, commit, prepared).await?;
    t.record("write_local_fs", start);

    let segments = segment_sizes(&store_root, &published.manifest_key)?;
    Ok(json!({
        "commit": commit,
        "timings_s": t.0,
        "graph_build_estimate_s": t.get("prepare_shard") - t.get("build_fts_alone"),
        "git_log_bytes": log_bytes,
        "fts_alone_bytes": fts_alone.len(),
        "history_nodes": history_counts.0,
        "history_edges_before_prune": history_counts.1,
        "syntactic_nodes": syntactic_counts.0,
        "syntactic_edges": syntactic_counts.1,
        "nodes": published.node_count,
        "edges": published.edge_count,
        "kinds": kinds,
        "search_docs": doc_stats,
        "segments": segments,
    }))
}

#[derive(Default)]
struct Timings(BTreeMap<&'static str, f64>);

impl Timings {
    fn record(&mut self, stage: &'static str, start: Instant) {
        self.0.insert(stage, start.elapsed().as_secs_f64());
    }
    fn get(&self, stage: &str) -> f64 {
        self.0.get(stage).copied().unwrap_or_default()
    }
}

/// Same pipeline as the worker's private `extract_tree_pipeline`.
fn extract_tree(mirror: &Path, commit: &str, dest: &Path) -> anyhow::Result<()> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(r#"git --git-dir "$1" archive "$2^{tree}" | tar -x -f - -C "$3""#)
        .args(["sh", &mirror.to_string_lossy(), commit, &dest.to_string_lossy()])
        .env("LC_ALL", "C")
        .status()?;
    if !status.success() {
        bail!("git archive | tar failed: {status}");
    }
    Ok(())
}

/// The worker's exact `git log` invocation (history.rs `git_log_history`).
fn git_log_bytes(mirror: &Path, commit: &str) -> anyhow::Result<usize> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(mirror)
        .args(["-c", "core.quotePath=false"])
        .args(["log", commit, "--no-renames", "--name-only", "-z"])
        .arg("--pretty=format:%H%x00%ct%x00%an%x00%ae%x00%s%x00")
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() {
        bail!("git log failed");
    }
    Ok(out.stdout.len())
}

/// Verbatim copy of `yg_index::history::merge_history` (pub(crate)).
fn merge_history(graph: &mut Graph, history: Graph) {
    let file_ids: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::File)
        .map(|n| n.id.as_str())
        .collect();
    let kept_edges: Vec<Edge> = history
        .edges
        .into_iter()
        .filter(|e| e.kind != EdgeKind::Touches || file_ids.contains(e.dst.as_str()))
        .collect();
    drop(file_ids);
    graph.nodes.extend(history.nodes);
    graph.edges.extend(kept_edges);
}

fn kind_breakdown(graph: &Graph) -> Value {
    let mut nodes: BTreeMap<&str, u64> = BTreeMap::new();
    for node in &graph.nodes {
        *nodes.entry(node.kind.as_str()).or_default() += 1;
    }
    let mut edges: BTreeMap<&str, u64> = BTreeMap::new();
    for edge in &graph.edges {
        *edges.entry(edge.kind.as_str()).or_default() += 1;
    }
    json!({ "nodes": nodes, "edges": edges })
}

fn doc_breakdown(docs: &[SearchDoc]) -> Value {
    let content_bytes: usize = docs.iter().map(|d| d.content.len()).sum();
    let files = docs.iter().filter(|d| d.kind == NodeKind::File).count();
    json!({ "count": docs.len(), "file_docs": files, "content_bytes": content_bytes })
}

fn copy_docs(docs: &[SearchDoc]) -> Vec<SearchDoc> {
    docs.iter()
        .map(|d| SearchDoc {
            node_id: d.node_id.clone(),
            kind: d.kind,
            name: d.name.clone(),
            path: d.path.clone(),
            content: d.content.clone(),
        })
        .collect()
}

fn segment_sizes(store_root: &Path, manifest_key: &str) -> anyhow::Result<Value> {
    let manifest = store_root.join(manifest_key);
    let dir = manifest.parent().context("manifest has a parent dir")?;
    let mut sizes = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        sizes.insert(
            entry.file_name().to_string_lossy().into_owned(),
            entry.metadata()?.len(),
        );
    }
    Ok(json!({ "dir": dir, "bytes": sizes }))
}
