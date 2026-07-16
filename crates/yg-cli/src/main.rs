//! yg binary: subcommands, serve roles, MCP proxy.

mod client_config;
mod deploy_config;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

const NODE_ADDRESS_HELP: &str = "Exact node id, or a bare symbol name with --repo. Bare-name matching is byte-exact and case-sensitive; names without a safely searchable term set are un-addressable.";

#[derive(Parser)]
#[command(
    name = "yg",
    version,
    about = "yggdrasil — Knowledge Graph Index Server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot the Index Server
    Serve {
        /// Which roles this process runs
        #[arg(long, value_enum, default_value_t = Role::All)]
        role: Role,
    },
    /// Report the resolved YG_* configuration and its validation errors
    /// without starting the server
    ConfigCheck {
        /// Which roles the configuration is checked for
        #[arg(long, value_enum, default_value_t = Role::All)]
        role: Role,
    },
    /// Show Index Server version, uptime, and indexed-repo count
    Status {
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Show one Knowledge Graph node with its edge summary
    Node {
        #[arg(help = NODE_ADDRESS_HELP)]
        id: String,
        /// Repo qualifier for a bare symbol name
        #[arg(long)]
        repo: Option<String>,
        /// Repository-relative path fragment narrowing a bare symbol name
        #[arg(long)]
        path: Option<String>,
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Show a node's neighboring subgraph
    Neighbors {
        #[arg(help = NODE_ADDRESS_HELP)]
        id: String,
        /// Repo qualifier for a bare symbol name
        #[arg(long)]
        repo: Option<String>,
        /// Repository-relative path fragment narrowing a bare symbol name
        #[arg(long)]
        path: Option<String>,
        /// Follow only edges pointing this way: in, out, or both
        #[arg(long)]
        direction: Option<String>,
        /// Follow only edges of this kind (repeatable), e.g. CALLS
        #[arg(long = "kind", visible_alias = "edge-kinds")]
        kinds: Vec<String>,
        /// How many hops to traverse (1-3)
        #[arg(long)]
        depth: Option<u32>,
        /// Nodes per page (1-1000)
        #[arg(long)]
        limit: Option<u32>,
        /// Resume where the previous page's next_cursor left off
        #[arg(long)]
        cursor: Option<String>,
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Search the Knowledge Graph for symbols, code, and docs
    Search {
        #[arg(help = "Query, e.g. \"rate limit\"")]
        query: String,
        /// Restrict to this node kind (repeatable), e.g. Symbol or File
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Restrict to this repo qualifier (repeatable), e.g.
        /// github.com/acme/widgets
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Hits per page (1-100)
        #[arg(long)]
        limit: Option<u32>,
        /// Resume where the previous page's next_cursor left off
        #[arg(long)]
        cursor: Option<String>,
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Show the commits that touched a file (or a symbol's file), newest first
    History {
        #[arg(help = NODE_ADDRESS_HELP)]
        id: String,
        /// Repo qualifier for a bare symbol name
        #[arg(long)]
        repo: Option<String>,
        /// Repository-relative path fragment narrowing a bare symbol name
        #[arg(long)]
        path: Option<String>,
        /// Only commits at or after this date (RFC3339 or YYYY-MM-DD)
        #[arg(long)]
        since: Option<String>,
        /// Commits per page (1-1000)
        #[arg(long)]
        limit: Option<u32>,
        /// Resume where the previous page's next_cursor left off
        #[arg(long)]
        cursor: Option<String>,
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Proxy MCP stdio clients to the Index Server's Streamable HTTP endpoint
    Mcp,
    /// Install yggdrasil's navigation Skill for local agents
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Administer the Index Server (Admin token required)
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Install or update the yggdrasil navigation Skill
    Install,
}

#[derive(Subcommand)]
enum AdminCommand {
    /// Manage the repositories the Index Server syncs
    Repo {
        #[command(subcommand)]
        command: RepoCommand,
    },
    /// Connect Forge orgs for repository discovery
    Forge {
        #[command(subcommand)]
        command: ForgeCommand,
    },
    /// Manage repository discovery include/exclude rules
    Rules {
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Manage Member bearer tokens
    Token {
        #[command(subcommand)]
        command: TokenCommand,
    },
    /// Show every registered repo with its sync state
    Status {
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ForgeCommand {
    /// Connect a GitHub org for hourly discovery
    Add {
        #[arg(help = "Forge kind; currently only github")]
        kind: String,
        #[arg(help = "Organization slug, e.g. acme")]
        org: String,
        /// Forge root (default: <https://github.com>)
        #[arg(long)]
        base_url: Option<String>,
        /// Env var holding the Forge token
        #[arg(long)]
        token_env: Option<String>,
    },
    /// Request discovery for a connected org now
    Discover {
        #[arg(help = "Forge kind; currently only github")]
        kind: String,
        #[arg(help = "Organization slug, e.g. acme")]
        org: String,
        /// Forge root (default: <https://github.com>)
        #[arg(long)]
        base_url: Option<String>,
    },
}

#[derive(Subcommand)]
enum RulesCommand {
    /// Add or update an include/exclude glob rule
    Add {
        #[arg(help = "Repo slug glob, e.g. acme/private-*")]
        pattern: String,
        #[arg(long, value_enum)]
        action: RuleActionArg,
        /// Forge root the rule belongs to
        #[arg(long, default_value = "https://github.com")]
        forge: String,
        /// Let this rule apply to private repos
        #[arg(long = "private")]
        applies_to_private: bool,
    },
    /// List discovery rules in evaluation order
    List,
}

#[derive(Subcommand)]
enum TokenCommand {
    /// Issue a Member bearer token and print it once
    Issue {
        #[arg(help = "Member name for a human or agent")]
        member: String,
    },
    /// Revoke an active Member bearer token by id
    Revoke {
        #[arg(help = "Token id shown by `yg admin token issue`")]
        id: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RuleActionArg {
    Include,
    Exclude,
}

impl RuleActionArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Include => "include",
            Self::Exclude => "exclude",
        }
    }
}

#[derive(Subcommand)]
enum RepoCommand {
    /// Register a repository for Sync and queue its first fetch
    Add {
        #[arg(help = "Repository URL, e.g. https://github.com/acme/widgets")]
        url: String,
        /// Shallow-clone override: sync only the most recent N commits
        /// (default: full history)
        #[arg(long, value_parser = clap::value_parser!(i32).range(1..))]
        depth: Option<i32>,
        /// Poll this repo for changes every N seconds (default: the
        /// server's poll interval)
        #[arg(long, value_parser = clap::value_parser!(i32).range(1..))]
        poll_interval: Option<i32>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { role } => serve(role).await,
        Command::ConfigCheck { role } => config_check(role),
        Command::Status { json } => status(json).await,
        Command::Node {
            id,
            repo,
            path,
            json,
        } => node(id, repo, path, json).await,
        Command::Neighbors {
            id,
            repo,
            path,
            direction,
            kinds,
            depth,
            limit,
            cursor,
            json,
        } => neighbors(id, repo, path, direction, kinds, depth, limit, cursor, json).await,
        Command::Search {
            query,
            kinds,
            repos,
            limit,
            cursor,
            json,
        } => search(query, kinds, repos, limit, cursor, json).await,
        Command::History {
            id,
            repo,
            path,
            since,
            limit,
            cursor,
            json,
        } => history(id, repo, path, since, limit, cursor, json).await,
        Command::Mcp => mcp_proxy().await,
        Command::Skill {
            command: SkillCommand::Install,
        } => skill_install(),
        Command::Admin { command } => match command {
            AdminCommand::Repo {
                command:
                    RepoCommand::Add {
                        url,
                        depth,
                        poll_interval,
                    },
            } => admin_repo_add(url, depth, poll_interval).await,
            AdminCommand::Forge {
                command:
                    ForgeCommand::Add {
                        kind,
                        org,
                        base_url,
                        token_env,
                    },
            } => admin_forge_add(kind, org, base_url, token_env).await,
            AdminCommand::Forge {
                command:
                    ForgeCommand::Discover {
                        kind,
                        org,
                        base_url,
                    },
            } => admin_forge_discover(kind, org, base_url).await,
            AdminCommand::Rules { command } => match command {
                RulesCommand::Add {
                    pattern,
                    action,
                    forge,
                    applies_to_private,
                } => admin_rules_add(pattern, action, forge, applies_to_private).await,
                RulesCommand::List => admin_rules_list().await,
            },
            AdminCommand::Token { command } => match command {
                TokenCommand::Issue { member } => admin_token_issue(member).await,
                TokenCommand::Revoke { id } => admin_token_revoke(id).await,
            },
            AdminCommand::Status { json } => admin_status(json).await,
        },
    }
}

fn skill_install() -> anyhow::Result<()> {
    const SKILL_NAME: &str = "yggdrasil-navigation";
    const SKILL_DOCUMENT: &str = include_str!("../skills/yggdrasil-navigation/SKILL.md");

    let skill_dir = skill_home_dir()?
        .join(".claude")
        .join("skills")
        .join(SKILL_NAME);
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("creating {}", skill_dir.display()))?;
    let skill_path = skill_dir.join("SKILL.md");
    std::fs::write(&skill_path, SKILL_DOCUMENT)
        .with_context(|| format!("writing {}", skill_path.display()))?;
    println!("installed {SKILL_NAME} Skill at {}", skill_path.display());
    Ok(())
}

fn skill_home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.as_os_str().is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.as_os_str().is_empty()))
        .map(std::path::PathBuf::from)
        .context("HOME or USERPROFILE must be set to install Claude Code skills")
}

/// Where the Index Server lives and how to authenticate, from the same
/// env every client command reads.
fn client_env() -> anyhow::Result<(String, String)> {
    let env_server = std::env::var("YG_SERVER").ok();
    let env_token = std::env::var("YG_TOKEN").ok();
    // The env wins over the file, so a broken config file only matters
    // when the env leaves something for the file to answer. When only
    // the server is left open it has a built-in default, so a broken
    // file downgrades to a warning; a missing credential has no
    // fallback, so there the parse error surfaces.
    let config = match (&env_server, &env_token) {
        (Some(_), Some(_)) => client_config::ClientConfig::default(),
        (None, Some(_)) => read_client_config().unwrap_or_else(|e| {
            eprintln!("warning: ignoring client config ({e:#}); using the default server");
            client_config::ClientConfig::default()
        }),
        (_, None) => read_client_config()?,
    };
    let server = env_server
        .or(config.server)
        .unwrap_or_else(|| "http://127.0.0.1:7311".into());
    let server = server.trim_end_matches('/').to_string();
    let token = env_token
        .or(config.token)
        .context("YG_TOKEN must be set or token must be configured in ~/.config/yg/config.toml")?;
    Ok((server, token))
}

/// The client config file's settings, if the file exists. A file that
/// exists but does not read or parse is an error — silently ignoring a
/// credential file would fall back to the env and fail with a message
/// pointing away from the real problem.
fn read_client_config() -> anyhow::Result<client_config::ClientConfig> {
    let Some(path) = client_config_path() else {
        return Ok(client_config::ClientConfig::default());
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(client_config::ClientConfig::default());
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    client_config::parse_client_config(&contents)
        .with_context(|| format!("parsing {}", path.display()))
}

fn client_config_path() -> Option<std::path::PathBuf> {
    if let Some(home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(home).join("yg/config.toml"));
    }
    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".config")
            .join("yg")
            .join("config.toml")
    })
}

