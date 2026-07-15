//! Shared response DTOs for the administrative HTTP surface.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Github,
}

impl ForgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
        }
    }
}

impl std::fmt::Display for ForgeKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Include,
    Exclude,
}

impl RuleAction {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "include" => Some(Self::Include),
            "exclude" => Some(Self::Exclude),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Exclude => "exclude",
        }
    }
}

impl std::fmt::Display for RuleAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoVisibility {
    Public,
    Internal,
    Private,
}

impl RepoVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Private => "private",
        }
    }
}

impl std::fmt::Display for RepoVisibility {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryState {
    Discovered,
    Included,
    Excluded,
}

impl DiscoveryState {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "discovered" => Some(Self::Discovered),
            "included" => Some(Self::Included),
            "excluded" => Some(Self::Excluded),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Included => "included",
            Self::Excluded => "excluded",
        }
    }
}

impl std::fmt::Display for DiscoveryState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Syncing,
    Synced,
    Registered,
    Queued,
    Retrying,
    Indexing,
    Indexed,
    Pending,
    Unknown,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Syncing => "syncing",
            Self::Synced => "synced",
            Self::Registered => "registered",
            Self::Queued => "queued",
            Self::Retrying => "retrying",
            Self::Indexing => "indexing",
            Self::Indexed => "indexed",
            Self::Pending => "pending",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRepoResponse {
    pub slug: String,
    pub created: bool,
    pub fetch_queued: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddForgeResponse {
    pub kind: ForgeKind,
    pub org: String,
    pub base_url: String,
    pub created: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverForgeResponse {
    pub kind: ForgeKind,
    pub org: String,
    pub base_url: String,
    pub queued: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRuleResponse {
    pub forge: String,
    pub pattern: String,
    pub action: RuleAction,
    pub applies_to_private: bool,
    pub created: bool,
    pub repos_reconsidered: u64,
    pub fetches_queued: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesResponse {
    pub rules: Vec<RuleResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleResponse {
    pub forge: String,
    pub pattern: String,
    pub action: RuleAction,
    pub applies_to_private: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueTokenResponse {
    pub id: String,
    pub member: String,
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeTokenResponse {
    pub id: String,
    pub revoked: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminStatusResponse {
    pub repos: Vec<AdminRepoStatus>,
    pub visibility_counts: VisibilityCounts,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibilityCounts {
    pub public: u64,
    pub internal: u64,
    pub private: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRepoStatus {
    pub slug: String,
    pub forge: String,
    pub visibility: RepoVisibility,
    pub discovery_state: DiscoveryState,
    pub last_synced_commit: Option<String>,
    pub sync: JobStatus,
    pub index: JobStatus,
    pub shard: Option<ShardStatus>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStatus {
    pub state: JobState,
    pub attempts: i32,
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShardStatus {
    pub revision: String,
    pub nodes: i64,
    pub edges: i64,
}
