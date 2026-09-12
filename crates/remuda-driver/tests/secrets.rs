//! File vault, token broker allowlist, and (ignored) Keychain tests.
//!
//! Vault fixtures are created at runtime under tempfile; they are not checked in.

use remuda_driver::{FileSecretStore, SecretBroker, SecretRef, TokenBroker};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

#[tokio::test]
async fn file_store_round_trip_json_and_hides_debug() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    let value = "sk-file-store-secret-value";
    store.put("anthropic", value.as_bytes()).unwrap();
    let secret = store
        .resolve(&SecretRef::parse("store:anthropic").unwrap())
        .await
        .unwrap();
    assert_eq!(secret.expose_str().unwrap(), value);
    assert!(!format!("{secret:?}").contains(value));
    assert!(!format!("{store:?}").contains(value));

    let master = fs::metadata(dir.path().join("master.key")).unwrap();
    let vault = fs::metadata(dir.path().join("secrets.json")).unwrap();
    assert_eq!(master.permissions().mode() & 0o777, 0o600);
    assert_eq!(vault.permissions().mode() & 0o777, 0o600);

    let raw = fs::read_to_string(dir.path().join("secrets.json")).unwrap();
    assert!(!raw.contains(value));
    assert_eq!(store.list_names().unwrap(), vec!["anthropic".to_string()]);
}

#[tokio::test]
async fn file_store_toml_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("secrets.toml");
    let store = FileSecretStore::open_vault(dir.path().to_path_buf(), vault.clone()).unwrap();
    store.put("codex", b"tok-toml-secret").unwrap();
    let secret = store
        .resolve(&SecretRef::parse("store:codex").unwrap())
        .await
        .unwrap();
    assert_eq!(secret.expose_str().unwrap(), "tok-toml-secret");
    let raw = fs::read_to_string(&vault).unwrap();
    assert!(raw.contains("nonce"));
    assert!(!raw.contains("tok-toml-secret"));
}

#[tokio::test]
async fn tampered_ciphertext_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    store.put("x", b"real-secret-bytes").unwrap();
    let path = dir.path().join("secrets.json");
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["secrets"]["x"]["ct"] = serde_json::json!("00");
    fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
    let err = store
        .resolve(&SecretRef::parse("store:x").unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid ciphertext"));
    assert!(!err.to_string().contains("real-secret"));
}

#[tokio::test]
async fn token_broker_allowlist_and_audit_omit_secret() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    let value = "sk-broker-secret-value";
    store.put("gateway", value.as_bytes()).unwrap();
    let audit = dir.path().join("audit.jsonl");
    let broker = TokenBroker::new(Arc::new(store), audit.clone());
    let secret_ref = SecretRef::parse("store:gateway").unwrap();
    let denied = broker
        .resolve_for("ins_denied", &secret_ref)
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("not allowlisted"));

    broker.allow_instance("ins_ok");
    let secret = broker.resolve_for("ins_ok", &secret_ref).await.unwrap();
    assert_eq!(secret.expose_str().unwrap(), value);

    let log = fs::read_to_string(&audit).unwrap();
    assert!(log.contains("ins_denied"));
    assert!(log.contains("ins_ok"));
    assert!(log.contains("store:gateway"));
    assert!(!log.contains(value));
    let mode = fs::metadata(&audit).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[tokio::test]
async fn file_store_rejects_non_store_refs() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    let err = store
        .resolve(&SecretRef::parse("env:ANTHROPIC_API_KEY").unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("store:NAME"));
}

#[cfg(unix)]
#[tokio::test]
async fn token_broker_uds_helper_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    store.put("anthropic", b"sk-uds-secret").unwrap();
    let broker = TokenBroker::new(Arc::new(store), dir.path().join("audit.jsonl"));
    broker.allow_instance("ins_live");
    let sock = dir.path().join("broker.sock");
    let server = broker.clone();
    let sock_server = sock.clone();
    let task = tokio::spawn(async move {
        let _ = remuda_driver::serve_token_broker(server, &sock_server).await;
    });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let got = remuda_driver::request_secret(
        &sock,
        "ins_live",
        &SecretRef::parse("store:anthropic").unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(got.expose_str().unwrap(), "sk-uds-secret");
    let denied = remuda_driver::request_secret(
        &sock,
        "ins_other",
        &SecretRef::parse("store:anthropic").unwrap(),
    )
    .await
    .unwrap_err();
    assert!(denied.to_string().contains("not allowlisted"));
    task.abort();
}

#[cfg(all(target_os = "macos", feature = "keychain"))]
#[tokio::test]
#[ignore = "writes a generic password to the login keychain via `security`"]
async fn keychain_put_and_resolve() {
    let broker = remuda_driver::KeychainSecretBroker::default();
    let account = format!(
        "remuda-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    broker
        .put(&account, "sk-fake-keychain-test-value")
        .await
        .unwrap();
    let secret = broker
        .resolve(&SecretRef::parse(format!("keychain:{account}")).unwrap())
        .await
        .unwrap();
    assert_eq!(secret.expose_str().unwrap(), "sk-fake-keychain-test-value");
    let _ = tokio::process::Command::new("security")
        .args(["delete-generic-password", "-s", "remuda", "-a", &account])
        .output()
        .await;
}
