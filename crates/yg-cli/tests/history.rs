//! The `history` Verb: commits touching a File (or a Symbol's defining
//! file), newest-first, with a `since` filter and cursor pagination, plus
//! its `yg history` CLI. Runs against the dev compose stack like e2e.rs
//! (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use serde_json::json;

/// The subject lines of a history response's commits, in order.
fn subjects(body: &serde_json::Value) -> Vec<String> {
    body["commits"]
        .as_array()
        .expect("a commits array")
        .iter()
        .map(|c| c["subject"].as_str().unwrap_or("?").to_string())
        .collect()
}

/// A repo whose history touches foo.go twice (by Alice) with an
/// unrelated README commit (by Bob) interleaved between them.
fn foo_history() -> (tempfile::TempDir, std::path::PathBuf, String) {
    history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "add foo",
            files: &[("foo.go", "package main\n")],
        },
        FixtureCommit {
            author: "Bob",
            email: "bob@example.com",
            when: 1_000_000_100,
            message: "touch readme",
            files: &[("README.md", "# hi\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_200,
            message: "edit foo",
            files: &[("foo.go", "package main\n// edit\n")],
        },
    ])
}

#[tokio::test]
async fn history_lists_commits_touching_a_file_newest_first() {
    let h = Harness::boot_around(foo_history()).await;
    h.add_repo().await;
    h.sync_and_index().await;
    let file = format!("file:{}:foo.go", h.qualifier());

    let body = h.verb_ok("history", json!({ "id": file })).await;
    let commits = body["commits"].as_array().expect("a commits array");

    // foo.go was touched by the first and third commits — not Bob's
    // README commit — newest first.
    assert_eq!(commits.len(), 2, "only commits touching foo.go: {body}");
    assert_eq!(commits[0]["subject"], "edit foo", "newest first");
    assert_eq!(commits[1]["subject"], "add foo");

    // Each commit carries its date and its author (a Contributor keyed by
    // email), externally addressable.
    assert_eq!(commits[0]["committed_at"], 1_000_000_200_i64);
    assert_eq!(commits[0]["author"]["email"], "alice@example.com");
    assert!(
        commits[0]["commit"]
            .as_str()
            .is_some_and(|id| id.starts_with(&format!("commit:{}:", h.qualifier()))),
        "the commit id is externally addressable: {}",
        commits[0]["commit"]
    );
    assert!(
        commits[0]["author"]["id"]
            .as_str()
            .is_some_and(|id| id == format!("contributor:{}:alice@example.com", h.qualifier())),
        "the author id is externally addressable: {}",
        commits[0]["author"]["id"]
    );
}

#[tokio::test]
async fn history_for_a_symbol_uses_its_defining_files_history() {
    let h = Harness::boot_around(history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "add foo",
            files: &[("foo.go", "package main\n\nfunc Foo() {}\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_200,
            message: "edit foo",
            files: &[("foo.go", "package main\n\nfunc Foo() { println(1) }\n")],
        },
    ]))
    .await;
    h.add_repo().await;
    h.sync_and_index().await;

    let symbol = format!("sym:{}:foo.go#Foo", h.qualifier());
    let file = format!("file:{}:foo.go", h.qualifier());

    let by_symbol = h.verb_ok("history", json!({ "id": symbol })).await;
    let by_file = h.verb_ok("history", json!({ "id": file })).await;

    // A Symbol's history is exactly its defining file's history (RFC 0001 §7).
    assert_eq!(
        by_symbol["commits"], by_file["commits"],
        "a Symbol's history mirrors its file's"
    );
    assert_eq!(
        by_symbol["commits"].as_array().expect("commits").len(),
        2,
        "both commits touched foo.go"
    );
}

#[tokio::test]
async fn history_since_filters_to_commits_at_or_after_the_date() {
    // 1_000_000_000 is 2001-09-09; 1_500_000_000 is 2017-07-14.
    let h = Harness::boot_around(history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "old foo",
            files: &[("foo.go", "package main\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_500_000_000,
            message: "new foo",
            files: &[("foo.go", "package main\n// edit\n")],
        },
    ]))
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let file = format!("file:{}:foo.go", h.qualifier());

    // A `since` between the two commits keeps only the newer one.
    let body = h
        .verb_ok(
            "history",
            json!({ "id": file, "since": "2010-01-01T00:00:00Z" }),
        )
        .await;
    assert_eq!(subjects(&body), ["new foo"], "only commits at/after since");

    // A plain date is accepted too (midnight UTC).
    let body = h
        .verb_ok("history", json!({ "id": file, "since": "2010-01-01" }))
        .await;
    assert_eq!(subjects(&body), ["new foo"]);

    // A `since` before everything keeps all of it.
    let body = h
        .verb_ok("history", json!({ "id": file, "since": "1999-01-01" }))
        .await;
    assert_eq!(subjects(&body), ["new foo", "old foo"]);

    // The boundary is inclusive: a `since` equal to the older commit's exact
    // instant (1_000_000_000 == 2001-09-09T01:46:40Z) still includes it.
    let body = h
        .verb_ok(
            "history",
            json!({ "id": file, "since": "2001-09-09T01:46:40Z" }),
        )
        .await;
    assert_eq!(
        subjects(&body),
        ["new foo", "old foo"],
        "since == a commit's exact second includes that commit (>=)"
    );

    // A malformed `since` is the client's to fix.
    let (status, _) = h
        .verb("history", json!({ "id": file, "since": "yesterday" }))
        .await;
    assert_eq!(status, 400, "an unparseable since is a 400");
}

