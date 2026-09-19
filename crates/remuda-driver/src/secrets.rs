//! Named secret stores: encrypted files, macOS Keychain, and a UDS token broker.
//!
//! Vault files and the master key are created `0600`. Ciphertext is
//! ChaCha20-Poly1305 with a per-secret nonce. Audit lines record the
//! [`SecretRef`] spelling and instance id, never the secret bytes or
//! per-instance tokens.
//!
//! Claude `apiKeyHelper` is the script [`crate::render_api_key_helper_script`]
//! writes into the launch overlay. The helper authenticates with a
//! per-instance token and prints one secret to stdout.

use crate::error::{DriverError, DriverResult};
use crate::profile::{Secret, SecretBroker, SecretRef};
use async_trait::async_trait;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use zeroize::Zeroize;

const MASTER_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const VAULT_VERSION: u32 = 1;
#[allow(dead_code)]
const DEFAULT_KEYCHAIN_SERVICE: &str = "remuda";

/// Envelope-encrypted named secrets under a data directory.
pub struct FileSecretStore {
    dir: PathBuf,
    vault_path: PathBuf,
    master: [u8; MASTER_LEN],
    lock: Mutex<()>,
}

impl fmt::Debug for FileSecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileSecretStore")
            .field("dir", &self.dir)
            .field("vault_path", &self.vault_path)
            .finish_non_exhaustive()
    }
}

impl FileSecretStore {
    /// Open or create `dir/master.key` and `dir/secrets.json`.
    pub fn open(dir: impl Into<PathBuf>) -> DriverResult<Self> {
        let dir = dir.into();
        Self::open_vault(dir.clone(), dir.join("secrets.json"))
    }

    /// Open or create a JSON or TOML vault (chosen by file extension).
    pub fn open_vault(dir: PathBuf, vault_path: PathBuf) -> DriverResult<Self> {
        fs::create_dir_all(&dir)?;
        set_dir_mode(&dir, 0o700)?;
        let master_path = dir.join("master.key");
        let master = load_or_create_master(&master_path)?;
        if !vault_path.exists() {
            let empty = VaultFile {
                version: VAULT_VERSION,
                secrets: BTreeMap::new(),
            };
            write_private(&vault_path, &encode_vault(&vault_path, &empty)?)?;
        }
        set_file_mode(&vault_path, 0o600)?;
        Ok(Self {
            dir,
            vault_path,
            master,
            lock: Mutex::new(()),
        })
    }

    /// Data directory that holds the master key.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Insert or replace a named secret.
    pub fn put(&self, name: &str, secret: &[u8]) -> DriverResult<()> {
        validate_secret_name(name)?;
        if secret.is_empty() {
            return Err(DriverError::CredentialUnavailable(
                "refusing to store an empty secret".into(),
            ));
        }
        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
        let mut vault = self.load_unlocked()?;
        vault
            .secrets
            .insert(name.to_string(), encrypt(self.master, secret)?);
        write_private(&self.vault_path, &encode_vault(&self.vault_path, &vault)?)?;
        info!(name, "secret store put");
        Ok(())
    }

    /// Names only; values stay encrypted on disk.
    pub fn list_names(&self) -> DriverResult<Vec<String>> {
        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
        let vault = self.load_unlocked()?;
        Ok(vault.secrets.keys().cloned().collect())
    }

    /// Fetch a named secret. Never log the return value.
    pub fn get(&self, name: &str) -> DriverResult<Secret> {
        self.get_named(name)
    }

    /// Remove a named secret. Returns whether it was present.
    pub fn delete(&self, name: &str) -> DriverResult<bool> {
        validate_secret_name(name)?;
        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
        let mut vault = self.load_unlocked()?;
        let removed = vault.secrets.remove(name).is_some();
        if removed {
            write_private(&self.vault_path, &encode_vault(&self.vault_path, &vault)?)?;
            info!(name, "secret store delete");
        }
        Ok(removed)
    }