/// One HTTP exchange with the Index Server, shared by every subcommand:
/// send, then either parse the JSON response or fail with the server's
/// own reason. The client is built once — it pools connections and is
/// designed to be reused.
struct ServerResponse<T> {
    headers: reqwest::header::HeaderMap,
    body: T,
}

fn canonical_json<T: serde::Serialize>(body: &T) -> anyhow::Result<String> {
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

    let mut value = serde_json::to_value(body)?;
    sort_keys(&mut value);
    Ok(serde_json::to_string(&value)?)
}

async fn server_json<T>(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<ServerResponse<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    server_request(method, path, body).await
}

/// [`server_json`], but also returning the response headers — volatile
/// values (uptime) ride there instead of the cache-stable body.
async fn server_request<T>(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<ServerResponse<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let (server, token) = client_env()?;
    let mut request = CLIENT
        .get_or_init(reqwest::Client::new)
        .request(method, format!("{server}{path}"))
        .bearer_auth(&token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let resp = request
        .send()
        .await
        .with_context(|| format!("requesting {server}{path}"))?;
    let status = resp.status();
    let headers = resp.headers().clone();
    let text = resp.text().await.context("reading the server's response")?;
    if !status.is_success() {
        // Prefer the server's {"error": …} shape, but a proxy or crash
        // can answer with anything — show whatever came back.
        let reason = server_error_reason(&text).unwrap_or(text);
        bail!("the server answered {path} with {status}: {reason}");
    }
    let body = serde_json::from_str(&text)
        .with_context(|| format!("parsing the typed response from {path}"))?;
    Ok(ServerResponse { headers, body })
}

fn server_error_reason(text: &str) -> Option<String> {
    let body = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if let Some(message) = body.get("error").and_then(serde_json::Value::as_str) {
        return Some(message.to_string());
    }
    let payload =
        serde_json::from_value::<yg_verbs::NoSuchSymbol>(body.get("error")?.clone()).ok()?;
    let mut address = match payload.kind {
        yg_verbs::NoSuchSymbolKind::NoSuchSymbol => format!(
            "no such symbol {:?} in {}",
            payload.address.name.as_str(),
            payload.address.repo.as_str()
        ),
        yg_verbs::NoSuchSymbolKind::UnaddressableSymbol => format!(
            "symbol name {:?} in {} is un-addressable because it has no safely searchable term set",
            payload.address.name.as_str(),
            payload.address.repo.as_str()
        ),
    };
    if let Some(path) = payload.address.path {
        address.push_str(&format!(" under path {:?}", path.as_str()));
    }
    Some(address)
}

/// POST one Verb request (RFC 0001 §7).
async fn post_verb<T>(verb: &str, body: serde_json::Value) -> anyhow::Result<ServerResponse<T>>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    server_json(
        reqwest::Method::POST,
        &format!("/v1/verbs/{verb}"),
        Some(body),
    )
    .await
}

