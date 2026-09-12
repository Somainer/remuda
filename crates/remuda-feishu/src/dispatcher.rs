//! Dispatcher runtime: session_key → instanceId, command routing, tickets, throttle.
//!
//! This is a channel adapter, not an agent loop. Hub HTTP is
//! [`crate::HubInstanceApi`]; this crate does not depend on `crates/remuda`.

use std::collections::BTreeMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use remuda_protocol::{AgentKind, InstanceId, Interaction, InteractionAnswer, InteractionId, U64};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::cards::{render_completion_card, render_progress_card};
use crate::consume::ConsumeEvent;
use crate::error::Error;
use crate::inbound::{
    CallbackValue, Deduper, ExplicitCommand, GateDecision, Inbound, InboundKind, InboundPolicy,
    Intent, SessionKey, admit,
};
use crate::outbound::{LarkCli, OutboundBody};
use crate::tickets::{
    AnswerScope, MappedAnswer, ShortcutMiss, ShortcutRef, TicketStore, request_title,
};

/// Minimum gap between static progress cards (tool-boundary updates only).
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// SQLite `session_key` → instance map (0600 when on disk).
pub struct SessionStore {
    conn: Mutex<Connection>,
}

/// One topic's routing row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    /// `feishu:{chat_id}:{thread||root||main}`.
    pub session_key: String,
    /// Live or last instance, if created.
    pub instance_id: Option<InstanceId>,
    /// Pinned host label (`/host`).
    pub host: String,
    /// Pinned agent (`/agent`).
    pub agent: AgentKind,
    /// Pinned model (`/model`).
    pub model: Option<String>,
    /// Mapping lifecycle.
    pub status: SessionStatus,
    /// Chat used for follow-up cards.
    pub chat_id: String,
    /// Last IM/card message id (reply target).
    pub last_message_id: Option<String>,
    /// Reply in topic when the session suffix is not `main`.
    pub in_thread: bool,
    /// Last consumed follow sequence.
    pub follow_seq: u64,
    /// Unix ms of the last progress card.
    pub last_progress_ms: u64,
}

/// Whether the mapped instance should be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Route pins only; next prompt creates.
    Idle,
    /// Instance exists and should receive `/resume` (send).
    Live,
    /// `/stop` or `/new` retired the instance.
    Stopped,
}

impl SessionStore {
    /// Open or create `path` (parent `0700`, file `0600`).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| Error::SessionStore(err.to_string()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        let conn = Connection::open(&path).map_err(store_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init()?;
        Ok(store)
    }

    /// Process-local in-memory map (tests).
    pub fn memory() -> Result<Self, Error> {
        let store = Self {
            conn: Mutex::new(Connection::open_in_memory().map_err(store_err)?),
        };
        store.init()?;
        Ok(store)
    }

    fn init(&self) -> Result<(), Error> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_map (
                session_key TEXT PRIMARY KEY,
                instance_id TEXT,
                host TEXT NOT NULL DEFAULT '',
                agent TEXT NOT NULL DEFAULT 'claude',
                model TEXT,
                status TEXT NOT NULL DEFAULT 'idle',
                chat_id TEXT NOT NULL DEFAULT '',
                last_message_id TEXT,
                in_thread INTEGER NOT NULL DEFAULT 0,
                follow_seq INTEGER NOT NULL DEFAULT 0,
                last_progress_ms INTEGER NOT NULL DEFAULT 0
            );",
        )
        .map_err(store_err)?;
        Ok(())
    }

    /// All mapped session keys.
    pub fn list_keys(&self) -> Result<Vec<String>, Error> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT session_key FROM session_map ORDER BY session_key")
            .map_err(store_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(store_err)?;
        let mut keys = Vec::new();
        for key in rows {
            keys.push(key.map_err(store_err)?);
        }
        Ok(keys)
    }

    /// Lookup one topic.
    pub fn get(&self, session_key: &str) -> Result<Option<SessionBinding>, Error> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT session_key, instance_id, host, agent, model, status, chat_id,
                    last_message_id, in_thread, follow_seq, last_progress_ms
             FROM session_map WHERE session_key = ?1",
            params![session_key],
            row_to_binding,
        )
        .optional()
        .map_err(store_err)
    }

    /// Insert or replace a row.
    pub fn put(&self, binding: &SessionBinding) -> Result<(), Error> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO session_map (
                session_key, instance_id, host, agent, model, status, chat_id,
                last_message_id, in_thread, follow_seq, last_progress_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(session_key) DO UPDATE SET
                instance_id = excluded.instance_id,
                host = excluded.host,
                agent = excluded.agent,
                model = excluded.model,
                status = excluded.status,
                chat_id = excluded.chat_id,
                last_message_id = excluded.last_message_id,
                in_thread = excluded.in_thread,
                follow_seq = excluded.follow_seq,
                last_progress_ms = excluded.last_progress_ms",
            params![
                binding.session_key,
                binding
                    .instance_id
                    .as_ref()
                    .map(|id| id.as_id().as_str().to_string()),
                binding.host,
                agent_wire(binding.agent),
                binding.model,
                status_wire(binding.status),
                binding.chat_id,
                binding.last_message_id,
                i64::from(binding.in_thread),
                binding.follow_seq as i64,
                binding.last_progress_ms as i64,
            ],
        )
        .map_err(store_err)?;
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, Error> {
        Ok(self.conn.lock().unwrap_or_else(|err| err.into_inner()))
    }
}

