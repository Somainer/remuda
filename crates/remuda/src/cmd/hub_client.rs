//! Thin CLI wrapper around [`remuda_hub_client`].
//!
//! Shared `--hub` / `--token` flags stay here because they are clap `Args`.
//! HTTP/WS talk lives in `crates/remuda-hub-client`.

use anyhow::Context;
use clap::Args;
use serde_json::Value;

pub(crate) use remuda_hub_client::{ClientError, HubClient, host_matches_labels, pick_host};

/// Shared `--hub` / `--token` / `--bootstrap-token` flags (`REMUDA_*` env).
#[derive(Debug, Clone, Args)]
pub(crate) struct HubOpts {
    /// Hub base URL. Defaults to `REMUDA_HUB` or `http://127.0.0.1:8080`.
    #[arg(long, global = true)]
    pub hub: Option<String>,
    /// Device bearer token. Defaults to `REMUDA_TOKEN`.
    #[arg(long, global = true)]
    pub token: Option<String>,
    /// Bootstrap token used to mint a device token (`REMUDA_BOOTSTRAP_TOKEN`).
    #[arg(long, global = true)]
    pub bootstrap_token: Option<String>,
}

impl HubOpts {
    fn hub_url(&self) -> String {
        self.hub
            .clone()
            .or_else(|| std::env::var("REMUDA_HUB").ok())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8080".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn token(&self) -> Option<String> {
        self.token
            .clone()
            .or_else(|| std::env::var("REMUDA_TOKEN").ok())
            .filter(|s| !s.is_empty())
    }

    fn bootstrap_token(&self) -> Option<String> {
        self.bootstrap_token
            .clone()
            .or_else(|| std::env::var("REMUDA_BOOTSTRAP_TOKEN").ok())
            .filter(|s| !s.is_empty())
    }

    /// Build a [`HubClient`] from flags and `REMUDA_*` environment variables.
    pub(crate) fn connect(&self) -> Result<HubClient, ClientError> {
        HubClient::new(self.hub_url(), self.token(), self.bootstrap_token())
    }
}

/// Convert `key=value` / bare-key CLI labels into a placement map.
#[cfg(test)]
pub(crate) fn labels_to_map(labels: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    for label in labels {
        if let Some((key, value)) = label.split_once('=') {
            map.insert(key.to_string(), json_value(value));
        } else {
            map.insert(label.clone(), Value::Bool(true));
        }
    }
    Value::Object(map)
}

#[cfg(test)]
fn json_value(value: &str) -> Value {
    Value::String(value.to_string())
}

pub(crate) fn print_json(value: &Value) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(crate) fn block_on<T>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(fut)
}

#[cfg(test)]
pub(crate) fn connect_for_test(base: String, token: String) -> Result<HubClient, ClientError> {
    HubClient::new(base, Some(token), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn labels_to_map_splits_key_value() {
        let value = labels_to_map(&["region=sg".into(), "gpu".into()]);
        assert_eq!(value["region"], json!("sg"));
        assert_eq!(value["gpu"], json!(true));
    }
}
