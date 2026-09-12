//! Local Hub process for development.
//!
//! The production binary is `remuda hub` in `crates/remuda`. This example exists
//! so the Hub crate can be run before that subcommand is wired.

use anyhow::Context;
use remuda_hub::{HubConfig, serve};
use std::net::SocketAddr;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("remuda_hub=info".parse()?),
        )
        .init();

    let mut config = HubConfig::default();
    if let Ok(dir) = std::env::var("REMUDA_DATA_DIR") {
        config.data_dir = PathBuf::from(dir);
    }
    if let Ok(listen) = std::env::var("REMUDA_LISTEN") {
        config.listen = listen.parse::<SocketAddr>().context("REMUDA_LISTEN")?;
    }
    if let Ok(token) = std::env::var("REMUDA_BOOTSTRAP_TOKEN") {
        config.bootstrap_token = token;
    }
    if let Ok(value) = std::env::var("REMUDA_COOKIE_SECURE") {
        config.cookie_secure = value != "0" && value != "false";
    }
    if let Ok(root) = std::env::var("REMUDA_WEB_ROOT") {
        config.web_root = Some(PathBuf::from(root));
    }
    serve(config).await
}
