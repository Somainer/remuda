//! Codex rollout → journal observations (D-028 §3, §6, §7, §13 P6).
//!
//! What the rollout gives us, and what it does not — every line here traces to
//! [codex-signals-1](../../../docs/design/evidence/codex-signals-1.md):
//!
//! * **Turn boundaries** are `event_msg/task_started` (working) and
//!   `task_complete` (idle) or `turn_aborted{reason:"interrupted"}` (idle,
//!   not a failure: the composer must come back). The `notify` hook also
//!   reports turn completion, but it fires for *title-generation child
//!   threads* too, so it can only corroborate a thread-filtered rollout event
//!   and is never a boundary by itself (§3 [V]).
//! * **Content is item-level.** Only completed items are mapped — codex writes
//!   no text deltas, and advertising token streaming from this file is
//!   explicitly forbidden (§7). `item_completed` of `UserMessage` /
//!   `AgentMessage` / `Reasoning` yields one complete message/thought.
//! * **Tools** come from two representations: `response_item/function_call`
//!   (the proposal, with its JSON `arguments`) and `item_completed` of
//!   `CommandExecution` (the result: status, stdout, exit code). They share
//!   `call_id`; `function_call_output` duplicates the result and is used only
//!   when no `CommandExecution` ever arrives for the call. A late completion
//!   can land under the **old** `turn_id` after an abort (evidence ordinal
//!   86); results are keyed by `call_id`, never re-assigned to the current
//!   turn.
//! * **Usage** is `token_usage_record` (per response, additive) via
//!   [`crate::usage::codex::CodexUsage`]; the cumulative `token_count`
//!   snapshots are fed to the same extractor and refused by its aggregator.
//! * **There is no queue/steer provenance in this file.** Delivered messages
//!   look like ordinary messages; same/different `turn_id` is the only
//!   evidence of steer-vs-queue (§7 [V]). The adapter invents nothing.
//! * **No approval decision exists in this file.** PermissionRequest is a hook
//!   channel (P5); the rollout only shows the resulting rejection string.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use remuda_protocol::{AgentKind, Id, ObservationPayload, Timestamp, ToolOutcome, UsageScope};

use crate::adapters::native_timestamp;
use serde_json::Value;

use crate::adapters::{
    AdapterBinding, AdapterHome, AdapterObservation, FileSignalAdapter, message_payload, node_id,
    thought_payload, tool_call_payload, tool_result_payload, turn_lifecycle,
};
use crate::codex_rollout::{
    CodexRolloutEvent, CwdRollout, RolloutTail, locate_rollout_by_cwd, locate_rollout_in,
    parse_rollout_line_at,
};
use crate::error::DriverResult;
use crate::usage::codex::CodexUsage;
use crate::usage::{UsageAggregator, to_usage_payload};

