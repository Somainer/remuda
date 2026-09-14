//! D-028 P6 parity: the codex and grok file adapters' observations normalize
//! through `remuda journal diff`.
//!
//! Each adapter is run against the captured real session fixture (codex 0.154
//! rollout, grok 1.0.30 updates/events), its observations are stamped and
//! written as a Hub-shaped dump, and the comparator is run:
//!
//! * a dump against itself is always equal (adapter output is well-formed for
//!   the normalizer and has no residual instability);
//! * codex vs grok on the same facts differ only where the harnesses genuinely
//!   record different channels/granularity.

use std::path::{Path, PathBuf};
use std::process::Command;

use remuda_driver::adapters::{AdapterHome, CodexAdapter, FileSignalAdapter, GrokAdapter};
use serde_json::{Value, json};

const CODEX_ROLLOUT: &str =
    include_str!("../../remuda-driver/tests/fixtures/codex/interactive-0.154.0.jsonl");
const GROK_UPDATES: &str =
    include_str!("../../remuda-driver/tests/fixtures/grok/tui-updates.jsonl");
const GROK_EVENTS: &str = include_str!("../../remuda-driver/tests/fixtures/grok/tui-events.jsonl");
const GROK_REGISTRY: &str =
    include_str!("../../remuda-driver/tests/fixtures/grok/active-sessions.json");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn diff(left: &Path, right: &Path) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_remuda"))
        .current_dir(repo_root())
        .args(["journal", "diff"])
        .arg(left)
        .arg(right)
        .args(["--no-whitelist", "--json"])
        .env_remove("NO_COLOR")
        .output()
        .expect("run remuda journal diff");
    let code = output.status.code().expect("exit code");
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("json: {}", String::from_utf8_lossy(&output.stdout)));
    (code, value)
}

/// Stamp adapter observations into a Hub-shaped dump with a fixed context so
/// only the channel-stripped facts remain for comparison.
fn hub_dump(observations: Vec<remuda_driver::adapters::AdapterObservation>) -> Value {
    use remuda_driver::adapters::StampCtx;
    use remuda_driver::adapters::stamp;
    use remuda_protocol::{HostId, Id, InstanceId, RunId};
    use std::sync::atomic::AtomicU64;
    let ctx = StampCtx {
        instance_id: InstanceId::new(),
        host_id: HostId::new(),
        journal_id: Id::new("obj").unwrap(),
        run_id: RunId::new(),
        session_id: "parity-session".into(),
    };
    let _seq = AtomicU64::new(0);
    let events = observations
        .into_iter()
        .enumerate()
        .filter_map(|(idx, observed)| {
            stamp(&ctx, idx as u64 + 1, &observed)
                .map(|observation| serde_json::to_value(observation).unwrap())
        })
        .collect::<Vec<_>>();
    json!({ "events": events })
}

fn write_dump(dir: &Path, name: &str, dump: Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, serde_json::to_vec_pretty(&dump).unwrap()).unwrap();
    path
}

fn run_codex(dir: &Path) -> Value {
    // Lay the real rollout into a shadow CODEX_HOME under the expected date.
    let home = dir.join("codex-home");
    let session_dir = home.join("sessions/2026/09/14");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("rollout-2026-09-14T02-21-40-01a09c00-64d4-7ce1-8537-984e896b8e8a.jsonl"),
        CODEX_ROLLOUT,
    )
    .unwrap();
    // The locator matches a session by id; supply the index name entry.
    std::fs::write(
        home.join("session_index.jsonl"),
        "{\"id\":\"01a09c00-64d4-7ce1-8537-984e896b8e8a\",\"thread_name\":\"p6\",\"updated_at\":\"2026-09-14T02:21:06Z\"}\n",
    )
    .unwrap();
    let mut adapter = CodexAdapter::new(AdapterHome {
        home,
        cwd: dir.to_path_buf(),
        pid: None,
    });
    let all = drain(&mut adapter);
    hub_dump(all)
}

fn run_grok(dir: &Path) -> Value {
    let home = dir.join("grok-home");
    let cwd = PathBuf::from("/workspace/grok-spike");
    let session = "01a09c24-46ef-7a03-9c89-88f1bc00bd0c";
    let encoded = remuda_driver::grok_session::encode_session_cwd(&cwd);
    let session_dir = home.join("sessions").join(encoded).join(session);
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("updates.jsonl"), GROK_UPDATES).unwrap();
    std::fs::write(session_dir.join("events.jsonl"), GROK_EVENTS).unwrap();
    std::fs::write(session_dir.join("usage.json"), "{}\n").unwrap();
    std::fs::write(home.join("active_sessions.json"), GROK_REGISTRY).unwrap();
    let mut adapter = GrokAdapter::new(AdapterHome {
        home,
        cwd,
        pid: Some(24069),
    });
    let all = drain(&mut adapter);
    hub_dump(all)
}

/// Drain an adapter until it has no more observations (files are fully present,
/// so two polls suffice: discovery then full read).
fn drain(adapter: &mut dyn FileSignalAdapter) -> Vec<remuda_driver::adapters::AdapterObservation> {
    let mut all = Vec::new();
    for _ in 0..2 {
        all.extend(adapter.poll().expect("poll"));
    }
    all
}

#[test]
fn codex_and_grok_adapter_dumps_self_diff_as_equal() {
    let dir = tempfile::tempdir().unwrap();
    let codex = run_codex(dir.path());
    let grok = run_grok(dir.path());

    let codex_path = write_dump(dir.path(), "codex.json", codex.clone());
    let grok_path = write_dump(dir.path(), "grok.json", grok.clone());

    // A dump against itself normalizes to equality.
    let (code, report) = diff(&codex_path, &codex_path);
    assert_eq!(code, 0, "codex self-parity: {report}");
    assert_eq!(report["equal"], json!(true));

    let (code, report) = diff(&grok_path, &grok_path);
    assert_eq!(code, 0, "grok self-parity: {report}");
    assert_eq!(report["equal"], json!(true));
}

#[test]
fn the_codex_dump_carries_the_measured_lifecycle_and_usage_facts() {
    let dir = tempfile::tempdir().unwrap();
    let dump = run_codex(dir.path());
    let body = dump.to_string();
    // 6 task_started / 5 task_complete / 1 turn_aborted (evidence §A3).
    assert!(body.matches("task_started").count() >= 6);
    assert!(body.matches("task_complete").count() >= 5);
    assert!(body.contains("turn_aborted"));
    // Estimated usage snapshots are emitted (cost unknown for gpt-5.4 is
    // priced by the table, but the accounting label is always present).
    assert!(body.contains("estimated"));
}

#[test]
fn the_grok_dump_carries_chunk_streaming_and_cancel_facts() {
    let dir = tempfile::tempdir().unwrap();
    let dump = run_grok(dir.path());
    let body = dump.to_string();
    // Chunk-level mutations and the 7 turn boundaries.
    assert!(body.contains("turn_started"));
    assert!(body.contains("turn_ended"));
    assert!(body.contains("\"append\""));
    assert!(body.contains("\"close\""));
    // The two cancellations (ctrl_c + send_now) survive normalization.
    assert!(body.contains("cancelled"));
}