async fn mcp_proxy() -> anyhow::Result<()> {
    let (server, token) = client_env()?;
    let endpoint = format!("{server}/v1/mcp");
    let client = reqwest::Client::new();
    let stdin = std::io::stdin();
    let mut input = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    while let Some(body) = read_mcp_message(&mut input)? {
        let id = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|body| body.get("id").cloned())
            .unwrap_or(serde_json::Value::Null);
        let resp = client
            .post(&endpoint)
            .bearer_auth(&token)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("posting MCP request to {endpoint}"))?;
        let status = resp.status();
        let text = resp.text().await.context("reading MCP HTTP response")?;
        let parsed = serde_json::from_str::<serde_json::Value>(&text).ok();
        let Some(payload) = (if status.is_success() {
            if text.is_empty() { None } else { Some(text) }
        } else if parsed
            .as_ref()
            .is_some_and(|body| body.get("jsonrpc").is_some())
        {
            // The server answers protocol failures (parse errors, bad
            // requests) with a JSON-RPC envelope already; forward it
            // verbatim instead of burying it in a synthetic error.
            Some(text)
        } else {
            let reason = parsed
                .and_then(|body| body["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            Some(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32000,
                        "message": format!("MCP HTTP endpoint answered {status}: {reason}")
                    }
                })
                .to_string(),
            )
        }) else {
            continue;
        };
        write_mcp_message(&mut output, payload.as_bytes())?;
    }
    Ok(())
}

fn read_mcp_message<R>(input: &mut R) -> anyhow::Result<Option<Vec<u8>>>
where
    R: std::io::BufRead,
{
    let mut line = String::new();
    let read = input.read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    let body = line.trim_end_matches(['\r', '\n']).as_bytes().to_vec();
    Ok(Some(body))
}