/// File-tail adapter for one codex TUI session.
pub struct CodexAdapter {
    home: AdapterHome,
    /// Stable identity of the driver launch owning this adapter. Every prompt
    /// it may bind to was dispatched under this exact key (c-usagefu r6 item
    /// 1); `None` for promoted/hook-attached adapters (whose claim is
    /// released on close).
    instance_key: Option<String>,
    /// Session id established by the HOOK channel (channel A). It stays
    /// distinct from a tentative cwd-discovery guess (r6 item 2).
    confirmed: Option<String>,
    /// Session id established by cwd discovery, recorded ONLY after a claim
    /// succeeded — a claim loser must never shortcut on the id it scanned.
    discovered: Option<String>,
    /// Bound session + its rollout tail.
    binding: Option<AdapterBinding>,
    /// Exact claim key recorded at bind for promoted adapters (r7 item 6):
    /// released verbatim on Drop even when the file/symlink is gone.
    claim_key: Option<Arc<PathBuf>>,
    tail: Option<RolloutTail>,
    /// Native id → journal node id (messages, thoughts, tool calls).
    nodes: HashMap<String, Id>,
    /// Tool calls already announced, so an item without a `function_call`
    /// record does not announce the same call twice.
    announced_tools: HashSet<String>,
    /// Tool calls that already have a result.
    results: HashSet<String>,
    /// Turns already announced as started.
    started_turns: HashSet<String>,
    /// Turns already announced as ended (complete or aborted).
    ended_turns: HashSet<String>,
    session_usage_revision: u64,
    /// Additive usage extractor + aggregator (refuses cumulative snapshots).
    usage: CodexUsage,
    usage_totals: UsageAggregator,
    /// Optional fallback model for usage records whose turn had no context.
    fallback_model: Option<String>,
    /// Discovery proven impossible: TWO unclaimed valid rollouts exist at
    /// once for this same cwd and this input (duplicate own prompts —
    /// genuinely indistinguishable), or a claim race left two identical
    /// candidates. A transient "another window overlaps" does NOT latch: the
    /// adapter keeps discovering until it binds or the instance closes, and
    /// the window is released either way (c-usagefu r5 item 1).
    discovery_gave_up: bool,
    /// Process-wide same-cwd launch window for a driver launch. None for a
    /// promoted/attach adapter (no launch floor); released on bind/give-up.
    window: Option<crate::adapters::codex_discovery::DiscoveryWindow>,
    /// r9 item 1: the instance's live interval in its cwd, registered BEFORE
    /// launch (production) and retained through close independent of binding.
    /// Clones are held by the live driver slot and by this adapter; the
    /// interval closes when the last one drops. None for a promoted/attach
    /// adapter and for non-codex launches.
    lifetime: Option<crate::adapters::codex_discovery::LifetimeGuard>,
    /// r8 item 1 tier b: process-tree ownership probe + live agent roots.
    ownership: Option<std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>>,
    agent_pid_roots: Option<std::sync::Arc<std::sync::Mutex<Vec<u32>>>>,
    /// Unique lifecycle owner for this adapter (r7 item 2). Every adapter —
    /// promoted included — gets its own UUID at construction so two promoted
    /// adapters never share the `hook:S` owner for the same indexed session.
    lifecycle_owner: String,
}

