//! Herdr-backed `claude-pty` driver (plan M0-09).
//!
//! Starts a full [`LaunchRecipe`] in an isolated Herdr pane via `agent.start`,
//! never `--bare`. Restore rebuilds the pane with `--resume <session>` and does
//! not rely on Herdr's agent-resume argv. Structured identity comes from a
//! SessionStart hook overlay; screen status is `pane.agent_status_changed`.

use crate::binary::{BinaryPin, hash_bytes, pin_binary};
use crate::capabilities::{ADAPTER_VERSION, capability_snapshot};
use crate::claude_print::TranscriptMapper;
use crate::driver::{Driver, DriverAck, RunHandle};
use crate::error::{DriverError, DriverResult};
use crate::materializer::{
    BinarySource, LaunchOrigin, MaterializeRequest, SessionAction, materialize,
};
use crate::profile::{EnvFileSecretBroker, ProviderProfile, SecretBroker};
use crate::pty_interaction::PtyInteractions;
use crate::recipe::{FileLifetime, FileRole, LaunchRecipe, MaterializedFile};
use async_trait::async_trait;
use remuda_herdr::{
    AgentPromptParams, AgentReadParams, AgentStartParams, AgentStatus, Client, EventKind,
    EventStream, HerdrServer, PaneSplitParams, ReadFormat, ReadSource, SplitDirection,
    Subscription, TerminalObserver, WorkspaceCreateParams,
};
use remuda_protocol::{
    AgentKind, ClaudeRef, Completeness, ContentBlock, Digest, DriverInput, DriverKind,
    EffortSelection, EventId, HerdrRef, HerdrRepresentation, HerdrServer as HerdrPin, HostId, Id,
    InstanceId, InstanceSpec, InteractionAnswer, InteractionId, Knowledge, LifecyclePayload,
    LifecycleTopic, ModelCatalogInfo, NativeLifecycle, NativeRef, NativeRequestKey,
    NativeTerminalFrame, Observation, ObservationPayload, ObservationSource, PtyBackend,
    PtyCarrier, RawRef, Redaction, RunId, SchemaVersion, Severity, SourceChannel, SourceCursor,
    SourceDelivery, Timestamp, TranscriptRef, TtyOutput, TtyRepresentation, U64,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tracing::info;

const HOOK_SCRIPT: &str = r#"#!/bin/sh
set -eu
out="$1"
raw="${out}.raw"
cat >"$raw"
if command -v python3 >/dev/null 2>&1; then
python3 - "$raw" "$out" <<'PY'
import json, sys
raw, out = sys.argv[1], sys.argv[2]
with open(raw, encoding="utf-8") as handle:
    data = json.load(handle)
payload = {
    "session_id": data.get("session_id") or data.get("sessionId"),
    "transcript_path": data.get("transcript_path") or data.get("transcriptPath"),
    "cwd": data.get("cwd"),
    "hook_event_name": data.get("hook_event_name") or data.get("hookEventName"),
}
with open(out, "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
    handle.write("\n")
PY
else
  cp "$raw" "$out"
fi
"#;

/// Construction options for [`ClaudePtyDriver`].
pub struct ClaudePtyOptions {
    /// Provider profile used at materialize time.
    pub profile: ProviderProfile,
    /// Directory for 0700 launch files (settings overlay + SessionStart hook).
    pub launch_dir: PathBuf,
    /// Registered `CLAUDE_CONFIG_DIR`.
    pub native_home: PathBuf,
    /// Binary to pin (the `claude` executable; Herdr locates it by kind).
    pub binary: BinarySource,
    /// Launch origin; bot/dispatcher cannot request bypass (D-011).
    pub origin: LaunchOrigin,
    /// Isolated Herdr session name. Never `"default"` unless explicit.
    pub session_name: String,
    /// If set, API socket is `{socket_dir}/herdr.sock` (tests bind fake-herdr here).
    pub socket_dir: Option<PathBuf>,
    /// Override the `herdr` binary used for RPC-adjacent terminal observe.
    pub herdr_binary: Option<PathBuf>,
    /// Secret resolver. Defaults to [`EnvFileSecretBroker`].
    pub broker: Arc<dyn SecretBroker>,
    /// Extra env forwarded onto the Herdr workspace/pane (no secrets logged).
    pub extra_env: BTreeMap<String, String>,
    /// Hub-issued instance context; separate from ordinary environment overlays.
    pub agent_mcp: Option<crate::agent_mcp::AgentMcpContext>,
    /// Override `--setting-sources`.
    pub setting_sources: Option<Vec<String>>,
    /// `agent.start` timeout in milliseconds.
    pub agent_start_timeout_ms: u64,
    /// Let Claude resolve its default config directory instead of exporting
    /// `CLAUDE_CONFIG_DIR`.
    pub inherit_default_config: bool,
    /// Node verified that cwd belongs to a registered workspace and enabled automatic trust.
    pub auto_trust_registered_workspace: bool,
    /// Mark the scoped `CLAUDE_CONFIG_DIR` as already onboarded before launch,
    /// mirroring the host user's theme/disclaimer flags but never credentials.
    /// Disabled when the launch inherits the user's own default config, which
    /// is onboarded already.
    pub seed_onboarding: bool,
    /// Where the host user's Claude configuration lives, as the source of the
    /// allowlisted flags copied by `seed_onboarding`. Defaults to `None`, which
    /// seeds only `hasCompletedOnboarding` and reads no file outside the scoped
    /// directory; the Node opts in with [`HostClaudeConfig::from_env`] so that
    /// merely constructing a driver never reads the operator's home.
    ///
    /// [`HostClaudeConfig::from_env`]: crate::claude_onboarding::HostClaudeConfig::from_env
    pub host_claude_config: Option<crate::claude_onboarding::HostClaudeConfig>,
    /// Host-validated `--settings` overlay. Contents are never logged.
    pub settings_overlay_path: Option<PathBuf>,
    /// The Node instance this driver serves, when a Node built it.
    ///
    /// Observation payloads carry ids derived under this scope
    /// (`Id::derive("obj", <instance id>, <native id>)`), and the store's
    /// append rewrites envelope identity but not those. A throwaway id here
    /// therefore makes the driver's own nodes unaddressable by anything the
    /// Node derives for the same session (c-wfdrill2 C). `None` outside a Node.
    pub instance_id: Option<InstanceId>,
}

impl ClaudePtyOptions {
    /// Isolated `remuda-test` session, unknown origin, env/file broker.
    pub fn new(
        profile: ProviderProfile,
        launch_dir: PathBuf,
        native_home: PathBuf,
        binary: BinarySource,
    ) -> Self {
        Self {
            profile,
            launch_dir,
            native_home,
            binary,
            origin: LaunchOrigin::default(),
            session_name: "remuda-test".into(),
            socket_dir: None,
            herdr_binary: None,
            broker: Arc::new(EnvFileSecretBroker::env_only()),
            extra_env: BTreeMap::new(),
            agent_mcp: None,
            setting_sources: None,
            agent_start_timeout_ms: 120_000,
            inherit_default_config: false,
            settings_overlay_path: None,
            auto_trust_registered_workspace: false,
            seed_onboarding: true,
            host_claude_config: None,
            instance_id: None,
        }
    }
}

struct PtyLive {
    ctx: crate::claude_pty::ObsCtx,
    client: Client,
    pane_id: String,
    recipe: LaunchRecipe,
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    session_id: String,
    native_store_id: Id,
    herdr_pin: HerdrPin,
    herdr_session: String,
    transcript_path: Arc<std::sync::Mutex<Option<String>>>,
    events: mpsc::Sender<Observation>,
    /// §9.1 in-session effort switch coordination. Kept on the live handle so
    /// resume/attach paths share the same read-back rendezvous.
    #[allow(dead_code)]
    effort_bridge: Arc<crate::effort::EffortBridge>,
    effort_queue: Arc<crate::effort::EffortQueue>,
    effort_worker: Option<JoinHandle<()>>,
    /// §9.1 in-session `/model` switch coordination.
    #[allow(dead_code)]
    model_bridge: Arc<crate::model::ModelBridge>,
    model_queue: Arc<crate::model::ModelQueue>,
    model_worker: Option<JoinHandle<()>>,
    /// In-session permission-mode switch coordination (shift+tab wheel).
    #[allow(dead_code)]
    permission_bridge: Arc<crate::permission::PermissionBridge>,
    permission_queue: Arc<crate::permission::PermissionQueue>,
    permission_worker: Option<JoinHandle<()>>,
    status_task: Option<JoinHandle<()>>,
    interactions: Arc<PtyInteractions>,
    interaction_task: JoinHandle<()>,
    hook_task: Option<JoinHandle<()>>,
    transcript_task: Option<JoinHandle<()>>,
    tty_task: Option<JoinHandle<()>>,
    /// Current alt-screen mode seen by the attach relay's byte scanner.
    alt_screen: Arc<AtomicBool>,
    closed: bool,
}

/// Native Claude TUI hosted in a Herdr pane.
pub struct ClaudePtyDriver {
    resources: crate::pty_resource::PtyResources,
    options: ClaudePtyOptions,
    inner: Mutex<Option<PtyLive>>,
    last_recipe: Mutex<Option<LaunchRecipe>>,
    /// Last successfully materialized spec; `resume` re-materializes from this.
    last_spec: Mutex<Option<InstanceSpec>>,
    closed: AtomicBool,
    seq: Arc<AtomicU64>,
}

impl ClaudePtyDriver {
    /// Build a driver from explicit options.
    pub fn new(options: ClaudePtyOptions) -> Self {
        Self {
            resources: Default::default(),
            options,
            inner: Mutex::new(None),
            last_recipe: Mutex::new(None),
            last_spec: Mutex::new(None),
            closed: AtomicBool::new(false),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Seed the scoped config dir so the native first-run flow never starts.
    ///
    /// Returns the outcome when anything changed; `None` when seeding is off
    /// (an inherited default config is already onboarded) or the directory
    /// needed no change. Never reads or copies credentials.
    fn seed_onboarding(&self) -> DriverResult<Option<crate::claude_onboarding::SeedOutcome>> {
        if !self.options.seed_onboarding || self.options.inherit_default_config {
            return Ok(None);
        }
        let outcome = crate::claude_onboarding::seed_scoped_config(
            &self.options.native_home,
            self.options.host_claude_config.as_ref(),
        )?;
        Ok((!outcome.is_noop()).then_some(outcome))
    }

    /// Last persisted recipe, if any. Contains no secrets or prompts.
    pub async fn persisted_recipe(&self) -> Option<LaunchRecipe> {
        self.last_recipe.lock().await.clone()
    }

    /// Transcript path recorded by the SessionStart hook, if it has fired.
    pub async fn session_transcript(&self) -> Option<std::path::PathBuf> {
        if let Some(inner) = self.inner.lock().await.as_ref()
            && let Ok(guard) = inner.transcript_path.lock()
            && let Some(path) = guard.as_ref()
            && !path.is_empty()
        {
            return Some(PathBuf::from(path));
        }
        read_session_meta_transcript(&self.options.launch_dir)
    }

    /// Open a read-only Herdr terminal observer for the live pane.
    pub async fn open_observer(&self) -> DriverResult<TerminalObserver> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        TerminalObserver::open(&live.client, &live.pane_id, 80, 24)
            .await
            .map_err(map_herdr)
    }

    async fn launch(&self, spec: InstanceSpec, session: SessionAction) -> DriverResult<RunHandle> {
        if spec.driver != DriverKind::ClaudePty {
            return Err(DriverError::InvalidLaunchSpec(
                "ClaudePtyDriver requires driverKind claude-pty".into(),
            ));
        }
        if self.options.origin == LaunchOrigin::Bot {
            reject_bot_bypass(&spec)?;
        }
        let session_id = session.session_id().to_string();
        let request = MaterializeRequest {
            spec: &spec,
            profile: &self.options.profile,
            launch_dir: self.options.launch_dir.clone(),
            native_home: self.options.native_home.clone(),
            session,
            launch_id: Id::new("launch")?,
            binary: self.options.binary.clone(),
            setting_sources: self.options.setting_sources.clone(),
            origin: self.options.origin,
            settings_overlay_path: self.options.settings_overlay_path.clone(),
            secret_policy: None,
        };
        let mut recipe = materialize(&request)?;
        apply_tty_bypass_flag(&mut recipe);
        refuse_bare(&recipe.argv)?;
        recipe = inject_session_start_hook(&mut recipe, &self.options.launch_dir)?;
        // A previous run's hook file cannot prove this carrier finished startup.
        match std::fs::remove_file(self.options.launch_dir.join("session-meta.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        apply_tty_bypass_flag(&mut recipe);
        refuse_bare(&recipe.argv)?;
        // A scoped CLAUDE_CONFIG_DIR is fresh on first use, so the native CLI
        // would run its first-run wizard instead of mounting a composer: Herdr
        // calls that pane idle, no SessionStart hook fires, and a queued prompt
        // waits forever. Seed the flags that gate the wizard before launch.
        let seeded = self.seed_onboarding()?;
        *self.last_recipe.lock().await = Some(recipe.clone());
        *self.last_spec.lock().await = Some(spec.clone());

        let session_name = herdr_session_name(&spec, &self.options.session_name);
        if session_name == "default" && self.options.socket_dir.is_none() {
            return Err(DriverError::InvalidLaunchSpec(
                "refusing the default herdr session unless socket_dir is explicit".into(),
            ));
        }
        let server = HerdrServer::ensure(&session_name, self.options.socket_dir.clone())
            .await
            .map_err(map_herdr)?;
        let client = bind_client(&server, &self.options)?;
        let pong = client.ping().await.map_err(map_herdr)?;
        let herdr_pin = herdr_pin(&client, &pong.version, pong.protocol)?;

        let mut env = HashMap::new();
        if !self.options.inherit_default_config {
            env.insert("CLAUDE_CONFIG_DIR".into(), recipe.native_home.clone());
        }
        for (key, value) in &self.options.extra_env {
            if crate::child_env::is_denied(key) {
                continue;
            }
            env.insert(key.clone(), value.clone());
        }
        if let Some(context) = &self.options.agent_mcp {
            env.extend(context.environment()?);
        }

        // §9.1 model list: discover the gateway cache / settings the launched
        // session can actually switch to, before the pane exists. The scoped
        // config dir is checked first; the operator's ~/.claude cache backs it
        // up when discovery has not populated the scoped dir yet.
        let host_config_dir =
            crate::claude_onboarding::HostClaudeConfig::from_env().and_then(|host| {
                host.user_settings
                    .parent()
                    .map(std::path::Path::to_path_buf)
            });
        let env_pairs: Vec<(&str, &str)> = env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let model_catalog = crate::model_discovery::resolve_catalog(
            Some(std::path::Path::new(&recipe.native_home)),
            host_config_dir.as_deref(),
            Some(&std::path::Path::new(&recipe.native_home).join("settings.json")),
            &env_pairs,
            spec.model_id.as_deref(),
        );
        // Owned snapshot for the catalog re-resolve task, which outlives the
        // move of `env` into the workspace/pane creation below.
        let refresh_env: Vec<(String, String)> =
            env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

        let created = self
            .resources
            .create_workspace(
                &client,
                &session_name,
                WorkspaceCreateParams {
                    cwd: Some(recipe.cwd.clone()),
                    env: env.clone(),
                    focus: false,
                    label: Some(format!("remuda-{}", uuid::Uuid::now_v7())),
                    source_workspace_id: None,
                },
            )
            .await?;
        let split = client
            .pane_split(PaneSplitParams {
                direction: SplitDirection::Right,
                workspace_id: Some(created.workspace.workspace_id.clone()),
                target_pane_id: Some(created.root_pane.pane_id.clone()),
                ratio: None,
                cwd: Some(recipe.cwd.clone()),
                env,
                focus: false,
            })
            .await
            .map_err(map_herdr)?;
        let pane_id = split.pane.pane_id.clone();
        self.resources
            .record(&client, &session_name, &created, &pane_id)
            .await?;
        let agent_name = agent_name_for();

        let started = crate::pty_interaction::start_agent(
            &client,
            AgentStartParams {
                name: agent_name.clone(),
                kind: "claude".into(),
                pane_id: pane_id.clone(),
                args: recipe.argv.clone(),
                timeout_ms: Some(self.options.agent_start_timeout_ms),
            },
        )
        .await?;
        refuse_bare(&started.argv)?;

        let native_session = started
            .agent
            .agent_session
            .as_ref()
            .map(|info| info.value.clone())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| session_id.clone());

        let stream = client
            .subscribe(vec![Subscription::pane_agent_status_changed(&pane_id)])
            .await
            .map_err(map_herdr)?;

        let instance_id = self.options.instance_id.clone().unwrap_or_default();
        let run_id = RunId::new();
        let journal_id = Id::new("obj")?;
        let (tx, rx) = mpsc::channel(64);
        self.seq.store(0, Ordering::SeqCst);
        self.closed.store(false, Ordering::SeqCst);

        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), native_session.clone());
        ack.native_ids.insert("paneId".into(), pane_id.clone());
        ack.native_ids
            .insert("herdrSession".into(), session_name.clone());
        ack.native_ids
            .insert("agentName".into(), agent_name.clone());

        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: instance_id.clone(),
            host_id: spec.host.clone(),
            journal_id: journal_id.clone(),
            run_id: run_id.clone(),
            session_id: native_session.clone(),
            pin_version: recipe.binary.version.clone(),
        };
        emit_obs(
            &tx,
            &self.seq,
            &ctx,
            SourceChannel::Herdr,
            Completeness::Structured,
            ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                NativeLifecycle {
                    topic: LifecycleTopic::Session,
                    native_name: "session".into(),
                    native_id: Knowledge::Known {
                        value: native_session.clone(),
                    },
                    status: Knowledge::Known {
                        value: format!("{:?}", started.agent.agent_status).to_ascii_lowercase(),
                    },
                    related_ids: BTreeMap::from([
                        ("paneId".into(), pane_id.clone()),
                        ("herdrSession".into(), session_name.clone()),
                    ]),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                },
            )))),
        )
        .await?;

        if let Some(seeded) = seeded {
            emit_obs(
                &tx,
                &self.seq,
                &ctx,
                SourceChannel::Herdr,
                Completeness::Structured,
                ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
                    NativeLifecycle {
                        topic: LifecycleTopic::Diagnostic,
                        native_name: "claude-onboarding".into(),
                        native_id: Knowledge::Known {
                            value: pane_id.clone(),
                        },
                        status: Knowledge::Known {
                            value: seeded.summary(),
                        },
                        related_ids: BTreeMap::new(),
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: false,
                    },
                )))),
            )
            .await?;
        }

        let interactions = PtyInteractions::new(
            client.clone(),
            pane_id.clone(),
            ctx.clone(),
            tx.clone(),
            Arc::clone(&self.seq),
            self.options.auto_trust_registered_workspace,
        );
        let interaction_task = interactions.spawn();
        let status_task = spawn_status_pump(stream, pane_id.clone(), Arc::clone(&interactions));
        let hook_path = self.options.launch_dir.join("session-meta.json");
        let transcript_path = Arc::new(std::sync::Mutex::new(None));
        let hook_task = spawn_hook_watch(
            hook_path,
            tx.clone(),
            ctx.clone(),
            Arc::clone(&self.seq),
            Arc::clone(&transcript_path),
        );
        // §9.1: effort switches type `/effort` into this pane and read back
        // through the same transcript pump below.
        let effort_bridge = Arc::new(crate::effort::EffortBridge::new());
        if let Some(effort) = spec.effort {
            effort_bridge.note_launch_request(crate::effort::EffortRequest {
                name: effort.name,
                ultracode: effort.ultracode,
            });
        }
        let effort_queue = Arc::new(crate::effort::EffortQueue::new());
        // §9.1: /model switches share the pane I/O and transcript pump but
        // keep their own bridge/queue, resolved from the `/model` verdict.
        let model_bridge = Arc::new(crate::model::ModelBridge::new());
        if let Some(model) = spec.model_id.clone() {
            model_bridge.note_launch_request(model);
        }
        let model_queue = Arc::new(crate::model::ModelQueue::new());
        // Permission-mode coordination. `bypass_allowed` is whether this
        // launch argv carries the bypass allowance; without it the wheel can
        // never reach bypassPermissions at runtime.
        let launch_permission = launch_claude_permission(&spec);
        let bypass_allowed = recipe
            .argv
            .iter()
            .any(|token| token == "--dangerously-skip-permissions");
        let permission_bridge = Arc::new(crate::permission::PermissionBridge::new(bypass_allowed));
        let permission_queue = Arc::new(crate::permission::PermissionQueue::new());
        // The TUI's tool calls exist only in the native transcript; follow it as
        // soon as the hook names the file (D-025's mapper, claude-pty's carrier).
        let transcript_task = spawn_transcript_pump(
            Arc::clone(&transcript_path),
            tx.clone(),
            ctx.clone(),
            Arc::clone(&self.seq),
            Some(Arc::clone(&effort_bridge)),
            spec.effort,
            Some(TranscriptModelSync {
                bridge: Arc::clone(&model_bridge),
                launch: spec.model_id.clone(),
                catalog: Some(model_catalog.clone()),
            }),
            Some(Arc::clone(&permission_bridge)),
            launch_permission,
        );
        let effort_io: Arc<dyn crate::effort::EffortSwitchIo> = Arc::new(HerdrEffortIo {
            client: client.clone(),
            pane_id: pane_id.clone(),
            events: tx.clone(),
            seq: Arc::clone(&self.seq),
            ctx: ctx.clone(),
        });
        let effort_worker = crate::effort::spawn_worker(
            Arc::clone(&effort_bridge),
            Arc::clone(&effort_queue),
            effort_io,
        );
        // §9.1: /model switches use the same pane I/O, queue and transcript
        // pump, with their own bridge the mapper resolves from the command
        // verdict.
        let model_io: Arc<dyn crate::model::SwitchIo> = Arc::new(HerdrEffortIo {
            client: client.clone(),
            pane_id: pane_id.clone(),
            events: tx.clone(),
            seq: Arc::clone(&self.seq),
            ctx: ctx.clone(),
        });
        let model_worker = crate::model::spawn_model_worker(
            Arc::clone(&model_bridge),
            Arc::clone(&model_queue),
            model_io,
        );
        let permission_io: Arc<dyn crate::permission::PermissionSwitchIo> =
            Arc::new(HerdrPermissionIo {
                client: client.clone(),
                pane_id: pane_id.clone(),
                events: tx.clone(),
                seq: Arc::clone(&self.seq),
                ctx: ctx.clone(),
            });
        let permission_worker = crate::permission::spawn_worker(
            Arc::clone(&permission_bridge),
            Arc::clone(&permission_queue),
            permission_io,
        );

        // Seed the selection-path catalog, then re-resolve once the session's
        // own scoped discovery cache lands (the promotion-time answer may be
        // the operator's host fallback) and re-stamp the picker's catalog.
        model_bridge.set_own_catalog(crate::model_discovery::own_ids(&model_catalog));
        {
            let bridge = Arc::clone(&model_bridge);
            let events = tx.clone();
            let seq = Arc::clone(&self.seq);
            let refresh_ctx = ctx.clone();
            let initial = model_catalog.clone();
            let native_home = std::path::PathBuf::from(recipe.native_home.clone());
            let host_dir = host_config_dir.clone();
            let settings_path = std::path::Path::new(&recipe.native_home).join("settings.json");
            let current = spec.model_id.clone();
            tokio::spawn(async move {
                let payload = crate::model_discovery::scoped_refresh_payload(
                    native_home,
                    host_dir,
                    Some(settings_path),
                    refresh_env,
                    current,
                    initial,
                    bridge,
                )
                .await;
                if let Some(body) = payload
                    && let Err(error) = emit_obs(
                        &events,
                        &seq,
                        &refresh_ctx,
                        SourceChannel::Transcript,
                        Completeness::Structured,
                        body,
                    )
                    .await
                {
                    tracing::debug!(%error, "model catalog refresh: event channel closed");
                }
            });
        }

        *self.inner.lock().await = Some(PtyLive {
            ctx,
            client,
            pane_id,
            recipe: recipe.clone(),
            instance_id,
            host_id: spec.host.clone(),
            journal_id,
            run_id,
            session_id: native_session,
            native_store_id: spec.native_home.store_id.clone(),
            herdr_pin,
            herdr_session: session_name,
            transcript_path,
            events: tx,
            effort_bridge,
            effort_queue,
            effort_worker: Some(effort_worker),
            model_bridge,
            model_queue,
            model_worker: Some(model_worker),
            permission_bridge,
            permission_queue,
            permission_worker: Some(permission_worker),
            status_task: Some(status_task),
            interactions,
            interaction_task,
            hook_task: Some(hook_task),
            transcript_task: Some(transcript_task),
            tty_task: None,
            alt_screen: Arc::new(AtomicBool::new(false)),
            closed: false,
        });

        info!(
            launch_id = %recipe.launch_id,
            "claude-pty agent.start dispatched"
        );
        Ok(RunHandle::new(recipe, ack, rx))
    }

    /// Native identity for the live pane, if started.
    pub async fn native_ref(&self) -> Option<NativeRef> {
        self.inner.lock().await.as_ref().map(native_ref_for)
    }
}