fn row_to_binding(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionBinding> {
    let instance_raw: Option<String> = row.get(1)?;
    let instance_id = match instance_raw.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => Some(InstanceId::try_from(raw.to_string()).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(err))
        })?),
        None => None,
    };
    let agent: String = row.get(3)?;
    let status: String = row.get(5)?;
    Ok(SessionBinding {
        session_key: row.get(0)?,
        instance_id,
        host: row.get(2)?,
        agent: agent_from_wire(&agent),
        model: row.get(4)?,
        status: status_from_wire(&status),
        chat_id: row.get(6)?,
        last_message_id: row.get(7)?,
        in_thread: row.get::<_, i64>(8)? != 0,
        follow_seq: row.get::<_, i64>(9)? as u64,
        last_progress_ms: row.get::<_, i64>(10)? as u64,
    })
}

fn store_err(err: rusqlite::Error) -> Error {
    Error::SessionStore(err.to_string())
}

/// Defaults applied when a session has no `/host` `/agent` `/model` pins yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteDefaults {
    /// Host label forwarded to [`InstanceApi::create`].
    pub host: String,
    /// Agent kind forwarded to create.
    pub agent: AgentKind,
    /// Optional gateway model id.
    pub model: Option<String>,
}

impl Default for RouteDefaults {
    fn default() -> Self {
        Self {
            host: String::new(),
            agent: AgentKind::Claude,
            model: None,
        }
    }
}

/// Hub/runtime surface the dispatcher talks to. Implemented later by
/// `crates/remuda/src/cmd/hub_client.rs` — do not take a dependency on it here.
pub trait InstanceApi: Send + Sync {
    /// Create an instance for a new topic (or after `/new`).
    fn create(
        &self,
        req: CreateRequest,
    ) -> impl Future<Output = Result<CreatedInstance, Error>> + Send;
    /// Append a prompt to a live instance.
    fn send(&self, req: SendRequest) -> impl Future<Output = Result<(), Error>> + Send;
    /// `/stop` or `/new` retiring a live run.
    fn cancel(&self, instance_id: &InstanceId) -> impl Future<Output = Result<(), Error>> + Send;
    /// Card or `/yes` `/no` answer.
    fn respond(&self, req: RespondRequest) -> impl Future<Output = Result<(), Error>> + Send;
    /// Pull journal/follow events after `after_seq`.
    fn follow(
        &self,
        instance_id: &InstanceId,
        after_seq: u64,
    ) -> impl Future<Output = Result<FollowPage, Error>> + Send;
}

/// Arguments for [`InstanceApi::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    /// Topic key.
    pub session_key: SessionKey,
    /// Pinned host label.
    pub host: String,
    /// Pinned agent.
    pub agent: AgentKind,
    /// Pinned model.
    pub model: Option<String>,
    /// First user prompt.
    pub prompt: String,
    /// IM `message_id` (durable inbox).
    pub idempotency_key: String,
}

/// Result of [`InstanceApi::create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedInstance {
    /// New instance id (`ins_…`).
    pub instance_id: InstanceId,
}

/// Arguments for [`InstanceApi::send`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRequest {
    /// Live instance.
    pub instance_id: InstanceId,
    /// Additional prompt.
    pub prompt: String,
    /// IM `message_id`.
    pub idempotency_key: String,
}

/// Arguments for [`InstanceApi::respond`].
#[derive(Debug, Clone)]
pub struct RespondRequest {
    /// Instance that owns the Interaction.
    pub instance_id: InstanceId,
    /// Protocol Interaction id.
    pub interaction_id: InteractionId,
    /// CAS field.
    pub request_version: U64,
    /// CAS field.
    pub process_generation: U64,
    /// Encoded answer (never logged).
    pub answer: InteractionAnswer,
}

/// One follow/journal page.
#[derive(Debug, Clone, Default)]
pub struct FollowPage {
    /// Normalized events (tool boundaries, interactions, completion).
    pub events: Vec<FollowEvent>,
    /// Next `after_seq` for [`InstanceApi::follow`].
    pub next_seq: u64,
}