impl CodexAdapter {
    /// Create an unbound adapter for a shadow `CODEX_HOME`.
    #[must_use]
    pub fn new(home: AdapterHome) -> Self {
        // The discovery window opens in with_instance_key so its owner matches
        // the instance key (r7 item 1).
        Self {
            home,
            instance_key: None,
            confirmed: None,
            discovered: None,
            binding: None,
            claim_key: None,
            tail: None,
            nodes: HashMap::new(),
            announced_tools: HashSet::new(),
            results: HashSet::new(),
            started_turns: HashSet::new(),
            ended_turns: HashSet::new(),
            session_usage_revision: 0,
            usage: CodexUsage::new(),
            usage_totals: UsageAggregator::new(),
            fallback_model: None,
            discovery_gave_up: false,
            window: None,
            lifetime: None,
            ownership: None,
            agent_pid_roots: None,
            lifecycle_owner: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Attach the stable per-launch identity used for prompt matching and
    /// rollout claims (c-usagefu r6 item 1). Opens the discovery window for a
    /// driver-launched adapter so live cross-owner exclusivity works (r7
    /// item 1).
    ///
    /// r9 item 1: also registers the instance's live interval (begins at the
    /// adapter's launch floor). Production registers it BEFORE spawning the
    /// agent and attaches the existing guard through
    /// [`Self::with_lifetime`]; in that case this does not open a second
    /// interval.
    #[must_use]
    pub fn with_instance_key(mut self, key: Option<String>) -> Self {
        if let Some(ref key) = key
            && self.window.is_none()
            && let Some(launched_at) = self.home.launched_at
        {
            self.window = Some(crate::adapters::codex_discovery::DiscoveryWindow::open(
                &self.home.cwd,
                key.clone(),
            ));
            if self.lifetime.is_none() {
                self.lifetime = Some(crate::adapters::codex_discovery::begin_lifetime(
                    &self.home.cwd,
                    key,
                    launched_at,
                ));
            }
        }
        self.instance_key = key;
        self
    }

    /// Attach the lifetime guard registered BEFORE this agent was launched
    /// (generic-pty, r9 item 1). Must be applied before
    /// [`Self::with_instance_key`] so the pre-launch interval floor is kept.
    #[must_use]
    pub(crate) fn with_lifetime(
        mut self,
        lifetime: crate::adapters::codex_discovery::LifetimeGuard,
    ) -> Self {
        self.lifetime = Some(lifetime);
        self
    }

    /// Test seam: record this instance's close at `at` when the adapter
    /// drops, instead of wall-clock time (deterministic lifetime overlaps).
    #[cfg(test)]
    pub(crate) fn close_lifetime_at(&self, at: time::OffsetDateTime) {
        use crate::adapters::codex_discovery::LifetimeGuardClock as _;
        if let Some(lifetime) = &self.lifetime {
            lifetime.close_lifetime_at(at);
        }
    }

    /// Attach the process-tree ownership evidence for tier-b discovery (r8
    /// item 1).
    #[must_use]
    pub(crate) fn with_ownership(
        mut self,
        probe: std::sync::Arc<dyn crate::adapters::codex_ownership::RolloutOwnership>,
        roots: Option<std::sync::Arc<std::sync::Mutex<Vec<u32>>>>,
    ) -> Self {
        self.ownership = Some(probe);
        self.agent_pid_roots = roots;
        self
    }

    /// Seed a model used to price usage when `turn_context` never reported one.
    #[must_use]
    pub fn with_fallback_model(mut self, model: impl Into<String>) -> Self {
        // Replace in place: CodexAdapter has a Drop impl, so moving the usage
        // field out of `&mut self` is not allowed.
        let previous = std::mem::replace(&mut self.usage, CodexUsage::new());
        let model = model.into();
        self.fallback_model = Some(model.clone());
        self.usage = previous.with_fallback_model(model);
        self
    }

    /// The hook channel confirmed this native session; discovery prefers it.
    pub fn confirm_session(&mut self, session_id: &str) {
        if session_id.trim().is_empty() {
            return;
        }
        self.confirmed = Some(session_id.trim().to_owned());
    }
    /// Currently bound session.
    #[must_use]
    pub fn binding(&self) -> Option<&AdapterBinding> {
        self.binding.as_ref()
    }

    /// Bind directly to a known rollout file (a `SessionStart` hook or a test
    /// harness may resolve it without going through cwd discovery).
    pub fn bind_rollout(&mut self, session_id: &str, path: PathBuf) {
        if !session_id.trim().is_empty() {
            self.confirmed = Some(session_id.trim().to_owned());
            self.bind(session_id.trim().to_owned(), path, true);
        }
    }

    /// Bind the rollout tail for a discovered or hook-confirmed session.
    ///
    /// Claims the file process-wide (no other adapter may bind it) and, for a
    /// driver launch, releases the same-cwd discovery window so a later
    /// launch is not tainted by this instance (c-usagefu r5 item 1).
    fn bind(&mut self, session_id: String, path: PathBuf, strict_launch: bool) -> bool {
        // Owner of the claim: the driver launch's instance key when present;
        // promoted attaches use their unique lifecycle_owner UUID (r7 item 2),
        // so two promoted adapters never co-claim the same indexed session.
        let owner = self
            .instance_key
            .clone()
            .unwrap_or_else(|| self.lifecycle_owner.clone());
        // Two adapters polling in parallel can race for the same file; a lost
        // claim means the candidate is someone else's — don't bind and don't
        // remember the id (r6 item 2).
        let cwd_for_claim = self.home.cwd.clone();
        if !crate::adapters::codex_discovery::claim_session(
            &path,
            &owner,
            Some((&cwd_for_claim, &session_id)),
        ) {
            return false;
        }
        // Tail position:
        // - promoted / hook-confirmed attaches (no launch floor): byte zero
        //   with durable dedupe;
        // - a STRICTLY post-launch session (session_meta.timestamp >=
        //   launched_at): byte zero too — codex 0.154 creates the rollout
        //   lazily on the first turn, so every record (task_started,
        //   turn_context, prompt, usage) belongs to THIS instance;
        // - an in-slack match (crash-loop possible): current EOF, so a prior
        //   session's records never replay (r4 item 3).
        let from_start = self.home.launched_at.is_none() || strict_launch;
        let tail = if from_start {
            RolloutTail::new(path.clone())
        } else {
            RolloutTail::new_at_end(path.clone()).unwrap_or_else(|_| RolloutTail::new(path.clone()))
        };
        self.tail = Some(tail);
        // r8 item 6: remember the exact PathBuf claim_session inserted (it
        // canonicalizes the incoming path once). Drop must release THAT key
        // verbatim — it never re-canonicalizes, so removing the file/symlink
        // after bind cannot change the key and leak the claim.
        let exact_claim_key =
            std::sync::Arc::new(crate::adapters::codex_discovery::canonical_claim_key(&path));
        self.binding = Some(AdapterBinding {
            session_id: session_id.clone(),
            directory: path.parent().map(std::path::Path::to_path_buf),
            main_file: Some(path),
        });
        self.claim_key = Some(exact_claim_key);
        // Bound: stop counting against later launches in this cwd.
        if let Some(mut window) = self.window.take() {
            window.release();
        }
        true
    }

    /// Find the session's rollout.
    ///
    /// Three evidence tiers:
    ///
    /// 1. **Hook-confirmed id** (channel A): the supervisor hands this only to
    ///    an adapter whose agent pid matches, so the file for exactly that id
    ///    may bind; a missing file means "wait", never "guess".
    /// 2. **Driver launch with a launch floor** (generic-pty: no hooks, no
    ///    child pid, real operator home): the ONLY acceptable match is a
    ///    unique rollout whose `session_meta.cwd` is the launch cwd and whose
    ///    session started at/after launch. Zero matches keeps polling for the
    ///    adapter's WHOLE life (codex 0.154 creates the rollout lazily on the
    ///    first turn; the locator's mtime/date pruning keeps that cheap); two
    ///    or more matches at once fail closed forever (see item 3 for the
    ///    cross-instance race). The `session_index.jsonl` recency guess is
    ///    deliberately NOT used here: it is a name index over EVERY project
    ///    and its last line is whatever thread the operator renamed last,
    ///    anywhere — binding that journals a foreign project's transcript,
    ///    tool calls, usage and cost under this instance (c-usagefu r3 item 1).
    /// 3. **Promoted/foreign-launched attach** (no launch floor): the session
    ///    predates us, so the name-index recency guess remains the last
    ///    resort, exactly as before.
    fn discover(&mut self) -> DriverResult<bool> {
        // A prior poll proved ambiguity: never rescan and never reverse.
        if self.discovery_gave_up {
            return Ok(false);
        }
        // Tier 1: a HOOK-confirmed id (channel A) — distinct from a cwd guess.
        if let Some(id) = self.confirmed.clone() {
            if let Some(path) = locate_rollout_in(&self.home.home, &id)
                && self.bind(id, path, true)
            {
                return Ok(true);
            }
            return Ok(false);
        }
        // A cwd discovery id is remembered only after its claim succeeded
        // (set below); a claim loser reaches here every poll and keeps
        // scanning, never shortcutting on an id it failed to claim.
        if let Some(id) = self.discovered.clone() {
            if let Some(path) = locate_rollout_in(&self.home.home, &id)
                && self.bind(id, path, true)
            {
                return Ok(true);
            }
            // The claimed file vanished before bind; forget the guess.
            self.discovered = None;
        }
        let Some(launched_at) = self.home.launched_at else {
            if let Some(id) = newest_indexed(&self.home.home)
                && let Some(path) = locate_rollout_in(&self.home.home, &id)
                && self.bind(id, path, true)
            {
                return Ok(true);
            }
            return Ok(false);
        };
        // No time deadline: the rollout can appear on a first prompt minutes
        // after launch. Old pre-launch rollouts are filtered by timestamp AND
        // skipped cheaply via mtime/date pruning in the locator.
        // Prompts are authorized per INSTANCE, and files already owned by a
        // different instance are excluded.
        let owner = self.instance_key.clone().unwrap_or_default();
        let cwd = self.home.cwd.clone();
        // r8 item 1 tier b: clone the process-ownership evidence so the gate
        // closure needs no self borrow.
        let probe = self.ownership.clone();
        let pid_roots = self.agent_pid_roots.clone();
        let process_owns = move |path: &Path| -> bool {
            let (Some(probe), Some(roots)) = (&probe, &pid_roots) else {
                return false;
            };
            let roots = roots.lock().map(|r| r.clone()).unwrap_or_default();
            roots
                .iter()
                .any(|root| probe.file_held_in_tree(path, *root))
        };
        match locate_rollout_by_cwd(
            &self.home.home,
            &cwd,
            launched_at,
            &|path: &Path| crate::adapters::codex_discovery::is_claimed_by_other(path, &owner),
            &|path, started, first_prompt, metadata| {
                use crate::codex_rollout::CandidateTier;
                // TIER A (r9 item 2 ordering): re-recognition by id of a
                // session THIS owner already claimed (the hook-confirmed id
                // itself is handled before the cwd scan).
                if metadata["id"]
                    .as_str()
                    .is_some_and(|id| crate::adapters::codex_discovery::owned_by(&cwd, &owner, id))
                {
                    return Some(CandidateTier::Hook);
                }
                // TIER B: the rollout file is open in THIS instance's agent
                // process tree — attribution independent of prompt text and
                // immune to identical prompts / closed owners.
                if process_owns(path) {
                    return Some(CandidateTier::Process);
                }
                // TIER C (r9 item 1): prompt text ONLY when the rollout was
                // created at/after this instance's ACTUAL launch (no
                // pre-launch slack) and this was the sole live instance in
                // the cwd over the whole [launch, creation] interval —
                // including since-closed instances whose lifetime the
                // registry retained. Anything else waits.
                crate::adapters::codex_discovery::sole_instance_matches(
                    &cwd,
                    &owner,
                    first_prompt,
                    started,
                )
                .then_some(CandidateTier::Prompt)
            },
        ) {
            CwdRollout::Found { id, path, strict } => {
                if self.bind(id.clone(), path, strict) {
                    // Remember the discovery id only NOW (post-claim).
                    self.discovered = Some(id);
                    Ok(true)
                } else {
                    // Lost a claim race: stay in discovery and scan again.
                    Ok(false)
                }
            }
            // r8 item 1: NEVER give up while a stronger tier (hook / process
            // ownership) can still arrive — keep polling.
            CwdRollout::Ambiguous | CwdRollout::NotYet => Ok(false),
        }
    }

    /// Map one parsed rollout line to zero or more observations.
    fn on_line(&mut self, ordinal: u64, line: &str) -> Vec<AdapterObservation> {
        let Ok(record) = parse_rollout_line_at(line, ordinal) else {
            return Vec::new();
        };
        // Usage first: the extractor is stateful (turn→model map) and must see
        // records in file order regardless of what else they map to.
        let usage_event = self.usage.on_record(&record);
        // c-usagefu r8 item 5: only keep/emit a usage event the in-memory
        // aggregator ACCEPTS (it rejects duplicate request ids and cumulative
        // token_count rows). A rejected event would still carry a durable
        // response id the Hub would dedupe, but suppressing it at the source
        // keeps the adapter and the Hub consistent on a replay.
        let usage_event = usage_event.filter(|event| self.usage_totals.push(event));
        let turn = turn_id(&record);
        match &record.event {
            CodexRolloutEvent::SessionMeta { session_id, .. } => {
                if let Some(id) = session_id {
                    self.confirmed = Some(id.clone());
                }
                Vec::new()
            }
            CodexRolloutEvent::TaskStarted { turn_id, .. } => {
                let Some(turn_id) = turn_id else {
                    return Vec::new();
                };
                if self.started_turns.insert(turn_id.clone()) {
                    vec![
                        AdapterObservation::structured(turn_lifecycle("task_started", "working"))
                            .with_turn(turn_id),
                    ]
                } else {
                    Vec::new()
                }
            }
            CodexRolloutEvent::TaskComplete {
                turn_id,
                last_agent_message,
                ..
            } => self.end_turn(
                turn_id.as_deref(),
                "task_complete",
                last_agent_message.clone(),
                native_timestamp(record.timestamp.as_deref()),
            ),
            CodexRolloutEvent::ItemCompleted { turn_id, item } => {
                self.on_completed_item(turn_id.clone(), item)
            }
            CodexRolloutEvent::ToolCall {
                call_id,
                name,
                input,
            } => {
                if let Some(call_id) = call_id
                    && self.announced_tools.insert(call_id.clone())
                {
                    let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                    let mut observed = AdapterObservation::structured(tool_call_payload(
                        id,
                        name.clone(),
                        structured_input(name.as_deref(), input),
                    ));
                    observed.item_id = Some(call_id.clone());
                    if let Some(turn) = turn {
                        observed.turn_id = Some(turn);
                    }
                    return vec![observed];
                }
                Vec::new()
            }
            CodexRolloutEvent::ToolOutput { call_id, output } => {
                // Duplicate of the `CommandExecution` item result; only use it
                // when that item never arrives for the call.
                let Some(call_id) = call_id else {
                    return Vec::new();
                };
                if self.announced_tools.contains(call_id) || self.results.contains(call_id) {
                    return Vec::new();
                }
                self.results.insert(call_id.clone());
                let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                let text = output.as_str().map(str::to_owned);
                let observed = AdapterObservation::structured(tool_result_payload(
                    id,
                    text,
                    Some(output.clone()),
                    None,
                    ToolOutcome::Unknown,
                ));
                match turn {
                    Some(turn) => vec![observed.with_turn(turn)],
                    None => vec![observed],
                }
            }
            CodexRolloutEvent::Message { .. }
            | CodexRolloutEvent::Reasoning { .. }
            | CodexRolloutEvent::TurnContext { .. }
            | CodexRolloutEvent::Compacted => Vec::new(),
            CodexRolloutEvent::TokenUsage { .. } => {
                // c-usagefu (b): one Message-scoped snapshot PER MODEL
                // RESPONSE, keyed on the native response_id. A turn may hold
                // several responses (retries/steers), and the end-of-turn
                // snapshot SUMS them — fine for session totals, but the
                // context ring asks "what does the next request carry", which
                // is the latest single response's input+cache counters.
                // Cumulative `token_count` snapshots (a different source) are
                // ignored here as they are in the aggregator.
                let Some(event) = usage_event.as_ref().filter(|event| {
                    event.source == crate::usage::UsageSource::CodexTokenUsageRecord
                }) else {
                    return Vec::new();
                };
                let Some(response_id) = event.request_id.clone() else {
                    return Vec::new();
                };
                let mut totals = crate::usage::UsageTotals::default();
                totals.add(event);
                // Stamp the native turn id so the Hub rollup can count
                // distinct turns from Message rows (r6 item 5).
                let mut obs = AdapterObservation::structured(ObservationPayload::Usage(Box::new(
                    to_usage_payload(UsageScope::Message, response_id, 1, &totals),
                )))
                // r7 item 7: stamp the record's own timestamp so the Hub
                // native-time rate SQL (60 s / 5 min TPM) and lastTurnAt use
                // the real model-call time instead of ingest time.
                .with_native_at(native_timestamp(record.timestamp.as_deref()));
                if let Some(turn) = event.turn_id.clone().or_else(|| turn_id(&record)) {
                    obs = obs.with_turn(turn);
                }
                vec![obs]
            }
            // The merged parser types the abort; only an explicit interruption
            // is a turn end here — a future/unknown abort reason must not
            // silently mean "interrupted" (A5: the only observed value is
            // "interrupted").
            CodexRolloutEvent::TurnAborted { turn_id, reason } => {
                if reason.as_deref() == Some("interrupted") {
                    self.end_turn(
                        turn_id.as_deref(),
                        "turn_aborted",
                        None,
                        native_timestamp(record.timestamp.as_deref()),
                    )
                } else {
                    Vec::new()
                }
            }
            CodexRolloutEvent::Unknown { .. } => Vec::new(),
        }
    }

    /// Map an `item_completed.item` object.
    fn on_completed_item(
        &mut self,
        turn_id: Option<String>,
        item: &Value,
    ) -> Vec<AdapterObservation> {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            return Vec::new();
        };
        let native_id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("anon:{kind}"));
        let mut out = Vec::new();
        match kind {
            "UserMessage" | "AgentMessage" => {
                let key = format!("msg:{native_id}");
                if self.nodes.contains_key(&key) {
                    return out;
                }
                let id = node_id(&mut self.nodes, &key);
                let role = if kind == "UserMessage" {
                    remuda_protocol::MessageRole::User
                } else {
                    remuda_protocol::MessageRole::Assistant
                };
                let text = item_text(item);
                let mut observed = AdapterObservation::structured(message_payload(id, role, text));
                observed.turn_id = turn_id;
                observed.item_id = Some(native_id);
                out.push(observed);
            }
            "Reasoning" => {
                let key = format!("thought:{native_id}");
                if self.nodes.contains_key(&key) {
                    return out;
                }
                let id = node_id(&mut self.nodes, &key);
                if let Some(text) = reasoning_text(item) {
                    let mut observed = AdapterObservation::structured(thought_payload(id, text));
                    observed.turn_id = turn_id;
                    out.push(observed);
                }
            }
            "CommandExecution" => {
                let call_id = native_id.clone();
                if !self.results.insert(call_id.clone()) {
                    return out;
                }
                // The proposal may have arrived as `function_call` already; if
                // not, announce it from the command item so the result is not
                // orphaned.
                let id = node_id(&mut self.nodes, &format!("call:{call_id}"));
                if !self.announced_tools.contains(&call_id) {
                    self.announced_tools.insert(call_id.clone());
                    let command = command_string(item);
                    let input = command
                        .clone()
                        .map(|command| serde_json::json!({ "command": command }));
                    let mut proposed = AdapterObservation::structured(tool_call_payload(
                        id.clone(),
                        Some("Bash".into()),
                        input,
                    ));
                    proposed.turn_id = turn_id.clone();
                    proposed.item_id = Some(call_id.clone());
                    out.push(proposed);
                }
                let (outcome, exit_code) = command_outcome(item);
                let text = item
                    .get("aggregated_output")
                    .or_else(|| item.get("stdout"))
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned);
                let mut result = AdapterObservation::structured(tool_result_payload(
                    id,
                    text,
                    Some(item.clone()),
                    exit_code,
                    outcome,
                ));
                result.turn_id = turn_id;
                result.item_id = Some(call_id);
                out.push(result);
            }
            _ => {}
        }
        out
    }

    /// Emit an end-of-turn lifecycle plus that turn's usage snapshot.
    fn end_turn(
        &mut self,
        turn_id: Option<&str>,
        name: &'static str,
        last_message: Option<String>,
        native_at: Option<Timestamp>,
    ) -> Vec<AdapterObservation> {
        let Some(turn_id) = turn_id else {
            return Vec::new();
        };
        if !self.ended_turns.insert(turn_id.to_owned()) {
            return Vec::new();
        }
        let mut out = Vec::new();
        // The last assistant message can arrive only with task_complete in
        // sparse rollouts; if no AgentMessage item carried it, still surface
        // it (item-level, complete).
        if let Some(text) = last_message.filter(|text| !text.is_empty()) {
            let key = "msg:task-complete:".to_owned() + turn_id;
            if !self.nodes.contains_key(&key) {
                let id = node_id(&mut self.nodes, &key);
                out.push(
                    AdapterObservation::structured(message_payload(
                        id,
                        remuda_protocol::MessageRole::Assistant,
                        text,
                    ))
                    .with_turn(turn_id),
                );
            }
        }
        out.push(AdapterObservation::structured(turn_lifecycle(name, "idle")).with_turn(turn_id));
        // c-usagefu r6 item 5: codex emits each priced call as a Message
        // usage observation (on_record, see on_line). It must NOT also emit a
        // Turn snapshot of the same totals: the Hub cost rollup treats Message
        // and Turn rows as two billable streams and would double-count
        // (0.10 + 0.20 read as 0.60 on priced/unpriced/priced turns). The
        // composer context basket does not need the Turn row — its turn
        // coverage rule already falls back to Message-scope sums when no Turn
        // rows exist.
        //
        // Session snapshot at turn end (cumulative; cost semantics decided by
        // the Hub rollup).
        self.session_usage_revision += 1;
        let payload = to_usage_payload(
            UsageScope::Session,
            self.binding
                .as_ref()
                .map(|binding| binding.session_id.clone())
                .unwrap_or_else(|| "codex-session".into()),
            self.session_usage_revision,
            self.usage_totals.session(),
        );
        out.push(
            AdapterObservation::structured(ObservationPayload::Usage(Box::new(payload)))
                .with_native_at(native_at),
        );
        out
    }
}

