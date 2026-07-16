//! The one error shape this server speaks: `{"error": "…"}` with a
//! client status, and never any error-chain content on a 500.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::wire::Wire;

/// The one shape every error leaves this server in: `{"error": "…"}`.
pub(crate) fn error_json(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Wire(serde_json::json!({"error": message.into()}))).into_response()
}

/// The one error type handlers leave the API through: a client status
/// plus a client-safe message. Internal faults log the full chain
/// server-side and cross the wire as a generic 500 — error-chain content
/// (database errors, filesystem paths, store addresses) never reaches a
/// response body.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub(crate) status: StatusCode,
    body: ErrorBody,
}

#[derive(Debug)]
enum ErrorBody {
    Message(String),
    NoSuchSymbol(yg_verbs::NoSuchSymbol),
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorBody::Message(message.into()),
        }
    }

    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    pub(crate) fn gone(message: impl Into<String>) -> Self {
        Self::new(StatusCode::GONE, message)
    }

    pub(crate) fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    /// An internal fault: the full chain goes to the server log, the
    /// client gets a generic body.
    pub(crate) fn internal(source: anyhow::Error) -> Self {
        tracing::error!("internal error: {source:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
    }
}

/// `?` on an `anyhow::Result` in a handler is an internal fault: logged
/// in full, sanitized on the wire.
impl From<anyhow::Error> for ApiError {
    fn from(source: anyhow::Error) -> Self {
        Self::internal(source)
    }
}

/// The engine's sanitized error, encoded: each category keeps its HTTP
/// status, and `Internal` takes the same log-and-sanitize path as every
/// other server fault.
impl From<yg_verbs::VerbError> for ApiError {
    fn from(e: yg_verbs::VerbError) -> Self {
        match e {
            yg_verbs::VerbError::BadRequest(message) => Self::bad_request(message),
            yg_verbs::VerbError::NotFound(message) => Self::not_found(message),
            yg_verbs::VerbError::NoSuchSymbol(payload) => Self {
                status: StatusCode::NOT_FOUND,
                body: ErrorBody::NoSuchSymbol(payload),
            },
            yg_verbs::VerbError::Gone(message) => Self::gone(message),
            yg_verbs::VerbError::Unavailable(message) => Self::unavailable(message),
            yg_verbs::VerbError::Internal(source) => Self::internal(source),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self.body {
            ErrorBody::Message(message) => error_json(self.status, message),
            ErrorBody::NoSuchSymbol(payload) => {
                (self.status, Wire(serde_json::json!({"error": payload}))).into_response()
            }
        }
    }
}