fn write_mcp_message<W>(output: &mut W, body: &[u8]) -> anyhow::Result<()>
where
    W: std::io::Write,
{
    output.write_all(body)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

async fn node(
    id: String,
    repo: Option<String>,
    path: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let response = post_verb::<yg_verbs::AddressedResponse<yg_verbs::NodeResponse>>(
        "node",
        serde_json::to_value(yg_verbs::NodeRequest {
            id,
            repo: repo.map(yg_verbs::RepoQualifier::new),
            path: path.map(yg_verbs::SearchPath::new),
        })?,
    )
    .await?;
    if json {
        println!("{}", canonical_json(&response.body)?);
        return Ok(());
    }
    let body = match response.body {
        yg_verbs::AddressedResponse::Resolved(body) => body,
        yg_verbs::AddressedResponse::Ambiguous(ambiguous) => {
            print_candidates(&ambiguous);
            return Ok(());
        }
    };
    let node = &body.node;
    let kind = &node.kind;
    match node.name.as_deref() {
        Some(name) => println!("{kind} {name}"),
        None => println!("{kind}"),
    }
    println!("id:   {}", node.id);
    if let Some(path) = node.path.as_deref() {
        println!("path: {path}");
    }
    for (direction, summaries) in [("in", &body.edges.inbound), ("out", &body.edges.out)] {
        // Pad "in:" so both directions' columns line up.
        let label = if direction == "in" { "in: " } else { "out:" };
        if summaries.is_empty() {
            println!("{label}  (no edges)");
            continue;
        }
        for summary in summaries {
            // "in:   DEFINES ×1 (syntactic: 1)"
            let provenance = format_provenance(&summary.provenance);
            println!(
                "{label}  {} ×{} ({provenance})",
                summary.kind.as_str(),
                summary.count
            );
        }
    }
    Ok(())
}

fn print_candidates(ambiguous: &yg_verbs::AmbiguousNodeAddress) {
    println!(
        "ambiguous symbol {:?} in {}; choose one:",
        ambiguous.address.name.as_str(),
        ambiguous.address.repo.as_str()
    );
    if ambiguous.candidates.len() < ambiguous.total_matches {
        println!(
            "showing {} of {} matches",
            ambiguous.candidates.len(),
            ambiguous.total_matches
        );
    }
    for candidate in &ambiguous.candidates {
        println!(
            "{:.6}  {}  {}  ({})",
            candidate.confidence,
            candidate.kind,
            candidate.id,
            candidate.path.as_str()
        );
    }
}

fn format_provenance(counts: &std::collections::BTreeMap<yg_verbs::Provenance, i64>) -> String {
    let mut counts = counts
        .iter()
        .map(|(how, count)| (how.as_str(), count))
        .collect::<Vec<_>>();
    counts.sort_unstable_by_key(|(how, _)| *how);
    counts
        .into_iter()
        .map(|(how, count)| format!("{how}: {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod typed_render_tests {
    use super::*;

    #[test]
    fn provenance_keeps_the_wire_objects_lexicographic_display_order() {
        let counts = std::collections::BTreeMap::from([
            (yg_verbs::Provenance::Syntactic, 2),
            (yg_verbs::Provenance::Precise, 1),
            (yg_verbs::Provenance::Inferred, 3),
        ]);
        assert_eq!(
            format_provenance(&counts),
            "inferred: 3, precise: 1, syntactic: 2"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn neighbors(
    id: String,
    repo: Option<String>,
    path: Option<String>,
    direction: Option<String>,
    kinds: Vec<String>,
    depth: Option<u32>,
    limit: Option<u32>,
    cursor: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let response = post_verb::<yg_verbs::AddressedResponse<yg_verbs::NeighborsResponse>>(
        "neighbors",
        serde_json::to_value(yg_verbs::NeighborsRequest {
            shape: yg_verbs::TraversalShape {
                id,
                repo: repo.map(yg_verbs::RepoQualifier::new),
                path: path.map(yg_verbs::SearchPath::new),
                direction,
                edge_kinds: (!kinds.is_empty()).then_some(kinds),
                depth,
            },
            limit,
            cursor,
        })?,
    )
    .await?;
    if json {
        println!("{}", canonical_json(&response.body)?);
        return Ok(());
    }
    let body = match response.body {
        yg_verbs::AddressedResponse::Resolved(body) => body,
        yg_verbs::AddressedResponse::Ambiguous(ambiguous) => {
            print_candidates(&ambiguous);
            return Ok(());
        }
    };
    if body.nodes.is_empty() {
        println!("no neighbors");
        return Ok(());
    }
    for node in &body.nodes {
        print!("{}  {}", node.kind, node.id);
        if let Some(name) = node.name.as_deref() {
            print!("  ({name})");
        }
        println!();
    }
    for edge in &body.edges {
        let confidence = serde_json::to_string(&edge.confidence)?;
        print!(
            "{} -[{} {} {}]-> {}",
            edge.src,
            edge.kind.as_str(),
            edge.provenance.as_str(),
            confidence,
            edge.dst,
        );
        // The edge's witnessed site (a CALLS edge's call site), when it
        // has one.
        if let Some(location) = edge.location.as_deref() {
            print!("  @ {location}");
        }
        println!();
    }
    if let Some(cursor) = body.next_cursor {
        println!("more: pass --cursor {cursor}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn search(
    query: String,
    kinds: Vec<String>,
    repos: Vec<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let response = post_verb::<yg_verbs::SearchWireResponse>(
        "search",
        serde_json::to_value(yg_verbs::SearchRequest {
            query: Some(query),
            kinds: (!kinds.is_empty()).then_some(kinds),
            repos: (!repos.is_empty()).then_some(repos),
            mode: None,
            limit,
            cursor,
        })?,
    )
    .await?;
    if json {
        println!("{}", canonical_json(&response.body)?);
        return Ok(());
    }
    let body = response.body;
    if body.hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for hit in &body.hits {
        print!("{}  {}", hit.kind.as_str(), hit.id.external());
        if let Some(name) = hit.name.as_ref() {
            let name = name.as_str();
            print!("  ({name})");
        }
        println!();
        // The snippet rides along on its own indented line, with the
        // server's <b>…</b> match highlighting flattened to plain text.
        if let Some(snippet) = hit.snippet.as_ref() {
            println!("    {}", plain_snippet(snippet.as_str()));
        }
    }
    if let Some(cursor) = body.next_cursor {
        println!("more: pass --cursor {cursor}");
    }
    Ok(())
}

/// Flatten the server's HTML-highlighted snippet (`<b>…</b>`, with the
/// surrounding text HTML-escaped) into plain text for the terminal.
fn plain_snippet(html: &str) -> String {
    html.replace("<b>", "")
        .replace("</b>", "")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        // &amp; last, so an escaped entity in the source text isn't
        // double-unescaped.
        .replace("&amp;", "&")
}

async fn history(
    id: String,
    repo: Option<String>,
    path: Option<String>,
    since: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let response = post_verb::<yg_verbs::AddressedResponse<yg_verbs::HistoryResponse>>(
        "history",
        serde_json::to_value(yg_verbs::HistoryRequest {
            id,
            repo: repo.map(yg_verbs::RepoQualifier::new),
            path: path.map(yg_verbs::SearchPath::new),
            since,
            limit,
            cursor,
        })?,
    )
    .await?;
    if json {
        println!("{}", canonical_json(&response.body)?);
        return Ok(());
    }
    let body = match response.body {
        yg_verbs::AddressedResponse::Resolved(body) => body,
        yg_verbs::AddressedResponse::Ambiguous(ambiguous) => {
            print_candidates(&ambiguous);
            return Ok(());
        }
    };
    if body.commits.is_empty() {
        println!("no history");
        return Ok(());
    }
    for view in &body.commits {
        let commit = &view.commit;
        let sha = &commit.sha;
        // Short sha: .get so an odd server value never splits a UTF-8 boundary.
        let short = sha.get(..12).unwrap_or(sha);
        let date = &view.date;
        // The author's name, falling back to their email, then to a
        // placeholder for an unattributable commit.
        let who = commit
            .author
            .as_ref()
            .map(|author| author.name.as_deref().unwrap_or(&author.email))
            .unwrap_or("(unknown)");
        let subject = commit.subject.as_deref().unwrap_or("");
        println!("{short}  {date}  {who}  {subject}");
    }
    if let Some(cursor) = body.next_cursor {
        println!("more: pass --cursor {cursor}");
    }
    Ok(())
}

async fn admin_repo_add(
    url: String,
    depth: Option<i32>,
    poll_interval: Option<i32>,
) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::AddRepoResponse>(
        reqwest::Method::POST,
        "/v1/admin/repos",
        Some(serde_json::json!({"url": url, "depth": depth, "poll_interval": poll_interval})),
    )
    .await?;
    let body = response.body;
    let registered = if body.created {
        format!("registered {}", body.slug)
    } else {
        format!("{} already registered", body.slug)
    };
    let sync = if body.fetch_queued {
        "fetch queued"
    } else {
        "sync already pending"
    };
    println!("{registered} ({sync})");
    Ok(())
}

async fn admin_forge_add(
    kind: String,
    org: String,
    base_url: Option<String>,
    token_env: Option<String>,
) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::AddForgeResponse>(
        reqwest::Method::POST,
        "/v1/admin/forges",
        Some(serde_json::json!({
            "kind": kind,
            "org": org,
            "base_url": base_url,
            "token_env": token_env,
        })),
    )
    .await?;
    let body = response.body;
    if body.created {
        println!(
            "connected {} org {} ({})",
            body.kind, body.org, body.base_url
        );
    } else {
        println!(
            "{} org {} already connected ({})",
            body.kind, body.org, body.base_url
        );
    }
    Ok(())
}

async fn admin_forge_discover(
    kind: String,
    org: String,
    base_url: Option<String>,
) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::DiscoverForgeResponse>(
        reqwest::Method::POST,
        "/v1/admin/forges/discover",
        Some(serde_json::json!({
            "kind": kind,
            "org": org,
            "base_url": base_url,
        })),
    )
    .await?;
    let body = response.body;
    println!(
        "discovery requested for {} org {} ({})",
        body.kind, body.org, body.base_url
    );
    Ok(())
}

async fn admin_rules_add(
    pattern: String,
    action: RuleActionArg,
    forge: String,
    applies_to_private: bool,
) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::AddRuleResponse>(
        reqwest::Method::POST,
        "/v1/admin/rules",
        Some(serde_json::json!({
            "forge": forge,
            "pattern": pattern,
            "action": action.as_str(),
            "private": applies_to_private,
        })),
    )
    .await?;
    let body = response.body;
    let scope = if body.applies_to_private {
        "private"
    } else {
        "public/internal"
    };
    println!(
        "{} {} on {} ({scope}; {} fetches queued)",
        body.action, body.pattern, body.forge, body.fetches_queued
    );
    Ok(())
}

async fn admin_rules_list() -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::RulesResponse>(
        reqwest::Method::GET,
        "/v1/admin/rules",
        None,
    )
    .await?;
    if response.body.rules.is_empty() {
        println!("no discovery rules");
        return Ok(());
    }
    for rule in response.body.rules {
        let private = if rule.applies_to_private {
            "private"
        } else {
            "public/internal"
        };
        println!(
            "{}  {}  {}  {private}",
            rule.forge, rule.action, rule.pattern
        );
    }
    Ok(())
}

async fn admin_token_issue(member: String) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::IssueTokenResponse>(
        reqwest::Method::POST,
        "/v1/admin/tokens",
        Some(serde_json::json!({ "member": member })),
    )
    .await?;
    let body = response.body;
    println!("id: {}", body.id);
    println!("member: {}", body.member);
    println!("token: {}", body.token);
    println!("save this token now; it will not be shown again");
    Ok(())
}

async fn admin_token_revoke(id: String) -> anyhow::Result<()> {
    if !yg_control::member_token_id_is_valid(&id) {
        bail!("member token id must look like mtok_<24 hex characters>");
    }
    let response = server_json::<yg_verbs::admin::RevokeTokenResponse>(
        reqwest::Method::POST,
        &format!("/v1/admin/tokens/{id}/revoke"),
        None,
    )
    .await?;
    println!("revoked {}", response.body.id);
    Ok(())
}

async fn admin_status(json: bool) -> anyhow::Result<()> {
    let response = server_json::<yg_verbs::admin::AdminStatusResponse>(
        reqwest::Method::GET,
        "/v1/admin/status",
        None,
    )
    .await?;

    if json {
        println!("{}", canonical_json(&response.body)?);
        return Ok(());
    }
    let body = response.body;
    if body.repos.is_empty() {
        println!("no repositories registered — add one with: yg admin repo add <url>");
        return Ok(());
    }
    print_visibility_counts(body.visibility_counts);
    for repo in body.repos {
        let commit = repo
            .last_synced_commit
            .as_deref()
            // .get: never split a UTF-8 boundary, however odd the server's
            // idea of a commit id.
            .map(|sha| sha.get(..12).unwrap_or(sha))
            .unwrap_or("-");
        print!("{}  {}  {commit}", repo.slug, repo.sync.state);
        if repo.visibility != yg_verbs::admin::RepoVisibility::Public
            || repo.discovery_state != yg_verbs::admin::DiscoveryState::Included
        {
            print!("  ({}, {})", repo.visibility, repo.discovery_state);
        }
        if let Some(error) = repo.sync.last_error.as_deref() {
            print!("  [attempt {}: {error}]", repo.sync.attempts);
        }
        if let Some(shard) = repo.shard {
            print!(
                "  shard {} ({} nodes, {} edges)",
                shard.revision, shard.nodes, shard.edges
            );
        }
        if let Some(error) = repo.index.last_error.as_deref() {
            print!("  [index attempt {}: {error}]", repo.index.attempts);
        }
        println!();
    }
    Ok(())
}

#[cfg(test)]
fn parse_visibility_counts(
    body: &serde_json::Value,
) -> anyhow::Result<yg_verbs::admin::VisibilityCounts> {
    #[derive(serde::Deserialize)]
    struct AdminStatusSummary {
        visibility_counts: yg_verbs::admin::VisibilityCounts,
    }

    serde_json::from_value::<AdminStatusSummary>(body.clone())
        .map(|status| status.visibility_counts)
        .context("parsing visibility counts from /v1/admin/status")
}

fn print_visibility_counts(counts: yg_verbs::admin::VisibilityCounts) {
    println!(
        "visibility: public={} internal={} private={}",
        counts.public, counts.internal, counts.private,
    );
}

#[cfg(test)]
mod admin_status_tests {
    use super::*;

    #[test]
    fn visibility_counts_are_required_and_typed() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({
                "visibility_counts": {"public": "1", "internal": 0, "private": 0},
            }),
        ] {
            let error = parse_visibility_counts(&body)
                .err()
                .expect("missing or malformed counts must fail");
            assert!(
                error
                    .to_string()
                    .contains("parsing visibility counts from /v1/admin/status"),
                "unexpected error: {error:#}"
            );
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// REST + MCP API plus Sync and indexing workers in one process
    All,
    /// API only — pair with worker processes elsewhere
    Api,
    /// Workers only; optionally expose process-local metrics over HTTP
    Worker,
}

/// Resolve and validate the deployment configuration, print the report,
/// and exit — never connecting to anything. The settings table goes to
/// stdout (scripts diff it); validation errors surface as the command's
/// error, one line each, and a non-zero exit.
fn config_check(role: Role) -> anyhow::Result<()> {
    let resolution = deploy_config::resolve(role, |var| std::env::var(var).ok());
    // clap's value-enum name, so the report echoes exactly what --role accepts.
    let role_name = clap::ValueEnum::to_possible_value(&role)
        .expect("no Role variant is skipped")
        .get_name()
        .to_string();
    println!("resolved configuration (role: {role_name}):");
    for setting in &resolution.settings {
        let source = match setting.source {
            deploy_config::Source::Env => "(env)",
            deploy_config::Source::Default => "(default)",
            deploy_config::Source::Unset => "(unset)",
        };
        // Source before value: values (URLs, paths) have no width bound,
        // so they take the ragged column.
        println!("  {:<22} {source:<10} {}", setting.var, setting.shown);
    }
    resolution.into_config()?;
    println!("configuration valid");
    Ok(())
}

async fn serve(role: Role) -> anyhow::Result<()> {
    // Logs go to stderr; stdout carries only the address announcement so
    // scripts (and the e2e tests) can parse it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    // Install both streams before configuration, database, object-store,
    // or socket boot can yield. Unix restores its default hard-kill
    // behavior if a listener has not been created yet.
    let mut signals = ShutdownSignals::install()?;

    // The one read of the deployment environment: every YG_* setting
    // resolves and validates here, before anything connects.
    let deploy = deploy_config::resolve(role, |var| std::env::var(var).ok()).into_config()?;
    let metrics = yg_api::Metrics::new();

    match role {
        Role::Api => {
            let mut server = yg_api::serve_with_metrics(
                server_config(&deploy)?,
                metrics,
                metrics_access(&deploy),
            )
            .await?;
            println!("listening on http://{}", server.local_addr());
            tokio::select! {
                result = server.wait() => {
                    let failure = component_exit(result, "API server");
                    log_shutdown_failure(&failure, "API server");
                    failure
                },
                signal = signals.recv() => {
                    signal?;
                    let deadlines = ShutdownDeadlines::for_cause(yg_sync::ShutdownCause::Signal);
                    server.begin_shutdown();
                    drain_until(
                        deadlines.terminate,
                        server.wait(),
                        "API server",
                        yg_sync::ShutdownCause::Signal,
                        &mut signals,
                    ).await
                }
            }
        }
        Role::Worker => {
            let workers = workers(&deploy, &metrics).await?;
            let (shutdown_trigger, shutdown) = yg_sync::shutdown_channel();
            if let Some(listen) = deploy.worker_metrics_addr {
                let mut server = yg_api::serve_metrics(
                    listen,
                    metrics,
                    metrics_access(&deploy),
                    deploy.bootstrap_token.clone(),
                )
                .await?;
                println!("listening on http://{}", server.local_addr());
                let mut running = std::pin::pin!(run_workers(
                    &deploy,
                    workers,
                    shutdown_trigger.clone(),
                    shutdown,
                ));
                tokio::select! {
                    result = server.wait() => {
                        let failure = component_exit(result, "worker metrics server");
                        log_shutdown_failure(&failure, "worker metrics server");
                        let cause = yg_sync::ShutdownCause::Failure;
                        let deadlines = ShutdownDeadlines::for_cause(cause);
                        shutdown_trigger.request(deadlines.work, cause);
                        drain_until(deadlines.terminate, &mut running, "workers", cause, &mut signals).await?;
                        failure
                    },
                    result = &mut running => {
                        let failure = component_exit(result, "workers");
                        log_shutdown_failure(&failure, "workers");
                        let cause = yg_sync::ShutdownCause::Failure;
                        let deadlines = ShutdownDeadlines::for_cause(cause);
                        server.begin_shutdown();
                        drain_until(deadlines.terminate, server.wait(), "worker metrics server", cause, &mut signals).await?;
                        failure
                    },
                    signal = signals.recv() => {
                        signal?;
                        let cause = yg_sync::ShutdownCause::Signal;
                        let deadlines = ShutdownDeadlines::for_cause(cause);
                        server.begin_shutdown();
                        shutdown_trigger.request(deadlines.work, cause);
                        drain_until(deadlines.terminate, async {
                            let (server_result, worker_result) = tokio::join!(server.wait(), &mut running);
                            server_result?;
                            worker_result
                        }, "worker metrics server and workers", cause, &mut signals).await
                    },
                }
            } else {
                println!("worker running");
                let mut running = std::pin::pin!(run_workers(
                    &deploy,
                    workers,
                    shutdown_trigger.clone(),
                    shutdown,
                ));
                tokio::select! {
                    result = &mut running => result,
                    signal = signals.recv() => {
                        signal?;
                        let cause = yg_sync::ShutdownCause::Signal;
                        let deadlines = ShutdownDeadlines::for_cause(cause);
                        shutdown_trigger.request(deadlines.work, cause);
                        drain_until(
                            deadlines.terminate,
                            &mut running,
                            "workers",
                            cause,
                            &mut signals,
                        ).await
                    }
                }
            }
        }
        Role::All => {
            let mut server = yg_api::serve_with_metrics(
                server_config(&deploy)?,
                metrics.clone(),
                metrics_access(&deploy),
            )
            .await?;
            let workers = workers(&deploy, &metrics).await?;
            let (shutdown_trigger, shutdown) = yg_sync::shutdown_channel();
            let mut worker_stopping = shutdown.clone();
            // Announce only once the whole process is up: scripts and
            // the e2e harness treat this line as the readiness signal,
            // and a worker-boot failure after it would read as a crash
            // mid-serve instead of a boot failure.
            println!("listening on http://{}", server.local_addr());
            // Either side dying takes the process down — a half-alive
            // server that accepts repos it will never sync helps nobody.
            let mut running_workers = std::pin::pin!(run_workers(
                &deploy,
                workers,
                shutdown_trigger.clone(),
                shutdown,
            ));
            tokio::select! {
                result = server.wait() => {
                    let failure = component_exit(result, "API server");
                    log_shutdown_failure(&failure, "API server");
                    let cause = yg_sync::ShutdownCause::Failure;
                    let deadlines = ShutdownDeadlines::for_cause(cause);
                    shutdown_trigger.request(deadlines.work, cause);
                    drain_until(
                        deadlines.terminate,
                        &mut running_workers,
                        "workers",
                        cause,
                        &mut signals,
                    ).await?;
                    failure
                },
                result = &mut running_workers => {
                    let failure = component_exit(result, "workers");
                    log_shutdown_failure(&failure, "workers");
                    let cause = yg_sync::ShutdownCause::Failure;
                    let terminate = worker_stopping.request()
                        .map(|request| request.work_deadline() + LEASE_RELEASE_RESERVE)
                        .unwrap_or_else(|| ShutdownDeadlines::for_cause(cause).terminate);
                    server.begin_shutdown();
                    drain_until(
                        terminate,
                        server.wait(),
                        "API server",
                        cause,
                        &mut signals,
                    ).await?;
                    failure
                },
                request = worker_stopping.requested() => {
                    let cause = request.cause();
                    let terminate = request.work_deadline() + LEASE_RELEASE_RESERVE;
                    server.begin_shutdown();
                    let result = drain_until(terminate, async {
                        let (server_result, worker_result) = tokio::join!(
                            server.wait(),
                            &mut running_workers,
                        );
                        server_result?;
                        worker_result.context("worker exited")
                    }, "combined server and workers", cause, &mut signals).await;
                    result?;
                    match cause {
                        yg_sync::ShutdownCause::Signal => Ok(()),
                        yg_sync::ShutdownCause::Failure => {
                            bail!("worker failure initiated shutdown")
                        }
                    }
                },
                signal = signals.recv() => {
                    signal?;
                    let cause = yg_sync::ShutdownCause::Signal;
                    let deadlines = ShutdownDeadlines::for_cause(cause);
                    server.begin_shutdown();
                    drain_until(deadlines.terminate, async {
                        // Workers stay live until every admitted API
                        // request has finished and can enqueue its final
                        // job. The API phase is itself bounded so worker
                        // lease release retains its reserved time.
                        let (api_result, worker_ended) = tokio::select! {
                            server_result = server.wait() => (Some(server_result), None),
                            worker_result = &mut running_workers => (None, Some(worker_result)),
                            _ = tokio::time::sleep_until(deadlines.api) => {
                                tracing::warn!("API drain phase elapsed; starting worker drain");
                                (None, None)
                            }
                        };
                        if let Some(worker_result) = worker_ended {
                            server.wait().await?;
                            return worker_result.context("worker exited");
                        }
                        shutdown_trigger.request(deadlines.work, cause);
                        let worker_result = if api_result.is_some() {
                            (&mut running_workers).await
                        } else {
                            let (server_result, worker_result) = tokio::join!(
                                server.wait(),
                                &mut running_workers,
                            );
                            server_result?;
                            worker_result
                        };
                        if let Some(server_result) = api_result {
                            server_result?;
                        }
                        worker_result.context("worker exited")
                    }, "combined server and workers", cause, &mut signals).await
                },
            }
        }
    }
}

fn metrics_access(deploy: &deploy_config::DeployConfig) -> yg_api::MetricsAccess {
    if deploy.metrics_unauthenticated {
        yg_api::MetricsAccess::Unauthenticated
    } else {
        yg_api::MetricsAccess::Admin
    }
}

/// Maximum wall-clock time allowed for the API and workers to drain
/// after shutdown begins. The final two seconds are reserved for a
/// worker to return an unfinished fenced lease before process exit.
const SHUTDOWN_DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);
const API_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
const LEASE_RELEASE_RESERVE: std::time::Duration = std::time::Duration::from_secs(2);
const FAILURE_WORK_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy)]
struct ShutdownDeadlines {
    api: tokio::time::Instant,
    work: tokio::time::Instant,
    terminate: tokio::time::Instant,
}

