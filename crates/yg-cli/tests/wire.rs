//! Wire-serialization guarantees (issue #67): every REST and MCP body is
//! compact, key-sorted, and byte-deterministic — tool results land in
//! model context windows, and prompt caching is a byte-exact prefix
//! match, so a wasted or wandering byte costs real tokens. Runs against
//! the dev compose stack like e2e.rs (see docs/DEVELOPMENT.md).

mod common;

use common::*;
use serde_json::json;

/// The test's own canonical form, independent of the server's
/// serializer: object keys sorted recursively, compact separators.
fn sorted(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| key.as_str());
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sorted(value)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sorted).collect())
        }
        other => other.clone(),
    }
}

/// Asserts `text` is exactly the canonical serialization of the value it
/// parses to: compact (rejects pretty-printing) and key-sorted.
fn assert_canonical(context: &str, text: &str) {
    let value: serde_json::Value =
        serde_json::from_str(text).unwrap_or_else(|_| panic!("{context} is not JSON: {text}"));
    let canonical = serde_json::to_string(&sorted(&value)).unwrap();
    assert_eq!(
        text, canonical,
        "{context} must serialize compact and key-sorted"
    );
}

/// POST a Verb, returning the raw body text (byte-level assertions must
/// never go through a parse → re-serialize round trip).
async fn raw_verb(base: &str, verb: &str, body: &serde_json::Value) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/verbs/{verb}"))
        .bearer_auth(TEST_TOKEN)
        .json(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "verb {verb} must succeed");
    resp.text().await.unwrap()
}

async fn raw_mcp(base: &str, body: &serde_json::Value) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/mcp"))
        .bearer_auth(TEST_TOKEN)
        .json(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "MCP must answer");
    resp.text().await.unwrap()
}

#[tokio::test]
async fn every_verb_serves_compact_key_sorted_byte_identical_bodies() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let requests = [
        (
            "node",
            json!({"id": format!("sym:{}:main.go#Hello", h.qualifier())}),
        ),
        (
            "neighbors",
            json!({"id": format!("file:{}:main.go", h.qualifier())}),
        ),
        ("search", json!({"query": "Hello"})),
        (
            "history",
            json!({"id": format!("file:{}:main.go", h.qualifier())}),
        ),
    ];
    for (verb, body) in &requests {
        let first = raw_verb(&h.base, verb, body).await;
        assert_canonical(&format!("verb {verb} response"), &first);
        let second = raw_verb(&h.base, verb, body).await;
        assert_eq!(
            first, second,
            "identical {verb} queries against an identical Shard must \
             serve byte-identical bodies"
        );
    }

    // Scores are f32s; canonicalization must not widen them into
    // seventeen-digit f64 forms ("5.480152130126953") on the wire.
    let search = raw_verb(&h.base, "search", &json!({"query": "Hello"})).await;
    let search: serde_json::Value = serde_json::from_str(&search).unwrap();
    for hit in search["hits"].as_array().expect("hits") {
        let score = hit["score"].as_f64().expect("score");
        let shortest: f64 = format!("{}", score as f32).parse().unwrap();
        assert_eq!(
            score, shortest,
            "scores must keep their f32-shortest form on the wire"
        );
    }

    // The Skill routes agents through `yg … --json`; the CLI must not
    // undo the server's compaction at the last hop.
    let node_id = format!("sym:{}:main.go#Hello", h.qualifier());
    let out = h.yg_ok(&["node", &node_id, "--json"]).await;
    assert_canonical("yg node --json output", out.trim_end());
    let out = h.yg_ok(&["status", "--json"]).await;
    assert_canonical("yg status --json output", out.trim_end());
}

#[tokio::test]
async fn rejections_and_unroutable_requests_keep_the_canonical_error_shape() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();

    // Malformed JSON to a Verb: axum's default rejection would answer
    // in text/plain, breaking the server's one error shape.
    let resp = client
        .post(format!("{base}/v1/verbs/node"))
        .bearer_auth(TEST_TOKEN)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let text = resp.text().await.unwrap();
    assert_canonical("verb parse rejection", &text);
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(body["error"].is_string(), "error shape: {body}");

    // Malformed JSON to MCP: a canonical JSON-RPC parse error.
    let resp = client
        .post(format!("{base}/v1/mcp"))
        .bearer_auth(TEST_TOKEN)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let text = resp.text().await.unwrap();
    assert_canonical("MCP parse rejection", &text);
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        body["error"]["code"], -32700,
        "JSON-RPC parse error: {body}"
    );

    // A wrong method on a live route must not fall back to axum's
    // empty-body 405.
    let resp = client
        .get(format!("{base}/v1/verbs/node"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 405);
    let text = resp.text().await.unwrap();
    assert_canonical("method-not-allowed response", &text);
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(body["error"].is_string(), "error shape: {body}");
}

#[tokio::test]
async fn mcp_responses_are_compact_key_sorted_and_stable_within_a_session() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let first = raw_mcp(&h.base, &list).await;
    assert_canonical("tools/list response", &first);
    let second = raw_mcp(&h.base, &list).await;
    assert_eq!(
        first, second,
        "tools/list must be byte-stable across calls within a session"
    );

    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {"name": "search", "arguments": {"query": "Hello"}}
    });
    let envelope = raw_mcp(&h.base, &call).await;
    assert_canonical("tools/call response", &envelope);
    assert_eq!(
        envelope,
        raw_mcp(&h.base, &call).await,
        "identical tools/call against an identical Shard must serve \
         byte-identical envelopes"
    );

    let parsed: serde_json::Value = serde_json::from_str(&envelope).unwrap();
    let text = parsed["result"]["content"][0]["text"]
        .as_str()
        .expect("tool result carries text content");
    assert_canonical("tool result text content", text);
    assert_eq!(
        text,
        serde_json::to_string(&sorted(&parsed["result"]["structuredContent"])).unwrap(),
        "text content must be the canonical serialization of structuredContent"
    );
}

#[tokio::test]
async fn status_body_holds_no_volatile_fields_and_uptime_lives_in_a_header() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());
    let get = || async {
        reqwest::Client::new()
            .get(format!("{base}/v1/status"))
            .bearer_auth(TEST_TOKEN)
            .send()
            .await
            .unwrap()
    };

    let first = get().await;
    let uptime = first
        .headers()
        .get("x-yggdrasil-uptime-seconds")
        .expect("uptime is volatile, so it rides in a header")
        .to_str()
        .unwrap()
        .to_string();
    uptime.parse::<u64>().expect("uptime header is seconds");
    let first_body = first.text().await.unwrap();
    assert_canonical("status response", &first_body);
    assert!(
        !first_body.contains("uptime_seconds"),
        "volatile fields must not appear in the body: {first_body}"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let second_body = get().await.text().await.unwrap();
    assert_eq!(
        first_body, second_body,
        "status bodies must be byte-identical across requests"
    );
}
