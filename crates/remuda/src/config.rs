//! Process configuration: defaults < remuda.toml < REMUDA_* environment < CLI.
//!
//! `--config` overrides `REMUDA_CONFIG`; an explicitly selected missing file is an
//! error. TOML paths are relative to that file, environment/CLI paths to the cwd.
//! Example (synthetic; credentials remain references):
//! ```toml
//! data_dir = "./data"
//! shutdown_timeout_secs = 10
//! [hub]
//! listen = "127.0.0.1:8080"
//! [node]
//! listen = "127.0.0.1:8787"
//! hub_url = "wss://hub.example/v1/node"
//! host_token = "file:./secrets/node-token"
//! maxInstances = 8
//! labels = { region = "sg" }
//! [provider_profiles.gateway]
//! endpoint = "https://gateway.example/v1"
//! models = ["example-model"]
//! secret_refs = { api_key = "env:GATEWAY_API_KEY" }
//! ```
//! Overrides: DATA_DIR, HUB_LISTEN (or LISTEN), NODE_LISTEN, HUB_URL,
//! HOST_TOKEN, HOST_TOKEN_FILE (or NODE_TOKEN_FILE), LABELS (JSON object),
//! MAX_INSTANCES, PROVIDER_PROFILES (JSON object), SHUTDOWN_TIMEOUT_SECS,
//! BOOTSTRAP_TOKEN, WEB_PASSWORD_FILE, COOKIE_SECURE, WEB_ROOT,
//! ALLOWED_ORIGINS and WEB_ORIGINS (JSON arrays), all prefixed `REMUDA_`.
//! Direct token environment variables take precedence over token-file variables.

use anyhow::{Context, bail, ensure};
use serde::{Deserialize, Deserializer};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(alias = "dataDir")]
    pub data_dir: PathBuf,
    pub hub: Hub,
    pub node: Node,
    #[serde(alias = "providerProfiles")]
    pub provider_profiles: BTreeMap<String, ProviderProfile>,
    #[serde(alias = "shutdownTimeoutSecs")]
    pub shutdown_timeout_secs: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Hub {
    pub listen: SocketAddr,
    #[serde(alias = "bootstrapToken")]
    pub bootstrap_token: Option<SecretRef>,
    #[serde(alias = "cookieSecure")]
    pub cookie_secure: bool,
    #[serde(alias = "allowedOrigins")]
    pub allowed_origins: Vec<String>,
    #[serde(alias = "webRoot")]
    pub web_root: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Node {
    pub listen: SocketAddr,
    #[serde(alias = "hubUrl")]
    pub hub_url: Option<String>,
    #[serde(alias = "hostToken")]
    pub host_token: Option<SecretRef>,
    pub labels: BTreeMap<String, String>,
    #[serde(alias = "maxInstances")]
    pub max_instances: usize,
    #[serde(alias = "herdrSocket")]
    pub herdr_socket: Option<PathBuf>,
    pub workspace: PathBuf,
    #[serde(alias = "webOrigins")]
    pub web_origins: Vec<String>,
}

/// A native-login profile may omit both endpoint and credentials.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderProfile {
    pub endpoint: Option<String>,
    pub models: Vec<String>,
    #[serde(alias = "secretRefs")]
    pub secret_refs: BTreeMap<String, SecretRef>,
}

/// Only references are accepted in TOML and provider-profile environment JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SecretRef {
    Env(String),
    File(PathBuf),
}

