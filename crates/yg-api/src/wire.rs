//! Canonical wire serialization: every JSON body this server emits is
//! compact and recursively key-sorted. Verb and MCP responses land in
//! model context windows where prompt caching is a byte-exact prefix
//! match, so identical data must always serialize to identical bytes —
//! and pretty-printing is pure token overhead.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// The canonical serialization of a body: compact separators, object
/// keys sorted recursively.
pub(crate) fn canonical_string(body: &impl Serialize) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(body)?;
    sort_keys(&mut value);
    serde_json::to_string(&value)
}

/// Serializes an `f32` as the `f64` its shortest decimal form denotes.
/// [`canonical_string`]'s trip through `serde_json::to_value` widens
/// floats with a bare cast, which would put "5.480152130126953" on the
/// wire where the f32 wrote "5.480152".
pub(crate) fn f32_shortest<S: serde::Serializer>(
    value: &f32,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let shortest: f64 = value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(*value));
    serializer.serialize_f64(shortest)
}

fn sort_keys(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.sort_keys();
            map.values_mut().for_each(sort_keys);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sort_keys),
        _ => {}
    }
}

/// Drop-in replacement for axum's `Json` responder that emits the
/// canonical form. Every response body leaves the server through this
/// type; `Json` remains for request extraction only.
pub(crate) struct Wire<T>(pub T);

impl<T: Serialize> IntoResponse for Wire<T> {
    fn into_response(self) -> Response {
        match canonical_string(&self.0) {
            Ok(body) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
            // Even this (unreachable-in-practice) failure keeps the
            // server's one error shape: {"error": …}.
            Err(e) => {
                tracing::error!("canonical serialization failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"error":"internal server error"}"#,
                )
                    .into_response()
            }
        }
    }
}

/// `Json` request extractor whose rejection keeps the server's one
/// error shape — axum's default rejection answers in text/plain.
pub(crate) struct WireJson<T>(pub T);

impl<S, T> FromRequest<S> for WireJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(crate::error::error_json(
                rejection.status(),
                rejection.body_text(),
            )),
        }
    }
}

/// The canonical shape for requests the router never matched to a
/// handler method.
pub(crate) async fn method_not_allowed() -> Response {
    crate::error::error_json(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

/// A request `/v1/mcp` could not read a JSON body from, as a JSON-RPC
/// error with id null. Only a body that failed to parse is the spec's
/// -32700; transport rejections (content-type, size) keep their own
/// HTTP status and answer "invalid request".
pub(crate) fn jsonrpc_parse_error(rejection: &JsonRejection) -> Response {
    // JsonDataError is syntactically valid JSON that failed
    // deserialization — an invalid request, not a parse error. It
    // cannot occur here today (the extractor targets Value), but the
    // mapping must not misclassify if a typed extractor ever lands.
    let code = match rejection {
        JsonRejection::JsonSyntaxError(_) => -32700,
        _ => -32600,
    };
    (
        rejection.status(),
        Wire(crate::mcp::jsonrpc_error(
            serde_json::Value::Null,
            code,
            rejection.body_text(),
        )),
    )
        .into_response()
}
