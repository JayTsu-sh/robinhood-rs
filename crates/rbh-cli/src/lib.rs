//! `rbh` command-line client — library half.
//!
//! Subcommands communicate with the `rbh-daemon` REST API via HTTP.
//! No direct database access — all operations go through `/api/`.

mod find;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

pub use find::{FindArgs, build_query};

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

    /// Query the entry catalog with find(1)-style filters.
    Find(FindArgs),

    /// Aggregate / summary reports.
    #[command(subcommand)]
    Report(ReportCmd),
}

#[derive(Subcommand)]
pub enum ReportCmd {
    /// Top-N biggest files.
    TopSize {
        #[arg(long, default_value = "20")]
        n: u64,
    },
    /// Top-N users by total size (or --by=count).
    TopUsers {
        #[arg(long, default_value = "20")]
        n: u64,
        #[arg(long, value_enum, default_value = "size")]
        by: ReportBy,
    },
    /// Top-N groups by total size.
    TopGroups {
        #[arg(long, default_value = "20")]
        n: u64,
        #[arg(long, value_enum, default_value = "size")]
        by: ReportBy,
    },
    /// Per-entry-kind summary (file/dir/symlink ...).
    FsInfo,
    /// Oldest N entries by atime.
    Oldest {
        #[arg(long, default_value = "20")]
        n: u64,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ReportBy {
    Count,
    Size,
}

/// Run the CLI.
pub async fn run() -> Result<()> {
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
        Command::Find(args) => run_find(&client, &cli.api_url, args).await?,
        Command::Report(cmd) => run_report(&client, &cli.api_url, cmd).await?,
    }

    Ok(())
}

async fn run_report(client: &reqwest::Client, api_url: &str, cmd: ReportCmd) -> Result<()> {
    match cmd {
        ReportCmd::TopSize { n } => {
            let v = fetch_json(client, &format!("{api_url}/api/reports/top-size?n={n}")).await?;
            print_entries_table(&v);
        }
        ReportCmd::Oldest { n } => {
            let v = fetch_json(client, &format!("{api_url}/api/reports/oldest?n={n}")).await?;
            print_entries_table(&v);
        }
        ReportCmd::TopUsers { n, by } => {
            let body = agg_body("uid", by, n);
            let v = post_json(client, &format!("{api_url}/api/reports/aggregate"), &body).await?;
            print_aggregate_table(&v, "uid");
        }
        ReportCmd::TopGroups { n, by } => {
            let body = agg_body("gid", by, n);
            let v = post_json(client, &format!("{api_url}/api/reports/aggregate"), &body).await?;
            print_aggregate_table(&v, "gid");
        }
        ReportCmd::FsInfo => {
            let body = agg_body("kind", ReportBy::Count, 20);
            let v = post_json(client, &format!("{api_url}/api/reports/aggregate"), &body).await?;
            print_aggregate_table(&v, "kind");
        }
    }
    Ok(())
}

fn agg_body(key: &str, by: ReportBy, n: u64) -> serde_json::Value {
    let sort = match by {
        ReportBy::Count => "count",
        ReportBy::Size => "size",
    };
    serde_json::json!({ "key": key, "sort": sort, "limit": n })
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let resp = client.get(url).send().await.context("request failed")?;
    let status = resp.status();
    let value: serde_json::Value = resp.json().await.context("response is not JSON")?;
    if !status.is_success() {
        anyhow::bail!("server {status}: {value}");
    }
    Ok(value)
}

async fn post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .await
        .context("request failed")?;
    let status = resp.status();
    let value: serde_json::Value = resp.json().await.context("response is not JSON")?;
    if !status.is_success() {
        anyhow::bail!("server {status}: {value}");
    }
    Ok(value)
}

fn print_entries_table(v: &serde_json::Value) {
    let arr = v.as_array().cloned().unwrap_or_default();
    for e in arr {
        let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let uid = e.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
        let atime = e.get("atime").and_then(|v| v.as_i64()).unwrap_or(0);
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        println!("{size:>14}  uid={uid:<6}  atime={atime:<12}  {name}");
    }
}

fn print_aggregate_table(v: &serde_json::Value, key_label: &str) {
    let arr = v.as_array().cloned().unwrap_or_default();
    println!("{:<16}  {:>10}  {:>18}", key_label, "count", "total_size");
    for row in arr {
        let k = row.get("key").and_then(|v| v.as_str()).unwrap_or("?");
        let c = row.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let s = row.get("total_size").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("{k:<16}  {c:>10}  {s:>18}");
    }
}

async fn run_find(client: &reqwest::Client, api_url: &str, args: FindArgs) -> Result<()> {
    let json_output = args.json;
    let (predicate, order_by) = build_query(&args, find::now_secs())?;

    let body = serde_json::json!({
        "predicate": predicate,
        "order_by": order_by,
        "limit": args.limit,
        "offset": args.offset,
    });

    let resp = client
        .post(format!("{api_url}/api/entries/query"))
        .json(&body)
        .send()
        .await
        .context("request failed")?;
    let status = resp.status();
    let value: serde_json::Value = resp
        .json()
        .await
        .with_context(|| format!("server returned {status}; body was not JSON"))?;
    if !status.is_success() {
        anyhow::bail!("server error ({status}): {value}");
    }

    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let entries = value
        .get("entries")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    for e in entries {
        let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
        let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let uid = e.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
        let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let fid = e
            .get("fid")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into());
        println!("{kind:8} {size:>12} uid={uid:<6} {name}  fid={fid}");
    }
    Ok(())
}