/// Resolved credential with redacted diagnostics and no serialization interface.
pub(crate) struct Secret(String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl Secret {
    pub fn into_string(self) -> String {
        self.0
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        match text.split_once(':') {
            Some(("env", name))
                if !name.is_empty()
                    && name.bytes().enumerate().all(|(i, byte)| {
                        byte == b'_'
                            || byte.is_ascii_alphabetic()
                            || (i > 0 && byte.is_ascii_digit())
                    }) =>
            {
                Ok(Self::Env(name.to_owned()))
            }
            Some(("file", path)) if !path.is_empty() && !path.contains(['\n', '\r', '\0']) => {
                Ok(Self::File(path.into()))
            }
            _ => Err(serde::de::Error::custom(
                "expected env:NAME or file:PATH secret reference",
            )),
        }
    }
}

impl SecretRef {
    /// Resolve only at the consumer that needs the credential, never during config loading.
    pub fn resolve(&self) -> anyhow::Result<Secret> {
        let value = match self {
            Self::Env(name) => std::env::var(name).map_err(|_| {
                anyhow::anyhow!("secret environment variable {name} is unavailable")
            })?,
            Self::File(path) => std::fs::read_to_string(path)
                .with_context(|| format!("cannot read secret file {}", path.display()))?,
        };
        let value = value.trim();
        ensure!(
            !value.is_empty(),
            "secret reference resolved to an empty value"
        );
        Ok(Secret(value.to_owned()))
    }

    fn absolutize(&mut self, base: &Path) {
        if let Self::File(path) = self {
            *path = absolute(base, path);
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: "./data".into(),
            hub: Hub::default(),
            node: Node::default(),
            provider_profiles: BTreeMap::new(),
            shutdown_timeout_secs: 10,
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8080)),
            bootstrap_token: None,
            cookie_secure: true,
            allowed_origins: Vec::new(),
            web_root: None,
        }
    }
}

