//! Explicit human replies to a fenced, screen-derived Herdr blocked prompt.
//! A key write proves transport delivery only; clearing the prompt settles it.

use crate::binary::hash_bytes;
use crate::claude_pty::{ObsCtx, emit_on, map_herdr, now_ts};
use crate::{DriverAck, DriverError, DriverResult};
use remuda_herdr::{AgentReadParams, AgentStatus, Client, ReadFormat, ReadSource};
use remuda_protocol::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

const SCREEN_LINES: usize = 32;
const SCREEN_BYTES: usize = 4096;
const ANSWER_BYTES: usize = 1024;

/// `agent.start` can report not-ready after it has created a blocked pane.
/// Reconcile that exact pane once; never replay the launch or auto-dismiss it.
pub(crate) async fn start_agent(
    client: &Client,
    params: remuda_herdr::AgentStartParams,
) -> DriverResult<remuda_herdr::AgentStarted> {
    match client.agent_start(params.clone()).await {
        Ok(started) => Ok(started),
        Err(error @ remuda_herdr::Error::Api { .. }) => {
            if let remuda_herdr::Error::Api { code, .. } = &error
                && matches!(code.as_str(), "agent_not_ready" | "agent_blocked")
                && let Ok(info) = client.agent_get(&params.name).await
                && info.agent.pane_id == params.pane_id
                && info.agent.agent_status == AgentStatus::Blocked
            {
                return Ok(remuda_herdr::AgentStarted {
                    kind: "agent_started".into(),
                    agent: info.agent,
                    argv: params.args,
                });
            }
            Err(map_herdr(error))
        }
        Err(error) => Err(map_herdr(error)),
    }
}

struct Pending {
    interaction: Interaction,
    screen: String,
    screen_truncated: bool,
    state_change_seq: u64,
    keys: BTreeMap<String, Vec<String>>,
    attempted: bool,
}

#[derive(Default)]
struct State {
    pending: Option<Pending>,
    status: Option<AgentStatus>,
    closed: bool,
    trust_attempted: bool,
}

pub(crate) struct PtyInteractions {
    client: Client,
    // Always an exact pane ID, never a mutable herdr agent alias.
    target: String,
    ctx: ObsCtx,
    tx: mpsc::Sender<Observation>,
    seq: Arc<AtomicU64>,
    state: Mutex<State>,
    auto_trust_registered_workspace: bool,
}

