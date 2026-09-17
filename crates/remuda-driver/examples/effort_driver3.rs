//! Live repro through the REAL ShellPtyDriver (production wiring) — the
//! owner's "second in-session effort switch fails" on the native carrier.
//!
//!   PROBE_DIR=/tmp/remuda-c-effort3-driver1 \
//!     cargo run -p remuda-driver --example effort_driver3
//!
//! Unlike `effort_repro3` (raw PTY + hand-fed mapper), this drives the exact
//! production path the Node uses: `ShellPtyDriver::start` materializes the
//! launch, the promotion poller binds the real transcript, the effort worker
//! types `/effort`, and `instance.configure` lifecycles + `effort` observations
//! arrive on the driver event stream. Switch words are delivered as
//! `DriverInput::ModelSwitch { effort }`, exactly what the Node sends.
//!
//! SCENARIO=turns (default): ultracode → prompt → high → prompt → ultracode →
//! prompt → max. SCENARIO=back-to-back: the four switches with no prompt.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use remuda_driver::shell_pty::{AgentLaunch, ShellPtyDriver, ShellPtyOptions};
use remuda_driver::{
    Delegation, Driver, DriverKind, LaunchOrigin, ProviderHealth, ProviderKind, ProviderProfile,
};
use remuda_protocol::{
    AgentKind, ClaudeInteractionMode, ClaudePermission, ClaudePermissionMode, ContentBlock,
    DriverInput, InstanceSpec, ModelEffective, ObservationPayload, PermissionMode, PromptInput,
    PromptMode, TextBlock,
};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

fn profile() -> ProviderProfile {
    ProviderProfile {
        id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
        kind: ProviderKind::Anthropic,
        // Empty base_url: the inherited ANTHROPIC_BASE_URL / token env carries
        // the gateway, the same way the native live test runs.
        base_url: String::new(),
        delegation: Delegation::None,
        secret_ref: None,
        models: vec!["haiku".into()],
        health: ProviderHealth::Healthy,
    }
}

fn spec_for(cwd: &Path) -> InstanceSpec {
    let mut spec: InstanceSpec =
        serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
    spec.driver = DriverKind::ShellPty;
    spec.kind = AgentKind::Claude;
    spec.cwd = cwd.to_string_lossy().into_owned();
    spec.model_id = Some("haiku".into());
    spec.permission_mode = PermissionMode::Claude(Box::new(ClaudePermission {
        mode: ClaudePermissionMode::BypassPermissions,
        interaction: ClaudeInteractionMode::NativeTty,
    }));
    spec
}

fn which_claude() -> PathBuf {
    if let Ok(path) = std::env::var("REMUDA_NATIVE_LIVE_CLAUDE") {
        return PathBuf::from(path);
    }
    let path = std::env::var_os("PATH").unwrap();
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("claude");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("no claude binary on PATH")
}

fn effort_switch(word: &str) -> DriverInput {
    DriverInput::ModelSwitch(Box::new(remuda_protocol::ModelSwitchInput {
        model_id: String::new(),
        effective: ModelEffective::NextTurn,
        effort: Some(word.to_owned()),
        permission_mode: None,
    }))
}

fn prompt(text: &str) -> DriverInput {
    DriverInput::Prompt(Box::new(PromptInput {
        mode: PromptMode::NewTurn,
        blocks: vec![ContentBlock::Text(Box::new(TextBlock {
            text: text.into(),
        }))],
        origin: remuda_protocol::InputOrigin::Human,
        native_client_message_id: format!("p-{}", now_ms()),
    }))
}

