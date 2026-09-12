//! File vault, token broker allowlist, and (ignored) Keychain tests.
//!
//! Vault fixtures are created at runtime under tempfile; they are not checked in.

use remuda_driver::{
    FileSecretStore, SecretBroker, SecretRef, TokenBroker, TokenBrokerBind,
    render_api_key_helper_script,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

#[tokio::test]
async fn file_store_round_trip_json_and_hides_debug() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileSecretStore::open(dir.path()).unwrap();
    let value = "sk-fake-file-store-value";
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

    let (fingerprint, last4) = remuda_driver::fingerprint_secret(value.as_bytes());
    assert_eq!(last4, "alue");
    assert_eq!(fingerprint.len(), 16);
    store.delete("anthropic").unwrap();
    assert!(store.list_names().unwrap().is_empty());
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
    let value = "sk-fake-broker-value";
    store.put("gateway", value.as_bytes()).unwrap();
    let audit = dir.path().join("audit.jsonl");
    let broker = TokenBroker::new(Arc::new(store), audit.clone());
    let secret_ref = SecretRef::parse("store:gateway").unwrap();
    let token = "tok_ok_instance_token_1";
    let denied = broker
        .resolve_for("ins_denied", token, &secret_ref)
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("denied the request"));

    broker.allow_instance("ins_ok", token);
    let wrong = broker
        .resolve_for("ins_ok", "tok_wrong_instance_token", &secret_ref)
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("denied the request"));

    let secret = broker
        .resolve_for("ins_ok", token, &secret_ref)
        .await
        .unwrap();
    assert_eq!(secret.expose_str().unwrap(), value);

    let log = fs::read_to_string(&audit).unwrap();
    assert!(log.contains("ins_denied"));
    assert!(log.contains("ins_ok"));
    assert!(log.contains("store:gateway"));
    assert!(!log.contains(value));
    assert!(!log.contains(token));
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
    store.put("anthropic", b"sk-fake-uds-secret").unwrap();
    let broker = TokenBroker::new(Arc::new(store), dir.path().join("audit.jsonl"));
    let token = broker.issue_instance("ins_live");
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
    let secret_ref = SecretRef::parse("store:anthropic").unwrap();
    let got = remuda_driver::request_secret(&sock, "ins_live", &token, &secret_ref)
        .await
        .unwrap();
    assert_eq!(got.expose_str().unwrap(), "sk-fake-uds-secret");
    let denied = remuda_driver::request_secret(&sock, "ins_other", &token, &secret_ref)
        .await
        .unwrap_err();
    assert!(denied.to_string().contains("denied the request"));

    let helper_path = dir.path().join("api-key-helper");
    let script = render_api_key_helper_script(
        &TokenBrokerBind {
            socket_path: sock.clone(),
            instance_id: "ins_live".into(),
            token: token.clone(),
        },
        &secret_ref,
    )
    .unwrap();
    assert!(!script.contains("sk-fake-uds-secret"));
    fs::write(&helper_path, &script).unwrap();
    fs::set_permissions(&helper_path, fs::Permissions::from_mode(0o700)).unwrap();
    let output = tokio::process::Command::new(&helper_path)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "sk-fake-uds-secret"
    );
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
