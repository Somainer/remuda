//! Feishu process composition. The adapter owns routing and the Hub client owns HTTP.

use std::{
    collections::BTreeSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, bail};
use clap::Args as ClapArgs;
use remuda_feishu::{
    ConsumeEvent, ConsumeSettings, ConsumeSupervisor, Dispatcher, HubInstanceApi, InboundPolicy,
    InstanceApi, LarkCli, RouteDefaults,
};
use remuda_hub_client::HubClient;
use tokio::sync::{mpsc, watch};

use crate::{
    Shutdown,
    config::{Config, Dispatcher as Settings, DispatcherOutbound, SecretRef},
};

#[derive(ClapArgs, Default)]
#[command(about = "Run the Feishu dispatcher against a Hub using a dedicated lark-cli app.")]
pub(crate) struct Args {
    /// Hub HTTP base URL; overrides dispatcher.hub_url.
    #[arg(long)]
    hub_url: Option<String>,
    /// Dedicated dispatcher app profile for both consume and outbound calls.
    #[arg(long)]
    profile: Option<String>,
    /// lark-cli executable name or path.
    #[arg(long)]
    lark_cli: Option<PathBuf>,
    /// Persistent topic-to-instance SQLite map.
    #[arg(long)]
    session_db: Option<PathBuf>,
    /// File holding a Hub device bearer token. The bootstrap token is not accepted
    /// for this role — mint a scoped, revocable device token instead.
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Allowed owner open ID; repeated values replace the configured owner list.
    #[arg(long = "owner-open-id")]
    owners: Vec<String>,
    /// Chat allowlist; repeated values replace the configured chat list.
    #[arg(long = "chat")]
    chats: Vec<String>,
    /// dry-run records outbound argv; live explicitly enables sending through lark-cli.
    #[arg(long, value_enum)]
    outbound: Option<DispatcherOutbound>,
}

impl Args {
    pub(crate) fn apply(self, config: &mut Config) -> anyhow::Result<()> {
        let settings = config.dispatcher.get_or_insert_with(Settings::default);
        if let Some(url) = self.hub_url {
            settings.hub_url = url;
        }
        if let Some(profile) = self.profile {
            settings.profile = Some(profile);
        }
        if let Some(binary) = self.lark_cli {
            settings.lark_cli = binary;
        }
        if let Some(path) = self.session_db {
            settings.session_db = Some(path);
        }
        if let Some(path) = self.token_file {
            settings.token = Some(SecretRef::File(path));
            settings.bootstrap_token = None;
        }
        if !self.owners.is_empty() {
            settings.owner_open_ids = self.owners;
        }
        if !self.chats.is_empty() {
            settings.chat_allowlist = self.chats;
        }
        if let Some(mode) = self.outbound {
            settings.outbound = mode;
        }
        config.validate()
    }
}

pub(crate) async fn run(
    mut config: Config,
    args: Args,
    mut shutdown: Shutdown,
) -> anyhow::Result<()> {
    args.apply(&mut config)?;
    run_configured(&config, None, shutdown.wait()).await
}

/// The combined mode authenticates against its actual listener, including port zero.
pub(crate) async fn run_configured(
    config: &Config,
    local_hub: Option<&remuda_hub::RunningHub>,
    stop: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let settings = config
        .dispatcher
        .as_ref()
        .context("configure [dispatcher] before starting the dispatcher")?;
    settings.validate()?;
    let consume = consume_settings(settings)?;
    let client = match local_hub {
        // Combined mode: mint a scoped device token in-process (D-018). The
        // bootstrap access code pairs devices and does not authenticate the
        // API, so the dispatcher holds a real revocable token — matching F13's
        // rule for the standalone path rather than making an exception to it.
        Some(hub) => {
            let token = hub
                .mint_device_token("remuda-dispatcher")
                .await
                .context("mint dispatcher device token")?;
            HubClient::new(local_hub_url(hub.addr), Some(token), None)?
        }
        None => {
            // F13: no bootstrap fallback. `Settings::validate` already rejects
            // `bootstrap_token`; this is the matching refusal at the point of use.
            let Some(reference) = &settings.token else {
                bail!(
                    "dispatcher requires dispatcher.token — a scoped Hub device token \
                     (env:NAME or file:PATH). The bootstrap token is not accepted for this role."
                );
            };
            HubClient::new(
                settings.hub_url.clone(),
                Some(reference.resolve()?.into_string()),
                None,
            )?
        }
    };
    tokio::pin!(stop);
    tokio::select! {
        biased;
        result = &mut stop => return result,
        result = client.list_hosts() => { result.context("dispatcher cannot authenticate with the Hub")?; }
    }
    let dispatcher = open_dispatcher(settings, &config.data_dir, HubInstanceApi::new(client))?;
    tracing::info!(outbound = ?settings.outbound, "remuda dispatcher starting");
    supervise(
        dispatcher,
        consume,
        Duration::from_secs(settings.startup_timeout_secs),
        config.shutdown_timeout(),
        Duration::from_millis(settings.follow_interval_ms),
        stop,
    )
    .await?;
    Ok(())
}

