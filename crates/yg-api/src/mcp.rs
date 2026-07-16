//! The MCP transport: JSON-RPC over `POST /v1/mcp`, exposing the Verb
//! catalog as tools. Verb tools call the shared engine directly and
//! encode its typed results and errors into MCP tool-result shapes.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::error_json;
use crate::wire::{self, Wire};

pub(crate) async fn mcp(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    input: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !mcp_origin_allowed(&headers) {
        return error_json(
            StatusCode::FORBIDDEN,
            "MCP Origin header must match the request Host",
        );
    }
    let input = match input {
        Ok(Json(input)) => input,
        Err(rejection) => return wire::jsonrpc_parse_error(&rejection),
    };
    match input {
        serde_json::Value::Array(messages) => {
            if messages.is_empty() {
                return Wire(jsonrpc_error(
                    serde_json::Value::Null,
                    -32600,
                    "JSON-RPC batch must not be empty",
                ))
                .into_response();
            }
            if messages.len() > state.mcp_batch_size_limit {
                return (
                    StatusCode::BAD_REQUEST,
                    Wire(jsonrpc_error(
                        serde_json::Value::Null,
                        -32000,
                        format!(
                            "JSON-RPC batch exceeds maximum of {} messages",
                            state.mcp_batch_size_limit
                        ),
                    )),
                )
                    .into_response();
            }
            let mut responses = Vec::new();
            for message in messages {
                if let Some(response) = handle_mcp_message(state.clone(), message).await {
                    responses.push(response);
                }
            }
            if responses.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                Wire(serde_json::Value::Array(responses)).into_response()
            }
        }
        message => match handle_mcp_message(state, message).await {
            Some(response) => Wire(response).into_response(),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn mcp_origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some(host) = headers.get(header::HOST) else {
        return false;
    };
    let Ok(host) = host.to_str() else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .and_then(|rest| rest.split('/').next())
    else {
        return false;
    };
    authority.eq_ignore_ascii_case(host)
}

async fn handle_mcp_message(
    state: Arc<AppState>,
    message: serde_json::Value,
) -> Option<serde_json::Value> {
    let Some(object) = message.as_object() else {
        return Some(jsonrpc_error(
            serde_json::Value::Null,
            -32600,
            "JSON-RPC message must be an object",
        ));
    };
    let Some(method) = object.get("method").and_then(serde_json::Value::as_str) else {
        return Some(jsonrpc_error(
            object.get("id").cloned().unwrap_or(serde_json::Value::Null),
            -32600,
            "JSON-RPC request method must be a string",
        ));
    };
    let id = object.get("id").cloned()?;
    let params = object
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let response = match method {
        "initialize" => jsonrpc_success(
            id,
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {
                    "name": "yggdrasil",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        ),
        "notifications/initialized" => jsonrpc_success(id, serde_json::json!({})),
        "tools/list" => match mcp_tools() {
            Ok(tools) => jsonrpc_success(id, serde_json::json!({"tools": tools})),
            Err((code, message)) => jsonrpc_error(id, code, message),
        },
        "tools/call" => match mcp_call_tool(state, params).await {
            Ok(result) => jsonrpc_success(id, result),
            Err((code, message)) => jsonrpc_error(id, code, message),
        },
        other => jsonrpc_error(id, -32601, format!("unknown MCP method {other:?}")),
    };
    Some(response)
}

fn jsonrpc_success(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn jsonrpc_error(
    id: serde_json::Value,
    code: i64,
    message: impl Into<String>,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()}
    })
}

fn mcp_tools() -> Result<Vec<serde_json::Value>, (i64, String)> {
    Ok(yg_verbs::VERB_TOOLS
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema()
            })
        })
        .collect())
}

