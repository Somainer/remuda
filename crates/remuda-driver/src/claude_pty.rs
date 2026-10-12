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
    /// Node-supplied stager for image content blocks a tool result carries
    /// (D-045 §6.2). `None` (default) degrades images to honest text blocks;
    /// bytes are never inlined into observations.
    pub media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
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
            media_stager: None,
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
        // Capture the resume session id before `session` moves into the
        // materialize request; the boundary itself is taken after the final
        // child env is assembled (so it uses the effective CLAUDE_CONFIG_DIR,
        // including any extra_env/MCP override) but before the agent runs.
        let resume_id: Option<String> = match &session {
            crate::materializer::SessionAction::Resume { session_id } => Some(session_id.clone()),
            crate::materializer::SessionAction::New { .. } => None,
        };
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
            native_home_managed: Some(!self.options.inherit_default_config),
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
        // D-045: driver-computed capability handshakes (e.g.
        // REMUDA_CAPABILITY_COMPUTER_USE=1), attached only when granted.
        for entry in recipe
            .env_allowlist
            .iter()
            .filter(|entry| entry.source == crate::recipe::EnvAllowlistSource::Capability)
            // Narrow hole: only the granted-handshake name passes the
            // REMUDA_ deny prefix; the value rides the grant entry.
            .filter(|entry| entry.name == crate::launch::skills::CAPABILITY_COMPUTER_USE_ENV)
        {
            if let Some(value) = entry.value.as_deref() {
                env.insert(entry.name.clone(), value.to_owned());
            }
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

        // D-056 (4), r4 item 3: snapshot the resume boundary from the FINAL
        // child config dir (an extra_env/MCP CLAUDE_CONFIG_DIR override wins
        // over the registered home; otherwise the registered native home the
        // recipe pins), and before the agent runs so even records written
        // between exec and hydration are read as current.
        let config_dir = env
            .get("CLAUDE_CONFIG_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.options.native_home.clone());
        // r4: a resume whose transcript is present is verified; one whose file
        // is not (or which cannot otherwise be bounded) is Unverified — never
        // the fresh byte-0 path.
        let resume_mode = match resume_id.as_deref() {
            Some(id) => crate::claude_transcript::ResumeBoundary::mode_for(
                &config_dir,
                std::path::Path::new(&spec.cwd),
                id,
            ),
            None => crate::claude_transcript::ResumeMode::Fresh,
        };

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
            effort_bridge.note_launch_request(crate::effort::EffortRequest::from_selection(effort));
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
            self.options.media_stager.clone(),
            resume_mode,
        );
        let effort_io: Arc<dyn crate::effort::EffortSwitchIo> = Arc::new(HerdrEffortIo {
            client: client.clone(),
            pane_id: pane_id.clone(),
            events: tx.clone(),
            seq: Arc::clone(&self.seq),
            ctx: ctx.clone(),
            transcript_path: Arc::clone(&transcript_path),
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
            transcript_path: Arc::clone(&transcript_path),
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
    /// §9.1: type `/effort <level>` (plus the D-056 ultracode axis) and let
    /// the transcript pump prove it.
    async fn switch_effort(
        &self,
        level: &str,
        ultracode: Option<bool>,
        axis: Option<remuda_protocol::EffortAxis>,
        request_id: Option<String>,
    ) -> DriverResult<DriverAck> {
        let (pane_id, session_id, queue, ready, io, request) = {
            let inner = self.inner.lock().await;
            let live = inner.as_ref().ok_or(DriverError::ControlUnavailable)?;
            if live.closed {
                return Err(DriverError::ControlUnavailable);
            }
            require_session_start(live)?;
            // D-056: the exact command word depends on the REPORTED build:
            // coupled builds take one bare `ultracode`, decoupled builds take
            // `ultracode on|off` for a same-tier toggle.
            let semantics = remuda_protocol::parse_effort_version(&live.recipe.binary.version)
                .unwrap_or(remuda_protocol::EffortSemantics::Unknown);
            if ultracode == Some(true) && !remuda_protocol::ultracode_supported(semantics) {
                return Err(DriverError::CapabilityUnsupported(format!(
                    "ultracode requires Claude Code >= 2.1.203; this session reports {semantics:?} ({})",
                    live.recipe.binary.version,
                )));
            }
            // c-effortui r4 item 2: classify against the latest OBSERVED
            // effective selection (read-back); fall back to launch/requested
            // provenance only before any read-back exists.
            // c-effortread r15 item 5: the fallback is the VALID LAUNCH
            // selection — a previously armed switch whose write failed left
            // its attempted target in the payload echo, and using it here let
            // a cold flag-on configure masquerade as a same-tier toggle.
            let proven = live.effort_bridge.proven_selection();
            let requested = live.effort_bridge.launch_selection();
            let Some(request) = crate::effort::EffortRequest::for_configure(
                level,
                ultracode,
                axis,
                semantics,
                proven,
                requested,
                request_id.as_deref(),
            ) else {
                // An honest refusal: a word the command does not take, or a
                // flag-on a legacy/ambiguous request cannot disambiguate
                // (c-effortread r9 item 6: execute exactly the stated axis,
                // never a substituted tier command). The Node surfaces the
                // rejection in the UI.
                return Err(DriverError::CapabilityUnsupported(format!(
                    "claude /effort cannot apply {ultracode:?} at {level:?} on this build/request; \
                     valid: low, medium, high, xhigh, max, ultracode, ultracode on, ultracode off"
                )));
            };
            let ready = crate::pty_interaction::prompt_ready(&live.client, &live.pane_id)
                .await
                .is_ok();
            let io = HerdrEffortIo {
                client: live.client.clone(),
                pane_id: live.pane_id.clone(),
                events: live.events.clone(),
                seq: Arc::clone(&self.seq),
                ctx: live.ctx.clone(),
                transcript_path: Arc::clone(&live.transcript_path),
            };
            (
                live.pane_id.clone(),
                live.session_id.clone(),
                Arc::clone(&live.effort_queue),
                ready,
                Arc::new(io) as Arc<dyn crate::effort::EffortSwitchIo>,
                request,
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
            io.journal_effort(
                crate::effort::SwitchOutcome::Queued.journal_status(request.command_word(), ""),
                Severity::Info,
                request.request_id.clone(),
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
                transcript_path: Arc::clone(&live.transcript_path),
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
            io.journal_effort(
                crate::model::ModelSwitchOutcome::Queued.journal_status(&request.id, ""),
                Severity::Info,
                None,
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
            return self
                .switch_effort(
                    level,
                    switch.effort_ultracode,
                    switch.effort_axis,
                    switch.effort_request_id.clone(),
                )
                .await;
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
    media_stager: Option<Arc<dyn remuda_protocol::ToolMediaStager>>,
    // Fresh / verified-resume / unverifiable-resume for this launch.
    resume_mode: crate::claude_transcript::ResumeMode,
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
                    if let Some(stager) = media_stager.clone() {
                        mapper = mapper.with_media_stager(stager);
                    }
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
                    // D-056 (4): the mode decides fresh vs proven vs
                    // unverifiable resume; an unverifiable resume whose
                    // transcript is still absent leaves the hydrator unbound
                    // until the file appears — never a byte-0/current tail.
                    hydrator = resume_mode.open_tail(&path).map(|tail| (tail, mapper));
                }
            }
            if let Some(hydrated) = hydrator.take() {
                // A read error is transient (the file is being appended to);
                // the next tick retries from the same offset.
                // D-045 §6.2: mapping (and the screenshot staging it can do)
                // runs on a blocking thread — the stager parks until the Hub
                // answers, so this never stalls a tokio worker.
                let (tail, mapper, batches) =
                    tokio::task::spawn_blocking(move || -> (
                        crate::claude_transcript::TranscriptTail,
                        TranscriptMapper,
                        Vec<Result<Vec<Observation>, DriverError>>,
                    ) {
                        use crate::claude_transcript::TailProvenance;
                        let (mut tail, mut mapper) = hydrated;
                        // The tail reports whether these bytes are provenanced
                        // to the current process; an unverified batch maps for
                        // conversation only, and a verified→unverified
                        // transition emits an explicit read-back-unavailable
                        // edge that clears the projected effective state.
                        let (provenance, poll_failed, poll_positions) = match tail.poll() {
                            Ok(read) => (read.provenance, read.lines, read.positions),
                            Err(error) => {
                                tracing::debug!(%error, "transcript tail unreadable this poll");
                                let p = if tail.verified() {
                                    TailProvenance::Current
                                } else {
                                    TailProvenance::Unverified
                                };
                                (p, Vec::new(), Vec::new())
                            }
                        };
                        let gate_edges = mapper
                            .apply_effort_provenance(provenance == TailProvenance::Current)
                            .expect("gate provenance is infallible");
                        // Flush the buffered assistant run at the end of the
                        // batch: the mapper holds a run open until superseded,
                        // so the final message of a finished turn would
                        // otherwise wait for the next record to arrive.
                        let mut batches: Vec<Result<Vec<Observation>, DriverError>> =
                            Vec::new();
                        if !gate_edges.is_empty() {
                            batches.push(Ok(gate_edges));
                        }
                        // c-effortread r15 item 4: each record rides with its
                        // verified transcript position so an armed slash
                        // attribution can exclude pre-submission records.
                        batches.extend(
                            poll_failed
                                .iter()
                                .zip(poll_positions.iter().copied())
                                .map(|(line, position)| mapper.map_line_at(line, Some(position))),
                        );
                        batches.push(mapper.flush());
                        (tail, mapper, batches)
                    })
                    .await
                    .unwrap_or_else(|error| {
                        panic!("transcript mapping task panicked: {error}")
                    });
                hydrator = Some((tail, mapper));
                for batch in batches {
                    let mapped = match batch {
                        Ok(mapped) => mapped,
                        Err(error) => {
                            tracing::debug!(%error, "transcript line did not map");
                            continue;
                        }
                    };
                    for observation in mapped {
                        if emit_mapped_obs(&tx, &seq, &ctx, SourceChannel::Transcript, observation)
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

/// Emit a mapper-produced observation while PRESERVING the deterministic
/// `event_id` the transcript mapper minted.
///
/// §9.1: an effort edge's id is derived from the native record (assistant
/// message id or the verdict/attachment record uuid), and the journal tailer
/// derives the same id for the same native edge. The TUI pump rebuilds every
/// other field with this run's identity, but rebuilding the event id with a
/// fresh random [`EventId::new`] — as [`emit_obs`] does — would publish a
/// different id live than the one the journal later derives, breaking live/
/// journal parity.
async fn emit_mapped_obs(
    tx: &mpsc::Sender<Observation>,
    seq: &AtomicU64,
    ctx: &ObsCtx,
    channel: SourceChannel,
    mapped: Observation,
) -> DriverResult<()> {
    let n = seq.fetch_add(1, Ordering::SeqCst) + 1;
    let event_id = mapped.event_id;
    let mut observation = build_observation(ctx, n, channel, mapped.completeness, mapped.body)?;
    observation.event_id = event_id;
    tx.send(observation)
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

/// Resolve the Herdr carrier's pre-submission provenance from its bound
/// transcript path (c-effortread r16 item 3): a successful stat is the
/// verified pre-submit EOF the switch's own slash is appended AT; no binding
/// or a failed stat (the file was rotated/removed between bind and submit) is
/// explicit provenance loss, never a word-only fallback.
fn herdr_submission_provenance(path: Option<&str>) -> crate::effort::SubmissionProvenance {
    use crate::effort::SubmissionProvenance;
    match path {
        Some(path) => match std::fs::metadata(path) {
            Ok(metadata) => SubmissionProvenance::Floor(metadata.len()),
            Err(_) => SubmissionProvenance::Unavailable,
        },
        None => SubmissionProvenance::Unavailable,
    }
}

/// §9.1 [`crate::effort::EffortSwitchIo`] backed by a Herdr pane.
struct HerdrEffortIo {
    client: Client,
    pane_id: String,
    events: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    ctx: ObsCtx,
    /// Bound transcript path, for the c-effortread r15 item 4 submission
    /// floor (EOF captured immediately before typing a switch command).
    transcript_path: Arc<std::sync::Mutex<Option<String>>>,
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

    async fn submission_floor(&self) -> crate::effort::SubmissionProvenance {
        // The bound transcript's EOF immediately before submission: the
        // switch's own slash is appended starting AT this position, so only a
        // record that starts at or after it can be this switch's command
        // (c-effortread r15 item 4, r16 item 1). A missing binding or a failed
        // stat is explicit provenance loss (r16 item 3): the mapper attributes
        // nothing rather than falling back to word-only matching.
        let path = match self.transcript_path.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => None,
        };
        herdr_submission_provenance(path.as_deref())
    }

    async fn journal_effort(&self, status: String, severity: Severity, request_id: Option<String>) {
        // c-effortread r10: echo the configure's per-request id so the web
        // binds the lifecycle edge to the exact in-flight switch.
        let related_ids = request_id
            .map(|id| BTreeMap::from([(remuda_protocol::EFFORT_REQUEST_ID_KEY.to_owned(), id)]))
            .unwrap_or_default();
        let payload = ObservationPayload::Lifecycle(Box::new(LifecyclePayload::Native(Box::new(
            NativeLifecycle {
                topic: LifecycleTopic::Configuration,
                native_name: "instance.configure".into(),
                native_id: Knowledge::NotApplicable,
                status: Knowledge::Known { value: status },
                related_ids,
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

    /// c-effortread r2: the deterministic effort edge id the transcript mapper
    /// mints must survive live publication. The TUI pump used to rebuild every
    /// observation (including a fresh random event id), so the published id did
    /// not match the journal tailer's derived id.
    #[tokio::test]
    async fn mapped_effort_observation_keeps_its_deterministic_event_id() {
        let instance = InstanceId::new();
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        );
        // First assistant record always emits one effort edge with a
        // deterministic id derived from the native message id.
        let line = json!({
            "type": "assistant",
            "uuid": "msg-1",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "id": "msg-1",
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "ok"}],
            },
            "effort": "high",
            "perTurnEffort": null,
        })
        .to_string();
        let mapped = mapper
            .map_line(&line)
            .expect("map")
            .into_iter()
            .find(|obs| matches!(obs.body, ObservationPayload::Effort(_)))
            .expect("an effort edge");
        let deterministic = mapped.event_id.clone();

        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: instance.clone(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").expect("journal id"),
            run_id: RunId::new(),
            session_id: "effort-session".into(),
            pin_version: "2.1.289".into(),
        };
        let (tx, mut rx) = mpsc::channel(4);
        let seq = Arc::new(AtomicU64::new(0));
        emit_mapped_obs(&tx, &seq, &ctx, SourceChannel::Transcript, mapped)
            .await
            .expect("emit");
        let published = rx.try_recv().expect("the observation was published");
        assert_eq!(
            published.event_id, deterministic,
            "the live TUI channel must publish the mapper's deterministic id"
        );
    }

    /// A `/effort <word>` user record shaped exactly like claude writes it.
    fn effort_slash_line(uuid: &str, word: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "role": "user",
                "content": format!(
                    "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n\
                     <command-args>{word}</command-args>"
                )
            }
        })
        .to_string()
    }

    /// A `<local-command-stdout>` verdict user record.
    fn effort_stdout_line(uuid: &str, body: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "role": "user",
                "content": format!("<local-command-stdout>{body}</local-command-stdout>")
            }
        })
        .to_string()
    }

    /// c-effortread r11 item 2: an accepted `/effort` verdict that changes
    /// NOTHING (a no-op switch targeting the state already in effect) produces
    /// no tracker edge, but it still has to publish request-specific
    /// completion evidence for OUR armed command — an observation carrying the
    /// request id — so the web's moved axes can settle from the id instead of
    /// leaving the chip on 切换中. Goes red when the no-edge accepted arm
    /// emits nothing.
    #[tokio::test]
    async fn r11_item2_an_accepted_noop_verdict_still_echoes_the_request_id() {
        use crate::effort::{EffortBridge, EffortRequest};
        use remuda_protocol::EffortAxis::Flag;
        use remuda_protocol::EffortSemantics::Decoupled;

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // The human already switched the flag OFF in the terminal: latch
        // {high, flag false} via the accepted verdict (normal edge).
        mapper
            .map_line(&effort_slash_line("cmd-human", "ultracode off"))
            .expect("map human slash");
        let first = mapper
            .map_line(&effort_stdout_line(
                "out-human",
                "Ultracode off. Effort stays high.",
            ))
            .expect("map human verdict");
        assert!(
            first
                .iter()
                .any(|obs| matches!(obs.body, ObservationPayload::Effort(_))),
            "the first OFF verdict latches with a normal edge"
        );

        // Remuda now runs a NO-OP OFF configure (target already in effect).
        let request = EffortRequest::for_configure(
            "high",
            Some(false),
            Some(Flag),
            Decoupled,
            None,
            None,
            Some("req-noop"),
        )
        .expect("off toggle");
        bridge.arm(request);
        mapper
            .map_line(&effort_slash_line("cmd-remuda", "ultracode off"))
            .expect("map remuda slash");
        let out = mapper
            .map_line(&effort_stdout_line(
                "out-remuda",
                "Ultracode off. Effort stays high.",
            ))
            .expect("map remuda no-op verdict");
        let edge = out
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("a no-op accepted verdict still publishes completion evidence");
        assert_eq!(edge.request_id.as_deref(), Some("req-noop"));
        assert_eq!(edge.effective.ultracode, Some(false));
        assert!(
            !bridge.has_pending(),
            "the accepted no-op resolves the armed switch"
        );
    }

    /// c-effortread r11 item 1: each `/effort` verdict is labelled with the
    /// request whose SLASH GENERATION asked for it, not the bridge's latest
    /// armed request. Production sequence through the real mapper:
    /// A executes (`ultracode off`) → A times out → B armed (`ultracode on`)
    /// → A's delayed accepted-OFF verdict arrives. It must resolve only A's
    /// stale generation (B stays pending) and its echo must carry A's id, not
    /// B's; B's own later ON verdict then settles B.
    #[tokio::test]
    async fn r11_item1_a_delayed_verdict_carries_its_own_request_id_not_b() {
        use crate::effort::{EffortBridge, EffortRequest};
        use remuda_protocol::EffortAxis::Flag;
        use remuda_protocol::EffortName::High;
        use remuda_protocol::EffortSemantics::Decoupled;

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        let request_a = EffortRequest::for_configure(
            "high",
            Some(false),
            Some(Flag),
            Decoupled,
            None,
            None,
            Some("req-a"),
        )
        .expect("A is the off toggle");
        assert_eq!(request_a.command_word(), "ultracode off");
        let gen_a = bridge.arm(request_a);
        mapper
            .map_line(&effort_slash_line("cmd-a", "ultracode off"))
            .expect("map A slash");

        // A's read-back window passes with no verdict: the worker gives up on
        // exactly A's generation (pending clears), exactly as perform_switch.
        bridge.fail(gen_a);

        let request_b = EffortRequest::for_configure(
            "high",
            Some(true),
            Some(Flag),
            Decoupled,
            None,
            None,
            Some("req-b"),
        )
        .expect("B is the on toggle");
        let _gen_b = bridge.arm(request_b);
        mapper
            .map_line(&effort_slash_line("cmd-b", "ultracode on"))
            .expect("map B slash");

        // A's DELAYED verdict lands after B's slash: accepted OFF.
        let out_a = mapper
            .map_line(&effort_stdout_line(
                "out-a",
                "Ultracode off. Effort stays high.",
            ))
            .expect("map A verdict");
        let edge_a = out_a
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("A's verdict emits an effort edge");
        assert_eq!(
            edge_a.request_id.as_deref(),
            Some("req-a"),
            "the delayed verdict echoes ITS request, not B"
        );
        assert_eq!(edge_a.effective.ultracode, Some(false));
        assert!(
            bridge.has_pending(),
            "resolving A's stale generation must not clear B"
        );
        let pending_b = bridge.pending().expect("B stays armed");
        assert_eq!(pending_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(pending_b.name, High);

        // B's own ON verdict settles B.
        let out_b = mapper
            .map_line(&effort_stdout_line(
                "out-b",
                "Ultracode on (this session only): dynamic workflows on every \
                 task. Effort stays high.",
            ))
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's verdict emits an effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(edge_b.effective.ultracode, Some(true));
        assert!(!bridge.has_pending(), "B's verdict settles B");
    }

    /// c-effortread r12 item 1: the slash binds its (generation, request)
    /// tuple from ONE bridge snapshot. In production the switch worker runs
    /// on another task: if A times out and B is armed in the gap between two
    /// snapshots, the old mapper stored B's tuple for A's slash, so A's
    /// delayed verdict carried B's id and cleared B. The snapshot hook
    /// reproduces that interleaving DETERMINISTICALLY, while A's slash record
    /// is being mapped.
    #[tokio::test]
    async fn r12_item1_slash_binds_its_own_tuple_when_rearm_hits_snapshot_gap() {
        use crate::effort::{EffortBridge, EffortRequest};
        use remuda_protocol::EffortAxis::Flag;
        use remuda_protocol::EffortSemantics::Decoupled;

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        let request_a = EffortRequest::for_configure(
            "high",
            Some(false),
            Some(Flag),
            Decoupled,
            None,
            None,
            Some("req-a"),
        )
        .expect("A is the off toggle");
        let gen_a = bridge.arm(request_a);

        // Between the snapshot taken for A's slash and the tuple attribution,
        // the worker gives up on A and arms B.
        let rearm_bridge = Arc::clone(&bridge);
        mapper.set_effort_slash_snapshot_hook(Box::new(move || {
            rearm_bridge.fail(gen_a);
            let request_b = EffortRequest::for_configure(
                "high",
                Some(true),
                Some(Flag),
                Decoupled,
                None,
                None,
                Some("req-b"),
            )
            .expect("B is the on toggle");
            rearm_bridge.arm(request_b);
        }));

        mapper
            .map_line(&effort_slash_line("cmd-a", "ultracode off"))
            .expect("map A slash while the re-arm hits the snapshot gap");

        // A's DELAYED verdict lands afterwards: it must carry A's id and must
        // not resolve B's generation.
        let out_a = mapper
            .map_line(&effort_stdout_line(
                "out-a",
                "Ultracode off. Effort stays high.",
            ))
            .expect("map A verdict");
        let edge_a = out_a
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("A's verdict emits an effort edge");
        assert_eq!(
            edge_a.request_id.as_deref(),
            Some("req-a"),
            "A's slash was bound from the snapshot taken before the re-arm"
        );
        assert_eq!(edge_a.effective.ultracode, Some(false));
        assert!(
            bridge.has_pending(),
            "A's stale generation must not clear the newly armed B"
        );
        let pending_b = bridge.pending().expect("B stays armed");
        assert_eq!(pending_b.request_id.as_deref(), Some("req-b"));

        // B's own later ON verdict settles B.
        mapper
            .map_line(&effort_slash_line("cmd-b", "ultracode on"))
            .expect("map B slash");
        let out_b = mapper
            .map_line(&effort_stdout_line(
                "out-b",
                "Ultracode on (this session only): dynamic workflows on every \
                 task. Effort stays high.",
            ))
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's verdict emits an effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(edge_b.effective.ultracode, Some(true));
        assert!(!bridge.has_pending(), "B's verdict settles B");
    }

    /// c-effortread r15 item 4 / r16 item 1: an older native `/effort max`
    /// slash already on disk but still UNREAD when web request B (also max)
    /// is armed must not be attributed to B — its old refusal must never
    /// reject B. The floor is the bound transcript's EOF captured immediately
    /// before B submits, and B's OWN slash record is appended to the file
    /// exactly AT that byte offset: the first byte after the pre-submit EOF
    /// starts at position L, so a record starting at L is B's command, not a
    /// pre-submission one. Only records starting BELOW L are excluded.
    #[tokio::test]
    async fn r16_item1_a_slash_appended_exactly_at_the_pre_submit_eof_claims_its_switch() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        // A REAL transcript file: the offsets below are its measured byte
        // lengths, not magic constants.
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("effort-session.jsonl");
        let prefix = json!({
            "type": "assistant",
            "uuid": "msg-prefix",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {"role": "assistant", "content": "warming the transcript"},
        })
        .to_string();
        std::fs::write(&path, format!("{prefix}\n")).expect("seed prefix");

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // The OLDER native /effort max (a human or a replaced command) is
        // already appended — slash plus its Kept refusal — but the pump has
        // not read either yet. Record both records' REAL start offsets.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        let pos_old_slash = std::fs::metadata(&path).expect("stat").len();
        use std::io::Write;
        writeln!(file, "{}", effort_slash_line("cmd-old", "max")).expect("append old slash");
        let pos_old_refusal = std::fs::metadata(&path).expect("stat").len();
        writeln!(
            file,
            "{}",
            effort_stdout_line("out-old", "Kept effort level as high")
        )
        .expect("append old refusal");
        assert!(pos_old_slash < pos_old_refusal);

        // B is armed at the bound transcript's EOF L, the exact byte offset
        // perform_switch stats immediately before typing (carrier seam).
        let floor = std::fs::metadata(&path).expect("stat").len();
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(floor)));

        // B's own command is appended to the file IMMEDIATELY AT L: the next
        // record after a length-L file starts at byte L, so its verified
        // position equals the floor.
        let pos_b_slash = std::fs::metadata(&path).expect("stat").len();
        assert_eq!(pos_b_slash, floor, "the next append starts exactly at EOF");
        writeln!(file, "{}", effort_slash_line("cmd-b", "max")).expect("append B slash");
        let pos_b_verdict = std::fs::metadata(&path).expect("stat").len();
        writeln!(
            file,
            "{}",
            effort_stdout_line("out-b", "Set effort level to max")
        )
        .expect("append B verdict");
        drop(file);

        // The pump catches up: the OLDER slash starts strictly BELOW the floor
        // and is not attributed to B, so its refusal neither emits a request
        // edge nor rejects B.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(pos_old_slash))
            .expect("map old slash");
        let old_verdict = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Kept effort level as high"),
                Some(pos_old_refusal),
            )
            .expect("map old refusal");
        assert!(
            old_verdict
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the pre-submission refusal emits no request edge"
        );
        assert!(
            bridge.has_pending(),
            "B is not rejected by the older command's refusal"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "no verdict of the older command resolves B"
        );

        // B's own slash starts AT the floor and arms; its accepted max verdict
        // resolves B and carries B's id.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(pos_b_slash))
            .expect("map B slash at the floor");
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(pos_b_verdict),
            )
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's verdict emits an effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(
            edge_b.effective.name,
            Some(remuda_protocol::EffortName::Max)
        );
        assert!(!bridge.has_pending(), "B's own verdict settles B");
    }

    /// c-effortread r16 item 2: a slash excluded from the armed request (an
    /// older pre-floor command, or a human `/effort`) must still keep its PLACE
    /// in the slash → verdict FIFO. Without that slot the old command's
    /// delayed verdict pops the newer owned request B and rejects (or settles)
    /// it. Sequence: old slash → B slash → old verdict → B verdict; the old
    /// Kept refusal answers the unowned slot, B's own acceptance answers B.
    #[tokio::test]
    async fn r16_item2_a_pre_floor_slash_keeps_a_fifo_slot_so_its_verdict_never_consumes_b() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // B arms at EOF 1000; nothing of B's is on disk yet.
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));

        // The pump catches up an OLDER same-word slash sitting below the floor.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        assert!(
            bridge.has_pending(),
            "the pre-floor slash does not arm/clear B"
        );

        // B's own slash lands at/after the floor and arms.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");

        // The OLDER command's delayed Kept verdict is read BETWEEN B's slash
        // and B's verdict: it answers the old slash's slot, so it never rejects
        // B and emits no request edge.
        let old_verdict = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Kept effort level as high"),
                Some(1600),
            )
            .expect("map old refusal");
        assert!(
            old_verdict
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old refusal carries no request edge"
        );
        assert!(
            bridge.has_pending(),
            "the old refusal must not reject B through the FIFO"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B is still awaiting ITS OWN verdict"
        );

        // B's own acceptance now pops the owned slot and settles B with B's id.
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(1700),
            )
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's verdict emits an effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert!(!bridge.has_pending(), "B's own verdict settles B");
    }

    /// c-effortread r17 item 1: the r16-item2 case with an ACCEPTED old verdict
    /// (not only a Kept refusal). The older pre-floor command is a SAME-WORD
    /// `/effort max` whose "Set effort level to max" verdict lands between B's
    /// slash and B's verdict. With B still armed, that unowned acceptance used
    /// to borrow B's awaiting attribution (the edge was labelled Remuda) and
    /// emit an id-less effective projection the store then settled B from. Now
    /// it folds the transcript level silently (no observation, no bridge
    /// observed refresh) and B settles only from ITS OWN id-bearing verdict.
    #[tokio::test]
    async fn r17_item1_an_unowned_accepted_verdict_while_b_is_armed_emits_nothing_and_b_waits_for_its_own()
     {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // B arms at EOF 1000; nothing of B's is on disk yet.
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));

        // The pump catches up an OLDER same-word slash sitting below the floor.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        // B's own slash lands at/after the floor and arms.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");

        // The OLDER command's delayed ACCEPTED verdict (same word max) is read
        // BETWEEN B's slash and B's verdict: it answers the unowned slot while
        // B is still armed, so it emits NO settlement-eligible edge and does
        // not touch the bridge (neither resolve nor observed refresh).
        let old_verdict = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(1600),
            )
            .expect("map old acceptance");
        assert!(
            old_verdict
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the unowned acceptance emits no effective projection while B is armed"
        );
        assert!(
            bridge.has_pending(),
            "the unowned acceptance never resolves B"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B is still awaiting ITS OWN verdict"
        );

        // B's own acceptance now pops the owned slot, settles B with B's id,
        // and carries the Remuda attribution (the old verdict did not consume
        // B's armed awaiting).
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(1700),
            )
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's verdict emits an effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(
            edge_b.effective.name,
            Some(remuda_protocol::EffortName::Max)
        );
        assert_eq!(
            edge_b.effective.source,
            remuda_protocol::EffortSource::Remuda
        );
        assert!(!bridge.has_pending(), "B's own verdict settles B");
    }

    /// c-effortread r18 item 1: the older pre-floor command's ACCEPTED verdict
    /// is read BEFORE B's own slash record acquires its OWNED slot (the pump
    /// catches up the old slash and its verdict while B is armed, then reaches
    /// B's slash). The r17 guard keyed on a later OWNED slot in the FIFO: with
    /// none yet pushed it fell through, and the id-less terminal projection
    /// settled B through the store's value fallback (a later dismissal then
    /// toasted against a cleared request). The r18 guard keys on the BRIDGE:
    /// any armed switch suppresses the unowned verdict; B's own later verdict
    /// still settles B with B's id.
    #[tokio::test]
    async fn r18_item1_a_pre_floor_accept_before_bs_slash_is_suppressed_and_bs_own_verdict_settles_b()
     {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // B arms at EOF 1000; nothing of B's is on disk yet.
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));

        // The pump catches up the OLD same-word slash (500) and its ACCEPTED
        // verdict (600) BEFORE B's own slash record (1500) is read. The old
        // verdict pops the unowned slot while no owned FIFO slot exists yet —
        // but the bridge still has B armed, so it is suppressed.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        let old_verdict = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(600),
            )
            .expect("map old acceptance");
        assert!(
            old_verdict
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the pre-slash unowned acceptance emits no projection while B is armed"
        );
        assert!(
            bridge.has_pending(),
            "the pre-slash unowned acceptance never resolves B"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B keeps waiting for ITS OWN verdict"
        );

        // B's own slash then acquires the owned FIFO slot, and B's own
        // accepted verdict settles B with B's id.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(1700),
            )
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's own verdict emits the effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(
            edge_b.effective.name,
            Some(remuda_protocol::EffortName::Max)
        );
        assert!(!bridge.has_pending(), "B's own verdict settles B");
    }

    /// c-effortread r18 item 1 refusal variant: the older pre-floor command's
    /// refusal is likewise read BEFORE B's own slash acquires its owned slot.
    /// The FIFO-keyed r17 guard found no owned slot and fell through to
    /// `note_stdout`, resetting the tracker's attribution; the r18 bridge-keyed
    /// guard folds NOTHING while B is armed. Observable on the next natural
    /// read-back: its edge keeps the terminal `Slash` attribution instead of
    /// being reset to `Unknown`, and B's own later acceptance still settles as
    /// Remuda with B's id.
    #[tokio::test]
    async fn r18_item1_a_pre_floor_refusal_before_bs_slash_folds_nothing_and_bs_accept_still_settles()
     {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));

        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        let old_refusal = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Kept effort level as high"),
                Some(600),
            )
            .expect("map old refusal");
        assert!(
            old_refusal
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the pre-slash unowned refusal emits nothing"
        );
        assert!(bridge.has_pending(), "the old refusal never rejects B");
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B keeps waiting for ITS OWN verdict"
        );

        // A natural assistant read-back BEFORE B's slash keeps the terminal
        // attribution: the folded-away refusal never reset pending_source.
        let readback = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-max",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-max", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "max",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(700),
            )
            .expect("map readback");
        let readback_edge = readback
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("the assistant readback emits its edge");
        assert_eq!(
            readback_edge.effective.source,
            remuda_protocol::EffortSource::Slash,
            "the skipped refusal never reset the terminal attribution to Unknown"
        );

        // B's slash arms and B's own accepted verdict is credited Remuda with
        // B's id despite the earlier refusal and read-back.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(1700),
            )
            .expect("map B verdict");
        let edge_b = out_b
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("B's own verdict emits the effort edge");
        assert_eq!(edge_b.request_id.as_deref(), Some("req-b"));
        assert_eq!(
            edge_b.effective.source,
            remuda_protocol::EffortSource::Remuda
        );
        assert!(!bridge.has_pending());
    }

    /// c-effortread r18 item 3: the suppressed unowned acceptance must not
    /// consume the effective-state update permanently. Sequence: high is
    /// published; an older pre-floor `/effort max` is accepted and suppressed
    /// while B is armed; B's own dialog is dismissed (Kept); the deferred max
    /// edge is then published under the ORIGINAL verdict record's event id;
    /// the later assistant max read-back dedups.
    #[tokio::test]
    async fn r18_item3_a_suppressed_pre_floor_accept_is_published_after_bs_refusal_under_its_own_record_id()
     {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // Published baseline high.
        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");

        // The old acceptance is suppressed: nothing emitted yet.
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(1600),
            )
            .expect("map old acceptance");
        assert!(
            suppressed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old acceptance stays suppressed while B is armed"
        );
        assert!(bridge.has_pending());

        // B's OWN dialog is dismissed (Kept). B rejects and the deferred max
        // edge publishes on the same mapper call, id-less and terminal-side.
        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1700),
            )
            .expect("map B refusal");
        let deferred_edge = dismissed
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some((obs, payload)),
                _ => None,
            })
            .expect("the suppressed max edge publishes after B's refusal");
        let (observation, payload) = deferred_edge;
        assert_eq!(payload.request_id.as_deref(), None, "never relabelled B");
        assert_eq!(
            payload.effective.name,
            Some(remuda_protocol::EffortName::Max)
        );
        assert_eq!(
            payload.effective.source,
            remuda_protocol::EffortSource::Slash
        );
        // Live-vs-journal parity: the deferred edge keeps the ORIGINAL verdict
        // record's deterministic id.
        assert_eq!(
            observation.event_id,
            remuda_protocol::effort_record_event_id(
                instance_id.as_id().as_str(),
                remuda_protocol::EFFORT_STDOUT_NATIVE,
                "out-old",
                remuda_protocol::EffortName::Max,
            )
        );
        assert!(!bridge.has_pending(), "B's refusal ends B");
        let verdict = bridge
            .wait(gen_b, std::time::Duration::from_millis(40))
            .await
            .expect("B recorded the rejection");
        assert!(matches!(verdict, crate::effort::Readback::Rejected { .. }));

        // Assistant records now report max: the value is already published, so
        // this read-back is a dedup and emits no second edge.
        let after = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-max",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-max", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "max",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(1800),
            )
            .expect("map assistant max");
        assert!(
            after
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the already-published max read-back is a dedup, not a second edge"
        );
    }

    /// c-effortread r18 item 3 read-back variant: the armed switch B ends
    /// through its bounded timeout WITHOUT any verdict reaching the mapper
    /// (failed-floor attribution), and the next authoritative assistant
    /// read-back reports the suppressed value. Because suppression no longer
    /// latches the last published state, that same-value read-back EMITS the
    /// deferred effective edge instead of being swallowed as an unchanged
    /// dedup — while B itself stays unresolved by the read-back.
    #[tokio::test]
    async fn r18_item3_after_a_suppressed_accept_the_next_same_value_readback_emits_while_b_stays_pending()
     {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        // The pump catches up the old slash and its acceptance BEFORE B's own
        // slash record is read: its edge is suppressed and stashed.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(600),
            )
            .expect("map old acceptance");
        assert!(
            suppressed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "suppressed while B is armed"
        );

        // B never produces a mappable verdict (bounded path). Assistant
        // records now report max — the same-value read-back EMITS the edge the
        // suppression withheld (its own assistant message id).
        let readback = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-max",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-max", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "max",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(700),
            )
            .expect("map assistant max");
        let edge = readback
            .iter()
            .find_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some(payload),
                _ => None,
            })
            .expect("the same-value read-back publishes the deferred max");
        assert_eq!(edge.effective.name, Some(remuda_protocol::EffortName::Max));
        assert_eq!(edge.effective.source, remuda_protocol::EffortSource::Slash);
        assert_eq!(edge.request_id.as_deref(), None);
        // The read-back never resolves B: an assistant record is not B's
        // verdict; B ends through its own bounded path.
        assert!(bridge.has_pending());
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B stays pending for its bounded timeout"
        );
    }

    /// c-effortread r19 item 2: (name, ultracode, requestId) of every effort
    /// observation in one mapper output.
    fn r19_effort_edges(
        out: &[Observation],
    ) -> Vec<(
        Option<remuda_protocol::EffortName>,
        Option<bool>,
        Option<String>,
    )> {
        out.iter()
            .filter_map(|obs| match &obs.body {
                ObservationPayload::Effort(payload) => Some((
                    payload.effective.name,
                    payload.effective.ultracode,
                    payload.request_id.clone(),
                )),
                _ => None,
            })
            .collect()
    }

    /// A `/model` accepted verdict carrying the ` with `<level>` effort`
    /// clause (c-effortread r19 item 2 scenario b).
    fn r19_model_stdout_line(uuid: &str, level: &str) -> String {
        json!({
            "type": "user",
            "uuid": uuid,
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "role": "user",
                "content": format!(
                    "<local-command-stdout>Set model to `Opus 5.5` and saved as your default \
                     for new sessions with `{level}` effort</local-command-stdout>"
                )
            }
        })
        .to_string()
    }

    /// An `ultra_effort_enter|exit` attachment record.
    fn r19_attachment_line(uuid: &str, label: &str) -> String {
        json!({
            "isSidechain": false,
            "attachment": {"type": label, "reminderType": "full"},
            "type": "attachment",
            "uuid": uuid,
            "sessionId": "effort-session",
            "version": "2.1.289",
        })
        .to_string()
    }

    /// c-effortread r19 item 2 ruling B, scenario (a): the suppressed stash is
    /// ONE latest slot. With high published and B=max armed, a suppressed
    /// `/effort max` acceptance is overwritten by a later suppressed
    /// `/effort high` acceptance (a return to baseline); when B is Kept, the
    /// slot publishes nothing — max is never published and effective stays
    /// high.
    #[tokio::test]
    async fn r19_item2_a_suppressed_return_to_baseline_overwrites_max_and_nothing_publishes() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // Published baseline high.
        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old-max", "max"), Some(500))
            .expect("map old max slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-old-high", "high"), Some(700))
            .expect("map old high slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");

        // The old max acceptance is suppressed: stashed, nothing emitted.
        let suppressed_max = mapper
            .map_line_at(
                &effort_stdout_line("out-old-max", "Set effort level to max"),
                Some(1600),
            )
            .expect("map old max acceptance");
        assert!(
            suppressed_max
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old max acceptance stays suppressed"
        );

        // The later old HIGH acceptance is also suppressed and OVERWRITES the
        // slot (a return to baseline), still emitting nothing.
        let suppressed_high = mapper
            .map_line_at(
                &effort_stdout_line("out-old-high", "Set effort level to high"),
                Some(1700),
            )
            .expect("map old high acceptance");
        assert!(
            suppressed_high
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the return-to-baseline acceptance stays suppressed"
        );
        assert!(bridge.has_pending());

        // B's OWN dialog is dismissed (Kept). B rejects; the one stashed slot
        // is high == the published high, so it publishes NOTHING.
        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1800),
            )
            .expect("map B refusal");
        assert!(
            dismissed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "no deferred edge after B ends: the stashed value was already published"
        );
        assert!(!bridge.has_pending());
        let verdict = bridge
            .wait(gen_b, std::time::Duration::from_millis(40))
            .await
            .expect("B recorded the rejection");
        assert!(matches!(verdict, crate::effort::Readback::Rejected { .. }));

        // Neither suppressed verdict's deterministic edge id is ever emitted,
        // and max is never the effective state.
        for (record, level) in [
            ("out-old-max", remuda_protocol::EffortName::Max),
            ("out-old-high", remuda_protocol::EffortName::High),
        ] {
            let suppressed_id = remuda_protocol::effort_record_event_id(
                instance_id.as_id().as_str(),
                remuda_protocol::EFFORT_STDOUT_NATIVE,
                record,
                level,
            );
            for out in [&suppressed_max, &suppressed_high, &dismissed] {
                assert!(
                    out.iter().all(|obs| obs.event_id != suppressed_id),
                    "the suppressed {record:?} edge is never emitted"
                );
            }
        }
        // A later assistant high record dedups: effective stays high, no edge.
        let after = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high-2",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high-2", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(1900),
            )
            .expect("map assistant high");
        assert!(
            r19_effort_edges(&after).is_empty(),
            "no max is ever published; high stays the effective level"
        );
    }

    /// c-effortread r19 item 2 ruling B, scenario (b): a suppressed max is
    /// superseded by a `/model` acceptance reporting xhigh; B is then refused
    /// and the effective state stays xhigh — the stashed max edge is dropped
    /// through the shared invalidation, never published.
    #[tokio::test]
    async fn r19_item2_b_a_model_readback_clears_the_stash_and_b_refuses_with_effective_xhigh() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(1600),
            )
            .expect("map old acceptance");
        assert!(
            suppressed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old max acceptance stays suppressed"
        );

        // A /model acceptance reports xhigh effort: an authoritative
        // effective-state publication that emits xhigh and clears the stash.
        let model = mapper
            .map_line_at(&r19_model_stdout_line("out-model", "xhigh"), Some(1700))
            .expect("map /model acceptance");
        let model_effort = r19_effort_edges(&model);
        assert_eq!(
            model_effort
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>(),
            vec![Some(remuda_protocol::EffortName::Xhigh)],
            "the /model effort clause publishes xhigh"
        );

        // B is then refused (Kept). No switch remains armed; the dropped stash
        // must not resurface as max.
        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1800),
            )
            .expect("map B refusal");
        assert!(
            dismissed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the suppressed max is never published after B's refusal"
        );
        assert!(matches!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .expect("B recorded the rejection"),
            crate::effort::Readback::Rejected { .. }
        ));
        let suppressed_id = remuda_protocol::effort_record_event_id(
            instance_id.as_id().as_str(),
            remuda_protocol::EFFORT_STDOUT_NATIVE,
            "out-old",
            remuda_protocol::EffortName::Max,
        );
        for out in [&model, &dismissed] {
            assert!(
                out.iter().all(|obs| obs.event_id != suppressed_id),
                "the suppressed max edge id is never emitted"
            );
        }
        // Effective stays xhigh.
        let after = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-xhigh",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-xhigh", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "xhigh",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(1900),
            )
            .expect("map assistant xhigh");
        assert!(
            r19_effort_edges(&after).is_empty(),
            "xhigh is the published effective state and simply dedups"
        );
    }

    /// c-effortread r19 item 2 ruling B, scenario (c): a suppressed max is
    /// superseded by an `ultra_effort_enter` attachment; B is then Kept and
    /// the effective state stays `{high, ultracode on}` — the tracker flag
    /// agrees — and the stashed max never publishes.
    #[tokio::test]
    async fn r19_item2_c_an_ultra_attachment_clears_the_stash_and_b_refuses_with_flag_on() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max"),
                Some(1600),
            )
            .expect("map old acceptance");
        assert!(
            suppressed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old max acceptance stays suppressed"
        );

        // The ultra_effort_enter attachment is authoritative flag evidence:
        // it emits {high, ultracode on} and clears the suppressed max stash.
        let attachment = mapper
            .map_line_at(
                &r19_attachment_line("att-enter", "ultra_effort_enter"),
                Some(1700),
            )
            .expect("map attachment");
        assert_eq!(
            r19_effort_edges(&attachment),
            vec![(Some(remuda_protocol::EffortName::High), Some(true), None)],
            "the attachment publishes high with ultracode on"
        );

        // B is refused: the dropped stash never publishes max.
        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1800),
            )
            .expect("map B refusal");
        assert!(
            dismissed
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the suppressed max is never published after B's refusal"
        );
        assert!(matches!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .expect("B recorded the rejection"),
            crate::effort::Readback::Rejected { .. }
        ));

        // The tracker flag agrees with the published ultracode value: a sparse
        // repeat ENTER is a dedup (no edge), and a later high assistant record
        // keeps {high, on} and likewise emits nothing.
        let repeat = mapper
            .map_line_at(
                &r19_attachment_line("att-repeat", "ultra_effort_enter"),
                Some(1900),
            )
            .expect("map sparse repeat attachment");
        assert!(
            r19_effort_edges(&repeat).is_empty(),
            "the flag already agrees: a sparse repeat enter emits nothing"
        );
        let after = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high-2",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high-2", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(2000),
            )
            .expect("map assistant high");
        assert!(
            r19_effort_edges(&after).is_empty(),
            "effective stays high with ultracode on; max is never published"
        );
    }

    /// c-effortread r20 item 1 ruling B′ (ENTER): with `{high, off}`
    /// published and B=max armed, a SUPPRESSED `Set effort level to max ·
    /// Ultracode on` acceptance moves the tracker's flag latch to on without
    /// publishing. The following `ultra_effort_enter` attachment must still
    /// publish `{high, on}` EXACTLY ONCE (dedupe against the last PUBLISHED
    /// tuple, never the moved latch). When B is then Kept nothing stale
    /// surfaces: the stashed {max, on} never publishes and max is never the
    /// effective level.
    #[tokio::test]
    async fn r20_item1_enter_after_a_flag_moving_suppressed_accept_publishes_high_on_once() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // Assistant high, then a human slider publishes {high, off} (bridge
        // idle, so the unowned verdict takes the normal path).
        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");
        mapper
            .map_line_at(&effort_slash_line("cmd-high-off", "high"), Some(430))
            .expect("map human high slash");
        let baseline = mapper
            .map_line_at(
                &effort_stdout_line("out-high-off", "Set effort level to high · Ultracode off"),
                Some(450),
            )
            .expect("map high/off accept");
        assert_eq!(
            r19_effort_edges(&baseline),
            vec![(Some(remuda_protocol::EffortName::High), Some(false), None)],
            "the published baseline is {{high, off}}"
        );

        // B=max armed; an older pre-floor slash is answered first.
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to max · Ultracode on"),
                Some(1600),
            )
            .expect("map suppressed max/on acceptance");
        assert!(
            r19_effort_edges(&suppressed).is_empty(),
            "the old max/on acceptance stays suppressed"
        );
        assert!(bridge.has_pending());

        // The attachment ENTER publishes {high, on} despite the latch already
        // being on, exactly once.
        let attachment = mapper
            .map_line_at(
                &r19_attachment_line("att-enter", "ultra_effort_enter"),
                Some(1700),
            )
            .expect("map enter attachment");
        assert_eq!(
            r19_effort_edges(&attachment),
            vec![(Some(remuda_protocol::EffortName::High), Some(true), None)],
            "the attachment publishes {{high, on}} once"
        );

        // B is Kept: the dropped stash must never publish {max, on}.
        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1800),
            )
            .expect("map B refusal");
        assert!(
            r19_effort_edges(&dismissed).is_empty(),
            "nothing stale publishes after B's Kept"
        );
        assert!(matches!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .expect("B recorded the rejection"),
            crate::effort::Readback::Rejected { .. }
        ));
        let suppressed_id = remuda_protocol::effort_record_event_id(
            instance_id.as_id().as_str(),
            remuda_protocol::EFFORT_STDOUT_NATIVE,
            "out-old",
            remuda_protocol::EffortName::Max,
        );
        for out in [&suppressed, &dismissed] {
            assert!(
                out.iter().all(|obs| obs.event_id != suppressed_id),
                "the suppressed max/on edge id is never emitted"
            );
        }
        // The published tuple is {high, on}: a sparse repeat enter and a high
        // assistant record both dedup, and max never surfaces.
        let repeat = mapper
            .map_line_at(
                &r19_attachment_line("att-repeat", "ultra_effort_enter"),
                Some(1900),
            )
            .expect("map sparse repeat enter");
        assert!(
            r19_effort_edges(&repeat).is_empty(),
            "a sparse repeat enter dedups against the published tuple"
        );
        let after = mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high-2",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high-2", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "done"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(2000),
            )
            .expect("map assistant high");
        assert!(
            r19_effort_edges(&after).is_empty(),
            "effective stays {{high, on}}; max is never published"
        );
    }

    /// c-effortread r20 item 1 ruling B′ (EXIT mirror): with `{high, on}`
    /// published and B=max armed, a suppressed `… · Ultracode off` acceptance
    /// moves the latch off without publishing; the `ultra_effort_exit`
    /// attachment publishes `{high, off}` exactly once, and B's later Kept
    /// emits nothing stale.
    #[tokio::test]
    async fn r20_item1_exit_after_a_flag_moving_suppressed_accept_publishes_high_off_once() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        let instance_id = InstanceId::new();
        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            instance_id.clone(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        mapper
            .map_line_at(
                &json!({
                    "type": "assistant",
                    "uuid": "msg-high",
                    "sessionId": "effort-session",
                    "version": "2.1.289",
                    "message": {"id": "msg-high", "role": "assistant", "type": "message",
                                "content": [{"type": "text", "text": "on it"}]},
                    "effort": "high",
                    "perTurnEffort": null,
                })
                .to_string(),
                Some(400),
            )
            .expect("map baseline");
        mapper
            .map_line_at(&effort_slash_line("cmd-on", "ultracode on"), Some(430))
            .expect("map human ultracode-on slash");
        let baseline = mapper
            .map_line_at(
                &effort_stdout_line(
                    "out-on",
                    "Ultracode on (this session only): dynamic workflows on every task. Effort \
                     stays high.",
                ),
                Some(450),
            )
            .expect("map high/on accept");
        assert_eq!(
            r19_effort_edges(&baseline),
            vec![(Some(remuda_protocol::EffortName::High), Some(true), None)],
            "the published baseline is {{high, on}}"
        );

        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(SubmissionProvenance::Floor(1000)));
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "high"), Some(500))
            .expect("map old slash");
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let suppressed = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Set effort level to high · Ultracode off"),
                Some(1600),
            )
            .expect("map suppressed high/off acceptance");
        assert!(
            r19_effort_edges(&suppressed).is_empty(),
            "the old high/off acceptance stays suppressed"
        );

        let attachment = mapper
            .map_line_at(
                &r19_attachment_line("att-exit", "ultra_effort_exit"),
                Some(1700),
            )
            .expect("map exit attachment");
        assert_eq!(
            r19_effort_edges(&attachment),
            vec![(Some(remuda_protocol::EffortName::High), Some(false), None)],
            "the attachment publishes {{high, off}} once"
        );

        let dismissed = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Kept effort level as max"),
                Some(1800),
            )
            .expect("map B refusal");
        assert!(
            r19_effort_edges(&dismissed).is_empty(),
            "nothing stale publishes after B's Kept"
        );
        assert!(matches!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .expect("B recorded the rejection"),
            crate::effort::Readback::Rejected { .. }
        ));
        let suppressed_id = remuda_protocol::effort_record_event_id(
            instance_id.as_id().as_str(),
            remuda_protocol::EFFORT_STDOUT_NATIVE,
            "out-old",
            remuda_protocol::EffortName::High,
        );
        for out in [&suppressed, &dismissed] {
            assert!(
                out.iter().all(|obs| obs.event_id != suppressed_id),
                "the suppressed high/off stdout edge id is never emitted"
            );
        }
        let repeat = mapper
            .map_line_at(
                &r19_attachment_line("att-repeat", "ultra_effort_exit"),
                Some(1900),
            )
            .expect("map sparse repeat exit");
        assert!(
            r19_effort_edges(&repeat).is_empty(),
            "a sparse repeat exit dedups against the published tuple"
        );
    }

    /// c-effortread r16 item 3: when the carrier's floor lookup FAILS (the
    /// bound transcript was rotated/removed or the stat errors), production no
    /// longer falls back to word-only attribution. The switch is armed with
    /// explicit "provenance unavailable" and the mapper attributes NOTHING to
    /// it, so an unread older same-word slash with a delayed refusal — and even
    /// the switch's own read-back restored afterwards — can never settle B; the
    /// request ends through its own bounded no-readback/unconfirmed path.
    #[tokio::test]
    async fn r16_item3_a_failed_floor_lookup_attributes_nothing_to_the_request() {
        use crate::effort::{EffortBridge, EffortRequest, SubmissionProvenance};

        // Exercise the carrier's REAL lookup seam against a REAL file.
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("effort-session.jsonl");
        std::fs::write(&path, "a bound transcript the pump is following\n").expect("seed");
        let present = herdr_submission_provenance(Some(path.to_string_lossy().as_ref()));
        assert!(
            matches!(present, SubmissionProvenance::Floor(_)),
            "a successful stat is a verified floor"
        );
        // The bound file disappears (rotation/cleanup) immediately before the
        // switch would stat it: the lookup fails.
        std::fs::remove_file(&path).expect("rotate the transcript away");
        let provenance = herdr_submission_provenance(Some(path.to_string_lossy().as_ref()));
        assert!(
            matches!(provenance, SubmissionProvenance::Unavailable),
            "a failed stat is explicit provenance loss, never word-only"
        );

        let bridge = Arc::new(EffortBridge::new());
        let mut mapper = TranscriptMapper::new(
            DriverKind::ClaudePty,
            InstanceId::new(),
            RunId::new(),
            Id::new("obj").expect("journal id"),
            HostId::new(),
            "effort-session".into(),
            "2.1.289".into(),
        )
        .with_effort_bridge(Arc::clone(&bridge) as Arc<EffortBridge>, None);

        // B is armed THROUGH the production seam with the failed provenance.
        let request_b = EffortRequest::from_level("max")
            .expect("max")
            .with_request_id(Some("req-b"));
        let gen_b = bridge.arm_with_floor(request_b, Some(provenance));

        // The pump restores an unread OLD same-word slash plus its refusal.
        mapper
            .map_line_at(&effort_slash_line("cmd-old", "max"), Some(500))
            .expect("map old slash");
        let old_verdict = mapper
            .map_line_at(
                &effort_stdout_line("out-old", "Kept effort level as high"),
                Some(600),
            )
            .expect("map old refusal");
        assert!(
            old_verdict
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "the old refusal emits no request edge"
        );
        assert!(
            bridge.has_pending(),
            "the restored old refusal never rejects B"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "nothing resolves B"
        );

        // Even B's OWN post-submission slash and accepted verdict attribute
        // NOTHING while provenance is unavailable. c-effortread r18 item 1:
        // with the ruling keyed on the BRIDGE (a switch armed) rather than on
        // a later owned FIFO slot, B's own accepted verdict (its slot is
        // Unowned too) emits NO observation at all — no id-less terminal edge
        // the store could settle B from through its value fallback — and the
        // bridge's observed selection is not refreshed. B stays pending and
        // ends through its own bounded timeout/unconfirmed path.
        mapper
            .map_line_at(&effort_slash_line("cmd-b", "max"), Some(1500))
            .expect("map B slash");
        let out_b = mapper
            .map_line_at(
                &effort_stdout_line("out-b", "Set effort level to max"),
                Some(1600),
            )
            .expect("map B verdict");
        assert!(
            out_b
                .iter()
                .all(|obs| !matches!(obs.body, ObservationPayload::Effort(_))),
            "B's own accepted verdict emits no projection while armed under provenance loss"
        );
        assert!(bridge.has_pending(), "B is not settled by any read-back");
        assert!(
            bridge.proven_selection().is_none(),
            "the suppressed verdict never refreshes the bridge's observed selection"
        );
        assert!(
            bridge
                .wait(gen_b, std::time::Duration::from_millis(40))
                .await
                .is_none(),
            "B ends through its own timeout/unconfirmed path with no stray settlement"
        );
    }

    /// c-r3 item 4: the claude-pty transcript pump driven through its REAL
    /// output channel (the spawned pump task + mpsc receiver), not by calling
    /// the emit helper on a hand-built observation. An effort edge read off the
    /// channel carries the mapper/journal-derived deterministic event id.
    #[tokio::test]
    async fn the_transcript_pump_publishes_effort_ids_on_its_real_channel() {
        let dir = tempfile::tempdir().expect("tmp");
        let transcript = dir.path().join("effort-session.jsonl");
        std::fs::write(
            &transcript,
            serde_json::to_string(&json!({
                "type": "assistant",
                "uuid": "msg-pump",
                "sessionId": "effort-session",
                "version": "2.1.289",
                "message": {
                    "id": "msg-pump",
                    "role": "assistant",
                    "type": "message",
                    "content": [{"type": "text", "text": "ok"}],
                },
                "effort": "high",
                "perTurnEffort": null,
            }))
            .expect("json")
                + "\n",
        )
        .expect("write transcript");

        let instance = InstanceId::new();
        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: instance.clone(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").expect("journal id"),
            run_id: RunId::new(),
            session_id: "effort-session".into(),
            pin_version: "2.1.289".into(),
        };
        // The id the journal tailer derives for the same record — the pump's
        // real output must match it.
        // The mapper prefixes the bare message id internally as
        // `effort:<message-id>:<name>`, so the journal derivation takes the id.
        let expected = remuda_protocol::effort_event_id(
            instance.as_id().as_str(),
            "msg-pump",
            remuda_protocol::EffortName::High,
        );

        let (tx, mut rx) = mpsc::channel(16);
        let seq = Arc::new(AtomicU64::new(0));
        let slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(Some(
            transcript.to_string_lossy().into_owned(),
        )));
        // A live (new-session) boundary: the pump reads from byte 0 as current.
        let task = spawn_transcript_pump(
            slot,
            tx,
            ctx,
            seq,
            None,
            None,
            None,
            None,
            None,
            None,
            crate::claude_transcript::ResumeMode::Fresh,
        );

        // Drain the real channel until the effort edge arrives.
        let mut found: Option<Observation> = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(obs)) if matches!(obs.body, ObservationPayload::Effort(_)) => {
                    found = Some(obs);
                    break;
                }
                Ok(Some(_)) | Err(_) => continue,
                Ok(None) => break,
            }
        }
        task.abort();
        let effort = found.expect("the real pump published an effort observation");
        assert_eq!(
            effort.event_id, expected,
            "the pump's real channel carries the journal-derived deterministic id"
        );
        let ObservationPayload::Effort(payload) = effort.body else {
            panic!("effort payload");
        };
        assert_eq!(
            payload.effective.name,
            Some(remuda_protocol::EffortName::High)
        );
    }

    /// c-effortread r4 item 5, claude-pty pump: a VERIFIED resume whose bound
    /// transcript is replaced emits the explicit read-back-unavailable edge
    /// (clearing the projected `high`) on the pump's REAL channel, and a
    /// pending Remuda switch is never settled Applied by a verdict that lands
    /// after provenance is lost.
    #[tokio::test]
    async fn the_transcript_pump_clears_effective_state_when_provenance_is_lost() {
        use crate::claude_transcript::{ResumeBoundary, ResumeMode};
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tmp");
        let transcript = dir.path().join("effort-session.jsonl");
        let history = "{\"type\":\"user\",\"sessionId\":\"effort-session\"}\n";
        std::fs::write(&transcript, history).expect("history");
        let boundary = ResumeBoundary::snapshot(&transcript).expect("pre-spawn EOF");

        let instance = InstanceId::new();
        let ctx = ObsCtx {
            driver: DriverKind::ClaudePty,
            instance_id: instance.clone(),
            host_id: HostId::new(),
            journal_id: Id::new("obj").expect("journal id"),
            run_id: RunId::new(),
            session_id: "effort-session".into(),
            pin_version: "2.1.289".into(),
        };
        let bridge = Arc::new(crate::effort::EffortBridge::new());
        let (tx, rx) = mpsc::channel(64);
        let seq = Arc::new(AtomicU64::new(0));
        let slot: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(Some(
            transcript.to_string_lossy().into_owned(),
        )));
        let task = spawn_transcript_pump(
            slot,
            tx,
            ctx,
            seq,
            Some(Arc::clone(&bridge)),
            None,
            None,
            None,
            None,
            None,
            ResumeMode::Boundary(boundary),
        );
        let mut rx = rx;

        let append = |line: serde_json::Value| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .expect("open");
            writeln!(file, "{line}").expect("append");
        };
        let effort_names = |obs: &[Observation]| {
            obs.iter()
                .filter_map(|o| match &o.body {
                    ObservationPayload::Effort(payload) => {
                        Some((payload.effective.name, payload.effective.readback_available))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        // A current-process high edge after the verified boundary.
        append(json!({
            "type": "assistant",
            "uuid": "msg-1",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "id": "msg-1",
                "role": "assistant",
                "type": "message",
                "content": [{"type": "text", "text": "ok"}],
            },
            "effort": "high",
            "perTurnEffort": null,
        }));
        let obs = drain_for(&mut rx, Duration::from_millis(800)).await;
        assert_eq!(
            effort_names(&obs),
            vec![(Some(remuda_protocol::EffortName::High), None)]
        );

        // A Remuda switch is armed; its slash record is a current record.
        let generation = bridge.arm(crate::effort::EffortRequest::from_level("max").expect("max"));
        append(json!({
            "type": "user",
            "uuid": "cmd-2",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "role": "user",
                "content": "<command-name>/effort</command-name>\n<command-message>effort</command-message>\n<command-args>max</command-args>",
            },
        }));
        let _ = drain_for(&mut rx, Duration::from_millis(400)).await;
        assert!(bridge.has_pending());

        // The transcript is replaced by something shorter: boundary lost.
        std::fs::write(&transcript, "{\"rotated\":true}\n").expect("rotate");
        let obs = drain_for(&mut rx, Duration::from_millis(800)).await;
        assert_eq!(
            effort_names(&obs),
            vec![(None, Some(false))],
            "the pump publishes one read-back-unavailable edge: {obs:?}"
        );
        assert!(bridge.has_pending());

        // The max verdict in the recreated file cannot settle it.
        append(json!({
            "type": "user",
            "uuid": "cmd-3",
            "sessionId": "effort-session",
            "version": "2.1.289",
            "message": {
                "role": "user",
                "content": "<local-command-stdout>Set effort level to max</local-command-stdout>",
            },
        }));
        let obs = drain_for(&mut rx, Duration::from_millis(800)).await;
        assert!(
            effort_names(&obs).is_empty(),
            "no edge from an unverified verdict: {obs:?}"
        );
        assert!(bridge.has_pending(), "the switch is never resolved Applied");
        let settled = bridge.wait(generation, Duration::from_millis(150)).await;
        assert!(settled.is_none());

        task.abort();
    }

    /// Collect everything the real pump emits within `window`.
    async fn drain_for(rx: &mut mpsc::Receiver<Observation>, window: Duration) -> Vec<Observation> {
        let deadline = tokio::time::Instant::now() + window;
        let mut out = Vec::new();
        while let Ok(Some(observation)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            out.push(observation);
        }
        out
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