/// Journal events the dispatcher understands. Token deltas are omitted.
#[derive(Debug, Clone)]
pub enum FollowEvent {
    /// Tool started or finished — the only trigger for a progress card.
    ToolBoundary {
        /// Tool name (not stdout).
        name: String,
        /// One-line summary.
        summary: String,
        /// Seconds since the run started, if known.
        elapsed_secs: u64,
    },
    /// Permission / AskUserQuestion — mapped to a card ticket.
    Interaction(Box<Interaction>),
    /// Run finished.
    Completed {
        /// Short conclusion for the static card.
        conclusion: String,
        /// Green vs red header.
        ok: bool,
    },
    /// Non-boundary output; ignored for IM (no per-token cards).
    Output {
        /// Text the adapter must not stream into Feishu.
        text: String,
    },
}

/// Recorded [`InstanceApi`] invocation (tests / Fake).
#[derive(Debug, Clone)]
pub enum ApiCall {
    /// [`InstanceApi::create`].
    Create(CreateRequest),
    /// [`InstanceApi::send`].
    Send(SendRequest),
    /// [`InstanceApi::cancel`].
    Cancel(InstanceId),
    /// [`InstanceApi::respond`].
    Respond {
        /// Instance.
        instance_id: InstanceId,
        /// Interaction.
        interaction_id: InteractionId,
        /// Chosen `DecisionOption` id for approvals. Question form values are not
        /// recorded — they may hold `sensitive` field text.
        option_id: Option<String>,
    },
    /// [`InstanceApi::follow`].
    Follow {
        /// Instance.
        instance_id: InstanceId,
        /// Cursor.
        after_seq: u64,
    },
}

/// In-memory [`InstanceApi`] that records calls and plays scripted follow events.
#[derive(Debug, Default)]
pub struct FakeInstanceApi {
    calls: Mutex<Vec<ApiCall>>,
    follow: Mutex<BTreeMap<String, FollowPage>>,
    next_id: Mutex<Option<InstanceId>>,
}

impl FakeInstanceApi {
    /// Force the next [`InstanceApi::create`] id (tests).
    pub fn set_next_id(&self, id: InstanceId) {
        *self.next_id.lock().unwrap_or_else(|err| err.into_inner()) = Some(id);
    }

    /// Queue follow events for an instance (consumed on the next `follow`).
    pub fn set_follow(&self, instance_id: &InstanceId, page: FollowPage) {
        self.follow
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(instance_id.as_id().as_str().to_string(), page);
    }

    /// Snapshot of recorded calls, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<ApiCall> {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

impl InstanceApi for FakeInstanceApi {
    fn create(
        &self,
        req: CreateRequest,
    ) -> impl Future<Output = Result<CreatedInstance, Error>> + Send {
        let id = self
            .next_id
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
            .unwrap_or_default();
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(ApiCall::Create(req));
        async move { Ok(CreatedInstance { instance_id: id }) }
    }

    fn send(&self, req: SendRequest) -> impl Future<Output = Result<(), Error>> + Send {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(ApiCall::Send(req));
        async move { Ok(()) }
    }

    fn cancel(&self, instance_id: &InstanceId) -> impl Future<Output = Result<(), Error>> + Send {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(ApiCall::Cancel(instance_id.clone()));
        async move { Ok(()) }
    }

    fn respond(&self, req: RespondRequest) -> impl Future<Output = Result<(), Error>> + Send {
        let option_id = match &req.answer {
            InteractionAnswer::Approval(answer) => Some(answer.option_id.clone()),
            _ => None,
        };
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(ApiCall::Respond {
                instance_id: req.instance_id,
                interaction_id: req.interaction_id,
                option_id,
            });
        async move { Ok(()) }
    }

    fn follow(
        &self,
        instance_id: &InstanceId,
        after_seq: u64,
    ) -> impl Future<Output = Result<FollowPage, Error>> + Send {
        self.calls
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(ApiCall::Follow {
                instance_id: instance_id.clone(),
                after_seq,
            });
        let page = self
            .follow
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(instance_id.as_id().as_str())
            .unwrap_or_default();
        async move { Ok(page) }
    }
}

/// One routing/outbound decision (no secret values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchReport {
    /// Topic key.
    pub session_key: String,
    /// What happened.
    pub action: DispatchAction,
}

