//! The Verb read path: `node` and `neighbors` served over REST from the
//! repo's current Shard, plus their CLI subcommands. Runs against the dev
//! compose stack like e2e.rs (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn yg_node_reports_the_symbol_humanly_and_as_raw_json() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let id = format!("sym:{}:main.go#Hello", h.qualifier());

    let human = h.yg_ok(&["node", &id]).await;
    for needle in [
        "Symbol",
        "Hello",
        "main.go",
        "DEFINES",
        "CALLS",
        "syntactic",
    ] {
        assert!(
            human.contains(needle),
            "human output lacks {needle:?}:\n{human}"
        );
    }

    let json: serde_json::Value = serde_json::from_str(&h.yg_ok(&["node", &id, "--json"]).await)
        .expect("--json emits the raw response");
    assert_eq!(json["node"]["id"], id);
    // Summaries are kind-ordered: Hello is called by main and defined
    // by main.go.
    assert_eq!(json["edges"]["in"][0]["kind"], "CALLS");
    assert_eq!(json["edges"]["in"][1]["kind"], "DEFINES");
}

#[tokio::test]
async fn yg_neighbors_lists_the_subgraph_humanly_and_as_raw_json() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let id = format!("file:{}:main.go", h.qualifier());

    let human = h.yg_ok(&["neighbors", &id]).await;
    for name in ["Hello", "main"] {
        assert!(
            human.contains(name),
            "human output lacks {name:?}:\n{human}"
        );
    }
    assert!(human.contains("DEFINES"), "edges show their kind:\n{human}");

    let json: serde_json::Value =
        serde_json::from_str(&h.yg_ok(&["neighbors", &id, "--json"]).await)
            .expect("--json emits the raw response");
    // Hello and main (DEFINES), plus the commit that touched main.go.
    assert_eq!(json["nodes"].as_array().expect("nodes").len(), 3);
    assert!(json["next_cursor"].is_null());

    // Filters and pagination ride along to the server.
    let json: serde_json::Value = serde_json::from_str(
        &h.yg_ok(&[
            "neighbors",
            &id,
            "--direction",
            "out",
            "--edge-kinds",
            "CALLS",
            "--json",
        ])
        .await,
    )
    .expect("--json emits the raw response");
    assert_eq!(json["nodes"].as_array().expect("nodes").len(), 0);

    let json: serde_json::Value =
        serde_json::from_str(&h.yg_ok(&["neighbors", &id, "--limit", "1", "--json"]).await)
            .expect("--json emits the raw response");
    assert_eq!(json["nodes"].as_array().expect("nodes").len(), 1);
    let cursor = json["next_cursor"].as_str().expect("a second page exists");
    let json: serde_json::Value = serde_json::from_str(
        &h.yg_ok(&["neighbors", &id, "--cursor", cursor, "--json"])
            .await,
    )
    .expect("--json emits the raw response");
    // The resume carries no --limit, so it returns the rest in one page:
    // the two neighbors after the first (of three: Hello, main, commit).
    assert_eq!(json["nodes"].as_array().expect("nodes").len(), 2);
}

#[tokio::test]
async fn yg_neighbors_edge_kinds_calls_lists_the_callers_of_a_function() {
    // Issue #6's demo flow, verbatim flag spelling included.
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("sym:{}:main.go#Hello", h.qualifier());
    let human = h.yg_ok(&["neighbors", &id, "--edge-kinds", "CALLS"]).await;
    assert!(
        human.contains("main") && human.contains("CALLS"),
        "the caller and how it was found:\n{human}"
    );
    assert!(
        !human.contains("file:"),
        "DEFINES neighbors are filtered out:\n{human}"
    );
    assert!(
        human.contains("main.go:8"),
        "the call site rides along:\n{human}"
    );
}

#[tokio::test]
async fn yg_node_surfaces_the_servers_reason_on_failure() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("sym:{}:main.go#Nonexistent", h.qualifier());
    let out = h.yg(&["node", &id]).await;
    assert!(!out.status.success(), "a missing node is a failed command");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no node"),
        "stderr carries the server's reason:\n{stderr}"
    );
}

