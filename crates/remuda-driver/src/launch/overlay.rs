//! Per-session `--settings` overlay (D-028 §4.2 materializer, §9.2 pinning).
//!
//! One merged settings file per session, under the instance directory, holding
//! exactly two things: the hook registrations that point at the relay, and the
//! three terminal keys §9.2 requires be pinned. It is handed to the harness
//! with `--settings`, which layers *on top of* the user's own configuration
//! rather than replacing it.
//!
//! Three rules this module exists to keep:
//!
//! 1. **Merge, never overwrite.** If the caller passes an existing overlay we
//!    keep every key and every hook in it and append ours. A user who has
//!    their own `SessionStart` hook keeps it — verified against claude 2.1.270,
//!    where both hooks fire.
//! 2. **Never touch the user's own config.** The only file written is under
//!    `<instance dir>/launch/`. `~/.claude/settings.json` is read by the
//!    harness through `--setting-sources` and never by us.
//! 3. **Pin both directions.** `tui` is written whether it is `fullscreen` or
//!    `default`; §9.2 is explicit that leaving it unset lets the host's own
//!    settings leak in through the user layer and makes the renderer
//!    non-deterministic across machines.

use crate::binary::hash_bytes;
use crate::error::{DriverError, DriverResult};
use remuda_protocol::AgentKind;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Renderer pinned for the session (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiMode {
    /// Alt-screen TUI.
    Fullscreen,
    /// Inline rendering.
    Default,
}

impl TuiMode {
    /// Wire spelling written into the overlay.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fullscreen => "fullscreen",
            Self::Default => "default",
        }
    }
}

/// What the overlay needs to know to register the relay.
#[derive(Debug, Clone)]
pub struct OverlayOptions {
    /// `<instance dir>/launch`.
    pub launch_dir: PathBuf,
    /// The `remuda` binary the relay runs as.
    pub relay_binary: PathBuf,
    /// `<instance dir>/hook.sock`.
    pub socket_path: PathBuf,
    /// Renderer to pin. Comes from the launch request.
    pub tui: TuiMode,
    /// An existing overlay to merge into, when the caller already built one.
    pub base: Option<Value>,
}

/// A materialized overlay.
#[derive(Debug, Clone)]
pub struct HookOverlay {
    /// Absolute path to the written settings file (0600).
    pub path: PathBuf,
    /// Digest of the written bytes, for the launch audit.
    pub digest: remuda_protocol::Digest,
    /// Events registered against the relay.
    pub events: Vec<String>,
}

/// Hook events registered against the relay.
///
/// `PermissionRequest` and `Elicitation` are registered **observe-only** in
/// P1: the Node journals them and answers `{}`, so the agent keeps prompting
/// on its own screen and there is never a state where Remuda believes it
/// answered and the agent believes it did not. P5 makes them load-bearing.
pub const HOOK_EVENTS: &[&str] = remuda_signal_events();

const fn remuda_signal_events() -> &'static [&'static str] {
    &[
        "SessionStart",
        "UserPromptSubmit",
        "Stop",
        "StopFailure",
        "SessionEnd",
        "Notification",
        "PreToolUse",
        "PostToolUse",
        "PostToolBatch",
        "MessageDisplay",
        "PermissionRequest",
        "Elicitation",
    ]
}

/// Write the per-session overlay and return where it landed.
pub fn materialize_overlay(options: &OverlayOptions) -> DriverResult<HookOverlay> {
    std::fs::create_dir_all(&options.launch_dir)?;
    set_mode(&options.launch_dir, 0o700)?;
    let mut settings = options.base.clone().unwrap_or_else(|| json!({}));
    if !settings.is_object() {
        return Err(DriverError::SettingsIsolationUnavailable(
            "settings overlay is not an object".into(),
        ));
    }
    for event in HOOK_EVENTS {
        merge_hook(&mut settings, event, &relay_command(options, event))?;
    }
    pin_terminal_keys(&mut settings, options.tui)?;
    let bytes = serde_json::to_vec_pretty(&settings)?;
    let path = options.launch_dir.join("settings.json");
    write_private(&path, &bytes, 0o600)?;
    Ok(HookOverlay {
        path,
        digest: hash_bytes(&bytes)?,
        events: HOOK_EVENTS
            .iter()
            .map(|event| (*event).to_owned())
            .collect(),
    })
}

