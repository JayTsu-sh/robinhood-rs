//! Root binary shim — dispatches to the `rbh-daemon` library.
//!
//! `cargo run` at the workspace root launches the robinhood-rs daemon. The CLI
//! is reached via `cargo run -p rbh-cli --bin rbh`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "heat-agent") {
        rbh_daemon::run_heat_agent().await?;
        return Ok(());
    }
    rbh_daemon::run().await
}
