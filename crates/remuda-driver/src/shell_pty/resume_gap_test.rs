//! c-usagefu r13 item 5: the gap-rollout refusal driven through the REAL
//! ShellPtyDriver close -> resume path.
//!
//! The r12 regression hand-built the lifetime segments and the resumed
//! CodexAdapter. This test drives the production lifecycle end to end:
//!   1. a hook-configured codex launch starts and is CLOSED;
//!   2. while the session is gone, a DIFFERENT codex session ("the gap") is
//!      created in the same shadow CODEX_HOME — same cwd, a real matching user
//!      prompt, the newest session_index entry, and UNCLAIMED by any Remuda
//!      adapter: otherwise fully eligible;
//!   3. `start_resumed(S)` spawns the continued run (a NEW census interval at
//!      the resume boundary, the real production construction) and adopts its
//!      file-tail adapter.
//!
//! A real codex run identifies the session it is continuing with a
//! SessionStart hook on startup; the test delivers that over the resumed run's
//! REAL hook socket with the REAL per-instance credential read from the
//! materialized shim. The adopted adapter must bind the continued session and
//! emit its records, and must NEVER read the gap session — the refusal depends
//! on the resume boundary alone (the resumed target wins over the newer
//! name-indexed gap), not on a claim or a prompt mismatch.
//!
//! Reverting the production resume-boundary gate (the supervisor hook-confirm
//! of the resumed session) makes the adapter fall back to the newest
//! session_index guess, bind the gap, and journal its usage — the test goes
//! RED there.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use remuda_protocol::{AgentKind, InstanceId, InstanceSpec, Knowledge, ObservationPayload};
use remuda_signal::{HookEnvelope, HookReply, socket};

use super::*;
use crate::{LaunchOrigin, ProviderHealth, ProviderProfile};

const CONTINUED_SESSION: &str = "r13-item5-session";
const GAP_SESSION: &str = "r13-item5-gap";
const CONTINUED_TURN: &str = "tafter";
const GAP_TURN: &str = "tgap";

fn options_for(dir: &Path, instance_id: &InstanceId, spec: &InstanceSpec) -> ShellPtyOptions {
    let instance_dir = dir.join(instance_id.as_id().as_str());
    let mut options = ShellPtyOptions::agent(
        dir.to_path_buf(),
        AgentKind::Codex,
        AgentLaunch {
            profile: Box::new(ProviderProfile {
                id: spec.provider_profile.id.clone(),
                kind: ProviderKind::OpenaiResponses,
                base_url: String::new(),
                delegation: Delegation::None,
                secret_ref: None,
                models: vec!["gpt-5.4".into()],
                health: ProviderHealth::Healthy,
            }),
            launch_dir: instance_dir.join("launch"),
            native_home: dir.join("codex-native-home"),
            // The child never has to be a real codex: the file-tail adapter
            // reads the shadow CODEX_HOME directly. /bin/sh parses the codex
            // argv and exits; the close/stop ladder handles an exited group.
            binary: Some(PathBuf::from("/bin/sh")),
            origin: LaunchOrigin::Human,
            native_home_managed: true,
            settings_overlay: None,
        },
    );
    options.instance_id = Some(instance_id.clone());
    options.hooks = Some(HookConfig {
        instance_dir,
        // Never executed by the /bin/sh child; embedded only in hook config.
        relay_binary: PathBuf::from("/bin/true"),
        tui: crate::launch::TuiMode::Default,
    });
    options
}

fn spec_for(dir: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("../../tests/fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Codex;
    spec.cwd = dir.to_string_lossy().into_owned();
    spec.model_id = Some("gpt-5.4".into());
    spec
}

fn shadow_home(dir: &Path, instance_id: &InstanceId) -> PathBuf {
    dir.join(instance_id.as_id().as_str())
        .join("launch")
        .join("codex-home")
}

/// Test-only: read the per-instance hook credential from the live resumed
/// driver's materialized hook session (the value the resumed child inherits).
async fn hook_credential(driver: &ShellPtyDriver) -> String {
    driver
        .hooks
        .lock()
        .await
        .as_ref()
        .expect("the resumed run holds the hook session")
        .test_hook_credential()
        .expect("the materialized shim env carries the hook credential")
}

/// Write one rollout with a header and a single priced turn into the shadow
/// home's date tree.
fn write_rollout(
    dir: &Path,
    instance_id: &InstanceId,
    session_id: &str,
    turn_id: &str,
    output: u64,
) -> PathBuf {
    let home = shadow_home(dir, instance_id);
    let session_dir = home.join("sessions/2026/10/13");
    std::fs::create_dir_all(&session_dir).unwrap();
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let path = session_dir.join(format!("rollout-{session_id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"timestamp\":\"{at}\",\"ordinal\":0,\"type\":\"session_meta\",\"payload\":{{\"id\":\"{sid}\",\"session_id\":\"{sid}\",\"cwd\":\"{cwd}\",\"timestamp\":\"{at}\"}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":1,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{turn}\",\"model\":\"gpt-5.4\"}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":2,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn}\"}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":3,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"response_id\":\"{turn}-r\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{output}}}}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn}\"}}}}\n",
            sid = session_id,
            turn = turn_id,
            output = output,
            cwd = dir.display(),
        ),
    )
    .unwrap();
    path
}

