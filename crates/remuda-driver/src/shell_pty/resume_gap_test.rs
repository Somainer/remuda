//! c-usagefu r14 item 5 / r15 item 3: the gap-rollout refusal and the
//! resume-by-id binding driven through the REAL ShellPtyDriver close ->
//! resume -> file-discovery path.
//!
//! A REAL `codex resume <id>` APPENDS new turns to the session's EXISTING
//! rollout, whose session_meta.timestamp is the ORIGINAL session start —
//! strictly before the resume boundary. The continued run therefore binds
//! that rollout BY ID (cwd verified); the timestamp floor stays the rule only
//! for anonymous cwd discovery. Timeline:
//!   1. a hook-shadowed codex launch writes its ORIGINAL rollout (original
//!      session_meta timestamp, first prompt, first turn) and is CLOSED;
//!   2. while the session is gone, a DIFFERENT codex session ("the gap") is
//!      created in the same shadow CODEX_HOME — same cwd, a REAL matching
//!      first user prompt, the newest session_index entry, and UNCLAIMED by
//!      any Remuda adapter: otherwise fully eligible;
//!   3. `start_resumed(S)` spawns the continued run (a NEW census interval at
//!      the resume boundary) whose tail adapter carries the resume id;
//!   4. the test dispatches the matching prompt through the REAL driver
//!      `send` (the production path that records prompt evidence) — no hook
//!      confirmation is injected;
//!   5. new turn records are APPENDED to S's original rollout (codex resume
//!      never rewrites session_meta).
//!
//! The adapter must bind the existing rollout BY ID and emit the appended
//! turn, and must NEVER read the gap. Reverting the production resume-by-id
//! wiring (the `resume_session_id` the shell-pty spawn path hands the adapter)
//! drops discovery back to the timestamp floor: the original-timestamp
//! rollout is rejected, the continued turn never binds, and the test goes
//! RED.

#![cfg(test)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use remuda_protocol::{
    AgentKind, ContentBlock, InputOrigin, InstanceId, InstanceSpec, Knowledge, ObservationPayload,
    PromptInput, PromptMode, TextBlock,
};

use super::*;
use crate::{LaunchOrigin, ProviderHealth, ProviderProfile};
use time::OffsetDateTime;

const CONTINUED_SESSION: &str = "r14-item5-session";
const GAP_SESSION: &str = "r14-item5-gap";
const ORIGIN_TURN: &str = "torig";
const CONTINUED_TURN: &str = "tafter";
const GAP_TURN: &str = "tgap";
/// The real prompt the resumed run dispatches; both rollouts carry it as
/// their first user message, so the resume id is the only differentiator.
const GAP_PROMPT: &str = "r14 item5 resume gap prompt";

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
            // reads the shadow CODEX_HOME directly. The script answers the
            // binary pin's --version probe and then SLEEPS so the resumed run
            // stays alive to receive the REAL prompt send below.
            binary: Some(fake_codex_script(dir)),
            origin: LaunchOrigin::Human,
            native_home_managed: true,
            settings_overlay: None,
        },
    );
    options.instance_id = Some(instance_id.clone());
    options.hooks = Some(HookConfig {
        instance_dir,
        // Never executed by the fake child; embedded only in hook config.
        relay_binary: PathBuf::from("/bin/true"),
        tui: crate::launch::TuiMode::Default,
    });
    options
}

/// A pinned fake codex: answers the version probe (so launch pinning passes),
/// ignores every other argument, and keeps running to accept the prompt send.
fn fake_codex_script(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("fake-codex-item5.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$1\" in --version|version) echo 'codex 0.154.0'; exit 0;; esac\nexec sleep 120\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
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

/// Wait until the wall clock is safely inside a fresh whole second, so the
/// RFC3339-second timestamps on files written afterwards cannot share the
/// resume boundary's second (the floor comparison is whole-second exact).
async fn after_whole_second() {
    let nanos = OffsetDateTime::now_utc().nanosecond();
    tokio::time::sleep(Duration::from_millis((1100 - nanos / 1_000_000) as u64)).await;
}

/// Write a session's ORIGINAL rollout exactly as the first codex run does at
/// session start: session_meta carrying the CURRENT (original) timestamp, the
/// environment and REAL first user prompt, and one priced turn.
fn write_original_rollout(
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
             {{\"timestamp\":\"{at}\",\"ordinal\":1,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_env\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"<environment_context><cwd>{cwd}</cwd></environment_context>\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"environments.environment_context\"]}}}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":2,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_user\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{prompt}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":3,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{turn}\",\"model\":\"gpt-5.4\"}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":4,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn}\"}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":5,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn}\",\"response_id\":\"{turn}-r\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{output}}}}}}}\n\
             {{\"timestamp\":\"{at}\",\"ordinal\":6,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn}\"}}}}\n",
            sid = session_id,
            turn = turn_id,
            output = output,
            cwd = dir.display(),
            prompt = GAP_PROMPT,
        ),
    )
    .unwrap();
    path
}

