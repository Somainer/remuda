//! Seeding and detection for Claude Code's first-run onboarding.
//!
//! A Node-scoped `CLAUDE_CONFIG_DIR` starts empty, so the native CLI runs its
//! first-run flow (theme picker → security notes → terminal setup) before it
//! ever mounts a prompt composer. Herdr reports such a pane as *idle*, the
//! SessionStart hook never fires, and a queued prompt waits forever.
//!
//! Two defences, in order of preference:
//!
//! 1. [`seed_scoped_config`] writes the flags that gate the flow into the
//!    scoped directory before launch, so onboarding never starts. This mirrors
//!    what Claude Code's own `plugin eval` sandbox does for its throwaway
//!    config dir (it seeds `.claude.json` with `hasCompletedOnboarding`).
//! 2. [`startup_dialog`] recognises the screens themselves, so a pane that
//!    reaches one anyway is detected rather than typed into.
//!
//! Verified against the installed Claude Code **2.1.270** binary:
//!
//! | Fact | Where |
//! | --- | --- |
//! | `<CLAUDE_CONFIG_DIR>/.claude.json` is the global config | `globalConfig: join(CLAUDE_CONFIG_DIR ?? homedir(), ".claude.json")` |
//! | `<CLAUDE_CONFIG_DIR>/settings.json` is the user settings | `userSettings: join(CLAUDE_CONFIG_DIR ?? join(homedir(), ".claude"), "settings.json")` |
//! | `hasCompletedOnboarding` gates the whole flow | `if (config.hasCompletedOnboarding && …) return null;` before the `Onboarding` import |
//! | completing it writes `hasCompletedOnboarding` + `lastOnboardingVersion` | the `onDone` reducer |
//! | `theme` lives in user settings, not the global config | `saveTheme → set("userSettings", {theme})` |
//! | the bypass disclaimer is a separate gate | `if (skipDangerousModePermissionPrompt || config.bypassPermissionsModeAccepted) return;` |
//!
//! Nothing here ever reads or copies credentials: the copy is an explicit
//! key allowlist, and `.credentials.json`, `oauthAccount`, `userID` and
//! `projects` are all outside it.

use crate::{DriverError, DriverResult};
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Global-config keys copied from the host user, when present and truthy.
///
/// `hasCompletedOnboarding` is seeded unconditionally (see
/// [`seed_scoped_config`]); the rest are only mirrored, never invented.
const GLOBAL_KEYS: &[&str] = &["lastOnboardingVersion", "bypassPermissionsModeAccepted"];

/// User-settings keys copied from the host user, when present.
///
/// `theme` keeps the pane legible in the operator's terminal;
/// `skipDangerousModePermissionPrompt` records a disclaimer this user already
/// accepted on this machine, so a bypass launch is not blocked by a modal.
const SETTINGS_KEYS: &[&str] = &["theme", "skipDangerousModePermissionPrompt"];

/// The host user's Claude configuration, used only as a source of the
/// allowlisted flags above.
#[derive(Debug, Clone)]
pub struct HostClaudeConfig {
    /// `~/.claude.json`.
    pub global_config: PathBuf,
    /// `~/.claude/settings.json`.
    pub user_settings: PathBuf,
}

impl HostClaudeConfig {
    /// The conventional layout below a home directory.
    #[must_use]
    pub fn for_home(home: &Path) -> Self {
        Self {
            global_config: home.join(".claude.json"),
            user_settings: home.join(".claude").join("settings.json"),
        }
    }

    /// `$HOME`-derived layout, absent when `HOME` is unset or relative.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        home.is_absolute().then(|| Self::for_home(&home))
    }
}

/// What [`seed_scoped_config`] changed. Carries key names only, never values.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SeedOutcome {
    /// `.claude.json` was created or amended.
    pub global_config_written: bool,
    /// `settings.json` was created or amended.
    pub user_settings_written: bool,
    /// Allowlisted keys taken from the host user's configuration.
    pub copied_keys: Vec<String>,
}

