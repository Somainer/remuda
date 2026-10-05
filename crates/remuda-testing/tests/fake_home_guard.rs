//! Guard for the `fake-claude` test double: it must never persist a session
//! into the operator's real `~/.claude` (c-resumehome review item 8).
//!
//! The fake is allowed to write only inside the OS temp tree:
//! - a non-temp implicit `$HOME` (the operator's home) silently skips
//!   persistence while the process still runs the turn normally;
//! - a per-test temp home gets the real `projects/` layout;
//! - an explicit non-temp `CLAUDE_CONFIG_DIR` is refused loudly at startup.

use remuda_testing::{
    FIXED_SESSION_ID, FakeClaudeProcess, ScriptKind, SpawnOptions, is_system_subtype, is_type,
    spawn_fake_claude,
};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Lexically normalize `path` (no symlink following), making it absolute.
fn normalized(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` resolves inside the OS temp directory.
fn is_under_temp(path: &Path) -> bool {
    let temp = std::fs::canonicalize(std::env::temp_dir())
        .unwrap_or_else(|_| normalized(&std::env::temp_dir()));
    let candidate = std::fs::canonicalize(path).unwrap_or_else(|_| normalized(path));
    candidate.starts_with(temp)
}

/// Boot, initialize and finish one OK turn; the returned child has not yet been
/// reaped (stdin is still open).
fn run_turn(envs: Vec<(String, String)>) -> FakeClaudeProcess {
    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.extra_envs = envs;
    let mut child = spawn_fake_claude(opts).expect("spawn fake-claude");
    child
        .recv_until(TIMEOUT, |v| is_system_subtype(v, "init"))
        .expect("system/init");
    child
        .send_initialize("guard-init")
        .expect("send initialize");
    child
        .recv_until(TIMEOUT, |v| {
            is_type(v, "control_response")
                && v.pointer("/response/request_id")
                    .and_then(serde_json::Value::as_str)
                    == Some("guard-init")
        })
        .expect("initialize ack");
    child.send_user("hi").expect("send user turn");
    child
        .recv_until(TIMEOUT, |v| is_type(v, "result"))
        .expect("result frame");
    child
}

/// A directory outside the temp tree for "operator home" simulations. Cargo's
/// integration-test scratch lives under the worktree target dir, which is
/// outside the temp tree on ordinary hosts. Returns `None` on hosts where the
/// target dir itself is under tmp (the guard premise cannot be simulated).
fn outside_temp_scratch(label: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("fake-guard-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    (!is_under_temp(&dir)).then_some(dir)
}

/// First `*.jsonl` found while walking `root`, if any.
fn first_jsonl(root: &Path) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop()
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                return Some(path);
            }
        }
    }
    None
}

#[test]
fn fresh_session_does_not_write_a_non_temp_operator_home() {
    let Some(operator_home) = outside_temp_scratch("operator") else {
        eprintln!("skipping: the target scratch dir is itself under the temp tree");
        return;
    };
    let child = run_turn(vec![(
        "HOME".to_owned(),
        operator_home.to_string_lossy().into_owned(),
    )]);
    let status = child.wait().expect("wait fake-claude");
    assert!(
        status.success(),
        "with no writable per-test home the turn still runs"
    );
    assert!(
        !operator_home.join(".claude").exists(),
        "the fake must never create a ~/.claude tree in a non-temp operator HOME: {}",
        operator_home.display()
    );
    let _ = std::fs::remove_dir_all(&operator_home);
}

#[test]
fn fresh_session_persists_under_a_per_test_temp_home() {
    let root = std::env::temp_dir().join(format!("fake-guard-temp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("temp root");
    let fake_home = root.join("testhome");
    std::fs::create_dir_all(&fake_home).expect("fake home");

    let child = run_turn(vec![(
        "HOME".to_owned(),
        fake_home.to_string_lossy().into_owned(),
    )]);
    assert!(child.wait().expect("wait").success());

    let projects = fake_home.join(".claude/projects");
    let transcript = first_jsonl(&projects)
        .expect("a HOME inside the temp tree receives the real projects/<slug>/<id>.jsonl layout");
    let expected = format!("{FIXED_SESSION_ID}.jsonl");
    assert_eq!(
        transcript.file_name().and_then(std::ffi::OsStr::to_str),
        Some(expected.as_str())
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn explicit_config_dir_outside_temp_is_refused_loudly() {
    let Some(foreign) = outside_temp_scratch("explicit") else {
        eprintln!("skipping: the target scratch dir is itself under the temp tree");
        return;
    };
    let config_dir = foreign.join("managed-home");
    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.extra_envs = vec![(
        "CLAUDE_CONFIG_DIR".to_owned(),
        config_dir.to_string_lossy().into_owned(),
    )];
    let child = spawn_fake_claude(opts).expect("spawn fake-claude");
    // Transcript resolution happens before the init frame is emitted.
    let init = child.recv_until(Duration::from_secs(2), |v| is_system_subtype(v, "init"));
    assert!(
        init.is_err(),
        "an explicit non-temp CLAUDE_CONFIG_DIR must stop the fake before init"
    );
    let status = child.wait().expect("wait");
    assert!(!status.success(), "the refusal is a non-zero exit");
    assert!(
        !config_dir.join("projects").exists(),
        "nothing is created at the refused config dir"
    );
    let _ = std::fs::remove_dir_all(&foreign);
}
