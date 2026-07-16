//! Bounded HTTP client construction shared by every CLI request path.

use std::sync::OnceLock;
use std::time::Duration;

/// Finite budgets for establishing a connection and completing a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HttpTimeouts {
    request: Duration,
    connect: Duration,
}

const DEFAULT_TIMEOUTS: HttpTimeouts = HttpTimeouts {
    request: Duration::from_secs(30),
    connect: Duration::from_secs(5),
};

/// The process-wide client. Clones retain the same connection pool, so the MCP
/// proxy and ordinary commands share both transport behavior and resources.
pub(crate) fn shared() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            build(DEFAULT_TIMEOUTS)
                .expect("the CLI's static HTTP timeout configuration must be valid")
        })
        .clone()
}

fn build(timeouts: HttpTimeouts) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeouts.request)
        .connect_timeout(timeouts.connect)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_that_accepts_but_never_answers_hits_the_request_deadline() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("binding wedged-server fixture: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let wedged_server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _socket = socket;
            std::future::pending::<()>().await;
        });
        let client = build(HttpTimeouts {
            request: Duration::from_millis(100),
            connect: Duration::from_millis(50),
        })
        .unwrap();

        let started = std::time::Instant::now();
        let error = client
            .get(format!("http://{address}/wedged"))
            .send()
            .await
            .expect_err("a response-less server must time out");

        assert!(error.is_timeout(), "unexpected client error: {error}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the configured 100ms timeout must bound the request"
        );
        wedged_server.abort();
    }

    #[test]
    fn production_deadlines_are_finite_and_connection_is_the_smaller_budget() {
        assert!(!DEFAULT_TIMEOUTS.request.is_zero());
        assert!(!DEFAULT_TIMEOUTS.connect.is_zero());
        assert!(DEFAULT_TIMEOUTS.connect <= DEFAULT_TIMEOUTS.request);
    }
}