/// Kind of dispatcher action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchAction {
    /// [`InstanceApi::create`].
    Created {
        /// New instance id.
        instance_id: String,
    },
    /// [`InstanceApi::send`] on a live instance.
    Sent {
        /// Live instance id.
        instance_id: String,
    },
    /// [`InstanceApi::cancel`].
    Cancelled {
        /// Retired instance id.
        instance_id: String,
    },
    /// `/host` `/agent` `/model` pin (no instance call).
    Routed,
    /// `/status` reply.
    Status,
    /// Card or `/yes` `/no`.
    Answered {
        /// Ticket id.
        ticket_id: String,
    },
    /// Static progress / interaction / completion / expired card.
    CardPosted {
        /// `progress` / `interaction` / `completion` / `expired` / `recorded`.
        kind: String,
    },
    /// Duplicate, empty prompt, or unknown command already answered in IM.
    Ignored,
}

/// Consume → admit → route → InstanceApi → throttled static cards.
pub struct Dispatcher<A> {
    api: A,
    outbound: LarkCli,
    sessions: SessionStore,
    tickets: TicketStore,
    policy: InboundPolicy,
    dedup: Deduper,
    defaults: RouteDefaults,
}

impl<A: InstanceApi> Dispatcher<A> {
    /// In-memory session map (tests).
    pub fn memory(
        api: A,
        outbound: LarkCli,
        policy: InboundPolicy,
        defaults: RouteDefaults,
    ) -> Result<Self, Error> {
        Ok(Self {
            api,
            outbound,
            sessions: SessionStore::memory()?,
            tickets: TicketStore::with_default_ttl(),
            policy,
            dedup: Deduper::default(),
            defaults,
        })
    }

    /// Persist the session map under `path`.
    pub fn open(
        path: impl Into<PathBuf>,
        api: A,
        outbound: LarkCli,
        policy: InboundPolicy,
        defaults: RouteDefaults,
    ) -> Result<Self, Error> {
        Ok(Self {
            api,
            outbound,
            sessions: SessionStore::open(path)?,
            tickets: TicketStore::with_default_ttl(),
            policy,
            dedup: Deduper::default(),
            defaults,
        })
    }

    /// Recorded outbound argv.
    #[must_use]
    pub fn outbound(&self) -> &LarkCli {
        &self.outbound
    }

    /// Inner [`InstanceApi`] (tests inspect [`FakeInstanceApi`]).
    #[must_use]
    pub fn api(&self) -> &A {
        &self.api
    }

    /// Session map.
    #[must_use]
    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    /// Interaction tickets.
    #[must_use]
    pub fn tickets(&self) -> &TicketStore {
        &self.tickets
    }

    /// Pull journal pages for every mapped session (tool-boundary cards).
    pub async fn follow_live(&mut self, now: SystemTime) -> Result<Vec<DispatchReport>, Error> {
        let keys = self.sessions.list_keys()?;
        let mut reports = Vec::new();
        for key in keys {
            reports.extend(self.pump_follow(&key, now).await?);
        }
        Ok(reports)
    }

    /// Drain a consume channel until it closes.
    pub async fn drive_consume(
        &mut self,
        mut events: mpsc::Receiver<ConsumeEvent>,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let mut reports = Vec::new();
        while let Some(event) = events.recv().await {
            reports.extend(self.handle_consume(event, now).await?);
        }
        Ok(reports)
    }

    /// Admit a consume event and route it when policy allows.
    pub async fn handle_consume(
        &mut self,
        event: ConsumeEvent,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let ConsumeEvent::Event { event, .. } = event else {
            return Ok(Vec::new());
        };
        match admit(*event, &self.policy, &mut self.dedup)? {
            GateDecision::Take(inbound) => self.handle(*inbound, now).await,
            GateDecision::Drop { reason } => {
                debug!(?reason, "dispatcher dropped inbound");
                Ok(Vec::new())
            }
        }
    }

    /// Route one admitted inbound event.
    pub async fn handle(
        &mut self,
        inbound: Inbound,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let mut reports = match &inbound.kind {
            InboundKind::Message { intent, .. } => {
                self.handle_message(&inbound, intent.clone(), now).await?
            }
            InboundKind::CardAction { .. } => self.handle_card(&inbound, now).await?,
        };
        reports.extend(self.pump_follow(inbound.session_key.as_str(), now).await?);
        reports.extend(self.expire_tickets(now).await?);
        Ok(reports)
    }