/// The shell command a hook runs.
///
/// The credential is deliberately *not* here: it reaches the relay through the
/// child environment, because a command line is world-readable via `ps`.
fn relay_command(options: &OverlayOptions, event: &str) -> String {
    format!(
        "{} hook emit --socket {} --event {}",
        quote(&options.relay_binary.to_string_lossy()),
        quote(&options.socket_path.to_string_lossy()),
        quote(event),
    )
}

/// Append one hook command under `event` without disturbing what is there.
///
/// Idempotent: re-materializing the same overlay does not stack duplicate
/// registrations, which would run the relay twice per event.
fn merge_hook(settings: &mut Value, event: &str, command: &str) -> DriverResult<()> {
    let object = settings.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("settings overlay is not an object".into())
    })?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks_object = hooks.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("hooks overlay is not an object".into())
    })?;
    let matchers = hooks_object
        .entry(event)
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| {
            DriverError::SettingsIsolationUnavailable(format!(
                "{event} hooks overlay is not an array"
            ))
        })?;
    let already = matchers.iter().any(|matcher| {
        matcher
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook.get("command").and_then(Value::as_str) == Some(command))
            })
    });
    if !already {
        matchers.push(json!({
            "hooks": [{ "type": "command", "command": command }]
        }));
    }
    Ok(())
}

/// Pin the three keys §9.2 requires.
///
/// These are set, not defaulted: `showStatusInTerminalTab` off means no OSC
/// title and `terminalProgressBarEnabled` off means no `OSC 9;4`, and each of
/// those is a whole layer of idle/working signal gone. A user who turned them
/// off in their own settings would otherwise silently remove it.
fn pin_terminal_keys(settings: &mut Value, tui: TuiMode) -> DriverResult<()> {
    let object = settings.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("settings overlay is not an object".into())
    })?;
    object.insert("tui".into(), json!(tui.as_str()));
    object.insert("showStatusInTerminalTab".into(), json!(true));
    object.insert("terminalProgressBarEnabled".into(), json!(true));
    Ok(())
}

/// Ensure `--settings <overlay>` and a non-empty `--setting-sources` are on
/// `argv`, replacing an existing `--settings` value rather than adding a second.
///
/// `--setting-sources` stays (§9.2): it is what makes the resolved
/// configuration deterministic. Its only cost is that in-session `/tui` is
/// refused, and the renderer is already decided at launch.
pub fn ensure_overlay_argv(argv: &mut Vec<String>, overlay: &Path) -> DriverResult<()> {
    let path = overlay.to_string_lossy().into_owned();
    if let Some(index) = argv.iter().position(|token| token == "--settings") {
        match argv.get_mut(index + 1) {
            Some(value) => *value = path,
            None => argv.push(path),
        }
    } else if let Some(index) = argv
        .iter()
        .position(|token| token.starts_with("--settings="))
    {
        argv[index] = format!("--settings={path}");
    } else {
        argv.push("--settings".into());
        argv.push(path);
    }
    if argv
        .iter()
        .any(|token| token == "--setting-sources" || token.starts_with("--setting-sources="))
    {
        return Ok(());
    }
    argv.push("--setting-sources".into());
    argv.push("user,project,local".into());
    Ok(())
}

