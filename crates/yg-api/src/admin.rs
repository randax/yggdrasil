//! The Admin-scoped control surface: registering repos and forges,
//! discovery, rules, member tokens, and pipeline status.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use yg_sync::RepoLocator;
use yg_sync::forge::Forge;

use crate::AppState;
use crate::error::ApiError;
use crate::wire::{Wire, WireJson};

#[derive(Deserialize)]
pub(crate) struct AddRepoRequest {
    url: String,
    /// Shallow-clone override; omitted = full history.
    depth: Option<i32>,
    /// Per-repo poll interval in seconds; omitted = the server default.
    poll_interval: Option<i32>,
}

#[derive(Serialize)]
struct AddRepoResponse {
    slug: String,
    created: bool,
    /// False when a fetch was already pending — nothing new was queued.
    fetch_queued: bool,
}

#[derive(Deserialize)]
pub(crate) struct AddForgeRequest {
    kind: String,
    org: String,
    base_url: Option<String>,
    token_env: Option<String>,
}

#[derive(Serialize)]
struct AddForgeResponse {
    kind: String,
    org: String,
    base_url: String,
    created: bool,
}

pub(crate) async fn admin_forge_add(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<AddForgeRequest>,
) -> Result<Response, ApiError> {
    let forge = discovery_capable_forge(&req.kind).map_err(ApiError::bad_request)?;
    let org = github_org_slug(&req.org).map_err(ApiError::bad_request)?;
    let base_url = github_base_url(req.base_url.as_deref()).map_err(ApiError::bad_request)?;
    let token_env = req
        .token_env
        .as_deref()
        .or_else(|| forge.default_token_env());
    let outcome = state
        .control
        .connect_forge_org(yg_control::ConnectForgeOrg {
            forge_kind: forge.kind(),
            base_url: &base_url,
            org_slug: &org,
            token_env,
            api_root: forge.default_api_root(&base_url).as_deref(),
        })
        .await?;
    Ok((
        if outcome.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Wire(AddForgeResponse {
            kind: forge.kind().to_string(),
            org,
            base_url,
            created: outcome.created,
        }),
    )
        .into_response())
}

/// Resolve a requested forge kind to a registered adapter that can
/// discover org repositories.
fn discovery_capable_forge(kind: &str) -> Result<&'static dyn Forge, String> {
    yg_sync::forge::builtin()
        .by_kind(kind.trim().to_ascii_lowercase().as_str())
        .filter(|forge| forge.discovery().is_some())
        .ok_or_else(|| format!("forge kind {kind:?} has no discovery adapter in this release"))
}

fn github_org_slug(org: &str) -> Result<String, &'static str> {
    let org = org.trim();
    let valid = !org.is_empty()
        && org.len() <= 39
        && !org.starts_with('-')
        && !org.ends_with('-')
        && !org.contains("--")
        && org.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if valid {
        Ok(org.to_string())
    } else {
        Err("org must be a GitHub organization slug (letters, numbers, single hyphens)")
    }
}

fn github_base_url(base_url: Option<&str>) -> Result<String, &'static str> {
    let base_url = base_url
        .unwrap_or("https://github.com")
        .trim()
        .trim_end_matches('/');
    let base_url = base_url.to_ascii_lowercase();
    let Some(rest) = base_url.strip_prefix("https://") else {
        return Err("github forge base_url must start with https://");
    };
    if rest.is_empty()
        || rest.contains('/')
        || rest.contains('@')
        || rest.contains('?')
        || rest.contains('#')
        || rest.bytes().any(|b| b.is_ascii_whitespace())
    {
        return Err("github forge base_url must be a clone root like https://github.com");
    }
    Ok(base_url)
}

#[derive(Deserialize)]
pub(crate) struct DiscoverForgeRequest {
    kind: String,
    org: String,
    base_url: Option<String>,
}

#[derive(Serialize)]
struct DiscoverForgeResponse {
    kind: String,
    org: String,
    base_url: String,
    queued: bool,
}

pub(crate) async fn admin_forge_discover(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<DiscoverForgeRequest>,
) -> Result<Response, ApiError> {
    let forge = discovery_capable_forge(&req.kind).map_err(ApiError::bad_request)?;
    let kind = forge.kind();
    let org = github_org_slug(&req.org).map_err(ApiError::bad_request)?;
    let base_url = github_base_url(req.base_url.as_deref()).map_err(ApiError::bad_request)?;
    if !state.control.request_discovery(&base_url, &org).await? {
        return Err(ApiError::not_found(format!(
            "{kind} org {org} is not connected; run yg admin forge add first"
        )));
    }
    Ok(Wire(DiscoverForgeResponse {
        kind: kind.to_string(),
        org,
        base_url,
        queued: true,
    })
    .into_response())
}

#[derive(Deserialize)]
pub(crate) struct AddRuleRequest {
    forge: Option<String>,
    pattern: String,
    action: String,
    private: Option<bool>,
}