    async fn handle_message(
        &mut self,
        inbound: &Inbound,
        intent: Intent,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let key = inbound.session_key.as_str();
        match intent {
            Intent::Command(ExplicitCommand::New) => self.cmd_new(inbound).await,
            Intent::Command(ExplicitCommand::Host { name }) => {
                self.cmd_pin(inbound, Pin::Host(name)).await
            }
            Intent::Command(ExplicitCommand::Agent { kind, .. }) => {
                self.cmd_pin(inbound, Pin::Agent(kind)).await
            }
            Intent::Command(ExplicitCommand::Model { model }) => {
                self.cmd_pin(inbound, Pin::Model(model)).await
            }
            Intent::Command(ExplicitCommand::Status) => self.cmd_status(inbound).await,
            Intent::Command(ExplicitCommand::Stop) => self.cmd_stop(inbound).await,
            Intent::Command(ExplicitCommand::Yes { ticket_id }) => {
                self.cmd_yes_no(inbound, ticket_id.as_deref(), "once", now)
                    .await
            }
            Intent::Command(ExplicitCommand::No { ticket_id }) => {
                self.cmd_yes_no(inbound, ticket_id.as_deref(), "deny", now)
                    .await
            }
            Intent::UnknownCommand { raw } => {
                self.reply_text(inbound, &format!("unknown command: {raw}"))
                    .await?;
                Ok(vec![report(key, DispatchAction::Ignored)])
            }
            Intent::Prompt { text } => self.handle_prompt(inbound, text).await,
        }
    }

    async fn handle_prompt(
        &mut self,
        inbound: &Inbound,
        text: String,
    ) -> Result<Vec<DispatchReport>, Error> {
        let key = inbound.session_key.as_str();
        if text.trim().is_empty() {
            return Ok(vec![report(key, DispatchAction::Ignored)]);
        }
        let mut binding = self.load_or_init(inbound)?;
        if binding.status == SessionStatus::Live
            && let Some(instance_id) = binding.instance_id.clone()
        {
            self.api
                .send(SendRequest {
                    instance_id: instance_id.clone(),
                    prompt: text,
                    idempotency_key: inbound.idempotency_key.clone(),
                })
                .await?;
            self.touch_inbound(&mut binding, inbound);
            self.sessions.put(&binding)?;
            info!(session_key = key, instance = %instance_id.as_id(), "dispatcher send");
            return Ok(vec![report(
                key,
                DispatchAction::Sent {
                    instance_id: instance_id.as_id().as_str().to_string(),
                },
            )]);
        }
        let created = self
            .api
            .create(CreateRequest {
                session_key: inbound.session_key.clone(),
                host: binding.host.clone(),
                agent: binding.agent,
                model: binding.model.clone(),
                prompt: text,
                idempotency_key: inbound.idempotency_key.clone(),
            })
            .await?;
        binding.instance_id = Some(created.instance_id.clone());
        binding.status = SessionStatus::Live;
        binding.follow_seq = 0;
        self.touch_inbound(&mut binding, inbound);
        self.sessions.put(&binding)?;
        info!(
            session_key = key,
            instance = %created.instance_id.as_id(),
            "dispatcher create"
        );
        Ok(vec![report(
            key,
            DispatchAction::Created {
                instance_id: created.instance_id.as_id().as_str().to_string(),
            },
        )])
    }

    async fn cmd_new(&mut self, inbound: &Inbound) -> Result<Vec<DispatchReport>, Error> {
        let key = inbound.session_key.as_str();
        let mut binding = self.load_or_init(inbound)?;
        let mut reports = Vec::new();
        if binding.status == SessionStatus::Live
            && let Some(instance_id) = binding.instance_id.clone()
        {
            self.api.cancel(&instance_id).await?;
            reports.push(report(
                key,
                DispatchAction::Cancelled {
                    instance_id: instance_id.as_id().as_str().to_string(),
                },
            ));
        }
        binding.instance_id = None;
        binding.status = SessionStatus::Idle;
        binding.follow_seq = 0;
        self.touch_inbound(&mut binding, inbound);
        self.sessions.put(&binding)?;
        // F9: a retired instance must not keep answerable cards behind it.
        let dropped = self.tickets.expire_session(key);
        if !dropped.is_empty() {
            info!(
                session_key = key,
                tickets = dropped.len(),
                "/new expired open tickets"
            );
        }
        self.reply_text(inbound, "new session; send a prompt to create")
            .await?;
        reports.push(report(key, DispatchAction::Routed));
        Ok(reports)
    }

    async fn cmd_pin(&mut self, inbound: &Inbound, pin: Pin) -> Result<Vec<DispatchReport>, Error> {
        let mut binding = self.load_or_init(inbound)?;
        let msg = match pin {
            Pin::Host(name) => {
                binding.host = name.clone();
                format!("host set to {name}")
            }
            Pin::Agent(kind) => {
                binding.agent = kind;
                format!("agent set to {}", agent_wire(kind))
            }
            Pin::Model(model) => {
                binding.model = Some(model.clone());
                format!("model set to {model}")
            }
        };
        self.touch_inbound(&mut binding, inbound);
        self.sessions.put(&binding)?;
        self.reply_text(inbound, &msg).await?;
        Ok(vec![report(
            inbound.session_key.as_str(),
            DispatchAction::Routed,
        )])
    }

