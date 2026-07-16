//! Shared request and response DTOs for the administrative HTTP surface.

use serde::{Deserialize, Serialize};

/// A repository slug validated by the control-plane boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoSlug(String);

impl RepoSlug {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepoSlug {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A repository qualifier that is already owned by another registration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoQualifier(String);

impl RepoQualifier {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RepoQualifier {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A forge organization name validated by the control-plane boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrgName(String);

impl OrgName {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OrgName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A non-empty forge base URL supplied by the control-plane boundary.
///
/// This type intentionally does not parse or fully validate URLs; issue #79 owns
/// typed URL validation for control-plane boundary structs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForgeBaseUrl(String);

impl ForgeBaseUrl {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Convert the validated control-plane value to its unchanged wire form.
    pub fn from_domain(value: yg_control::ForgeUrl) -> Self {
        Self(value.into_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ForgeBaseUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A stable member-token identifier validated by the control-plane boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenId(String);

impl TokenId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TokenId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A member name validated by the control-plane boundary.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemberName(String);

impl MemberName {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// Positive token lifetime requested on the administrative wire surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenLifetimeSeconds(u64);

impl TokenLifetimeSeconds {
    pub fn new(seconds: u64) -> Self {
        Self(seconds)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for MemberName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A repository URL supplied for server-side validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoRegistrationUrl(String);

impl RepoRegistrationUrl {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The environment-variable name containing a forge credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ForgeTokenEnvironment(String);

impl ForgeTokenEnvironment {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A forge adapter kind supplied for registry lookup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestedForgeKind(String);

impl RequestedForgeKind {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A repository-slug glob supplied for discovery-rule validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiscoveryRulePattern(String);

impl DiscoveryRulePattern {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    Github,
    Git,
}

impl ForgeKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "github" => Some(Self::Github),
            "git" => Some(Self::Git),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Git => "git",
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
pub struct AddRepoRequest {
    pub url: RepoRegistrationUrl,
    /// Shallow-clone override; omitted = full history.
    pub depth: Option<i32>,
    /// Per-repo poll interval in seconds; omitted = the server default.
    pub poll_interval: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRepoResponse {
    pub slug: RepoSlug,
    pub created: bool,
    pub fetch_queued: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddForgeRequest {
    pub kind: RequestedForgeKind,
    pub org: OrgName,
    pub base_url: Option<ForgeBaseUrl>,
    pub token_env: Option<ForgeTokenEnvironment>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddForgeResponse {
    pub kind: ForgeKind,
    pub org: OrgName,
    pub base_url: ForgeBaseUrl,
    pub created: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverForgeRequest {
    pub kind: RequestedForgeKind,
    pub org: OrgName,
    pub base_url: Option<ForgeBaseUrl>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoverForgeResponse {
    pub kind: ForgeKind,
    pub org: OrgName,
    pub base_url: ForgeBaseUrl,
    pub queued: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRuleRequest {
    pub forge: Option<ForgeBaseUrl>,
    pub pattern: DiscoveryRulePattern,
    pub action: RuleAction,
    #[serde(rename = "private")]
    pub applies_to_private: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRuleResponse {
    pub forge: ForgeBaseUrl,
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
    pub forge: ForgeBaseUrl,
    pub pattern: String,
    pub action: RuleAction,
    pub applies_to_private: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueTokenRequest {
    pub member: MemberName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<TokenLifetimeSeconds>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueTokenResponse {
    pub id: TokenId,
    pub member: MemberName,
    pub token: String,
    pub expires_at: Option<TokenTimestampSeconds>,
}

/// Whole Unix seconds on the member-token administrative wire surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TokenTimestampSeconds(i64);

impl TokenTimestampSeconds {
    pub fn new(seconds: i64) -> Self {
        Self(seconds)
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberTokenStatus {
    Active,
    Expired,
    Revoked,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberTokenResponse {
    pub id: TokenId,
    pub member: MemberName,
    pub created_at: TokenTimestampSeconds,
    pub last_used_at: Option<TokenTimestampSeconds>,
    pub expires_at: Option<TokenTimestampSeconds>,
    pub revoked_at: Option<TokenTimestampSeconds>,
    pub status: MemberTokenStatus,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberTokensResponse {
    pub tokens: Vec<MemberTokenResponse>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeTokenResponse {
    pub id: TokenId,
    pub revoked: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminStatusResponse {
    pub repos: Vec<AdminRepoStatus>,
    pub visibility_counts: VisibilityCounts,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub discovery_conflicts: Vec<DiscoveryQualifierConflictStatus>,
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
    pub slug: RepoSlug,
    pub forge: ForgeBaseUrl,
    pub visibility: RepoVisibility,
    pub discovery_state: DiscoveryState,
    pub last_synced_commit: Option<String>,
    pub sync: JobStatus,
    pub index: JobStatus,
    pub shard: Option<ShardStatus>,
}

/// A Forge-listed repository rejected because its qualifier is already owned.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryQualifierConflictStatus {
    pub forge: ForgeBaseUrl,
    pub org: OrgName,
    pub slug: RepoSlug,
    pub qualifier: RepoQualifier,
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

#[cfg(test)]
mod member_token_tests {
    use super::MemberTokenStatus;

    #[test]
    fn member_token_status_uses_operator_facing_lowercase_values() {
        assert_eq!(
            serde_json::to_string(&MemberTokenStatus::Expired).unwrap(),
            "\"expired\""
        );
    }
}