async fn mcp_call_tool(
    state: Arc<AppState>,
    params: serde_json::Value,
) -> Result<serde_json::Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                -32602,
                "tools/call params.name must be a string".to_string(),
            )
        })?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let result = call_verb_tool(state, name, arguments).await?;
    Ok(match result {
        Ok(body) => mcp_tool_result(body, false),
        Err(error) => mcp_tool_result(serde_json::json!({"error": error}), true),
    })
}

fn mcp_tool_result(structured: serde_json::Value, is_error: bool) -> serde_json::Value {
    let text = wire::canonical_string(&structured).expect("tool content serializes");
    serde_json::json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured,
        "isError": is_error
    })
}

async fn call_verb_tool(
    state: Arc<AppState>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<Result<serde_json::Value, McpVerbError>, (i64, String)> {
    let tool =
        yg_verbs::verb_tool(name).ok_or_else(|| (-32602, format!("unknown Verb tool {name:?}")))?;
    match tool.verb {
        yg_verbs::Verb::Node => {
            let req = decode_tool_args(arguments)?;
            encode_verb_result(state.engine.node(req).await)
        }
        yg_verbs::Verb::Neighbors => {
            let req = decode_tool_args(arguments)?;
            encode_verb_result(state.engine.neighbors(req).await)
        }
        yg_verbs::Verb::Search => {
            let req = decode_tool_args(arguments)?;
            let response = state
                .search(req)
                .await
                .map(yg_verbs::SearchWireResponse::from);
            encode_verb_result(response)
        }
        yg_verbs::Verb::History => {
            let req = decode_tool_args(arguments)?;
            encode_verb_result(state.engine.history(req).await)
        }
    }
}

fn decode_tool_args<T>(value: serde_json::Value) -> Result<T, (i64, String)>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(value).map_err(|e| (-32602, format!("invalid tool arguments: {e}")))
}

fn encode_verb_result<T>(
    result: Result<T, yg_verbs::VerbError>,
) -> Result<Result<serde_json::Value, McpVerbError>, (i64, String)>
where
    T: Serialize,
{
    match result {
        Ok(response) => serde_json::to_value(response).map(Ok).map_err(|error| {
            tracing::error!("serializing MCP Verb result failed: {error}");
            (-32603, "internal server error".to_string())
        }),
        Err(error) => Ok(Err(error.into())),
    }
}

#[derive(Serialize)]
struct McpVerbError {
    kind: McpVerbErrorKind,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<yg_verbs::NoSuchSymbol>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum McpVerbErrorKind {
    BadRequest,
    NotFound,
    Gone,
    Unavailable,
    Internal,
}

impl From<yg_verbs::VerbError> for McpVerbError {
    fn from(error: yg_verbs::VerbError) -> Self {
        let (kind, message, detail) = match error {
            yg_verbs::VerbError::BadRequest(message) => {
                (McpVerbErrorKind::BadRequest, message, None)
            }
            yg_verbs::VerbError::NotFound(message) => (McpVerbErrorKind::NotFound, message, None),
            yg_verbs::VerbError::NoSuchSymbol(detail) => (
                McpVerbErrorKind::NotFound,
                "no such symbol".to_string(),
                Some(detail),
            ),
            yg_verbs::VerbError::Gone(message) => (McpVerbErrorKind::Gone, message, None),
            yg_verbs::VerbError::Unavailable(message) => {
                (McpVerbErrorKind::Unavailable, message, None)
            }
            yg_verbs::VerbError::Internal(source) => {
                tracing::error!("internal error: {source:#}");
                (
                    McpVerbErrorKind::Internal,
                    "internal server error".to_string(),
                    None,
                )
            }
        };
        Self {
            kind,
            message,
            detail,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_origin_must_match_host_when_present() {
        use axum::http::{HeaderValue, header};

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:7311"));
        assert!(
            super::mcp_origin_allowed(&headers),
            "non-browser clients do not send Origin"
        );

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:7311"),
        );
        assert!(super::mcp_origin_allowed(&headers));

        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        assert!(
            !super::mcp_origin_allowed(&headers),
            "cross-origin browser access is rejected"
        );
    }
}
