//! The full-text segment: an indexing pass builds it from Symbol and
//! File documents, and the read side searches it for ranked hits whose
//! node ids feed straight into the other Verbs. Pure, in-process — no
//! object storage — so it exercises only the build → pack → unpack →
//! open → search pipeline.

use yg_shard::{
    NodeKind, SearchDoc, SearchParams, build_fts, open_fts, search, snippets_for, unpack_fts,
};

/// Build a segment from `docs`, round-trip it through the packed bytes
/// (the form object storage holds), and open it for reading.
fn open_built(docs: &[SearchDoc]) -> (tempfile::TempDir, yg_shard::FtsIndex) {
    let bytes = build_fts(docs).expect("building the fts segment");
    let dir = tempfile::tempdir().unwrap();
    unpack_fts(&bytes, dir.path()).expect("unpacking the fts segment");
    let index = open_fts(dir.path()).expect("opening the fts segment");
    (dir, index)
}

#[test]
fn a_natural_query_finds_the_matching_symbol() {
    // Issue #7's demo, in miniature: "rate limit" surfaces the
    // RateLimit Symbol over unrelated prose.
    let docs = [
        SearchDoc {
            node_id: "sym:limit.go#RateLimit".into(),
            kind: NodeKind::Symbol,
            name: Some("RateLimit".into()),
            path: Some("limit.go".into()),
            content: String::new(),
        },
        SearchDoc {
            node_id: "file:README.md".into(),
            kind: NodeKind::File,
            name: Some("README.md".into()),
            path: Some("README.md".into()),
            content: "Widgets are great. This file says nothing about throttling.".into(),
        },
    ];
    let (_dir, index) = open_built(&docs);

    let hits = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: None,
            limit: 10,
        },
    )
    .expect("search runs");

    assert!(!hits.is_empty(), "the query must find the symbol");
    assert_eq!(
        hits[0].node_id, "sym:limit.go#RateLimit",
        "the RateLimit symbol ranks first for 'rate limit'"
    );
}

/// The fixture used by the filter/snippet/ranking tests: a Symbol named
/// for the query, and a File whose content also mentions it.
fn rate_limit_corpus() -> [SearchDoc; 2] {
    [
        SearchDoc {
            node_id: "sym:limit.go#RateLimit".into(),
            kind: NodeKind::Symbol,
            name: Some("RateLimit".into()),
            path: Some("limit.go".into()),
            content: String::new(),
        },
        SearchDoc {
            node_id: "file:README.md".into(),
            kind: NodeKind::File,
            name: Some("README.md".into()),
            path: Some("README.md".into()),
            content: "Operators can configure the rate limit per member.".into(),
        },
    ]
}

#[test]
fn the_kind_filter_restricts_hits_to_the_named_kinds() {
    let (_dir, index) = open_built(&rate_limit_corpus());

    let files = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: Some(&[NodeKind::File]),
            limit: 10,
        },
    )
    .unwrap();
    assert!(
        files.iter().all(|h| h.kind == "File"),
        "a File filter yields only Files: {files:?}"
    );
    assert!(
        files.iter().any(|h| h.node_id == "file:README.md"),
        "the matching File is still found: {files:?}"
    );

    let symbols = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: Some(&[NodeKind::Symbol]),
            limit: 10,
        },
    )
    .unwrap();
    assert!(
        symbols.iter().all(|h| h.kind == "Symbol"),
        "a Symbol filter yields only Symbols: {symbols:?}"
    );
    // ...and the matching Symbol is still present — an empty result would
    // satisfy the `all` above vacuously, hiding an over-eager filter.
    assert!(
        symbols
            .iter()
            .any(|h| h.node_id == "sym:limit.go#RateLimit"),
        "the matching Symbol survives the Symbol filter: {symbols:?}"
    );
}

#[test]
fn a_pathologically_nested_query_is_refused_not_crashed() {
    // tantivy's parser recurses per '(', so a long run overflows the stack
    // and *aborts the process* — uncatchable. The guard must turn it into a
    // clean QueryMalformed (a client 400) instead of crashing the server.
    let (_dir, index) = open_built(&rate_limit_corpus());
    let nested = "(".repeat(5_000);
    let err = search(
        &index,
        &SearchParams {
            query: &nested,
            kinds: None,
            limit: 10,
        },
    )
    .expect_err("a pathologically nested query errors, never crashes");
    assert!(
        err.downcast_ref::<yg_shard::QueryMalformed>().is_some(),
        "the guard surfaces a client error: {err:#}"
    );
}