impl ShutdownDeadlines {
    fn for_cause(cause: yg_sync::ShutdownCause) -> Self {
        let now = tokio::time::Instant::now();
        match cause {
            yg_sync::ShutdownCause::Signal => {
                let terminate = now + SHUTDOWN_DRAIN_DEADLINE;
                Self {
                    api: now + API_DRAIN_BUDGET,
                    work: terminate - LEASE_RELEASE_RESERVE,
                    terminate,
                }
            }
            yg_sync::ShutdownCause::Failure => {
                let work = now + FAILURE_WORK_DRAIN_BUDGET;
                Self {
                    api: now,
                    work,
                    terminate: work + LEASE_RELEASE_RESERVE,
                }
            }
        }
    }
}

async fn drain_until<F>(
    deadline: tokio::time::Instant,
    future: F,
    component: &'static str,
    cause: yg_sync::ShutdownCause,
    signals: &mut ShutdownSignals,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    tokio::select! {
        result = future => result,
        _ = tokio::time::sleep_until(deadline) => {
            tracing::warn!(component, "shutdown drain deadline elapsed");
            // Tokio waits indefinitely for started spawn_blocking tasks
            // when its runtime drops. A hard process exit is therefore
            // the only wall-clock guarantee after graceful cleanup has
            // consumed the full advertised budget.
            std::process::exit(shutdown_exit_code(cause, true))
        }
        signal = signals.recv() => {
            if let Err(error) = signal {
                tracing::error!(error = format!("{error:#}"), "shutdown signal listener failed");
            } else {
                tracing::warn!("shutdown signal received during drain; forcing exit");
            }
            std::process::exit(shutdown_exit_code(cause, true))
        }
    }
}