impl ClaudePtyDriver {
    /// §9.1: type `/effort <level>` and let the transcript pump prove it.
    async fn switch_effort(&self, level: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::effort::EffortRequest::from_level(level) else {
            // An honest refusal for a word the in-session command does not take
            // (`auto` is a mode, and an unknown future name must not be typed
            // and hoped about). The Node surfaces the rejection in the UI.
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude /effort does not accept {level:?} in-session; \
                 valid: low, medium, high, xhigh, max, ultracode"
            )));
        };
        let (pane_id, session_id, queue, ready, io) = {
            let inner = self.inner.lock().await;
            let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
            if live.closed {
                return Err(DriverError::ControlUnavailable);
            }
            require_session_start(live)?;
            let ready = crate::pty_interaction::prompt_ready(&live.client, &live.pane_id)
                .await
                .is_ok();
            let io = HerdrEffortIo {
                client: live.client.clone(),
                pane_id: live.pane_id.clone(),
                events: live.events.clone(),
                seq: Arc::clone(&self.seq),
                ctx: live.ctx.clone(),
            };
            (
                live.pane_id.clone(),
                live.session_id.clone(),
                Arc::clone(&live.effort_queue),
                ready,
                Arc::new(io) as Arc<dyn crate::effort::EffortSwitchIo>,
            )
        };

        if ready {
            // Fast path: the composer is idle, so the worker applies it now and
            // the read-back settles this command.
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait =
                std::time::Duration::from_millis(crate::effort::EFFORT_READBACK_TIMEOUT_MS + 5_000);
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // Bounded window elapsed without a terminal outcome. The worker
                // keeps running and will journal applied/degraded; this command
                // settles as dispatched, never as applied.
            }
        } else {
            // Ready ladder: a turn is running. Journal `queued` and hand off;
            // the worker types it at the next idle and journals the result.
            io.journal(
                crate::effort::SwitchOutcome::Queued.journal_status(request.command_word(), ""),
                Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }

        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), session_id.clone());
        ack.native_ids.insert("paneId".into(), pane_id);
        Ok(ack)
    }

    /// §9.1: type `/model <id>` and let the transcript verdict prove it.
    async fn switch_model(&self, model_id: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::model::ModelRequest::new(model_id) else {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude /model requires a non-empty id; got {model_id:?}"
            )));
        };
        let (pane_id, session_id, queue, ready, io) = {
            let inner = self.inner.lock().await;
            let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
            if live.closed {
                return Err(DriverError::ControlUnavailable);
            }
            require_session_start(live)?;
            let ready = crate::pty_interaction::prompt_ready(&live.client, &live.pane_id)
                .await
                .is_ok();
            let io = HerdrEffortIo {
                client: live.client.clone(),
                pane_id: live.pane_id.clone(),
                events: live.events.clone(),
                seq: Arc::clone(&self.seq),
                ctx: live.ctx.clone(),
            };
            (
                live.pane_id.clone(),
                live.session_id.clone(),
                Arc::clone(&live.model_queue),
                ready,
                Arc::new(io) as Arc<dyn crate::model::SwitchIo>,
            )
        };

        if ready {
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait =
                std::time::Duration::from_millis(crate::model::MODEL_READBACK_TIMEOUT_MS + 5_000);
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // Bounded window elapsed without a terminal outcome; never
                // claim applied from this command.
            }
        } else {
            io.journal(
                crate::model::ModelSwitchOutcome::Queued.journal_status(&request.id, ""),
                Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }

        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), session_id.clone());
        ack.native_ids.insert("paneId".into(), pane_id);
        Ok(ack)
    }

    /// Shift+tab the native permission wheel to `mode` and let the TUI status
    /// line / transcript prove it.
    async fn switch_permission(&self, mode: &str) -> DriverResult<DriverAck> {
        let Some(request) = crate::permission::PermissionRequest::parse(mode) else {
            return Err(DriverError::CapabilityUnsupported(format!(
                "claude permission mode {mode:?} is not a valid mode; \
                 valid: manual, acceptEdits, plan, auto, bypassPermissions, dontAsk"
            )));
        };
        let (pane_id, session_id, queue, ready, io) = {
            let inner = self.inner.lock().await;
            let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
            if live.closed {
                return Err(DriverError::ControlUnavailable);
            }
            require_session_start(live)?;
            // Launch-only modes are an honest refusal, never a keystroke.
            if !crate::permission::live_reachable(
                request.mode,
                live.permission_bridge.bypass_allowed(),
            ) {
                return Err(DriverError::CapabilityUnsupported(format!(
                    "claude permission mode {mode:?} is launch-only for this session"
                )));
            }
            let ready = crate::pty_interaction::prompt_ready(&live.client, &live.pane_id)
                .await
                .is_ok();
            let io = HerdrPermissionIo {
                client: live.client.clone(),
                pane_id: live.pane_id.clone(),
                events: live.events.clone(),
                seq: Arc::clone(&self.seq),
                ctx: live.ctx.clone(),
            };
            (
                live.pane_id.clone(),
                live.session_id.clone(),
                Arc::clone(&live.permission_queue),
                ready,
                Arc::new(io) as Arc<dyn crate::permission::PermissionSwitchIo>,
            )
        };

        if ready {
            let (done, rx_outcome) = tokio::sync::oneshot::channel();
            queue.enqueue(request, Some(done));
            let wait = std::time::Duration::from_millis(
                crate::permission::PERMISSION_READBACK_TIMEOUT_MS + 5_000,
            );
            if tokio::time::timeout(wait, rx_outcome).await.is_err() {
                // The worker keeps running and journals applied/degraded; this
                // command settles as dispatched, never as applied.
            }
        } else {
            io.journal(
                crate::permission::SwitchOutcome::Queued.journal_status(request.word(), ""),
                Severity::Info,
            )
            .await;
            queue.enqueue(request, None);
        }

        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), session_id.clone());
        ack.native_ids.insert("paneId".into(), pane_id);
        Ok(ack)
    }
}