#[test]
fn a_range_query_is_refused() {
    // Range queries force a full term-dictionary scan whose cost ignores the
    // page limit — refused before tantivy runs them.
    let (_dir, index) = open_built(&rate_limit_corpus());
    // Every range spelling: bracket, exclusive `{}`, exotic whitespace around
    // `TO`, and the bracketless elastic `>`/`<`/`>=` comparison forms.
    for q in [
        "body:[a TO z]",
        "body:{a TO z}",
        "terms:[a TO *]",
        "body:[a\tTO\tz]",
        "body:>a",
        "terms:<=z",
        "body:>=a",
    ] {
        let err = search(
            &index,
            &SearchParams {
                query: q,
                kinds: None,
                limit: 10,
            },
        )
        .unwrap_err();
        assert!(
            err.downcast_ref::<yg_shard::QueryMalformed>().is_some(),
            "{q:?} is refused as a client error: {err:#}"
        );
    }
}

#[test]
fn an_over_long_query_is_refused() {
    // A multi-kilobyte query (no nesting) is rejected before tantivy sees
    // it — bounding parser CPU and the cursor that would carry the query.
    let (_dir, index) = open_built(&rate_limit_corpus());
    let long = "a ".repeat(2_000);
    let err = search(
        &index,
        &SearchParams {
            query: &long,
            kinds: None,
            limit: 10,
        },
    )
    .expect_err("an over-long query errors");
    assert!(
        err.downcast_ref::<yg_shard::QueryMalformed>().is_some(),
        "the guard surfaces a client error: {err:#}"
    );
}

#[test]
fn a_hit_reports_the_raw_name_not_the_indexed_split_words() {
    // The index splits "RateLimit" into "rate"/"limit" so a natural query
    // matches — but that split text must never leak into what the hit
    // reports as the name.
    let (_dir, index) = open_built(&rate_limit_corpus());

    let hits = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: Some(&[NodeKind::Symbol]),
            limit: 10,
        },
    )
    .unwrap();

    let symbol = hits
        .iter()
        .find(|h| h.node_id == "sym:limit.go#RateLimit")
        .expect("the symbol is found");
    assert_eq!(
        symbol.name.as_deref(),
        Some("RateLimit"),
        "the hit reports the raw name, not the split index text: {symbol:?}"
    );
}

#[test]
fn a_hit_carries_a_highlighted_snippet_of_the_match() {
    let (_dir, index) = open_built(&rate_limit_corpus());

    let hits = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: Some(&[NodeKind::File]),
            limit: 10,
        },
    )
    .unwrap();
    // Ranking carries no snippets; they're hydrated for the chosen hits.
    assert!(
        hits.iter().all(|h| h.snippet.is_none()),
        "ranking returns no snippets"
    );

    let ids: Vec<String> = hits.iter().map(|h| h.node_id.clone()).collect();
    let snippets = snippets_for(&index, "rate limit", &ids).expect("snippet hydration runs");
    let snippet = snippets
        .get("file:README.md")
        .expect("a content match comes with a snippet");
    assert!(
        snippet.contains("<b>rate</b>") || snippet.contains("<b>limit</b>"),
        "the snippet highlights the matched terms: {snippet:?}"
    );
    // It's a window of the surrounding content, not just the matched term
    // (the README sentence is "Operators can configure the rate limit per
    // member.").
    assert!(
        snippet.contains("configure") || snippet.contains("member"),
        "the snippet carries surrounding context: {snippet:?}"
    );
}

#[test]
fn a_symbol_named_for_the_query_outranks_a_file_that_merely_mentions_it() {
    let (_dir, index) = open_built(&rate_limit_corpus());

    let hits = search(
        &index,
        &SearchParams {
            query: "rate limit",
            kinds: None,
            limit: 10,
        },
    )
    .unwrap();

    assert_eq!(
        hits[0].node_id, "sym:limit.go#RateLimit",
        "the name match beats the incidental content match: {hits:?}"
    );
}
