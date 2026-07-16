//! MCP surface: the Index Server exposes every shipped Verb as a tool
//! over authenticated Streamable HTTP, and `yg mcp` proxies stdio clients
//! to that same endpoint.

mod common;

use common::*;
use serde_json::json;

async fn mcp(base: &str, body: serde_json::Value, token: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/mcp"))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .unwrap_or_else(|_| panic!("MCP answered non-JSON ({status}): {text}"));
    (status, body)
}

fn stdio_message(body: &serde_json::Value) -> String {
    let body = serde_json::to_string(body).unwrap();
    format!("{body}\n")
}

fn read_stdio_message(out: &[u8]) -> serde_json::Value {
    let text = String::from_utf8(out.to_vec()).expect("proxy writes UTF-8 frames");
    serde_json::from_str(text.trim_end_matches(['\r', '\n']))
        .unwrap_or_else(|_| panic!("stdio message is JSON: {text}"))
}

#[tokio::test]
async fn streamable_http_lists_and_calls_verbs_with_bearer_auth() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let (status, body) = mcp(
        &h.base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        TEST_TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let tools = body["result"]["tools"].as_array().expect("tools");
    let names: Vec<_> = tools.iter().map(|tool| &tool["name"]).collect();
    for name in ["node", "neighbors", "search", "history"] {
        assert!(names.contains(&&json!(name)), "{name} missing from {body}");
    }
    let node = tools
        .iter()
        .find(|tool| tool["name"] == "node")
        .expect("node tool");
    assert_eq!(node["inputSchema"]["properties"]["id"]["type"], "string");
    assert!(
        node["inputSchema"]["required"]
            .as_array()
            .expect("required")
            .contains(&json!("id")),
        "node id comes from the shared Verb schema: {node}"
    );
    let search = tools
        .iter()
        .find(|tool| tool["name"] == "search")
        .expect("search tool");
    assert!(
        search["inputSchema"]["anyOf"]
            .as_array()
            .expect("search has fresh-or-resume schema")
            .iter()
            .any(|shape| shape["required"]
                .as_array()
                .expect("required list")
                .contains(&json!("cursor"))),
        "search schema allows cursor-only resume calls: {search}"
    );

    let id = format!("sym:{}:main.go#Hello", h.qualifier());
    let (status, body) = mcp(
        &h.base,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "node",
                "arguments": {"id": id}
            }
        }),
        TEST_TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["structuredContent"]["node"]["name"], "Hello");
    assert_eq!(body["result"]["isError"], false);

    let issued: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/v1/admin/tokens", h.base))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({"member": "mcp-agent"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let member_token = issued["token"].as_str().expect("issued token");
    let (status, body) = mcp(
        &h.base,
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/list"}),
        member_token,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["result"]["tools"]
            .as_array()
            .expect("member can list MCP tools")
            .iter()
            .any(|tool| tool["name"] == "neighbors"),
        "{body}"
    );

    let (status, body) = mcp(
        &h.base,
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/list"}),
        "wrong",
    )
    .await;
    assert_eq!(status, 401, "{body}");

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/mcp", h.base))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    assert_eq!(resp.text().await.unwrap(), "");
}

#[tokio::test]
async fn initialize_negotiates_versions_and_notification_methods_never_get_results() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    for (requested, expected) in [
        ("2024-11-05", "2024-11-05"),
        ("2025-03-26", "2025-03-26"),
        ("2099-01-01", "2025-03-26"),
    ] {
        let (status, body) = mcp(
            &base,
            json!({
                "jsonrpc": "2.0",
                "id": requested,
                "method": "initialize",
                "params": {
                    "protocolVersion": requested,
                    "capabilities": {},
                    "clientInfo": {"name": "conformance-test", "version": "1"}
                }
            }),
            TEST_TOKEN,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["protocolVersion"], expected, "{body}");
    }

    let (status, body) = mcp(
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 16,
            "method": "initialize",
            "params": {"protocolVersion": "2025-03-26"}
        }),
        TEST_TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["error"]["code"], -32602, "{body}");

    let client = reqwest::Client::new();
    for method in ["tools/list", "notifications/initialized"] {
        let response = client
            .post(format!("{base}/v1/mcp"))
            .bearer_auth(TEST_TOKEN)
            .json(&json!({"jsonrpc": "2.0", "method": method}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 202, "{method}");
        assert_eq!(response.text().await.unwrap(), "", "{method}");
    }

    let (status, body) = mcp(
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 17,
            "method": "notifications/initialized"
        }),
        TEST_TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["id"], 17);
    assert_eq!(body["error"]["code"], -32600, "{body}");
    assert!(body.get("result").is_none(), "{body}");

    let (status, body) = mcp(
        &base,
        json!([
            {"jsonrpc": "2.0", "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 18, "method": "tools/list"},
            {"jsonrpc": "2.0", "id": 19, "method": "notifications/initialized"},
            {"jsonrpc": "1.0", "id": 20, "method": "tools/list"},
            {}
        ]),
        TEST_TOKEN,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let entries = body.as_array().expect("mixed batch response");
    assert_eq!(
        entries.len(),
        4,
        "id-less notification must be omitted: {body}"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["id"] == 18 && entry.get("result").is_some()),
        "{body}"
    );
    assert!(
        entries.iter().any(|entry| {
            entry["id"] == 19 && entry["error"]["code"] == -32600 && entry.get("result").is_none()
        }),
        "{body}"
    );
    assert!(
        entries.iter().any(|entry| {
            entry["id"].is_null()
                && entry["error"]["code"] == -32600
                && entry.get("result").is_none()
        }),
        "malformed id-less objects are invalid requests, not notifications: {body}"
    );
    assert!(
        entries.iter().any(|entry| {
            entry["id"] == 20 && entry["error"]["code"] == -32600 && entry.get("result").is_none()
        }),
        "invalid JSON-RPC versions must not reach method dispatch: {body}"
    );
}

#[tokio::test]
async fn yg_mcp_proxies_stdio_frames_to_the_remote_server() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "neighbors",
            "arguments": {
                "id": format!("file:{}:main.go", h.qualifier()),
                "edge_kinds": ["DEFINES"]
            }
        }
    });
    let config_home = tempfile::tempdir().unwrap();
    let config_dir = config_home.path().join("yg");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!("server = \"{}\"\ntoken = \"{}\"\n", h.base, TEST_TOKEN),
    )
    .unwrap();
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"));
    cmd.env_remove("YG_SERVER")
        .env_remove("YG_TOKEN")
        .env("XDG_CONFIG_HOME", config_home.path())
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let input = stdio_message(&request);
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        stdin.write_all(input.as_bytes()).unwrap();
    })
    .await
    .unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "yg mcp failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = read_stdio_message(&out.stdout);
    let nodes = body["result"]["structuredContent"]["nodes"]
        .as_array()
        .expect("proxied neighbors result");
    assert_eq!(nodes.len(), 2, "{body}");
}

#[tokio::test]
async fn yg_mcp_forwards_the_servers_error_envelopes_verbatim() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("yg"));
    cmd.env("YG_SERVER", &base)
        .env("YG_TOKEN", TEST_TOKEN)
        .arg("mcp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        stdin.write_all(b"{not json\n").unwrap();
    })
    .await
    .unwrap();
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "yg mcp failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The server already answers a spec-shaped JSON-RPC parse error;
    // the proxy must hand it through, not wrap it in a synthetic
    // -32000 envelope.
    let body = read_stdio_message(&out.stdout);
    assert_eq!(body["error"]["code"], -32700, "{body}");
    assert_eq!(body["id"], serde_json::Value::Null, "{body}");
}
