//! The lexical `search` Verb served over REST from the repos' full-text
//! segments: ranked hits with snippets and node ids that feed straight
//! into the other Verbs, with kind and repo filters and cursor
//! pagination. Runs against the dev compose stack like verbs.rs (see
//! docs/DEVELOPMENT.md).

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

/// A fixture whose graph holds a Symbol named for the demo query plus a
/// doc that mentions the same words in prose.
const RATE_LIMIT_FIXTURE: &[(&str, &str)] = &[
    (
        "ratelimit.go",
        "package svc\n\n// RateLimit throttles requests.\nfunc RateLimit() {}\n\nfunc main() {\n\tRateLimit()\n}\n",
    ),
    (
        "README.md",
        "# svc\n\nThis service applies a rate limit to each member.\n",
    ),
];

#[tokio::test]
async fn search_finds_the_symbol_and_its_id_feeds_node() {
    // Issue #7's demo: a natural query finds the right Symbol, and the id
    // it returns is usable in the other Verbs.
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    let body = h.verb_ok("search", json!({"query": "rate limit"})).await;
    let hits = body["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "the query must find something: {body}");

    let top = &hits[0];
    let id = top["id"].as_str().expect("a hit carries a node id");
    assert!(
        id.ends_with("ratelimit.go#RateLimit"),
        "the RateLimit Symbol ranks first, above the prose that mentions it: {body}"
    );
    assert_eq!(top["kind"], "Symbol");
    assert_eq!(top["repo"], h.qualifier());
    // The hit reports the raw name, not the split text the index matches on.
    assert_eq!(
        top["name"], "RateLimit",
        "the hit's name is the raw symbol name: {body}"
    );

    // The returned id resolves end-to-end through `node`.
    let node = h.verb_ok("node", json!({"id": id})).await;
    assert_eq!(node["node"]["id"], id);
    assert_eq!(node["node"]["name"], "RateLimit");
}

#[tokio::test]
async fn the_kind_filter_narrows_hits_to_the_named_kinds() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    let body = h
        .verb_ok("search", json!({"query": "rate limit", "kinds": ["File"]}))
        .await;
    let hits = body["hits"].as_array().expect("hits array");
    assert!(!hits.is_empty(), "the README still matches: {body}");
    assert!(
        hits.iter().all(|h| h["kind"] == "File"),
        "a File filter yields only Files: {body}"
    );
    let readme = hits
        .iter()
        .find(|h| h["id"].as_str().unwrap().ends_with("README.md"))
        .expect("the matching File is the README");
    // A content hit carries a highlighted snippet of the match.
    assert!(
        readme["snippet"]
            .as_str()
            .is_some_and(|s| s.contains("<b>rate</b>") || s.contains("<b>limit</b>")),
        "the File hit carries a highlighted snippet: {body}"
    );

    // Symmetric exclusion: filtering to Symbol drops the README File that
    // an unfiltered search returns — the filter removes, not just relabels.
    let body = h
        .verb_ok(
            "search",
            json!({"query": "rate limit", "kinds": ["Symbol"]}),
        )
        .await;
    let hits = body["hits"].as_array().expect("hits array");
    assert!(
        hits.iter().all(|h| h["kind"] == "Symbol"),
        "a Symbol filter yields only Symbols: {body}"
    );
    assert!(
        hits.iter()
            .all(|h| !h["id"].as_str().unwrap().ends_with("README.md")),
        "the README File is excluded under the Symbol filter: {body}"
    );
    assert!(
        hits.iter()
            .any(|h| h["id"].as_str().unwrap().ends_with("RateLimit")),
        "the RateLimit Symbol still matches under the Symbol filter: {body}"
    );
}

#[tokio::test]
async fn an_empty_repos_filter_is_refused_like_an_empty_kinds_filter() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // An explicit empty list is ambiguous (no repos vs no filter); it's a
    // 400, the same as an empty `kinds` — omit the filter to search all.
    let (status, body) = h
        .verb("search", json!({"query": "rate limit", "repos": []}))
        .await;
    assert_eq!(status, 400, "an explicit empty repos list is a 400: {body}");

    let (status, body) = h
        .verb("search", json!({"query": "rate limit", "kinds": []}))
        .await;
    assert_eq!(status, 400, "an explicit empty kinds list is a 400: {body}");
}

#[tokio::test]
async fn a_garbled_cursor_is_a_client_error_not_a_panic() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // A cursor is untrusted input; an undecodable one is the client's 400,
    // not a 500 or a crash.
    let (status, body) = h
        .verb(
            "search",
            json!({"query": "rate limit", "cursor": "!!!not-base64!!!"}),
        )
        .await;
    assert_eq!(status, 400, "a malformed cursor is a 400: {body}");
}

