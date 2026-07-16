//! Repository classification against configured Forge records.

mod common;

use common::*;

#[tokio::test]
async fn repo_add_uses_the_adapter_from_a_matching_enterprise_forge_record() {
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

    let server = yg_api::serve(test_config(&db_name))
        .await
        .expect("server should boot");
    let base = format!("http://{}", server.local_addr());
    let response = post_repo(
        &base,
        serde_json::json!({"url": "https://github.enterprise.example/acme/widgets"}),
    )
    .await;
    assert_eq!(response.status(), 201, "repo add should succeed");

    let fetch = control
        .claim_due_fetch(std::time::Duration::from_secs(60))
        .await
        .expect("fetch claim should succeed")
        .expect("repo add should queue a fetch");
    assert_eq!(fetch.forge_kind, "github");
    assert_eq!(fetch.token_env.as_deref(), Some("YG_GITHUB_TOKEN"));

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