impl PtyInteractions {
    pub(crate) async fn observe(&self) -> DriverResult<()> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Ok(());
        }
        self.refresh(&mut state).await
    }
    pub(crate) fn new(
        client: Client,
        target: String,
        ctx: ObsCtx,
        tx: mpsc::Sender<Observation>,
        seq: Arc<AtomicU64>,
        auto_trust_registered_workspace: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            target,
            ctx,
            tx,
            seq,
            state: Mutex::new(State::default()),
            auto_trust_registered_workspace,
        })
    }

    pub(crate) fn spawn(self: &Arc<Self>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let mut state = this.state.lock().await;
                if state.closed || this.tx.is_closed() {
                    break;
                }
                if let Err(error) = this.refresh(&mut state).await {
                    tracing::debug!(%error, "PTY interaction observation unavailable");
                }
            }
        })
    }

    async fn emit(&self, payload: ObservationPayload) -> DriverResult<()> {
        emit_on(
            &self.tx,
            &self.seq,
            &self.ctx,
            SourceChannel::Herdr,
            Completeness::ScreenDerived,
            payload,
        )
        .await
    }

    async fn screen(&self) -> DriverResult<(String, bool)> {
        let read = self
            .client
            .agent_read(AgentReadParams {
                target: self.target.clone(),
                source: ReadSource::Visible,
                lines: Some(SCREEN_LINES as u32),
                format: ReadFormat::Text,
                strip_ansi: true,
            })
            .await
            .map_err(map_herdr)?;
        let truncated = read.truncated.unwrap_or(false)
            || read.read.as_ref().is_some_and(|body| body.truncated);
        Ok((read.text().to_owned(), truncated))
    }

    async fn refresh(&self, state: &mut State) -> DriverResult<()> {
        let info = self
            .client
            .agent_get(&self.target)
            .await
            .map_err(map_herdr)?
            .agent;
        let status = info.agent_status;
        if state.status != Some(status) {
            let label = match status {
                AgentStatus::Blocked => "blocked",
                AgentStatus::Working => "working",
                AgentStatus::Idle | AgentStatus::Done => "idle",
                AgentStatus::Unknown => "unknown",
            };
            self.emit(ObservationPayload::Lifecycle(Box::new(
                LifecyclePayload::Native(Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Turn,
                    native_name: "agent_status".into(),
                    native_id: Knowledge::Known {
                        value: self.target.clone(),
                    },
                    status: Knowledge::Known {
                        value: label.into(),
                    },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: Severity::Info,
                    affects_completion: false,
                })),
            )))
            .await?;
            state.status = Some(status);
        }
        if status != AgentStatus::Blocked {
            if status != AgentStatus::Unknown {
                self.clear(state, InteractionState::Resolved, "native-cleared")
                    .await?;
            }
            return Ok(());
        }
        let (screen, screen_truncated) = self.screen().await?;
        // Check again after reading: never associate an old viewport with a new status epoch.
        let after = self
            .client
            .agent_get(&self.target)
            .await
            .map_err(map_herdr)?
            .agent;
        if after.agent_status != status || after.state_change_seq != info.state_change_seq {
            return Ok(());
        }
        if state.pending.as_ref().is_some_and(|p| {
            p.screen == screen
                && p.screen_truncated == screen_truncated
                && p.state_change_seq == info.state_change_seq
        }) {
            return Ok(());
        }
        self.clear(state, InteractionState::Resolved, "native-cleared")
            .await?;
        let (mut interaction, keys) = screen_request(&self.ctx, &screen)?;
        interaction.answerable &= !screen_truncated;
        self.emit(ObservationPayload::InteractionRequested(Box::new(
            InteractionRequestedPayload {
                interaction: interaction.clone(),
            },
        )))
        .await?;
        state.pending = Some(Pending {
            interaction,
            keys,
            screen,
            screen_truncated,
            state_change_seq: info.state_change_seq,
            attempted: false,
        });
        if self.auto_trust_registered_workspace
            && self.ctx.driver == DriverKind::ClaudePty
            && !state.trust_attempted
            && !screen_truncated
            && let Some(pending) = state.pending.as_mut()
            && pending.interaction.answerable
            && let Some(keys) = trust_dialog_keys(&pending.screen)
        {
            // One automatic attempt per carrier, even if an ACK is lost and the
            // screen changes. Only observed native clearing resolves the dialog.
            state.trust_attempted = true;
            let result = self.commit_answer(pending, keys).await;
            self.emit(ObservationPayload::Lifecycle(Box::new(
                LifecyclePayload::Native(Box::new(NativeLifecycle {
                    topic: LifecycleTopic::Diagnostic,
                    native_name: "trust-dialog".into(),
                    native_id: Knowledge::Known {
                        value: self.target.clone(),
                    },
                    status: Knowledge::Known {
                        value: if result.is_ok() {
                            "trust-dialog auto-accepted (registered workspace)"
                        } else {
                            "trust-dialog automatic answer delivery unknown; not replayed"
                        }
                        .into(),
                    },
                    related_ids: BTreeMap::new(),
                    data_ref: None,
                    severity: if result.is_ok() {
                        Severity::Info
                    } else {
                        Severity::Warning
                    },
                    affects_completion: false,
                })),
            )))
            .await?;
            result?;
        }
        Ok(())
    }

    async fn clear(
        &self,
        state: &mut State,
        next: InteractionState,
        reason: &str,
    ) -> DriverResult<()> {
        let Some(pending) = state.pending.as_mut() else {
            return Ok(());
        };
        let mut interaction = pending.interaction.clone();
        interaction.state = next;
        interaction.blocking = false;
        interaction.answerable = false;
        interaction.meta.revision.0 += 1;
        interaction.meta.updated_at = now_ts()?;
        interaction.delivery = if pending.interaction.delivery == DeliveryState::Unknown {
            DeliveryState::Unknown
        } else if pending.attempted {
            DeliveryState::Written
        } else {
            DeliveryState::NotSent
        };
        interaction.resolution = Knowledge::Known {
            value: InteractionResolution {
                reason: if next == InteractionState::Resolved {
                    InteractionResolutionReason::NativeCleared
                } else {
                    InteractionResolutionReason::GenerationEnded
                },
                event_ids: vec![],
            },
        };
        self.emit(ObservationPayload::Lifecycle(Box::new(
            LifecyclePayload::Entity(Box::new(EntityLifecycle {
                entity_id: interaction.meta.id.as_id().clone(),
                revision: interaction.meta.revision,
                previous_state: Some(
                    if pending.attempted {
                        "answer-committed"
                    } else {
                        "pending"
                    }
                    .into(),
                ),
                state: if next == InteractionState::Resolved {
                    "resolved"
                } else {
                    "invalidated"
                }
                .into(),
                reason_code: reason.into(),
                evidence_event_ids: vec![],
                entity_value: LifecycleEntity::Interaction(Box::new(interaction)),
            })),
        )))
        .await?;
        state.pending = None;
        Ok(())
    }

    /// Both `tty.write` and interaction replies use this exact key transport.
    pub(crate) async fn send_keys(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        let state = self.state.lock().await;
        if state.closed {
            return Err(DriverError::ControlUnavailable);
        }
        self.write(keys).await
    }

    async fn write(&self, keys: Vec<String>) -> DriverResult<DriverAck> {
        self.client
            .agent_send_keys(&self.target, keys)
            .await
            .map_err(map_herdr)?;
        Ok(DriverAck::transport_written())
    }

    pub(crate) async fn respond(
        &self,
        id: InteractionId,
        answer: InteractionAnswer,
    ) -> DriverResult<DriverAck> {
        let mut state = self.state.lock().await;
        if state.closed {
            return Err(DriverError::ControlUnavailable);
        }
        self.refresh(&mut state).await?;
        let pending = state
            .pending
            .as_mut()
            .filter(|p| p.interaction.meta.id == id)
            .ok_or_else(|| invalid("PTY prompt cleared or changed"))?;
        if pending.attempted {
            return Err(invalid("PTY answer already attempted; do not replay"));
        }
        if !pending.interaction.answerable {
            return Err(invalid("PTY screen unavailable or truncated"));
        }
        let keys = answer_keys(pending, &answer)?;
        self.commit_answer(pending, keys).await
    }

    async fn commit_answer(
        &self,
        pending: &mut Pending,
        keys: Vec<String>,
    ) -> DriverResult<DriverAck> {
        // Mark before I/O: even a lost RPC ACK must never replay Enter.
        pending.attempted = true;
        let result = self.write(keys).await;
        pending.interaction.delivery = if result.is_ok() {
            DeliveryState::Written
        } else {
            DeliveryState::Unknown
        };
        pending.interaction.state = InteractionState::AnswerCommitted;
        pending.interaction.answerable = false;
        pending.interaction.meta.revision.0 += 1;
        pending.interaction.meta.updated_at = now_ts()?;
        self.emit(ObservationPayload::Lifecycle(Box::new(
            LifecyclePayload::Entity(Box::new(EntityLifecycle {
                entity_id: pending.interaction.meta.id.as_id().clone(),
                revision: pending.interaction.meta.revision,
                previous_state: Some("pending".into()),
                state: "answer-committed".into(),
                reason_code: if result.is_ok() {
                    "tty-written"
                } else {
                    "tty-write-unknown"
                }
                .into(),
                evidence_event_ids: vec![],
                entity_value: LifecycleEntity::Interaction(Box::new(pending.interaction.clone())),
            })),
        )))
        .await?;
        result
    }

    pub(crate) async fn close(&self) -> DriverResult<()> {
        let closing = async {
            let mut state = self.state.lock().await;
            state.closed = true;
            self.clear(
                &mut state,
                InteractionState::Invalidated,
                "generation-ended",
            )
            .await
        };
        // Match carrier shutdown: a detached/stalled optional observer cannot
        // prevent reclamation of the owned Herdr resources.
        match tokio::time::timeout(Duration::from_millis(250), closing).await {
            Ok(result) if !self.tx.is_closed() => result,
            _ => Ok(()),
        }
    }
}