#[derive(serde::Serialize, Default, Clone)]
struct Record {
    word: String,
    submit_ms: u128,
    /// `instance.configure` lifecycle statuses observed after the submit.
    lifecycles: Vec<String>,
    lifecycle_ms: Vec<u128>,
    /// Effort observation edges observed after the submit.
    edges: Vec<String>,
    edge_ms: Vec<u128>,
    terminal: Option<String>,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let dir = PathBuf::from(
        std::env::var("PROBE_DIR").unwrap_or_else(|_| "/tmp/remuda-c-effort3-driver1".into()),
    );
    assert!(
        dir.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("remuda-c-effort3-driver"))
            && dir.parent().is_some_and(|p| p == Path::new("/tmp"))
            && !dir.is_symlink(),
        "refusing to clean {dir:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let workspace = dir.join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let instance_dir = dir.join("instance");
    let host_claude_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".claude"))
        .expect("HOME");

    let mut options = ShellPtyOptions::agent(
        workspace.clone(),
        AgentKind::Claude,
        AgentLaunch {
            profile: Box::new(profile()),
            launch_dir: instance_dir.join("launch"),
            native_home: host_claude_home.clone(),
            binary: Some(which_claude()),
            origin: LaunchOrigin::Human,
            settings_overlay: None,
        },
    );
    options.emulator = true;
    options.cols = 100;
    options.rows = 40;
    options.claude_home = Some(host_claude_home);
    // Ephemeral /tmp workspace; answer the one-time trust prompt the same way
    // the fixed live scenario does.
    options.auto_trust_workspace = true;

    let driver = ShellPtyDriver::new(options);
    let spec = spec_for(&workspace);
    let mut events = driver.start(spec).await.expect("start").into_events();

    let (tx_config, mut rx_config) = mpsc::unbounded_channel::<(String, u128)>();
    let (tx_edge, mut rx_edge) = mpsc::unbounded_channel::<(String, u128)>();
    let (tx_assistant, mut rx_assistant) = mpsc::unbounded_channel::<()>();
    let t0 = Instant::now();
    tokio::spawn(async move {
        while let Some(obs) = events.recv().await {
            let dt = t0.elapsed().as_millis();
            match &obs.body {
                ObservationPayload::Lifecycle(lifecycle) => {
                    if let remuda_protocol::LifecyclePayload::Native(native) = lifecycle.as_ref()
                        && native.native_name == "instance.configure"
                        && let remuda_protocol::Knowledge::Known { value: status } = &native.status
                    {
                        tx_config.send((status.to_owned(), dt)).unwrap();
                    }
                }
                ObservationPayload::Effort(payload) => {
                    tx_edge
                        .send((
                            format!(
                                "{:?}/ultracode={:?}/source={:?}",
                                payload.effective.name,
                                payload.effective.ultracode,
                                payload.effective.source
                            ),
                            dt,
                        ))
                        .unwrap();
                }
                ObservationPayload::Message(message) => {
                    if message.role == remuda_protocol::MessageRole::Assistant {
                        let _ = tx_assistant.send(());
                    }
                }
                _ => {}
            }
        }
    });

    // Wait for the composer.
    let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        if driver.wait_control().await.is_ok() {
            break;
        }
        if tokio::time::Instant::now() >= ready_deadline {
            panic!("composer never became ready");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    println!("ready +{}ms", t0.elapsed().as_millis());

    // Drain lifecycle/edge events already queued (none expected pre-switch).
    while rx_config.try_recv().is_ok() || rx_edge.try_recv().is_ok() {}

    driver
        .send(prompt("reply with exactly the two characters: ok"))
        .await
        .expect("baseline send");
    let baseline_deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        if rx_assistant.recv().await.is_some() {
            break;
        }
        if tokio::time::Instant::now() >= baseline_deadline {
            panic!("baseline assistant never arrived");
        }
    }
    println!("baseline assistant +{}ms", t0.elapsed().as_millis());
    tokio::time::sleep(Duration::from_millis(2_000)).await;
    while rx_config.try_recv().is_ok() || rx_edge.try_recv().is_ok() {}

    enum Step {
        Switch(&'static str),
        Prompt(&'static str),
    }
    let turns = vec![
        Step::Switch("ultracode"),
        Step::Prompt("reply with exactly the two characters: ok"),
        Step::Switch("high"),
        Step::Prompt("reply with exactly the two characters: ok"),
        Step::Switch("ultracode"),
        Step::Prompt("reply with exactly the two characters: ok"),
        Step::Switch("max"),
    ];
    let back_to_back = vec![
        Step::Switch("ultracode"),
        Step::Switch("high"),
        Step::Switch("ultracode"),
        Step::Switch("max"),
    ];
    let scenario = std::env::var("SCENARIO").unwrap_or_else(|_| "turns".into());
    let steps = if scenario == "back-to-back" {
        &back_to_back
    } else {
        &turns
    };

    let mut records: Vec<Record> = Vec::new();
    for step in steps {
        match step {
            Step::Switch(word) => {
                let word = *word;
                let submit = t0.elapsed().as_millis();
                driver
                    .send(effort_switch(word))
                    .await
                    .expect("configure send");
                println!("SWITCH {word} @ {submit}ms");

                // Collect configure lifecycles + effort edges for 14 s — the
                // driver's own read-back window is 10 s (+5 s ack slack).
                let deadline = tokio::time::Instant::now() + Duration::from_secs(14);
                let mut rec = Record {
                    word: word.to_owned(),
                    submit_ms: submit,
                    ..Default::default()
                };
                loop {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    tokio::select! {
                        Some((status, dt)) = rx_config.recv() => {
                            println!("  +{}ms lifecycle {status}", dt - submit);
                            rec.lifecycles.push(status.clone());
                            rec.lifecycle_ms.push(dt - submit);
                            if status.starts_with("effort-applied:")
                                || status.starts_with("effort-degraded:")
                                || status.starts_with("effort-unsupported")
                                || status.starts_with("effort-control-unavailable")
                            {
                                rec.terminal = Some(status);
                            }
                        }
                        Some((edge, dt)) = rx_edge.recv() => {
                            println!("  +{}ms edge {edge}", dt - submit);
                            rec.edges.push(edge);
                            rec.edge_ms.push(dt - submit);
                        }
                        _ = tokio::time::sleep(remaining) => break,
                    }
                }
                println!(
                    "  => terminal={:?} lifecycles={:?}",
                    rec.terminal, rec.lifecycles
                );
                records.push(rec);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Step::Prompt(text) => {
                while rx_assistant.try_recv().is_ok() {}
                driver.send(prompt(text)).await.expect("prompt send");
                println!("PROMPT {text}");
                let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
                loop {
                    if rx_assistant.recv().await.is_some() {
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        panic!("assistant never arrived for turn");
                    }
                }
                // Drain the ultra attachment / post-turn records.
                tokio::time::sleep(Duration::from_millis(2_500)).await;
                while rx_config.try_recv().is_ok() {}
                let mut drained_edges = 0;
                while rx_edge.try_recv().is_ok() {
                    drained_edges += 1;
                }
                println!("  turn done (+{drained_edges} edges drained)");
            }
        }
    }

    let mut timing = BTreeMap::new();
    for (i, rec) in records.iter().enumerate() {
        timing.insert(
            format!("S{}-{}", i + 1, rec.word),
            serde_json::to_value(rec).unwrap(),
        );
    }
    std::fs::write(
        dir.join("timing.json"),
        serde_json::to_vec_pretty(&timing).unwrap(),
    )
    .unwrap();
    // The idle fast path journals NO lifecycle on success — its outcome rides
    // the oneshot back to the configure call and the effort observation is the
    // only on-stream evidence. So a switch is failed iff it got neither an
    // applied lifecycle nor an effort edge (or it was explicitly degraded).
    let degraded = records
        .iter()
        .filter(|r| {
            let degraded_lifecycle = r
                .terminal
                .as_deref()
                .is_some_and(|status| status.contains("degraded"));
            degraded_lifecycle
                || (r.edges.is_empty()
                    && !r
                        .lifecycles
                        .iter()
                        .any(|s| s.starts_with("effort-applied:")))
        })
        .count();
    let _ = Driver::close(&driver).await;
    if degraded > 0 {
        println!(
            "REPRODUCED: {degraded} switch(es) degraded/missing — {}",
            dir.display()
        );
        std::process::exit(1);
    }
    println!("ALL APPLIED through the real driver — {}", dir.display());
}
