//! Whole-request deadline for every API route.

use std::future::Future;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::error_json;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestTimeout(Duration);

impl RequestTimeout {
    pub(crate) fn new(duration: Duration) -> Self {
        Self(duration.max(Duration::from_millis(1)))
    }
}

#[derive(Debug, Clone, Copy)]
enum TimeoutProtocol {
    Http,
    Mcp,
}

impl TimeoutProtocol {
    fn for_request(request: &Request) -> Self {
        if request.uri().path() == "/v1/mcp" {
            Self::Mcp
        } else {
            Self::Http
        }
    }
}

async fn run_with_timeout(
    timeout: RequestTimeout,
    protocol: TimeoutProtocol,
    work: impl Future<Output = Response>,
) -> Response {
    match tokio::time::timeout(timeout.0, work).await {
        Ok(response) => response,
        Err(_) => match protocol {
            TimeoutProtocol::Http => error_json(StatusCode::REQUEST_TIMEOUT, "request timed out"),
            // This outer deadline cannot see the JSON-RPC id after axum's
            // body extractor consumes it. Treat it as a transport failure:
            // inventing id:null would be an invalid response to a request
            // with a real id (and notifications must receive no envelope).
            // MCP currently exposes only read-only Verbs, so this transport
            // 408 cannot hide a committed side effect.
            TimeoutProtocol::Mcp => StatusCode::REQUEST_TIMEOUT.into_response(),
        },
    }
}

pub(crate) async fn enforce(
    State(timeout): State<RequestTimeout>,
    request: Request,
    next: Next,
) -> Response {
    let protocol = TimeoutProtocol::for_request(&request);
    run_with_timeout(timeout, protocol, next.run(request)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deliberately_slow_http_work_is_cut_off_with_the_api_error_shape() {
        let timeout = RequestTimeout::new(Duration::from_millis(5));
        let started = std::time::Instant::now();
        let response = run_with_timeout(timeout, TimeoutProtocol::Http, async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            StatusCode::OK.into_response()
        })
        .await;

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert!(started.elapsed() < Duration::from_millis(100));
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"error": "request timed out"})
        );
    }

    #[tokio::test]
    async fn deliberately_slow_mcp_work_is_a_transport_timeout_without_an_invented_id() {
        let response = run_with_timeout(
            RequestTimeout::new(Duration::from_millis(5)),
            TimeoutProtocol::Mcp,
            async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                StatusCode::OK.into_response()
            },
        )
        .await;

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }
}
