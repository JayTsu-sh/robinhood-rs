//! Root binary shim — dispatches to the `rbh-daemon` library.
//!
//! `cargo run` at the workspace root launches the robinhood-rs daemon. The CLI
//! is reached via `cargo run -p rbh-cli --bin rbh`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rbh_daemon::run().await
}