    async fn cmd_status(&mut self, inbound: &Inbound) -> Result<Vec<DispatchReport>, Error> {
        let binding = self.load_or_init(inbound)?;
        let instance = binding
            .instance_id
            .as_ref()
            .map(|id| id.as_id().as_str())
            .unwrap_or("none");
        let model = binding.model.as_deref().unwrap_or("default");
        let text = format!(
            "session {}\ninstance {instance}\nstatus {:?}\nhost {}\nagent {}\nmodel {model}",
            binding.session_key,
            binding.status,
            binding.host,
            agent_wire(binding.agent),
        );
        self.reply_text(inbound, &text).await?;
        Ok(vec![report(
            inbound.session_key.as_str(),
            DispatchAction::Status,
        )])
    }

    async fn cmd_stop(&mut self, inbound: &Inbound) -> Result<Vec<DispatchReport>, Error> {
        let key = inbound.session_key.as_str();
        let mut binding = self.load_or_init(inbound)?;
        let mut reports = Vec::new();
        if let Some(instance_id) = binding.instance_id.clone() {
            if binding.status == SessionStatus::Live {
                self.api.cancel(&instance_id).await?;
                reports.push(report(
                    key,
                    DispatchAction::Cancelled {
                        instance_id: instance_id.as_id().as_str().to_string(),
                    },
                ));
            }
        } else {
            self.reply_text(inbound, "no live instance").await?;
            return Ok(vec![report(key, DispatchAction::Ignored)]);
        }
        binding.status = SessionStatus::Stopped;
        self.touch_inbound(&mut binding, inbound);
        self.sessions.put(&binding)?;
        let dropped = self.tickets.expire_session(key);
        if !dropped.is_empty() {
            info!(
                session_key = key,
                tickets = dropped.len(),
                "/stop expired open tickets"
            );
        }
        self.reply_text(inbound, "stopped").await?;
        Ok(reports)
    }

    async fn cmd_yes_no(
        &mut self,
        inbound: &Inbound,
        ticket_id: Option<&str>,
        a: &str,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let key = inbound.session_key.as_str();
        let reference = ShortcutRef {
            ticket_id,
            reply_to: inbound.thread.reply_to.as_deref(),
        };
        let ticket = match self.tickets.resolve_shortcut(key, reference, now) {
            Ok(ticket) => ticket.clone(),
            Err(miss) => {
                self.reply_text(inbound, &shortcut_miss_text(&miss)).await?;
                return Ok(vec![report(key, DispatchAction::Ignored)]);
            }
        };
        let callback = CallbackValue {
            tid: ticket.ticket_id.clone(),
            a: a.to_string(),
        };
        let mapped =
            self.tickets
                .answer_callback(&callback, None, AnswerScope::Session(key), now)?;
        // Echo what was approved: the owner sees the tool, not just "ok".
        let verb = if a == "deny" {
            "Denied"
        } else {
            "Allowed once"
        };
        let echo = format!(
            "{verb}: {} ({})",
            request_title(&mapped.ticket.request),
            mapped.ticket.ticket_id
        );
        self.reply_text(inbound, &echo).await?;
        self.commit_answer(inbound, mapped).await
    }

    async fn handle_card(
        &mut self,
        inbound: &Inbound,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let InboundKind::CardAction { action, .. } = &inbound.kind else {
            return Ok(Vec::new());
        };
        // Card actions carry no thread identity, so they bind to the chat (F9).
        let scope = AnswerScope::Chat(&inbound.chat_id);
        match self.tickets.answer_card(action, scope, now) {
            Ok(mapped) => self.commit_answer(inbound, mapped).await,
            Err(Error::TicketExpired(tid)) => {
                let card = crate::render_expired_card("Expired")?;
                self.reply_card(inbound, &card, &format!("expired:{tid}"))
                    .await?;
                Ok(vec![report(
                    inbound.session_key.as_str(),
                    DispatchAction::CardPosted {
                        kind: "expired".into(),
                    },
                )])
            }
            Err(Error::TicketScope { ticket_id, scope }) => {
                warn!(%ticket_id, %scope, "card answer rejected: ticket belongs to another chat");
                self.reply_text(inbound, "card is not actionable here")
                    .await?;
                Ok(vec![report(
                    inbound.session_key.as_str(),
                    DispatchAction::Ignored,
                )])
            }
            Err(Error::TicketNotFound(_)) | Err(Error::TicketAnswered(_)) => {
                self.reply_text(inbound, "card is not actionable").await?;
                Ok(vec![report(
                    inbound.session_key.as_str(),
                    DispatchAction::Ignored,
                )])
            }
            Err(err) => Err(err),
        }
    }