#[tokio::test]
async fn node_returns_a_symbol_with_its_defines_edge_summary() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("sym:{}:main.go#Hello", h.qualifier());
    let body = h.verb_ok("node", json!({ "id": id })).await;

    assert_eq!(body["node"]["id"], id, "ids echo back fully qualified");
    assert_eq!(body["node"]["kind"], "Symbol");
    assert_eq!(body["node"]["name"], "Hello");
    assert_eq!(body["node"]["path"], "main.go");

    // The defining File reaches this Symbol by one inbound DEFINES
    // edge, main calls it once, and each summary says how its edges
    // were derived. Summaries are kind-ordered: CALLS before DEFINES.
    let inbound = body["edges"]["in"].as_array().expect("edge summary");
    assert_eq!(inbound.len(), 2, "two inbound edge kinds: {body}");
    assert_eq!(inbound[0]["kind"], "CALLS");
    assert_eq!(inbound[0]["count"], 1);
    assert_eq!(inbound[0]["provenance"]["syntactic"], 1);
    assert_eq!(inbound[1]["kind"], "DEFINES");
    assert_eq!(inbound[1]["count"], 1);
    assert_eq!(inbound[1]["provenance"]["syntactic"], 1);
    assert_eq!(
        body["edges"]["out"].as_array().expect("edge summary").len(),
        0,
        "a leaf Symbol has no outbound edges"
    );
}

#[tokio::test]
async fn node_distinguishes_malformed_ids_unknown_repos_and_missing_nodes() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let (status, body) = h.verb("node", json!({ "id": "not-an-id" })).await;
    assert_eq!(status, 400, "malformed id: {body}");
    let reason = body["error"].as_str().expect("the error envelope");
    assert!(reason.contains("not-an-id"), "names the bad id: {reason}");

    let (status, body) = h
        .verb(
            "node",
            json!({ "id": "sym:github.com/no/such:main.go#Hello" }),
        )
        .await;
    assert_eq!(status, 404, "unknown repo: {body}");

    let id = format!("sym:{}:main.go#Nonexistent", h.qualifier());
    let (status, body) = h.verb("node", json!({ "id": id })).await;
    assert_eq!(status, 404, "missing node: {body}");
}

#[tokio::test]
async fn node_on_a_registered_but_unindexed_repo_says_so_instead_of_erroring() {
    let h = Harness::boot().await;
    h.add_repo().await; // registered, but no sync or index has run

    let id = format!("sym:{}:main.go#Hello", h.qualifier());
    let (status, body) = h.verb("node", json!({ "id": id })).await;
    assert_eq!(status, 404, "{body}");
    let reason = body["error"].as_str().expect("the error envelope");
    assert!(
        reason.contains("not yet indexed"),
        "tells the caller to retry, not that the repo is unknown: {reason}"
    );
}

