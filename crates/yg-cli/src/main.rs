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
    /// Boot the Index Server
    Serve {
        /// Which roles this process runs (only "all" in this milestone)
        #[arg(long, default_value = "all")]
        role: String,
    },
    /// Show Index Server version, uptime, and indexed-repo count
    Status {
        /// Emit the raw JSON response instead of the human report
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve { role } => serve(role).await,
        Command::Status { json } => status(json).await,
    }
}

async fn serve(role: String) -> anyhow::Result<()> {
    if role != "all" {
        bail!("only --role=all is supported in this milestone (got {role:?})");
    }
    // Logs go to stderr; stdout carries only the address announcement so
    // scripts (and the e2e tests) can parse it.
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let server = yg_api::serve(yg_api::ServerConfig::from_env()?).await?;
    println!("listening on http://{}", server.local_addr());
    server.wait().await
}

async fn status(json: bool) -> anyhow::Result<()> {
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