#[tokio::test]
async fn history_paginates_with_a_cursor_without_gaps_or_duplicates() {
    let h = Harness::boot_around(history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "c1",
            files: &[("foo.go", "package main\n// 1\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_100,
            message: "c2",
            files: &[("foo.go", "package main\n// 2\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_200,
            message: "c3",
            files: &[("foo.go", "package main\n// 3\n")],
        },
    ]))
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let file = format!("file:{}:foo.go", h.qualifier());

    // Page size 2: newest two, then the cursor yields the remaining one.
    let p1 = h
        .verb_ok(
            "history",
            json!({ "id": file, "limit": 2, "since": "2000-01-01" }),
        )
        .await;
    assert_eq!(subjects(&p1), ["c3", "c2"], "newest-first first page");
    let cursor = p1["next_cursor"].as_str().expect("a next cursor");

    let (status, body) = h
        .verb(
            "history",
            json!({ "id": file, "limit": 2, "since": "2000-01-02", "cursor": cursor }),
        )
        .await;
    assert_eq!(status, 400, "a changed since filter is rejected: {body}");

    let p2 = h
        .verb_ok(
            "history",
            json!({ "id": file, "limit": 2, "cursor": cursor }),
        )
        .await;
    assert_eq!(subjects(&p2), ["c1"], "the page resumes without gap or dup");
    assert!(
        p2["next_cursor"].is_null(),
        "the history is exhausted: {p2}"
    );

    // A cursor replayed against a different request is rejected, never
    // served from another traversal.
    let other = format!("sym:{}:foo.go#Nope", h.qualifier());
    let (status, _) = h
        .verb(
            "history",
            json!({ "id": other, "limit": 2, "cursor": cursor }),
        )
        .await;
    assert_eq!(status, 400, "a cursor must match the request it came from");
}

#[tokio::test]
async fn history_pagination_is_stable_across_commits_sharing_a_timestamp() {
    // Three commits at the SAME committer second, all touching foo.go. The
    // newest-first order is (committed_at DESC, id ASC); the keyset resume
    // must walk the tie by id without a gap or a duplicate.
    let h = Harness::boot_around(history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "c1",
            files: &[("foo.go", "package main\n// 1\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "c2",
            files: &[("foo.go", "package main\n// 2\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "c3",
            files: &[("foo.go", "package main\n// 3\n")],
        },
    ]))
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let file = format!("file:{}:foo.go", h.qualifier());

    // Page one commit at a time, collecting shas across the cursor chain.
    let mut seen: Vec<String> = Vec::new();
    let mut cursor = serde_json::Value::Null;
    let mut pages = 0;
    loop {
        let mut req = json!({ "id": file, "limit": 1 });
        if !cursor.is_null() {
            req["cursor"] = cursor.clone();
        }
        let body = h.verb_ok("history", req).await;
        let commits = body["commits"].as_array().expect("commits");
        assert!(commits.len() <= 1, "limit respected: {body}");
        for c in commits {
            seen.push(c["sha"].as_str().expect("a sha").to_string());
        }
        pages += 1;
        cursor = body["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(pages < 10, "pagination must terminate");
    }

    assert_eq!(pages, 3, "three tied commits are three pages at limit 1");
    let mut unique = seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        seen.len(),
        3,
        "no commit returned twice across the tie: {seen:?}"
    );
    assert_eq!(
        unique.len(),
        3,
        "all three commits returned, none skipped: {seen:?}"
    );
}

#[tokio::test]
async fn yg_history_agrees_with_git_log_for_a_file() {
    // foo.go is touched by commits 1 and 3; README.md by 2 and 4 — so a
    // file's history must exclude the commits that didn't touch it.
    let h = Harness::boot_around(history_fixture(&[
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_000,
            message: "add foo",
            files: &[("foo.go", "package main\n")],
        },
        FixtureCommit {
            author: "Bob",
            email: "bob@example.com",
            when: 1_000_000_100,
            message: "add readme",
            files: &[("README.md", "# one\n")],
        },
        FixtureCommit {
            author: "Alice",
            email: "alice@example.com",
            when: 1_000_000_200,
            message: "edit foo",
            files: &[("foo.go", "package main\n// edit\n")],
        },
        FixtureCommit {
            author: "Bob",
            email: "bob@example.com",
            when: 1_000_000_300,
            message: "edit readme",
            files: &[("README.md", "# two\n")],
        },
    ]))
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let file = format!("file:{}:foo.go", h.qualifier());

    // The graph's view: the shas touching foo.go, newest-first.
    let json: serde_json::Value =
        serde_json::from_str(&h.yg_ok(&["history", &file, "--json"]).await)
            .expect("--json emits the raw response");
    let graph_shas: Vec<&str> = json["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["sha"].as_str().expect("each commit carries its sha"))
        .collect();

    // git's own answer for the same path on the same repo.
    let git_log = git(
        &h.repo_dir,
        &["log", "--no-renames", "--format=%H", "--", "foo.go"],
    );
    let git_shas: Vec<&str> = git_log.lines().collect();

    assert_eq!(
        graph_shas, git_shas,
        "yg history must agree with `git log -- foo.go`"
    );
    assert_eq!(graph_shas.len(), 2, "only the two foo.go commits");

    // The human report names each commit's subject and author.
    let human = h.yg_ok(&["history", &file]).await;
    for needle in ["edit foo", "add foo", "Alice"] {
        assert!(
            human.contains(needle),
            "human output lacks {needle:?}:\n{human}"
        );
    }
    assert!(
        !human.contains("readme"),
        "commits that didn't touch foo.go must not appear:\n{human}"
    );
}