#[tokio::test]
async fn neighbors_returns_the_adjacent_subgraph_with_full_edge_detail() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:main.go", h.qualifier());
    // Filtered to the code edges this test is about: main.go is also
    // TOUCHES-linked to the commit that added it, which the default
    // traversal would surface (see the dedup test) but isn't the subject
    // here.
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": id, "edge_kinds": ["DEFINES", "CALLS"] }),
        )
        .await;

    let mut node_ids: Vec<&str> = body["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|n| n["id"].as_str().expect("external id"))
        .collect();
    node_ids.sort();
    assert_eq!(
        node_ids,
        vec![
            format!("sym:{}:main.go#Hello", h.qualifier()),
            format!("sym:{}:main.go#main", h.qualifier()),
        ],
        "main.go's neighbors are exactly the symbols it defines"
    );

    // The induced subgraph: both DEFINES edges from the File, plus the
    // CALLS edge between the two reached Symbols.
    let edges = body["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 3, "{body}");
    for edge in edges {
        assert_eq!(edge["provenance"], "syntactic");
        let confidence = edge["confidence"].as_f64().expect("confidence");
        assert!(
            confidence > 0.0 && confidence <= 1.0,
            "confidence in (0,1]: {edge}"
        );
    }
    let defines: Vec<_> = edges.iter().filter(|e| e["kind"] == "DEFINES").collect();
    assert_eq!(defines.len(), 2, "{body}");
    for edge in defines {
        assert_eq!(edge["src"], id, "edges keep their stored direction");
    }
    let calls: Vec<_> = edges.iter().filter(|e| e["kind"] == "CALLS").collect();
    assert_eq!(calls.len(), 1, "{body}");
    assert_eq!(calls[0]["src"], node_ids[1], "main calls Hello");
    assert_eq!(calls[0]["dst"], node_ids[0], "main calls Hello");
    // Issue #6: call-site locations ride the edge all the way to the
    // wire. The fixture's one call sits on main.go line 8.
    assert_eq!(calls[0]["location"], "main.go:8:10", "{body}");
    assert!(
        body["next_cursor"].is_null(),
        "two neighbors fit one page: {body}"
    );
}

/// A harness around lib.go declaring five symbols (A through E): the
/// shape the pagination tests slice into pages.
async fn five_symbol_harness() -> Harness {
    Harness::boot_with(&[(
        "lib.go",
        "package lib\n\nfunc A() {}\n\nfunc B() {}\n\nfunc C() {}\n\nfunc D() {}\n\nfunc E() {}\n",
    )])
    .await
}

#[tokio::test]
async fn neighbors_paginates_with_a_cursor_without_gaps_or_duplicates() {
    // Five symbols in one file: three pages at limit 2.
    let h = five_symbol_harness().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:lib.go", h.qualifier());
    let mut seen: Vec<String> = Vec::new();
    let mut edges_seen = 0;
    let mut cursor = serde_json::Value::Null;
    let mut pages = 0;
    loop {
        // DEFINES only: lib.go is also TOUCHES-linked to its commit, which
        // would otherwise add a sixth neighbor and a sixth edge to the
        // five-symbol shape this test paginates.
        let mut req = json!({ "id": id, "limit": 2, "edge_kinds": ["DEFINES"] });
        if !cursor.is_null() {
            req["cursor"] = cursor.clone();
        }
        let body = h.verb_ok("neighbors", req).await;
        let nodes = body["nodes"].as_array().expect("nodes");
        assert!(nodes.len() <= 2, "pages respect the limit: {body}");
        seen.extend(
            nodes
                .iter()
                .map(|n| n["id"].as_str().expect("external id").to_string()),
        );
        edges_seen += body["edges"].as_array().expect("edges").len();
        pages += 1;
        cursor = body["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
        assert!(pages < 10, "pagination must terminate");
    }

    assert_eq!(pages, 3, "five symbols at limit 2 are three pages");
    let mut expected: Vec<String> = ["A", "B", "C", "D", "E"]
        .iter()
        .map(|name| format!("sym:{}:lib.go#{name}", h.qualifier()))
        .collect();
    expected.sort();
    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted, expected, "no gaps, no duplicates: {seen:?}");
    assert_eq!(
        edges_seen, 5,
        "each symbol's DEFINES edge arrives exactly once across pages"
    );
}

#[tokio::test]
async fn neighbors_depth_reaches_across_hops_breadth_first() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // From Hello, following only DEFINES (the CALLS edge main → Hello
    // would otherwise make the sibling adjacent in one hop): hop 1 is
    // its defining File, hop 2 crosses the File to the sibling Symbol.
    let origin = format!("sym:{}:main.go#Hello", h.qualifier());
    let file = format!("file:{}:main.go", h.qualifier());
    let sibling = format!("sym:{}:main.go#main", h.qualifier());

    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": origin, "depth": 1, "edge_kinds": ["DEFINES"] }),
        )
        .await;
    let ids: Vec<&str> = body["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![file.as_str()], "depth 1 stops at the File");

    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": origin, "depth": 2, "edge_kinds": ["DEFINES"] }),
        )
        .await;
    let ids: Vec<&str> = body["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![file.as_str(), sibling.as_str()],
        "depth 2 adds the sibling, in breadth-first order"
    );
    // The sibling arrived through the File, not the origin: its edge
    // joins it to the subgraph at hop 1.
    let edges = body["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 2, "{body}");
    assert!(
        edges
            .iter()
            .any(|e| e["src"] == file.as_str() && e["dst"] == sibling.as_str()),
        "the sibling's DEFINES edge is part of the subgraph: {body}"
    );
}

