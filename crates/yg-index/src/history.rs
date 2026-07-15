use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use yg_shard::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance};

/// The git-history layer (RFC 0001 §4): walk every commit reachable from
/// `commit` and distill it into Commit and Contributor nodes plus AUTHORED
/// (Contributor→Commit) and TOUCHES (Commit→File) edges. Contributors are
/// deduped by email within this Shard — cross-Forge identity merge is M2.
///
/// A TOUCHES edge is emitted for *every* path a commit changed, named as
/// the File node it would have (`file:<path>`). The history walk can't see
/// which of those Files the syntactic pass minted nodes for (a since-deleted
/// path, a non-UTF-8 name it skipped), so `merge_history` prunes TOUCHES
/// to absent Files when folding this into the syntactic graph — the read
/// path refuses a graph with edges to nodes it doesn't hold. Commits whose
/// every changed path is gone still become Commit nodes (full history is
/// ingested), exactly as `git log -- <deleted-path>` omits them from a live
/// file's history.
///
/// Every edge is `extracted` provenance at confidence 1.0 (CONTEXT.md):
/// git history is deterministically derived, not heuristically guessed.
///
/// Parsing is NUL-delimited (`git log -z`): commit messages, names, and
/// emails are attacker-controlled by anyone who can push, and NUL is the
/// one byte git forbids in a message or pathname — so a crafted subject
/// can't forge a record, shift a field, or corrupt a path. As a second
/// guard a record whose first field isn't a hex sha is skipped.
///
/// Reads the bare mirror but never mutates it — the caller holds the
/// mirror lock, since a concurrent `--prune` fetch could GC objects
/// mid-walk. `--git-dir`, never `-C`: repository discovery must not climb
/// out of a missing mirror into an enclosing checkout.
pub async fn extract_history(git_dir: &Path, commit: &str) -> anyhow::Result<Graph> {
    let log = git_log_history(git_dir, commit).await?;
    // Walk the log off the runtime threads: every other CPU-bound pass
    // (syntactic_pass, extract_tree, the segment builds in write_shard)
    // runs in spawn_blocking, and a large repo's history is non-trivial to
    // parse — keep it off the async executor.
    tokio::task::spawn_blocking(move || parse_history(&log))
        .await
        .context("history parse task panicked")?
}