const fn shutdown_exit_code(cause: yg_sync::ShutdownCause, forced: bool) -> i32 {
    match (cause, forced) {
        (yg_sync::ShutdownCause::Signal, false) => 0,
        (yg_sync::ShutdownCause::Signal | yg_sync::ShutdownCause::Failure, true)
        | (yg_sync::ShutdownCause::Failure, false) => 1,
    }
}

fn component_exit(result: anyhow::Result<()>, component: &'static str) -> anyhow::Result<()> {
    match result {
        Ok(()) => bail!("{component} exited unexpectedly"),
        Err(error) => Err(error).context(format!("{component} failed")),
    }
}

fn log_shutdown_failure(result: &anyhow::Result<()>, component: &'static str) {
    let error = result
        .as_ref()
        .expect_err("component_exit always returns an error");
    tracing::error!(
        component,
        error = format!("{error:#}"),
        "component failure initiated shutdown"
    );
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn install() -> anyhow::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .context("installing SIGINT handler")?,
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("installing SIGTERM handler")?,
        })
    }

    async fn recv(&mut self) -> anyhow::Result<()> {
        tokio::select! {
            signal = self.interrupt.recv() => signal.context("SIGINT stream ended").map(drop),
            signal = self.terminate.recv() => signal.context("SIGTERM stream ended").map(drop),
        }
    }
}