    fn get_named(&self, name: &str) -> DriverResult<Secret> {
        validate_secret_name(name)?;
        let _guard = self.lock.lock().unwrap_or_else(|err| err.into_inner());
        let vault = self.load_unlocked()?;
        let stored = vault.secrets.get(name).ok_or_else(|| {
            DriverError::CredentialUnavailable(format!("store:{name} is not present"))
        })?;
        let plain = decrypt(self.master, stored)?;
        Ok(Secret::new(plain))
    }

    fn load_unlocked(&self) -> DriverResult<VaultFile> {
        let bytes = fs::read(&self.vault_path)?;
        decode_vault(&self.vault_path, &bytes)
    }
}

#[async_trait]
impl SecretBroker for FileSecretStore {
    async fn resolve(&self, secret_ref: &SecretRef) -> DriverResult<Secret> {
        let Some(name) = secret_ref.store_name() else {
            return Err(DriverError::CredentialUnavailable(
                "FileSecretStore only resolves store:NAME refs".into(),
            ));
        };
        self.get_named(name)
    }
}

/// S7: the vault master key decrypts every stored secret, so wipe it on drop
/// rather than leaving 32 bytes of key material in freed memory.
impl Drop for FileSecretStore {
    fn drop(&mut self) {
        self.master.zeroize();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultFile {
    version: u32,
    secrets: BTreeMap<String, StoredSecret>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSecret {
    nonce: String,
    ct: String,
}

fn encode_vault(path: &Path, vault: &VaultFile) -> DriverResult<Vec<u8>> {
    if is_toml(path) {
        toml::to_string_pretty(vault)
            .map(String::into_bytes)
            .map_err(|err| DriverError::CredentialUnavailable(format!("vault toml: {err}")))
    } else {
        Ok(serde_json::to_vec_pretty(vault)?)
    }
}

fn decode_vault(path: &Path, bytes: &[u8]) -> DriverResult<VaultFile> {
    let vault: VaultFile = if is_toml(path) {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| DriverError::CredentialUnavailable("vault is not utf-8".into()))?;
        toml::from_str(text)
            .map_err(|err| DriverError::CredentialUnavailable(format!("vault toml: {err}")))?
    } else {
        serde_json::from_slice(bytes)?
    };
    if vault.version != VAULT_VERSION {
        return Err(DriverError::CredentialUnavailable(format!(
            "unsupported vault version {}",
            vault.version
        )));
    }
    Ok(vault)
}

fn is_toml(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("toml")
}

fn encrypt(master: [u8; MASTER_LEN], plaintext: &[u8]) -> DriverResult<StoredSecret> {
    let cipher = ChaCha20Poly1305::new(&Key::from(master));
    let mut nonce_bytes = [0_u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from(nonce_bytes);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|_| DriverError::CredentialUnavailable("encrypt failed".into()))?;
    Ok(StoredSecret {
        nonce: hex_encode(&nonce_bytes),
        ct: hex_encode(&ct),
    })
}

fn decrypt(master: [u8; MASTER_LEN], stored: &StoredSecret) -> DriverResult<Vec<u8>> {
    let nonce_bytes = hex_decode(&stored.nonce)?;
    if nonce_bytes.len() != NONCE_LEN {
        return Err(DriverError::CredentialUnavailable(
            "invalid ciphertext".into(),
        ));
    }
    let ct = hex_decode(&stored.ct)?;
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes
        .as_slice()
        .try_into()
        .map_err(|_| DriverError::CredentialUnavailable("invalid ciphertext".into()))?;
    let cipher = ChaCha20Poly1305::new(&Key::from(master));
    let nonce = Nonce::from(nonce_arr);
    cipher
        .decrypt(&nonce, ct.as_ref())
        .map_err(|_| DriverError::CredentialUnavailable("invalid ciphertext".into()))
}

fn load_or_create_master(path: &Path) -> DriverResult<[u8; MASTER_LEN]> {
    if path.is_file() {
        let bytes = fs::read(path)?;
        if bytes.len() != MASTER_LEN {
            return Err(DriverError::CredentialUnavailable(
                "master.key has the wrong length".into(),
            ));
        }
        let mut key = [0_u8; MASTER_LEN];
        key.copy_from_slice(&bytes);
        set_file_mode(path, 0o600)?;
        return Ok(key);
    }
    let mut key = [0_u8; MASTER_LEN];
    OsRng.fill_bytes(&mut key);
    write_private(path, &key)?;
    Ok(key)
}

fn validate_secret_name(name: &str) -> DriverResult<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(DriverError::CredentialUnavailable(
            "secret name must be 1..=128 bytes".into(),
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
    {
        return Err(DriverError::CredentialUnavailable(
            "secret name may contain only [A-Za-z0-9._-]".into(),
        ));
    }
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(TABLE[(byte >> 4) as usize] as char);
        out.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> DriverResult<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return Err(DriverError::CredentialUnavailable(
            "invalid ciphertext".into(),
        ));
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = hex_nibble(bytes[index])?;
        let lo = hex_nibble(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> DriverResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(DriverError::CredentialUnavailable(
            "invalid ciphertext".into(),
        )),
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_mode(parent, 0o700)?;
    }
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
    set_file_mode(path, 0o600)?;
    Ok(())
}

fn set_dir_mode(path: &Path, mode: u32) -> DriverResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, mode);
    Ok(())
}

fn set_file_mode(path: &Path, mode: u32) -> DriverResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    let _ = (path, mode);
    Ok(())
}