#[async_trait]
impl Driver for ClaudePtyDriver {
    fn track_pty_resources(&self, id: InstanceId, store: Arc<dyn crate::PtyResourceStore>) {
        self.resources.configure(id, store);
    }

    async fn capabilities(&self) -> DriverResult<remuda_protocol::CapabilitySnapshot> {
        let pin = pin_source(&self.options.binary)?;
        Ok(capability_snapshot(
            DriverKind::ClaudePty,
            &pin,
            U64(1),
            U64(1),
        )?)
    }

    async fn start(&self, spec: InstanceSpec) -> DriverResult<RunHandle> {
        let session_id = uuid::Uuid::now_v7().to_string();
        match self.launch(spec, SessionAction::New { session_id }).await {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if let Err(cleanup) = self.resources.close().await {
                    tracing::error!(%cleanup, "failed launch resource cleanup");
                }
                Err(error)
            }
        }
    }

    async fn wait_control(&self) -> DriverResult<()> {
        let inner = self.inner.lock().await;
        let live = inner
            .as_ref()
            .filter(|live| !live.closed)
            .ok_or(DriverError::ControlUnavailable)?;
        require_session_start(live)?;
        // SessionStart proves the native session is real, so no later screen
        // can be a first-run wizard; stop reading the viewport for one.
        live.interactions.disarm_startup_watch().await;
        crate::pty_interaction::prompt_ready_for(&live.client, &live.pane_id, &live.interactions)
            .await
    }

    async fn attach(&self, native_ref: NativeRef) -> DriverResult<DriverAck> {
        let mut inner = self.inner.lock().await;
        let live = inner.as_mut().ok_or(DriverError::NativeSessionNotFound)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        match &native_ref.session_id {
            Knowledge::Known { value } if value == &live.session_id || value.is_empty() => {}
            Knowledge::Known { .. } => {
                return Err(DriverError::NativeSessionNotFound);
            }
            Knowledge::Unknown { .. } | Knowledge::NotApplicable => {
                if native_ref.herdr.as_ref().map(|h| h.pane_id.as_str())
                    != Some(live.pane_id.as_str())
                {
                    return Err(DriverError::NativeSessionNotFound);
                }
            }
        }
        let pane_id = native_ref
            .herdr
            .as_ref()
            .map(|herdr| herdr.pane_id.clone())
            .unwrap_or_else(|| live.pane_id.clone());
        let observer = TerminalObserver::open(&live.client, &pane_id, 80, 24)
            .await
            .map_err(map_herdr)?;
        if let Some(task) = live.tty_task.take() {
            task.abort();
        }
        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: live.instance_id.clone(),
            host_id: live.host_id.clone(),
            journal_id: live.journal_id.clone(),
            run_id: live.run_id.clone(),
            session_id: live.session_id.clone(),
            pin_version: live.recipe.binary.version.clone(),
        };
        live.tty_task = Some(spawn_tty_pump(
            observer,
            live.events.clone(),
            ctx,
            Arc::clone(&self.seq),
            Arc::clone(&live.alt_screen),
        ));
        let mut ack = DriverAck::transport_written();
        ack.native_ids.insert("paneId".into(), pane_id);
        ack.native_ids
            .insert("sessionId".into(), live.session_id.clone());
        Ok(ack)
    }

    async fn send(&self, input: DriverInput) -> DriverResult<DriverAck> {
        // §9.1: an effort-bearing switch is typed into the composer as
        // `/effort <level>` and read back through the transcript — it is not a
        // model switch and never was unsupported on the PTY carrier.
        if let DriverInput::ModelSwitch(switch) = &input
            && let Some(level) = switch.effort.as_deref()
            && !level.is_empty()
            && switch.model_id.is_empty()
        {
            return self.switch_effort(level).await;
        }
        // §9.1: a model switch is `/model <id>` typed into the composer and
        // proven by the transcript verdict. (A combined model+effort configure
        // applies the model here; the structured composer sends them as
        // separate commands.)
        if let DriverInput::ModelSwitch(switch) = &input
            && !switch.model_id.is_empty()
        {
            return self.switch_model(&switch.model_id).await;
        }
        // A permission-bearing switch is a shift+tab wheel walk read back from
        // the status line; a bare model id falls through to the prompt path,
        // which rejects it honestly for this carrier.
        if let DriverInput::ModelSwitch(switch) = &input
            && let Some(mode) = switch.permission_mode.as_deref()
            && !mode.is_empty()
        {
            return self.switch_permission(mode).await;
        }
        let text = prompt_text(&input)?;
        let inner = self.inner.lock().await;
        let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
        if live.closed {
            return Err(DriverError::ControlUnavailable);
        }
        require_session_start(live)?;
        crate::pty_interaction::prompt_ready(&live.client, &live.pane_id).await?;
        live.client
            .agent_prompt(AgentPromptParams {
                target: live.pane_id.clone(),
                text,
                wait: None,
            })
            .await
            .map_err(map_herdr)?;
        let mut ack = DriverAck::transport_written();
        ack.native_ids
            .insert("sessionId".into(), live.session_id.clone());
        ack.native_ids.insert("paneId".into(), live.pane_id.clone());
        Ok(ack)
    }

    async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let interactions = self
            .inner
            .lock()
            .await
            .as_ref()
            .filter(|live| !live.closed)
            .map(|live| Arc::clone(&live.interactions))
            .ok_or(DriverError::ControlUnavailable)?;
        interactions.send_keys(keys).await
    }

    async fn tty_bridge(&self) -> Option<crate::tty::TtyBridge> {
        let inner = self.inner.lock().await;
        let live = inner.as_ref()?;
        if live.closed {
            return None;
        }
        Some(crate::tty::TtyBridge::Herdr {
            client: live.client.clone(),
            pane_id: live.pane_id.clone(),
        })
    }

    async fn alt_screen(&self) -> Option<bool> {
        // The attach relay scans the exact byte stream the follower renders
        // (DECSET/DECRST 1049/1047/47, RIS); before any relay has attached the
        // pane's mode has no trustworthy observation.
        let inner = self.inner.lock().await;
        let live = inner.as_ref()?;
        (live.tty_task.is_some() && !live.closed).then(|| live.alt_screen.load(Ordering::SeqCst))
    }

    async fn cancel(&self) -> DriverResult<DriverAck> {
        self.send_keys(vec!["esc".into()]).await
    }

    async fn respond_interaction(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        let interactions = self
            .inner
            .lock()
            .await
            .as_ref()
            .filter(|live| !live.closed)
            .map(|live| Arc::clone(&live.interactions))
            .ok_or(DriverError::ControlUnavailable)?;
        interactions.respond(id, answer).await
    }

    async fn close(&self) -> DriverResult<DriverAck> {
        self.closed.store(true, Ordering::SeqCst);
        let mut inner = self.inner.lock().await;
        let Some(live) = inner.as_mut() else {
            drop(inner);
            self.resources.close().await?;
            return Ok(DriverAck::not_dispatched());
        };
        if live.closed {
            drop(inner);
            self.resources.close().await?;
            return Ok(DriverAck::not_dispatched());
        }
        live.closed = true;
        live.interaction_task.abort();
        live.effort_queue.close();
        if let Some(task) = live.effort_worker.take() {
            task.abort();
        }
        live.model_queue.close();
        if let Some(task) = live.model_worker.take() {
            task.abort();
        }
        live.permission_queue.close();
        if let Some(task) = live.permission_worker.take() {
            task.abort();
        }
        if let Some(task) = live.status_task.take() {
            task.abort();
        }
        if let Some(task) = live.hook_task.take() {
            task.abort();
        }
        if let Some(task) = live.transcript_task.take() {
            task.abort();
        }
        if let Some(task) = live.tty_task.take() {
            task.abort();
        }
        live.interactions.close().await?;
        let events = live.events.clone();
        let ctx = live.ctx.clone();
        let recipe = live.recipe.clone();
        drop(inner);
        self.resources.close().await?;
        // S5: the pane is closed, so the launch overlays can go.
        crate::recipe::report_launch_cleanup(&recipe, "claude-pty");
        crate::claude_pty::emit_pty_closed(&events, &self.seq, &ctx).await?;
        self.inner.lock().await.take();
        Ok(DriverAck::not_dispatched())
    }

    async fn resume(&self, native_ref: NativeRef) -> DriverResult<RunHandle> {
        let session_id = match &native_ref.session_id {
            Knowledge::Known { value } if !value.is_empty() => value.clone(),
            _ => native_ref
                .claude
                .as_ref()
                .map(|claude| claude.session_id.clone())
                .filter(|value| !value.is_empty())
                .ok_or(DriverError::NativeSessionNotFound)?,
        };
        let mut spec = self
            .last_spec
            .lock()
            .await
            .clone()
            .ok_or(DriverError::NativeSessionNotFound)?;
        spec.driver = DriverKind::ClaudePty;
        spec.host = native_ref.host_id.clone();
        spec.kind = AgentKind::Claude;
        if let Some(herdr) = &native_ref.herdr {
            spec.carrier = remuda_protocol::CarrierSpec::Pty(Box::new(PtyCarrier {
                backend: PtyBackend::Herdr,
                server: herdr.server.clone(),
                session: herdr.session.clone(),
            }));
        }
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }

    async fn start_resumed(
        &self,
        mut spec: InstanceSpec,
        session_id: String,
    ) -> DriverResult<RunHandle> {
        if session_id.trim().is_empty() {
            return Err(DriverError::NativeSessionNotFound);
        }
        spec.driver = DriverKind::ClaudePty;
        self.launch(spec, SessionAction::Resume { session_id })
            .await
    }
}