fn invalid(message: &str) -> DriverError {
    DriverError::InvalidLaunchSpec(message.into())
}

/// Prompt readiness is a pre-dispatch query. A busy/blocked pane is safe to queue.
pub(crate) async fn prompt_ready(client: &Client, target: &str) -> DriverResult<()> {
    let agent = client.agent_get(target).await.map_err(map_herdr)?.agent;
    if !matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done)
        || !agent.interactive_ready
    {
        return Err(DriverError::ControlUnavailable);
    }
    Ok(())
}

/// Only Claude's exact folder question with both known choices and one cursor.
fn trust_dialog_keys(screen: &str) -> Option<Vec<String>> {
    let normalized = screen.split_whitespace().collect::<Vec<_>>().join(" ");
    if !normalized.contains("Quick safety check:")
        || !normalized.contains("Is this a project you created or one you trust?")
    {
        return None;
    }
    let mut choices = Vec::new();
    let mut selected = None;
    for line in screen.lines() {
        let line = line.trim();
        let marked = line.starts_with(['❯', '›', '>']);
        let label = line.trim_start_matches(['❯', '›', '>']).trim();
        // Numbered versions of the same Claude menu use the same cursor keys.
        let label = label
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')'])
            .trim();
        if matches!(label, "No, exit" | "Yes, I trust this folder") {
            if marked && selected.replace(choices.len()).is_some() {
                return None;
            }
            choices.push(label);
        } else if marked {
            return None;
        }
    }
    if choices.len() != 2 || choices[0] == choices[1] {
        return None;
    }
    let selected = selected?;
    let yes = choices
        .iter()
        .position(|label| *label == "Yes, I trust this folder")?;
    let mut keys = vec![
        if yes < selected {
            "up".into()
        } else {
            "down".into()
        };
        yes.abs_diff(selected)
    ];
    keys.push("enter".into());
    Some(keys)
}