/// macOS `security` CLI generic-password backend.
#[cfg(all(target_os = "macos", feature = "keychain"))]
#[derive(Debug, Clone)]
pub struct KeychainSecretBroker {
    /// `-s` service. Defaults to `remuda`.
    pub service: String,
}

#[cfg(all(target_os = "macos", feature = "keychain"))]
impl Default for KeychainSecretBroker {
    fn default() -> Self {
        Self {
            service: DEFAULT_KEYCHAIN_SERVICE.into(),
        }
    }
}

#[cfg(all(target_os = "macos", feature = "keychain"))]
impl KeychainSecretBroker {
    /// Store or update a generic password. Password is passed to `security -w`.
    pub async fn put(&self, account: &str, secret: &str) -> DriverResult<()> {
        validate_secret_name(account)?;
        let output = tokio::process::Command::new("security")
            .args([
                "add-generic-password",
                "-s",
                &self.service,
                "-a",
                account,
                "-w",
                secret,
                "-U",
            ])
            .output()
            .await?;
        if output.status.success() {
            info!(account, "keychain put");
            Ok(())
        } else {
            Err(DriverError::CredentialUnavailable(
                "keychain store failed".into(),
            ))
        }
    }
}

#[cfg(all(target_os = "macos", feature = "keychain"))]
#[async_trait]
impl SecretBroker for KeychainSecretBroker {
    async fn resolve(&self, secret_ref: &SecretRef) -> DriverResult<Secret> {
        let Some((service, account)) = secret_ref.keychain_spec() else {
            return Err(DriverError::CredentialUnavailable(
                "KeychainSecretBroker only resolves keychain: refs".into(),
            ));
        };
        validate_secret_name(account)?;
        let service = if service == DEFAULT_KEYCHAIN_SERVICE {
            self.service.as_str()
        } else {
            service
        };
        let output = tokio::process::Command::new("security")
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(DriverError::CredentialUnavailable(
                "keychain lookup failed".into(),
            ));
        }
        let mut bytes = output.stdout;
        while bytes
            .last()
            .is_some_and(|b| *b == b'\n' || *b == b'\r' || *b == b' ')
        {
            bytes.pop();
        }
        if bytes.is_empty() {
            return Err(DriverError::CredentialUnavailable(
                "keychain item is empty".into(),
            ));
        }
        Ok(Secret::new(bytes))
    }
}

