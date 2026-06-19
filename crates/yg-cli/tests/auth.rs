//! Member bearer-token lifecycle and authorization.

mod common;

use common::*;
use serde_json::json;

#[tokio::test]
async fn member_token_can_call_verbs_but_not_admin_and_revocation_is_immediate() {
    let h = Harness::boot().await;
    h.add_repo().await;
    h.sync_and_index().await;
    let id = format!("sym:{}:main.go#Hello", h.qualifier());

    let issued = h.yg_ok(&["admin", "token", "issue", "alice"]).await;
    let token_id = field(&issued, "id:");
    let token = field(&issued, "token:");
    assert_eq!(
        issued.matches(&token).count(),
        1,
        "the issued token is shown exactly once:\n{issued}"
    );

    let (status, body) =
        post_with_token(&h.base, "/v1/verbs/node", &token, Some(json!({ "id": id }))).await;
    assert_eq!(status, 200, "member token may call Verbs: {body}");
    assert_eq!(body["node"]["id"], id);

    let (token_hash, last_used_at): (String, Option<String>) =
        sqlx::query_as("SELECT token_hash, last_used_at::text FROM member_tokens WHERE id = $1")
            .bind(&token_id)
            .fetch_one(
                &sqlx::PgPool::connect(&format!("{DEV_POSTGRES}/{}", h.db_name))
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_ne!(token_hash, token, "member tokens are hashed at rest");
    assert!(
        !token_hash.contains(&token),
        "the stored hash must not embed the bearer token"
    );
    assert!(
        last_used_at.is_some(),
        "successful member auth tracks last use"
    );

    let (status, body) = get_with_token(&h.base, "/v1/admin/status", &token).await;
    assert_eq!(status, 403, "member token must not call admin: {body}");

    let revoked = h.yg_ok(&["admin", "token", "revoke", &token_id]).await;
    assert!(
        revoked.contains(&token_id),
        "revoke output names the revoked token id:\n{revoked}"
    );

    let (status, body) =
        post_with_token(&h.base, "/v1/verbs/node", &token, Some(json!({ "id": id }))).await;
    assert_eq!(status, 401, "revoked member token must be rejected: {body}");
}

fn field(output: &str, label: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix(label).map(str::trim))
        .unwrap_or_else(|| panic!("missing {label} in:\n{output}"))
        .to_string()
}

async fn post_with_token(
    base: &str,
    path: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value) {
    let client = reqwest::Client::new();
    let mut req = client.post(format!("{base}{path}")).bearer_auth(token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    response(req.send().await.unwrap()).await
}

async fn get_with_token(base: &str, path: &str, token: &str) -> (u16, serde_json::Value) {
    response(
        reqwest::Client::new()
            .get(format!("{base}{path}"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn response(resp: reqwest::Response) -> (u16, serde_json::Value) {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    let body = serde_json::from_str(&text)
        .unwrap_or_else(|_| panic!("response was non-JSON ({status}): {text}"));
    (status, body)
}
