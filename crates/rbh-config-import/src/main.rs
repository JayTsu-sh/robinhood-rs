//! `rbh-config-import` CLI — read a robinhood-C `.conf`, print or POST
//! the translated `PolicyDef` JSON.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "rbh-config-import",
    about = "Convert robinhood-C *.conf files to rbh-daemon PolicyDef JSON."
)]
struct Cli {
    /// Path to the robinhood-C config file.
    config: PathBuf,

    /// POST each policy to the given daemon base URL (/api/policies) on
    /// success. Without this flag, just print the JSON to stdout.
    #[arg(long, env = "RBH_API_URL")]
    post: Option<String>,

    /// Pretty-print the JSON.
    #[arg(long)]
    pretty: bool,

    /// Treat warnings as errors.
    #[arg(long)]
    strict: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let src = std::fs::read_to_string(&cli.config)
        .with_context(|| format!("reading {}", cli.config.display()))?;
    let result = rbh_config_import::import(&src)?;

    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    if cli.strict && !result.warnings.is_empty() {
        anyhow::bail!("{} warning(s) — aborting because --strict", result.warnings.len());
    }

    if result.policies.is_empty() {
        anyhow::bail!("no policies extracted from {}", cli.config.display());
    }

    for pol in &result.policies {
        let j = if cli.pretty {
            serde_json::to_string_pretty(pol)?
        } else {
            serde_json::to_string(pol)?
        };
        match &cli.post {
            None => println!("{j}"),
            Some(base) => {
                let url = format!("{base}/api/policies");
                let client = reqwest::Client::new();
                let resp = client
                    .post(&url)
                    .header("content-type", "application/json")
                    .body(j.clone())
                    .send()
                    .await
                    .with_context(|| format!("POST {url}"))?;
                let status = resp.status();
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                eprintln!("{} -> {status} {body}", pol.name);
                if !status.is_success() {
                    anyhow::bail!("server rejected policy {}", pol.name);
                }
            }
        }
    }

    Ok(())
}
