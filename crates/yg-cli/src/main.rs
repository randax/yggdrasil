//! yg binary: subcommands, serve roles, MCP proxy.

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
    /// Administer the Index Server (Admin token required)
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
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

/// Where the Index Server lives and how to authenticate, from the same
/// env every client command reads.
fn client_env() -> anyhow::Result<(String, String)> {
    let server = std::env::var("YG_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7311".into());
    let server = server.trim_end_matches('/').to_string();
    let token = std::env::var("YG_TOKEN").context("YG_TOKEN must be set")?;
    Ok((server, token))
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
    serde_json::from_str(&text).with_context(|| format!("parsing the response from {path}"))
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

async fn node(id: String, json: bool) -> anyhow::Result<()> {
    let body = post_verb("node", serde_json::json!({"id": id})).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
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
    let mut req = serde_json::json!({"id": id});
    if let Some(direction) = direction {
        req["direction"] = direction.into();
    }
    if !kinds.is_empty() {
        req["edge_kinds"] = kinds.into();
    }
    if let Some(depth) = depth {
        req["depth"] = depth.into();
    }
    if let Some(limit) = limit {
        req["limit"] = limit.into();
    }
    if let Some(cursor) = cursor {
        req["cursor"] = cursor.into();
    }
    let body = post_verb("neighbors", req).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
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
    let mut req = serde_json::json!({"query": query});
    if !kinds.is_empty() {
        req["kinds"] = kinds.into();
    }
    if !repos.is_empty() {
        req["repos"] = repos.into();
    }
    if let Some(limit) = limit {
        req["limit"] = limit.into();
    }
    if let Some(cursor) = cursor {
        req["cursor"] = cursor.into();
    }
    let body = post_verb("search", req).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
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
    let mut req = serde_json::json!({ "id": id });
    if let Some(since) = since {
        req["since"] = since.into();
    }
    if let Some(limit) = limit {
        req["limit"] = limit.into();
    }
    if let Some(cursor) = cursor {
        req["cursor"] = cursor.into();
    }
    let body = post_verb("history", req).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
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
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
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

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// REST + MCP API plus Sync and indexing workers in one process
    All,
    /// API only — pair with worker processes elsewhere
    Api,
    /// Workers only: drain the fetch and index queues (no HTTP, no token)
    Worker,
}

async fn serve(role: Role) -> anyhow::Result<()> {
    // Logs go to stderr; stdout carries only the address announcement so
    // scripts (and the e2e tests) can parse it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    match role {
        Role::Api => {
            let server = yg_api::serve(yg_api::ServerConfig::from_env()?).await?;
            println!("listening on http://{}", server.local_addr());
            server.wait().await
        }
        Role::Worker => {
            let workers = workers_from_env().await?;
            println!("worker running");
            run_workers(workers).await
        }
        Role::All => {
            let server = yg_api::serve(yg_api::ServerConfig::from_env()?).await?;
            let workers = workers_from_env().await?;
            // Announce only once the whole process is up: scripts and
            // the e2e harness treat this line as the readiness signal,
            // and a worker-boot failure after it would read as a crash
            // mid-serve instead of a boot failure.
            println!("listening on http://{}", server.local_addr());
            // Either side dying takes the process down — a half-alive
            // server that accepts repos it will never sync helps nobody.
            tokio::select! {
                result = server.wait() => result,
                result = run_workers(workers) => result.context("worker exited"),
            }
        }
    }
}

/// Build the worker pair from the `YG_*` environment. Workers need the
/// control plane, the git cache, and object storage (Shards land there)
/// — no bootstrap token.
async fn workers_from_env() -> anyhow::Result<(yg_sync::SyncWorker, yg_index::IndexWorker)> {
    let database_url = std::env::var("YG_DATABASE_URL")
        .unwrap_or_else(|_| yg_control::DEFAULT_DATABASE_URL.to_string());
    let git_cache = std::env::var("YG_GIT_CACHE").unwrap_or_else(|_| "./data/git".to_string());
    let control = yg_control::ControlPlane::connect_and_migrate(&database_url).await?;
    let store = yg_api::ObjectStoreConfig::from_env().connect()?;
    // Fail at boot, not on every index job: connect() never touches the
    // network, so this probe is the first thing that would notice a
    // missing or wrong YG_S3_* configuration.
    yg_api::probe_object_store(store.as_ref())
        .await
        .context("object storage unreachable at worker boot")?;
    Ok((
        yg_sync::SyncWorker::new(control.clone(), &git_cache),
        yg_index::IndexWorker::new(control, store, &git_cache),
    ))
}