    async fn commit_answer(
        &mut self,
        inbound: &Inbound,
        mapped: MappedAnswer,
    ) -> Result<Vec<DispatchReport>, Error> {
        // F9: resolve the instance from the ticket's own session, not from whichever
        // session happened to deliver the answer.
        let key = mapped.ticket.session_key.clone();
        let binding = self
            .sessions
            .get(&key)?
            .ok_or_else(|| Error::NoLiveInstance(key.clone()))?;
        let Some(instance_id) = binding.instance_id.clone() else {
            return Err(Error::NoLiveInstance(key));
        };
        self.api
            .respond(RespondRequest {
                instance_id,
                interaction_id: mapped.ticket.interaction_id.clone(),
                request_version: mapped.ticket.request_version,
                process_generation: mapped.ticket.process_generation,
                answer: mapped.answer,
            })
            .await?;
        self.reply_card(
            inbound,
            &mapped.replacement_card,
            &format!("recorded:{}", mapped.ticket.ticket_id),
        )
        .await?;
        Ok(vec![
            report(
                &key,
                DispatchAction::Answered {
                    ticket_id: mapped.ticket.ticket_id.clone(),
                },
            ),
            report(
                &key,
                DispatchAction::CardPosted {
                    kind: "recorded".into(),
                },
            ),
        ])
    }

    /// Pull one session's follow page and emit static cards.
    pub async fn pump_follow(
        &mut self,
        session_key: &str,
        now: SystemTime,
    ) -> Result<Vec<DispatchReport>, Error> {
        let Some(mut binding) = self.sessions.get(session_key)? else {
            return Ok(Vec::new());
        };
        if binding.status != SessionStatus::Live {
            return Ok(Vec::new());
        }
        let Some(instance_id) = binding.instance_id.clone() else {
            return Ok(Vec::new());
        };
        let page = self.api.follow(&instance_id, binding.follow_seq).await?;
        binding.follow_seq = page.next_seq;
        let mut reports = Vec::new();
        for event in page.events {
            match event {
                FollowEvent::Output { .. } => {}
                FollowEvent::ToolBoundary {
                    name,
                    summary,
                    elapsed_secs,
                } => {
                    if !allow_progress(binding.last_progress_ms, now) {
                        continue;
                    }
                    let card = render_progress_card("Running", &name, &summary, elapsed_secs)?;
                    self.send_chat_card(&binding, &card, &format!("progress:{name}"))
                        .await?;
                    binding.last_progress_ms = unix_ms(now);
                    reports.push(report(
                        session_key,
                        DispatchAction::CardPosted {
                            kind: "progress".into(),
                        },
                    ));
                }
                FollowEvent::Interaction(interaction) => {
                    let (ticket, card) = self.tickets.issue(&interaction, session_key, now)?;
                    let receipt = self
                        .send_chat_card(
                            &binding,
                            &card,
                            &format!("interaction:{}", ticket.ticket_id),
                        )
                        .await?;
                    // Lets `/yes` bind by replying to the card itself (F8).
                    if let Some(message_id) = receipt.message_id.as_deref() {
                        self.tickets
                            .set_card_message_id(&ticket.ticket_id, message_id);
                    }
                    reports.push(report(
                        session_key,
                        DispatchAction::CardPosted {
                            kind: "interaction".into(),
                        },
                    ));
                }
                FollowEvent::Completed { conclusion, ok } => {
                    let card = render_completion_card("Done", &conclusion, ok)?;
                    self.send_chat_card(&binding, &card, "completion").await?;
                    reports.push(report(
                        session_key,
                        DispatchAction::CardPosted {
                            kind: "completion".into(),
                        },
                    ));
                }
            }
        }
        self.sessions.put(&binding)?;
        Ok(reports)
    }

    async fn expire_tickets(&mut self, now: SystemTime) -> Result<Vec<DispatchReport>, Error> {
        let expired = self.tickets.expire_due(now)?;
        let mut reports = Vec::new();
        for (ticket, card) in expired {
            if let Some(binding) = self.sessions.get(&ticket.session_key)? {
                self.send_chat_card(&binding, &card, &format!("expired:{}", ticket.ticket_id))
                    .await?;
                reports.push(report(
                    &ticket.session_key,
                    DispatchAction::CardPosted {
                        kind: "expired".into(),
                    },
                ));
            } else {
                warn!(
                    session_key = %ticket.session_key,
                    "expired ticket with no session row"
                );
            }
        }
        Ok(reports)
    }

