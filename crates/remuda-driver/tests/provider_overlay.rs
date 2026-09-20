//! Claude gateway/direct settings overlay: 0600 file, env names, no log leak.

use remuda_driver::launch::{merge_provider_overlay_over_user, redact_settings};
use remuda_driver::{
    ClaudeProviderOverlay, Delegation, Secret, apply_model_pin, claude_provider_settings_json,
    write_claude_provider_overlay,
};
use serde_json::{Value, json};
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

// ── D-047 via delivery: the overlay carries the relay, never the gateway ──

/// Synthetic gateway credential and proxy-host origin: the values a `via`
/// launch must keep off the worker host. The relay bearer is the only
/// credential W's overlay may hold.
const GATEWAY_TOKEN: &str = "sk-fake-gateway-0001";
const RELAY_BEARER: &str = "fake-relay-0002";
const LISTENER_URL: &str = "http://127.0.0.1:41317/v1";
const HOST_GATEWAY_URL: &str = "https://host-gateway.example/v1";
/// A credential the host user's own settings would carry in a real deployment.
const HOST_TOKEN: &str = "sk-fake-host-0003";
const OVERLAY_MODEL: &str = "passthrough/auto";
const PINNED_MODEL: &str = "model_hub/es1_orange_o50[1m]";
const HOST_MODEL: &str = "model_hub/host_default";

/// Build the exact overlay the Node writes on W for a `via` launch: gateway
/// delegation, but the URL is the per-instance loopback listener and the secret
/// is the minted per-instance relay bearer.
fn via_relay_overlay<'a>(
    secret: &'a Secret,
    extra: &'a BTreeMap<String, String>,
) -> ClaudeProviderOverlay<'a> {
    ClaudeProviderOverlay {
        delegation: Delegation::Gateway,
        base_url: LISTENER_URL,
        model: OVERLAY_MODEL,
        secret,
        extra_env: extra,
    }
}