#[tokio::test]
async fn a_cursor_re_sent_with_a_different_query_is_refused() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // A first page small enough to leave a cursor.
    let first = h
        .verb_ok("search", json!({"query": "rate limit", "limit": 1}))
        .await;
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a multi-hit search leaves a cursor")
        .to_string();

    // Re-using that cursor under a different query must be refused, never
    // silently answered against the wrong ranking.
    let (status, body) = h
        .verb(
            "search",
            json!({"query": "something unrelated", "limit": 1, "cursor": cursor}),
        )
        .await;
    assert_eq!(
        status, 400,
        "a cursor bound to another query is a 400: {body}"
    );
}

#[tokio::test]
async fn a_cursor_re_sent_with_a_changed_filter_is_refused() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // A first page (no filters) that leaves a cursor.
    let first = h
        .verb_ok("search", json!({"query": "rate limit", "limit": 1}))
        .await;
    let cursor = first["next_cursor"]
        .as_str()
        .expect("a multi-hit search leaves a cursor")
        .to_string();

    // The cursor pins the filters too: re-sending it with a kinds (or repos)
    // filter the original search didn't have must be refused, not silently
    // ignored — the client would otherwise think it narrowed the results.
    let (status, body) = h
        .verb(
            "search",
            json!({"query": "rate limit", "limit": 1, "kinds": ["Symbol"], "cursor": cursor}),
        )
        .await;
    assert_eq!(
        status, 400,
        "a cursor re-sent with a new kinds filter is a 400: {body}"
    );

    // Re-sending it unchanged (filters omitted) still pages cleanly.
    let (status, _) = h
        .verb(
            "search",
            json!({"query": "rate limit", "limit": 1, "cursor": cursor}),
        )
        .await;
    assert_eq!(status, 200, "the unchanged cursor still pages");
}

#[tokio::test]
async fn the_repos_filter_scopes_and_rejects_unknown_repos() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Naming the indexed repo still finds the hit.
    let body = h
        .verb_ok(
            "search",
            json!({"query": "rate limit", "repos": [h.qualifier()]}),
        )
        .await;
    assert!(
        !body["hits"].as_array().unwrap().is_empty(),
        "scoping to the indexed repo finds the hit: {body}"
    );

    // Naming a repo the server doesn't index is the client's error.
    let (status, body) = h
        .verb(
            "search",
            json!({"query": "rate limit", "repos": ["github.com/no/such"]}),
        )
        .await;
    assert_eq!(status, 404, "an unindexed repo is a 404: {body}");
}

#[tokio::test]
async fn an_unsupported_mode_is_rejected() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    let (status, body) = h
        .verb("search", json!({"query": "rate limit", "mode": "semantic"}))
        .await;
    assert_eq!(status, 400, "semantic search isn't available in M0: {body}");
    assert!(
        body["error"].as_str().unwrap().contains("lexical"),
        "the error names the supported mode: {body}"
    );
}

/// Smoke test for the warm-cache latency target (PRD / ARCHITECTURE §NFR:
/// `search p95 < 500 ms warm`). It is not a benchmark — it asserts that
/// once the Shard is materialized, repeated searches clear the documented
/// target with wide margin (the *slowest* of many, so no stall is hidden).
/// That the warm path skips object storage entirely is the cache tier's
/// invariant, pinned by yg-shard's
/// `the_full_text_segment_materializes_once_and_then_searches_warm`; here
/// we just guard the end-to-end latency. Timed through the real HTTP
/// client, so localhost transport is included.
#[tokio::test]
async fn warm_search_latency_clears_the_documented_target() {
    const WARM_LATENCY_TARGET: Duration = Duration::from_millis(500);

    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Warm the local tier: the first query fetches the Shard from object
    // storage and unpacks the segment; every later query reads locally.
    let _ = h.verb_ok("search", json!({"query": "rate limit"})).await;

    let mut slowest = Duration::ZERO;
    for _ in 0..20 {
        let start = Instant::now();
        let body = h.verb_ok("search", json!({"query": "rate limit"})).await;
        slowest = slowest.max(start.elapsed());
        assert!(
            !body["hits"].as_array().unwrap().is_empty(),
            "each warm query still returns the hit"
        );
    }
    assert!(
        slowest < WARM_LATENCY_TARGET,
        "slowest warm search was {slowest:?}, over the {WARM_LATENCY_TARGET:?} target"
    );
}

