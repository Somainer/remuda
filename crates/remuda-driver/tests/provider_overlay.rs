//! Claude gateway/direct settings overlay: 0600 file, env names, no log leak.

use remuda_driver::{
    ClaudeProviderOverlay, Delegation, Secret, claude_provider_settings_json,
    write_claude_provider_overlay,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn writes_gateway_overlay_0600_without_leaking_debug() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    fs::create_dir_all(&launch).unwrap();
    let secret = Secret::new(b"sk-fake-gateway-token-aaaa".to_vec());
    let extra = BTreeMap::new();
    let overlay = ClaudeProviderOverlay {
        delegation: Delegation::Gateway,
        base_url: "https://gateway.example/v1",
        model: "passthrough/auto",
        secret: &secret,
        extra_env: &extra,
    };
    let path = write_claude_provider_overlay(&launch, &overlay).unwrap();
    assert_eq!(path, launch.join("settings.json"));
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let text = fs::read_to_string(&path).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        json["env"]["ANTHROPIC_BASE_URL"],
        "https://gateway.example/v1"
    );
    assert_eq!(
        json["env"]["ANTHROPIC_AUTH_TOKEN"],
        "sk-fake-gateway-token-aaaa"
    );
    assert_eq!(
        json["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
        "1"
    );
    assert_eq!(json["model"], "passthrough/auto");
    assert!(!format!("{overlay:?}").contains("sk-fake-gateway-token-aaaa"));
    let settings = claude_provider_settings_json(&overlay).unwrap();
    assert_eq!(
        settings["env"]["ANTHROPIC_AUTH_TOKEN"],
        "sk-fake-gateway-token-aaaa"
    );
}

#[test]
fn writes_direct_overlay_with_api_key() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    fs::create_dir_all(&launch).unwrap();
    let secret = Secret::new(b"sk-direct-bbbb".to_vec());
    let extra = BTreeMap::new();
    let overlay = ClaudeProviderOverlay {
        delegation: Delegation::Direct,
        base_url: "",
        model: "claude-sonnet",
        secret: &secret,
        extra_env: &extra,
    };
    let path = write_claude_provider_overlay(&launch, &overlay).unwrap();
    let json: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(json["env"]["ANTHROPIC_API_KEY"], "sk-direct-bbbb");
    assert!(json["env"].get("ANTHROPIC_AUTH_TOKEN").is_none());
    assert!(json["env"].get("ANTHROPIC_BASE_URL").is_none());
}