/// D-047: the 0600 overlay on W points at the Node-local listener and carries
/// the per-instance relay bearer. The gateway credential is not an input to
/// this path, so it cannot be in the file, the Debug render, or the settings
/// JSON builder output.
#[test]
fn via_mode_writes_listener_url_and_relay_bearer_without_gateway_credential() {
    let tmp = tempfile::tempdir().unwrap();
    let launch = tmp.path().join("launch");
    fs::create_dir_all(&launch).unwrap();
    let bearer = Secret::new(RELAY_BEARER.as_bytes().to_vec());
    let extra = BTreeMap::new();
    let overlay = via_relay_overlay(&bearer, &extra);
    let path = write_claude_provider_overlay(&launch, &overlay).unwrap();
    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the relay overlay stays 0600");
    let text = fs::read_to_string(&path).unwrap();
    let json: Value = serde_json::from_str(&text).unwrap();

    // Listener URL + relay bearer are present, on the gateway env names.
    assert_eq!(json["env"]["ANTHROPIC_BASE_URL"], LISTENER_URL);
    assert_eq!(json["env"]["ANTHROPIC_AUTH_TOKEN"], RELAY_BEARER);
    assert!(json["env"].get("ANTHROPIC_API_KEY").is_none());
    assert_eq!(
        json["env"]["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
        "1"
    );
    assert_eq!(json["model"], OVERLAY_MODEL);

    // At the driver level the gateway token is structurally absent: it is not
    // an argument to this builder, so this assertion cannot fire here. It is
    // kept as a boundary marker; the load-bearing sweep is the node test
    // via_launch_artefacts_never_carry_the_gateway_token, where the delivered
    // token genuinely sits on the request.
    assert!(!text.contains(GATEWAY_TOKEN), "gateway token on W: {text}");
    assert!(
        !text.contains("gateway.example"),
        "gateway origin on W: {text}"
    );

    // Debug and the redacted settings view hide the bearer.
    assert!(!format!("{overlay:?}").contains(RELAY_BEARER));
    let redacted = redact_settings(&json);
    assert_eq!(
        redacted["env"]["ANTHROPIC_AUTH_TOKEN"],
        json!("[redacted]"),
        "the relay bearer rides the same env name and must redact like a token: {redacted}"
    );
    assert_eq!(
        redacted["env"]["ANTHROPIC_BASE_URL"], LISTENER_URL,
        "the loopback URL is not a secret and stays diagnostic"
    );

    // The builder API agrees with the file writer.
    let built = claude_provider_settings_json(&overlay).unwrap();
    assert_eq!(built["env"]["ANTHROPIC_AUTH_TOKEN"], RELAY_BEARER);
    assert_eq!(built["env"]["ANTHROPIC_BASE_URL"], LISTENER_URL);
}

/// D-047 plus gateway-carryover-1: merged on top of a host user's own settings
/// exactly like an ordinary gateway overlay, the relay overlay evicts every
/// host `ANTHROPIC_*` endpoint/credential key — the session can neither talk to
/// the host's gateway nor authenticate with its credential — and an explicit
/// model pin then wins over the overlay model and every host model variable.
#[test]
fn via_overlay_evicts_host_anthropic_keys_and_the_pin_wins() {
    let bearer = Secret::new(RELAY_BEARER.as_bytes().to_vec());
    let extra = BTreeMap::new();
    let overlay = via_relay_overlay(&bearer, &extra);
    let overlay_json = claude_provider_settings_json(&overlay).unwrap();

    // What a worker host's own ~/.claude/settings.json looks like before the
    // relay overlay is merged: its gateway, its credential, its default model.
    let host = json!({
        "model": HOST_MODEL,
        "env": {
            "ANTHROPIC_BASE_URL": HOST_GATEWAY_URL,
            "ANTHROPIC_AUTH_TOKEN": HOST_TOKEN,
            "ANTHROPIC_API_KEY": HOST_TOKEN,
            "ANTHROPIC_MODEL": HOST_MODEL,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": HOST_MODEL,
            "ANTHROPIC_SMALL_FAST_MODEL": HOST_MODEL,
            "CLAUDE_CODE_SUBAGENT_MODEL": HOST_MODEL,
            "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY": "0",
        },
        "theme": "dark",
    });

    let mut merged = merge_provider_overlay_over_user(&host, &overlay_json);

    // Endpoint and credential are the listener and the bearer — no host value
    // survives under any of the provider key names.
    let env = merged["env"].as_object().unwrap();
    assert_eq!(env["ANTHROPIC_BASE_URL"], json!(LISTENER_URL));
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], json!(RELAY_BEARER));
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        assert!(
            !env.contains_key(name),
            "{name} survived the merge: {env:#?}"
        );
    }
    assert_eq!(
        env["CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"],
        json!("1"),
        "the overlay's discovery flag replaces the host's"
    );
    // Host credential/origin are gone from the whole document.
    let rendered = serde_json::to_string(&merged).unwrap();
    assert!(
        !rendered.contains(HOST_TOKEN),
        "host credential survived: {rendered}"
    );
    assert!(
        !rendered.contains(HOST_GATEWAY_URL),
        "host gateway origin survived: {rendered}"
    );
    // Non-provider host config is untouched.
    assert_eq!(merged["theme"], json!("dark"));

    // Before the pin, the overlay model is in effect.
    assert_eq!(merged["model"], json!(OVERLAY_MODEL));
    // A dispatch pin outranks even the overlay model (model-pin-1).
    apply_model_pin(&mut merged, PINNED_MODEL);
    assert_eq!(merged["model"], json!(PINNED_MODEL));
    assert_eq!(
        merged["env"]["ANTHROPIC_BASE_URL"],
        json!(LISTENER_URL),
        "the pin changes only model keys: the relay route is intact"
    );
    assert_eq!(merged["env"]["ANTHROPIC_AUTH_TOKEN"], json!(RELAY_BEARER));
    let rendered = serde_json::to_string(&merged).unwrap();
    assert!(
        !rendered.contains(HOST_MODEL),
        "the host model id survived the pin: {rendered}"
    );
    assert!(
        !rendered.contains(OVERLAY_MODEL),
        "the pinned model must replace the overlay model: {rendered}"
    );
}

