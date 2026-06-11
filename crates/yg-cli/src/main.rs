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

async fn admin_repo_add(url: String, depth: Option<i32>) -> anyhow::Result<()> {
    let (server, token) = client_env()?;
    let resp = reqwest::Client::new()
        .post(format!("{server}/v1/admin/repos"))
        .bearer_auth(&token)
        .json(&serde_json::json!({"url": url, "depth": depth}))
        .send()
        .await
        .with_context(|| format!("requesting {server}/v1/admin/repos"))?;
    let status = resp.status();
    let text = resp.text().await.context("reading add response")?;
    if !status.is_success() {
        // Prefer the server's {"error": …} shape, but a proxy or crash
        // can answer with anything — show whatever came back.
        let reason = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|body| body["error"].as_str().map(str::to_string))
            .unwrap_or(text);
        bail!("server rejected the repository ({status}): {reason}");
    }
    let body: serde_json::Value = serde_json::from_str(&text).context("parsing add response")?;
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
    let (server, token) = client_env()?;
    let resp = reqwest::Client::new()
        .get(format!("{server}/v1/admin/status"))
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("requesting {server}/v1/admin/status"))?;
    if !resp.status().is_success() {
        bail!(
            "server returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    let body: serde_json::Value = resp.json().await.context("parsing admin status")?;

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
        println!();
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Role {
    /// REST + MCP API and a Sync worker in one process
    All,
    /// API only — pair with worker processes elsewhere
    Api,
    /// Sync worker only: drains the fetch queue (no HTTP, no token)
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
            let worker = worker_from_env().await?;
            println!("worker running");
            worker.run().await
        }
        Role::All => {
            let server = yg_api::serve(yg_api::ServerConfig::from_env()?).await?;
            println!("listening on http://{}", server.local_addr());
            let worker = worker_from_env().await?;
            // Either side dying takes the process down — a half-alive
            // server that accepts repos it will never sync helps nobody.
            tokio::select! {
                result = server.wait() => result,
                result = worker.run() => result.context("worker exited"),
            }
        }
    }
}

/// Build a Sync worker from the `YG_*` environment. Workers need the
/// control plane and a git cache directory — no bootstrap token.
async fn worker_from_env() -> anyhow::Result<yg_sync::SyncWorker> {
    let database_url = std::env::var("YG_DATABASE_URL")
        .unwrap_or_else(|_| yg_control::DEFAULT_DATABASE_URL.to_string());
    let git_cache = std::env::var("YG_GIT_CACHE").unwrap_or_else(|_| "./data/git".to_string());
    let control = yg_control::ControlPlane::connect_and_migrate(&database_url).await?;
    Ok(yg_sync::SyncWorker::new(control, git_cache))
}

async fn status(json: bool) -> anyhow::Result<()> {
    let (server, token) = client_env()?;
    let resp = reqwest::Client::new()
        .get(format!("{server}/v1/status"))
        .bearer_auth(&token)
        .send()
        .await
        .with_context(|| format!("requesting {server}/v1/status"))?;
    if !resp.status().is_success() {
        bail!(
            "server returned {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    let body: serde_json::Value = resp.json().await.context("parsing status response")?;

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