fn require_session_start(live: &PtyLive) -> DriverResult<()> {
    // Herdr can report a transient idle immediately after folder trust, before
    // Claude has mounted its prompt composer. Its own SessionStart hook fences startup.
    if live
        .transcript_path
        .lock()
        .ok()
        .is_some_and(|slot| slot.is_some())
    {
        Ok(())
    } else {
        Err(DriverError::ControlUnavailable)
    }
}

#[derive(Clone)]
pub(crate) struct ObsCtx {
    pub(crate) driver: DriverKind,
    pub(crate) instance_id: InstanceId,
    pub(crate) host_id: HostId,
    pub(crate) journal_id: Id,
    pub(crate) run_id: RunId,
    pub(crate) session_id: String,
    pub(crate) pin_version: String,
}

fn spawn_status_pump(
    mut stream: EventStream,
    pane_id: String,
    interactions: Arc<PtyInteractions>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(event)) = stream.next_event().await {
            if event.kind == EventKind::PaneAgentStatusChanged
                && event.pane_id() == Some(pane_id.as_str())
            {
                let _ = interactions.observe().await;
            }
        }
    })
}

fn read_session_meta_transcript(launch_dir: &Path) -> Option<PathBuf> {
    let body = std::fs::read_to_string(launch_dir.join("session-meta.json")).ok()?;
    let value: Value = serde_json::from_str(&body).ok()?;
    value
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn spawn_hook_watch(
    path: PathBuf,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
    transcript_slot: Arc<std::sync::Mutex<Option<String>>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // A user-owned startup dialog may remain pending indefinitely. Close
        // aborts this watcher; elapsed time must not make queued input unrecoverable.
        while !tx.is_closed() {
            if let Ok(body) = tokio::fs::read_to_string(&path).await
                && let Ok(value) = serde_json::from_str::<Value>(&body)
            {
                let session = value
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or(&ctx.session_id);
                let transcript = value
                    .get("transcript_path")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if transcript.is_empty() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
                let mut related = BTreeMap::new();
                if !transcript.is_empty() {
                    related.insert("transcriptPath".into(), transcript.to_string());
                }
                let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(
                    Box::new(NativeLifecycle {
                        topic: LifecycleTopic::Hook,
                        native_name: "SessionStart".into(),
                        native_id: Knowledge::Known {
                            value: session.to_string(),
                        },
                        status: Knowledge::Known {
                            value: "recorded".into(),
                        },
                        related_ids: related,
                        data_ref: None,
                        severity: Severity::Info,
                        affects_completion: false,
                    }),
                )));
                if emit_obs(
                    &tx,
                    &seq,
                    &ctx,
                    SourceChannel::Hook,
                    Completeness::Structured,
                    payload,
                )
                .await
                .is_ok()
                    && let Ok(mut slot) = transcript_slot.lock()
                {
                    *slot = Some(transcript.to_string());
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
}

/// Poll interval for the native transcript, matching the hook watcher's cadence.
/// Transcript tail poll. §9.1 measured the `/effort` stdout verdict landing
/// ~125–270 ms after the confirm CR; a 300 ms poll could double that, so the
/// tail is checked at 75 ms (one stat + appended-bytes read, no busy spin).
const TRANSCRIPT_POLL: Duration = Duration::from_millis(75);

/// Follow the native transcript and map it into structured Observations.
///
/// The TUI does not speak stream-json, so its `tool_use` / `tool_result` blocks
/// only ever exist in `~/.claude/projects/<encoded cwd>/<session>.jsonl`. The
/// SessionStart hook names that file; without this pump the 结构 view sees the
/// prompt the Node queued and nothing the agent actually did.
/// §9.1 model-sync inputs for the transcript hydrator, mirroring the effort
/// bridge pair.
#[derive(Clone)]
struct TranscriptModelSync {
    bridge: Arc<crate::model::ModelBridge>,
    launch: Option<String>,
    catalog: Option<ModelCatalogInfo>,
}

#[allow(clippy::too_many_arguments)]
fn spawn_transcript_pump(
    transcript_slot: Arc<std::sync::Mutex<Option<String>>>,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
    effort_bridge: Option<Arc<crate::effort::EffortBridge>>,
    launch_effort: Option<EffortSelection>,
    model: Option<TranscriptModelSync>,
    permission_bridge: Option<Arc<crate::permission::PermissionBridge>>,
    launch_permission: Option<remuda_protocol::ClaudePermissionMode>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut hydrator: Option<(crate::claude_transcript::TranscriptTail, TranscriptMapper)> =
            None;
        // The launch model snapshot (carrying the discovered catalog) is
        // emitted once when the pump first binds the transcript.
        let mut launch_snapshot = model.as_ref().and_then(|m| m.launch.clone());
        while !tx.is_closed() {
            if hydrator.is_none() {
                let path = transcript_slot
                    .lock()
                    .ok()
                    .and_then(|slot| slot.clone())
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .filter(|path| path.is_file());
                if let Some(path) = path {
                    info!(session = %ctx.session_id, "hydrating claude-pty from native transcript");
                    let mut mapper = TranscriptMapper::new(
                        ctx.driver,
                        ctx.instance_id.clone(),
                        ctx.run_id.clone(),
                        ctx.journal_id.clone(),
                        ctx.host_id.clone(),
                        ctx.session_id.clone(),
                        ctx.pin_version.clone(),
                    );
                    if let Some(bridge) = &effort_bridge {
                        mapper = mapper.with_effort_bridge(Arc::clone(bridge), launch_effort);
                    }
                    if let Some(model) = &model {
                        mapper = mapper.with_model_bridge(
                            Arc::clone(&model.bridge),
                            launch_snapshot.clone(),
                            model.catalog.clone(),
                        );
                    }
                    // Emit the launch-time model baseline with the discovered
                    // catalog before any transcript line is mapped, so the
                    // picker has its real list immediately.
                    if model.is_some()
                        && let Some(id) = launch_snapshot.take()
                        && !id.is_empty()
                    {
                        match mapper.take_launch_model_snapshot(Some(&id)) {
                            Ok(observations) => {
                                for observation in observations {
                                    if emit_obs(
                                        &tx,
                                        &seq,
                                        &ctx,
                                        SourceChannel::Transcript,
                                        observation.completeness,
                                        observation.body,
                                    )
                                    .await
                                    .is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Err(error) => {
                                tracing::debug!(%error, "launch model snapshot not emitted");
                            }
                        }
                    }
                    if let Some(bridge) = &permission_bridge {
                        mapper =
                            mapper.with_permission_bridge(Arc::clone(bridge), launch_permission);
                    }
                    hydrator = Some((crate::claude_transcript::TranscriptTail::new(path), mapper));
                }
            }
            if let Some((tail, mapper)) = hydrator.as_mut() {
                // A read error is transient (the file is being appended to);
                // the next tick retries from the same offset.
                let lines = tail.poll().unwrap_or_default();
                // Flush the buffered assistant run at the end of the batch:
                // the mapper holds a run open until it is superseded, so the
                // final message of a finished turn would otherwise wait for
                // the next record to arrive.
                let mut batches: Vec<_> = lines.iter().map(|line| mapper.map_line(line)).collect();
                batches.push(mapper.flush());
                for batch in batches {
                    let mapped = match batch {
                        Ok(mapped) => mapped,
                        Err(error) => {
                            tracing::debug!(%error, "transcript line did not map");
                            continue;
                        }
                    };
                    for observation in mapped {
                        if emit_obs(
                            &tx,
                            &seq,
                            &ctx,
                            SourceChannel::Transcript,
                            observation.completeness,
                            observation.body,
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            tokio::time::sleep(TRANSCRIPT_POLL).await;
        }
    })
}

fn spawn_tty_pump(
    mut observer: TerminalObserver,
    tx: mpsc::Sender<Observation>,
    ctx: ObsCtx,
    seq: Arc<AtomicU64>,
    alt_screen_slot: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(frame) = observer.next_frame().await {
            let Ok(frame) = frame else {
                break;
            };
            // The relay scans the exact bytes followers render
            // (?1049/?1047/?47, RIS) — no herdr mode API, no full emulator.
            alt_screen_slot.store(frame.alt_screen, Ordering::SeqCst);
            let stream_id = match Id::new("tty") {
                Ok(id) => id,
                Err(_) => break,
            };
            let stream_epoch = match Id::new("epoch") {
                Ok(id) => id,
                Err(_) => break,
            };
            let object_id = match Id::new("obj") {
                Ok(id) => id,
                Err(_) => break,
            };
            let digest = match hash_bytes(&frame.bytes) {
                Ok(digest) => digest,
                Err(_) => match dummy_digest() {
                    Ok(digest) => digest,
                    Err(_) => break,
                },
            };
            let payload = ObservationPayload::RawTty(Box::new(
                remuda_protocol::RawTtyPayload::Output(Box::new(TtyOutput {
                    stream_id,
                    stream_epoch,
                    representation: TtyRepresentation::RenderedAnsi,
                    offset: U64(frame.seq),
                    byte_length: U64(frame.bytes.len() as u64),
                    data_ref: RawRef {
                        object_id,
                        offset: U64(0),
                        length: U64(frame.bytes.len() as u64),
                        digest,
                        media_type: "application/vnd.remuda.tty-ansi".into(),
                        redaction: Redaction::None,
                    },
                    native_frame: Knowledge::Known {
                        value: NativeTerminalFrame {
                            seq: U64(frame.seq),
                            width: frame.width,
                            height: frame.height,
                            full: frame.full,
                        },
                    },
                })),
            ));
            if emit_obs(
                &tx,
                &seq,
                &ctx,
                SourceChannel::Pty,
                Completeness::Partial,
                payload,
            )
            .await
            .is_err()
            {
                break;
            }
        }
    })
}

fn status_payloads(
    ctx: &ObsCtx,
    status: AgentStatus,
) -> Vec<(SourceChannel, Completeness, ObservationPayload)> {
    let label = match status {
        AgentStatus::Working => "working",
        AgentStatus::Idle => "idle",
        AgentStatus::Blocked => "blocked",
        AgentStatus::Done => "idle",
        AgentStatus::Unknown => "unknown",
    };
    let status_completeness = if status == AgentStatus::Blocked {
        Completeness::ScreenDerived
    } else {
        Completeness::Partial
    };
    vec![(
        SourceChannel::Herdr,
        status_completeness,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Turn,
                native_name: "agent_status".into(),
                native_id: Knowledge::Known {
                    value: ctx.session_id.clone(),
                },
                status: Knowledge::Known {
                    value: label.into(),
                },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        )))),
    )]
}

