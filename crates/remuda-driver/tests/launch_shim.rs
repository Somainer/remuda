//! The launch shim executed for real (D-028 §4.2, §5.1, risk #2).
//!
//! The shim's whole job is to be invisible to a human typing `claude` in a
//! terminal, so a unit test on its text proves very little. These run it: a
//! fake `claude` on PATH records the argv and environment it was invoked with,
//! and the assertions are about what that fake saw.
//!
//! Risk #2 in the design lists four ways a PATH shim goes wrong — `which`
//! showing a Remuda path, colliding with a user's own wrapper, failing to
//! inject in a non-login shell, and being bypassed by an absolute path. Each
//! has a test here.
#![cfg(unix)]

use remuda_driver::{materialize_shims, shim_disabled};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Harness {
    _dir: tempfile::TempDir,
    bin_dir: PathBuf,
    real_bin: PathBuf,
    overlay: PathBuf,
    record: PathBuf,
}

/// A shim set plus a fake `claude` earlier on PATH that records its argv.
fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    let overlay = launch.join("settings.json");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::write(&overlay, r#"{"hooks":{}}"#).unwrap();
    let set = materialize_shims(&launch, &overlay, "cred-test", false, None).unwrap();

    let real_bin = dir.path().join("realbin");
    std::fs::create_dir_all(&real_bin).unwrap();
    let record = dir.path().join("argv.txt");
    let script = format!(
        "#!/bin/sh\n: > '{}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\nprintf 'CRED=%s\\n' \"$REMUDA_HOOK_CREDENTIAL\" >> '{}'\nexit 0\n",
        record.display(),
        record.display(),
        record.display()
    );
    let claude = real_bin.join("claude");
    std::fs::write(&claude, script).unwrap();
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();

    Harness {
        _dir: dir,
        bin_dir: set.bin_dir,
        real_bin,
        overlay,
        record,
    }
}

impl Harness {
    fn path(&self) -> String {
        format!(
            "{}:{}:/usr/bin:/bin",
            self.bin_dir.display(),
            self.real_bin.display()
        )
    }

    /// Run `sh -c <script>` with the shim on PATH. Not a login shell: §risk #2
    /// calls out that a non-login shell must still get the injection, and it
    /// does because PATH is inherited rather than sourced from a profile.
    fn sh(&self, script: &str) -> std::process::Output {
        self.sh_env(script, &[])
    }

    /// [`Self::sh`] with `extra` added on top of the fixed environment.
    ///
    /// `env_clear` first, deliberately. These tests are *about* which
    /// environment reaches the agent, so inheriting the runner's own is the
    /// one thing they must not do: a `REMUDA_SHIM=off` in the ambient shell
    /// silently turns the shim into a passthrough and the assertions then
    /// describe the operator's machine rather than the code. Verified by
    /// running this binary under `REMUDA_SHIM=off`, which failed two tests
    /// before this and passes after.
    ///
    /// `HOME` is kept because the shadow `ZDOTDIR` logic reads it, and nothing
    /// else is: `sh -c` needs no more than `PATH`.
    fn sh_env(&self, script: &str, extra: &[(&str, &str)]) -> std::process::Output {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", self.path())
            .env("REMUDA_HOOK_CREDENTIAL", "cred-test");
        if let Some(home) = std::env::var_os("HOME") {
            command.env("HOME", home);
        }
        for (key, value) in extra {
            command.env(key, value);
        }
        command.output().expect("shell runs")
    }