#[tokio::test]
async fn neighbors_default_traversal_dedups_a_node_reachable_by_two_kinds_and_depths() {
    // The default traversal follows every kind, so the new CALLS edges
    // make the graph denser and a node reachable more than one way. A
    // node found at one depth must never reappear at another, and each
    // edge of the induced subgraph must arrive exactly once — pin the
    // mixed-kind BFS that no kind-filtered test exercises.
    //
    //   leaf  <- mid (CALLS)   <- top (CALLS)
    //   leaf, mid, top all DEFINES-linked to chain.go
    // From `top`, leaf is reachable at depth 2 two ways: chain.go
    // DEFINES leaf, and mid CALLS leaf. It must appear once.
    let h = Harness::boot_with(&[(
        "chain.go",
        "package lib\n\nfunc leaf() {}\n\nfunc mid() {\n\tleaf()\n}\n\nfunc top() {\n\tmid()\n}\n",
    )])
    .await;
    h.add_repo().await;
    h.sync_and_index().await;

    let q = h.qualifier();
    let head = git(&h.repo_dir, &["rev-parse", "HEAD"]);
    let top = format!("sym:{q}:chain.go#top");
    let body = h
        .verb_ok("neighbors", json!({ "id": top, "depth": 2, "limit": 1000 }))
        .await;

    let mut ids: Vec<&str> = body["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .map(|n| n["id"].as_str().unwrap())
        .collect();
    let unique: std::collections::BTreeSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        ids.len(),
        unique.len(),
        "no node appears twice across depths/kinds: {ids:?}"
    );
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![
            // The commit that touched chain.go, reached at depth 2 through
            // the file's inbound TOUCHES — history joins the default
            // traversal like any other edge, and must dedup the same way.
            format!("commit:{q}:{head}"),
            format!("file:{q}:chain.go"),
            format!("sym:{q}:chain.go#leaf"),
            format!("sym:{q}:chain.go#mid"),
        ],
        "top reaches its file, the commit that touched it, and \
         (transitively) mid and leaf within 2 hops"
    );

    // Every induced edge once: top's DEFINES (from file) + mid's +
    // leaf's = 3 DEFINES, plus top→mid and mid→leaf CALLS, plus the
    // commit→chain.go TOUCHES joining the reached commit = 6. The
    // file→top DEFINES edge sits on the origin's own page.
    let edges = body["edges"].as_array().expect("edges");
    let mut seen: Vec<(String, String, String)> = edges
        .iter()
        .map(|e| {
            (
                e["src"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
                e["dst"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    seen.sort();
    let dedup_len = {
        let mut u = seen.clone();
        u.dedup();
        u.len()
    };
    assert_eq!(seen.len(), dedup_len, "no edge appears twice: {seen:?}");
    assert_eq!(
        seen.len(),
        6,
        "3 DEFINES + 2 CALLS + 1 TOUCHES in the induced subgraph: {seen:?}"
    );
}

#[tokio::test]
async fn neighbors_rejects_nonsense_pagination_and_depth() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:main.go", h.qualifier());
    for (field, value) in [
        ("limit", json!(0)),
        ("limit", json!(1001)),
        ("depth", json!(0)),
        ("depth", json!(4)),
        ("cursor", json!("not-a-cursor")),
        // Empty kind filters are ambiguous and would render as `IN ()`.
        ("edge_kinds", json!([])),
    ] {
        let mut req = json!({ "id": id });
        req[field] = value.clone();
        let (status, body) = h.verb("neighbors", req).await;
        assert_eq!(status, 400, "{field}={value} must be rejected: {body}");
    }
}

#[tokio::test]
async fn neighbors_filters_by_direction_and_edge_kind() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let symbol = format!("sym:{}:main.go#Hello", h.qualifier());
    let file = format!("file:{}:main.go", h.qualifier());

    // Both of the Symbol's edges point in (its defining File, its
    // caller): direction "in" finds them, "out" finds nothing.
    let body = h
        .verb_ok("neighbors", json!({ "id": symbol, "direction": "in" }))
        .await;
    let nodes = body["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 2, "{body}");
    assert_eq!(nodes[0]["id"], file);
    assert_eq!(nodes[1]["name"], "main", "main calls Hello");

    let body = h
        .verb_ok("neighbors", json!({ "id": symbol, "direction": "out" }))
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 0, "{body}");
    assert_eq!(body["edges"].as_array().expect("edges").len(), 0, "{body}");

    // Kind filters: the matching kind keeps the subgraph, a different
    // kind empties it.
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": file, "edge_kinds": ["DEFINES"] }),
        )
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 2, "{body}");

    let body = h
        .verb_ok("neighbors", json!({ "id": file, "edge_kinds": ["CALLS"] }))
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 0, "{body}");

    // Direction is a closed enum; anything else is the client's mistake.
    let (status, body) = h
        .verb("neighbors", json!({ "id": file, "direction": "sideways" }))
        .await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn a_pointer_swap_is_picked_up_by_the_next_query_without_restart() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let new_symbol = format!("sym:{}:main.go#Goodbye", h.qualifier());
    let (status, _) = h.verb("node", json!({ "id": new_symbol })).await;
    assert_eq!(status, 404, "the symbol does not exist at the first commit");

    // A new commit lands and is re-indexed: the pointer swaps to the new
    // Shard while the server keeps running.
    std::fs::write(
        h.repo_dir.join("main.go"),
        "package main\n\nfunc Hello() string {\n\treturn \"hello\"\n}\n\nfunc Goodbye() string {\n\treturn \"bye\"\n}\n\nfunc main() {\n\tprintln(Hello())\n}\n",
    )
    .unwrap();
    git(&h.repo_dir, &["add", "."]);
    git(&h.repo_dir, &["commit", "-m", "add Goodbye"]);
    h.add_repo().await; // re-registering re-queues a fetch (poll loop stand-in)
    h.sync_and_index().await;

    let body = h.verb_ok("node", json!({ "id": new_symbol })).await;
    assert_eq!(body["node"]["name"], "Goodbye");
}

