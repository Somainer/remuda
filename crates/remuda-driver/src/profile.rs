//! Thin ProviderProfile, M0 env/file SecretBroker, and Claude settings overlay.

use crate::error::{DriverError, DriverResult};
use async_trait::async_trait;
use remuda_protocol::Id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Client-side wire contract of a profile, not the upstream vendor brand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    /// Anthropic Messages (`/v1/messages`).
    Anthropic,
    /// OpenAI Responses (`/v1/responses`).
    OpenaiResponses,
    /// xAI native or Responses-compatible ingress.
    Xai,
    /// Gemini-native ingress (agy).
    Google,
}

/// How the CLI authenticates. `decisions.md` D-012. Default is native login.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Delegation {
    /// CLI uses its own native login. No env/settings overlay is injected.
    #[default]
    None,
    /// Any Anthropic-Messages-compatible endpoint (`ANTHROPIC_BASE_URL` overlay).
    Gateway,
    /// Direct provider keys with runtime rotation. v2; rejected at materialize.
    Direct,
}

/// Runtime health of a profile/endpoint. Unknown is not auto-selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderHealth {
    /// Eligible for automatic selection.
    Healthy,
    /// Cooling down after a failure.
    Cooldown,
    /// Operator-disabled.
    Disabled,
    /// No successful probe yet; do not auto-select.
    Unknown,
}

/// Reference to a secret that must never be persisted in a launch recipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretRef(String);

impl SecretRef {
    /// Accept `env:NAME`, `file:PATH`, `store:NAME`, `keychain:ACCOUNT`, or `helper:/abs/command`.
    pub fn parse(raw: impl Into<String>) -> DriverResult<Self> {
        let raw = raw.into();
        if raw.starts_with("env:") && raw.len() > 4
            || raw.starts_with("file:") && raw.len() > 5
            || raw.starts_with("helper:") && raw.len() > 7
            || raw.starts_with("store:") && raw.len() > 6
            || raw.starts_with("keychain:") && raw.len() > 9
        {
            Ok(Self(raw))
        } else {
            Err(DriverError::InvalidLaunchSpec(
                "secret_ref must use env:, file:, store:, keychain:, or helper: scheme".into(),
            ))
        }
    }

    /// Wire spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `env:` variable name.
    pub fn env_name(&self) -> Option<&str> {
        self.0.strip_prefix("env:")
    }

    /// `file:` path.
    pub fn file_path(&self) -> Option<&str> {
        self.0.strip_prefix("file:")
    }

    /// Absolute `apiKeyHelper` command; secret stays out of settings/env files.
    pub fn helper_command(&self) -> Option<&str> {
        self.0.strip_prefix("helper:")
    }

    /// `store:NAME` vault key.
    pub fn store_name(&self) -> Option<&str> {
        self.0.strip_prefix("store:")
    }

    /// `keychain:ACCOUNT` or `keychain:SERVICE/ACCOUNT`. Default service is `remuda`.
    pub fn keychain_spec(&self) -> Option<(&str, &str)> {
        let rest = self.0.strip_prefix("keychain:")?;
        match rest.split_once('/') {
            Some((service, account)) if !service.is_empty() && !account.is_empty() => {
                Some((service, account))
            }
            None if !rest.is_empty() => Some(("remuda", rest)),
            _ => None,
        }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Thin provider profile: how a CLI reaches an endpoint. `gateway-auth.md` §6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    /// Profile identity (`pvp_` prefix).
    pub id: Id,
    /// Client wire contract.
    pub kind: ProviderKind,
    /// Ingress base URL for [`Delegation::Gateway`]. Ignored for [`Delegation::None`].
    pub base_url: String,
    /// Authentication mode. Default [`Delegation::None`] is native CLI login.
    #[serde(default)]
    pub delegation: Delegation,
    /// Vault/env/file/helper reference. Unused when `delegation` is `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretRef>,
    /// Catalog model ids this profile may resolve.
    pub models: Vec<String>,
    /// Last known health. Only [`ProviderHealth::Healthy`] auto-launches.
    pub health: ProviderHealth,
}

/// Redacted secret bytes resolved at spawn time. Drop zeroes the buffer.
pub struct Secret {
    bytes: Vec<u8>,
}

impl Secret {
    /// Wrap already-resolved bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Borrow the secret. Callers must not log this.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }

    /// UTF-8 view of the secret.
    pub fn expose_str(&self) -> DriverResult<&str> {
        std::str::from_utf8(&self.bytes)
            .map_err(|_| DriverError::CredentialUnavailable("secret is not utf-8".into()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([redacted])")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in &mut self.bytes {
            *byte = 0;
        }
    }
}

/// Resolves [`SecretRef`] values. M0 reads env or files; named stores use [`crate::FileSecretStore`].
#[async_trait]
pub trait SecretBroker: Send + Sync {
    /// Fetch the current secret. Never persist the return value.
    async fn resolve(&self, secret_ref: &SecretRef) -> DriverResult<Secret>;
}

/// M0 broker: `env:NAME` from the process environment, `file:PATH` from a 0600 file.
///
/// `helper:` refs are commands, not secrets; resolving them is a protocol error.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvFileSecretBroker;

#[async_trait]
impl SecretBroker for EnvFileSecretBroker {
    async fn resolve(&self, secret_ref: &SecretRef) -> DriverResult<Secret> {
        if let Some(name) = secret_ref.env_name() {
            let value = std::env::var(name).map_err(|_| {
                DriverError::CredentialUnavailable(format!("environment variable {name} is unset"))
            })?;
            return Ok(Secret::new(value.into_bytes()));
        }
        if let Some(path) = secret_ref.file_path() {
            let contents = fs::read(Path::new(path)).map_err(|error| {
                DriverError::CredentialUnavailable(format!("read {path}: {error}"))
            })?;
            let trimmed = trim_secret_bytes(contents);
            if trimmed.is_empty() {
                return Err(DriverError::CredentialUnavailable(format!(
                    "secret file {path} is empty"
                )));
            }
            return Ok(Secret::new(trimmed));
        }
        if secret_ref.helper_command().is_some() {
            return Err(DriverError::CredentialUnavailable(
                "helper: refs are commands invoked by the native CLI, not broker secrets".into(),
            ));
        }
        Err(DriverError::CredentialUnavailable(
            "unsupported secret_ref scheme".into(),
        ))
    }
}

fn trim_secret_bytes(mut bytes: Vec<u8>) -> Vec<u8> {
    while bytes
        .last()
        .is_some_and(|b| *b == b'\n' || *b == b'\r' || *b == b' ')
    {
        bytes.pop();
    }
    bytes
}

/// Inputs for [`write_claude_provider_overlay`]. x-place calls this at Claude launch.
///
/// The secret is written only into the 0600 settings file. Do not log this struct
/// with a custom formatter that exposes [`Secret::expose`].
pub struct ClaudeProviderOverlay<'a> {
    /// `gateway` writes `ANTHROPIC_AUTH_TOKEN`; `direct` writes `ANTHROPIC_API_KEY`.
    pub delegation: Delegation,
    /// Ingress base URL. Required for [`Delegation::Gateway`].
    pub base_url: &'a str,
    /// Overlay `model` field (also passed as `--model` by the materializer).
    pub model: &'a str,
    /// Auth token. Never log [`Secret::expose`].
    pub secret: &'a Secret,
    /// Extra env names copied into `settings.env` (non-secret).
    pub extra_env: &'a BTreeMap<String, String>,
}

impl fmt::Debug for ClaudeProviderOverlay<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeProviderOverlay")
            .field("delegation", &self.delegation)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("secret", &self.secret)
            .field("extra_env_keys", &self.extra_env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Build Claude `settings.json` for a gateway/direct profile. Never log the return.
pub fn claude_provider_settings_json(overlay: &ClaudeProviderOverlay<'_>) -> DriverResult<Value> {
    if overlay.delegation == Delegation::None {
        return Err(DriverError::InvalidLaunchSpec(
            "native delegation does not write a provider settings overlay".into(),
        ));
    }
    let token = overlay.secret.expose_str()?.to_string();
    if token.is_empty() {
        return Err(DriverError::InvalidLaunchSpec(
            "provider overlay requires a non-empty auth token".into(),
        ));
    }
    let mut env = BTreeMap::new();
    match overlay.delegation {
        Delegation::Gateway => {
            if overlay.base_url.trim().is_empty() {
                return Err(DriverError::InvalidLaunchSpec(
                    "gateway delegation requires a base_url".into(),
                ));
            }
            env.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                Value::String(overlay.base_url.trim().to_string()),
            );
            env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), Value::String(token));
            env.insert(
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".to_string(),
                Value::String("1".into()),
            );
        }
        Delegation::Direct => {
            env.insert("ANTHROPIC_API_KEY".to_string(), Value::String(token));
            if !overlay.base_url.trim().is_empty() {
                env.insert(
                    "ANTHROPIC_BASE_URL".to_string(),
                    Value::String(overlay.base_url.trim().to_string()),
                );
            }
        }
        Delegation::None => unreachable!(),
    }
    for (name, value) in overlay.extra_env {
        if name == "ANTHROPIC_AUTH_TOKEN" || name == "ANTHROPIC_API_KEY" {
            continue;
        }
        env.entry(name.clone())
            .or_insert_with(|| Value::String(value.clone()));
    }
    Ok(json!({
        "model": overlay.model,
        "env": env,
    }))
}