impl SeedOutcome {
    /// Nothing needed changing; the directory was already onboarded.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        !self.global_config_written && !self.user_settings_written
    }

    /// One-line journal text. Names keys, never values.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_noop() {
            return "claude onboarding already seeded".into();
        }
        let mut files = Vec::new();
        if self.global_config_written {
            files.push(".claude.json");
        }
        if self.user_settings_written {
            files.push("settings.json");
        }
        let copied = if self.copied_keys.is_empty() {
            "none".to_owned()
        } else {
            self.copied_keys.join(",")
        };
        format!(
            "claude onboarding seeded in scoped config ({}); copied host keys: {copied}",
            files.join("+")
        )
    }
}

/// Mark `native_home` as already onboarded so the native CLI skips its
/// first-run flow.
///
/// Idempotent and additive: an existing key in the scoped directory always
/// wins, so a human who changed the theme inside the pane keeps their choice.
/// `hasCompletedOnboarding` is forced to `true` because that is the whole
/// point of the scoped directory — it is Remuda's own launch surface, not a
/// place where a person is expected to answer a wizard.
pub fn seed_scoped_config(
    native_home: &Path,
    host: Option<&HostClaudeConfig>,
) -> DriverResult<SeedOutcome> {
    if !native_home.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "claude config dir must be an absolute path".into(),
        ));
    }
    let host_global = host.and_then(|host| read_object(&host.global_config));
    let host_settings = host.and_then(|host| read_object(&host.user_settings));
    let mut outcome = SeedOutcome::default();

    let mut global = read_object(&native_home.join(".claude.json")).unwrap_or_default();
    let mut global_changed = false;
    if global.get("hasCompletedOnboarding") != Some(&Value::Bool(true)) {
        global.insert("hasCompletedOnboarding".into(), Value::Bool(true));
        global_changed = true;
    }
    global_changed |= copy_keys(
        host_global.as_ref(),
        &mut global,
        GLOBAL_KEYS,
        &mut outcome.copied_keys,
    );
    if global_changed {
        write_private_json(&native_home.join(".claude.json"), &global)?;
        outcome.global_config_written = true;
    }

    let mut settings = read_object(&native_home.join("settings.json")).unwrap_or_default();
    if copy_keys(
        host_settings.as_ref(),
        &mut settings,
        SETTINGS_KEYS,
        &mut outcome.copied_keys,
    ) {
        write_private_json(&native_home.join("settings.json"), &settings)?;
        outcome.user_settings_written = true;
    }
    Ok(outcome)
}

/// Copy allowlisted keys that the target does not define yet. Returns whether
/// anything was added. A `false` or `null` source value is not worth copying:
/// it only records the absence the target already has.
fn copy_keys(
    source: Option<&Map<String, Value>>,
    target: &mut Map<String, Value>,
    allowlist: &[&str],
    copied: &mut Vec<String>,
) -> bool {
    let Some(source) = source else {
        return false;
    };
    let mut changed = false;
    for key in allowlist {
        if target.contains_key(*key) {
            continue;
        }
        let Some(value) = source.get(*key) else {
            continue;
        };
        if matches!(value, Value::Null | Value::Bool(false)) {
            continue;
        }
        target.insert((*key).to_owned(), value.clone());
        copied.push((*key).to_owned());
        changed = true;
    }
    changed
}

/// A parsed JSON object, or `None` for a missing or unreadable file.
///
/// An unreadable host config is not an error: seeding still produces a
/// directory that skips onboarding, just without the operator's theme.
fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let bytes = fs::read(path).ok()?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(object)) => Some(object),
        _ => None,
    }
}