async fn emit_obs(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<()> {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    tx.send(build_observation(ctx, n, channel, completeness, payload)?)
        .await
        .map_err(|_| DriverError::ControlUnavailable)?;
    Ok(())
}

pub(crate) fn build_observation(
    ctx: &ObsCtx,
    seq: u64,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<Observation> {
    Ok(Observation {
        schema_version: SchemaVersion,
        event_id: EventId::new(),
        journal_id: ctx.journal_id.clone(),
        instance_id: ctx.instance_id.clone(),
        run_id: Some(ctx.run_id.clone()),
        host_id: ctx.host_id.clone(),
        process_generation: U64(1),
        run_generation: Some(U64(1)),
        seq: U64(seq),
        observed_at: now_ts()?,
        native_at: Knowledge::Unknown {
            reason: "herdr-clock".into(),
            evidence_event_ids: vec![],
        },
        source: ObservationSource {
            driver_kind: ctx.driver,
            driver_version: ctx.pin_version.clone(),
            adapter_version: ADAPTER_VERSION.into(),
            channel,
            delivery: SourceDelivery::Live,
            native_session_id: Knowledge::Known {
                value: ctx.session_id.clone(),
            },
            native_turn_id: Knowledge::Unknown {
                reason: "pty-no-turn-id".into(),
                evidence_event_ids: vec![],
            },
            native_agent_id: Knowledge::NotApplicable,
            native_item_id: Knowledge::Unknown {
                reason: "pty-no-item".into(),
                evidence_event_ids: vec![],
            },
            native_event_id: Knowledge::Unknown {
                reason: "pty-no-native-event".into(),
                evidence_event_ids: vec![],
            },
            native_request_id: NativeRequestKey::None,
            source_cursor: SourceCursor::Runtime(Box::new(remuda_protocol::RuntimeCursor {
                ledger_revision: U64(seq),
            })),
        },
        completeness,
        raw_ref: None,
        evidence_event_ids: vec![],
        body: payload,
    })
}

pub(crate) fn now_ts() -> DriverResult<Timestamp> {
    let now = time::OffsetDateTime::now_utc();
    let date = now.date();
    let (hour, minute, second) = now.time().as_hms();
    let formatted = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        date.year(),
        u8::from(date.month()),
        date.day(),
        hour,
        minute,
        second,
        now.millisecond()
    );
    Timestamp::try_from(formatted).map_err(DriverError::Protocol)
}

pub(crate) fn dummy_digest() -> DriverResult<Digest> {
    Digest::try_from(
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
    )
    .map_err(DriverError::Protocol)
}

/// §9.1 [`crate::effort::EffortSwitchIo`] backed by a Herdr pane.
struct HerdrEffortIo {
    client: Client,
    pane_id: String,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    ctx: ObsCtx,
}

#[async_trait]
impl crate::effort::EffortSwitchIo for HerdrEffortIo {
    async fn is_idle(&self) -> bool {
        let Ok(info) = self.client.agent_get(&self.pane_id).await else {
            return false;
        };
        matches!(
            info.agent.agent_status,
            AgentStatus::Idle | AgentStatus::Done
        ) && info.agent.interactive_ready
    }

    async fn type_body(&self, body: &str) -> DriverResult<()> {
        // Body as its own write; Enter comes separately (§5.2 measured split).
        self.client
            .pane_send_text(self.pane_id.clone(), body)
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn press_enter(&self) -> DriverResult<()> {
        self.client
            .agent_send_keys(self.pane_id.clone(), vec!["enter".into()])
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn screen_text(&self) -> DriverResult<String> {
        let read = self
            .client
            .agent_read(AgentReadParams {
                target: self.pane_id.clone(),
                source: ReadSource::Visible,
                lines: Some(40),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(map_herdr)?;
        Ok(read.text().to_owned())
    }

    async fn journal(&self, status: String, severity: Severity) {
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Configuration,
                native_name: "instance.configure".into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known { value: status },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity,
                affects_completion: false,
            },
        ))));
        if emit_obs(
            &self.events,
            &self.seq,
            &self.ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            payload,
        )
        .await
        .is_err()
        {
            tracing::debug!("effort lifecycle dropped: event channel closed");
        }
    }
}