/// Allowlisted UDS broker in front of another [`SecretBroker`].
#[derive(Clone)]
pub struct TokenBroker {
    inner: Arc<dyn SecretBroker>,
    /// instance id → per-instance token. Empty map denies every request.
    allowlist: Arc<Mutex<HashMap<String, String>>>,
    audit_path: PathBuf,
}

impl fmt::Debug for TokenBroker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenBroker")
            .field("audit_path", &self.audit_path)
            .finish_non_exhaustive()
    }
}

impl TokenBroker {
    /// `inner` does the actual resolve. Empty allowlist denies every instance.
    pub fn new(inner: Arc<dyn SecretBroker>, audit_path: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            allowlist: Arc::new(Mutex::new(HashMap::new())),
            audit_path: audit_path.into(),
        }
    }

    /// 32-byte hex token for a helper script. Never log the return value.
    #[must_use]
    pub fn random_token() -> String {
        let mut bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut bytes);
        hex_encode(&bytes)
    }

    /// Permit `instance_id` when the helper presents `token`.
    pub fn allow_instance(&self, instance_id: impl Into<String>, token: impl Into<String>) {
        let mut guard = self.allowlist.lock().unwrap_or_else(|err| err.into_inner());
        guard.insert(instance_id.into(), token.into());
    }

    /// Generate a token, store it, and return it. Never log the return value.
    pub fn issue_instance(&self, instance_id: impl Into<String>) -> String {
        let token = Self::random_token();
        self.allow_instance(instance_id, token.clone());
        token
    }

    /// Revoke a previously allowed instance.
    pub fn deny_instance(&self, instance_id: &str) {
        let mut guard = self.allowlist.lock().unwrap_or_else(|err| err.into_inner());
        guard.remove(instance_id);
    }

    /// True when `instance_id` presents the stored token.
    #[must_use]
    pub fn is_allowed(&self, instance_id: &str, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let guard = self.allowlist.lock().unwrap_or_else(|err| err.into_inner());
        guard
            .get(instance_id)
            .is_some_and(|expected| token_matches(expected, token))
    }

    /// Resolve if the instance token matches. Writes an audit line either way.
    pub async fn resolve_for(
        &self,
        instance_id: &str,
        token: &str,
        secret_ref: &SecretRef,
    ) -> DriverResult<Secret> {
        if !self.is_allowed(instance_id, token) {
            self.audit(instance_id, secret_ref.as_str(), "deny")?;
            return Err(DriverError::CredentialUnavailable(
                "token broker denied the request".into(),
            ));
        }
        match self.inner.resolve(secret_ref).await {
            Ok(secret) => {
                self.audit(instance_id, secret_ref.as_str(), "ok")?;
                Ok(secret)
            }
            Err(err) => {
                self.audit(instance_id, secret_ref.as_str(), "error")?;
                Err(err)
            }
        }
    }

    fn audit(&self, instance_id: &str, secret_ref: &str, outcome: &str) -> DriverResult<()> {
        info!(instance_id, secret_ref, outcome, "token broker");
        if let Some(parent) = self.audit_path.parent() {
            fs::create_dir_all(parent)?;
            set_dir_mode(parent, 0o700)?;
        }
        let line = serde_json::json!({
            "ts": now_rfc3339(),
            "event": "resolve",
            "secretRef": secret_ref,
            "instanceId": instance_id,
            "outcome": outcome,
        });
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&self.audit_path)?;
        writeln!(file, "{line}")?;
        set_file_mode(&self.audit_path, 0o600)?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct BrokerRequest {
    #[serde(rename = "instanceId")]
    instance_id: String,
    /// Per-instance bearer. Never logged.
    #[serde(default)]
    token: String,
    #[serde(rename = "secretRef")]
    secret_ref: String,
}

