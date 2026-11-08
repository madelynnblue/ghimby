mod app;
mod changes;
mod checkout;
mod codeowners;
mod glob;
mod item;
mod prpage;
mod rules;
mod source;
mod store;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Filter GitHub notifications with rules stored in SQLite")]
struct Args {
    /// SQLite file holding config and caches; created if missing.
    #[arg(long, default_value = "ghimby.db")]
    db: PathBuf,
    /// Address to serve the web UI on; must be loopback, since there is no login.
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    use std::io::IsTerminal;
    // No color codes when launchd sends output to a log file.
    tracing_subscriber::fmt().with_ansi(std::io::stderr().is_terminal()).init();
    let args = Args::parse();
    let addr: std::net::SocketAddr = args.listen.parse().context("--listen must be ip:port")?;
    // There's no login, and the app can run commands and approve PRs as you.
    anyhow::ensure!(addr.ip().is_loopback(), "--listen must be a loopback address such as 127.0.0.1");
    let store = store::Store::open(&args.db)?;
    let github = Arc::new(source::github::GitHub::new(store.clone()));
    let changes = changes::Changes::default();
    let app = Arc::new(app::App {
        sources: vec![github.clone()],
        github,
        checkouts: checkout::Checkouts::new(changes.clone()),
        changes,
        store,
        items: Default::default(),
        status: Default::default(),
        kick: Default::default(),
    });
    app.rebuild_items()?;
    tokio::spawn(app.clone().sync_loop());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{}", args.listen);
    axum::serve(listener, web::router(app, addr.port())).await?;
    Ok(())
}