impl Drop for CodexAdapter {
    fn drop(&mut self) {
        // Driver-launch claims (instance_key present) OUTLIVE the adapter: a
        // rollout a closed Remuda instance owned must never be re-bound by a
        // different idle instance (r6 item 1). A PROMOTED attach
        // (instance_key None) owns nothing durable — release its claim with
        // the exact key recorded at claim time (r6 item 7), so a deleted or
        // symlinked file leaves no ghost.
        if self.instance_key.is_none()
            && let Some(binding) = self.binding.take()
            && let Some(key) = self.claim_key.take()
        {
            let _ = binding;
            // Release the EXACT key stored at bind time (r7 item 6) — never
            // re-canonicalize: a removed symlink/file would resolve to a
            // different (or fallback lexical) key and leak the claim.
            crate::adapters::codex_discovery::release_claim_key(&key, &self.lifecycle_owner);
        }
    }
}

impl FileSignalAdapter for CodexAdapter {
    fn kind(&self) -> AgentKind {
        AgentKind::Codex
    }

    fn session_id(&self) -> Option<&str> {
        self.binding
            .as_ref()
            .map(|binding| binding.session_id.as_str())
    }

    fn confirm_session(&mut self, session_id: &str) {
        CodexAdapter::confirm_session(self, session_id);
    }