impl Default for Node {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], remuda_node::DEFAULT_DEV_PORT)),
            hub_url: None,
            host_token: None,
            labels: BTreeMap::new(),
            max_instances: 8,
            herdr_socket: None,
            workspace: ".".into(),
            web_origins: vec![
                "http://localhost:5173".into(),
                "http://127.0.0.1:5173".into(),
            ],
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        Self::load_with(path, &std::env::current_dir()?, &|key| {
            std::env::var_os(key)
        })
    }

    fn load_with(
        selected: Option<&Path>,
        cwd: &Path,
        environment: &impl Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<Self> {
        let selected = selected
            .map(PathBuf::from)
            .or_else(|| environment("REMUDA_CONFIG").map(PathBuf::from));
        let path = absolute(cwd, selected.as_deref().unwrap_or(Path::new("remuda.toml")));
        let (mut config, base) = match std::fs::read_to_string(&path) {
            Ok(text) => {
                // TOML diagnostics can echo source lines containing accidental literal secrets.
                let config = toml::from_str::<Self>(&text).map_err(|_| {
                    anyhow::anyhow!("invalid configuration in {}; check TOML syntax, field names and secret references", path.display())
                })?;
                (config, path.parent().unwrap_or(cwd))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && selected.is_none() => {
                (Self::default(), cwd)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot read configuration {}", path.display()));
            }
        };
        config.absolutize(base)?;
        config.apply_environment(cwd, environment)?;
        config.validate()?;
        Ok(config)
    }

    fn absolutize(&mut self, base: &Path) -> anyhow::Result<()> {
        ensure!(
            !self.data_dir.as_os_str().is_empty(),
            "data_dir must not be empty"
        );
        self.data_dir = absolute(base, &self.data_dir);
        self.node.workspace = absolute(base, &self.node.workspace);
        for path in [&mut self.hub.web_root, &mut self.node.herdr_socket]
            .into_iter()
            .flatten()
        {
            ensure!(
                !path.as_os_str().is_empty(),
                "configured path must not be empty"
            );
            *path = absolute(base, path);
        }
        for reference in [&mut self.hub.bootstrap_token, &mut self.node.host_token]
            .into_iter()
            .flatten()
        {
            reference.absolutize(base);
        }
        for profile in self.provider_profiles.values_mut() {
            for reference in profile.secret_refs.values_mut() {
                reference.absolutize(base);
            }
        }
        Ok(())
    }

    fn apply_environment(
        &mut self,
        cwd: &Path,
        env: &impl Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<()> {
        if let Some(path) = env("REMUDA_DATA_DIR") {
            ensure!(!path.is_empty(), "REMUDA_DATA_DIR must not be empty");
            self.data_dir = absolute(cwd, &PathBuf::from(path));
        }
        if let Some(path) = env("REMUDA_WEB_ROOT") {
            ensure!(!path.is_empty(), "REMUDA_WEB_ROOT must not be empty");
            self.hub.web_root = Some(absolute(cwd, &PathBuf::from(path)));
        }
        if let Some(value) = env_text(env, "REMUDA_HUB_LISTEN")?.or(env_text(env, "REMUDA_LISTEN")?)
        {
            self.hub.listen = parse_env(&value, "REMUDA_HUB_LISTEN/REMUDA_LISTEN")?;
        }
        if let Some(value) = env_text(env, "REMUDA_NODE_LISTEN")? {
            self.node.listen = parse_env(&value, "REMUDA_NODE_LISTEN")?;
        }
        if let Some(value) = env_text(env, "REMUDA_HUB_URL")? {
            self.node.hub_url = Some(value);
        }
        if let Some(value) = env_text(env, "REMUDA_MAX_INSTANCES")? {
            self.node.max_instances = parse_env(&value, "REMUDA_MAX_INSTANCES")?;
        }
        if let Some(value) = env_text(env, "REMUDA_SHUTDOWN_TIMEOUT_SECS")? {
            self.shutdown_timeout_secs = parse_env(&value, "REMUDA_SHUTDOWN_TIMEOUT_SECS")?;
        }
        if let Some(value) = env_text(env, "REMUDA_COOKIE_SECURE")? {
            self.hub.cookie_secure = match value.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                _ => bail!("REMUDA_COOKIE_SECURE must be true, false, 1 or 0"),
            };
        }
        if let Some(value) = env_text(env, "REMUDA_LABELS")? {
            self.node.labels = parse_json_env(&value, "REMUDA_LABELS")?;
        }
        if let Some(value) = env_text(env, "REMUDA_PROVIDER_PROFILES")? {
            self.provider_profiles = parse_json_env(&value, "REMUDA_PROVIDER_PROFILES")?;
            for profile in self.provider_profiles.values_mut() {
                for reference in profile.secret_refs.values_mut() {
                    reference.absolutize(cwd);
                }
            }
        }
        if let Some(value) = env_text(env, "REMUDA_ALLOWED_ORIGINS")? {
            self.hub.allowed_origins = parse_json_env(&value, "REMUDA_ALLOWED_ORIGINS")?;
        }
        if let Some(value) = env_text(env, "REMUDA_WEB_ORIGINS")? {
            self.node.web_origins = parse_json_env(&value, "REMUDA_WEB_ORIGINS")?;
        }
        if let Some(path) = env("REMUDA_HOST_TOKEN_FILE").or_else(|| env("REMUDA_NODE_TOKEN_FILE"))
        {
            ensure!(!path.is_empty(), "host token file path must not be empty");
            self.node.host_token = Some(SecretRef::File(absolute(cwd, &PathBuf::from(path))));
        }
        if env("REMUDA_HOST_TOKEN").is_some() {
            self.node.host_token = Some(SecretRef::Env("REMUDA_HOST_TOKEN".into()));
        }
        if let Some(path) = env("REMUDA_WEB_PASSWORD_FILE") {
            ensure!(
                !path.is_empty(),
                "REMUDA_WEB_PASSWORD_FILE must not be empty"
            );
            self.hub.bootstrap_token = Some(SecretRef::File(absolute(cwd, &PathBuf::from(path))));
        }
        if env("REMUDA_BOOTSTRAP_TOKEN").is_some() {
            self.hub.bootstrap_token = Some(SecretRef::Env("REMUDA_BOOTSTRAP_TOKEN".into()));
        }
        Ok(())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.data_dir.as_os_str().is_empty(),
            "data_dir must not be empty"
        );
        ensure!(self.node.max_instances > 0, "maxInstances must be positive");
        ensure!(
            self.shutdown_timeout_secs > 0,
            "shutdown_timeout_secs must be positive"
        );
        validate_labels(&self.node.labels)?;
        if let Some(url) = &self.node.hub_url {
            validate_url(url, true)?;
        }
        for (id, profile) in &self.provider_profiles {
            ensure!(
                !id.trim().is_empty(),
                "provider profile ID must not be empty"
            );
            if let Some(url) = &profile.endpoint {
                validate_url(url, false)?;
            }
            ensure!(
                profile.models.iter().all(|name| !name.trim().is_empty()),
                "provider model names must not be empty"
            );
            ensure!(
                profile
                    .secret_refs
                    .keys()
                    .all(|name| !name.trim().is_empty()),
                "provider secret reference names must not be empty"
            );
        }
        Ok(())
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_secs(self.shutdown_timeout_secs)
    }
}

