//! Thin CLI wrapper around [`remuda_hub_client`].
//!
//! Shared `--hub` / `--token` flags stay here because they are clap `Args`.
//! HTTP/WS talk lives in `crates/remuda-hub-client`.

use anyhow::Context;
use clap::Args;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(crate) use remuda_hub_client::{ClientError, HubClient, pick_host};

/// `remuda dev` Hub listen used by dogfood when no `listen` file is present.
const DEV_HUB_URL: &str = "http://127.0.0.1:18080";
/// Plain `remuda hub` default.
const HUB_URL: &str = "http://127.0.0.1:8080";

/// Shared `--hub` / `--token` / `--bootstrap-token` flags (`REMUDA_*` env).
#[derive(Debug, Clone, Args)]
pub(crate) struct HubOpts {
    /// Hub base URL. Defaults to `REMUDA_HUB`, then the data-dir `listen` file.
    #[arg(long, global = true)]
    pub hub: Option<String>,
    /// Device bearer token. Defaults to `REMUDA_TOKEN`.
    #[arg(long, global = true)]
    pub token: Option<String>,
    /// Bootstrap token used to mint a device token (`REMUDA_BOOTSTRAP_TOKEN`).
    #[arg(long, global = true)]
    pub bootstrap_token: Option<String>,
}

/// Inputs for [`resolve_hub`] (flags + process env + data dir).
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolveInput {
    pub flag_hub: Option<String>,
    pub flag_token: Option<String>,
    pub flag_bootstrap: Option<String>,
    pub env_hub: Option<String>,
    pub env_token: Option<String>,
    pub env_bootstrap: Option<String>,
    pub env_data_dir: Option<PathBuf>,
    pub fallback_hub: Option<String>,
    pub cwd: PathBuf,
}

/// Resolved Hub URL and credentials for [`HubClient::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedHub {
    pub url: String,
    pub token: Option<String>,
    pub bootstrap_token: Option<String>,
}

impl HubOpts {
    /// Build a [`HubClient`] from flags, `REMUDA_*` env, and the Hub data dir.
    pub(crate) fn connect(&self) -> Result<HubClient, ClientError> {
        let resolved = resolve_hub(&ResolveInput::from_opts(self));
        let instance = env_present("REMUDA_INSTANCE_ID");
        // Instances never fall back to a Node's bootstrap or a repo token file.
        let bootstrap = if instance.is_some() {
            None
        } else {
            resolved.bootstrap_token
        };
        HubClient::new(resolved.url, resolved.token, bootstrap)
            .map(|client| client.with_caller_instance(instance))
    }
}

