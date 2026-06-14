//! What the history extraction pulls out of a repo's git log (RFC 0001
//! §4, M0 scope): Commit nodes, Contributor nodes deduped by email, and
//! AUTHORED (Contributor→Commit) + TOUCHES (Commit→File) edges, all
//! `extracted` provenance — deterministically derived from git, not
//! guessed.

use std::collections::HashSet;
use std::path::Path;

use yg_shard::{EdgeKind, NodeKind, Provenance};

/// Run git in a directory, panicking (with stderr) on failure. The
/// author identity and dates come in through the environment so each
/// fixture commit has a known, machine-independent timestamp.
fn git(repo: &Path, envs: &[(&str, &str)], args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let out = cmd
        .output()
        .expect("git must be installed for the history tests");
    assert!(
        out.status.success(),
        "git {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A fresh repo with the machine-config guards every fixture needs, plus
/// a stable identity for the few git calls that don't override it.
fn init_repo() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().to_path_buf();
    git(&repo, &[], &["init", "-b", "main"]);
    git(&repo, &[], &["config", "user.email", "fixture@example.com"]);
    git(&repo, &[], &["config", "user.name", "Fixture"]);
    git(&repo, &[], &["config", "commit.gpgsign", "false"]);
    (root, repo)
}

/// Commit `files` as `name <email>` at unix time `when`, returning the
/// new commit's full sha.
fn commit_as(
    repo: &Path,
    name: &str,
    email: &str,
    when: i64,
    message: &str,
    files: &[(&str, &str)],
) -> String {
    for (path, contents) in files {
        let full = repo.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }
    git(repo, &[], &["add", "."]);
    let date = format!("{when} +0000");
    git(
        repo,
        &[
            ("GIT_AUTHOR_NAME", name),
            ("GIT_AUTHOR_EMAIL", email),
            ("GIT_AUTHOR_DATE", &date),
            ("GIT_COMMITTER_NAME", name),
            ("GIT_COMMITTER_EMAIL", email),
            ("GIT_COMMITTER_DATE", &date),
        ],
        &["commit", "-m", message],
    );
    git(repo, &[], &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn one_commit_yields_a_commit_a_contributor_and_authored_plus_touches_edges() {
    let (_root, repo) = init_repo();
    let sha = commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        1_000_000_000,
        "add foo",
        &[("foo.go", "package main\n")],
    );
    let graph = yg_index::extract_history(&repo.join(".git"), &sha)
        .await
        .expect("history extraction must handle a plain repo");

    // The commit itself: a Commit node carrying its subject and date.
    let commit = graph
        .nodes
        .iter()
        .find(|n| n.id == format!("commit:{sha}"))
        .expect("a Commit node for the head commit");
    assert_eq!(commit.kind, NodeKind::Commit);
    assert_eq!(commit.name.as_deref(), Some("add foo"));
    assert_eq!(
        commit.committed_at,
        Some(1_000_000_000),
        "the Commit carries its committer date for newest-first ordering"
    );

    // The author, as a Contributor keyed by email.
    let contributor = graph
        .nodes
        .iter()
        .find(|n| n.id == "contributor:alice@example.com")
        .expect("a Contributor node for the author");
    assert_eq!(contributor.kind, NodeKind::Contributor);
    assert_eq!(contributor.name.as_deref(), Some("Alice"));

    // AUTHORED: Contributor → Commit, extracted with full confidence.
    let authored = graph
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Authored)
        .expect("an AUTHORED edge");
    assert_eq!(authored.src, "contributor:alice@example.com");
    assert_eq!(authored.dst, format!("commit:{sha}"));
    assert_eq!(authored.provenance, Provenance::Extracted);
    assert_eq!(authored.confidence, 1.0);

    // TOUCHES: Commit → File, for the file the commit added.
    let touches = graph
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Touches)
        .expect("a TOUCHES edge");
    assert_eq!(touches.src, format!("commit:{sha}"));
    assert_eq!(touches.dst, "file:foo.go");
    assert_eq!(touches.provenance, Provenance::Extracted);
}

#[tokio::test]
async fn a_crafted_commit_subject_cannot_inject_forged_commits_or_contributors() {
    // A commit message is attacker-controlled by anyone who can push to an
    // indexed repo. The walk frames records with NUL (`git log -z`), the one
    // byte git forbids in a message or ident — so injection is impossible by
    // construction. This payload uses the ASCII RS/US bytes that an earlier
    // implementation used as delimiters: it must now be inert (just subject
    // bytes), and this test stands as a regression guard against ever
    // reverting to in-band, forgeable separators.
    let (_root, repo) = init_repo();
    let fake_sha = "d".repeat(40);
    let payload = format!(
        "real\u{1e}{fake_sha}\u{1f}1700000000\u{1f}Mallory\u{1f}mallory@evil.com\u{1f}injected"
    );
    let sha = commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        1_000_000_000,
        &payload,
        &[("foo.go", "package main\n")],
    );

    let graph = yg_index::extract_history(&repo.join(".git"), &sha)
        .await
        .unwrap();

    // No forged nodes from the subject's embedded separators.
    assert!(
        graph
            .nodes
            .iter()
            .all(|n| n.id != format!("commit:{fake_sha}")),
        "a subject must not be able to inject a Commit node: {:?}",
        graph.nodes.iter().map(|n| &n.id).collect::<Vec<_>>()
    );
    assert!(
        graph
            .nodes
            .iter()
            .all(|n| n.id != "contributor:mallory@evil.com"),
        "a subject must not be able to inject a Contributor node"
    );

    // Exactly one real commit, attributed to the real author, keeping its
    // own TOUCHES.
    let commits: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Commit)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        commits,
        vec![format!("commit:{sha}").as_str()],
        "one commit only"
    );
    assert!(
        graph.edges.iter().any(|e| e.kind == EdgeKind::Touches
            && e.src == format!("commit:{sha}")
            && e.dst == "file:foo.go"),
        "the real commit keeps its TOUCHES (not stolen by a phantom record)"
    );
}

