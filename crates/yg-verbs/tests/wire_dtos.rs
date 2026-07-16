use serde_json::json;

#[test]
fn status_reports_the_typed_verb_contract_and_served_inventory() {
    let response = yg_verbs::status::StatusResponse {
        version: "0.1.0".to_owned(),
        repos_indexed: 2,
        verb_contract_version: yg_verbs::status::VERB_CONTRACT_VERSION,
        verbs: yg_verbs::Verb::ALL.to_vec(),
    };
    let encoded = serde_json::to_string(&response).expect("status serialization");

    assert_eq!(
        encoded,
        r#"{"version":"0.1.0","repos_indexed":2,"verb_contract_version":1,"verbs":["node","neighbors","search","history"]}"#
    );
    let decoded = serde_json::from_str::<yg_verbs::status::StatusResponse>(&encoded)
        .expect("status deserialization");
    assert_eq!(
        decoded.verb_contract_version,
        yg_verbs::status::VERB_CONTRACT_VERSION
    );
    assert_eq!(decoded.verbs, yg_verbs::Verb::ALL);
}

#[test]
fn untruncated_neighbors_keep_the_legacy_wire_bytes() {
    let legacy = r#"{"nodes":[],"edges":[],"next_cursor":null}"#;
    let mut response: yg_verbs::NeighborsResponse =
        serde_json::from_str(legacy).expect("the pre-cap response remains accepted");
    assert!(!response.truncated);
    assert_eq!(serde_json::to_string(&response).unwrap(), legacy);

    response.truncated = true;
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
        r#"{"nodes":[],"edges":[],"next_cursor":null,"truncated":true}"#
    );
}

#[test]
fn history_commit_view_preserves_the_flat_wire_shape() {
    let response = yg_verbs::HistoryResponse {
        commits: vec![yg_verbs::HistoryCommitView {
            commit: yg_verbs::HistoryCommit {
                commit: "commit:github.com/acme/widgets:abc".to_string(),
                sha: "abc".to_string(),
                subject: Some("keep the wire stable".to_string()),
                committed_at: 1,
                author: Some(yg_verbs::HistoryAuthor {
                    id: "contributor:github.com/acme/widgets:ada@example.com".to_string(),
                    name: Some("Ada".to_string()),
                    email: "ada@example.com".to_string(),
                }),
            },
            date: "1970-01-01T00:00:01Z".to_string(),
        }],
        next_cursor: None,
    };
    let expected = r#"{"commits":[{"commit":"commit:github.com/acme/widgets:abc","sha":"abc","subject":"keep the wire stable","committed_at":1,"author":{"id":"contributor:github.com/acme/widgets:ada@example.com","name":"Ada","email":"ada@example.com"},"date":"1970-01-01T00:00:01Z"}],"next_cursor":null}"#;

    let encoded = serde_json::to_string(&response).expect("history response serialization");
    assert_eq!(encoded, expected);
    let decoded: yg_verbs::HistoryResponse =
        serde_json::from_str(&encoded).expect("history response deserialization");
    assert_eq!(
        serde_json::to_string(&decoded).expect("history response reserialization"),
        expected
    );
}

#[test]
fn history_commit_view_omits_absent_optional_fields() {
    let response = yg_verbs::HistoryResponse {
        commits: vec![
            yg_verbs::HistoryCommitView {
                commit: yg_verbs::HistoryCommit {
                    commit: "commit:github.com/acme/widgets:abc".to_string(),
                    sha: "abc".to_string(),
                    subject: None,
                    committed_at: 1,
                    author: None,
                },
                date: "1970-01-01T00:00:01Z".to_string(),
            },
            yg_verbs::HistoryCommitView {
                commit: yg_verbs::HistoryCommit {
                    commit: "commit:github.com/acme/widgets:def".to_string(),
                    sha: "def".to_string(),
                    subject: None,
                    committed_at: 2,
                    author: Some(yg_verbs::HistoryAuthor {
                        id: "contributor:github.com/acme/widgets:ada@example.com".to_string(),
                        name: None,
                        email: "ada@example.com".to_string(),
                    }),
                },
                date: "1970-01-01T00:00:02Z".to_string(),
            },
        ],
        next_cursor: None,
    };
    let expected = r#"{"commits":[{"commit":"commit:github.com/acme/widgets:abc","sha":"abc","committed_at":1,"date":"1970-01-01T00:00:01Z"},{"commit":"commit:github.com/acme/widgets:def","sha":"def","committed_at":2,"author":{"id":"contributor:github.com/acme/widgets:ada@example.com","email":"ada@example.com"},"date":"1970-01-01T00:00:02Z"}],"next_cursor":null}"#;

    assert_eq!(
        serde_json::to_string(&response).expect("history response serialization"),
        expected
    );
}

#[test]
fn admin_identifier_newtypes_preserve_bare_string_wire_values() {
    use yg_verbs::admin::{ForgeBaseUrl, MemberName, OrgName, RepoSlug, TokenId};

    macro_rules! assert_transparent_string {
        ($type:ty, $value:expr) => {{
            let typed = <$type>::new($value.into());
            assert_eq!(
                serde_json::to_value(&typed).expect("newtype serialization"),
                json!($value)
            );
            assert_eq!(typed.as_str(), $value);
            assert_eq!(typed.to_string(), $value);
            let decoded: $type =
                serde_json::from_value(json!($value)).expect("newtype deserialization");
            assert_eq!(decoded.as_str(), $value);
        }};
    }

    assert_transparent_string!(RepoSlug, "acme/widgets");
    assert_transparent_string!(OrgName, "acme");
    assert_transparent_string!(ForgeBaseUrl, "https://github.com");
    assert_transparent_string!(TokenId, "mtok_0123456789abcdefABCDEF01");
    assert_transparent_string!(MemberName, "Ada");
}

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