#[cfg(windows)]
struct ShutdownSignals {
    interrupt: tokio::signal::windows::CtrlC,
}

#[cfg(windows)]
impl ShutdownSignals {
    fn install() -> anyhow::Result<Self> {
        Ok(Self {
            interrupt: tokio::signal::windows::ctrl_c().context("installing Ctrl-C handler")?,
        })
    }

    async fn recv(&mut self) -> anyhow::Result<()> {
        self.interrupt
            .recv()
            .await
            .context("interrupt stream ended")
    }
}

#[cfg(all(not(unix), not(windows)))]
struct ShutdownSignals {
    interrupt: tokio::sync::mpsc::Receiver<anyhow::Result<()>>,
}

#[cfg(all(not(unix), not(windows)))]
impl ShutdownSignals {
    fn install() -> anyhow::Result<Self> {
        let (sender, interrupt) = tokio::sync::mpsc::channel(1);
        tokio::spawn(async move {
            // Persistent: each interrupt is forwarded so a second signal
            // during the drain still reaches the drain's listener instead
            // of the stream ending after the first delivery.
            loop {
                let result = tokio::signal::ctrl_c()
                    .await
                    .context("installing interrupt handler");
                let failed = result.is_err();
                if sender.send(result).await.is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self { interrupt })
    }

    async fn recv(&mut self) -> anyhow::Result<()> {
        self.interrupt
            .recv()
            .await
            .context("interrupt stream ended")?
    }
}

/// The API server's slice of the deployment config. The bootstrap token
/// was already validated for API-serving roles during resolution.
fn server_config(deploy: &deploy_config::DeployConfig) -> anyhow::Result<yg_api::ServerConfig> {
    Ok(yg_api::ServerConfig {
        listen: deploy.listen,
        database_url: deploy.database_url.clone(),
        object_store: deploy.object_store.clone(),
        bootstrap_token: deploy
            .bootstrap_token
            .clone()
            .context("YG_BOOTSTRAP_TOKEN must be set for API-serving roles")?,
        shard_cache: deploy.shard_cache.clone(),
    })
}

/// Build the worker pair and retain a control-plane handle for periodic
/// metric refreshes. Workers need the control plane, the git
/// cache, and object storage (Shards land there). A bootstrap token is only
/// consumed by the optional authenticated metrics listener at composition.
async fn workers(
    deploy: &deploy_config::DeployConfig,
    metrics: &yg_api::Metrics,
) -> anyhow::Result<(
    yg_sync::SyncWorker,
    yg_index::IndexWorker,
    yg_control::ControlPlane,
)> {
    let control = yg_control::ControlPlane::connect_and_migrate_with_metrics(
        &deploy.database_url,
        metrics.control(),
    )
    .await?;
    let store = deploy.object_store.connect()?;
    // Fail at boot, not on every index job: connect() never touches the
    // network, so this probe is the first thing that would notice a
    // missing or wrong YG_S3_* configuration.
    yg_api::probe_object_store(store.as_ref())
        .await
        .context("object storage unreachable at worker boot")?;
    Ok((
        yg_sync::SyncWorker::with_metrics(
            control.clone(),
            &deploy.git_cache,
            metrics.sync_worker(),
        ),
        yg_index::IndexWorker::new(control.clone(), store, &deploy.git_cache),
        control,
    ))
}

/// Drain both job queues forever, each on its own loop so a slow job of
/// one kind (a cold monorepo clone, a huge syntactic pass) never stalls
/// the other queue. An error from either side ends both.
async fn run_workers(
    deploy: &deploy_config::DeployConfig,
    (sync, index, control): (
        yg_sync::SyncWorker,
        yg_index::IndexWorker,
        yg_control::ControlPlane,
    ),
    shutdown_trigger: yg_sync::ShutdownTrigger,
    shutdown: yg_sync::Shutdown,
) -> anyhow::Result<()> {
    let mut shutdown_monitor = shutdown.clone();
    // Converge after a deploy that bumped the Shard schema: the read
    // path refuses artifacts from older schema versions, so every
    // stranded Shard needs re-indexing. Queued once at boot, then
    // drained like any other index work. A control-plane hiccup here
    // shouldn't keep the workers from starting — the next boot retries.
    if let Err(e) = index.requeue_outdated_shards().await {
        tracing::warn!(
            error = format!("{e:#}"),
            "could not queue re-index of outdated Shards"
        );
    }
    // Continuous Sync (RFC 0001 §3, issue #9): the poll loop watches
    // indexed repos for pushed changes and the GC loop reclaims
    // superseded Shards after their grace window, both beside the queue
    // drains. Each loop runs on its own task so a slow job of one kind
    // never stalls another. Queue and poll control-plane errors end the
    // process; GC logs transient sweep failures and retries on its cadence.
    let poll = yg_sync::PollConfig {
        default_interval: deploy.poll_interval,
        jitter_fraction: POLL_JITTER_FRACTION,
    };
    let discovery = yg_sync::DiscoveryConfig {
        interval: deploy.discovery_interval,
    };
    let discovery_shutdown = shutdown.clone();
    let discover = supervise_worker_loop(
        shutdown_trigger.clone(),
        drain_queue(shutdown.clone(), || {
            sync.discover_once_with_shutdown(&discovery, discovery_shutdown.clone())
        }),
    );
    let fetch_shutdown = shutdown.clone();
    let fetch = supervise_worker_loop(
        shutdown_trigger.clone(),
        drain_queue(shutdown.clone(), || {
            sync.run_once_with_shutdown(fetch_shutdown.clone())
        }),
    );
    let index_shutdown = shutdown.clone();
    let indexing = supervise_worker_loop(
        shutdown_trigger.clone(),
        drain_queue(shutdown.clone(), || {
            index.run_once_with_shutdown(index_shutdown.clone())
        }),
    );
    let polling = supervise_worker_loop(
        shutdown_trigger.clone(),
        drain_queue(shutdown.clone(), || sync.poll_once(&poll)),
    );
    let gc = supervise_worker_loop(
        shutdown_trigger,
        gc_loop(
            &index,
            &control,
            deploy.gc_grace,
            deploy.job_retention,
            deploy.gc_interval,
            shutdown,
        ),
    );
    let joined = async { tokio::join!(discover, fetch, indexing, polling, gc) };
    let mut joined = std::pin::pin!(joined);
    let results = tokio::select! {
        results = &mut joined => results,
        request = shutdown_monitor.requested() => {
            let terminate = request.work_deadline() + LEASE_RELEASE_RESERVE;
            match tokio::time::timeout_at(terminate, &mut joined).await {
                Ok(results) => results,
                Err(_) => {
                    tracing::warn!("worker shutdown deadline elapsed");
                    std::process::exit(shutdown_exit_code(request.cause(), true))
                }
            }
        }
    };
    results.0?;
    results.1?;
    results.2?;
    results.3?;
    results.4
}

async fn supervise_worker_loop<F>(
    shutdown: yg_sync::ShutdownTrigger,
    future: F,
) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let result = future.await;
    match result {
        Ok(()) => {
            let cause = yg_sync::ShutdownCause::Failure;
            let initiated = shutdown.request(ShutdownDeadlines::for_cause(cause).work, cause);
            if initiated {
                bail!("worker loop exited unexpectedly")
            }
            Ok(())
        }
        Err(error) => {
            let cause = yg_sync::ShutdownCause::Failure;
            let initiated = shutdown.request(ShutdownDeadlines::for_cause(cause).work, cause);
            if initiated {
                tracing::error!(
                    error = format!("{error:#}"),
                    "worker loop failure initiated shutdown"
                );
            } else {
                tracing::error!(
                    error = format!("{error:#}"),
                    "worker loop failed during shutdown"
                );
            }
            Err(error)
        }
    }
}

/// Run one queue's claim loop forever, sleeping briefly when it's empty.
/// Drives the poll loop too: `poll_once` returns `true` while repos are
/// due (claiming one per call) and `false` when none is, so the same
/// work-or-sleep shape applies.
async fn drain_queue<F, Fut>(mut shutdown: yg_sync::Shutdown, run_once: F) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<bool>>,
{
    loop {
        if shutdown.deadline().is_some() {
            return Ok(());
        }
        if !run_once().await? {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
                _ = shutdown.requested() => return Ok(()),
            }
        }
    }
}

