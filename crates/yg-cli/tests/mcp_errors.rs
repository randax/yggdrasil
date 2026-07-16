//! MCP Verb failures stay tool results while preserving the engine's
//! client-actionable error categories.

mod common;

use base64::Engine;
use common::*;
use object_store::ObjectStoreExt;
use serde_json::json;

async fn call_tool(h: &Harness, name: &str, arguments: serde_json::Value) -> serde_json::Value {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/mcp", h.base))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status.as_u16(), 200, "MCP returned {status}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|_| panic!("MCP answered non-JSON: {text}"))
}

fn assert_tool_error(body: &serde_json::Value, kind: &str, message_fragment: &str) {
    assert!(
        body.get("error").is_none(),
        "Verb failures are tool results: {body}"
    );
    assert_eq!(body["result"]["isError"], true, "{body}");
    let structured = &body["result"]["structuredContent"];
    assert_eq!(structured["error"]["kind"], kind, "{body}");
    assert!(
        structured["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains(message_fragment)),
        "{body}"
    );
    let text: serde_json::Value = serde_json::from_str(
        body["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error has text content"),
    )
    .expect("tool error text is structured JSON");
    assert_eq!(&text, structured, "text and structured error content agree");
}

#[tokio::test]
async fn verb_tool_errors_preserve_not_found_gone_and_unavailable() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let not_found = call_tool(
        &h,
        "node",
        json!({"id": "sym:github.com/no/such:main.go#Hello"}),
    )
    .await;
    assert_tool_error(&not_found, "not_found", "no indexed repository");

    let id = format!("file:{}:main.go", h.qualifier());
    let missing_revision = format!(
        "0000000000000000000000000000000000000000-{}-v{}",
        yg_shard::SYNTACTIC_PASS,
        yg_shard::SCHEMA_VERSION
    );
    let pool = sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
        .await
        .unwrap();
    let (repo_id, commit): (i64, String) = sqlx::query_as("SELECT repo_id, commit_sha FROM shards")
        .fetch_one(&pool)
        .await
        .unwrap();
    let payload = json!({
        "repo_id": repo_id,
        "rev": missing_revision,
        "id": id,
        "direction": null,
        "edge_kinds": null,
        "depth": 1,
        "after_depth": 1,
        "after": id,
    });
    let cursor = signed_cursor(&payload);
    let gone = call_tool(&h, "neighbors", json!({"id": id, "cursor": cursor})).await;
    assert_tool_error(&gone, "gone", "restart the traversal");

    let unsigned = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
    let invalid = call_tool(&h, "neighbors", json!({"id": id, "cursor": unsigned})).await;
    assert_tool_error(&invalid, "bad_request", "invalid cursor");
    let message = invalid["result"]["structuredContent"]["error"]["message"]
        .as_str()
        .expect("typed MCP error message");
    assert!(
        !message.contains("restart the traversal") && !message.contains(&missing_revision),
        "an unsigned cursor cannot probe revision existence: {invalid}"
    );

    let old_revision = format!(
        "{commit}-{}-v{}",
        yg_shard::SYNTACTIC_PASS,
        yg_shard::SCHEMA_VERSION - 1
    );
    let manifest_key = yg_shard::manifest_key(repo_id, &old_revision);
    let manifest = json!({
        "schema_version": yg_shard::SCHEMA_VERSION - 1,
        "commit": commit,
        "pass": yg_shard::SYNTACTIC_PASS,
        "counts": {"nodes": 0, "edges": 0},
        "segments": {}
    });
    h.store
        .put(
            &manifest_key.as_str().into(),
            serde_json::to_vec(&manifest).unwrap().into(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE shards SET revision = $1, manifest_key = $2 WHERE repo_id = $3")
        .bind(&old_revision)
        .bind(&manifest_key)
        .bind(repo_id)
        .execute(&pool)
        .await
        .unwrap();

    let unavailable = call_tool(&h, "node", json!({"id": id})).await;
    assert_tool_error(&unavailable, "unavailable", "try again shortly");
}

#[tokio::test]
async fn ambiguous_fuzzy_address_is_a_successful_mcp_tool_result() {
    let h = Harness::boot_with(&[
        ("alpha/service.go", "package alpha\n\nfunc Resolve() {}\n"),
        ("beta/service.go", "package beta\n\nfunc Resolve() {}\n"),
    ])
    .await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo = h.qualifier();

    let ambiguous = call_tool(&h, "node", json!({"id": "Resolve", "repo": repo})).await;

    assert!(ambiguous.get("error").is_none(), "{ambiguous}");
    assert_eq!(ambiguous["result"]["isError"], false, "{ambiguous}");
    let structured = &ambiguous["result"]["structuredContent"];
    assert_eq!(structured["resolution"], "ambiguous", "{ambiguous}");
    assert_eq!(
        structured["candidates"]
            .as_array()
            .expect("ambiguous result has candidates")
            .len(),
        2
    );
    assert_eq!(structured["total_matches"].as_u64(), Some(2));
}

#[tokio::test]
async fn no_such_symbol_mcp_error_preserves_typed_fuzzy_detail() {
    let h =
        Harness::boot_with(&[("alpha/service.go", "package alpha\n\nfunc Resolve() {}\n")]).await;
    h.add_repo().await;
    h.sync_and_index().await;
    let repo = h.qualifier();

    let missing = call_tool(&h, "node", json!({"id": "Missing", "repo": repo})).await;

    assert_tool_error(&missing, "not_found", "no such symbol");
    assert_eq!(
        missing["result"]["structuredContent"]["error"]["detail"],
        json!({
            "kind": "no_such_symbol",
            "address": {
                "name": "Missing",
                "repo": repo,
            }
        }),
        "{missing}"
    );
}