impl ResolveInput {
    pub(crate) fn from_opts(opts: &HubOpts) -> Self {
        Self {
            flag_hub: opts.hub.clone(),
            flag_token: opts.token.clone(),
            flag_bootstrap: opts.bootstrap_token.clone(),
            env_hub: env_present("REMUDA_HUB"),
            env_token: env_present("REMUDA_TOKEN"),
            env_bootstrap: env_present("REMUDA_BOOTSTRAP_TOKEN"),
            env_data_dir: std::env::var_os("REMUDA_DATA_DIR").map(PathBuf::from),
            fallback_hub: None,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// Resolve Hub URL and tokens.
///
/// Order for URL: `--hub`, `REMUDA_HUB`, `$data_dir/listen` (and `dev-hub/listen`),
/// then `http://127.0.0.1:18080` when a bootstrap/access-code file exists (remuda
/// dev), else `http://127.0.0.1:8080`.
///
/// Order for device token: `--token`, `REMUDA_TOKEN`.
/// Order for bootstrap / access code: `--bootstrap-token`, `REMUDA_BOOTSTRAP_TOKEN`,
/// then `bootstrap-token` or `access-code` in the data dir.
pub(crate) fn resolve_hub(input: &ResolveInput) -> ResolvedHub {
    let dirs = data_dir_candidates(input);
    let url = first_present([&input.flag_hub, &input.env_hub])
        .or_else(|| first_listen_file(&dirs))
        .or_else(|| input.fallback_hub.clone())
        .unwrap_or_else(|| default_hub_url(&dirs))
        .trim_end_matches('/')
        .to_string();
    let token = first_present([&input.flag_token, &input.env_token]);
    let bootstrap_token = first_present([&input.flag_bootstrap, &input.env_bootstrap])
        .or_else(|| first_secret_file(&dirs));
    ResolvedHub {
        url,
        token,
        bootstrap_token,
    }
}

fn data_dir_candidates(input: &ResolveInput) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut push = |path: PathBuf| {
        if !dirs.iter().any(|d| d == &path) {
            dirs.push(path);
        }
    };
    if let Some(dir) = &input.env_data_dir {
        push(dir.clone());
        push(dir.join("dev-hub"));
    }
    push(input.cwd.join("data").join("dev-hub"));
    push(input.cwd.join("data"));
    dirs
}

fn default_hub_url(dirs: &[PathBuf]) -> String {
    if dirs.iter().any(|dir| secret_file(dir).is_some()) {
        DEV_HUB_URL.to_string()
    } else {
        HUB_URL.to_string()
    }
}

fn first_listen_file(dirs: &[PathBuf]) -> Option<String> {
    for dir in dirs {
        let path = dir.join("listen");
        if let Some(raw) = read_trimmed(&path) {
            return Some(normalize_listen(&raw));
        }
    }
    None
}

fn first_secret_file(dirs: &[PathBuf]) -> Option<String> {
    dirs.iter().find_map(|dir| secret_file(dir))
}

fn secret_file(dir: &Path) -> Option<String> {
    for name in ["bootstrap-token", "access-code"] {
        if let Some(value) = read_trimmed(&dir.join(name)) {
            return Some(value);
        }
    }
    None
}

fn normalize_listen(raw: &str) -> String {
    let value = raw.trim();
    if value.starts_with("http://") || value.starts_with("https://") {
        value.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", value.trim_start_matches('/'))
    }
}

fn first_present(values: [&Option<String>; 2]) -> Option<String> {
    values
        .into_iter()
        .find_map(|value| present(value.as_deref()))
}

fn present(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.starts_with('<') {
        return None;
    }
    Some(value.to_string())
}

fn env_present(key: &str) -> Option<String> {
    present(std::env::var(key).ok().as_deref())
}

fn read_trimmed(path: &Path) -> Option<String> {
    let value = std::fs::read_to_string(path).ok()?;
    present(Some(value.as_str()))
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

/// Render a Hub refusal (e.g. 409 `PIN_REFUSED`) with its `reasons[]` lines
/// on stderr instead of an opaque JSON body, so the operator sees both the
/// rejected pin and the did-you-mean suggestions. Other HTTP errors pass
/// through unchanged.
pub(crate) fn hub_http_error(err: ClientError) -> anyhow::Error {
    if let ClientError::Http { status, body } = &err
        && let Ok(value) = serde_json::from_str::<Value>(body)
    {
        let code = value.get("code").and_then(Value::as_str).unwrap_or("");
        if code == "PIN_REFUSED" {
            let headline = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("pin refused");
            let mut lines = vec![format!("hub HTTP {status}: {headline}")];
            if let Some(reasons) = value.get("reasons").and_then(Value::as_array) {
                for reason in reasons.iter().filter_map(Value::as_str) {
                    lines.push(format!("  - {reason}"));
                }
            }
            return anyhow::anyhow!(lines.join("\n"));
        }
        // D-047: a delivery refusal is terminal — dispatch exits non-zero and
        // nothing retries another route. Surface the stable refusal code
        // (`api-via-unknown-host` / `-host-offline` / `-unsupported` /
        // `-unreachable`) on the first line instead of burying it in JSON.
        if code.starts_with("api-via-") {
            let headline = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("delivery refused");
            return anyhow::anyhow!("hub HTTP {status}: {code}\n  {headline}");
        }
    }
    anyhow::Error::new(err)
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
    use std::fs;

    #[test]
    fn labels_to_map_splits_key_value() {
        let value = labels_to_map(&["region=sg".into(), "gpu".into()]);
        assert_eq!(value["region"], json!("sg"));
        assert_eq!(value["gpu"], json!(true));
    }

    fn input(cwd: &Path) -> ResolveInput {
        ResolveInput {
            cwd: cwd.to_path_buf(),
            ..ResolveInput::default()
        }
    }

    #[test]
    fn resolve_prefers_flag_hub_over_env_and_listen_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("listen"), "http://127.0.0.1:18080").unwrap();
        let mut input = input(dir.path());
        input.env_data_dir = Some(dir.path().to_path_buf());
        input.env_hub = Some("http://127.0.0.1:9999".into());
        input.flag_hub = Some("http://127.0.0.1:1111".into());
        let resolved = resolve_hub(&input);
        assert_eq!(resolved.url, "http://127.0.0.1:1111");
    }

    #[test]
    fn resolve_prefers_env_hub_over_listen_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("listen"), "http://127.0.0.1:18080").unwrap();
        let mut input = input(dir.path());
        input.env_data_dir = Some(dir.path().to_path_buf());
        input.env_hub = Some("http://127.0.0.1:9999".into());
        let resolved = resolve_hub(&input);
        assert_eq!(resolved.url, "http://127.0.0.1:9999");
    }

    #[test]
    fn resolve_reads_listen_and_bootstrap_from_data_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = dir.path().join("dev-hub");
        fs::create_dir_all(&hub).unwrap();
        fs::write(hub.join("listen"), "http://127.0.0.1:18080\n").unwrap();
        fs::write(hub.join("bootstrap-token"), "dev-access\n").unwrap();
        let mut input = input(dir.path());
        input.env_data_dir = Some(dir.path().to_path_buf());
        let resolved = resolve_hub(&input);
        assert_eq!(resolved.url, "http://127.0.0.1:18080");
        assert_eq!(resolved.bootstrap_token.as_deref(), Some("dev-access"));
        assert!(resolved.token.is_none());
    }

    #[test]
    fn resolve_prefers_env_token_over_access_code_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("access-code"), "file-secret").unwrap();
        let mut input = input(dir.path());
        input.env_data_dir = Some(dir.path().to_path_buf());
        input.env_token = Some("device-token".into());
        input.env_bootstrap = Some("env-boot".into());
        let resolved = resolve_hub(&input);
        assert_eq!(resolved.token.as_deref(), Some("device-token"));
        assert_eq!(resolved.bootstrap_token.as_deref(), Some("env-boot"));
    }

    #[test]
    fn resolve_defaults_dev_port_when_bootstrap_exists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = dir.path().join("data").join("dev-hub");
        fs::create_dir_all(&hub).unwrap();
        fs::write(hub.join("bootstrap-token"), "dev-access").unwrap();
        let resolved = resolve_hub(&input(dir.path()));
        assert_eq!(resolved.url, DEV_HUB_URL);
        assert_eq!(resolved.bootstrap_token.as_deref(), Some("dev-access"));
    }

    #[test]
    fn resolve_defaults_plain_hub_without_data_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_hub(&input(dir.path()));
        assert_eq!(resolved.url, HUB_URL);
        assert!(resolved.token.is_none());
        assert!(resolved.bootstrap_token.is_none());
    }

    #[test]
    fn resolve_ignores_placeholder_env() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut input = input(dir.path());
        input.env_hub = Some("<device-bootstrap-or-omit-if-REMUDA_TOKEN-set>".into());
        input.env_bootstrap = Some("<device-bootstrap-or-omit-if-REMUDA_TOKEN-set>".into());
        let resolved = resolve_hub(&input);
        assert_eq!(resolved.url, HUB_URL);
        assert!(resolved.bootstrap_token.is_none());
    }
}