#[tokio::test]
async fn yg_search_reports_hits_humanly_and_as_raw_json() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    let human = h.yg_ok(&["search", "rate limit"]).await;
    assert!(
        human.contains("Symbol") && human.contains("ratelimit.go#RateLimit"),
        "the human report names the kind and the node id:\n{human}"
    );
    // The snippet reaches the terminal as plain text — no HTML tags.
    assert!(
        !human.contains("<b>"),
        "the highlight markup is flattened for the terminal:\n{human}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&h.yg_ok(&["search", "rate limit", "--json"]).await)
            .expect("--json emits the raw response");
    let top = &json["hits"][0];
    assert!(
        top["id"]
            .as_str()
            .unwrap()
            .ends_with("ratelimit.go#RateLimit"),
        "the raw JSON carries the ranked hits: {json}"
    );

    // The kind filter flag rides along to the server.
    let json: serde_json::Value = serde_json::from_str(
        &h.yg_ok(&["search", "rate limit", "--kind", "File", "--json"])
            .await,
    )
    .expect("--json emits the raw response");
    assert!(
        json["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["kind"] == "File"),
        "a --kind File search yields only Files: {json}"
    );
}

#[tokio::test]
async fn paging_with_the_cursor_unions_to_the_whole_result_set() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // The whole ranking in one page, as the reference.
    let whole = h
        .verb_ok("search", json!({"query": "rate limit", "limit": 50}))
        .await;
    let expected: Vec<String> = whole["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    assert!(expected.len() >= 2, "the corpus has several hits: {whole}");
    assert!(
        whole["next_cursor"].is_null(),
        "a page holding everything offers no cursor: {whole}"
    );

    // Page through one hit at a time and reassemble.
    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut req = json!({"query": "rate limit", "limit": 1});
        if let Some(c) = &cursor {
            req["cursor"] = c.clone().into();
        }
        let page = h.verb_ok("search", req).await;
        let hits = page["hits"].as_array().expect("hits");
        assert!(hits.len() <= 1, "limit is honored: {page}");
        for hit in hits {
            paged.push(hit["id"].as_str().unwrap().to_string());
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
        assert!(paged.len() <= expected.len(), "pagination terminates");
    }

    assert_eq!(
        paged, expected,
        "the pages union to the whole ranking, in order, without loss or duplication"
    );
}

impl Harness {
    /// Register and index a second fixture repo against this harness's
    /// server, returning the tempdir guard (keep it alive) and the repo's
    /// qualifier.
    async fn index_extra_repo(&self, files: &[(&str, &str)]) -> (tempfile::TempDir, String) {
        let (fixture, repo_dir, url) = fixture_repo_with(files);
        post_repo(&self.base, json!({"url": url})).await;
        assert!(
            self.sync.run_once().await.unwrap(),
            "the extra repo's fetch runs"
        );
        assert!(
            self.indexer.run_once().await.unwrap(),
            "the extra repo's index runs"
        );
        (fixture, repo_dir.display().to_string())
    }
}

/// The repo qualifiers present in a search response's hits.
fn hit_repos(body: &serde_json::Value) -> std::collections::HashSet<String> {
    body["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| h["repo"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn an_org_wide_search_fans_out_and_the_repos_filter_narrows_it() {
    // The headline of issue #7: search spans every indexed repo by default.
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo_a = h.qualifier();
    let (_b_guard, repo_b) = h
        .index_extra_repo(&[(
            "budget.md",
            "# limits\n\nEvery tenant gets a rate limit budget.\n",
        )])
        .await;
    assert_ne!(repo_a, repo_b, "the two repos have distinct qualifiers");

    // Org-wide (no repo filter): hits come from both repos.
    let body = h.verb_ok("search", json!({"query": "rate limit"})).await;
    let repos = hit_repos(&body);
    assert!(
        repos.contains(&repo_a) && repos.contains(&repo_b),
        "an org-wide search merges hits from both repos: {body}"
    );

    // The repos filter narrows to the named repo — the other is excluded.
    let body = h
        .verb_ok("search", json!({"query": "rate limit", "repos": [&repo_a]}))
        .await;
    let repos = hit_repos(&body);
    assert!(
        repos.contains(&repo_a) && !repos.contains(&repo_b),
        "the repos filter excludes the unnamed repo: {body}"
    );

    // A repo named twice is one target — its hits are not duplicated.
    let body = h
        .verb_ok(
            "search",
            json!({"query": "rate limit", "repos": [&repo_a, &repo_a]}),
        )
        .await;
    let ids: Vec<&str> = body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "a duplicated repo filter doesn't duplicate hits: {body}"
    );
}

#[tokio::test]
async fn pagination_is_consistent_when_hits_tie_on_score() {
    // Three files with identical content score identically; paging must
    // still union to the whole set without dropping or duplicating a tied
    // hit — a per-page fetch that varied with the offset would not.
    let h = Harness::boot_with(&[
        ("a.md", "rate limit budget shared prose"),
        ("b.md", "rate limit budget shared prose"),
        ("c.md", "rate limit budget shared prose"),
    ])
    .await;
    h.add_repo().await;
    h.sync_and_index().await;

    let whole = h
        .verb_ok(
            "search",
            json!({"query": "rate limit", "kinds": ["File"], "limit": 50}),
        )
        .await;
    let expected: Vec<String> = whole["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(expected.len(), 3, "all three tied files match: {whole}");

    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut req = json!({"query": "rate limit", "kinds": ["File"], "limit": 1});
        if let Some(c) = &cursor {
            req["cursor"] = c.clone().into();
        }
        let page = h.verb_ok("search", req).await;
        for hit in page["hits"].as_array().unwrap() {
            paged.push(hit["id"].as_str().unwrap().to_string());
        }
        match page["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
        assert!(paged.len() <= 3, "pagination terminates");
    }

    let unique: std::collections::HashSet<&String> = paged.iter().collect();
    assert_eq!(
        unique.len(),
        paged.len(),
        "no tied hit appears on two pages: {paged:?}"
    );
    let mut sorted = paged.clone();
    sorted.sort();
    let mut sorted_expected = expected.clone();
    sorted_expected.sort();
    assert_eq!(
        sorted, sorted_expected,
        "the tied hits page without loss or duplication"
    );
}

#[tokio::test]
async fn search_rejects_an_empty_query_and_a_malformed_one() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    let (status, body) = h.verb("search", json!({"query": "   "})).await;
    assert_eq!(status, 400, "an empty query is rejected: {body}");

    // A query tantivy can't parse (an unbalanced group) is the client's
    // error, surfaced as a 400 rather than a 500.
    let (status, body) = h.verb("search", json!({"query": "(rate limit"})).await;
    assert_eq!(status, 400, "a malformed query is a 400, not a 500: {body}");
}

#[tokio::test]
async fn a_search_filtered_to_an_unindexed_kind_is_empty_not_an_error() {
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Package nodes carry no searchable text; filtering to them is a valid
    // query that simply matches nothing.
    let body = h
        .verb_ok(
            "search",
            json!({"query": "rate limit", "kinds": ["Package"]}),
        )
        .await;
    assert!(
        body["hits"].as_array().unwrap().is_empty(),
        "a Package filter returns an empty result, not an error: {body}"
    );
}

#[tokio::test]
async fn the_org_wide_fan_out_excludes_repos_on_an_outdated_schema() {
    // The fan-out treats one repo's shard-read error as fatal (it aborts the
    // whole search). After a schema bump, repos still pointing at an older
    // shard would otherwise 503 every org-wide search for the entire
    // re-index window. `indexed_repos` filters by the current revision
    // suffix, so such a repo is simply absent from the fan-out — searched
    // only once it carries a current-schema (hence searchable) segment.
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    h.add_repo().await;
    h.sync_and_index().await;
    let control = control_plane(&h.db_name).await;

    // The freshly indexed repo is at the current schema...
    let current = control
        .indexed_repos(&yg_shard::syntactic_revision_suffix())
        .await
        .unwrap();
    assert_eq!(current.len(), 1, "the indexed repo joins the fan-out");

    // ...and is excluded under any other schema suffix — the discriminator
    // that keeps an as-yet-unmigrated repo from failing the whole search.
    let other_schema = control.indexed_repos("-syntactic-v0").await.unwrap();
    assert!(
        other_schema.is_empty(),
        "a repo whose shard is on a different schema is filtered out: {other_schema:?}"
    );
}

#[tokio::test]
async fn searching_before_anything_is_indexed_is_empty_not_an_error() {
    // A fresh server with no indexed repos: an org-wide search is a valid
    // empty result, not a failure.
    let h = Harness::boot_with(RATE_LIMIT_FIXTURE).await;
    // Deliberately do not register or index the fixture.
    let body = h.verb_ok("search", json!({"query": "rate limit"})).await;
    assert!(
        body["hits"].as_array().unwrap().is_empty(),
        "nothing is indexed yet: {body}"
    );
    assert!(body["next_cursor"].is_null());
}
