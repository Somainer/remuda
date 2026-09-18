//! D-036 / model-pin-1: a model pin outranks the host's own settings, even
//! under delegation `none`.
//!
//! The 2026-09-18 demo ran every worker on the host's default model while the
//! dispatch records, the Hub roster and the provider audit all said the pin had
//! been honoured. Two channels were closed at once on the native shell-pty path:
//! the materializer emitted no `--model` argv (covered in `materializer.rs`),
//! and the settings merge left the launching user's own `model` key and
//! `ANTHROPIC_MODEL` in place because the host-layer strip only ran for
//! `gateway`/`direct`. This file covers the second one.
//!
//! The scope is deliberately narrow: only keys that *name a model* change
//! hands. Under `none` the host still owns the endpoint and the credential —
//! 跟随主机 keeps meaning that — so the base URL and token survive untouched.

use remuda_driver::{
    apply_model_pin, is_model_env, load_effective_user_settings, merge_settings_layers,
};
use serde_json::{Value, json};

/// A synthetic host id, in the shape a gateway catalog actually lists (a
/// namespaced id with a `[1m]` context variant), so the assertions exercise the
/// suffix handling rather than a bare alias.
const HOST_MODEL: &str = "model_hub/es1_orange_o48[1m]";
const PIN: &str = "model_hub/es1_orange_o50[1m]";

/// Seed a fake user settings home the way a launching user's `~/.claude` looks:
/// a `model` key plus provider env, including the model-naming variables that
/// outrank that key inside Claude Code.
fn seed_user_home(dir: &std::path::Path) -> Value {
    std::fs::create_dir_all(dir).unwrap();
    let settings = json!({
        "model": HOST_MODEL,
        "env": {
            "ANTHROPIC_MODEL": HOST_MODEL,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": HOST_MODEL,
            "ANTHROPIC_SMALL_FAST_MODEL": "model_hub/es1_orange_o47",
            "CLAUDE_CODE_SUBAGENT_MODEL": HOST_MODEL,
            // Endpoint + credential: the host keeps these under `none`.
            "ANTHROPIC_BASE_URL": "https://host-gateway.example/v1",
            "ANTHROPIC_AUTH_TOKEN": "sk-host-token-placeholder",
        },
        // Unrelated host config must survive the strip untouched.
        "statusLine": {"type": "command", "command": "host-statusline"},
        "theme": "dark",
    });
    std::fs::write(
        dir.join("settings.json"),
        serde_json::to_vec_pretty(&settings).unwrap(),
    )
    .unwrap();
    settings
}

/// The merged overlay a `none`-delegation launch hands the CLI must not carry
/// any host value that names a model once a pin exists.
#[test]
fn a_pin_evicts_every_host_model_key_under_delegation_none() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("user-home");
    seed_user_home(&home);

    // Exactly what the native carrier does for `none`: the host layer is the
    // base (no provider strip), then the pin is applied on top.
    let user = load_effective_user_settings(&home).unwrap().unwrap();
    let mut merged = merge_settings_layers(&user, &json!({}));
    apply_model_pin(&mut merged, PIN);

    // The one key Claude reads for a model says the pin, verbatim suffix and all.
    assert_eq!(merged["model"], json!(PIN));

    // Every model-naming env variable is gone: each outranks the `model` key,
    // so leaving one would let the host win while the overlay looked correct.
    let env = merged["env"].as_object().unwrap();
    for name in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_SMALL_FAST_MODEL",
        "CLAUDE_CODE_SUBAGENT_MODEL",
    ] {
        assert!(
            !env.contains_key(name),
            "{name} still carries a host model: {env:#?}"
        );
    }
    // No host model id survives anywhere in the document.
    let text = serde_json::to_string(&merged).unwrap();
    assert!(
        !text.contains(HOST_MODEL),
        "a host model id survived the merge: {text}"
    );

    // Delegation `none` still means the host owns where it talks and as whom.
    assert_eq!(
        env["ANTHROPIC_BASE_URL"],
        json!("https://host-gateway.example/v1")
    );
    assert_eq!(
        env["ANTHROPIC_AUTH_TOKEN"],
        json!("sk-host-token-placeholder")
    );
    // And everything unrelated is untouched.
    assert_eq!(merged["theme"], json!("dark"));
    assert_eq!(merged["statusLine"]["command"], json!("host-statusline"));
}

/// Rule 3: with no pin, nothing changes. The host's model answers exactly as it
/// did before D-036 — this brief added a channel for an explicit choice, not a
/// new default.
#[test]
fn without_a_pin_the_host_model_is_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("user-home");
    seed_user_home(&home);

    let user = load_effective_user_settings(&home).unwrap().unwrap();
    let merged = merge_settings_layers(&user, &json!({}));

    assert_eq!(merged["model"], json!(HOST_MODEL));
    assert_eq!(merged["env"]["ANTHROPIC_MODEL"], json!(HOST_MODEL));
}

/// An empty or whitespace pin is not a pin: it must not blank the host's model
/// and leave the session with no answer at all.
#[test]
fn a_blank_pin_is_not_a_pin() {
    let mut settings = json!({"model": HOST_MODEL, "env": {"ANTHROPIC_MODEL": HOST_MODEL}});
    apply_model_pin(&mut settings, "   ");
    assert_eq!(settings["model"], json!(HOST_MODEL));
    assert_eq!(settings["env"]["ANTHROPIC_MODEL"], json!(HOST_MODEL));
}

/// A host that spelled the variable in lower case still exports it, so the
/// eviction matches case-insensitively — the same rule the provider strip uses.
#[test]
fn model_env_matching_ignores_case_and_spares_endpoint_variables() {
    assert!(is_model_env("anthropic_model"));
    assert!(is_model_env("ANTHROPIC_MODEL"));
    assert!(is_model_env("Claude_Code_Subagent_Model"));
    // Endpoint/credential variables are not a model pin's business.
    assert!(!is_model_env("ANTHROPIC_BASE_URL"));
    assert!(!is_model_env("ANTHROPIC_AUTH_TOKEN"));
    assert!(!is_model_env("ANTHROPIC_API_KEY"));

    let mut settings = json!({"env": {"anthropic_model": HOST_MODEL}});
    apply_model_pin(&mut settings, PIN);
    assert!(
        settings["env"].as_object().unwrap().is_empty(),
        "lower-case spelling survived: {settings:#?}"
    );
    assert_eq!(settings["model"], json!(PIN));
}

/// The pin applies even when the host had no settings at all: the merge starts
/// from nothing and still produces a document naming the pinned model.
#[test]
fn a_pin_applies_to_an_absent_host_layer() {
    let mut settings = json!({});
    apply_model_pin(&mut settings, PIN);
    assert_eq!(settings["model"], json!(PIN));
}