fn local_hub_url(mut addr: SocketAddr) -> String {
    if addr.ip().is_unspecified() {
        addr.set_ip(match addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        });
    }
    format!("http://{addr}")
}

fn consume_settings(settings: &Settings) -> anyhow::Result<ConsumeSettings> {
    let mut consume = ConsumeSettings::new(resolve_executable(&settings.lark_cli)?);
    consume.profile = settings.profile.clone();
    consume.backoff.initial = Duration::from_millis(settings.restart_initial_ms);
    consume.backoff.max = Duration::from_millis(settings.restart_max_ms);
    consume.line_max_bytes = settings.line_max_bytes;
    Ok(consume)
}

fn resolve_executable(path: &Path) -> anyhow::Result<PathBuf> {
    let candidates = if path.components().count() == 1 {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(path))
            .collect::<Vec<_>>()
    } else {
        vec![path.to_owned()]
    };
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if candidate.metadata()?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return std::fs::canonicalize(candidate)
            .context("cannot resolve dispatcher lark-cli executable");
    }
    bail!("dispatcher.lark_cli must name an executable file on PATH or an executable path")
}

fn open_dispatcher<A: InstanceApi>(
    settings: &Settings,
    data_dir: &Path,
    api: A,
) -> anyhow::Result<Dispatcher<A>> {
    let outbound = match settings.outbound {
        DispatcherOutbound::DryRun => LarkCli::dry_run(),
        // Selecting live in config/CLI is the outbound facade's explicit opt-in.
        DispatcherOutbound::Live => LarkCli::live(resolve_executable(&settings.lark_cli)?),
    }
    .with_profile(
        settings
            .profile
            .as_deref()
            .context("missing dispatcher profile")?,
    )
    .with_timeout(Duration::from_secs(settings.outbound_timeout_secs));
    let policy = InboundPolicy {
        owner_open_ids: settings.owner_open_ids.clone(),
        chat_allowlist: settings.chat_allowlist.clone(),
        bot_open_id: settings.bot_open_id.clone(),
        bot_name: settings.bot_name.clone(),
        allow_unaddressed: settings.allow_unaddressed,
    };
    let defaults = RouteDefaults {
        host: settings.host.clone(),
        agent: settings.agent,
        model: settings.model.clone(),
    };
    let path = settings
        .session_db
        .clone()
        .unwrap_or_else(|| data_dir.join("dispatcher/sessions.sqlite"));
    Ok(Dispatcher::open(path, api, outbound, policy, defaults)?)
}

/// Stop accepting events, terminate consume children, and drain accepted events.
async fn supervise<A: InstanceApi + 'static>(
    dispatcher: Dispatcher<A>,
    consume: ConsumeSettings,
    startup_timeout: Duration,
    shutdown_timeout: Duration,
    follow_interval: Duration,
    stop: impl Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<Dispatcher<A>> {
    let keys = consume.event_keys.iter().cloned().collect();
    let (supervisor, events) = ConsumeSupervisor::start(consume);
    let (stopping, stopped) = watch::channel(false);
    let mut worker = tokio::spawn(drive(
        dispatcher,
        events,
        stopped,
        keys,
        startup_timeout,
        follow_interval,
    ));
    tokio::pin!(stop);
    let reason = tokio::select! {
        biased;
        reason = &mut stop => reason,
        result = &mut worker => {
            supervisor.shutdown().await;
            return result.context("dispatcher worker failed")?;
        }
    };
    let _ = stopping.send(true);
    let mut completed = None;
    let drained = tokio::time::timeout(shutdown_timeout, async {
        tokio::join!(supervisor.shutdown(), async {
            completed = Some((&mut worker).await);
        });
    })
    .await;
    if drained.is_err() {
        if completed.is_none() {
            worker.abort();
            let _ = worker.await;
        }
        bail!(
            "dispatcher shutdown deadline exceeded; an in-flight Hub operation may have completed and was not replayed"
        );
    }
    let result = completed
        .context("dispatcher worker was not drained")?
        .context("dispatcher worker failed")?;
    reason?;
    result
}

