//! Shared response DTOs for the authenticated service-status endpoint.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub version: String,
    pub repos_indexed: i64,
}
