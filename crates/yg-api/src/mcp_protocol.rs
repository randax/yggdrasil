//! Typed MCP lifecycle values and protocol-shape validation.

use serde::{Deserialize, Serialize};

/// A protocol version requested by an MCP client.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct ProtocolVersion(Box<str>);

impl ProtocolVersion {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// One protocol revision whose wire behavior this server implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SupportedProtocolVersion {
    V2024_11_05,
    V2025_03_26,
}

impl SupportedProtocolVersion {
    const fn as_str(self) -> &'static str {
        match self {
            Self::V2024_11_05 => "2024-11-05",
            Self::V2025_03_26 => "2025-03-26",
        }
    }
}

impl Serialize for SupportedProtocolVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Revisions this server can faithfully speak, oldest to newest.
pub(crate) const SUPPORTED_PROTOCOL_VERSIONS: &[SupportedProtocolVersion] = &[
    SupportedProtocolVersion::V2024_11_05,
    SupportedProtocolVersion::V2025_03_26,
];

const LATEST_PROTOCOL_VERSION: SupportedProtocolVersion = SupportedProtocolVersion::V2025_03_26;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeParams {
    protocol_version: ProtocolVersion,
    capabilities: ClientCapabilities,
    client_info: ClientInfo,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClientCapabilities {}

impl ClientCapabilities {
    fn validate(self) {}
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClientInfo {
    name: ClientName,
    version: ClientVersion,
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClientName(Box<str>);

impl ClientName {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
pub(crate) struct ClientVersion(Box<str>);

impl ClientVersion {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeResult {
    protocol_version: SupportedProtocolVersion,
    capabilities: ServerCapabilities,
    server_info: ServerInfo,
}

#[derive(Serialize)]
pub(crate) struct ServerCapabilities {
    tools: ToolsCapability,
}

#[derive(Serialize)]
pub(crate) struct ToolsCapability {}

#[derive(Serialize)]
pub(crate) struct ServerInfo {
    name: &'static str,
    version: &'static str,
}

/// MCP requires the requested version to be echoed when supported. For an
/// unsupported request, the server returns its latest supported version and
/// lets the client disconnect if that version is unacceptable.
///
/// See <https://modelcontextprotocol.io/specification/2025-03-26/basic/lifecycle#version-negotiation>.
pub(crate) fn initialize(params: InitializeParams) -> InitializeResult {
    params.capabilities.validate();
    let _client_name = params.client_info.name.as_str();
    let _client_version = params.client_info.version.as_str();
    let protocol_version = SUPPORTED_PROTOCOL_VERSIONS
        .iter()
        .copied()
        .find(|version| version.as_str() == params.protocol_version.as_str())
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    InitializeResult {
        protocol_version,
        capabilities: ServerCapabilities {
            tools: ToolsCapability {},
        },
        server_info: ServerInfo {
            name: "yggdrasil",
            version: env!("CARGO_PKG_VERSION"),
        },
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum McpProtocolError {
    #[error("MCP notification methods must not include an id")]
    NotificationHasId,
    #[error("invalid initialize parameters")]
    InvalidInitializeParams(#[source] serde_json::Error),
    #[error("serializing the initialize result failed")]
    SerializeInitializeResult(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialize_version(requested: &str) -> serde_json::Value {
        let params = serde_json::from_value::<InitializeParams>(serde_json::json!({
            "protocolVersion": requested,
            "capabilities": {},
            "clientInfo": {"name": "unit-test", "version": "1"},
        }))
        .expect("initialize parameters");
        serde_json::to_value(initialize(params)).expect("initialize result")
    }

    #[test]
    fn supported_protocol_version_is_echoed() {
        for version in ["2024-11-05", "2025-03-26"] {
            assert_eq!(initialize_version(version)["protocolVersion"], version);
        }
    }

    #[test]
    fn unsupported_protocol_version_gets_latest_supported_version() {
        assert_eq!(
            initialize_version("2099-01-01")["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            SUPPORTED_PROTOCOL_VERSIONS.last().copied(),
            Some(LATEST_PROTOCOL_VERSION),
            "the fallback must stay aligned with the ordered supported set"
        );
    }

    #[test]
    fn required_initialize_fields_are_typed_and_validated() {
        for invalid in [
            serde_json::json!({"protocolVersion": "2025-03-26"}),
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": false,
                "clientInfo": {"name": "unit-test", "version": "1"}
            }),
            serde_json::json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": false
            }),
        ] {
            assert!(serde_json::from_value::<InitializeParams>(invalid).is_err());
        }
    }
}
