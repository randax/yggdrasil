use serde_json::json;

#[test]
fn optional_field_renames_are_rejected_instead_of_becoming_absent() {
    let node = json!({
        "node": {
            "id": "file:github.com/acme/widgets:main.rs",
            "kind": "File",
            "display_name": "main.rs"
        },
        "edges": {"in": [], "out": []}
    });
    let error = serde_json::from_value::<yg_verbs::NodeResponse>(node)
        .expect_err("a renamed optional node field must not be ignored");
    assert!(error.to_string().contains("display_name"), "{error}");

    let history = json!({
        "commits": [{
            "commit": "commit:github.com/acme/widgets:abc",
            "sha": "abc",
            "message": "renamed subject",
            "committed_at": 1,
            "date": "1970-01-01T00:00:01Z"
        }],
        "next_cursor": null
    });
    let error = serde_json::from_value::<yg_verbs::HistoryResponse>(history)
        .expect_err("a renamed optional history field must not be ignored");
    assert!(error.to_string().contains("message"), "{error}");
}

#[test]
fn closed_wire_vocabularies_reject_unknown_values() {
    let node = json!({
        "node": {
            "id": "file:github.com/acme/widgets:main.rs",
            "kind": "Document",
            "path": "main.rs"
        },
        "edges": {"in": [], "out": []}
    });
    let error = serde_json::from_value::<yg_verbs::NodeResponse>(node)
        .expect_err("an unknown node kind must fail typed node parsing");
    assert!(error.to_string().contains("Document"), "{error}");

    let neighbors = json!({
        "nodes": [],
        "edges": [{
            "src": "file:github.com/acme/widgets:main.rs",
            "dst": "sym:github.com/acme/widgets:main.rs#main",
            "kind": "CALLS",
            "provenance": "guessed",
            "confidence": 1.0
        }],
        "next_cursor": null
    });
    let error = serde_json::from_value::<yg_verbs::NeighborsResponse>(neighbors)
        .expect_err("an unknown provenance must fail typed neighbors parsing");
    assert!(error.to_string().contains("guessed"), "{error}");

    let search = json!({
        "hits": [{
            "id": "file:github.com/acme/widgets:main.rs",
            "kind": "Document",
            "repo": "github.com/acme/widgets",
            "score": 1.0
        }],
        "next_cursor": null
    });
    let error = serde_json::from_value::<yg_verbs::SearchWireResponse>(search)
        .expect_err("an unknown node kind must fail typed search parsing");
    assert!(error.to_string().contains("Document"), "{error}");

    let status = json!({
        "repos": [{
            "slug": "acme/widgets",
            "forge": "https://github.com",
            "visibility": "secret",
            "discovery_state": "included",
            "last_synced_commit": null,
            "sync": {"state": "registered", "attempts": 0, "last_error": null},
            "index": {"state": "pending", "attempts": 0, "last_error": null},
            "shard": null
        }],
        "visibility_counts": {"public": 0, "internal": 0, "private": 1}
    });
    let error = serde_json::from_value::<yg_verbs::admin::AdminStatusResponse>(status)
        .err()
        .expect("an unknown visibility must fail typed admin parsing");
    assert!(error.to_string().contains("secret"), "{error}");
}