fn answer_keys(pending: &Pending, answer: &InteractionAnswer) -> DriverResult<Vec<String>> {
    let selected = match (&pending.interaction.request, answer) {
        (InteractionRequest::Approval(request), InteractionAnswer::Approval(answer)) => {
            if request.input_digest != answer.input_digest {
                return Err(invalid("PTY input digest mismatch"));
            }
            answer.option_id.as_str()
        }
        (InteractionRequest::Question(request), InteractionAnswer::Question(answer)) => {
            let field = &request.fields[0];
            if answer.answers.len() != 1 {
                return Err(invalid("PTY reply requires exactly one field"));
            }
            let value = answer
                .answers
                .get(&field.id)
                .ok_or_else(|| invalid("unknown PTY question field"))?;
            if field.input == QuestionInput::Text {
                let text = value
                    .text
                    .as_deref()
                    .ok_or_else(|| invalid("PTY reply requires text"))?;
                if !value.option_ids.is_empty()
                    || text.is_empty()
                    || text.len() > ANSWER_BYTES
                    || text.chars().any(char::is_control)
                {
                    return Err(invalid(
                        "PTY reply must be a nonempty single line of at most 1024 bytes",
                    ));
                }
                let mut keys: Vec<_> = text.chars().map(|c| c.to_string()).collect();
                keys.push("enter".into());
                return Ok(keys);
            }
            if value.option_ids.len() != 1 || value.text.is_some() {
                return Err(invalid("PTY reply requires one option and no text"));
            }
            &value.option_ids[0]
        }
        _ => return Err(invalid("PTY answer kind mismatch")),
    };
    pending
        .keys
        .get(selected)
        .cloned()
        .ok_or_else(|| invalid("unknown PTY answer option"))
}

fn excerpt(screen: &str) -> (String, bool) {
    let lines: Vec<_> = screen.lines().collect();
    let tail = lines[lines.len().saturating_sub(SCREEN_LINES)..].join("\n");
    let mut start = tail.len().saturating_sub(SCREEN_BYTES);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    let truncated = start != 0 || lines.len() > SCREEN_LINES;
    (tail[start..].trim().to_owned(), truncated)
}