/// Append a NEW turn to an existing rollout — exactly what `codex resume`
/// does: session_meta is untouched (its timestamp stays the ORIGINAL start),
/// only fresh records with continuing ordinals are appended.
fn append_rollout_turn(
    dir: &Path,
    instance_id: &InstanceId,
    session_id: &str,
    turn_id: &str,
    output: u64,
) {
    let home = shadow_home(dir, instance_id);
    let path = home
        .join("sessions/2026/10/13")
        .join(format!("rollout-{session_id}.jsonl"));
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    use std::io::Write as _;
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":7,\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"id\":\"m_user2\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"{GAP_PROMPT}\"}}],\"internal_chat_message_metadata_passthrough\":{{\"content_item_kinds\":[\"user.text\"]}}}}}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":8,\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{turn_id}\",\"model\":\"gpt-5.4\"}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":9,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{turn_id}\"}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":10,\"type\":\"token_usage_record\",\"payload\":{{\"turn_id\":\"{turn_id}\",\"response_id\":\"{turn_id}-r2\",\"usage\":{{\"input_tokens\":0,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":{output},\"reasoning_output_tokens\":0,\"total_tokens\":{output}}}}}}}"
    )
    .unwrap();
    writeln!(
        file,
        "{{\"timestamp\":\"{at}\",\"ordinal\":11,\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{turn_id}\"}}}}"
    )
    .unwrap();
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

/// The REAL prompt dispatch through the driver's production send path (the
/// same path that records codex prompt evidence for file discovery).
async fn dispatch_real_prompt(driver: &ShellPtyDriver) {
    driver
        .send(DriverInput::Prompt(Box::new(PromptInput {
            mode: PromptMode::NewTurn,
            blocks: vec![ContentBlock::Text(Box::new(TextBlock {
                text: GAP_PROMPT.into(),
            }))],
            origin: InputOrigin::Human,
            native_client_message_id: "r14-item5".into(),
        })))
        .await
        .expect("the resumed live child accepts the prompt");
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

    // START: real hook-shadowed codex launch.
    let driver = ShellPtyDriver::new(options_for(dir.path(), &instance_id, &spec));
    let mut first = driver.start(spec.clone()).await.expect("first start");
    let _ = tokio::time::timeout(Duration::from_millis(300), first.recv()).await;
    // The ORIGINAL rollout exists while the first run is alive, with its real
    // original session_meta timestamp and first turn — exactly the file codex
    // resume will later append to.
    after_whole_second().await;
    write_original_rollout(dir.path(), &instance_id, CONTINUED_SESSION, ORIGIN_TURN, 3);
    index_session(dir.path(), &instance_id, CONTINUED_SESSION);
    // CLOSE: the stop ladder confirms the first child gone and closes the
    // first run's census cohort.
    Driver::close(&driver).await.expect("close");
    drop(first);

    // Fresh whole second, THEN THE GAP: a different codex session appears
    // while the continued session is gone — same cwd, the REAL first prompt
    // the resumed run will dispatch, the newest name-index entry, never
    // claimed by any adapter. Its session also predates the resume boundary.
    after_whole_second().await;
    write_original_rollout(dir.path(), &instance_id, GAP_SESSION, GAP_TURN, 999);
    index_session(dir.path(), &instance_id, GAP_SESSION);

    // RESUME: the production close -> resume path spawns the continued run (a
    // NEW lifetime interval at the resume boundary), adopts its tail, and
    // hands it the native resume id so it binds the EXISTING rollout by id.
    after_whole_second().await;
    let mut resumed = driver
        .start_resumed(spec.clone(), CONTINUED_SESSION.to_owned())
        .await
        .expect("resume spawns the continued run");

    // The resumed codex dispatches the REAL prompt through the production
    // send path — no injected hook confirmation anywhere in this test.
    dispatch_real_prompt(&driver).await;

    // codex resume APPENDS new turn records to the original rollout; its
    // session_meta timestamp stays the ORIGINAL start, before the floor.
    after_whole_second().await;
    append_rollout_turn(
        dir.path(),
        &instance_id,
        CONTINUED_SESSION,
        CONTINUED_TURN,
        7,
    );

    // Drain the resumed run's channel. The adopted adapter must discover the
    // original rollout BY ID and emit the appended turn; byte-0 hydration may
    // also replay the original turn of the SAME session. It must never emit
    // anything from the gap session. Adapter ticks are 250 ms.
    let mut saw_continued = false;
    let mut saw_gap = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
        "the resumed tail binds the original rollout by id and emits the appended turn"
    );
    assert!(
        !saw_gap,
        "the unclaimed, prompt-matching gap rollout must never be attributed \
         to the resumed run: refusal is by the resume boundary, not a claim or hook"
    );

    Driver::close(&driver).await.expect("final close");
}