fn write_private_json(path: &Path, object: &Map<String, Value>) -> DriverResult<()> {
    let bytes = serde_json::to_vec_pretty(&Value::Object(object.clone()))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        set_mode(parent, 0o700)?;
    }
    let tmp = {
        let mut raw = path.as_os_str().to_os_string();
        raw.push(".tmp");
        PathBuf::from(raw)
    };
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    set_mode(path, 0o600)
}

fn set_mode(path: &Path, mode: u32) -> DriverResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

/// Does this config directory carry evidence of a Claude login?
///
/// A caller that pins `CLAUDE_CONFIG_DIR` must not point the CLI at a
/// directory with no credentials: Claude 2.1 then reports "Not logged in"
/// even when the host user is authenticated. The presence of `.claude.json`
/// alone does **not** answer this — [`seed_scoped_config`] writes that file
/// with onboarding flags and no credentials whatsoever — so look for the
/// credential file, or for the account fields a real login leaves behind.
#[must_use]
pub fn has_login_material(config_dir: &Path) -> bool {
    if config_dir.join(".credentials.json").is_file() {
        return true;
    }
    read_object(&config_dir.join(".claude.json")).is_some_and(|config| {
        ["oauthAccount", "userID", "customApiKeyResponses"]
            .iter()
            .any(|key| config.get(*key).is_some_and(|value| !value.is_null()))
    })
}

/// Pre-accept Claude's folder-trust dialog for exactly `cwd` in a scoped
/// config directory.
///
/// This is the design-sanctioned, non-GUI answer to a trust dialog that would
/// otherwise park the agent before its `SessionStart` hook fires: Claude's own
/// global config records the decision per project under
/// `projects.<cwd>.hasTrustDialogAccepted`, and writing that exact key is the
/// same state a human accepting the dialog produces.
///
/// Deliberately scoped to an **isolated, Node-owned** config directory: the
/// caller must never point this at the operator's real `~/.claude.json`, where
/// editing trust decisions on their behalf is out of bounds — there the
/// carrier answers the dialog with its documented key sequence instead. The
/// path must be absolute for the same reason [`seed_scoped_config`] requires
/// it: a relative entry could never match the absolute cwd Claude records.
///
/// Idempotent and additive: an existing project object keeps every other key;
/// an existing truthy `hasTrustDialogAccepted` is left as written.
pub fn pre_trust_workspace(config_dir: &Path, cwd: &Path) -> DriverResult<()> {
    if !config_dir.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "claude config dir must be an absolute path".into(),
        ));
    }
    if !cwd.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "a pre-trusted workspace cwd must be absolute".into(),
        ));
    }
    let mut global = read_object(&config_dir.join(".claude.json")).unwrap_or_default();
    let changed = {
        let projects = global
            .entry("projects")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| {
                DriverError::SettingsIsolationUnavailable(
                    ".claude.json projects is not an object".into(),
                )
            })?;
        let project = projects
            .entry(cwd.to_string_lossy().into_owned())
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .ok_or_else(|| {
                DriverError::SettingsIsolationUnavailable(
                    ".claude.json project entry is not an object".into(),
                )
            })?;
        match project.get("hasTrustDialogAccepted") {
            Some(Value::Bool(true)) => false,
            _ => {
                project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
                true
            }
        }
    };
    if changed {
        write_private_json(&config_dir.join(".claude.json"), &global)?;
    }
    Ok(())
}

/// Global-config key Claude Code writes when a human answers the auto-mode
/// "Allow reads outside the working directories?" dialog with
/// **"Yes, keep allowing"**.
///
/// Verified against the installed Claude Code 2.1.274 bundle: the dialog
/// (`auto_mode_outside_reads`) is offered only while this key is falsy, and the
/// `allow` branch runs `set(globalConfig, n => ({…n,
/// hasSeenAutoModeOutsideReadPrompt: true}))`. The `block` branch instead writes
/// `permissions.blockReadsOutsideWorkingDirectories: true` to **user**
/// `settings.json`, which is a machine-wide refusal Remuda must never seed.
///
/// This lives in the same global config [`pre_trust_workspace`] writes
/// (`.claude.json` under `CLAUDE_CONFIG_DIR`), not under `projects` — the
/// answer is not per cwd.
pub const OUTSIDE_READS_ALLOW_KEY: &str = "hasSeenAutoModeOutsideReadPrompt";

