//! yg binary: subcommands, serve roles, MCP proxy.

mod client_config;
mod deploy_config;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

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
        #[arg(help = "Node id, e.g. sym:github.com/acme/widgets:main.go#Hello")]
        id: String,
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
    /// Show a node's neighboring subgraph
    Neighbors {
        #[arg(help = "Node id, e.g. file:github.com/acme/widgets:main.go")]
        id: String,
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
        #[arg(help = "Node id, e.g. file:github.com/acme/widgets:main.go")]
        id: String,
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
        Command::Node { id, json } => node(id, json).await,
        Command::Neighbors {
            id,
            direction,
            kinds,
            depth,
            limit,
            cursor,
            json,
        } => neighbors(id, direction, kinds, depth, limit, cursor, json).await,
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
            since,
            limit,
            cursor,
            json,
        } => history(id, since, limit, cursor, json).await,
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
async fn server_json(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    Ok(server_request(method, path, body).await?.1)
}

/// [`server_json`], but also returning the response headers — volatile
/// values (uptime) ride there instead of the cache-stable body.
async fn server_request(
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<(reqwest::header::HeaderMap, serde_json::Value)> {
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
        let reason = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|body| body["error"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!("the server answered {path} with {status}: {reason}");
    }
    let body =
        serde_json::from_str(&text).with_context(|| format!("parsing the response from {path}"))?;
    Ok((headers, body))
}

/// POST one Verb request (RFC 0001 §7).
async fn post_verb(verb: &str, body: serde_json::Value) -> anyhow::Result<serde_json::Value> {
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

async fn node(id: String, json: bool) -> anyhow::Result<()> {
    let body = post_verb("node", serde_json::to_value(yg_verbs::NodeRequest { id })?).await?;
    if json {
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    let node = &body["node"];
    let kind = node["kind"].as_str().unwrap_or("?");
    match node["name"].as_str() {
        Some(name) => println!("{kind} {name}"),
        None => println!("{kind}"),
    }
    println!("id:   {}", node["id"].as_str().unwrap_or("?"));
    if let Some(path) = node["path"].as_str() {
        println!("path: {path}");
    }
    for direction in ["in", "out"] {
        // Pad "in:" so both directions' columns line up.
        let label = if direction == "in" { "in: " } else { "out:" };
        let summaries = body["edges"][direction]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if summaries.is_empty() {
            println!("{label}  (no edges)");
            continue;
        }
        for summary in summaries {
            // "in:   DEFINES ×1 (syntactic: 1)"
            let provenance = summary["provenance"]
                .as_object()
                .map(|p| {
                    p.iter()
                        .map(|(how, n)| format!("{how}: {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            println!(
                "{label}  {} ×{} ({provenance})",
                summary["kind"].as_str().unwrap_or("?"),
                summary["count"]
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn neighbors(
    id: String,
    direction: Option<String>,
    kinds: Vec<String>,
    depth: Option<u32>,
    limit: Option<u32>,
    cursor: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let body = post_verb(
        "neighbors",
        serde_json::to_value(yg_verbs::NeighborsRequest {
            shape: yg_verbs::TraversalShape {
                id,
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
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    let nodes = body["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if nodes.is_empty() {
        println!("no neighbors");
        return Ok(());
    }
    for node in nodes {
        print!(
            "{}  {}",
            node["kind"].as_str().unwrap_or("?"),
            node["id"].as_str().unwrap_or("?")
        );
        if let Some(name) = node["name"].as_str() {
            print!("  ({name})");
        }
        println!();
    }
    for edge in body["edges"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
        print!(
            "{} -[{} {} {}]-> {}",
            edge["src"].as_str().unwrap_or("?"),
            edge["kind"].as_str().unwrap_or("?"),
            edge["provenance"].as_str().unwrap_or("?"),
            edge["confidence"],
            edge["dst"].as_str().unwrap_or("?"),
        );
        // The edge's witnessed site (a CALLS edge's call site), when it
        // has one.
        if let Some(location) = edge["location"].as_str() {
            print!("  @ {location}");
        }
        println!();
    }
    if let Some(cursor) = body["next_cursor"].as_str() {
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
    let body = post_verb(
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
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    let hits = body["hits"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if hits.is_empty() {
        println!("no matches");
        return Ok(());
    }
    for hit in hits {
        print!(
            "{}  {}",
            hit["kind"].as_str().unwrap_or("?"),
            hit["id"].as_str().unwrap_or("?")
        );
        if let Some(name) = hit["name"].as_str() {
            print!("  ({name})");
        }
        println!();
        // The snippet rides along on its own indented line, with the
        // server's <b>…</b> match highlighting flattened to plain text.
        if let Some(snippet) = hit["snippet"].as_str() {
            println!("    {}", plain_snippet(snippet));
        }
    }
    if let Some(cursor) = body["next_cursor"].as_str() {
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
    since: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
    json: bool,
) -> anyhow::Result<()> {
    let body = post_verb(
        "history",
        serde_json::to_value(yg_verbs::HistoryRequest {
            id,
            since,
            limit,
            cursor,
        })?,
    )
    .await?;
    if json {
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    let commits = body["commits"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if commits.is_empty() {
        println!("no history");
        return Ok(());
    }
    for commit in commits {
        let sha = commit["sha"].as_str().unwrap_or("?");
        // Short sha: .get so an odd server value never splits a UTF-8 boundary.
        let short = sha.get(..12).unwrap_or(sha);
        let date = commit["date"].as_str().unwrap_or("?");
        // The author's name, falling back to their email, then to a
        // placeholder for an unattributable commit.
        let who = commit["author"]["name"]
            .as_str()
            .or_else(|| commit["author"]["email"].as_str())
            .unwrap_or("(unknown)");
        let subject = commit["subject"].as_str().unwrap_or("");
        println!("{short}  {date}  {who}  {subject}");
    }
    if let Some(cursor) = body["next_cursor"].as_str() {
        println!("more: pass --cursor {cursor}");
    }
    Ok(())
}

async fn admin_repo_add(
    url: String,
    depth: Option<i32>,
    poll_interval: Option<i32>,
) -> anyhow::Result<()> {
    let body = server_json(
        reqwest::Method::POST,
        "/v1/admin/repos",
        Some(serde_json::json!({"url": url, "depth": depth, "poll_interval": poll_interval})),
    )
    .await?;
    let slug = body["slug"].as_str().unwrap_or("?");
    let registered = if body["created"] == true {
        format!("registered {slug}")
    } else {
        format!("{slug} already registered")
    };
    let sync = if body["fetch_queued"] == true {
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
    let body = server_json(
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
    let kind = body["kind"].as_str().unwrap_or("?");
    let org = body["org"].as_str().unwrap_or("?");
    let base_url = body["base_url"].as_str().unwrap_or("?");
    if body["created"] == true {
        println!("connected {kind} org {org} ({base_url})");
    } else {
        println!("{kind} org {org} already connected ({base_url})");
    }
    Ok(())
}

async fn admin_forge_discover(
    kind: String,
    org: String,
    base_url: Option<String>,
) -> anyhow::Result<()> {
    let body = server_json(
        reqwest::Method::POST,
        "/v1/admin/forges/discover",
        Some(serde_json::json!({
            "kind": kind,
            "org": org,
            "base_url": base_url,
        })),
    )
    .await?;
    println!(
        "discovery requested for {} org {} ({})",
        body["kind"].as_str().unwrap_or("?"),
        body["org"].as_str().unwrap_or("?"),
        body["base_url"].as_str().unwrap_or("?")
    );
    Ok(())
}

async fn admin_rules_add(
    pattern: String,
    action: RuleActionArg,
    forge: String,
    applies_to_private: bool,
) -> anyhow::Result<()> {
    let body = server_json(
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
    let scope = if body["applies_to_private"] == true {
        "private"
    } else {
        "public/internal"
    };
    println!(
        "{} {} on {} ({scope}; {} fetches queued)",
        body["action"].as_str().unwrap_or("?"),
        body["pattern"].as_str().unwrap_or("?"),
        body["forge"].as_str().unwrap_or("?"),
        body["fetches_queued"].as_u64().unwrap_or(0)
    );
    Ok(())
}

async fn admin_rules_list() -> anyhow::Result<()> {
    let body = server_json(reqwest::Method::GET, "/v1/admin/rules", None).await?;
    let rules = body["rules"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if rules.is_empty() {
        println!("no discovery rules");
        return Ok(());
    }
    for rule in rules {
        let private = if rule["applies_to_private"] == true {
            "private"
        } else {
            "public/internal"
        };
        println!(
            "{}  {}  {}  {private}",
            rule["forge"].as_str().unwrap_or("?"),
            rule["action"].as_str().unwrap_or("?"),
            rule["pattern"].as_str().unwrap_or("?")
        );
    }
    Ok(())
}

async fn admin_token_issue(member: String) -> anyhow::Result<()> {
    let body = server_json(
        reqwest::Method::POST,
        "/v1/admin/tokens",
        Some(serde_json::json!({ "member": member })),
    )
    .await?;
    println!("id: {}", body["id"].as_str().unwrap_or("?"));
    println!("member: {}", body["member"].as_str().unwrap_or("?"));
    println!("token: {}", body["token"].as_str().unwrap_or("?"));
    println!("save this token now; it will not be shown again");
    Ok(())
}

async fn admin_token_revoke(id: String) -> anyhow::Result<()> {
    if !yg_control::member_token_id_is_valid(&id) {
        bail!("member token id must look like mtok_<24 hex characters>");
    }
    let body = server_json(
        reqwest::Method::POST,
        &format!("/v1/admin/tokens/{id}/revoke"),
        None,
    )
    .await?;
    println!("revoked {}", body["id"].as_str().unwrap_or(id.as_str()));
    Ok(())
}

async fn admin_status(json: bool) -> anyhow::Result<()> {
    let body = server_json(reqwest::Method::GET, "/v1/admin/status", None).await?;

    if json {
        println!("{}", serde_json::to_string(&body)?);
        return Ok(());
    }
    print_visibility_counts(&body);
    let repos = body["repos"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if repos.is_empty() {
        println!("no repositories registered — add one with: yg admin repo add <url>");
        return Ok(());
    }
    for repo in repos {
        let slug = repo["slug"].as_str().unwrap_or("?");
        let state = repo["sync"]["state"].as_str().unwrap_or("?");
        let commit = repo["last_synced_commit"]
            .as_str()
            // .get: never split a UTF-8 boundary, however odd the server's
            // idea of a commit id.
            .map(|sha| sha.get(..12).unwrap_or(sha))
            .unwrap_or("-");
        print!("{slug}  {state}  {commit}");
        let visibility = repo["visibility"].as_str().unwrap_or("public");
        let discovery = repo["discovery_state"].as_str().unwrap_or("included");
        if visibility != "public" || discovery != "included" {
            print!("  ({visibility}, {discovery})");
        }
        if let Some(error) = repo["sync"]["last_error"].as_str() {
            print!("  [attempt {}: {error}]", repo["sync"]["attempts"]);
        }
        if let Some(revision) = repo["shard"]["revision"].as_str() {
            print!(
                "  shard {revision} ({} nodes, {} edges)",
                repo["shard"]["nodes"], repo["shard"]["edges"]
            );
        }
        if let Some(error) = repo["index"]["last_error"].as_str() {
            print!("  [index attempt {}: {error}]", repo["index"]["attempts"]);
        }
        println!();
    }
    Ok(())
}

fn print_visibility_counts(body: &serde_json::Value) {
    let counts = &body["visibility_counts"];
    println!(
        "visibility: public={} internal={} private={} unknown={}",
        counts["public"].as_u64().unwrap_or(0),
        counts["internal"].as_u64().unwrap_or(0),
        counts["private"].as_u64().unwrap_or(0),
        counts["unknown"].as_u64().unwrap_or(0),
    );
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// REST + MCP API plus Sync and indexing workers in one process
    All,
    /// API only — pair with worker processes elsewhere
    Api,
    /// Workers only: drain the fetch and index queues (no HTTP, no token)
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

    // The one read of the deployment environment: every YG_* setting
    // resolves and validates here, before anything connects.
    let deploy = deploy_config::resolve(role, |var| std::env::var(var).ok()).into_config()?;

    match role {
        Role::Api => {
            let server = yg_api::serve(server_config(&deploy)?).await?;
            println!("listening on http://{}", server.local_addr());
            server.wait().await
        }
        Role::Worker => {
            let workers = workers(&deploy).await?;
            println!("worker running");
            run_workers(&deploy, workers).await
        }
        Role::All => {
            let server = yg_api::serve(server_config(&deploy)?).await?;
            let workers = workers(&deploy).await?;
            // Announce only once the whole process is up: scripts and
            // the e2e harness treat this line as the readiness signal,
            // and a worker-boot failure after it would read as a crash
            // mid-serve instead of a boot failure.
            println!("listening on http://{}", server.local_addr());
            // Either side dying takes the process down — a half-alive
            // server that accepts repos it will never sync helps nobody.
            tokio::select! {
                result = server.wait() => result,
                result = run_workers(&deploy, workers) => result.context("worker exited"),
            }
        }
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

/// Build the worker pair. Workers need the control plane, the git
/// cache, and object storage (Shards land there) — no bootstrap token.
async fn workers(
    deploy: &deploy_config::DeployConfig,
) -> anyhow::Result<(yg_sync::SyncWorker, yg_index::IndexWorker)> {
    let control = yg_control::ControlPlane::connect_and_migrate(&deploy.database_url).await?;
    let store = deploy.object_store.connect()?;
    // Fail at boot, not on every index job: connect() never touches the
    // network, so this probe is the first thing that would notice a
    // missing or wrong YG_S3_* configuration.
    yg_api::probe_object_store(store.as_ref())
        .await
        .context("object storage unreachable at worker boot")?;
    Ok((
        yg_sync::SyncWorker::new(control.clone(), &deploy.git_cache),
        yg_index::IndexWorker::new(control, store, &deploy.git_cache),
    ))
}

/// Drain both job queues forever, each on its own loop so a slow job of
/// one kind (a cold monorepo clone, a huge syntactic pass) never stalls
/// the other queue. An error from either side ends both.
async fn run_workers(
    deploy: &deploy_config::DeployConfig,
    (sync, index): (yg_sync::SyncWorker, yg_index::IndexWorker),
) -> anyhow::Result<()> {
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
    tokio::try_join!(
        drain_queue(|| sync.discover_once(&discovery)),
        drain_queue(|| sync.run_once()),
        drain_queue(|| index.run_once()),
        drain_queue(|| sync.poll_once(&poll)),
        gc_loop(
            &index,
            deploy.gc_grace,
            deploy.job_retention,
            deploy.gc_interval
        ),
    )?;
    Ok(())
}

/// Run one queue's claim loop forever, sleeping briefly when it's empty.
/// Drives the poll loop too: `poll_once` returns `true` while repos are
/// due (claiming one per call) and `false` when none is, so the same
/// work-or-sleep shape applies.
async fn drain_queue<F, Fut>(run_once: F) -> anyhow::Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<bool>>,
{
    loop {
        if !run_once().await? {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
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
/// logged and retried next interval rather than ending the process —
/// GC is best-effort.
async fn gc_loop(
    index: &yg_index::IndexWorker,
    grace: std::time::Duration,
    job_retention: std::time::Duration,
    interval: std::time::Duration,
) -> anyhow::Result<()> {
    loop {
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
        tokio::time::sleep(interval).await;
    }
}

async fn status(json: bool) -> anyhow::Result<()> {
    let (server, _) = client_env()?;
    let (headers, mut body) = server_request(reqwest::Method::GET, "/v1/status", None).await?;
    let uptime_seconds: Option<u64> = headers
        .get(yg_api::UPTIME_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok());

    if json {
        // The server keeps volatile uptime out of the (cache-stable)
        // body; fold it back in for machine consumers, keeping the
        // body's key-sorted form.
        if let (Some(uptime), Some(map)) = (uptime_seconds, body.as_object_mut()) {
            map.insert("uptime_seconds".into(), uptime.into());
            map.sort_keys();
        }
        println!("{}", serde_json::to_string(&body)?);
    } else {
        println!("yggdrasil Index Server at {server}");
        println!("version:       {}", body["version"].as_str().unwrap_or("?"));
        match uptime_seconds {
            Some(uptime) => println!("uptime:        {uptime}s"),
            None => println!("uptime:        ?"),
        }
        println!("repos indexed: {}", body["repos_indexed"]);
    }
    Ok(())
}