/// Hand-written so the bearer cannot reach a log through `{:?}` (S6).
///
/// The derive that used to sit here was the landmine `BrokerResponse` and
/// `TokenBrokerBind` were given redacting impls to avoid.
impl fmt::Debug for BrokerRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerRequest")
            .field("instance_id", &self.instance_id)
            .field("token", &"[redacted]")
            .field("secret_ref", &self.secret_ref)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct BrokerResponse {
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl fmt::Debug for BrokerResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerResponse")
            .field("ok", &self.ok)
            .field("secret", &self.secret.as_ref().map(|_| "[redacted]"))
            .field("error", &self.error)
            .finish()
    }
}

/// Serve JSON-line requests on a Unix socket (mode `0600`).
///
/// The chooser applies the same AF_UNIX placement rule as the hook socket:
/// when `socket_path` is too long for `sun_path` the broker binds a short
/// hashed name in the per-user runtime directory and leaves a symlink at
/// `socket_path`. Clients cannot connect through a long symlink (the limit
/// applies to connect as well as bind), so callers with long paths derive the
/// real address with [`crate::launch::place_token_broker_socket`] and bake
/// that into the `apiKeyHelper` bind; the digest is deterministic, so the
/// caller's placement and this one resolve identically.
#[cfg(unix)]
pub async fn serve_token_broker(broker: TokenBroker, socket_path: &Path) -> DriverResult<()> {
    use tokio::net::UnixListener;

    let placement = crate::launch::place_token_broker_socket(socket_path)?;
    let real_path = placement.bind_path();
    if real_path.exists() {
        fs::remove_file(real_path)?;
    }
    if let Some(parent) = real_path.parent() {
        fs::create_dir_all(parent)?;
        set_dir_mode(parent, 0o700)?;
    }
    let listener = UnixListener::bind(real_path).map_err(|error| {
        DriverError::CredentialUnavailable(format!(
            "token broker could not bind: {}",
            remuda_signal::runtime_dir::bind_io_error(real_path, error)
        ))
    })?;
    placement.install_link().map_err(|error| {
        DriverError::CredentialUnavailable(format!(
            "token broker discovery link could not be created: {error}"
        ))
    })?;
    set_file_mode(real_path, 0o600)?;
    info!(path = %real_path.display(), "token broker listening");
    let result = loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let broker = broker.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_broker_conn(broker, stream).await {
                        warn!(error = %err, "token broker connection");
                    }
                });
            }
            Err(error) => break Err::<(), _>(error),
        }
    };
    placement.remove_link();
    let _ = fs::remove_file(real_path);
    result?;
    Ok(())
}

#[cfg(unix)]
async fn handle_broker_conn(
    broker: TokenBroker,
    stream: tokio::net::UnixStream,
) -> DriverResult<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    let Some(line) = lines.next_line().await? else {
        return Ok(());
    };
    if line.len() > 64 * 1024 {
        return Err(DriverError::CredentialUnavailable(
            "token broker request is too large".into(),
        ));
    }
    let request: BrokerRequest = serde_json::from_str(&line)
        .map_err(|_| DriverError::CredentialUnavailable("invalid broker request".into()))?;
    let parsed = SecretRef::parse(request.secret_ref)?;
    let response = match broker
        .resolve_for(&request.instance_id, &request.token, &parsed)
        .await
    {
        Ok(secret) => BrokerResponse {
            ok: true,
            secret: Some(secret.expose_str()?.to_string()),
            error: None,
        },
        Err(err) => BrokerResponse {
            ok: false,
            secret: None,
            error: Some(err.to_string()),
        },
    };
    let mut payload = serde_json::to_vec(&response)?;
    payload.push(b'\n');
    writer.write_all(&payload).await?;
    Ok(())
}