pub(crate) fn parse_labels(labels: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .context("--label requires KEY=VALUE")?;
        ensure!(
            parsed
                .insert(key.trim().to_owned(), value.trim().to_owned())
                .is_none(),
            "duplicate --label key"
        );
    }
    validate_labels(&parsed)?;
    Ok(parsed)
}

fn validate_labels(labels: &BTreeMap<String, String>) -> anyhow::Result<()> {
    ensure!(
        labels
            .iter()
            .all(|(key, value)| !key.trim().is_empty() && !value.trim().is_empty()),
        "labels require non-empty keys and values"
    );
    Ok(())
}

fn validate_url(text: &str, websocket: bool) -> anyhow::Result<()> {
    let uri = text
        .parse::<axum::http::Uri>()
        .map_err(|_| anyhow::anyhow!("invalid configured endpoint URL"))?;
    let authority = uri.authority().context("endpoint URL requires a host")?;
    ensure!(
        !authority.as_str().contains('@') && uri.query().is_none() && !text.contains('#'),
        "endpoint credentials must use secret references, not URL userinfo, query or fragment"
    );
    if websocket {
        ensure!(uri.scheme_str() == Some("wss"), "hub_url must use wss://");
    } else {
        ensure!(
            matches!(uri.scheme_str(), Some("http" | "https")),
            "provider endpoint must use http:// or https://"
        );
    }
    Ok(())
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

fn env_text(env: &impl Fn(&str) -> Option<OsString>, key: &str) -> anyhow::Result<Option<String>> {
    env(key)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("{key} is not valid UTF-8"))
        })
        .transpose()
}

fn parse_env<T: FromStr>(value: &str, key: &str) -> anyhow::Result<T> {
    value.parse().map_err(|_| anyhow::anyhow!("invalid {key}"))
}