/// [`crate::permission::PermissionSwitchIo`] backed by a Herdr pane.
///
/// Keys go as raw bytes through `pane.send_text` — the measured encoding on a
/// real TTY: shift+tab `ESC [ Z`, down `ESC [ B`, esc `ESC`, enter `CR`.
struct HerdrPermissionIo {
    client: Client,
    pane_id: String,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    ctx: ObsCtx,
}

#[async_trait]
impl crate::permission::PermissionSwitchIo for HerdrPermissionIo {
    async fn is_idle(&self) -> bool {
        let Ok(info) = self.client.agent_get(&self.pane_id).await else {
            return false;
        };
        matches!(
            info.agent.agent_status,
            AgentStatus::Idle | AgentStatus::Done
        ) && info.agent.interactive_ready
    }

    async fn send_cycle(&self) -> DriverResult<()> {
        self.client
            .pane_send_text(self.pane_id.clone(), "\u{1b}[Z")
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn press_down(&self) -> DriverResult<()> {
        self.client
            .pane_send_text(self.pane_id.clone(), "\u{1b}[B")
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn press_enter(&self) -> DriverResult<()> {
        self.client
            .pane_send_text(self.pane_id.clone(), "\r")
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn press_esc(&self) -> DriverResult<()> {
        self.client
            .pane_send_text(self.pane_id.clone(), "\u{1b}")
            .await
            .map_err(map_herdr)?;
        Ok(())
    }

    async fn screen_text(&self) -> DriverResult<String> {
        let read = self
            .client
            .agent_read(AgentReadParams {
                target: self.pane_id.clone(),
                source: ReadSource::Visible,
                lines: Some(40),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(map_herdr)?;
        Ok(read.text().to_owned())
    }

    async fn journal(&self, status: String, severity: Severity) {
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Configuration,
                native_name: "instance.configure".into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known { value: status },
                related_ids: BTreeMap::new(),
                data_ref: None,
                severity,
                affects_completion: false,
            },
        ))));
        if emit_obs(
            &self.events,
            &self.seq,
            &self.ctx,
            SourceChannel::Runtime,
            Completeness::Structured,
            payload,
        )
        .await
        .is_err()
        {
            tracing::debug!("permission lifecycle dropped: event channel closed");
        }
    }
}