    fn load_or_init(&self, inbound: &Inbound) -> Result<SessionBinding, Error> {
        if let Some(existing) = self.sessions.get(inbound.session_key.as_str())? {
            return Ok(existing);
        }
        Ok(SessionBinding {
            session_key: inbound.session_key.as_str().to_string(),
            instance_id: None,
            host: self.defaults.host.clone(),
            agent: self.defaults.agent,
            model: self.defaults.model.clone(),
            status: SessionStatus::Idle,
            chat_id: inbound.chat_id.clone(),
            last_message_id: reply_target(inbound).map(str::to_string),
            in_thread: inbound.thread.session_suffix() != "main",
            follow_seq: 0,
            last_progress_ms: 0,
        })
    }

    fn touch_inbound(&self, binding: &mut SessionBinding, inbound: &Inbound) {
        binding.chat_id = inbound.chat_id.clone();
        binding.last_message_id = reply_target(inbound).map(str::to_string);
        binding.in_thread = inbound.thread.session_suffix() != "main";
    }

    async fn reply_text(&mut self, inbound: &Inbound, text: &str) -> Result<(), Error> {
        let target = reply_target(inbound).unwrap_or("");
        let in_thread = inbound.thread.session_suffix() != "main";
        if target.is_empty() {
            self.outbound
                .send_text(&inbound.chat_id, text, &inbound.idempotency_key)
                .await?;
        } else {
            self.outbound
                .reply(
                    target,
                    OutboundBody::Text(text.to_string()),
                    in_thread,
                    &inbound.idempotency_key,
                )
                .await?;
        }
        Ok(())
    }

    async fn reply_card(
        &mut self,
        inbound: &Inbound,
        card: &serde_json::Value,
        seed: &str,
    ) -> Result<(), Error> {
        let target = reply_target(inbound).unwrap_or("");
        let in_thread = inbound.thread.session_suffix() != "main";
        if target.is_empty() {
            self.outbound
                .send_card(&inbound.chat_id, card, seed)
                .await?;
        } else {
            self.outbound
                .reply(
                    target,
                    OutboundBody::Interactive(card.clone()),
                    in_thread,
                    seed,
                )
                .await?;
        }
        Ok(())
    }

    async fn send_chat_card(
        &mut self,
        binding: &SessionBinding,
        card: &serde_json::Value,
        seed: &str,
    ) -> Result<crate::outbound::OutboundReceipt, Error> {
        if let Some(message_id) = binding.last_message_id.as_deref() {
            self.outbound
                .reply(
                    message_id,
                    OutboundBody::Interactive(card.clone()),
                    binding.in_thread,
                    seed,
                )
                .await
        } else {
            self.outbound.send_card(&binding.chat_id, card, seed).await
        }
    }
}

enum Pin {
    Host(String),
    Agent(AgentKind),
    Model(String),
}

fn report(session_key: &str, action: DispatchAction) -> DispatchReport {
    DispatchReport {
        session_key: session_key.to_string(),
        action,
    }
}

fn reply_target(inbound: &Inbound) -> Option<&str> {
    match &inbound.kind {
        InboundKind::Message { message_id, .. } => Some(message_id.as_str()),
        InboundKind::CardAction { action, .. } => Some(action.message_id.as_str()),
    }
}

/// What to tell the owner when `/yes` could not be bound to exactly one ticket (F8).
fn shortcut_miss_text(miss: &ShortcutMiss) -> String {
    match miss {
        ShortcutMiss::NoneOpen => "no pending approval".to_string(),
        ShortcutMiss::UnknownTicket(tid) => {
            format!("no pending approval {tid} in this topic")
        }
        ShortcutMiss::Ambiguous(ids) => format!(
            "{} approvals pending; reply to a card or name one: {}",
            ids.len(),
            ids.iter()
                .map(|id| format!("/yes {id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn allow_progress(last_progress_ms: u64, now: SystemTime) -> bool {
    let now_ms = unix_ms(now);
    if last_progress_ms == 0 {
        return true;
    }
    now_ms.saturating_sub(last_progress_ms) >= PROGRESS_MIN_INTERVAL.as_millis() as u64
}

fn unix_ms(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn agent_wire(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Grok => "grok",
        AgentKind::Agy => "agy",
        AgentKind::Generic => "generic",
        AgentKind::Terminal => "terminal",
    }
}

fn agent_from_wire(raw: &str) -> AgentKind {
    match raw {
        "codex" => AgentKind::Codex,
        "grok" => AgentKind::Grok,
        "agy" => AgentKind::Agy,
        "generic" => AgentKind::Generic,
        "terminal" => AgentKind::Terminal,
        _ => AgentKind::Claude,
    }
}

fn status_wire(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Live => "live",
        SessionStatus::Stopped => "stopped",
    }
}

fn status_from_wire(raw: &str) -> SessionStatus {
    match raw {
        "live" => SessionStatus::Live,
        "stopped" => SessionStatus::Stopped,
        _ => SessionStatus::Idle,
    }
}
