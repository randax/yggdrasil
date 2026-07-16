//! End-to-end coverage for the API protection boundaries added in issue #58.

mod common;

use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use common::*;
use serde_json::{Value, json};

async fn boot_with_protection(
    protection: yg_api::ProtectionConfig,
) -> (yg_api::RunningServer, String, String) {
    let db_name = create_test_db().await;
    let server = yg_api::serve_with_metrics_and_protection(
        test_config(&db_name),
        yg_api::Metrics::new(),
        yg_api::MetricsAccess::Admin,
        protection,
    )
    .await
    .expect("protected server boots");
    let base = format!("http://{}", server.local_addr());
    (server, base, db_name)
}

async fn issue(base: &str, member: &str, expires_in_seconds: Option<u64>) -> Value {
    let response = reqwest::Client::new()
        .post(format!("{base}/v1/admin/tokens"))
        .bearer_auth(TEST_TOKEN)
        .json(&json!({
            "member": member,
            "expires_in_seconds": expires_in_seconds,
        }))
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 201, "token issue failed: {body}");
    body
}

async fn member_status(base: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base}/v1/status"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn rate_limit_is_per_token_and_returns_retry_after() {
    let db_name = create_test_db().await;
    let control = control_plane(&db_name).await;
    let first = control
        .issue_member_token(yg_control::MemberName::new("first").unwrap(), None)
        .await
        .unwrap();
    let second = control
        .issue_member_token(yg_control::MemberName::new("second").unwrap(), None)
        .await
        .unwrap();
    let protection = yg_api::ProtectionConfig {
        token_rate_limit: yg_api::TokenRateLimitConfig {
            requests: NonZeroU32::new(1).unwrap(),
            window: Duration::from_secs(60),
        },
        ..yg_api::ProtectionConfig::default()
    };
    let server = yg_api::serve_with_metrics_and_protection(
        test_config(&db_name),
        yg_api::Metrics::new(),
        yg_api::MetricsAccess::Admin,
        protection,
    )
    .await
    .unwrap();
    let base = format!("http://{}", server.local_addr());

    assert_eq!(member_status(&base, &first.token).await.status(), 200);
    let limited = member_status(&base, &first.token).await;
    assert_eq!(limited.status(), 429);
    let retry_after = limited
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .expect("429 carries Retry-After")
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    assert!(retry_after >= 1);
    assert_eq!(member_status(&base, &second.token).await.status(), 200);
}

#[tokio::test]
async fn oversized_mcp_batch_is_rejected_as_one_json_rpc_error() {
    let protection = yg_api::ProtectionConfig {
        mcp_batch_size_limit: NonZeroUsize::new(2).unwrap(),
        ..yg_api::ProtectionConfig::default()
    };
    let (_server, base, _db_name) = boot_with_protection(protection).await;
    let batch = json!([
        {"jsonrpc": "2.0", "id": 1, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        {"jsonrpc": "2.0", "id": 3, "method": "tools/list"},
    ]);

    let response = reqwest::Client::new()
        .post(format!("{base}/v1/mcp"))
        .bearer_auth(TEST_TOKEN)
        .json(&batch)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert!(
        body.is_object(),
        "batch rejection is a single error: {body}"
    );
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], Value::Null);
    assert_eq!(body["error"]["code"], -32000);
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains('2')),
        "error names the configured cap: {body}"
    );
}

#[tokio::test]
async fn expired_tokens_are_rejected_and_listed_while_non_expiring_tokens_work() {
    let (_server, base, _db_name) = boot_with_protection(yg_api::ProtectionConfig::default()).await;
    let expiring = issue(&base, "short-lived", Some(1)).await;
    let permanent = issue(&base, "no-expiry", None).await;
    assert!(expiring["expires_at"].is_i64());
    assert!(permanent["expires_at"].is_null());

    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(
        member_status(&base, expiring["token"].as_str().unwrap())
            .await
            .status(),
        401
    );
    assert_eq!(
        member_status(&base, permanent["token"].as_str().unwrap())
            .await
            .status(),
        200
    );

    let response = reqwest::Client::new()
        .get(format!("{base}/v1/admin/tokens"))
        .bearer_auth(TEST_TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let tokens = body["tokens"].as_array().expect("token listing");
    let expired = tokens
        .iter()
        .find(|token| token["id"] == expiring["id"])
        .expect("expired token remains listed");
    assert_eq!(expired["status"], "expired");
    let active = tokens
        .iter()
        .find(|token| token["id"] == permanent["id"])
        .expect("non-expiring token is listed");
    assert_eq!(active["status"], "active");
    assert!(active["expires_at"].is_null());
}