#[tokio::test]
async fn node_serves_file_nodes_with_their_outbound_defines_summary() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:main.go", h.qualifier());
    let body = h.verb_ok("node", json!({ "id": id })).await;

    assert_eq!(body["node"]["id"], id);
    assert_eq!(body["node"]["kind"], "File");
    assert_eq!(body["node"]["path"], "main.go");
    // main.go declares Hello and main: one outbound DEFINES kind, two edges.
    let out = body["edges"]["out"].as_array().expect("edge summary");
    assert_eq!(out.len(), 1, "{body}");
    assert_eq!(out[0]["kind"], "DEFINES");
    assert_eq!(out[0]["count"], 2);
    assert_eq!(out[0]["provenance"]["syntactic"], 2);
}

#[tokio::test]
async fn package_nodes_are_addressable_and_reached_over_imports_edges() {
    let h = Harness::boot_with(&[
        ("go.mod", "module example.com/mod\n\ngo 1.22\n"),
        (
            "main.go",
            "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(\"hi\")\n}\n",
        ),
    ])
    .await;
    h.add_repo().await;
    h.sync_and_index().await;

    // The imported package is a first-class node: `node` answers for
    // its external id, with the IMPORTS edge in its summary.
    let pkg = format!("pkg:{}:fmt", h.qualifier());
    let body = h.verb_ok("node", json!({ "id": pkg })).await;
    assert_eq!(body["node"]["id"], pkg, "{body}");
    assert_eq!(body["node"]["kind"], "Package");
    assert_eq!(body["node"]["name"], "fmt");
    let inbound = body["edges"]["in"].as_array().expect("edge summary");
    assert_eq!(inbound.len(), 1, "{body}");
    assert_eq!(inbound[0]["kind"], "IMPORTS");
    assert_eq!(inbound[0]["provenance"]["syntactic"], 1);

    // And `neighbors` reaches it from the importing file, the edge
    // locating the import spec.
    let file = format!("file:{}:main.go", h.qualifier());
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": file, "edge_kinds": ["IMPORTS"] }),
        )
        .await;
    let nodes = body["nodes"].as_array().expect("nodes");
    assert_eq!(nodes.len(), 1, "{body}");
    assert_eq!(nodes[0]["id"], pkg);
    let edges = body["edges"].as_array().expect("edges");
    assert_eq!(edges.len(), 1, "{body}");
    assert_eq!(edges[0]["src"], file);
    assert_eq!(edges[0]["dst"], pkg);
    assert_eq!(edges[0]["location"], "main.go:3:8");
}

/// Forge a neighbors cursor the way only a client tampering with (or
/// outliving) one could: the wire format is opaque base64 JSON.
fn forged_cursor(fields: serde_json::Value) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(fields.to_string())
}