#[derive(Serialize)]
struct AddRuleResponse {
    forge: String,
    pattern: String,
    action: String,
    applies_to_private: bool,
    created: bool,
    repos_reconsidered: u64,
    fetches_queued: u64,
}

pub(crate) async fn admin_rules_add(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<AddRuleRequest>,
) -> Result<Response, ApiError> {
    let action = match req.action.as_str() {
        "include" => yg_control::RuleAction::Include,
        "exclude" => yg_control::RuleAction::Exclude,
        other => {
            return Err(ApiError::bad_request(format!(
                "rule action must be include or exclude, got {other:?}"
            )));
        }
    };
    let pattern = req.pattern.trim();
    if pattern.is_empty() {
        return Err(ApiError::bad_request("rule pattern must not be empty"));
    }
    let forge = github_base_url(req.forge.as_deref()).map_err(ApiError::bad_request)?;
    let forge_id = state
        .control
        .forge_id_by_base_url(&forge)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "forge {forge} is not connected; run yg admin forge add first"
            ))
        })?;
    let outcome = state
        .control
        .add_rule(yg_control::AddRule {
            forge_id,
            pattern,
            action,
            applies_to_private: req.private.unwrap_or(false),
        })
        .await?;
    Ok((
        if outcome.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Wire(AddRuleResponse {
            forge,
            pattern: pattern.to_string(),
            action: req.action,
            applies_to_private: req.private.unwrap_or(false),
            created: outcome.created,
            repos_reconsidered: outcome.repos_reconsidered,
            fetches_queued: outcome.fetches_queued,
        }),
    )
        .into_response())
}

#[derive(Serialize)]
struct RulesResponse {
    rules: Vec<RuleResponse>,
}

#[derive(Serialize)]
struct RuleResponse {
    forge: String,
    pattern: String,
    action: String,
    applies_to_private: bool,
}

pub(crate) async fn admin_rules_list(
    State(state): State<Arc<AppState>>,
) -> Result<Response, ApiError> {
    let rules = state.control.rules().await?;
    Ok(Wire(RulesResponse {
        rules: rules
            .into_iter()
            .map(|rule| RuleResponse {
                forge: rule.forge,
                pattern: rule.pattern,
                action: rule.action,
                applies_to_private: rule.applies_to_private,
            })
            .collect(),
    })
    .into_response())
}

pub(crate) async fn admin_repo_add(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<AddRepoRequest>,
) -> Result<Response, ApiError> {
    if let Some(depth) = req.depth
        && depth < 1
    {
        return Err(ApiError::bad_request(format!(
            "depth must be a positive number of commits (got {depth})"
        )));
    }
    if let Some(interval) = req.poll_interval
        && interval < 1
    {
        return Err(ApiError::bad_request(format!(
            "poll_interval must be a positive number of seconds (got {interval})"
        )));
    }
    // The typed parse error renders to its human-facing form here, at
    // the I/O edge.
    let locator = RepoLocator::parse(&req.url).map_err(|e| ApiError::bad_request(e.to_string()))?;
    let forge = yg_sync::forge::builtin()
        .by_kind(locator.kind)
        .ok_or_else(|| ApiError::internal(anyhow::anyhow!("locator kind without an adapter")))?;
    // Every node id this repo will ever mint embeds its qualifier
    // (RFC 0001 §5); a qualifier the id grammar can't address — an
    // IPv6 host, a path with a stray colon — would index a repo no
    // query could reach. Refused here, with the reason, instead.
    let qualifier = yg_control::repo_qualifier(&locator.base_url, &locator.slug);
    if !yg_verbs::addressable_qualifier(&qualifier) {
        return Err(ApiError::bad_request(format!(
            "{} maps to repo qualifier {qualifier:?}, which node ids cannot address \
             (it contains a colon outside a numeric port); \
             use a hostname-based URL without colons in its path",
            req.url
        )));
    }
    let outcome = state
        .control
        .add_repo(yg_control::AddRepo {
            forge_kind: locator.kind,
            base_url: &locator.base_url,
            token_env: forge.default_token_env(),
            api_root: forge.default_api_root(&locator.base_url).as_deref(),
            slug: &locator.slug,
            fetch_depth: req.depth,
            poll_interval_seconds: req.poll_interval,
        })
        .await
        .map_err(|e| {
            // The same host/slug registered through a different forge URL
            // (http vs https, say) is the caller's collision to resolve.
            if e.downcast_ref::<yg_control::QualifierConflict>().is_some() {
                ApiError::conflict(format!("{e}"))
            } else {
                ApiError::internal(e)
            }
        })?;
    Ok((
        if outcome.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Wire(AddRepoResponse {
            slug: locator.slug,
            created: outcome.created,
            fetch_queued: outcome.fetch_queued,
        }),
    )
        .into_response())
}

#[derive(Deserialize)]
pub(crate) struct IssueTokenRequest {
    member: String,
}

#[derive(Serialize)]
struct IssueTokenResponse {
    id: String,
    member: String,
    token: String,
}