/// Shadow config directory for a non-claude harness (§5.1).
///
/// Reserved, not implemented: codex reads `CODEX_HOME` and grok reads
/// `GROK_HOME`, and both need a populated shadow tree (`config.toml` with
/// `hooks = true` plus a `hooks.json` for codex; `hooks/*.json` for grok) that
/// P1 does not build. Creating the directory and stopping there would be worse
/// than not creating it: an empty `CODEX_HOME` loses the user's real config
/// without replacing it. So this returns the path it *would* use and writes
/// nothing, and P6 fills it in.
///
/// Grok additionally still reads `~/.claude/settings.json` hook entries even
/// with `GROK_HOME` redirected (design §3.1 [V]), so its hook scripts must
/// self-identify by harness before this becomes live.
#[must_use]
pub fn shadow_home(launch_dir: &Path, kind: AgentKind) -> Option<(&'static str, PathBuf)> {
    match kind {
        AgentKind::Codex => Some(("CODEX_HOME", launch_dir.join("codex-home"))),
        AgentKind::Grok => Some(("GROK_HOME", launch_dir.join("grok-home"))),
        _ => None,
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn write_private(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    set_mode(path, mode)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> DriverResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> DriverResult<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(dir: &Path) -> OverlayOptions {
        OverlayOptions {
            launch_dir: dir.join("launch"),
            relay_binary: PathBuf::from("/opt/remuda/bin/remuda"),
            socket_path: dir.join("hook.sock"),
            tui: TuiMode::Fullscreen,
            base: None,
        }
    }

    fn read(overlay: &HookOverlay) -> Value {
        serde_json::from_slice(&std::fs::read(&overlay.path).unwrap()).unwrap()
    }

    #[test]
    fn every_event_the_signal_bus_understands_is_registered() {
        let dir = tempfile::tempdir().unwrap();
        let overlay = materialize_overlay(&options(dir.path())).unwrap();
        let settings = read(&overlay);
        let hooks = settings["hooks"].as_object().unwrap();
        for event in HOOK_EVENTS {
            assert!(hooks.contains_key(*event), "{event} was not registered");
        }
    }

    #[test]
    fn the_users_own_hooks_survive_the_merge() {
        // A user who has their own SessionStart hook keeps it: the overlay is
        // additive, and claude runs both (verified against 2.1.270).
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        opts.base = Some(json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "/home/u/mine.sh"}]}]
            },
            "somethingTheUserSet": "keep me"
        }));
        let overlay = materialize_overlay(&opts).unwrap();
        let settings = read(&overlay);
        let rendered = settings["hooks"]["SessionStart"].to_string();
        assert!(rendered.contains("/home/u/mine.sh"), "{rendered}");
        assert!(rendered.contains("hook emit"), "{rendered}");
        assert_eq!(settings["somethingTheUserSet"], "keep me");
    }

    #[test]
    fn re_materializing_does_not_stack_duplicate_registrations() {
        // Otherwise the relay runs twice per event and the journal doubles.
        let dir = tempfile::tempdir().unwrap();
        let first = materialize_overlay(&options(dir.path())).unwrap();
        let mut opts = options(dir.path());
        opts.base = Some(read(&first));
        let second = materialize_overlay(&opts).unwrap();
        let settings = read(&second);
        assert_eq!(
            settings["hooks"]["SessionStart"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    fn the_three_terminal_keys_are_pinned_in_both_directions() {
        let dir = tempfile::tempdir().unwrap();
        for (mode, expected) in [
            (TuiMode::Fullscreen, "fullscreen"),
            (TuiMode::Default, "default"),
        ] {
            let mut opts = options(dir.path());
            opts.tui = mode;
            let settings = read(&materialize_overlay(&opts).unwrap());
            // Written even when it matches the harness default: an unset `tui`
            // lets the host's own settings decide (§9.2).
            assert_eq!(settings["tui"], expected);
            assert_eq!(settings["showStatusInTerminalTab"], json!(true));
            assert_eq!(settings["terminalProgressBarEnabled"], json!(true));
        }
    }

    #[test]
    fn a_user_who_disabled_the_osc_signals_has_them_re_enabled_for_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        opts.base = Some(json!({
            "showStatusInTerminalTab": false,
            "terminalProgressBarEnabled": false,
            "tui": "default",
        }));
        opts.tui = TuiMode::Fullscreen;
        let settings = read(&materialize_overlay(&opts).unwrap());
        assert_eq!(settings["showStatusInTerminalTab"], json!(true));
        assert_eq!(settings["terminalProgressBarEnabled"], json!(true));
        assert_eq!(settings["tui"], "fullscreen");
    }

    #[test]
    fn the_credential_is_never_written_into_the_hook_command() {
        // A command line is readable by every process on the machine.
        let dir = tempfile::tempdir().unwrap();
        let overlay = materialize_overlay(&options(dir.path())).unwrap();
        let body = std::fs::read_to_string(&overlay.path).unwrap();
        assert!(!body.contains("--credential"), "{body}");
        assert!(!body.contains("REMUDA_HOOK_CREDENTIAL"), "{body}");
    }

    #[cfg(unix)]
    #[test]
    fn the_overlay_and_its_directory_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let opts = options(dir.path());
        let overlay = materialize_overlay(&opts).unwrap();
        let file = std::fs::metadata(&overlay.path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let parent = std::fs::metadata(&opts.launch_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file, 0o600);
        assert_eq!(parent, 0o700);
    }

    #[test]
    fn a_path_with_a_quote_in_it_cannot_break_out_of_the_hook_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        opts.socket_path = PathBuf::from("/tmp/it's here/hook.sock");
        let overlay = materialize_overlay(&opts).unwrap();
        let settings = read(&overlay);
        let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            command.contains(r"'/tmp/it'\''s here/hook.sock'"),
            "{command}"
        );
    }

    #[test]
    fn overlay_argv_replaces_an_existing_settings_flag_rather_than_adding_a_second() {
        let mut argv = vec![
            "claude".to_owned(),
            "--settings".to_owned(),
            "/old.json".to_owned(),
        ];
        ensure_overlay_argv(&mut argv, Path::new("/new.json")).unwrap();
        assert_eq!(
            argv.iter().filter(|token| *token == "--settings").count(),
            1
        );
        assert!(argv.contains(&"/new.json".to_owned()));
        assert!(!argv.contains(&"/old.json".to_owned()));
    }

    #[test]
    fn overlay_argv_keeps_setting_sources_and_never_leaves_it_empty() {
        let mut argv = vec!["claude".to_owned()];
        ensure_overlay_argv(&mut argv, Path::new("/new.json")).unwrap();
        let index = argv
            .iter()
            .position(|token| token == "--setting-sources")
            .expect("setting sources must be present");
        assert_eq!(argv[index + 1], "user,project,local");

        // A caller's own choice is respected, not doubled.
        let mut argv = vec![
            "claude".to_owned(),
            "--setting-sources".to_owned(),
            "project".to_owned(),
        ];
        ensure_overlay_argv(&mut argv, Path::new("/new.json")).unwrap();
        assert_eq!(
            argv.iter()
                .filter(|token| *token == "--setting-sources")
                .count(),
            1
        );
        assert!(argv.contains(&"project".to_owned()));
    }

    #[test]
    fn shadow_homes_are_named_but_not_created_this_phase() {
        // An empty CODEX_HOME would lose the user's real config without
        // replacing it, which is worse than leaving the variable unset.
        let dir = tempfile::tempdir().unwrap();
        let (name, path) = shadow_home(dir.path(), AgentKind::Codex).unwrap();
        assert_eq!(name, "CODEX_HOME");
        assert!(
            !path.exists(),
            "P1 must not materialize a codex shadow home"
        );
        let (name, path) = shadow_home(dir.path(), AgentKind::Grok).unwrap();
        assert_eq!(name, "GROK_HOME");
        assert!(!path.exists());
        assert!(shadow_home(dir.path(), AgentKind::Claude).is_none());
    }
}