fn parse_json_env<T: serde::de::DeserializeOwned>(value: &str, key: &str) -> anyhow::Result<T> {
    serde_json::from_str(value).map_err(|_| anyhow::anyhow!("invalid JSON configuration in {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(text: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "remuda-config-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).expect("fixture directory");
            std::fs::write(path.join("remuda.toml"), text).expect("synthetic TOML fixture");
            Self(path)
        }
        fn load(&self, values: &[(&str, &str)]) -> anyhow::Result<Config> {
            Config::load_with(None, &self.0, &|key| {
                values
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn environment_overrides_file_without_resetting_other_fields() {
        // Synthetic config with reference-only credentials; no real secret is accessed.
        let fixture = Fixture::new(
            r#"
data_dir = "file-data"
[hub]
listen = "127.0.0.1:8100"
[node]
listen = "127.0.0.1:8101"
hub_url = "wss://file.example/v1/node"
host_token = "file:./token"
labels = { region = "file" }
maxInstances = 3
[provider_profiles.file]
secret_refs = { token = "env:UNREAD_TEST_SECRET" }
"#,
        );
        let config = fixture
            .load(&[
                ("REMUDA_DATA_DIR", "env-data"),
                ("REMUDA_HUB_LISTEN", "127.0.0.1:8200"),
                ("REMUDA_NODE_LISTEN", "127.0.0.1:8201"),
                ("REMUDA_HUB_URL", "wss://env.example/v1/node"),
                ("REMUDA_HOST_TOKEN_FILE", "env-token"),
                ("REMUDA_HOST_TOKEN", "never-logged-fixture"),
                ("REMUDA_LABELS", r#"{"region":"env"}"#),
                ("REMUDA_MAX_INSTANCES", "5"),
            ])
            .expect("precedence");
        assert_eq!(config.data_dir, fixture.0.join("env-data"));
        assert_eq!(config.hub.listen.port(), 8200);
        assert_eq!(config.node.listen.port(), 8201);
        assert_eq!(
            config.node.hub_url.as_deref(),
            Some("wss://env.example/v1/node")
        );
        assert_eq!(
            config.node.host_token,
            Some(SecretRef::Env("REMUDA_HOST_TOKEN".into()))
        );
        assert_eq!(config.node.labels["region"], "env");
        assert_eq!(config.node.max_instances, 5);
        assert_eq!(config.provider_profiles.len(), 1);
        assert_eq!(config.shutdown_timeout_secs, 10);
        assert!(!format!("{config:?}").contains("never-logged-fixture"));
    }

    #[test]
    fn explicit_config_wins_over_environment_and_paths_follow_its_directory() {
        let fixture = Fixture::new("data_dir = 'local'\n[node]\nhost_token = 'file:token'\n");
        let config = Config::load_with(
            Some(&fixture.0.join("remuda.toml")),
            Path::new("/"),
            &|key| (key == "REMUDA_CONFIG").then(|| OsString::from("absent.toml")),
        )
        .expect("explicit config");
        assert_eq!(config.data_dir, fixture.0.join("local"));
        assert_eq!(
            config.node.host_token,
            Some(SecretRef::File(fixture.0.join("token")))
        );
        assert!(Config::load_with(Some(Path::new("missing.toml")), &fixture.0, &|_| None).is_err());
        std::fs::remove_file(fixture.0.join("remuda.toml")).expect("remove default fixture");
        assert_eq!(
            fixture
                .load(&[])
                .expect("optional default")
                .node
                .max_instances,
            8
        );
        assert!(fixture.load(&[("REMUDA_CONFIG", "missing.toml")]).is_err());
    }

    #[test]
    fn profile_environment_replaces_file_profiles_and_preserves_references() {
        let fixture = Fixture::new("[provider_profiles.old]\nmodels = ['old']\n");
        let config = fixture
            .load(&[(
                "REMUDA_PROVIDER_PROFILES",
                r#"{"new":{"models":["new"],"secret_refs":{"key":"file:credential"}}}"#,
            )])
            .expect("profile override");
        assert!(!config.provider_profiles.contains_key("old"));
        assert_eq!(
            config.provider_profiles["new"].secret_refs["key"],
            SecretRef::File(fixture.0.join("credential"))
        );
    }

    #[test]
    fn invalid_config_fails_without_echoing_literal_credentials() {
        let fixture = Fixture::new(
            "[provider_profiles.bad]\nsecret_refs = { key = 'private-fixture-secret' }\n",
        );
        let error = fixture
            .load(&[])
            .expect_err("literal secret rejected")
            .to_string();
        assert!(!error.contains("private-fixture-secret"));
        for text in [
            "data_dir = ''",
            "[hub]\nweb_root = ''",
            "[node]\nmaxInstances = 0",
            "[node]\nmaxInstnaces = 3",
            "[node]\nhub_url = 'wss://host/path?token=secret'",
            "shutdown_timeout_secs = 0",
        ] {
            let fixture = Fixture::new(text);
            assert!(fixture.load(&[]).is_err(), "invalid config must fail");
        }
        let fixture = Fixture::new("");
        assert!(fixture.load(&[("REMUDA_DATA_DIR", "")]).is_err());
        assert!(fixture.load(&[("REMUDA_WEB_ROOT", "")]).is_err());
        assert!(
            fixture
                .load(&[("REMUDA_MAX_INSTANCES", "invalid")])
                .is_err()
        );
        assert!(
            fixture
                .load(&[("REMUDA_LABELS", r#"{"":"value"}"#)])
                .is_err()
        );
        assert!(parse_labels(&["region=one".into(), "region=two".into()]).is_err());
        assert_eq!(
            format!("{:?}", Secret("private-fixture-secret".into())),
            "Secret([redacted])"
        );
    }
}