/// Parse the NUL-framed `git log -z` output into a history graph. Split
/// from [`extract_history`] so the CPU-bound walk can run in spawn_blocking.
///
/// The `-z` stream is NUL-separated tokens: each record is the five header
/// fields (`%H %ct %an %ae %s`, each NUL-terminated) followed by the
/// `--name-only` paths (NUL-terminated), then a NUL record separator (an
/// empty token). Only the first path of a record carries git's leading
/// `\n` separator between the format and the file list. Iterating the
/// split directly avoids materializing one big Vec of token slices.
fn parse_history(log: &str) -> anyhow::Result<Graph> {
    let mut graph = Graph::default();
    // Emit each contributor's node once; AUTHORED edges still attribute
    // every commit, deduping only the node by email.
    let mut seen_contributors: HashSet<String> = HashSet::new();
    let mut tokens = log.split('\0');
    while let Some(sha) = tokens.next() {
        // Skip record separators and the trailing empty token.
        if sha.is_empty() {
            continue;
        }
        // The rest of the five-field header; a short tail is truncation.
        let (Some(ct), Some(name), Some(email), Some(subject)) =
            (tokens.next(), tokens.next(), tokens.next(), tokens.next())
        else {
            break;
        };
        // Paths run until the record-separator (empty) token or the end;
        // the `for` consumes that separator so the next record starts clean.
        let mut paths: Vec<&str> = Vec::new();
        let mut first_path = true;
        for token in tokens.by_ref() {
            if token.is_empty() {
                break;
            }
            // git inserts a `\n` between the format and the file list; it
            // rides on the first path token only.
            let path = if first_path {
                token.strip_prefix('\n').unwrap_or(token)
            } else {
                token
            };
            first_path = false;
            paths.push(path);
        }
        // Defense in depth: NUL framing already prevents forged records, but
        // a misframed record (a future git output change) would desync —
        // skip anything whose first field isn't a real sha.
        if !is_hex_sha(sha) {
            continue;
        }
        let Ok(committed_at) = ct.parse::<i64>() else {
            continue;
        };
        let commit_node = Node::commit(sha, subject, committed_at);
        let commit_id = commit_node.id.clone();
        graph.nodes.push(commit_node);

        // git already trims surrounding whitespace from idents; trim again
        // so a degenerate email can't key a whitespace Contributor. An
        // empty email can't key one at all (its id would be `contributor:`
        // — no local part, unaddressable): record the commit, but leave it
        // unattributed rather than mint a broken node.
        let email = email.trim();
        if !email.is_empty() {
            let contributor = Node::contributor(email, name);
            let contributor_id = contributor.id.clone();
            if seen_contributors.insert(email.to_string()) {
                graph.nodes.push(contributor);
            }
            graph.edges.push(Edge {
                src: contributor_id,
                dst: commit_id.clone(),
                kind: EdgeKind::Authored,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }
        for path in paths.into_iter().filter(|p| !p.is_empty()) {
            graph.edges.push(Edge {
                src: commit_id.clone(),
                dst: Node::file(path).id,
                kind: EdgeKind::Touches,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }
    }
    Ok(graph)
}

/// Whether `s` is a full git object name — 40 (SHA-1) or 64 (SHA-256) hex
/// digits. The `%H` format always emits one; anything else marks a desynced
/// record the parser should drop.
fn is_hex_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fold an extracted [`extract_history`] graph into the syntactic `graph`,
/// keeping only TOUCHES edges that reach a File node the syntactic pass
/// actually minted. The syntactic graph's File nodes are ground truth for
/// what the current tree holds, so this one rule drops every TOUCHES the
/// read path would choke on — a since-deleted file, a path skipped as
/// non-UTF-8, a submodule gitlink — without the history walk needing to
/// know why. Commit and Contributor nodes and AUTHORED edges always carry
/// over: full history is ingested even for commits that touch nothing in
/// the current tree.
pub(crate) fn merge_history(graph: &mut Graph, history: Graph) {
    let file_ids: std::collections::HashSet<&str> = graph
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

/// Last-resort cap on the history walk. `git log` is local, so the only
/// realistic way it runs long is a sick mirror filesystem — and it runs
/// while the mirror lock is held, so an unbounded run would starve the
/// repo's fetches. Generous (full-history `--name-only` over a large repo
/// still finishes in well under this), but finite.
const HISTORY_LOG_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Walk `git log` over the bare mirror, one NUL-framed record per commit.
/// `-z` NUL-separates commits and (un-quoted) pathnames; `%ct` is the
/// committer date in unix seconds (the `history` Verb's newest-first key);
/// `--no-renames` keeps a rename as delete+add, which is what plain
/// `git log -- <path>` (no `--follow`) reports too, so the demo agrees.
///
/// `--name-only` shows no files for a merge commit (git suppresses merge
/// diffs by default), so a path changed only within a merge gets no
/// TOUCHES — M0 attributes paths per non-merge commit, which agrees with
/// `git log -- <path>` on the linear/first-parent history the tracer
/// targets. Merge-commit path attribution is later-milestone work.
///
/// `kill_on_drop` + a timeout, like `yg_sync`'s `run_git`: a wedged git
/// must not hold the mirror lock forever (the future being dropped reaps
/// the child). Lossy UTF-8 decode: a repo's history is arbitrary, and a
/// latin-1 author name must cost a replacement character at worst, never a
/// pass that fails forever identically. (Paths are kept literal via
/// `core.quotePath=false` so they match the File-node ids the checkout
/// walk built; any that still don't are pruned by `merge_history`.)
async fn git_log_history(git_dir: &Path, commit: &str) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--git-dir")
        .arg(git_dir)
        .args(["-c", "core.quotePath=false"])
        .args(["log", commit, "--no-renames", "--name-only", "-z"])
        .arg("--pretty=format:%H%x00%ct%x00%an%x00%ae%x00%s%x00")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let out = tokio::time::timeout(HISTORY_LOG_TIMEOUT, cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "git log {commit} still running after {} minutes; killed \
                 (hung mirror filesystem?)",
                HISTORY_LOG_TIMEOUT.as_secs() / 60
            )
        })?
        .context("running git log (is git installed on this worker?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git log {commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Folding history into the syntactic graph keeps a TOUCHES only when
    /// the syntactic pass minted a File node for its target — a since-
    /// deleted (or otherwise skipped) path is pruned so the read path never
    /// meets an edge to an absent node. Commit/Contributor nodes and
    /// AUTHORED edges always carry over (full history is ingested).
    #[test]
    fn merge_history_prunes_touches_to_files_the_syntactic_graph_lacks() {
        let mut graph = Graph::default();
        graph.nodes.push(Node::file("foo.go"));

        let mut history = Graph::default();
        history.nodes.push(Node::commit("abc123", "edit", 1_000));
        history
            .nodes
            .push(Node::contributor("alice@example.com", "Alice"));
        history.edges.push(Edge {
            src: "contributor:alice@example.com".into(),
            dst: "commit:abc123".into(),
            kind: EdgeKind::Authored,
            provenance: Provenance::Extracted,
            confidence: 1.0,
            location: None,
        });
        // The commit touched foo.go (a current File) and scratch.txt (none).
        for path in ["foo.go", "scratch.txt"] {
            history.edges.push(Edge {
                src: "commit:abc123".into(),
                dst: format!("file:{path}"),
                kind: EdgeKind::Touches,
                provenance: Provenance::Extracted,
                confidence: 1.0,
                location: None,
            });
        }

        merge_history(&mut graph, history);

        assert!(graph.nodes.iter().any(|n| n.id == "commit:abc123"));
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| n.id == "contributor:alice@example.com")
        );
        assert_eq!(
            graph
                .edges
                .iter()
                .filter(|e| e.kind == EdgeKind::Authored)
                .count(),
            1,
            "AUTHORED always carries over"
        );
        let touched: Vec<&str> = graph
            .edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Touches)
            .map(|e| e.dst.as_str())
            .collect();
        assert_eq!(
            touched,
            ["file:foo.go"],
            "the absent file's TOUCHES is pruned"
        );
    }
}
