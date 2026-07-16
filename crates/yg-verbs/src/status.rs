//! Shared response DTOs for the authenticated service-status endpoint.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Verb;

/// Revision of the shipped Verb inventory and its public wire contract.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct VerbContractVersion(u32);

impl std::fmt::Display for VerbContractVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for VerbContractVersion {
    type Err = ParseVerbContractVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let version = value.parse::<u32>()?;
        if version == 0 {
            return Err(ParseVerbContractVersionError::Zero);
        }
        Ok(Self(version))
    }
}

/// Why a stamped Verb contract version could not be parsed.
#[derive(Debug, thiserror::Error)]
pub enum ParseVerbContractVersionError {
    #[error("Verb contract version must be a positive integer")]
    InvalidInteger(#[from] std::num::ParseIntError),
    #[error("Verb contract version must be greater than zero")]
    Zero,
}

/// Current revision of [`crate::VERB_TOOLS`] and their typed wire payloads.
/// Bump this whenever that inventory or any public Verb request/response wire
/// contract changes, then reinstall the shipped Skill to update its stamp.
pub const VERB_CONTRACT_VERSION: VerbContractVersion = VerbContractVersion(1);

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub version: String,
    pub repos_indexed: i64,
    pub verb_contract_version: VerbContractVersion,
    pub verbs: Vec<Verb>,
}
