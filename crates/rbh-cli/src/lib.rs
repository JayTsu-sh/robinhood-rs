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

    /// Inspect and manage entries in the `removed_entries` table.
    #[command(subcommand)]
    Undelete(UndeleteCmd),

    /// Show differences between a Lustre mount and the catalog.
    Diff {
        /// Lustre mount point to walk.
        #[arg(long, default_value = "/lustre")]
        mount: String,
        /// Max entries to show per category.
        #[arg(long, default_value = "50")]
        limit: usize,
    },
}

#[derive(Subcommand)]
pub enum UndeleteCmd {
    /// List entries in `removed_entries`, newest first.
    List {
        #[arg(long, default_value = "50")]
        n: u64,
        /// Only entries removed at or after this unix timestamp.
        #[arg(long)]
        since: Option<i64>,
    },
    /// Drop a removed-entry row from the catalog (after operator confirms
    /// the file has been recovered externally, or is truly unwanted).
    Forget {
        /// FID of the entry to forget, e.g. `[0x200000401:0x42:0x0]`.
        fid: String,
    },
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
    /// File-size distribution (files only).
    SizeProfile,
    /// Dump all entries matching a simple filter (user/group/ost).
    Dump {
        #[arg(long)]
        user: Option<u32>,
        #[arg(long)]
        group: Option<u32>,
        #[arg(long)]
        ost: Option<u32>,
        #[arg(long, default_value = "1000")]
        limit: u64,
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
        Command::Undelete(cmd) => run_undelete(&client, &cli.api_url, cmd).await?,
        Command::Diff { mount, limit } => run_diff(&client, &cli.api_url, &mount, limit).await?,
    }

    Ok(())
}

async fn run_undelete(
    client: &reqwest::Client,
    api_url: &str,
    cmd: UndeleteCmd,
) -> Result<()> {
    match cmd {
        UndeleteCmd::List { n, since } => {
            let mut url = format!("{api_url}/api/removed?limit={n}");
            if let Some(s) = since {
                url.push_str(&format!("&since={s}"));
            }
            let v = fetch_json(client, &url).await?;
            let arr = v.as_array().cloned().unwrap_or_default();
            println!(
                "{:<14} {:>12} {:>10} {:<8} {}",
                "rm_time", "size", "uid", "kind", "name  fid"
            );
            for e in arr {
                let rm = e.get("rm_time").and_then(|v| v.as_i64()).unwrap_or(0);
                let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let uid = e.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
                let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
                let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let fid = e
                    .get("fid")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into());
                println!(
                    "{rm:<14} {size:>12} {uid:>10} {kind:<8} {name}  fid={fid}"
                );
            }
        }
        UndeleteCmd::Forget { fid } => {
            let resp = client
                .delete(format!("{api_url}/api/removed/{fid}"))
                .send()
                .await
                .context("request failed")?;
            match resp.status().as_u16() {
                204 => println!("forgotten: {fid}"),
                404 => anyhow::bail!("not in removed_entries: {fid}"),
                s => {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    anyhow::bail!("server {s}: {body}");
                }
            }
        }
    }
    Ok(())
}

