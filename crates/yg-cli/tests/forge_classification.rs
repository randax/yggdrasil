//! Repository classification against configured Forge records.

mod common;

use common::*;

#[tokio::test]
async fn repo_add_uses_the_adapter_from_a_matching_enterprise_forge_record() {
    assert_repo_add_uses_enterprise_forge("https://github.enterprise.example/acme/widgets").await;
}

#[tokio::test]
async fn http_repo_add_uses_the_matching_https_enterprise_forge_record() {
    assert_repo_add_uses_enterprise_forge("http://github.enterprise.example/acme/widgets").await;
}

async fn assert_repo_add_uses_enterprise_forge(repo_url: &str) {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: "github",
            base_url: "https://github.enterprise.example",
            org_slug: "acme",
            token_env: None,
            api_root: None,
        })
        .await
        .expect("enterprise Forge should be configured");
    let configured_forge_id = control
        .forge_id_by_base_url("https://github.enterprise.example")
        .await
        .expect("configured Forge lookup should succeed")
        .expect("configured Forge should exist");

    let server = yg_api::serve(test_config(&db_name))
        .await
        .expect("server should boot");
    let base = format!("http://{}", server.local_addr());
    let response = post_repo(&base, serde_json::json!({"url": repo_url})).await;
    assert_eq!(response.status(), 201, "repo add should succeed");

    let fetch = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .expect("fetch claim should succeed")
        .expect("repo add should queue a fetch");
    assert_eq!(fetch.forge_kind, "github");
    assert_eq!(fetch.base_url.as_str(), "https://github.enterprise.example");
    assert_eq!(fetch.token_env.as_deref(), Some("YG_GITHUB_TOKEN"));
    assert_eq!(
        control
            .forge_id_by_base_url("https://github.enterprise.example")
            .await
            .expect("canonical Forge lookup should succeed"),
        Some(configured_forge_id)
    );
    assert_eq!(
        control
            .forge_id_by_base_url("http://github.enterprise.example")
            .await
            .expect("HTTP Forge lookup should succeed"),
        None,
        "the typed HTTP spelling must not create a duplicate Forge row"
    );

    let discovery = control
        .claim_due_discovery(std::time::Duration::from_secs(3600))
        .await
        .expect("discovery claim should succeed")
        .expect("configured org should be due");
    assert_eq!(discovery.forge_kind, "github");
    assert_eq!(
        discovery.api_root.as_deref(),
        Some("https://github.enterprise.example/api/v3")
    );
}