/// Drain both job queues forever, each on its own loop so a slow job of
/// one kind (a cold monorepo clone, a huge syntactic pass) never stalls
/// the other queue. An error from either side ends both.
async fn run_workers(
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
    let poll = poll_config_from_env();
    let discovery = discovery_config_from_env();
    let gc_grace = env_duration_secs("YG_GC_GRACE", DEFAULT_GC_GRACE_SECS);
    let gc_interval = env_duration_secs("YG_GC_INTERVAL", DEFAULT_GC_INTERVAL_SECS);
    tokio::try_join!(
        drain_queue(|| sync.discover_once(&discovery)),
        drain_queue(|| sync.run_once()),
        drain_queue(|| index.run_once()),
        drain_queue(|| sync.poll_once(&poll)),
        gc_loop(&index, gc_grace, gc_interval),
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

/// Default poll interval per repo when `YG_POLL_INTERVAL` is unset.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5 * 60;
/// Default connected-forge discovery interval (`YG_DISCOVERY_INTERVAL`).
const DEFAULT_DISCOVERY_INTERVAL_SECS: u64 = 60 * 60;
/// Default grace window before a superseded Shard is reclaimed
/// (`YG_GC_GRACE`).
const DEFAULT_GC_GRACE_SECS: u64 = 60 * 60;
/// Default cadence of the GC sweep (`YG_GC_INTERVAL`).
const DEFAULT_GC_INTERVAL_SECS: u64 = 10 * 60;
/// Fraction of the interval added as random jitter so a forge's repos
/// don't poll in lockstep.
const POLL_JITTER_FRACTION: f64 = 0.2;
/// Ceiling on an env-configured duration. A poll/GC cadence is fed to
/// Postgres `make_interval`, which errors (out of range) on absurd
/// values — and that error propagates out of the poll loop and kills the
/// worker. Clamp to ten years: far beyond any sane cadence, far inside
/// `make_interval`'s range, so a typo degrades to "very slow" not "crash".
const MAX_DURATION_SECS: u64 = 10 * 365 * 24 * 3600;

fn poll_config_from_env() -> yg_sync::PollConfig {
    yg_sync::PollConfig {
        default_interval: env_duration_secs("YG_POLL_INTERVAL", DEFAULT_POLL_INTERVAL_SECS),
        jitter_fraction: POLL_JITTER_FRACTION,
    }
}

fn discovery_config_from_env() -> yg_sync::DiscoveryConfig {
    yg_sync::DiscoveryConfig {
        interval: env_duration_secs("YG_DISCOVERY_INTERVAL", DEFAULT_DISCOVERY_INTERVAL_SECS),
    }
}

/// A positive-integer-seconds duration from `var`, falling back to
/// `default_secs` when the variable is unset or not a positive integer.
fn env_duration_secs(var: &str, default_secs: u64) -> std::time::Duration {
    let secs = match std::env::var(var) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(secs) if secs >= 1 => secs.min(MAX_DURATION_SECS),
            _ => {
                tracing::warn!(
                    var,
                    value,
                    "ignoring {var} (want a positive integer of seconds); using the default"
                );
                default_secs
            }
        },
        Err(_) => default_secs,
    };
    std::time::Duration::from_secs(secs)
}

/// Reclaim superseded Shards on a fixed cadence. Unlike a queue drain
/// there is no per-item "work or not": the sweep runs every `interval`,
/// reclaiming whatever has aged past `grace`. A sweep that fails (a
/// control-plane or object-storage blip) is logged and retried next
/// interval rather than ending the process — GC is best-effort.
async fn gc_loop(
    index: &yg_index::IndexWorker,
    grace: std::time::Duration,
    interval: std::time::Duration,
) -> anyhow::Result<()> {
    loop {
        if let Err(e) = index.gc_once(grace).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "shard GC sweep failed; retrying next interval"
            );
        }
        tokio::time::sleep(interval).await;
    }
}

async fn status(json: bool) -> anyhow::Result<()> {
    let (server, _) = client_env()?;
    let body = server_json(reqwest::Method::GET, "/v1/status", None).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        println!("yggdrasil Index Server at {server}");
        println!("version:       {}", body["version"].as_str().unwrap_or("?"));
        println!("uptime:        {}s", body["uptime_seconds"]);
        println!("repos indexed: {}", body["repos_indexed"]);
    }
    Ok(())
}
