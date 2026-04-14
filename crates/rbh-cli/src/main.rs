//! `rbh` binary entry point — thin wrapper over `rbh_cli::run()`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rbh_cli::run().await
}