/// Persist the equivalent of answering **"Yes, keep allowing"** on Claude's
/// auto-mode "Allow reads outside the working directories?" dialog before the
/// process starts.
///
/// Dispatch carries `bypassPermissions`, where the sandboxed auto mode reads
/// wherever the task's tools point; a parked outside-reads question stops the
/// worker exactly like the folder-trust dialog does. Under any other posture
/// the dialog must be left alone, so callers gate this on the bypass posture
/// and never call it otherwise.
///
/// Same confinement as [`pre_trust_workspace`]: an absolute, Node-owned scoped
/// config directory only, idempotent. An existing truthy key is left as
/// written; any other value is upgraded to `true`, exactly like
/// [`pre_trust_workspace`] — in a Node-scoped dir this flag can only record
/// this launch's own bypass intent, never a prior human refusal.
/// Returns whether the file was written.
pub fn allow_reads_outside_workspaces(config_dir: &Path) -> DriverResult<bool> {
    if !config_dir.is_absolute() {
        return Err(DriverError::InvalidLaunchSpec(
            "claude config dir must be an absolute path".into(),
        ));
    }
    let mut global = read_object(&config_dir.join(".claude.json")).unwrap_or_default();
    if global.get(OUTSIDE_READS_ALLOW_KEY) == Some(&Value::Bool(true)) {
        return Ok(false);
    }
    global.insert(OUTSIDE_READS_ALLOW_KEY.into(), Value::Bool(true));
    write_private_json(&config_dir.join(".claude.json"), &global)?;
    Ok(true)
}

/// A recognised Claude Code startup screen that is not the prompt composer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupDialog {
    /// Stable diagnostic name (`onboarding-theme`, `login`, …).
    pub name: &'static str,
    /// Keys that accept the default, or `None` when only a human may answer.
    pub keys: Option<Vec<String>>,
}

impl StartupDialog {
    fn answered(name: &'static str, keys: &[&str]) -> Self {
        Self {
            name,
            keys: Some(keys.iter().map(|key| (*key).to_owned()).collect()),
        }
    }

    fn human_only(name: &'static str) -> Self {
        Self { name, keys: None }
    }
}

