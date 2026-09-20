//! Bound filesystem access before Claude starts loading user extensions.
//!
//! macOS privacy checks can stall a launchd child while following a skill
//! symlink. Probe in a process group that can be killed; timing out a Rust
//! filesystem worker would leave that worker blocked after the request ends.

use crate::DriverError;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);
const SCAN: &str = r#"
probe_path() {
    probe_error=$(/bin/ls -ld "$1" 2>&1) && return 0
    case "$probe_error" in
        *": No such file or directory") return 2 ;;
        *) printf '%s\n' "$probe_error" >&2; return 1 ;;
    esac
}
probe_path "$1"
case $? in 0) ;; 2) exit 0 ;; *) exit 1 ;; esac
CDPATH=; cd "$1" || exit
/bin/ls -f . >/dev/null || exit
for settings in settings.json settings.local.json; do
    detail=$(/usr/bin/head -c 1 "$settings" 2>&1 >/dev/null)
    status=$?
    if [ "$status" -ne 0 ]; then
        case "$detail" in
            *': No such file or directory'|*': Not a directory') continue ;;
            *) printf '%s\n' "$detail" >&2; exit "$status" ;;
        esac
    fi
done
if [ -d skills ] || [ -L skills ]; then
    /bin/ls -f skills >/dev/null || exit
    for skill in skills/* skills/.[!.]* skills/..?*; do
        if [ ! -d "$skill" ] && [ ! -L "$skill" ]; then continue; fi
        detail=$(/bin/cat "$skill/SKILL.md" 2>&1 >/dev/null)
        status=$?
        if [ "$status" -ne 0 ]; then
            case "$detail" in
                *': No such file or directory'|*': Not a directory') continue ;;
                *) printf '%s\n' "$detail" >&2; exit "$status" ;;
            esac
        fi
    done
fi
for source in commands agents; do
    probe_path "$source"
    case $? in 0) ;; 2) continue ;; *) exit 1 ;; esac
    /usr/bin/find -L "./$source" \
        \( -name .git -o -name node_modules \) -prune -o \
        -type f -name '*.md' -exec /bin/cat {} + >/dev/null || exit
done
"#;

pub(crate) fn check_claude_config_access(home: &Path) -> Result<(), DriverError> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", SCAN, "remuda-claude-config-probe"])
        .arg(home);
    run_probe(&mut command, home, TIMEOUT)
}

fn run_probe(command: &mut Command, home: &Path, timeout: Duration) -> Result<(), DriverError> {
    let result = crate::workspace_access::bounded_workspace_command(command, home, timeout);
    match result {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(config_error(
                home,
                &format!("{}: {}", output.status, stderr.trim()),
            ))
        }
        Err(error) => Err(config_error(home, &error.to_string())),
    }
}

fn config_error(home: &Path, detail: &str) -> DriverError {
    let detail: String = detail.chars().take(2048).collect();
    let mut message = format!(
        "Claude startup configuration {} could not be read: {detail}; inspect settings, skills, commands, and agents, including their symlink targets",
        home.display()
    );
    if cfg!(target_os = "macos") && !detail.contains("Full Disk Access") {
        let executable = std::env::current_exe().unwrap_or_else(|_| "remuda".into());
        message.push_str("; macOS privacy restrictions may apply: ");
        message.push_str(&crate::workspace_access::workspace_access_guidance(
            &executable,
        ));
    }
    message.push_str("; alternatively select an accessible Claude configuration with claudeConfigDir; no skills or credentials were changed");
    DriverError::Failed(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::Instant;

    #[test]
    fn safe_skill_symlinks_missing_optional_sources_and_broken_links_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("config ' with $literal spaces");
        let source = directory.path().join("skill-source");
        std::fs::create_dir_all(home.join("skills")).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: fixture\n---\nfixture").unwrap();
        symlink(&source, home.join("skills/safe")).unwrap();
        symlink(directory.path().join("absent"), home.join("skills/broken")).unwrap();
        check_claude_config_access(&home).unwrap();
        assert_eq!(
            std::fs::read_link(home.join("skills/safe")).unwrap(),
            source
        );
        assert!(home.join("skills/broken").is_symlink());
        check_claude_config_access(&directory.path().join("missing-config")).unwrap();
    }

    #[test]
    fn unrelated_skill_fixture_trees_are_not_traversed_or_read() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let skill = directory.path().join("skill-source");
        let fixtures = skill.join("fixtures");
        std::fs::create_dir_all(home.join("skills")).unwrap();
        std::fs::create_dir_all(&fixtures).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: fixture\n---\nfixture").unwrap();
        let unreadable = fixtures.join("private-example.md");
        std::fs::write(&unreadable, "unrelated fixture").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o0)).unwrap();
        symlink(&fixtures, fixtures.join("cycle")).unwrap();
        symlink(&skill, home.join("skills/example")).unwrap();
        check_claude_config_access(&home).unwrap();
        assert!(fixtures.join("cycle").is_symlink());
    }

    #[test]
    fn settings_access_probe_discards_contents_and_preserves_files() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let settings = directory.path().join("settings-source.json");
        let contents = "{\"fixture\":\"private-content-must-not-be-returned\"}";
        std::fs::write(&settings, contents).unwrap();
        symlink(&settings, home.join("settings.json")).unwrap();
        symlink(
            directory.path().join("missing"),
            home.join("settings.local.json"),
        )
        .unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", SCAN, "remuda-claude-config-probe"])
            .arg(&home);
        let output =
            crate::workspace_access::bounded_workspace_command(&mut command, &home, TIMEOUT)
                .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), contents);
        assert!(home.join("settings.json").is_symlink());
    }

    #[test]
    fn stalled_settings_open_is_bounded_before_extension_loading() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir(&home).unwrap();
        nix::unistd::mkfifo(&home.join("settings.json"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", SCAN, "remuda-claude-config-probe"])
            .arg(&home);
        let started = Instant::now();
        let error = run_probe(&mut command, &home, Duration::from_millis(100))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("Claude startup configuration"), "{error}");
        assert!(error.contains("access probe timed out"), "{error}");
    }

    #[test]
    fn denied_probe_preserves_error_and_names_configuration_remediation() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'Permission denied: skills/example/SKILL.md' >&2; exit 73",
        ]);
        let error = run_probe(&mut command, Path::new("/tmp/claude-config"), TIMEOUT)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Claude startup configuration"));
        assert!(error.contains("Permission denied: skills/example/SKILL.md"));
        assert!(error.contains("claudeConfigDir"));
        if cfg!(target_os = "macos") {
            assert!(error.contains("Full Disk Access"));
        }
    }

    #[test]
    fn denied_ancestor_is_not_mistaken_for_a_missing_optional_configuration() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let denied = directory.path().join("denied");
        std::fs::create_dir(&denied).unwrap();
        let original = std::fs::metadata(&denied).unwrap().permissions();
        std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o0)).unwrap();
        // Root can bypass mode bits; use this fixture only where access is denied.
        let denied_here = std::fs::read_dir(&denied).is_err();
        let result = check_claude_config_access(&denied.join("missing-config"));
        std::fs::set_permissions(&denied, original).unwrap();
        if denied_here {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("Permission denied"), "{error}");
            assert!(error.contains("Claude startup configuration"));
        }
    }

    #[test]
    fn stalled_probe_is_bounded_and_returns_configuration_error() {
        let started = Instant::now();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec /bin/sleep 30"]);
        let error = run_probe(
            &mut command,
            Path::new("/tmp/claude-config"),
            Duration::from_millis(100),
        )
        .unwrap_err()
        .to_string();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("Claude startup configuration"));
        assert!(error.contains("access probe timed out"));
        assert!(error.contains("claudeConfigDir"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn factory_rejects_inaccessible_configuration_before_materializing_or_creating_carrier() {
        use remuda_protocol::{DriverKind, HostId, InstanceId, WorkspaceId};

        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("not-a-directory");
        std::fs::write(&home, "fixture").unwrap();
        let data = directory.path().join("node");
        let registry = crate::native_driver_registry(
            crate::NativeDriverConfig::new(data.clone())
                .with_claude_binary(directory.path().join("must-not-be-probed")),
        )
        .unwrap();
        let instance = crate::runtime::fixture_instance(
            InstanceId::new(),
            HostId::new(),
            WorkspaceId::new(),
            DriverKind::ClaudePty,
        )
        .unwrap();
        let launch_dir = data
            .join("instances")
            .join(instance.meta.id.as_id().as_str())
            .join("launch");
        let request = serde_json::from_value(serde_json::json!({
            "kind": "claude", "driver": "claude-pty", "delegation": "none",
            "claudeConfigDir": home,
        }))
        .unwrap();
        let result = registry.build(
            DriverKind::ClaudePty,
            crate::DriverLaunch {
                instance,
                request,
                workspace_root: directory.path().into(),
                registered_workspace_root: directory.path().into(),
                api_relay: None,
            },
        );
        let error = match result {
            Ok(_) => panic!("configuration must be rejected before a carrier exists"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("Claude startup configuration"), "{error}");
        assert!(!launch_dir.join("session-start.sh").exists());
        assert!(!data.join("herdr").exists());
    }
}
