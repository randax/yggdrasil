//! Hub-node neighborhood traversal through a real spawned server.

mod common;

use common::*;
use yg_verbs::MAX_NEIGHBORS_EDGES_PER_NODE;

#[tokio::test(flavor = "multi_thread")]
async fn hub_neighborhood_is_bounded_and_marked_truncated() {
    let mut source = String::from("package hub\n\n");
    for ordinal in 0..=MAX_NEIGHBORS_EDGES_PER_NODE {
        source.push_str(&format!("func Fanout{ordinal:04}() {{}}\n"));
    }
    let (fixture, repo_dir, fixture_url) = fixture_repo_with(&[("hub.go", &source)]);
    let db_name = create_test_db().await;
    let (_server, url) = spawn_yg_serve(&db_name, |cmd| {
        cmd.env("YG_BOOTSTRAP_TOKEN", TEST_TOKEN)
            .env("YG_GIT_CACHE", fixture.path().join("git-cache"));
    });

    let added = post_repo(&url, serde_json::json!({"url": fixture_url})).await;
    assert!(added.status().is_success(), "repo add failed: {added:?}");
    await_symbol(
        &url,
        &format!("Fanout{:04}", MAX_NEIGHBORS_EDGES_PER_NODE),
        std::time::Duration::from_secs(60),
    )
    .await;

    let id = format!("file:{}:hub.go", repo_dir.display());
    let response = reqwest::Client::new()
        .post(format!("{url}/v1/verbs/neighbors"))
        .bearer_auth(TEST_TOKEN)
        .json(&serde_json::json!({
            "id": id,
            "edge_kinds": ["DEFINES"],
            "limit": 1000
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(status, 200, "neighbors failed: {body}");
    assert_eq!(
        body["nodes"].as_array().unwrap().len(),
        MAX_NEIGHBORS_EDGES_PER_NODE,
        "the hub traversal must stop at the per-node cap: {body}"
    );
    assert_eq!(
        body["edges"].as_array().unwrap().len(),
        MAX_NEIGHBORS_EDGES_PER_NODE,
        "the hub page must return only retained edges: {body}"
    );
    assert_eq!(body["truncated"], true, "truncation is explicit: {body}");
    assert!(
        body["next_cursor"].is_null(),
        "omitted edges are not cursor pages"
    );

    let output = assert_cmd::Command::cargo_bin("yg")
        .unwrap()
        .env("YG_SERVER", &url)
        .env("YG_TOKEN", TEST_TOKEN)
        .args([
            "neighbors",
            &id,
            "--edge-kinds",
            "DEFINES",
            "--limit",
            "1000",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "CLI failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("warning: neighborhood truncated"),
        "human output must disclose truncation:\n{stderr}"
    );
}