async fn run_diff(
    client: &reqwest::Client,
    api_url: &str,
    mount: &str,
    limit: usize,
) -> Result<()> {
    // Page through the catalog to collect name -> fid (name-only match
    // is coarse; full parent+name join would be accurate but this stays
    // client-side for simplicity).
    use std::collections::HashSet;
    let mut in_catalog: HashSet<String> = HashSet::new();
    let mut offset: u64 = 0;
    let page_size: u64 = 5000;
    loop {
        let body = serde_json::json!({
            "predicate": {"op": "true"},
            "limit": page_size,
            "offset": offset,
        });
        let v = post_json(client, &format!("{api_url}/api/entries/query"), &body).await?;
        let entries = v.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
        let n = entries.len() as u64;
        for e in entries {
            if let Some(n) = e.get("name").and_then(|x| x.as_str()) {
                in_catalog.insert(n.to_string());
            }
        }
        offset += n;
        if n < page_size {
            break;
        }
    }

    // Walk FS (shallow — don't hammer Lustre; a real diff would reuse
    // FsScanner async). Use blocking walkdir in a spawn_blocking.
    let mount_path = std::path::PathBuf::from(mount);
    let walked: Vec<String> = tokio::task::spawn_blocking(move || {
        let mut names = Vec::new();
        for entry in walkdir::WalkDir::new(&mount_path)
            .max_depth(5)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if let Some(n) = entry.file_name().to_str() {
                names.push(n.to_string());
            }
        }
        names
    })
    .await
    .context("walker join error")?;

    let fs_set: std::collections::HashSet<String> = walked.into_iter().collect();
    let only_fs: Vec<_> = fs_set.difference(&in_catalog).take(limit).collect();
    let only_db: Vec<_> = in_catalog.difference(&fs_set).take(limit).collect();

    println!("-- only on filesystem ({}): --", only_fs.len());
    for n in &only_fs {
        println!("  + {n}");
    }
    println!("-- only in catalog ({}): --", only_db.len());
    for n in &only_db {
        println!("  - {n}");
    }
    println!(
        "-- summary: catalog={}  fs(walked)={}  lonely_fs={}  lonely_db={} --",
        in_catalog.len(),
        fs_set.len(),
        only_fs.len(),
        only_db.len()
    );
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
        ReportCmd::SizeProfile => {
            let v = fetch_json(client, &format!("{api_url}/api/reports/size-profile")).await?;
            let arr = v.as_array().cloned().unwrap_or_default();
            println!("{:<12} {:>10} {:>18}", "bucket", "count", "total_size");
            for row in arr {
                let b = row.get("bucket").and_then(|v| v.as_str()).unwrap_or("?");
                let c = row.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                let s = row.get("total_size").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("{b:<12} {c:>10} {s:>18}");
            }
        }
        ReportCmd::Dump { user, group, ost, limit } => {
            let mut children: Vec<serde_json::Value> = Vec::new();
            if let Some(u) = user {
                children.push(serde_json::json!({
                    "op": "cmp", "field": "uid", "cmp": "eq", "value": u,
                }));
            }
            if let Some(g) = group {
                children.push(serde_json::json!({
                    "op": "cmp", "field": "gid", "cmp": "eq", "value": g,
                }));
            }
            if let Some(o) = ost {
                children.push(serde_json::json!({
                    "op": "on_ost", "osts": [o],
                }));
            }
            let predicate = match children.len() {
                0 => serde_json::json!({"op": "true"}),
                1 => children.into_iter().next().unwrap(),
                _ => serde_json::json!({"op": "and", "children": children}),
            };
            let body = serde_json::json!({
                "predicate": predicate,
                "limit": limit,
            });
            let v = post_json(client, &format!("{api_url}/api/entries/query"), &body).await?;
            let entries = v.get("entries").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            for e in entries {
                let size = e.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                let uid = e.get("uid").and_then(|v| v.as_u64()).unwrap_or(0);
                let name = e.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let fid = e.get("fid").map(|v| v.to_string()).unwrap_or_else(|| "?".into());
                println!("{size:>14} uid={uid:<6} {name}  fid={fid}");
            }
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
        let raw = row.get("key").and_then(|v| v.as_str()).unwrap_or("?");
        let k = if key_label == "kind" {
            kind_code_to_label(raw)
        } else {
            raw.to_string()
        };
        let c = row.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        let s = row.get("total_size").and_then(|v| v.as_u64()).unwrap_or(0);
        println!("{k:<16}  {c:>10}  {s:>18}");
    }
}

fn kind_code_to_label(code: &str) -> String {
    match code {
        "0" => "file".into(),
        "1" => "dir".into(),
        "2" => "symlink".into(),
        "3" => "chardev".into(),
        "4" => "blockdev".into(),
        "5" => "fifo".into(),
        "6" => "socket".into(),
        other => other.to_string(),
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
