//! Local Hub process for development.
//!
//! The production binary is `remuda hub` in `crates/remuda`. This example exists
//! so the Hub crate can be run before that subcommand is wired.

use anyhow::Context;
use remuda_hub::{BootstrapSource, HubConfig, serve};
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
    match std::env::var("REMUDA_BOOTSTRAP_TOKEN") {
        // Operator-supplied provenance: the Hub must never mint over this
        // code. Trim the way SecretRef::resolve does so a trailing newline or
        // spaces from `$'secret\n'` cannot re-stamp the code on every restart.
        Ok(token) if !token.trim().is_empty() => {
            config.bootstrap_token = token.trim().to_owned();
            config.bootstrap_source = BootstrapSource::ExplicitEnv;
        }
        // An empty code would accept empty-string logins and overwrite a
        // previously persisted real code; refuse startup instead.
        Ok(_) => anyhow::bail!("REMUDA_BOOTSTRAP_TOKEN must not be empty"),
        Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            anyhow::bail!("REMUDA_BOOTSTRAP_TOKEN is not valid UTF-8")
        }
    }
    if let Ok(value) = std::env::var("REMUDA_COOKIE_SECURE") {
        config.cookie_secure = value != "0" && value != "false";
    }
    if let Ok(root) = std::env::var("REMUDA_WEB_ROOT") {
        config.web_root = Some(PathBuf::from(root));
    }
    serve(config).await
}