    fn poll(&mut self) -> DriverResult<Vec<AdapterObservation>> {
        if self.tail.is_none() && !self.discover()? {
            return Ok(Vec::new());
        }
        let Some(tail) = self.tail.as_mut() else {
            return Ok(Vec::new());
        };
        let lines = tail.poll()?;
        let mut out = Vec::new();
        // Physical line index: the tail returns only non-empty lines, so use the
        // parser's own ordinal when present and a running counter otherwise.
        let mut ordinal = 0;
        for line in lines {
            ordinal += 1;
            out.extend(self.on_line(ordinal, &line));
        }
        Ok(out)
    }
}

/// Turn id carried by a record, when present.
fn turn_id(record: &crate::codex_rollout::CodexRolloutRecord) -> Option<String> {
    use crate::codex_rollout::CodexRolloutEvent::*;
    match &record.event {
        TaskStarted { turn_id, .. }
        | TaskComplete { turn_id, .. }
        | ItemCompleted { turn_id, .. }
        | TurnContext { turn_id, .. } => turn_id.clone(),
        _ => None,
    }
}

/// Extract text from a message item, joining its content blocks.
fn item_text(item: &Value) -> String {
    item.get("content")
        .and_then(|content| content.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .or_else(|| block.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Extract visible reasoning text from a reasoning item (summary first).
fn reasoning_text(item: &Value) -> Option<String> {
    let summary = item
        .get("summary")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty());
    summary.or_else(|| Some(item_text(item)).filter(|text| !text.is_empty()))
}

/// Best-effort command string from a `CommandExecution` item.
fn command_string(item: &Value) -> Option<String> {
    item.get("command")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            item.get("command").and_then(Value::as_array).map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| {
                        part.get("command")
                            .and_then(Value::as_str)
                            .or_else(|| part.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        })
}

/// Map a `CommandExecution` status/exit code to outcome + exit code.
fn command_outcome(item: &Value) -> (ToolOutcome, Option<i32>) {
    let exit_code = item
        .get("exit_code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    // A hook rejection is persisted as an error status carrying `Rejected(…)`.
    let rejected = item
        .get("status")
        .and_then(Value::as_object)
        .and_then(|status| status.get("message").and_then(Value::as_str))
        .map(|message| message.contains("Rejected"))
        .unwrap_or(false);
    if rejected {
        return (ToolOutcome::Denied, None);
    }
    match exit_code {
        Some(0) => (ToolOutcome::Succeeded, Some(0)),
        Some(code) => (ToolOutcome::Failed, Some(code)),
        None => (ToolOutcome::Unknown, None),
    }
}

/// Coerce a tool call's arguments into structured JSON when possible.
fn structured_input(name: Option<&str>, input: &Value) -> Option<Value> {
    match input {
        Value::String(text) if !text.is_empty() => serde_json::from_str(text).ok().or_else(|| {
            Some(serde_json::json!({
                "command": text,
            }))
        }),
        Value::Null => name.map(|name| serde_json::json!({ "tool": name })),
        other => Some(other.clone()),
    }
}

/// Newest entry of `$CODEX_HOME/session_index.jsonl`, by file order.
///
/// The index is append-only names (§A4 [V]); the last line is the most recently
/// renamed session, which is the least-bad guess only when no hook confirmed an
/// id. Missing/garbled files yield nothing.
fn newest_indexed(home: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("session_index.jsonl")).ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .and_then(|row| row.get("id").and_then(Value::as_str).map(str::to_owned))
}

#[cfg(test)]
mod tests;