async fn drive<A: InstanceApi>(
    mut dispatcher: Dispatcher<A>,
    mut events: mpsc::Receiver<ConsumeEvent>,
    mut stopped: watch::Receiver<bool>,
    mut waiting: BTreeSet<String>,
    startup_timeout: Duration,
    follow_interval: Duration,
) -> anyhow::Result<Dispatcher<A>> {
    let deadline = tokio::time::sleep(startup_timeout);
    tokio::pin!(deadline);
    let mut follow = tokio::time::interval(follow_interval);
    follow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut draining = false;
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed(), if !draining => {
                draining = true;
                events.close();
            }
            _ = &mut deadline, if !draining && !waiting.is_empty() => {
                bail!("dispatcher consume startup timed out before both subscriptions became ready");
            }
            _ = follow.tick(), if !draining && waiting.is_empty() => {
                // TODO(remuda-feishu): follow_live does not expire idle interaction tickets.
                let reports = dispatcher.follow_live(SystemTime::now()).await?;
                tracing::debug!(actions = reports.len(), "dispatcher follow polled");
            }
            event = events.recv() => {
                let Some(event) = event else {
                    if draining { return Ok(dispatcher); }
                    bail!("dispatcher consume supervisor stopped unexpectedly");
                };
                match event {
                    ConsumeEvent::Ready { event_key } => {
                        waiting.remove(&event_key);
                        if waiting.is_empty() { tracing::info!("remuda dispatcher ready"); }
                    }
                    ConsumeEvent::Event { .. } => {
                        // Use current time for each dispatch; drive_consume takes one timestamp.
                        let reports = dispatcher.handle_consume(event, SystemTime::now()).await?;
                        tracing::debug!(actions = reports.len(), "dispatcher inbound handled");
                    }
                    ConsumeEvent::BadLine { event_key, .. } => tracing::warn!(%event_key, "dispatcher ignored invalid consume line"),
                    ConsumeEvent::Exited { event_key, status } => tracing::warn!(%event_key, ?status, "dispatcher consume exited"),
                    ConsumeEvent::Restarting { .. } => {} // ConsumeSupervisor logs and owns restart/backoff.
                }
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use remuda_feishu::{
        ApiCall, CreateRequest, CreatedInstance, Error, FakeInstanceApi, FollowEvent, FollowPage,
        RespondRequest, SendRequest, SessionStore,
    };
    use remuda_protocol::InstanceId;
    use std::sync::Arc;
    use tokio::sync::Notify;

    const KEY: &str = "feishu:oc_dispatcher_fixture:main";

    struct GatedApi {
        inner: FakeInstanceApi,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        followed: Arc<Notify>,
    }

    impl InstanceApi for GatedApi {
        async fn create(&self, request: CreateRequest) -> Result<CreatedInstance, Error> {
            self.entered.notify_one();
            self.release.notified().await;
            self.inner.create(request).await
        }
        async fn send(&self, request: SendRequest) -> Result<(), Error> {
            self.inner.send(request).await
        }
        async fn cancel(&self, id: &InstanceId) -> Result<(), Error> {
            self.inner.cancel(id).await
        }
        async fn respond(&self, request: RespondRequest) -> Result<(), Error> {
            self.inner.respond(request).await
        }
        async fn follow(&self, id: &InstanceId, seq: u64) -> Result<FollowPage, Error> {
            let page = self.inner.follow(id, seq).await;
            self.followed.notify_one();
            page
        }
    }

    fn settings() -> Settings {
        Settings {
            profile: Some("dispatcher-test".into()),
            owner_open_ids: vec!["ou_dispatcher_owner".into()],
            lark_cli: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-dispatcher-lark.py"),
            restart_initial_ms: 10,
            restart_max_ms: 20,
            ..Settings::default()
        }
    }

    fn consume(settings: &Settings, dir: &Path) -> ConsumeSettings {
        let mut consume = consume_settings(settings).expect("executable settings");
        consume
            .extra_env
            .push(("REMUDA_TEST_LARK_DIR".into(), dir.display().to_string()));
        consume
    }

    async fn wait_for(path: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("test process evidence");
    }

    fn assert_terminated(dir: &Path) {
        for key in [
            remuda_feishu::EVENT_IM_RECEIVE,
            remuda_feishu::EVENT_CARD_ACTION,
        ] {
            assert_eq!(
                std::fs::read_to_string(dir.join(format!("{key}.stopped"))).expect("child reaped"),
                "SIGTERM"
            );
        }
        assert!(
            !dir.join("unexpected-outbound").exists(),
            "DryRun must never invoke lark-cli outbound"
        );
    }

    #[tokio::test]
    async fn recorded_inbound_restarts_consume_and_drains_accepted_dispatches() {
        let dir = tempfile::tempdir().expect("directory");
        let settings = settings();
        let mut consume = consume(&settings, dir.path());
        consume
            .extra_env
            .push(("REMUDA_TEST_LARK_FAIL_ONCE".into(), "1".into()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let api = GatedApi {
            inner: FakeInstanceApi::default(),
            entered: entered.clone(),
            release: release.clone(),
            followed: Arc::new(Notify::new()),
        };
        let id = InstanceId::new();
        api.inner.set_next_id(id.clone());
        api.inner.set_follow(
            &id,
            FollowPage {
                events: vec![FollowEvent::Completed {
                    conclusion: "fixture finished".into(),
                    ok: true,
                }],
                next_seq: 1,
            },
        );
        let dispatcher = open_dispatcher(&settings, dir.path(), api).expect("dispatcher");
        let stopped_dir = dir.path().to_owned();
        let stop = async move {
            entered.notified().await;
            // Both subscriptions have restarted before requesting shutdown.
            for key in [
                remuda_feishu::EVENT_IM_RECEIVE,
                remuda_feishu::EVENT_CARD_ACTION,
            ] {
                let path = stopped_dir.join(format!("{key}.starts"));
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if std::fs::read_to_string(&path).ok().as_deref() == Some("2") {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("consume restarted");
            }
            tokio::spawn(async move {
                wait_for(&stopped_dir.join("im.message.receive_v1.stopped")).await;
                wait_for(&stopped_dir.join("card.action.trigger.stopped")).await;
                release.notify_one();
            });
            Ok(())
        };
        let dispatcher = supervise(
            dispatcher,
            consume,
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_millis(20),
            stop,
        )
        .await
        .expect("graceful drain");
        let calls = dispatcher.api().inner.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, ApiCall::Create(_)))
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|c| matches!(c, ApiCall::Send(_)))
                .count(),
            1,
            "the accepted next prompt must drain before shutdown"
        );
        assert_eq!(
            dispatcher.outbound().mode(),
            remuda_feishu::ExecutionMode::DryRun
        );
        assert!(
            dispatcher
                .outbound()
                .recorded()
                .iter()
                .any(|c| c.argv.iter().any(|arg| arg.contains("fixture finished")))
        );
        let path = dir.path().join("dispatcher/sessions.sqlite");
        drop(dispatcher);
        let store = SessionStore::open(path).expect("reopen durable map");
        assert_eq!(
            store
                .get(KEY)
                .expect("lookup")
                .expect("binding")
                .instance_id,
            Some(id)
        );
        assert_terminated(dir.path());
    }

    #[tokio::test]
    async fn missing_ready_marker_fails_startup_and_terminates_consume() {
        let dir = tempfile::tempdir().expect("directory");
        let settings = settings();
        let mut consume = consume(&settings, dir.path());
        consume
            .extra_env
            .push(("REMUDA_TEST_LARK_UNREADY".into(), "1".into()));
        let dispatcher =
            open_dispatcher(&settings, dir.path(), FakeInstanceApi::default()).expect("dispatcher");
        let startup_timeout = Duration::from_secs(60);
        let task = tokio::spawn(supervise(
            dispatcher,
            consume,
            startup_timeout,
            Duration::from_secs(5),
            Duration::from_secs(1),
            std::future::pending(),
        ));
        // Process startup uses wall time; expire the deadline only after both
        // fixtures have installed their signal handlers, without sending ready.
        for key in [
            remuda_feishu::EVENT_IM_RECEIVE,
            remuda_feishu::EVENT_CARD_ACTION,
        ] {
            wait_for(&dir.path().join(format!("{key}.starts"))).await;
        }
        assert!(
            !task.is_finished(),
            "consume must still be waiting for ready"
        );
        tokio::time::pause();
        tokio::time::advance(startup_timeout).await;
        tokio::time::resume();
        let error = task
            .await
            .expect("supervisor task")
            .err()
            .expect("startup failure");
        assert!(error.to_string().contains("startup timed out"));
        assert_terminated(dir.path());
    }

    #[tokio::test]
    async fn idle_journal_poll_posts_a_dry_run_card_without_new_inbound() {
        let dir = tempfile::tempdir().expect("directory");
        let followed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        release.notify_one();
        let api = GatedApi {
            inner: FakeInstanceApi::default(),
            entered: Arc::new(Notify::new()),
            release,
            followed: followed.clone(),
        };
        let mut dispatcher = open_dispatcher(&settings(), dir.path(), api).expect("dispatcher");
        let line = include_str!("../../tests/fixtures/dispatcher-inbound.jsonl")
            .lines()
            .find(|line| line.contains("om_dispatcher_prompt"))
            .expect("recorded prompt");
        dispatcher
            .handle_consume(
                ConsumeEvent::Event {
                    event_key: remuda_feishu::EVENT_IM_RECEIVE.into(),
                    event: Box::new(remuda_feishu::parse_event_line(line).expect("recording")),
                },
                SystemTime::now(),
            )
            .await
            .expect("create topic instance");
        followed.notified().await;
        let id = dispatcher
            .sessions()
            .get(KEY)
            .expect("lookup")
            .expect("binding")
            .instance_id
            .expect("instance");
        dispatcher.api().inner.set_follow(
            &id,
            FollowPage {
                events: vec![FollowEvent::ToolBoundary {
                    name: "Read".into(),
                    summary: "idle poll README.md".into(),
                    elapsed_secs: 1,
                }],
                next_seq: 1,
            },
        );
        let (_inbound, events) = mpsc::channel(1);
        let (stop, stopped) = watch::channel(false);
        let stop_task = tokio::spawn(async move {
            followed.notified().await;
            stop.send(true).expect("stop after idle poll");
        });
        let dispatcher = tokio::time::timeout(
            Duration::from_secs(5),
            drive(
                dispatcher,
                events,
                stopped,
                BTreeSet::new(),
                Duration::from_secs(5),
                Duration::from_millis(10),
            ),
        )
        .await
        .expect("idle follow deadline")
        .expect("drive");
        stop_task.await.expect("stop task");
        assert!(dispatcher.outbound().recorded().iter().any(|command| {
            command
                .argv
                .iter()
                .any(|arg| arg.contains("idle poll README.md"))
        }));
    }

    #[tokio::test]
    async fn shutdown_deadline_stops_a_blocked_dispatch_without_replay() {
        let dir = tempfile::tempdir().expect("directory");
        let settings = settings();
        let consume = consume(&settings, dir.path());
        let entered = Arc::new(Notify::new());
        let api = GatedApi {
            inner: FakeInstanceApi::default(),
            entered: entered.clone(),
            release: Arc::new(Notify::new()),
            followed: Arc::new(Notify::new()),
        };
        let dispatcher = open_dispatcher(&settings, dir.path(), api).expect("dispatcher");
        let error = supervise(
            dispatcher,
            consume,
            Duration::from_secs(5),
            Duration::from_millis(100),
            Duration::from_secs(1),
            async {
                entered.notified().await;
                wait_for(&dir.path().join("card.action.trigger.starts")).await;
                Ok(())
            },
        )
        .await
        .err()
        .expect("bounded drain");
        assert!(error.to_string().contains("shutdown deadline exceeded"));
        assert!(error.to_string().contains("not replayed"));
        for key in [
            remuda_feishu::EVENT_IM_RECEIVE,
            remuda_feishu::EVENT_CARD_ACTION,
        ] {
            wait_for(&dir.path().join(format!("{key}.stopped"))).await;
        }
        assert_terminated(dir.path());
    }

    #[test]
    fn combined_hub_client_uses_an_actual_connectable_listener() {
        assert_eq!(
            local_hub_url("0.0.0.0:54321".parse().expect("address")),
            "http://127.0.0.1:54321"
        );
        assert_eq!(
            local_hub_url("[::]:54321".parse().expect("address")),
            "http://[::1]:54321"
        );
        assert_eq!(
            local_hub_url("127.0.0.1:54321".parse().expect("address")),
            "http://127.0.0.1:54321"
        );
    }
}

impl super::registry::Entrypoint for Args {
    fn enter(self, context: super::registry::Context) -> anyhow::Result<i32> {
        super::registry::service(context, |config, shutdown| run(config, self, shutdown))
    }
}