#[tokio::test]
async fn a_cursor_replayed_against_a_different_request_is_rejected() {
    // Five symbols so limit 2 leaves a continuation cursor.
    let h = five_symbol_harness().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:lib.go", h.qualifier());
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": id, "limit": 2, "direction": "out" }),
        )
        .await;
    let cursor = body["next_cursor"].as_str().expect("a second page exists");

    // The pages of one traversal must share origin and filters; a
    // contradicting replay is the client's bug, said out loud.
    for (field, value) in [
        ("direction", json!("in")),
        ("edge_kinds", json!(["CALLS"])),
        ("depth", json!(2)),
        ("id", json!(format!("sym:{}:lib.go#A", h.qualifier()))),
    ] {
        let mut req = json!({ "id": id, "cursor": cursor });
        req[field] = value.clone();
        let (status, body) = h.verb("neighbors", req).await;
        assert_eq!(status, 400, "{field}={value} must be rejected: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("cursor"),
            "the reason names the cursor: {body}"
        );
    }

    // Repeating the original filters (or omitting them — the cursor
    // remembers) continues the walk. Only the page size is
    // per-request: omitted here, the default swallows the remaining
    // three symbols in one page.
    let body = h
        .verb_ok("neighbors", json!({ "id": id, "cursor": cursor }))
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 3, "{body}");
    assert!(
        body["next_cursor"].is_null(),
        "the walk is complete: {body}"
    );
}

#[tokio::test]
async fn a_cursor_whose_revision_is_gone_says_expired_not_server_error() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let id = format!("file:{}:main.go", h.qualifier());
    let cursor = forged_cursor(json!({
        "rev": "0000000000000000000000000000000000000000-syntactic-v2",
        "id": id,
        "direction": null,
        "edge_kinds": null,
        "depth": 1,
        "after_depth": 1,
        "after": id,
    }));
    let (status, body) = h
        .verb("neighbors", json!({ "id": id, "cursor": cursor }))
        .await;
    assert_eq!(status, 410, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("restart the traversal"),
        "tells the client the way forward: {body}"
    );
}

#[tokio::test]
async fn the_same_repo_via_a_second_scheme_conflicts_instead_of_shadowing() {
    let h = Harness::boot().await;

    let created = post_repo(
        &h.base,
        json!({"url": "https://gitlab.example/acme/widgets"}),
    )
    .await;
    assert_eq!(created.status().as_u16(), 201);

    // http and https strip to the same qualifier; external ids must
    // resolve to exactly one repo, so the second registration is a
    // conflict, not a coin flip at query time.
    let conflicted = post_repo(
        &h.base,
        json!({"url": "http://gitlab.example/acme/widgets"}),
    )
    .await;
    assert_eq!(conflicted.status().as_u16(), 409);
    let body: serde_json::Value = conflicted.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("gitlab.example/acme/widgets"),
        "names the colliding qualifier: {body}"
    );
}

#[tokio::test]
async fn a_cursor_replay_accepts_equivalent_spellings_of_the_same_request() {
    let h = five_symbol_harness().await;
    h.add_repo().await;
    h.sync_and_index().await;

    // Page 1 leaves direction and depth implicit.
    let id = format!("file:{}:lib.go", h.qualifier());
    let body = h
        .verb_ok("neighbors", json!({ "id": id, "limit": 2 }))
        .await;
    let cursor = body["next_cursor"].as_str().expect("a second page exists");

    // Spelling the defaults out — direction "both", depth 1 — is the
    // same request, not a contradiction.
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": id, "cursor": cursor, "direction": "both", "depth": 1, "limit": 2 }),
        )
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 2, "{body}");

    // And kind order carries no meaning: a reordered filter list on
    // replay matches the cursor's.
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": id, "limit": 2, "edge_kinds": ["DEFINES", "CALLS"] }),
        )
        .await;
    let cursor = body["next_cursor"].as_str().expect("a second page exists");
    let body = h
        .verb_ok(
            "neighbors",
            json!({ "id": id, "cursor": cursor, "edge_kinds": ["CALLS", "DEFINES"], "limit": 2 }),
        )
        .await;
    assert_eq!(body["nodes"].as_array().expect("nodes").len(), 2, "{body}");
}

#[tokio::test]
async fn a_forge_url_minting_unaddressable_ids_is_refused_at_registration() {
    let h = Harness::boot().await;

    // An IPv6 host puts colons in the qualifier that the id grammar
    // cannot tell apart from the qualifier/local boundary: better a 400
    // with the reason now than an indexed repo no query can reach.
    let resp = post_repo(&h.base, json!({"url": "https://[::1]:8443/acme/widgets"})).await;
    assert_eq!(resp.status().as_u16(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot address"),
        "says why the URL is refused: {body}"
    );
}
