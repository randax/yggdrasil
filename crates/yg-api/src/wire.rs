//! Canonical wire serialization: every JSON body this server emits is
//! compact and recursively key-sorted. Verb and MCP responses land in
//! model context windows where prompt caching is a byte-exact prefix
//! match, so identical data must always serialize to identical bytes —
//! and pretty-printing is pure token overhead.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// The canonical serialization of a body: compact separators, object
/// keys sorted recursively.
pub(crate) fn canonical_string(body: &impl Serialize) -> serde_json::Result<String> {
    let mut value = serde_json::to_value(body)?;
    sort_keys(&mut value);
    serde_json::to_string(&value)
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
