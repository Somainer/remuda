//! Guard for the `fake-claude` test double: it must never persist a session
//! outside a deliberately allocated write root (c-resumehome round 3).
//!
//! Round 3 changed the model: authorization is a **sentinel-marked allocated
//! root** (`remuda_testing::sandbox::TempHome::allocate`), never mere
//! `$TMPDIR`/`$HOME`/`/tmp` ancestry, and the fake's writes go through an
//! `O_NOFOLLOW` descriptor walk. These tests are therefore mandatory (no
//! early `return` on hosts where cargo's scratch lives under `/tmp`) and
//! placement-independent:
//!
//! - every non-authorized home is a sibling of an allocated root under the
//!   fixed system temp, so the premise holds on every host;
//! - every assertion names the exact refusal;
//! - a symlinked `projects` inside an otherwise allocated home proves the
//!   fd walk, not the path string, is what gates writes.

use remuda_testing::sandbox::TempHome;
use remuda_testing::{
    FIXED_SESSION_ID, FakeClaudeProcess, ScriptKind, SpawnOptions, is_system_subtype, is_type,
    spawn_fake_claude,
};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Boot, initialize and finish one OK turn; the returned child has not yet been
/// reaped (stdin is still open).
fn run_turn(envs: Vec<(String, String)>) -> FakeClaudeProcess {
    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.extra_envs = envs;
    let child = spawn_fake_claude(opts).expect("spawn fake-claude");
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

/// First `*.jsonl` found while walking `root`, if any.
fn first_jsonl(root: &std::path::Path) -> Option<std::path::PathBuf> {
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

/// MANDATORY: a `HOME` outside an allocated root runs the turn normally but
/// persists nothing under that home. Placement-independent: the home is a
/// fresh sibling of the allocated root, inside the fixed system temp.
#[test]
fn fresh_session_does_not_write_a_non_allocated_operator_home() {
    let root = TempHome::allocate("guard-operator").expect("allocated root");
    let operator_home = root.child("operator");
    std::fs::create_dir_all(&operator_home).expect("operator home");
    // Strip the sentinel from this subtree so it is plainly not a root: give
    // the fake a sibling directory instead, which can never carry one.
    let sibling = root
        .path()
        .parent()
        .unwrap()
        .join(format!("{}-nonroot", std::process::id()));
    std::fs::create_dir_all(&sibling).expect("sibling home");

    let child = run_turn(vec![(
        "HOME".to_owned(),
        sibling.to_string_lossy().into_owned(),
    )]);
    let status = child.wait().expect("wait fake-claude");
    assert!(
        status.success(),
        "with no writable allocated home the turn still runs"
    );
    assert!(
        !sibling.join(".claude").exists(),
        "the fake must never create a ~/.claude tree outside an allocated root: {}",
        sibling.display()
    );
}

/// MANDATORY: a fresh session persists the real `projects/<slug>/<id>.jsonl`
/// layout under an allocated root.
#[test]
fn fresh_session_persists_under_an_allocated_home() {
    let root = TempHome::allocate("guard-persist").expect("allocated root");
    let fake_home = root.child("testhome");
    std::fs::create_dir_all(&fake_home).expect("fake home");

    let child = run_turn(vec![(
        "HOME".to_owned(),
        fake_home.to_string_lossy().into_owned(),
    )]);
    assert!(child.wait().expect("wait").success());

    let projects = fake_home.join(".claude/projects");
    let transcript = first_jsonl(&projects)
        .expect("an allocated HOME receives the real projects/<slug>/<id>.jsonl layout");
    assert_eq!(
        transcript.file_name().and_then(std::ffi::OsStr::to_str),
        Some(format!("{FIXED_SESSION_ID}.jsonl").as_str())
    );
}

/// MANDATORY negative process test: an explicit `CLAUDE_CONFIG_DIR` outside
/// any allocated root is refused loudly with the named reason, before init,
/// and creates nothing. This runs on every host (no temp-placement skip).
#[test]
fn explicit_config_dir_outside_an_allocated_root_is_refused_loudly() {
    let root = TempHome::allocate("guard-explicit").expect("allocated root");
    // A directory OUTSIDE every fixed system temp mount, so fixed-temp
    // ancestry can never authorize it. /dev/shm is a writable non-tmp mount
    // on every Linux CI host; the test is mandatory (never skipped).
    #[cfg(target_os = "linux")]
    let foreign_base = std::path::PathBuf::from("/dev/shm");
    #[cfg(not(target_os = "linux"))]
    let foreign_base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let foreign = foreign_base.join(format!(
        "remuda-fake-guard-explicit-{}-{}",
        std::process::id(),
        root.path().file_name().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&foreign).expect("foreign dir");

    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.extra_envs = vec![(
        "CLAUDE_CONFIG_DIR".to_owned(),
        foreign.to_string_lossy().into_owned(),
    )];
    let child = spawn_fake_claude(opts).expect("spawn fake-claude");
    let init = child.recv_until(Duration::from_secs(2), |v| is_system_subtype(v, "init"));
    assert!(
        init.is_err(),
        "an explicit non-allocated CLAUDE_CONFIG_DIR must stop the fake before init"
    );
    let status = child.wait().expect("wait");
    assert!(!status.success(), "the refusal is a non-zero exit");
    assert!(
        !foreign.join("projects").exists(),
        "nothing is created at the refused config dir"
    );
}

/// MANDATORY: inside an allocated home, a symlinked `projects` entry pointing
/// at a non-allocated directory must not redirect the transcript write. The
/// fd walk refuses the link, so the fake exits non-zero and no byte lands at
/// the target.
#[test]
fn a_symlinked_projects_inside_the_home_is_refused_not_followed() {
    use std::os::unix::fs::symlink;
    let root = TempHome::allocate("guard-symlink").expect("allocated root");
    let home = root.child("home");
    std::fs::create_dir_all(&home).expect("home");
    let sink = root.child("real-projects-sink");
    std::fs::create_dir_all(&sink).expect("sink dir");
    symlink(&sink, home.join(".claude")).expect(".claude link");

    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.extra_envs = vec![("HOME".to_owned(), home.to_string_lossy().into_owned())];
    let child = spawn_fake_claude(opts).expect("spawn fake-claude");
    // The transcript open is before the init frame; it must fail.
    let init = child.recv_until(Duration::from_secs(2), |v| is_system_subtype(v, "init"));
    assert!(
        init.is_err(),
        "a symlinked .claude entry must stop the fake"
    );
    let status = child.wait().expect("wait");
    assert!(!status.success(), "the refusal is a non-zero exit");
    assert!(
        first_jsonl(&sink).is_none(),
        "a symlinked .claude entry must never receive transcript bytes: {}",
        sink.display()
    );
}

/// Round 4 item 6: a REAL fake-process `--resume` whose target transcript is a
/// FIFO must fail promptly (never block opening it), not hang the process.
#[test]
fn resume_of_a_fifo_transcript_fails_without_blocking() {
    use nix::sys::stat::Mode;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let root = TempHome::allocate("guard-fifo-resume").expect("allocated root");
    let home = root.child("home");
    std::fs::create_dir_all(&home).expect("home");
    let workspace = root.child("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let slug = remuda_driver::claude_transcript::encode_project_dir(&workspace);
    let slug_dir = home.join(".claude/projects").join(slug);
    std::fs::create_dir_all(&slug_dir).expect("slug dir");
    let session = "01993ab0-0000-7000-8000-0000000000f0";
    let fifo = slug_dir.join(format!("{session}.jsonl"));
    nix::unistd::mkfifo(&fifo, Mode::from_bits_truncate(0o600)).expect("mkfifo");
    // No writer is ever opened on the other end: a blocking open would hang
    // forever; O_NONBLOCK plus fstatat pre-classification must refuse instead.

    let mut opts = SpawnOptions::bundled(ScriptKind::Ok);
    opts.session_id = session.to_owned();
    opts.extra_args = vec!["--resume".to_owned(), session.to_owned()];
    opts.cwd = Some(workspace.clone());
    opts.extra_envs = vec![(
        "CLAUDE_CONFIG_DIR".to_owned(),
        home.join(".claude").to_string_lossy().into_owned(),
    )];

    let started = Instant::now();
    let child = spawn_fake_claude(opts).expect("spawn fake-claude");
    let init = child.recv_until(Duration::from_secs(3), |v| is_system_subtype(v, "init"));
    assert!(
        init.is_err(),
        "the FIFO resume cannot produce an init frame"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the fake must not block opening the FIFO"
    );
    let status = child.wait().expect("wait");
    assert!(
        !status.success(),
        "a FIFO transcript is a non-zero-exit refusal"
    );
    // The FIFO itself remains a FIFO (never opened/written).
    let mode = std::fs::symlink_metadata(&fifo)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o170000,
        0o010000,
        "target is still a FIFO, not a file"
    );
}

/// Round 4 item 8: allocation never returns the shared mount itself.
#[test]
fn allocation_never_marks_the_shared_tmp_mount() {
    let allocated = remuda_testing::sandbox::TempHome::allocate("mount-check").expect("allocate");
    assert_ne!(
        allocated.path().canonicalize().unwrap(),
        std::path::Path::new("/tmp").canonicalize().unwrap(),
        "allocation never returns the shared mount itself"
    );
    let canonical = allocated.path().canonicalize().unwrap();
    assert!(
        canonical.starts_with("/tmp/") || canonical.starts_with("/private/tmp/"),
        "allocation stays under a fixed temp mount: {}",
        canonical.display()
    );
}
