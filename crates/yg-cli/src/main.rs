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
    /// Show every registered repo with its sync state
    Status {
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
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
        Command::Admin { command } => match command {
            AdminCommand::Repo {
                command: RepoCommand::Add { url, depth },
            } => admin_repo_add(url, depth).await,
            AdminCommand::Status { json } => admin_status(json).await,
        },
    }
}

/// Where the Index Server lives and how to authenticate, from the same
/// env every client command reads.
fn client_env() -> anyhow::Result<(String, String)> {
    let server = std::env::var("YG_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7311".into());
    let server = server.trim_end_matches('/').to_string();
    let token = std::env::var("YG_TOKEN")
        .context("YG_TOKEN must be set (the bootstrap Admin token for now)")?;
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

async fn admin_repo_add(url: String, depth: Option<i32>) -> anyhow::Result<()> {
    let body = server_json(
        reqwest::Method::POST,
        "/v1/admin/repos",
        Some(serde_json::json!({"url": url, "depth": depth})),
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
    tokio::try_join!(
        drain_queue(|| sync.run_once()),
        drain_queue(|| index.run_once()),
    )?;
    Ok(())
}

/// Run one queue's claim loop forever, sleeping briefly when it's empty.
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