pub(crate) fn prompt_text(input: &DriverInput) -> DriverResult<String> {
    match input {
        DriverInput::Prompt(prompt) => blocks_to_text(&prompt.blocks),
        DriverInput::Steer(_) => Err(DriverError::CapabilityUnsupported(
            "claude-pty does not accept structured steer".into(),
        )),
        DriverInput::ModelSwitch(switch) => Err(DriverError::CapabilityUnsupported(format!(
            "claude-pty has no runtime model command; requested model={} (effort switches \
             are typed as /effort before this path)",
            switch.model_id,
        ))),
    }
}

/// Text plus an absolute-path mention per attachment (D-027).
///
/// A PTY driver can only type, so an image reaches Claude through its own Read
/// tool — the design's one verified remote path for the TUI.
fn blocks_to_text(blocks: &[ContentBlock]) -> DriverResult<String> {
    crate::attachment::text_with_path_mentions(blocks)
}

pub(crate) fn refuse_bare(argv: &[String]) -> DriverResult<()> {
    if argv.iter().any(|token| {
        crate::flags::is_banned_flag_token(token)
            || matches!(
                token.as_str(),
                "--bare" | "--safe-mode" | "--no-session-persistence" | "--continue"
            )
            || token.starts_with("--bare=")
            || token.starts_with("--safe-mode=")
            || token.starts_with("--no-session-persistence=")
            || token.starts_with("--continue=")
    }) {
        return Err(DriverError::NativeFeatureDisabled(
            "refusing prohibited flag on claude-pty/bg argv".into(),
        ));
    }
    Ok(())
}

pub(crate) fn apply_tty_bypass_flag(recipe: &mut LaunchRecipe) {
    const TTY: &str = "--dangerously-skip-permissions";
    const PRINT: &str = "--allow-dangerously-skip-permissions";
    recipe.argv.retain(|token| token != PRINT);
    recipe.permission.extra_flags.retain(|token| token != PRINT);
    if recipe
        .permission
        .extra_flags
        .iter()
        .any(|token| token == TTY)
        && !recipe.argv.iter().any(|token| token == TTY)
    {
        recipe.argv.push(TTY.into());
    }
}

pub(crate) fn argv_for_resume(argv: &[String], session_id: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    let mut replaced = false;
    while index < argv.len() {
        let token = &argv[index];
        if token == "--session-id" || token == "--resume" || token == "--continue" {
            index += 1;
            if index < argv.len() && !argv[index].starts_with('-') {
                index += 1;
            }
            if !replaced {
                out.push("--resume".into());
                out.push(session_id.to_string());
                replaced = true;
            }
            continue;
        }
        if token.starts_with("--session-id=")
            || token.starts_with("--resume=")
            || token.starts_with("--continue=")
        {
            if !replaced {
                out.push("--resume".into());
                out.push(session_id.to_string());
                replaced = true;
            }
            index += 1;
            continue;
        }
        out.push(token.clone());
        index += 1;
    }
    if !replaced {
        out.push("--resume".into());
        out.push(session_id.to_string());
    }
    out
}

pub(crate) fn strip_named_flags(argv: &[String], names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut index = 0;
    while index < argv.len() {
        let token = &argv[index];
        let matched = names.iter().any(|name| {
            token == name
                || token
                    .strip_prefix(*name)
                    .is_some_and(|rest| rest.starts_with('='))
        });
        if matched {
            if !token.contains('=') {
                index += 1;
                if index < argv.len() && !argv[index].starts_with('-') {
                    index += 1;
                }
            } else {
                index += 1;
            }
            continue;
        }
        out.push(token.clone());
        index += 1;
    }
    out
}

pub(crate) fn inject_session_start_hook(
    recipe: &mut LaunchRecipe,
    launch_dir: &Path,
) -> DriverResult<LaunchRecipe> {
    std::fs::create_dir_all(launch_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(launch_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let hook_path = launch_dir.join("session-start.sh");
    let meta_path = launch_dir.join("session-meta.json");
    write_private(&hook_path, HOOK_SCRIPT.as_bytes(), 0o700)?;
    let command = format!(
        "{} {}",
        shell_single_quote(&hook_path.to_string_lossy()),
        shell_single_quote(&meta_path.to_string_lossy())
    );
    let settings_path = launch_dir.join("settings.json");
    let overlay_src = recipe
        .argv
        .windows(2)
        .find_map(|pair| (pair[0] == "--settings").then(|| PathBuf::from(&pair[1])));
    let mut settings = if let Some(src) = overlay_src.filter(|path| path.exists()) {
        serde_json::from_slice(&std::fs::read(src)?)?
    } else if settings_path.exists() {
        serde_json::from_slice(&std::fs::read(&settings_path)?)?
    } else {
        json!({})
    };
    merge_session_start_hook(&mut settings, &command)?;
    if let Some(object) = settings.as_object_mut() {
        object
            .entry("bypassPermissionsModeAccepted")
            .or_insert(json!(true));
    }
    let bytes = serde_json::to_vec_pretty(&settings)?;
    write_private(&settings_path, &bytes, 0o600)?;
    let digest = hash_bytes(&bytes)?;
    if let Some(file) = recipe
        .materialized_files
        .iter_mut()
        .find(|file| file.role == FileRole::Settings)
    {
        file.path = settings_path.to_string_lossy().into_owned();
        file.content_digest = digest.clone();
    } else {
        recipe.materialized_files.push(MaterializedFile {
            path: settings_path.to_string_lossy().into_owned(),
            role: FileRole::Settings,
            mode: "0600".into(),
            content_digest: digest.clone(),
            lifetime: FileLifetime::Launch,
        });
    }
    recipe.audit.settings_digest = Some(digest);
    ensure_settings_flag(&mut recipe.argv, &settings_path);
    validate_setting_sources(&recipe.argv)?;
    recipe.audit.redacted_argv = recipe
        .argv
        .iter()
        .map(|token| {
            if token == &settings_path.to_string_lossy() {
                "<settings>".into()
            } else {
                token.clone()
            }
        })
        .collect();
    Ok(recipe.clone())
}

fn merge_session_start_hook(settings: &mut Value, command: &str) -> DriverResult<()> {
    let object = settings.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("settings overlay is not an object".into())
    })?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks_object = hooks.as_object_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable("hooks overlay is not an object".into())
    })?;
    let session_start = hooks_object
        .entry("SessionStart")
        .or_insert_with(|| json!([]));
    let matchers = session_start.as_array_mut().ok_or_else(|| {
        DriverError::SettingsIsolationUnavailable(
            "SessionStart hooks overlay is not an array".into(),
        )
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
            "hooks": [{
                "type": "command",
                "command": command
            }]
        }));
    }
    Ok(())
}

fn ensure_settings_flag(argv: &mut Vec<String>, settings_path: &Path) {
    let path = settings_path.to_string_lossy().into_owned();
    if argv.windows(2).any(|pair| pair[0] == "--settings")
        || argv.iter().any(|token| token.starts_with("--settings="))
    {
        if let Some(index) = argv.iter().position(|token| token == "--settings")
            && let Some(value) = argv.get_mut(index + 1)
        {
            *value = path;
        }
        return;
    }
    argv.push("--settings".into());
    argv.push(path);
}

fn validate_setting_sources(argv: &[String]) -> DriverResult<()> {
    if let Some(index) = argv.iter().position(|token| token == "--setting-sources") {
        let value = argv.get(index + 1).map(String::as_str).unwrap_or("");
        if value.trim().is_empty() || value.starts_with('-') {
            return Err(DriverError::NativeFeatureDisabled(
                "empty --setting-sources is prohibited".into(),
            ));
        }
        return Ok(());
    }
    if let Some(token) = argv
        .iter()
        .find(|token| token.starts_with("--setting-sources="))
    {
        let value = token.trim_start_matches("--setting-sources=");
        if value.trim().is_empty() {
            return Err(DriverError::NativeFeatureDisabled(
                "empty --setting-sources is prohibited".into(),
            ));
        }
        return Ok(());
    }
    Ok(())
}

fn write_private(path: &Path, contents: &[u8], mode: u32) -> DriverResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    let _ = mode;
    Ok(())
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn bind_client(server: &HerdrServer, options: &ClaudePtyOptions) -> DriverResult<Client> {
    let mut client = Client::connect(server.socket_path()).with_timeout(Duration::from_secs(30));
    if options.socket_dir.is_none() {
        client = client.with_session_name(server.session_name());
    }
    if let Some(binary) = &options.herdr_binary {
        client = client.with_binary(binary);
    }
    Ok(client)
}

fn herdr_pin(client: &Client, version: &str, protocol: u32) -> DriverResult<HerdrPin> {
    let path = client.binary();
    let digest = crate::binary::hash_file(path).or_else(|_| dummy_digest())?;
    Ok(HerdrPin {
        binary_path: path.to_string_lossy().into_owned(),
        version: version.to_string(),
        digest,
        protocol_version: protocol.to_string(),
        server_identity: Id::new("obj")?,
        server_epoch: Id::new("epoch")?,
        representation: HerdrRepresentation::RenderedAnsi,
    })
}

fn herdr_session_name(spec: &InstanceSpec, fallback: &str) -> String {
    if let remuda_protocol::CarrierSpec::Pty(carrier) = &spec.carrier
        && !carrier.session.is_empty()
    {
        return carrier.session.clone();
    }
    fallback.to_string()
}

fn agent_name_for() -> String {
    // A host ID is shared by every Claude pane; use fresh random UUID bytes.
    let uuid = uuid::Uuid::now_v7().simple().to_string();
    let suffix = &uuid[20..];
    format!("remuda-{suffix}")
}

fn pin_source(source: &BinarySource) -> DriverResult<BinaryPin> {
    match source {
        BinarySource::Pinned(pin) => Ok(pin.clone()),
        BinarySource::Path(path) => pin_binary(path),
        BinarySource::Command(name) => pin_binary(name),
    }
}