/// Write 0600 `settings.json` into `launch_dir` and return the path for `--settings`.
///
/// Callers must not log the file contents. Existing `materialize` still uses
/// `apiKeyHelper` when a token broker is bound; this function is the Hub/Node
/// path that puts the token in the overlay env.
pub fn write_claude_provider_overlay(
    launch_dir: &Path,
    overlay: &ClaudeProviderOverlay<'_>,
) -> DriverResult<PathBuf> {
    if !launch_dir.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "launch dir must be an absolute path".into(),
        ));
    }
    fs::create_dir_all(launch_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(launch_dir, fs::Permissions::from_mode(0o700))?;
    }
    let settings = claude_provider_settings_json(overlay)?;
    let bytes = serde_json::to_vec_pretty(&settings)?;
    let path = launch_dir.join("settings.json");
    write_private_overlay(&path, &bytes)?;
    Ok(path)
}

fn write_private_overlay(path: &Path, bytes: &[u8]) -> DriverResult<()> {
    let tmp = {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(".tmp");
        PathBuf::from(raw)
    };
    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_broker_reads_and_does_not_debug_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        let value = "sk-super-secret-value";
        std::fs::write(&path, format!("{value}\n")).unwrap();
        let broker = EnvFileSecretBroker;
        let secret = broker
            .resolve(&SecretRef::parse(format!("file:{}", path.display())).unwrap())
            .await
            .unwrap();
        assert_eq!(secret.expose_str().unwrap(), value);
        assert!(!format!("{secret:?}").contains(value));
    }

    #[test]
    fn gateway_overlay_writes_env_and_redacts_debug() {
        let secret = Secret::new(b"sk-overlay-secret-value".to_vec());
        let extra = BTreeMap::new();
        let overlay = ClaudeProviderOverlay {
            delegation: Delegation::Gateway,
            base_url: "https://gateway.example/v1",
            model: "passthrough/auto",
            secret: &secret,
            extra_env: &extra,
        };
        let json = claude_provider_settings_json(&overlay).unwrap();
        assert_eq!(json["model"], "passthrough/auto");
        assert_eq!(
            json["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example/v1"
        );
        assert_eq!(
            json["env"]["ANTHROPIC_AUTH_TOKEN"],
            "sk-overlay-secret-value"
        );
        assert_eq!(
            json["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
            "1"
        );
        assert!(!format!("{overlay:?}").contains("sk-overlay-secret-value"));
    }

    #[test]
    fn direct_overlay_writes_api_key() {
        let secret = Secret::new(b"sk-direct-key".to_vec());
        let extra = BTreeMap::new();
        let overlay = ClaudeProviderOverlay {
            delegation: Delegation::Direct,
            base_url: "",
            model: "claude-sonnet",
            secret: &secret,
            extra_env: &extra,
        };
        let json = claude_provider_settings_json(&overlay).unwrap();
        assert_eq!(json["env"]["ANTHROPIC_API_KEY"], "sk-direct-key");
        assert!(json["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    }
}