/// Redaction sweep over *every artefact* a `via` launch's settings handling
/// produces: the 0600 overlay, the merged launch settings, their redacted
/// render, and a log line built the way the native carrier logs the merge.
/// The synthetic gateway token (and the host's own credential) must appear in
/// none of them except where a credential legitimately lives — the 0600 files
/// — and there it must be the relay bearer, never the gateway value.
#[test]
fn no_launch_artefact_or_log_carries_the_gateway_token() {
    let root = tempfile::tempdir().unwrap();
    let launch = root.path().join("launch");
    fs::create_dir_all(&launch).unwrap();
    let bearer = Secret::new(RELAY_BEARER.as_bytes().to_vec());
    let extra = BTreeMap::new();
    let overlay = via_relay_overlay(&bearer, &extra);
    let overlay_path = write_claude_provider_overlay(&launch, &overlay).unwrap();

    let host = json!({
        "model": HOST_MODEL,
        "env": {
            "ANTHROPIC_BASE_URL": HOST_GATEWAY_URL,
            "ANTHROPIC_AUTH_TOKEN": HOST_TOKEN,
            "ANTHROPIC_MODEL": HOST_MODEL,
        },
    });
    let overlay_json: Value =
        serde_json::from_str(&fs::read_to_string(&overlay_path).unwrap()).unwrap();
    let mut merged = merge_provider_overlay_over_user(&host, &overlay_json);
    apply_model_pin(&mut merged, PINNED_MODEL);

    // Artefacts a launch leaves beside the overlay.
    fs::write(
        root.path().join("merged-settings.json"),
        serde_json::to_vec_pretty(&merged).unwrap(),
    )
    .unwrap();
    fs::write(
        root.path().join("merged-settings.redacted.json"),
        serde_json::to_vec_pretty(&redact_settings(&merged)).unwrap(),
    )
    .unwrap();
    // What a tracing event renders (shell_pty logs exactly this shape).
    let log_line = format!(
        "user settings merged into the launch overlay: {:?}",
        redact_settings(&merged)
    );
    fs::write(root.path().join("launch-debug.log"), log_line).unwrap();
    // A Debug of the overlay inputs must not expose the bearer either.
    fs::write(
        root.path().join("overlay-debug.txt"),
        format!("{overlay:?}"),
    )
    .unwrap();

    // Sweep every file under the root.
    let mut checked = 0usize;
    let mut entries = vec![root.path().to_path_buf()];
    while let Some(dir) = entries.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                entries.push(path);
                continue;
            }
            let bytes = fs::read(&path).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            // Vacuous at this layer — the gateway token never enters the
            // driver dataflow; kept as a boundary marker. The load-bearing
            // sweep is the node test
            // via_launch_artefacts_never_carry_the_gateway_token.
            assert!(
                !text.contains(GATEWAY_TOKEN),
                "gateway token reached artefact {}",
                path.display()
            );
            assert!(
                !text.contains(HOST_TOKEN),
                "host credential reached artefact {}",
                path.display()
            );
            assert!(
                !text.contains(HOST_GATEWAY_URL),
                "host gateway origin reached artefact {}",
                path.display()
            );
            // The relay bearer is allowed in the 0600 settings files only; it
            // must be redacted out of every log/debug/redacted artefact.
            let is_settings_artifact = path
                .file_name()
                .is_some_and(|name| name == "settings.json" || name == "merged-settings.json");
            if !is_settings_artifact {
                assert!(
                    !text.contains(RELAY_BEARER),
                    "relay bearer leaked into non-settings artefact {}",
                    path.display()
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 4, "the sweep must cover every produced artefact");
}
