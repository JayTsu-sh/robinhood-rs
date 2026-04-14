//! `rbh` command-line client — library half.
//!
//! Subcommands communicate with the `rbh-daemon` REST API via HTTP.
//! No direct database access — all operations go through `/api/`.

use clap::{Parser, Subcommand};

/// robinhood-rs command-line client.
#[derive(Parser)]
#[command(name = "rbh", version, about = "robinhood-rs CLI")]
pub struct Cli {
    /// Daemon API base URL.
    #[arg(long, env = "RBH_API_URL", default_value = "http://127.0.0.1:8080")]
    pub api_url: String,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List all policies.
    #[command(name = "policy-list")]
    PolicyList,

    /// Show a policy by ID.
    #[command(name = "policy-show")]
    PolicyShow {
        /// Policy ID.
        id: u64,
    },

    /// Show entry catalog count.
    Status,

    /// Check daemon health.
    Health,
}

/// Run the CLI.
pub async fn run() -> anyhow::Result<()> {
    let _guard = rbh_observability::init(rbh_observability::ObservabilityConfig {
        service_name: "rbh-cli",
        format: rbh_observability::LogFormat::Pretty,
        ..Default::default()
    })?;

    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Command::PolicyList => {
            let resp = client.get(format!("{}/api/policies", cli.api_url)).send().await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Command::PolicyShow { id } => {
            let resp = client
                .get(format!("{}/api/policies/{}", cli.api_url, id))
                .send()
                .await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Command::Status => {
            let resp = client.get(format!("{}/api/entries/count", cli.api_url)).send().await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
        Command::Health => {
            let resp = client.get(format!("{}/api/health", cli.api_url)).send().await?;
            let body: serde_json::Value = resp.json().await?;
            println!("{}", serde_json::to_string_pretty(&body)?);
        }
    }

    Ok(())
}
