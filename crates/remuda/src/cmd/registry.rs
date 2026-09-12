//! Shared CLI context and service lifetime; no feature-specific dispatch.

use std::{future::Future, path::PathBuf};

use anyhow::{Context as _, Result, ensure};

use crate::{Shutdown, config::Config};

#[derive(clap::Args, Default)]
pub(crate) struct Context {
    /// TOML configuration; defaults to REMUDA_CONFIG or ./remuda.toml when present.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Override the configured data directory.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
}

impl Context {
    pub fn load_config(&self) -> Result<Config> {
        let mut config = Config::load(self.config.as_deref())?;
        if let Some(path) = &self.data_dir {
            ensure!(!path.as_os_str().is_empty(), "--data-dir must not be empty");
            config.data_dir = path.clone();
        }
        Ok(config)
    }
}

pub(crate) trait Entrypoint {
    fn enter(self, context: Context) -> Result<i32>;
    fn tracing(&self) -> bool {
        true
    }
}

pub(super) fn service<F: Future<Output = Result<()>>>(
    context: Context,
    start: impl FnOnce(Config, Shutdown) -> F,
) -> Result<i32> {
    let config = context.load_config()?;
    let timeout = config.shutdown_timeout();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot create service runtime")?;
    let result = runtime.block_on(async move { start(config, Shutdown::install()?).await });
    // Tokio stdin cannot be cancelled on signal. Drivers already drained;
    // preserve bounded teardown for every service command.
    runtime.shutdown_timeout(timeout);
    result.map(|()| 0)
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