fn reject_bot_bypass(spec: &InstanceSpec) -> DriverResult<()> {
    if let remuda_protocol::PermissionMode::Claude(claude) = &spec.permission_mode
        && matches!(
            claude.mode,
            remuda_protocol::ClaudePermissionMode::BypassPermissions
                | remuda_protocol::ClaudePermissionMode::DontAsk
        )
    {
        return Err(DriverError::BypassNotAllowedForBot);
    }
    Ok(())
}

/// The Claude permission mode an instance launched with, if pinned.
pub(crate) fn launch_claude_permission(
    spec: &InstanceSpec,
) -> Option<remuda_protocol::ClaudePermissionMode> {
    match &spec.permission_mode {
        remuda_protocol::PermissionMode::Claude(claude) => Some(claude.mode),
        _ => None,
    }
}

fn native_ref_for(live: &PtyLive) -> NativeRef {
    let recorded = live
        .transcript_path
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let transcript = match recorded {
        Some(path) => match Id::new("obj") {
            Ok(object_id) => Knowledge::Known {
                value: TranscriptRef {
                    object_id,
                    source_path: path,
                },
            },
            Err(_) => Knowledge::Unknown {
                reason: "transcript-id".into(),
                evidence_event_ids: vec![],
            },
        },
        None => Knowledge::Unknown {
            reason: "session-start-pending".into(),
            evidence_event_ids: vec![],
        },
    };
    NativeRef {
        host_id: live.host_id.clone(),
        native_store_id: live.native_store_id.clone(),
        kind: AgentKind::Claude,
        session_id: Knowledge::Known {
            value: live.session_id.clone(),
        },
        transcript,
        signal_tier: None,
        capabilities: Vec::new(),
        codex: None,
        acp: None,
        claude: Some(ClaudeRef {
            session_id: live.session_id.clone(),
        }),
        claude_bg: None,
        agy: None,
        herdr: Some(HerdrRef {
            server: live.herdr_pin.clone(),
            session: live.herdr_session.clone(),
            pane_id: live.pane_id.clone(),
        }),
    }
}

pub(crate) fn map_herdr(err: remuda_herdr::Error) -> DriverError {
    match err {
        remuda_herdr::Error::Io(error) => DriverError::Io(error),
        remuda_herdr::Error::Json(error) => DriverError::Json(error),
        remuda_herdr::Error::BinaryNotFound => DriverError::BinaryNotFound(PathBuf::from("herdr")),
        remuda_herdr::Error::DefaultSessionGuard { socket } => DriverError::InvalidLaunchSpec(
            format!("refusing default herdr socket {}", socket.display()),
        ),
        remuda_herdr::Error::Api {
            code, message: _, ..
        } if matches!(code.as_str(), "agent_not_ready" | "agent_blocked") => {
            DriverError::ControlUnavailable
        }
        // Keep the herdr wording intact: the Node reads it to tell a lost
        // carrier (restartable) from an agent that simply refused the work.
        other if other.is_transient_carrier() => DriverError::CarrierUnavailable(other.to_string()),
        other => DriverError::InvalidLaunchSpec(other.to_string()),
    }
}

/// Shared observation context used by the bg driver.
pub(crate) fn obs_ctx(
    driver: DriverKind,
    instance_id: InstanceId,
    host_id: HostId,
    journal_id: Id,
    run_id: RunId,
    session_id: String,
    pin_version: String,
) -> ObsCtx {
    ObsCtx {
        driver,
        instance_id,
        host_id,
        journal_id,
        run_id,
        session_id,
        pin_version,
    }
}

pub(crate) async fn emit_pty_closed(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
) -> DriverResult<()> {
    let emission = emit_on(
        tx,
        seq,
        ctx,
        SourceChannel::Runtime,
        Completeness::Structured,
        ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Session,
                native_name: "carrier_closed".into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known {
                    value: "exited".into(),
                },
                related_ids: BTreeMap::from([("reason".into(), "owner-forced-stop".into())]),
                data_ref: None,
                severity: Severity::Info,
                affects_completion: false,
            },
        )))),
    );
    // The Node emits its durable exited entity after close returns. A detached or
    // stalled optional observer must not hold successful resource cleanup open.
    match tokio::time::timeout(Duration::from_millis(250), emission).await {
        Ok(result) if !tx.is_closed() => result,
        _ => Ok(()),
    }
}

pub(crate) async fn emit_on(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
    channel: SourceChannel,
    completeness: Completeness,
    payload: ObservationPayload,
) -> DriverResult<()> {
    emit_obs(tx, seq, ctx, channel, completeness, payload).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-027: a PTY driver cannot inline bytes, so an attachment has to reach
    /// the agent as an absolute path it can open with its own Read tool.
    #[test]
    fn a_pty_prompt_mentions_attachment_paths_after_the_text() {
        use remuda_protocol::{Knowledge, MediaBlock, ResourceBlock, TextBlock};
        let id = remuda_protocol::Id::new("obj").expect("id");
        let blocks = vec![
            ContentBlock::Image(Box::new(MediaBlock {
                object_id: id.clone(),
                media_type: "image/png".into(),
                name: Some("shot.png".into()),
                anchor: None,
                size: None,
            })),
            ContentBlock::Resource(Box::new(ResourceBlock {
                uri: "file:///data/instances/ins_1/attachments/obj_1.png".into(),
                media_type: Knowledge::Known {
                    value: "image/png".into(),
                },
                object_id: Some(id),
            })),
            ContentBlock::Text(Box::new(TextBlock {
                text: "what colour is the image?".into(),
            })),
        ];
        let text = blocks_to_text(&blocks).expect("prompt text");
        assert!(text.starts_with("what colour is the image?"), "{text}");
        assert!(
            text.contains("附件: /data/instances/ins_1/attachments/obj_1.png"),
            "{text}"
        );
        assert!(
            text.contains("请读取"),
            "the mention must instruct, not merely name the file: {text}"
        );
    }

    /// The common text-only prompt is untouched by the attachment path.
    #[test]
    fn a_text_only_pty_prompt_is_unchanged() {
        use remuda_protocol::TextBlock;
        let blocks = vec![ContentBlock::Text(Box::new(TextBlock {
            text: "just text".into(),
        }))];
        assert_eq!(blocks_to_text(&blocks).expect("text"), "just text");
    }

    #[test]
    fn resume_argv_replaces_session_id_and_never_continue() {
        let argv = vec![
            "--permission-mode".into(),
            "default".into(),
            "--session-id".into(),
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        ];
        let resumed = argv_for_resume(&argv, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        assert!(resumed.contains(&"--resume".into()));
        assert!(resumed.contains(&"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into()));
        assert!(!resumed.iter().any(|token| token == "--continue"));
        assert!(!resumed.iter().any(|token| token == "--session-id"));
        assert!(!resumed.iter().any(|token| token == "--bare"));
    }

    #[test]
    fn refuse_bare_rejects_prohibited_tokens() {
        assert!(refuse_bare(&["--model".into(), "haiku".into()]).is_ok());
        assert!(refuse_bare(&["--bare".into()]).is_err());
        assert!(refuse_bare(&["--continue".into()]).is_err());
    }
}

/// Mapping / gating helpers used by `tests/claude_pty_review.rs`.
#[doc(hidden)]
pub mod review {
    use super::*;

    /// Final argv check for `--bare` / `--continue` / banned tokens.
    pub fn refuse_argv(argv: &[String]) -> DriverResult<()> {
        refuse_bare(argv)
    }

    /// Rebuild resume argv from a persisted recipe, keeping `--settings`.
    pub fn resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
        argv_for_resume(argv, session_id)
    }

    /// Append a SessionStart command without replacing other hook events.
    pub fn merge_hooks(settings: &mut Value, command: &str) -> DriverResult<()> {
        merge_session_start_hook(settings, command)
    }

    /// Write the launch-dir SessionStart overlay and keep `--settings`.
    pub fn inject_hook(recipe: &mut LaunchRecipe, launch_dir: &Path) -> DriverResult<LaunchRecipe> {
        inject_session_start_hook(recipe, launch_dir)
    }

    /// Map print-style bypass onto the TTY flag.
    pub fn apply_bypass(recipe: &mut LaunchRecipe) {
        apply_tty_bypass_flag(recipe);
    }

    /// Validate an explicit `--setting-sources`; normal sources stay implicit.
    pub fn ensure_sources(argv: &[String]) -> DriverResult<()> {
        validate_setting_sources(argv)
    }

    /// Screen-derived blocked payloads (lifecycle + interaction).
    pub fn blocked_status_payloads() -> DriverResult<Vec<(Completeness, ObservationPayload)>> {
        let ctx = obs_ctx(
            DriverKind::ClaudePty,
            InstanceId::new(),
            HostId::new(),
            Id::new("obj")?,
            RunId::new(),
            "review-session".into(),
            "review".into(),
        );
        let mut payloads = status_payloads(&ctx, AgentStatus::Blocked);
        payloads.push((
            SourceChannel::Herdr,
            Completeness::ScreenDerived,
            ObservationPayload::InteractionRequested(Box::new(
                remuda_protocol::InteractionRequestedPayload {
                    interaction: crate::pty_interaction::screen_request(&ctx, "")?.0,
                },
            )),
        ));
        Ok(payloads
            .into_iter()
            .map(|(_, completeness, payload)| (completeness, payload))
            .collect())
    }

    /// Driver-level bot/dontAsk/bypass gate.
    pub fn reject_bot(spec: &InstanceSpec) -> DriverResult<()> {
        reject_bot_bypass(spec)
    }

    /// Strip named flags (`--session-id`, `--cwd`, …).
    pub fn strip_flags(argv: &[String], names: &[&str]) -> Vec<String> {
        strip_named_flags(argv, names)
    }
}
