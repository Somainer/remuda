//! Tests for shadow-home materialization and the codex trust-hash algorithm.

use super::*;
use serde_json::Value;
use std::path::{Path, PathBuf};

struct TestOpts {
    launch_dir: PathBuf,
    socket: PathBuf,
}

impl TestOpts {
    fn new(root: &Path) -> Self {
        Self {
            launch_dir: root.join("launch"),
            socket: root.join("hook.sock"),
        }
    }
    fn opts(&self) -> ShadowOptions<'_> {
        ShadowOptions {
            launch_dir: &self.launch_dir,
            relay_binary: Path::new("/opt/remuda/bin/remuda"),
            socket_path: &self.socket,
        }
    }
}

fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn codex_shadow_writes_config_with_inline_trust_and_hooks() {
    let root = tempfile::tempdir().unwrap();
    let t = TestOpts::new(root.path());
    let shadow = materialize_codex(&t.opts()).unwrap();
    assert_eq!(
        shadow.env,
        vec![(
            "CODEX_HOME".to_owned(),
            shadow.home.to_string_lossy().into_owned()
        )]
    );
    let config = std::fs::read_to_string(shadow.home.join("config.toml")).unwrap();
    assert!(config.contains("[features]"));
    assert!(config.contains("hooks = true"));
    // Trust state lives IN config.toml under [hooks.state …] (measured).
    assert!(config.contains("[hooks.state."));
    assert!(config.contains(":session_start:0:0"));
    assert!(config.contains(":permission_request:0:0"));
    assert!(config.contains("trusted_hash = \"sha256:"));
    // No standalone hooks.state file: codex does not load it.
    assert!(!shadow.home.join("hooks.state").exists());
    // hooks.json carries both events with the relay command.
    let hooks = read(&shadow.home.join("hooks.json"));
    assert_eq!(hooks["hooks"].as_object().unwrap().len(), 2);
    for event in ["SessionStart", "PermissionRequest"] {
        let command = hooks["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(command.contains("hook emit"), "{event}");
        assert!(command.contains(&format!("--event '{event}'")), "{event}");
        assert!(command.contains("hook.sock"), "{event}");
        assert_eq!(
            hooks["hooks"][event][0]["hooks"][0]["timeout"].as_u64(),
            Some(CODEX_HOOK_TIMEOUT_SECS)
        );
    }
    // Audit files are recorded.
    assert_eq!(shadow.files.len(), 2);
}

#[test]
fn the_canonical_hash_matches_a_live_codex_currenthash() {
    // Ground truth, not a hand-derived vector: this currentHash was read from
    // `hooks/list` on the codex app-server installed on this host (0.147) for
    // hooks.json `{"type":"command","command":"echo hello","timeout":60}`
    // (native-pty-6 evidence notes). The trust hash must equal what the binary
    // itself reports, or writing hooks.state cannot flip trustStatus to
    // trusted. The canonical form is compact, recursively-key-sorted JSON of
    // {event_name, hooks:[{async:false, command, timeout, type:"command"}]}.
    let identity = serde_json::json!({
        "event_name": "permission_request",
        "hooks": [{
            "async": false,
            "command": "echo hello",
            "timeout": 60,
            "type": "command",
        }],
    });
    let expected = hex_hash(&identity);
    assert_eq!(
        expected,
        "sha256:5f44c900f8dd4d3e93fadabc3e027676f42cd92c9228c171b8427aa9117ca510"
    );
    assert_eq!(
        codex_trust_hash("permission_request", "echo hello", 60),
        expected
    );
}

#[test]
fn the_canonical_hash_includes_the_handler_type_tag() {
    // The `type:"command"` tag and the filled-in `async:false` default both
    // participate: dropping either changes the hash the binary reports.
    let command = "echo hello";
    let identity = serde_json::json!({
        "event_name": "permission_request",
        "hooks": [{
            "async": false,
            "command": command,
            "timeout": 60,
            "type": "command",
        }],
    });
    assert_eq!(
        codex_trust_hash("permission_request", command, 60),
        hex_hash(&identity)
    );
    let without_type = serde_json::json!({
        "event_name": "permission_request",
        "hooks": [{"async": false, "command": command, "timeout": 60}],
    });
    assert_ne!(
        codex_trust_hash("permission_request", command, 60),
        hex_hash(&without_type)
    );
}

/// Independent `sha256:` hex of a canonical identity, for cross-checking the
/// production hasher without calling it.
fn hex_hash(identity: &serde_json::Value) -> String {
    let digest = sha2::Sha256::digest(canonical_json(identity).as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256:{hex}")
}

#[test]
fn the_hash_tracks_the_command_and_is_order_independent() {
    assert_ne!(
        codex_trust_hash("permission_request", "echo a", 60),
        codex_trust_hash("permission_request", "echo b", 60),
        "changing the command must change the hash"
    );
    let direct = canonical_json(&serde_json::json!({
        "event_name": "permission_request",
        "hooks": [{"async": false, "command": "x", "timeout": 60, "type": "command"}]
    }));
    let reordered = canonical_json(&serde_json::json!({
        "hooks": [{"type": "command", "timeout": 60, "command": "x", "async": false}],
        "event_name": "permission_request"
    }));
    assert_eq!(direct, reordered);
    assert!(!direct.contains(", "), "compact JSON has no spaces");
}

#[test]
fn the_written_trust_hash_is_byte_for_byte_the_one_codex_would_report() {
    // The hashed command must be exactly the command written into hooks.json,
    // using the file's absolute path as the trust-key prefix.
    let root = tempfile::tempdir().unwrap();
    let t = TestOpts::new(root.path());
    let shadow = materialize_codex(&t.opts()).unwrap();
    let hooks_path = shadow.home.join("hooks.json");
    let hooks = read(&hooks_path);
    let command = hooks["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_owned();
    let want = codex_trust_hash("permission_request", &command, CODEX_HOOK_TIMEOUT_SECS);
    let config = std::fs::read_to_string(shadow.home.join("config.toml")).unwrap();
    assert!(
        config.contains(&want),
        "config.toml must persist the currentHash codex reports\n{config}"
    );
    assert!(config.contains(&hooks_path.to_string_lossy().to_string()));
}

#[test]
fn grok_shadow_neutralises_the_cross_read_claude_hooks() {
    let root = tempfile::tempdir().unwrap();
    let t = TestOpts::new(root.path());
    let shadow = materialize_grok(&t.opts()).unwrap();
    assert!(
        shadow
            .env
            .iter()
            .any(|(key, value)| key == "GROK_HOME" && value.ends_with("grok-home"))
    );
    assert!(
        shadow
            .env
            .iter()
            .any(|(key, value)| key == GROK_CLAUDE_HOOKS_ENV && value == "0"),
        "the Claude-hook cross-read must be switched off (§3.1 [V])"
    );
    let hooks = read(&shadow.home.join("hooks/remuda.json"));
    for event in GROK_EVENTS {
        assert!(hooks["hooks"][event].is_array(), "{event} registered");
    }
    assert!(
        hooks["hooks"].get("PermissionRequest").is_none(),
        "grok silently ignores PermissionRequest"
    );
}

#[test]
fn non_shadow_kinds_are_rejected_and_no_credential_is_written() {
    let root = tempfile::tempdir().unwrap();
    let t = TestOpts::new(root.path());
    assert!(materialize(AgentKind::Claude, &t.opts()).is_err());
    assert!(materialize(AgentKind::Agy, &t.opts()).is_err());
    materialize(AgentKind::Codex, &t.opts()).unwrap();
    materialize(AgentKind::Grok, &t.opts()).unwrap();
    for entry in walkdir(&t.launch_dir) {
        let body = std::fs::read_to_string(&entry).unwrap_or_default();
        assert!(!body.contains("REMUDA_HOOK_CREDENTIAL"), "{entry:?}");
        assert!(!body.contains("--credential"), "{entry:?}");
    }
}

#[cfg(unix)]
#[test]
fn shadow_files_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let t = TestOpts::new(root.path());
    for shadow in [
        materialize_codex(&t.opts()).unwrap(),
        materialize_grok(&t.opts()).unwrap(),
    ] {
        for file in &shadow.files {
            let mode = std::fs::metadata(&file.path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{:?}", file.path);
        }
    }
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