    fn recorded(&self) -> Vec<String> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

#[test]
fn typing_claude_by_hand_reaches_the_real_binary_with_the_overlay_attached() {
    // This is the P1 acceptance criterion in miniature: nobody passed a flag,
    // and the agent still starts with the overlay.
    let harness = harness();
    let output = harness.sh("claude");
    assert!(output.status.success(), "{output:?}");
    let argv = harness.recorded();
    assert!(
        argv.contains(&"--settings".to_owned()),
        "the overlay was not injected: {argv:?}"
    );
    assert!(
        argv.contains(&harness.overlay.to_string_lossy().into_owned()),
        "{argv:?}"
    );
    assert!(
        argv.contains(&"--setting-sources".to_owned()),
        "--setting-sources is what makes the resolved config deterministic: {argv:?}"
    );
}

#[test]
fn the_users_own_flags_survive_and_come_after_ours() {
    // `claude --resume X` must still resume X.
    let harness = harness();
    assert!(
        harness
            .sh("claude --resume abc123 --model sonnet")
            .status
            .success()
    );
    let argv = harness.recorded();
    assert!(argv.contains(&"--resume".to_owned()), "{argv:?}");
    assert!(argv.contains(&"abc123".to_owned()), "{argv:?}");
    let settings = argv.iter().position(|a| a == "--settings").unwrap();
    let resume = argv.iter().position(|a| a == "--resume").unwrap();
    assert!(
        settings < resume,
        "the user's flags must come last so they win a clash: {argv:?}"
    );
}

#[test]
fn a_user_who_passes_their_own_settings_keeps_it() {
    // Overriding an explicit --settings would be worse than losing our signal:
    // they asked for that file.
    let harness = harness();
    assert!(
        harness
            .sh("claude --settings /tmp/theirs.json")
            .status
            .success()
    );
    let argv = harness.recorded();
    assert_eq!(
        argv.iter().filter(|a| *a == "--settings").count(),
        1,
        "{argv:?}"
    );
    assert!(argv.contains(&"/tmp/theirs.json".to_owned()), "{argv:?}");
    assert!(
        !argv.contains(&harness.overlay.to_string_lossy().into_owned()),
        "our overlay must not override an explicit one: {argv:?}"
    );
}

#[test]
fn remuda_shim_off_makes_the_shim_a_plain_passthrough() {
    let harness = harness();
    // Through the same fixed-environment helper as every other case, with the
    // switch set explicitly: this test asserts what `REMUDA_SHIM=off` does, so
    // the value has to come from the test rather than from whatever the
    // runner's shell happened to export.
    let output = harness.sh_env("claude --resume abc123", &[("REMUDA_SHIM", "off")]);
    assert!(output.status.success(), "{output:?}");
    let argv = harness.recorded();
    assert!(
        !argv.contains(&"--settings".to_owned()),
        "REMUDA_SHIM=off must inject nothing: {argv:?}"
    );
    assert!(argv.contains(&"abc123".to_owned()), "{argv:?}");
}

#[test]
fn which_claude_reports_the_shim_and_the_shim_still_resolves_the_real_one() {
    // Risk #2: `which claude` showing a Remuda path is expected and visible.
    // What must not happen is the shim finding itself and recursing.
    let harness = harness();
    let output = harness.sh("command -v claude");
    let resolved = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    assert_eq!(
        resolved,
        harness.bin_dir.join("claude").to_string_lossy(),
        "the shim is expected to be what PATH resolves to"
    );
    assert!(
        harness.sh("claude").status.success(),
        "and it must still reach the real binary"
    );
}

#[test]
fn an_absolute_path_bypasses_the_shim_entirely() {
    // A documented degradation, not a bug: the user asked for that binary.
    // The signal tier drops to screen and the UI must say so.
    let harness = harness();
    let output = harness.sh(&format!("{}/claude", harness.real_bin.display()));
    assert!(output.status.success());
    let argv = harness.recorded();
    assert!(
        !argv.contains(&"--settings".to_owned()),
        "an absolute path cannot be intercepted: {argv:?}"
    );
}

#[test]
fn the_credential_reaches_the_agent_but_is_not_on_its_command_line() {
    let harness = harness();
    assert!(harness.sh("claude").status.success());
    let recorded = harness.recorded();
    assert!(
        recorded.contains(&"CRED=cred-test".to_owned()),
        "the hook credential must be in the environment: {recorded:?}"
    );
    assert!(
        !recorded.iter().any(|line| line.contains("--credential")),
        "and never in argv, which ps exposes: {recorded:?}"
    );
}

#[test]
fn a_shim_alone_on_path_exits_rather_than_resolving_to_itself() {
    // The fork-bomb case, and the reason the shim locates itself with builtins
    // instead of `dirname`: with only the shim directory on PATH there is no
    // `dirname` to call, and a shim that cannot recognise itself resolves to
    // itself and exec-loops forever. Bounded so a regression fails the test
    // rather than the machine.
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    std::fs::create_dir_all(&launch).unwrap();
    let overlay = launch.join("settings.json");
    std::fs::write(&overlay, "{}").unwrap();
    let set = materialize_shims(&launch, &overlay, "cred", false, None).unwrap();
    let status = run_bounded(
        Command::new(set.bin_dir.join("claude"))
            .env("PATH", set.bin_dir.to_string_lossy().into_owned()),
    );
    assert_eq!(
        status,
        Some(127),
        "a shim that cannot find its binary must exit like `command not found`, not loop"
    );
}

/// Run `command`, killing it if it does not finish quickly.
///
/// Returns the exit code, or `None` if it had to be killed. Every shim path is
/// an `exec` into a fast fixture, so anything slow is a loop.
fn run_bounded(command: &mut Command) -> Option<i32> {
    use std::time::{Duration, Instant};
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().expect("wait") {
            Some(status) => return status.code(),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[test]
fn a_missing_overlay_does_not_stop_the_agent_from_starting() {
    // The instance directory could have been purged under a live session.
    // Losing the signal is acceptable; refusing to start the agent is not.
    let harness = harness();
    std::fs::remove_file(&harness.overlay).unwrap();
    let output = harness.sh("claude");
    assert!(output.status.success(), "{output:?}");
    assert!(!harness.recorded().contains(&"--settings".to_owned()));
}

#[test]
fn the_pass_through_shims_reach_their_real_binaries_unchanged() {
    // codex and grok have no overlay until P6; the shim must not pretend.
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    std::fs::create_dir_all(&launch).unwrap();
    let overlay = launch.join("settings.json");
    std::fs::write(&overlay, "{}").unwrap();
    let set = materialize_shims(&launch, &overlay, "cred", false, None).unwrap();

    let real = dir.path().join("realbin");
    std::fs::create_dir_all(&real).unwrap();
    let record = dir.path().join("argv.txt");
    write_recorder(&real.join("codex"), &record);

    let status = run_bounded(Command::new("/bin/sh").arg("-c").arg("codex --yolo").env(
        "PATH",
        format!("{}:{}", set.bin_dir.display(), real.display()),
    ));
    assert_eq!(status, Some(0), "the pass-through shim must exec, not loop");
    let argv = std::fs::read_to_string(&record).unwrap();
    assert!(argv.contains("--yolo"), "{argv}");
    assert!(!argv.contains("--settings"), "{argv}");
}

fn write_recorder(path: &Path, record: &Path) {
    std::fs::write(
        path,
        format!(
            "#!/bin/sh\n: > '{}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\n",
            record.display(),
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_disable_switch_is_read_the_same_way_everywhere() {
    assert!(shim_disabled(Some("off")));
    assert!(!shim_disabled(Some("on")));
}

/// A pinned binary beats whatever `claude` PATH would have found first.
///
/// The PATH loop is the right default, but it is exactly wrong once an
/// operator names a specific executable: the loop would silently run the decoy
/// that happens to sit earlier on PATH. This puts a decoy first and asserts the
/// pinned one is what ran.
#[test]
fn shim_execs_the_pinned_binary_not_the_first_on_path() {
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    let overlay = launch.join("settings.json");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::write(&overlay, r#"{"hooks":{}}"#).unwrap();

    // The decoy goes on PATH; the pinned one deliberately does not, so finding
    // it at all proves the shim used the pin rather than a lookup.
    let decoy_dir = dir.path().join("decoy");
    let pinned_dir = dir.path().join("pinned");
    std::fs::create_dir_all(&decoy_dir).unwrap();
    std::fs::create_dir_all(&pinned_dir).unwrap();
    let decoy_record = dir.path().join("decoy-argv.txt");
    let pinned_record = dir.path().join("pinned-argv.txt");
    write_recorder(&decoy_dir.join("claude"), &decoy_record);
    let pinned = pinned_dir.join("claude-custom");
    write_recorder(&pinned, &pinned_record);

    let set = materialize_shims(&launch, &overlay, "cred", false, Some(&pinned)).unwrap();
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("claude --effort high")
        .env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                set.bin_dir.display(),
                decoy_dir.display()
            ),
        )
        .output()
        .expect("shell runs");
    assert!(output.status.success(), "{output:?}");

    assert!(
        !decoy_record.exists(),
        "the decoy earlier on PATH must not have run"
    );
    let argv: Vec<String> = std::fs::read_to_string(&pinned_record)
        .expect("the pinned binary ran")
        .lines()
        .map(str::to_owned)
        .collect();
    // The overlay contract is unchanged by pinning: our flags first, the
    // user's after.
    assert!(argv.contains(&"--settings".to_owned()), "{argv:?}");
    assert!(argv.contains(&"--effort".to_owned()), "{argv:?}");

    // `command -v` still reports a working path, so the session does not look
    // broken from the inside.
    let which = Command::new("/bin/sh")
        .arg("-c")
        .arg("command -v claude")
        .env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                set.bin_dir.display(),
                decoy_dir.display()
            ),
        )
        .output()
        .expect("shell runs");
    assert!(which.status.success(), "{which:?}");
}

/// A pinned path that disappeared fails open on 127 rather than silently
/// running some other `claude` the PATH happens to offer.
#[test]
fn a_missing_pinned_binary_exits_127_without_falling_back_to_path() {
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    let overlay = launch.join("settings.json");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::write(&overlay, r#"{"hooks":{}}"#).unwrap();

    let decoy_dir = dir.path().join("decoy");
    std::fs::create_dir_all(&decoy_dir).unwrap();
    let decoy_record = dir.path().join("decoy-argv.txt");
    write_recorder(&decoy_dir.join("claude"), &decoy_record);

    let absent = dir.path().join("gone").join("claude");
    let set = materialize_shims(&launch, &overlay, "cred", false, Some(&absent)).unwrap();
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("claude")
        .env(
            "PATH",
            format!(
                "{}:{}:/usr/bin:/bin",
                set.bin_dir.display(),
                decoy_dir.display()
            ),
        )
        .output()
        .expect("shell runs");
    assert_eq!(output.status.code(), Some(127), "{output:?}");
    assert!(
        !decoy_record.exists(),
        "a missing pin must not fall back to PATH"
    );
}

/// A pinned path containing a quote is still one argument.
#[test]
fn a_pinned_path_with_a_quote_is_quoted_into_the_script() {
    let dir = tempfile::tempdir().unwrap();
    let launch = dir.path().join("launch");
    let overlay = launch.join("settings.json");
    std::fs::create_dir_all(&launch).unwrap();
    std::fs::write(&overlay, r#"{"hooks":{}}"#).unwrap();
    let odd_dir = dir.path().join("it's");
    std::fs::create_dir_all(&odd_dir).unwrap();
    let record = dir.path().join("argv.txt");
    let pinned = odd_dir.join("claude");
    write_recorder(&pinned, &record);

    let set = materialize_shims(&launch, &overlay, "cred", false, Some(&pinned)).unwrap();
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg("claude")
        .env("PATH", format!("{}:/usr/bin:/bin", set.bin_dir.display()))
        .output()
        .expect("shell runs");
    assert!(output.status.success(), "{output:?}");
    assert!(record.exists(), "the quoted pin must still exec");
}