#[tokio::test]
async fn contributors_dedup_by_email_and_touches_only_reach_current_files() {
    let (_root, repo) = init_repo();
    // Alice opens foo.go and a since-deleted scratch file.
    commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        1_000_000_000,
        "add foo and scratch",
        &[("foo.go", "package main\n"), ("scratch.txt", "wip\n")],
    );
    // The same person, spelled differently but the same email — one
    // Contributor — touches two current files in one commit.
    let c2 = commit_as(
        &repo,
        "alice",
        "alice@example.com",
        1_000_000_100,
        "add bar, edit foo",
        &[("foo.go", "package main\n// edit\n"), ("bar.md", "# bar\n")],
    );
    // A different author by a distinct email is a second Contributor.
    let c3 = commit_as(
        &repo,
        "Bob",
        "bob@example.com",
        1_000_000_200,
        "delete scratch",
        &[("scratch.txt", "")],
    );
    git(&repo, &[], &["rm", "scratch.txt"]);
    git(
        &repo,
        &[
            ("GIT_AUTHOR_NAME", "Bob"),
            ("GIT_AUTHOR_EMAIL", "bob@example.com"),
            ("GIT_AUTHOR_DATE", "1000000300 +0000"),
            ("GIT_COMMITTER_NAME", "Bob"),
            ("GIT_COMMITTER_EMAIL", "bob@example.com"),
            ("GIT_COMMITTER_DATE", "1000000300 +0000"),
        ],
        &["commit", "-m", "remove scratch"],
    );
    let _ = c3; // c3 is the pre-rm content commit; the rm commit follows.
    let head = git(&repo, &[], &["rev-parse", "HEAD"]);

    let graph = yg_index::extract_history(&repo.join(".git"), &head)
        .await
        .unwrap();

    // Two distinct Contributors, deduped by email — Alice authored two
    // commits under two name spellings but appears once.
    let contributors: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Contributor)
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        contributors.len(),
        2,
        "one node per email, however the name is spelled: {contributors:?}"
    );
    assert!(contributors.contains(&"contributor:alice@example.com"));
    assert!(contributors.contains(&"contributor:bob@example.com"));

    // Every commit got exactly one AUTHORED edge — Alice's two commits
    // both attribute to her single node.
    let commit_count = graph
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Commit)
        .count();
    let authored_count = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Authored)
        .count();
    assert_eq!(commit_count, 4, "all four commits are ingested");
    assert_eq!(authored_count, 4, "one AUTHORED edge per commit");
    let alice_authored = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Authored && e.src == "contributor:alice@example.com")
        .count();
    assert_eq!(
        alice_authored, 2,
        "Alice's two commits both attribute to her"
    );

    // TOUCHES only reach files in the current tree: the second commit
    // touched both foo.go and bar.md.
    let c2_touches: HashSet<&str> = graph
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Touches && e.src == format!("commit:{c2}"))
        .map(|e| e.dst.as_str())
        .collect();
    assert_eq!(
        c2_touches,
        HashSet::from(["file:foo.go", "file:bar.md"]),
        "the commit's TOUCHES cover exactly its current-tree files"
    );

    // extract_history is unfiltered: it records a TOUCHES for every
    // changed path, scratch.txt included — pruning to current-tree Files
    // is merge_history's job (covered by a focused unit test in the lib).
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Touches && e.dst == "file:scratch.txt"),
        "the raw history walk records TOUCHES for every changed path"
    );
}

#[tokio::test]
async fn an_authorless_commit_is_ingested_without_a_contributor() {
    // A commit with an empty author email (git permits it) can't key an
    // addressable `contributor:<email>` node, so it is recorded but left
    // unattributed — never minting a broken `contributor:` id.
    let (_root, repo) = init_repo();
    let sha = commit_as(
        &repo,
        "Nobody",
        "",
        1_000_000_000,
        "no author",
        &[("foo.go", "package main\n")],
    );

    let graph = yg_index::extract_history(&repo.join(".git"), &sha)
        .await
        .unwrap();

    assert!(
        graph
            .nodes
            .iter()
            .any(|n| n.id == format!("commit:{sha}") && n.kind == NodeKind::Commit),
        "the commit is still ingested"
    );
    assert!(
        graph.nodes.iter().all(|n| n.kind != NodeKind::Contributor),
        "an empty email mints no Contributor"
    );
    assert!(
        graph.edges.iter().all(|e| e.kind != EdgeKind::Authored),
        "and no AUTHORED edge"
    );
    // The commit's file change is still recorded.
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.kind == EdgeKind::Touches && e.dst == "file:foo.go"),
        "the TOUCHES still lands"
    );
}

#[tokio::test]
async fn extraction_errors_when_git_log_cannot_resolve_the_commit() {
    // A git-log failure must surface as an error — the index job then fails
    // and retries — never a silent empty history. Revisions are
    // deterministic per (commit, schema) and published Shards are
    // immutable, so a published empty history would be permanent; failing
    // loudly is the only safe choice.
    let (_root, repo) = init_repo();
    commit_as(
        &repo,
        "Alice",
        "alice@example.com",
        1_000_000_000,
        "real",
        &[("foo.go", "package main\n")],
    );
    let unresolvable = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let result = yg_index::extract_history(&repo.join(".git"), unresolvable).await;

    assert!(
        result.is_err(),
        "an unresolvable commit must error, not yield an empty graph"
    );
}