/// Fetch one secret from a running token broker. The secret is only in the return value.
#[cfg(unix)]
pub async fn request_secret(
    socket_path: &Path,
    instance_id: &str,
    token: &str,
    secret_ref: &SecretRef,
) -> DriverResult<Secret> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).await?;
    let request = BrokerRequest {
        instance_id: instance_id.to_string(),
        token: token.to_string(),
        secret_ref: secret_ref.as_str().to_string(),
    };
    let mut payload = serde_json::to_vec(&request)?;
    payload.push(b'\n');
    stream.write_all(&payload).await?;
    let mut lines = BufReader::new(stream).lines();
    let line = lines.next_line().await?.ok_or_else(|| {
        DriverError::CredentialUnavailable("token broker closed the socket".into())
    })?;
    let response: BrokerResponse = serde_json::from_str(&line)
        .map_err(|_| DriverError::CredentialUnavailable("invalid broker response".into()))?;
    if response.ok {
        let secret = response.secret.ok_or_else(|| {
            DriverError::CredentialUnavailable("broker omitted the secret".into())
        })?;
        Ok(Secret::new(secret.into_bytes()))
    } else {
        Err(DriverError::CredentialUnavailable(
            response.error.unwrap_or_else(|| "broker denied".into()),
        ))
    }
}

/// SHA-256 prefix (16 hex chars) and last four UTF-8 characters of a secret.
///
/// Used by Hub GET `/v1/providers` so the token itself is never returned.
#[must_use]
pub fn fingerprint_secret(secret: &[u8]) -> (String, String) {
    let digest = Sha256::digest(secret);
    let fingerprint = hex_encode(&digest[..8]);
    (fingerprint, utf8_last4(secret))
}

fn utf8_last4(secret: &[u8]) -> String {
    match std::str::from_utf8(secret) {
        Ok(text) if !text.is_empty() => {
            let mut chars: Vec<char> = text.chars().collect();
            if chars.len() > 4 {
                chars = chars.split_off(chars.len() - 4);
            }
            chars.into_iter().collect()
        }
        _ if secret.len() >= 2 => hex_encode(&secret[secret.len() - 2..]),
        _ => "****".into(),
    }
}

fn token_matches(expected: &str, presented: &str) -> bool {
    let left = Sha256::digest(expected.as_bytes());
    let right = Sha256::digest(presented.as_bytes());
    let mut acc = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        acc |= a ^ b;
    }
    acc == 0
}

fn now_rfc3339() -> String {
    let t = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// S6: the derive that used to sit on `BrokerRequest` printed the bearer,
    /// directly under a "Never logged" comment — the landmine `BrokerResponse`
    /// and `TokenBrokerBind` were given hand-written impls to avoid.
    #[test]
    fn broker_request_debug_redacts_the_bearer() {
        let request = BrokerRequest {
            instance_id: "ins_01993ab0-0000-7000-8000-000000000001".into(),
            token: "brk-bearer-that-must-not-be-logged".into(),
            secret_ref: "env:REMUDA_TEST_SECRET".into(),
        };
        let debug = format!("{request:?}");
        assert!(
            !debug.contains("brk-bearer-that-must-not-be-logged"),
            "{debug}"
        );
        assert!(debug.contains("[redacted]"), "{debug}");
        // The non-secret fields stay useful for diagnosis.
        assert!(
            debug.contains("ins_01993ab0-0000-7000-8000-000000000001"),
            "{debug}"
        );
        assert!(debug.contains("env:REMUDA_TEST_SECRET"), "{debug}");
    }

    /// The redacting Debug must not change what goes on the wire.
    #[test]
    fn broker_request_still_serializes_the_token() {
        let request = BrokerRequest {
            instance_id: "ins_x".into(),
            token: "brk-wire-token".into(),
            secret_ref: "env:X".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("brk-wire-token"), "{json}");
        assert!(json.contains("\"instanceId\""), "{json}");
        assert!(json.contains("\"secretRef\""), "{json}");
    }
}