/// Fraction of the interval added as random jitter so a forge's repos
/// don't poll in lockstep.
const POLL_JITTER_FRACTION: f64 = 0.2;

/// Reclaim superseded Shards and retire old terminal jobs on a fixed
/// cadence. Unlike a queue drain there is no per-item "work or not":
/// the sweep runs every `interval`, reclaiming whatever has aged past
/// `grace` and removing terminal job rows older than `job_retention`.
/// A sweep that fails (a control-plane or object-storage blip) is
/// logged and retried next interval rather than ending the process. The
/// queue-depth gauge refresh shares this cadence so scrape frequency never
/// drives database work. Both maintenance activities are best-effort.
async fn gc_loop(
    index: &yg_index::IndexWorker,
    control: &yg_control::ControlPlane,
    grace: std::time::Duration,
    job_retention: std::time::Duration,
    interval: std::time::Duration,
    mut shutdown: yg_sync::Shutdown,
) -> anyhow::Result<()> {
    loop {
        if shutdown.deadline().is_some() {
            return Ok(());
        }
        if let Err(e) = control.refresh_job_queue_depths().await {
            tracing::warn!(
                error = format!("{e:#}"),
                "job queue depth refresh failed; retaining stale gauges until next GC interval"
            );
        }
        if let Err(e) = index.gc_once(grace).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "shard GC sweep failed; retrying next interval"
            );
        }
        if let Err(e) = index.retire_terminal_jobs(job_retention).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "terminal-job retention sweep failed; retrying next interval"
            );
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.requested() => return Ok(()),
        }
    }
}

async fn status(json: bool) -> anyhow::Result<()> {
    let (server, _) = client_env()?;
    let response = server_request::<yg_verbs::status::StatusResponse>(
        reqwest::Method::GET,
        "/v1/status",
        None,
    )
    .await?;
    let uptime_header = response
        .headers
        .get(yg_api::UPTIME_HEADER)
        .with_context(|| format!("{0} is missing from /v1/status", yg_api::UPTIME_HEADER))?;
    let uptime_header = uptime_header.to_str().with_context(|| {
        format!(
            "parsing the {} header from /v1/status as text",
            yg_api::UPTIME_HEADER
        )
    })?;
    let uptime_seconds: u64 = uptime_header.parse().with_context(|| {
        format!(
            "parsing the {} header from /v1/status as seconds",
            yg_api::UPTIME_HEADER
        )
    })?;

    if json {
        // The server keeps volatile uptime out of the (cache-stable)
        // body; fold it back in for machine consumers, keeping the
        // body's key-sorted form.
        let mut body = serde_json::to_value(&response.body)?;
        let map = body
            .as_object_mut()
            .context("the typed /v1/status response must serialize as an object")?;
        map.insert("uptime_seconds".into(), uptime_seconds.into());
        println!("{}", canonical_json(&body)?);
    } else {
        let body = response.body;
        println!("yggdrasil Index Server at {server}");
        println!("version:       {}", body.version);
        println!("uptime:        {uptime_seconds}s");
        println!("repos indexed: {}", body.repos_indexed);
    }
    Ok(())
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    #[test]
    fn exit_code_is_clean_only_for_a_fully_drained_signal_shutdown() {
        assert_eq!(shutdown_exit_code(yg_sync::ShutdownCause::Signal, false), 0);
        assert_eq!(shutdown_exit_code(yg_sync::ShutdownCause::Signal, true), 1);
        assert_eq!(
            shutdown_exit_code(yg_sync::ShutdownCause::Failure, false),
            1
        );
        assert_eq!(shutdown_exit_code(yg_sync::ShutdownCause::Failure, true), 1);
    }

    #[test]
    fn failure_drain_is_shorter_but_keeps_the_release_reserve() {
        let signal = ShutdownDeadlines::for_cause(yg_sync::ShutdownCause::Signal);
        let failure = ShutdownDeadlines::for_cause(yg_sync::ShutdownCause::Failure);

        assert_eq!(signal.terminate - signal.work, LEASE_RELEASE_RESERVE);
        assert_eq!(failure.terminate - failure.work, LEASE_RELEASE_RESERVE);
        assert!(failure.terminate < signal.terminate);
    }

    #[tokio::test]
    async fn worker_error_preserves_the_error_and_requests_a_short_failure_drain() {
        let (trigger, mut shutdown) = yg_sync::shutdown_channel();
        let earliest_expected_deadline = tokio::time::Instant::now() + FAILURE_WORK_DRAIN_BUDGET;

        let error = supervise_worker_loop(trigger, async {
            Err(anyhow::anyhow!("control plane unavailable"))
        })
        .await
        .expect_err("worker error must be preserved");
        let latest_expected_deadline = tokio::time::Instant::now() + FAILURE_WORK_DRAIN_BUDGET;
        let request = shutdown.requested().await;

        assert!(error.to_string().contains("control plane unavailable"));
        assert_eq!(request.cause(), yg_sync::ShutdownCause::Failure);
        assert!(request.work_deadline() >= earliest_expected_deadline);
        assert!(request.work_deadline() <= latest_expected_deadline);
    }
}

#[cfg(test)]
mod address_tests {
    use super::*;

    #[test]
    fn node_address_help_documents_matching_and_zero_term_names() {
        for command in ["node", "neighbors", "history"] {
            let error = match Cli::try_parse_from(["yg", command, "--help"]) {
                Ok(_) => panic!("--help must exit through clap's display error"),
                Err(error) => error,
            };
            let help = error.to_string();
            assert!(help.contains("byte-exact"), "{command} help: {help}");
            assert!(help.contains("case-sensitive"), "{command} help: {help}");
            assert!(help.contains("un-addressable"), "{command} help: {help}");
        }
    }

    #[test]
    fn zero_term_symbol_404_is_not_reported_as_absent() {
        let body = serde_json::json!({
            "error": {
                "kind": "unaddressable_symbol",
                "address": {
                    "name": "::",
                    "repo": "github.com/acme/widgets"
                }
            }
        });

        let reason = server_error_reason(&body.to_string()).expect("typed error is rendered");

        assert!(reason.contains("un-addressable"), "{reason}");
        assert!(reason.contains("no safely searchable term set"), "{reason}");
        assert!(!reason.contains("no such symbol"), "{reason}");
    }
}