/// Append a session name to the shadow home's session index (codex appends on
/// session creation; the last line is the name-index fallback's guess).
fn index_session(dir: &Path, instance_id: &InstanceId, session_id: &str) {
    use std::io::Write;
    let path = shadow_home(dir, instance_id).join("session_index.jsonl");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{{\"id\":\"{session_id}\"}}").unwrap();
}

/// Emit a SessionStart on the resumed run's REAL hook socket, authenticated
/// with its REAL shim credential, exactly as the resumed codex process does on
/// startup. `ppid` is the continued run's child process.
async fn emit_resume_session_start(
    driver: &ShellPtyDriver,
    dir: &Path,
    instance_id: &InstanceId,
    ppid: i32,
    transcript_path: &Path,
) {
    let socket_path = dir.join(instance_id.as_id().as_str()).join("hook.sock");
    let credential = hook_credential(driver).await;
    let envelope = HookEnvelope {
        credential,
        event: "SessionStart".to_owned(),
        ppid,
        payload: serde_json::json!({
            "session_id": CONTINUED_SESSION,
            "transcript_path": transcript_path.to_string_lossy(),
            "cwd": dir.to_string_lossy(),
        }),
    };
    // A short retry: the resumed HookServer binds during spawn, which the
    // returned channel and the state read may just precede.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let reply = socket::send_event(&socket_path, &envelope, Duration::from_secs(2)).await;
        if reply != HookReply::empty() {
            // SessionStart carries no decision, so any non-empty reply is
            // unexpected but proves delivery.
            return;
        }
        if socket_path.exists() {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("resumed hook socket never came up at {socket_path:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn observation_turn(observation: &remuda_protocol::Observation) -> Option<&str> {
    match &observation.source.native_turn_id {
        Knowledge::Known { value } => Some(value.as_str()),
        _ => None,
    }
}

#[tokio::test]
async fn a_real_close_resume_refuses_an_unclaimed_otherwise_eligible_gap_rollout() {
    let dir = tempfile::tempdir().unwrap();
    let instance_id = InstanceId::new();
    let spec = spec_for(dir.path());

    // START: real hook-configured codex launch.
    let driver = ShellPtyDriver::new(options_for(dir.path(), &instance_id, &spec));
    let mut first = driver.start(spec.clone()).await.expect("first start");
    let _ = tokio::time::timeout(Duration::from_millis(300), first.recv()).await;
    // CLOSE: the stop ladder confirms the first child gone and closes the
    // first run's census cohort.
    Driver::close(&driver).await.expect("close");
    drop(first);

    // THE GAP: a different codex session appears while the continued session
    // is gone — same cwd, a real prompt, the newest name-index entry. No
    // Remuda adapter ever claims it.
    write_rollout(dir.path(), &instance_id, GAP_SESSION, GAP_TURN, 999);
    index_session(dir.path(), &instance_id, GAP_SESSION);

    // RESUME: the production close -> resume path spawns the continued run (a
    // NEW lifetime interval at the resume boundary) and adopts its tail.
    let mut resumed = driver
        .start_resumed(spec.clone(), CONTINUED_SESSION.to_owned())
        .await
        .expect("resume spawns the continued run");
    let resumed_pgid = driver.state().await.unwrap().pgid.load(Ordering::SeqCst);

    // The continued session's rollout exists only AFTER resume (codex appends
    // to the same session on `codex resume S`).
    let continued_path = write_rollout(
        dir.path(),
        &instance_id,
        CONTINUED_SESSION,
        CONTINUED_TURN,
        7,
    );

    // The resumed codex identifies the session it is continuing. That hook is
    // the resume boundary: the gap is newer in the index, but it is NOT S.
    emit_resume_session_start(
        &driver,
        dir.path(),
        &instance_id,
        resumed_pgid,
        &continued_path,
    )
    .await;

    // Drain the resumed run's channel. The adopted adapter must emit the
    // continued turn and NEVER anything from the gap session. Adapter ticks are
    // 250 ms; bound the observation window generously above a few ticks.
    let mut saw_continued = false;
    let mut saw_gap = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, resumed.recv()).await {
            Ok(Some(observation)) => {
                let turn = observation_turn(&observation);
                if turn == Some(GAP_TURN) {
                    saw_gap = true;
                }
                if matches!(&observation.body, ObservationPayload::Usage(_))
                    && turn == Some(CONTINUED_TURN)
                {
                    saw_continued = true;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_continued,
        "the adopted resumed tail binds the continued session and emits its turn"
    );
    assert!(
        !saw_gap,
        "the unclaimed, otherwise eligible gap rollout must never be attributed \
         to the resumed run: refusal is by the resume boundary, not a claim"
    );

    Driver::close(&driver).await.expect("final close");
}