pub(crate) fn screen_request(
    ctx: &ObsCtx,
    screen: &str,
) -> DriverResult<(Interaction, BTreeMap<String, Vec<String>>)> {
    let (excerpt, truncated) = excerpt(screen);
    let lower = excerpt.to_lowercase();
    let mut choices: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut approval;
    if ["[y/n]", "(y/n)", "[yes/no]", "(yes/no)"]
        .iter()
        .any(|s| lower.contains(s))
    {
        approval = true;
        choices = vec![
            (
                "y".into(),
                "Yes (y)".into(),
                vec!["y".into(), "enter".into()],
            ),
            (
                "n".into(),
                "No (n)".into(),
                vec!["n".into(), "enter".into()],
            ),
        ];
    } else {
        let mut selected = None;
        for line in excerpt.lines() {
            let line = line.trim();
            let marked = line.starts_with(['❯', '›', '>']);
            let line = line.trim_start_matches(['❯', '›', '>']).trim_start();
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 2 {
                continue;
            }
            let rest = &line[digits..];
            if !rest.starts_with(['.', ')', ':']) {
                continue;
            }
            let label = rest[1..].trim();
            if label.is_empty() {
                continue;
            }
            if marked {
                selected = Some(choices.len());
            }
            let id = line[..digits].to_owned();
            let mut keys: Vec<_> = id.chars().map(|c| c.to_string()).collect();
            keys.push("enter".into());
            choices.push((id, label.to_owned(), keys));
        }
        if choices.len() < 2 || choices.len() > 12 {
            choices.clear();
        }
        if let Some(selected) = selected {
            for (index, (_, _, keys)) in choices.iter_mut().enumerate() {
                *keys = vec![
                    if index < selected {
                        "up".into()
                    } else {
                        "down".into()
                    };
                    index.abs_diff(selected)
                ];
                keys.push("enter".into());
            }
        }
        if choices.is_empty()
            && (lower.contains("enter to continue") || lower.contains("press enter to continue"))
        {
            choices.push((
                "enter".into(),
                "Enter to continue".into(),
                vec!["enter".into()],
            ));
        }
        approval = !choices.is_empty()
            && [
                "approve",
                "approval",
                "permission",
                "allow",
                "trust",
                "proceed",
                "run this command",
            ]
            .iter()
            .any(|word| lower.contains(word));
    }
    let mut keys = BTreeMap::new();
    let mut ambiguous = false;
    for (id, _, value) in &choices {
        ambiguous |= keys.insert(id.clone(), value.clone()).is_some();
    }
    if ambiguous {
        keys.clear();
        choices.clear();
        approval = false;
    }
    let request = if approval {
        InteractionRequest::Approval(Box::new(ApprovalRequest {
            title: "Terminal approval".into(),
            description: excerpt.clone(),
            tool_call_id: None,
            action_ref: Id::new("obj")?,
            requested_permissions_ref: None,
            input_digest: hash_bytes(screen.as_bytes())?,
            options: choices
                .iter()
                .map(|(id, label, _)| {
                    Ok(DecisionOption {
                        id: id.clone(),
                        label: label.clone(),
                        effect: DecisionEffect::NativeSpecific,
                        native_value_ref: Id::new("obj")?,
                    })
                })
                .collect::<DriverResult<_>>()?,
        }))
    } else {
        InteractionRequest::Question(Box::new(QuestionRequest {
            title: "Terminal question".into(),
            fields: vec![QuestionField {
                id: "screen".into(),
                title: "Reply to the terminal prompt".into(),
                description: Some(excerpt.clone()),
                input: if choices.is_empty() {
                    QuestionInput::Text
                } else {
                    QuestionInput::SingleSelect
                },
                required: true,
                options: choices
                    .iter()
                    .map(|(id, label, _)| QuestionOption {
                        id: id.clone(),
                        label: label.clone(),
                    })
                    .collect(),
                allow_free_text: choices.is_empty(),
                sensitive: false,
            }],
        }))
    };
    let ts = now_ts()?;
    Ok((
        Interaction {
            meta: EntityMeta {
                id: InteractionId::new(),
                revision: U64(1),
                created_at: ts.clone(),
                updated_at: ts,
            },
            instance_id: ctx.instance_id.clone(),
            host_id: ctx.host_id.clone(),
            run_id: Some(ctx.run_id.clone()),
            kind: if approval {
                InteractionKind::Approval
            } else {
                InteractionKind::Question
            },
            request_key: InteractionRequestKey {
                native: NativeRequestKey::None,
                process_generation: U64(1),
                run_generation: Some(U64(1)),
                connection_epoch: Id::new("epoch")?,
            },
            request_version: U64(1),
            state: InteractionState::Pending,
            blocking: true,
            answerable: !truncated && !ambiguous && !excerpt.is_empty(),
            carrier: InteractionCarrier::NativeTty,
            request,
            deadline: Knowledge::Unknown {
                reason: "native-tty".into(),
                evidence_event_ids: vec![],
            },
            deadline_source: DeadlineSource::None,
            answer: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
            delivery: DeliveryState::NotSent,
            resolution: Knowledge::Unknown {
                reason: "pending".into(),
                evidence_event_ids: vec![],
            },
        },
        keys,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx() -> ObsCtx {
        crate::claude_pty::obs_ctx(
            DriverKind::GenericPty,
            InstanceId::new(),
            HostId::new(),
            Id::new("obj").unwrap(),
            RunId::new(),
            "fixture".into(),
            "fixture".into(),
        )
    }
    #[tokio::test]
    async fn closing_blocked_prompt_does_not_wait_for_optional_observer() {
        for detached in [false, true] {
            let (tx, rx) = mpsc::channel(1);
            let interactions = PtyInteractions::new(
                Client::connect("/tmp/unused-pty-interaction-test.sock"),
                "pane-fixture".into(),
                ctx(),
                tx,
                Arc::new(AtomicU64::new(0)),
                false,
            );
            let screen = "Continue? [y/n]";
            let (interaction, keys) = screen_request(&interactions.ctx, screen).unwrap();
            interactions
                .emit(ObservationPayload::InteractionRequested(Box::new(
                    InteractionRequestedPayload {
                        interaction: interaction.clone(),
                    },
                )))
                .await
                .unwrap();
            interactions.state.lock().await.pending = Some(Pending {
                interaction,
                screen: screen.into(),
                screen_truncated: false,
                state_change_seq: 1,
                keys,
                attempted: false,
            });
            let _observer = if detached {
                drop(rx);
                None
            } else {
                Some(rx)
            };
            tokio::time::timeout(Duration::from_secs(1), interactions.close())
                .await
                .expect("observer cannot block carrier reclamation")
                .unwrap();
            assert!(interactions.state.lock().await.closed);
            assert!(interactions.send_keys(vec!["enter".into()]).await.is_err());
        }
    }
    #[test]
    fn exact_trust_dialog_requires_unambiguous_selected_menu() {
        let title = "Quick safety check: Is this a project you created or one you trust?";
        assert_eq!(
            trust_dialog_keys(&format!("{title}\n❯ No, exit\nYes, I trust this folder")),
            Some(vec!["down".into(), "enter".into()])
        );
        assert_eq!(
            trust_dialog_keys(&format!("{title}\n❯ Yes, I trust this folder\nNo, exit")),
            Some(vec!["enter".into()])
        );
        for screen in [
            "Do you trust this command?\n❯ No, exit\nYes, I trust this folder".to_owned(),
            format!("{title}\nNo, exit\nYes, I trust this folder"),
            format!("{title}\n❯ No, exit\n❯ Yes, I trust this folder"),
            format!("{title}\n❯ No, exit\nYes, I trust this folder\nNo, exit"),
        ] {
            assert!(trust_dialog_keys(&screen).is_none());
        }
    }

    #[test]
    fn bounded_excerpt_is_utf8_safe_and_disables_truncated_reply() {
        assert!(
            !screen_request(&ctx(), "Choose:\n1. First\n1. Duplicate")
                .unwrap()
                .0
                .answerable
        );
        let screen = "界".repeat(3000);
        let (interaction, _) = screen_request(&ctx(), &screen).unwrap();
        assert!(!interaction.answerable);
        let InteractionRequest::Question(request) = interaction.request else {
            panic!()
        };
        assert!(request.fields[0].description.as_ref().unwrap().len() <= SCREEN_BYTES);
    }
    #[test]
    fn exact_candidates_and_control_input_validation() {
        let (interaction, keys) =
            screen_request(&ctx(), "Select environment:\n❯ 1. Development\n2. Staging").unwrap();
        assert_eq!(keys["1"], ["enter"]);
        assert_eq!(keys["2"], ["down", "enter"]);
        assert_eq!(interaction.kind, InteractionKind::Question);
        let (interaction, keys) = screen_request(&ctx(), "Type a project name:").unwrap();
        let pending = Pending {
            interaction,
            screen: String::new(),
            screen_truncated: false,
            state_change_seq: 1,
            keys,
            attempted: false,
        };
        let answer = InteractionAnswer::Question(Box::new(QuestionAnswer {
            answers: BTreeMap::from([(
                "screen".into(),
                QuestionFieldAnswer {
                    option_ids: vec![],
                    text: Some("bad\ncommand".into()),
                },
            )]),
        }));
        assert!(answer_keys(&pending, &answer).is_err());
    }
}
