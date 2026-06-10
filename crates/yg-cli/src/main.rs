//! yg binary: subcommands, serve roles, MCP proxy.

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "yg", version, about = "yggdrasil — Knowledge Graph Index Server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show Index Server version, uptime, and indexed-repo count
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Status => status().await,
    }
}

async fn status() -> anyhow::Result<()> {
    let server = std::env::var("YG_SERVER").unwrap_or_else(|_| "http://127.0.0.1:7311".into());
    let token = std::env::var("YG_TOKEN")
        .context("YG_TOKEN must be set (the bootstrap Admin token for now)")?;

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

    println!("yggdrasil Index Server at {server}");
    println!("version:       {}", body["version"].as_str().unwrap_or("?"));
    println!("uptime:        {}s", body["uptime_seconds"]);
    println!("repos indexed: {}", body["repos_indexed"]);
    Ok(())
}