pub(crate) async fn admin_token_issue(
    State(state): State<Arc<AppState>>,
    WireJson(req): WireJson<IssueTokenRequest>,
) -> Result<Response, ApiError> {
    let member = req.member.trim();
    if member.is_empty() {
        return Err(ApiError::bad_request("member must not be empty"));
    }
    let issued = state.control.issue_member_token(member).await?;
    Ok((
        StatusCode::CREATED,
        Wire(IssueTokenResponse {
            id: issued.id,
            member: issued.member,
            token: issued.token,
        }),
    )
        .into_response())
}

#[derive(Serialize)]
struct RevokeTokenResponse {
    id: String,
    revoked: bool,
}

pub(crate) async fn admin_token_revoke(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !yg_control::member_token_id_is_valid(&id) {
        return Err(ApiError::bad_request(
            "member token id must look like mtok_<24 hex characters>",
        ));
    }
    if !state.control.revoke_member_token(&id).await? {
        return Err(ApiError::not_found(format!(
            "no active member token {id:?}"
        )));
    }
    Ok(Wire(RevokeTokenResponse { id, revoked: true }).into_response())
}

#[derive(Serialize)]
struct AdminStatusResponse {
    repos: Vec<AdminRepoStatus>,
    visibility_counts: VisibilityCounts,
}

#[derive(Default, Serialize)]
struct VisibilityCounts {
    public: usize,
    internal: usize,
    private: usize,
}

impl VisibilityCounts {
    fn record(&mut self, visibility: yg_control::RepoVisibility) {
        match visibility {
            yg_control::RepoVisibility::Public => self.public += 1,
            yg_control::RepoVisibility::Internal => self.internal += 1,
            yg_control::RepoVisibility::Private => self.private += 1,
        }
    }
}

#[derive(Serialize)]
struct AdminRepoStatus {
    slug: String,
    forge: String,
    visibility: &'static str,
    discovery_state: String,
    last_synced_commit: Option<String>,
    sync: JobStatus,
    index: JobStatus,
    /// The repo's current Shard; null until first indexed.
    shard: Option<ShardStatus>,
}

/// One pipeline stage's position, as admin status reports it for both
/// sync and index.
#[derive(Serialize)]
struct JobStatus {
    state: &'static str,
    attempts: i32,
    last_error: Option<String>,
}

#[derive(Serialize)]
struct ShardStatus {
    revision: String,
    nodes: i64,
    edges: i64,
}

pub(crate) async fn admin_status(State(state): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let repos = state.control.admin_status().await?;
    let mut visibility_counts = VisibilityCounts::default();
    for repo in &repos {
        visibility_counts.record(repo.visibility);
    }
    let repos = repos
        .into_iter()
        .map(|r| AdminRepoStatus {
            sync: JobStatus {
                state: job_state(
                    r.job_state.as_deref(),
                    r.attempts,
                    r.last_synced_commit.is_some(),
                    StageWords {
                        active: "syncing",
                        done: "synced",
                        never_ran: "registered",
                    },
                ),
                attempts: r.attempts,
                last_error: r.last_error,
            },
            index: JobStatus {
                state: job_state(
                    r.index_job_state.as_deref(),
                    r.index_attempts,
                    r.shard_revision.is_some(),
                    StageWords {
                        active: "indexing",
                        done: "indexed",
                        never_ran: "pending",
                    },
                ),
                attempts: r.index_attempts,
                last_error: r.index_last_error,
            },
            shard: r.shard_revision.map(|revision| ShardStatus {
                revision,
                // Set together with the revision when a Shard is recorded.
                nodes: r.shard_node_count.unwrap_or(0),
                edges: r.shard_edge_count.unwrap_or(0),
            }),
            slug: r.slug,
            forge: r.forge,
            visibility: match r.visibility {
                yg_control::RepoVisibility::Public => "public",
                yg_control::RepoVisibility::Internal => "internal",
                yg_control::RepoVisibility::Private => "private",
            },
            discovery_state: r.discovery_state,
            last_synced_commit: r.last_synced_commit,
        })
        .collect();
    Ok(Wire(AdminStatusResponse {
        repos,
        visibility_counts,
    })
    .into_response())
}

/// The stage-specific words [`job_state`] fills in: what to call a
/// leased job, a stage that finished, and one that never ran.
struct StageWords {
    active: &'static str,
    done: &'static str,
    never_ran: &'static str,
}

/// Collapse a pipeline stage's queue position into the one word
/// `yg admin status` shows for it. `attempts` only ever rises above zero
/// through failures (`fail_*` re-queues with a backoff), so a queued job
/// with attempts is a retry, not a first run.
fn job_state(
    job_state: Option<&str>,
    attempts: i32,
    has_output: bool,
    words: StageWords,
) -> &'static str {
    match (job_state, attempts, has_output) {
        (Some("leased"), ..) => words.active,
        (Some("queued"), 0, _) => "queued",
        (Some("queued"), ..) => "retrying",
        (Some(_), ..) => "unknown",
        (None, _, true) => words.done,
        (None, _, false) => words.never_ran,
    }
}
