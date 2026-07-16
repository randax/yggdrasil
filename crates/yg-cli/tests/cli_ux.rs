//! Script-facing CLI contracts: JSON writes, exit classes, and flag names.

mod common;

use std::io::Write;

use common::{TEST_TOKEN, boot_test_server};

fn yg(base: &str, token: &str, args: &[&str]) -> std::process::Output {
    assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", base)
        .env("YG_TOKEN", token)
        .args(args)
        .output()
        .unwrap()
}

fn json_stdout(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("--json stdout must be one JSON document")
}

fn one_shot_server_error() -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let body = br#"{"error":"temporary failure"}"#;
        write!(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });
    (base, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_admin_write_emits_its_typed_response_as_json() {
    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());

    let repo = json_stdout(&yg(
        &base,
        TEST_TOKEN,
        &[
            "admin",
            "repo",
            "add",
            "https://github.com/acme/widgets",
            "--json",
        ],
    ));
    assert_eq!(repo["slug"], "acme/widgets");
    assert_eq!(repo["created"], true);

    let forge = json_stdout(&yg(
        &base,
        TEST_TOKEN,
        &["admin", "forge", "add", "github", "acme", "--json"],
    ));
    assert_eq!(forge["kind"], "github");
    assert_eq!(forge["org"], "acme");

    let discovery = json_stdout(&yg(
        &base,
        TEST_TOKEN,
        &["admin", "forge", "discover", "github", "acme", "--json"],
    ));
    assert_eq!(discovery["org"], "acme");
    assert!(discovery["queued"].is_boolean());

    let rule = json_stdout(&yg(
        &base,
        TEST_TOKEN,
        &[
            "admin",
            "rules",
            "add",
            "acme/private-*",
            "--action",
            "include",
            "--private",
            "--json",
        ],
    ));
    assert_eq!(rule["pattern"], "acme/private-*");
    assert_eq!(rule["action"], "include");

    let issued_output = yg(
        &base,
        TEST_TOKEN,
        &["admin", "token", "issue", "automation", "--json"],
    );
    let issued = json_stdout(&issued_output);
    let id = issued["id"].as_str().expect("token id");
    let secret = issued["token"].as_str().expect("one-time token secret");
    assert!(id.starts_with("mtok_"));
    assert!(secret.starts_with("ygt_"));
    assert_eq!(
        String::from_utf8_lossy(&issued_output.stdout)
            .matches(secret)
            .count(),
        1,
        "the secret occurs exactly once in the JSON document"
    );

    let revoked = json_stdout(&yg(
        &base,
        TEST_TOKEN,
        &["admin", "token", "revoke", id, "--json"],
    ));
    assert_eq!(revoked["id"], id);
    assert_eq!(revoked["revoked"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_use_distinct_scriptable_exit_classes() {
    let usage = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env_remove("YG_SERVER")
        .env_remove("YG_TOKEN")
        .args(["search", "needle", "--not-a-real-flag"])
        .output()
        .unwrap();
    assert_eq!(usage.status.code(), Some(2));

    let server = boot_test_server().await;
    let base = format!("http://{}", server.local_addr());
    let auth = yg(&base, "wrong-token", &["status"]);
    assert_eq!(auth.status.code(), Some(3));

    let missing = yg(
        &base,
        TEST_TOKEN,
        &["search", "needle", "--repo", "github.com/acme/missing"],
    );
    assert_eq!(
        missing.status.code(),
        Some(4),
        "stderr: {}",
        String::from_utf8_lossy(&missing.stderr)
    );

    let transport = yg("http://127.0.0.1:0", TEST_TOKEN, &["status"]);
    assert_eq!(transport.status.code(), Some(5));
}

#[test]
fn http_500_uses_the_server_exit_class() {
    let (base, server) = one_shot_server_error();
    let failure = yg(&base, TEST_TOKEN, &["status"]);
    server.join().unwrap();
    assert_eq!(
        failure.status.code(),
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&failure.stderr)
    );
}