/// Recognise a first-run screen from a stripped PTY viewport.
///
/// Every arm requires at least two independent markers: a single stock phrase
/// can appear in ordinary model output, and answering the composer with
/// `enter` because a transcript quoted "Security notes:" would submit a turn.
#[must_use]
pub fn startup_dialog(screen: &str) -> Option<StartupDialog> {
    let flat = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    let has = |needle: &str| flat.contains(needle);

    // The same picker is reachable via `/theme` mid-session, where Remuda
    // must not press keys; only the onboarding variant prints the intro line.
    if has("Choose the text style that looks best with your terminal") && has("Let's get started.")
    {
        return Some(StartupDialog::answered("onboarding-theme", &["enter"]));
    }
    if has("Security notes:")
        && has("Claude can make mistakes.")
        && has("Due to prompt injection risks")
    {
        return Some(StartupDialog::answered("onboarding-security", &["enter"]));
    }
    // Enter here rewrites the operator's terminal profile (key bindings, bell).
    // Escape is the documented skip and leaves the host untouched.
    if has("Use Claude Code's terminal setup?") && has("recommended settings") {
        return Some(StartupDialog::answered(
            "onboarding-terminal-setup",
            &["escape"],
        ));
    }
    if has("Select login method:") && has("Claude account with subscription") {
        return Some(StartupDialog::human_only("login"));
    }
    if has("WARNING: Claude Code running in Bypass Permissions mode") && has("Yes, I accept") {
        return Some(StartupDialog::human_only("bypass-disclaimer"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write(path: &Path, value: Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    #[test]
    fn seeding_completes_onboarding_and_copies_only_allowlisted_host_keys() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        write(
            &home.join(".claude.json"),
            serde_json::json!({
                "hasCompletedOnboarding": true,
                "lastOnboardingVersion": "2.1.74",
                "bypassPermissionsModeAccepted": true,
                "oauthAccount": {"accessToken": "secret-token"},
                "userID": "0123456789abcdef",
                "projects": {"/somewhere": {"hasTrustDialogAccepted": true}},
            }),
        );
        write(
            &home.join(".claude/settings.json"),
            serde_json::json!({
                "theme": "dark",
                "skipDangerousModePermissionPrompt": true,
                "apiKeyHelper": "/opt/secret-helper",
            }),
        );
        let scoped = dir.path().join("native-home");
        let outcome =
            seed_scoped_config(&scoped, Some(&HostClaudeConfig::for_home(&home))).unwrap();
        assert!(outcome.global_config_written && outcome.user_settings_written);

        let global = read_object(&scoped.join(".claude.json")).unwrap();
        assert_eq!(global["hasCompletedOnboarding"], Value::Bool(true));
        assert_eq!(global["lastOnboardingVersion"], Value::from("2.1.74"));
        assert_eq!(global["bypassPermissionsModeAccepted"], Value::Bool(true));
        for secret in ["oauthAccount", "userID", "projects"] {
            assert!(!global.contains_key(secret), "copied {secret}");
        }
        let settings = read_object(&scoped.join("settings.json")).unwrap();
        assert_eq!(settings["theme"], Value::from("dark"));
        assert_eq!(
            settings["skipDangerousModePermissionPrompt"],
            Value::Bool(true)
        );
        assert!(!settings.contains_key("apiKeyHelper"));

        // Re-seeding is a no-op and never fights a choice made inside the pane.
        let mut settings = settings;
        settings.insert("theme".into(), Value::from("light"));
        write_private_json(&scoped.join("settings.json"), &settings).unwrap();
        let again = seed_scoped_config(&scoped, Some(&HostClaudeConfig::for_home(&home))).unwrap();
        assert!(again.is_noop(), "{again:?}");
        assert_eq!(
            read_object(&scoped.join("settings.json")).unwrap()["theme"],
            Value::from("light")
        );
    }

    #[test]
    fn seeding_without_a_readable_host_config_still_skips_onboarding() {
        let dir = tempfile::tempdir().unwrap();
        let scoped = dir.path().join("native-home");
        let host = HostClaudeConfig::for_home(&dir.path().join("missing-home"));
        let outcome = seed_scoped_config(&scoped, Some(&host)).unwrap();
        assert!(outcome.global_config_written && outcome.copied_keys.is_empty());
        assert!(!outcome.user_settings_written, "no settings to copy");
        assert_eq!(
            read_object(&scoped.join(".claude.json")).unwrap()["hasCompletedOnboarding"],
            Value::Bool(true)
        );
        assert!(seed_scoped_config(&scoped, None).unwrap().is_noop());
    }

    #[test]
    fn a_seeded_config_dir_is_not_mistaken_for_a_logged_in_one() {
        let dir = tempfile::tempdir().unwrap();
        let scoped = dir.path().join("native-home");
        assert!(!has_login_material(&scoped), "empty dir");
        seed_scoped_config(&scoped, None).unwrap();
        assert!(
            !has_login_material(&scoped),
            "seeding writes onboarding flags, never credentials"
        );
        write(
            &scoped.join(".claude.json"),
            serde_json::json!({"hasCompletedOnboarding": true, "oauthAccount": {"a": 1}}),
        );
        assert!(has_login_material(&scoped), "an account field is a login");

        let credentials = dir.path().join("with-credentials");
        std::fs::create_dir_all(&credentials).unwrap();
        std::fs::write(credentials.join(".credentials.json"), "{}").unwrap();
        assert!(has_login_material(&credentials));
    }

    #[test]
    fn seeding_refuses_a_relative_config_dir() {
        assert!(matches!(
            seed_scoped_config(Path::new("relative/home"), None),
            Err(DriverError::InvalidLaunchSpec(_))
        ));
    }

    #[test]
    fn pre_trust_writes_exactly_the_one_project_key_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("native-home");
        let cwd = dir.path().join("workspace");
        // First call creates the global config with just the trust decision.
        pre_trust_workspace(&home, &cwd).unwrap();
        let global = read_object(&home.join(".claude.json")).unwrap();
        assert_eq!(
            global["projects"][cwd.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        assert!(
            !global["projects"][cwd.to_str().unwrap()]
                .as_object()
                .unwrap()
                .contains_key("allowedTools"),
            "pre-trust never invents other project keys"
        );

        // A second call after Claude added other keys preserves them.
        let mut object = global.clone();
        object["projects"][cwd.to_str().unwrap()]["lastCost"] = json!(12);
        write_private_json(&home.join(".claude.json"), &object).unwrap();
        pre_trust_workspace(&home, &cwd).unwrap();
        let reread = read_object(&home.join(".claude.json")).unwrap();
        assert_eq!(
            reread["projects"][cwd.to_str().unwrap()]["lastCost"],
            json!(12)
        );
        assert_eq!(
            reread["projects"][cwd.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn pre_trust_does_not_touch_other_projects() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("native-home");
        let existing_cwd = dir.path().join("elsewhere");
        write(
            &home.join(".claude.json"),
            json!({
                "projects": {
                    existing_cwd.to_string_lossy(): {"hasTrustDialogAccepted": false}
                }
            }),
        );
        pre_trust_workspace(&home, &dir.path().join("workspace")).unwrap();
        let global = read_object(&home.join(".claude.json")).unwrap();
        assert_eq!(
            global["projects"][existing_cwd.to_str().unwrap()]["hasTrustDialogAccepted"],
            false,
            "a pre-trust for one cwd must not rewrite another project's decision"
        );
    }

    #[test]
    fn pre_trust_requires_absolute_paths() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            pre_trust_workspace(Path::new("relative/home"), dir.path()),
            Err(DriverError::InvalidLaunchSpec(_))
        ));
        assert!(matches!(
            pre_trust_workspace(&dir.path().join("home"), Path::new("relative/ws")),
            Err(DriverError::InvalidLaunchSpec(_))
        ));
    }

    #[test]
    fn outside_reads_allow_writes_the_global_flag_once_and_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("native-home");
        let cwd = dir.path().join("workspace");
        // A pre-existing trust decision and an unrelated key must survive.
        write(
            &home.join(".claude.json"),
            json!({
                "hasCompletedOnboarding": true,
                "projects": { cwd.to_string_lossy(): {"hasTrustDialogAccepted": true} }
            }),
        );
        assert!(allow_reads_outside_workspaces(&home).unwrap());
        let global = read_object(&home.join(".claude.json")).unwrap();
        assert_eq!(global[OUTSIDE_READS_ALLOW_KEY], Value::Bool(true));
        assert_eq!(global["hasCompletedOnboarding"], Value::Bool(true));
        assert_eq!(
            global["projects"][cwd.to_str().unwrap()]["hasTrustDialogAccepted"],
            Value::Bool(true)
        );
        // Idempotent: the second call writes nothing.
        assert!(!allow_reads_outside_workspaces(&home).unwrap());
        // An empty scoped dir gets the flag too.
        let fresh = dir.path().join("fresh-home");
        assert!(allow_reads_outside_workspaces(&fresh).unwrap());
        assert_eq!(
            read_object(&fresh.join(".claude.json")).unwrap()[OUTSIDE_READS_ALLOW_KEY],
            Value::Bool(true)
        );
    }

    #[test]
    fn outside_reads_allow_upgrades_an_explicit_false_like_pre_trust() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("native-home");
        write(
            &home.join(".claude.json"),
            json!({ OUTSIDE_READS_ALLOW_KEY: false }),
        );
        // A Node-scoped dir only ever records this launch's own bypass intent,
        // so the ask-again spelling is upgraded rather than respected — same
        // rule pre_trust_workspace applies to the trust flag.
        assert!(allow_reads_outside_workspaces(&home).unwrap());
        assert_eq!(
            read_object(&home.join(".claude.json")).unwrap()[OUTSIDE_READS_ALLOW_KEY],
            Value::Bool(true)
        );
        assert!(!allow_reads_outside_workspaces(&home).unwrap());
    }

    #[test]
    fn outside_reads_allow_refuses_a_relative_config_dir() {
        assert!(matches!(
            allow_reads_outside_workspaces(Path::new("relative/home")),
            Err(DriverError::InvalidLaunchSpec(_))
        ));
    }

    /// The fixtures are the screens this detector exists for; a change in
    /// Claude Code's wording must fail here rather than in a live pane.
    #[test]
    fn startup_screens_are_recognised_with_the_right_default() {
        for (fixture, name, keys) in [
            (
                include_str!("../../remuda-testing/tests/fixtures/claude-onboarding-theme.txt"),
                "onboarding-theme",
                Some(vec!["enter".to_owned()]),
            ),
            (
                include_str!("../../remuda-testing/tests/fixtures/claude-onboarding-security.txt"),
                "onboarding-security",
                Some(vec!["enter".to_owned()]),
            ),
            (
                include_str!(
                    "../../remuda-testing/tests/fixtures/claude-onboarding-terminal-setup.txt"
                ),
                "onboarding-terminal-setup",
                // Enter here rewrites the operator's terminal profile.
                Some(vec!["escape".to_owned()]),
            ),
            (
                include_str!("../../remuda-testing/tests/fixtures/claude-onboarding-login.txt"),
                "login",
                None,
            ),
            (
                include_str!("../../remuda-testing/tests/fixtures/claude-bypass-disclaimer.txt"),
                "bypass-disclaimer",
                None,
            ),
        ] {
            let dialog = startup_dialog(fixture).unwrap_or_else(|| panic!("{name} not detected"));
            assert_eq!(dialog.name, name);
            assert_eq!(dialog.keys, keys, "{name}");
        }
    }

    #[test]
    fn an_ordinary_screen_is_never_a_startup_dialog() {
        for screen in [
            include_str!("../../remuda-testing/tests/fixtures/claude-prompt-composer.txt"),
            include_str!("../../remuda-testing/tests/fixtures/pty-approval.txt"),
            include_str!("../../remuda-testing/tests/fixtures/pty-question.txt"),
            "",
            // The trust dialog is D-022's, answered with its own exact matcher.
            "Quick safety check: Is this a project you created or one you trust?\n             ❯ No, exit\n  Yes, I trust this folder",
            // A mid-session /theme picker has no onboarding intro line, so a
            // human's own theme change is never answered from under them.
            "Theme\nChoose the text style that looks best with your terminal\n             ❯ 1. Auto (match terminal)\n  2. Dark mode",
            // Model output that merely quotes one marker must not trip a detector.
            "Here is what the docs say: Security notes: read them before running code.",
            "The wizard asks \"Select login method:\" and then waits.",
        ] {
            assert_eq!(startup_dialog(screen), None, "{screen:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn seeded_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let scoped = dir.path().join("native-home");
        seed_scoped_config(&scoped, None).unwrap();
        let mode = std::fs::metadata(scoped.join(".claude.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(&scoped).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
