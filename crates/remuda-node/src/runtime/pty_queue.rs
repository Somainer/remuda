//! Bounded PTY prompt delivery; control operations remain usable while input waits.

use super::*;
use crate::DriverError;
use remuda_protocol::{ContentStatus, DriverKind, MessagePayload, MutationOperation};
use std::{collections::VecDeque, time::Duration};

struct PendingPrompt {
    command_id: CommandId,
    prompt: String,
    /// Hub refs staged at accept time (D-027). Bytes are pulled in this worker
    /// when the prompt reaches the front of the queue, so a slow fetch never
    /// delays the RPC ack — only the prompt it belongs to.
    attachment_refs: Vec<remuda_protocol::hubnode::AttachmentRef>,
    origin: remuda_protocol::InputOrigin,
    /// c-steer: new-turn/queue prompts wait for the next turn boundary; a
    /// steer waits for the running turn to be interrupted (`Activity::Idle`),
    /// then jumps ahead of every other queued prompt.
    mode: remuda_protocol::PromptMode,
    message: Box<MessagePayload>,
}

pub(super) fn is_pty(kind: DriverKind) -> bool {
    matches!(
        kind,
        DriverKind::ClaudePty | DriverKind::GenericPty | DriverKind::ShellPty
    )
}

pub(super) async fn run(
    store: Arc<dyn LocalStore>,
    instance_id: InstanceId,
    driver: Arc<dyn Driver>,
    mut receiver: mpsc::Receiver<QueuedCommand>,
    interactions: Arc<InteractionRuntime>,
    create: Option<(Command, String)>,
    carrier: Option<crate::carrier_recovery::CarrierSupervisor>,
    loader: super::AttachmentLoader,
    prompts: Arc<crate::prompt_correlation::PromptCorrelator>,
) -> Result<(), NodeError> {
    let capacity = receiver.max_capacity();
    let mut pending = VecDeque::new();
    if let Some((mut command, prompt)) = create {
        if !prompt.is_empty() {
            pending.push_back(enqueue(
                store.as_ref(),
                &instance_id,
                command.command_id.clone(),
                prompt,
                // A create-time prompt never carries attachments (D-027).
                Vec::new(),
                crate::origin::input_origin(command.origin),
                remuda_protocol::PromptMode::NewTurn,
                prompts.as_ref(),
            )?);
        }
        // Creating the live runtime and delivering its first input are separate facts.
        settle_without_driver(store.as_ref(), &instance_id, &mut command)?;
        // The create prompt waits for the harness to come up; project the
        // waiting state the queue used to mark inside `enqueue`.
        remark_after_queue_change(store.as_ref(), &instance_id, pending.front())?;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // c-steer: when a 插队 arrives mid-turn the worker sends the harness Esc
    // itself and holds the steer at the head until turn-ended/idle evidence
    // lands — bounded, so a missed signal still delivers instead of sticking
    // the prompt in 排队 forever.
    let mut steer_deadline: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            queued = receiver.recv() => {
                let Some(queued) = queued else {
                    interrupt_pending(store.as_ref(), &instance_id, &mut pending, "instance worker closed", prompts.as_ref())?;
                    return Err(NodeError::DriverUnavailable);
                };
                if let DriverRequest::Send { prompt, attachments: _, origin, mode } = queued.request {
                    if pending.len() == capacity {
                        let command = store.get_command(&queued.command_id)?;
                        prompts.cancel(&queued.command_id);
                        reject_before_dispatch(store.as_ref(), &instance_id, command, "instance-queue-full")?;
                        remark_after_queue_change(store.as_ref(), &instance_id, pending.front())?;
                    } else {
                        let was_working = store.get_instance(&instance_id)?.activity
                            == (Knowledge::Known { value: Activity::Working });
                        let item = enqueue(store.as_ref(), &instance_id, queued.command_id, prompt, queued.attachment_refs, origin, mode, prompts.as_ref())?;
                        if mode == remuda_protocol::PromptMode::Steer {
                            // c-steer: jump ahead of every ORDINARY queued
                            // prompt, but keep FIFO among consecutive steers —
                            // the second 插队 must not overtake the first.
                            let slot = pending
                                .iter()
                                .position(|p| p.mode != remuda_protocol::PromptMode::Steer)
                                .unwrap_or(pending.len());
                            pending.insert(slot, item);
                            let blocked_on_human = was_blocked_on_human(
                                store.as_ref(),
                                interactions.as_ref(),
                                &instance_id,
                            )
                            .await;
                            if was_working && steer_deadline.is_none() {
                                // Atomic interrupt-then-deliver: one Esc through
                                // the driver's own key path, journaled as the
                                // reason the running turn ends. The steer goes
                                // out on a later tick once the turn ends.
                                arm_steer_interrupt(
                                    store.as_ref(),
                                    &instance_id,
                                    driver.as_ref(),
                                    pending.front().expect("just inserted"),
                                ).await?;
                                steer_deadline =
                                    Some(std::time::Instant::now() + STEER_INTERRUPT_BUDGET);
                            } else if !blocked_on_human && steer_deadline.is_none() {
                                // Not a running turn and no dialog waiting: the
                                // next tick writes directly (wait_control still
                                // probes). No Esc — there is nothing to cancel,
                                // and a stray Esc at a startup/control dialog
                                // would dismiss it.
                                steer_deadline = Some(std::time::Instant::now());
                            }
                            // Blocked on a real interaction: hold behind the
                            // dialog until it is answered; no deadline.
                        } else {
                            pending.push_back(item);
                        }
                        remark_after_enqueue(store.as_ref(), &instance_id, pending.front().map(|p| p.mode))?;
                    }
                } else {
                    let close_after = queued.close_after;
                    if matches!(queued.request, DriverRequest::Cancel | DriverRequest::Close) {
                        interrupt_pending(store.as_ref(), &instance_id, &mut pending, "queued input cancelled", prompts.as_ref())?;
                    }
                    execute_queued(store.clone(), &instance_id, driver.clone(), queued, interactions.clone(), carrier.clone(), loader.clone(), Arc::clone(&prompts)).await?;
                    if close_after && store.get_instance(&instance_id)?.lifecycle == InstanceLifecycle::Exited {
                        // The Instance is exited, so nothing may still hold a
                        // pane in the operator's Herdr session. The driver's
                        // own close() covers the panes it tracks in memory;
                        // this covers the durable ownership rows, which are
                        // what survive a rebuilt or adopted driver (DEFECT B).
                        crate::reclaim::reclaim_instance_carriers(store.as_ref(), &instance_id).await;
                        receiver.close();
                        while let Some(queued) = receiver.recv().await {
                            let command = store.get_command(&queued.command_id)?;
                            prompts.cancel(&queued.command_id);
                            reject_before_dispatch(store.as_ref(), &instance_id, command, "instance closed")?;
                        }
                        return Ok(());
                    }
                    if !pending.is_empty() {
                        remark_after_queue_change(store.as_ref(), &instance_id, pending.front())?;
                    }
                }
            }
            _ = tick.tick(), if !pending.is_empty() => {
                if store.get_instance(&instance_id)?.lifecycle != InstanceLifecycle::Ready {
                    interrupt_pending(store.as_ref(), &instance_id, &mut pending, "instance no longer ready", prompts.as_ref())?;
                    continue;
                }
                let front = pending.front().expect("guarded by non-empty tick");
                // c-steer: the interrupted turn must actually have ended before
                // a steer is written. `wait_control` alone is not enough — at a
                // tool boundary it can answer while the turn is still running.
                // The Esc's idle/turn-ended evidence folds into instance
                // activity; until it lands, the steer holds at the head (ahead
                // of the FIFO queue) and is retried on the next tick. A pending
                // human interaction (question/approval/elicitation/plan review)
                // gates the same way: steer bytes must never land inside a
                // dialog — the composer answers those through the interaction.
                // Past the interrupt budget a missed idle signal may no longer
                // block: `deliver` still probes control readiness, and a
                // harness that absorbs the text natively (claude's tool-
                // boundary queue) does the right thing with it.
                let turn_still_busy = matches!(
                    store.get_instance(&instance_id)?.activity,
                    Knowledge::Known {
                        value: Activity::Working
                    }
                );
                let dialog_open = was_blocked_on_human(store.as_ref(), interactions.as_ref(), &instance_id).await;
                let budget_passed = steer_deadline.is_some_and(|deadline| {
                    std::time::Instant::now() >= deadline
                });
                let steer_gated = front.mode == remuda_protocol::PromptMode::Steer
                    && ((turn_still_busy && !budget_passed) || dialog_open);
                if !steer_gated
                    && let Some(prompt) = pending.front_mut()
                    && deliver(store.as_ref(), &instance_id, driver.as_ref(), &interactions, prompt, carrier.as_ref(), &loader, prompts.as_ref()).await?
                {
                    if prompt.mode == remuda_protocol::PromptMode::Steer {
                        steer_deadline = None;
                    }
                    pending.pop_front();
                    // Two 插队 can queue back-to-back: delivering the first
                    // starts a new turn (Working). The steer behind it still
                    // needs its own Esc, so arm the interrupt and budget here
                    // rather than leaving it gated forever.
                    if let Some(next) = pending.front()
                        && next.mode == remuda_protocol::PromptMode::Steer
                        && store.get_instance(&instance_id)?.activity
                            == (Knowledge::Known { value: Activity::Working })
                    {
                        arm_steer_interrupt(
                            store.as_ref(),
                            &instance_id,
                            driver.as_ref(),
                            next,
                        )
                        .await?;
                        steer_deadline =
                            Some(std::time::Instant::now() + STEER_INTERRUPT_BUDGET);
                    }
                }
            }
        }
    }
}

fn enqueue(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    command_id: CommandId,
    prompt: String,
    attachment_refs: Vec<remuda_protocol::hubnode::AttachmentRef>,
    origin: remuda_protocol::InputOrigin,
    mode: remuda_protocol::PromptMode,
    prompts: &crate::prompt_correlation::PromptCorrelator,
) -> Result<PendingPrompt, NodeError> {
    let ObservationPayload::Message(mut message) = crate::driver::message_payload(
        MessageRole::User,
        MessagePhase::Input,
        prompt.clone(),
        Vec::new(),
    )?
    else {
        return Err(NodeError::InvalidRequest("expected user message".into()));
    };
    message.status = ContentStatus::Queued;
    // c-steer: the queued row remembers whether it is a normal turn or a 插队,
    // so the transcript tag survives revision replaces (Queued → Complete).
    message.prompt_mode = Some(mode);
    // C2: the queued message is the server-command-authored copy. Its
    // commandId lets the web upgrade the optimistic bubble in place the
    // moment this observation arrives, and the correlator joins the
    // subsequent hook/transcript evidence onto this same node.
    let node_id = message.mutation.node_id.clone();
    message.command_id = Some(command_id.clone());
    store.append_observation(
        instance_id,
        None,
        Completeness::Structured,
        ObservationPayload::Message(message.clone()),
    )?;
    prompts.register(command_id.clone(), node_id, prompt.clone());
    // NOTE: instance activity is re-projected by the caller
    // (`remark_after_enqueue`), never from inside `enqueue`: the right state
    // depends on the queued prompt's mode and on whatever sits at the head.
    Ok(PendingPrompt {
        command_id,
        prompt,
        attachment_refs,
        origin,
        mode,
        message,
    })
}

/// Whether a `waiting-interaction` activity is backed by a REAL pending
/// interaction (question/approval/elicitation/plan review). The queue also
/// projects `waiting-interaction` for ordinary prompts parked at a turn
/// boundary ("Remuda-held"), and that state must not gate a steer like a
/// dialog — only the broker's pending table proves a human is actually being
/// asked.
async fn was_blocked_on_human(
    store: &dyn LocalStore,
    interactions: &InteractionRuntime,
    instance_id: &InstanceId,
) -> bool {
    let blocked_on_human = store
        .get_instance(instance_id)
        .map(|instance| {
            instance.activity
                == (Knowledge::Known {
                    value: Activity::WaitingInteraction,
                })
        })
        .unwrap_or(false);
    blocked_on_human && !interactions.list(Some(instance_id), None).await.is_empty()
}

/// Wait at most this long for turn-ended/idle evidence after a steer's Esc.
const STEER_INTERRUPT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// c-steer: send the harness interrupt key (Esc on claude/codex; the driver
/// picks the per-harness sequence) and journal why the running turn ends. A
/// failed Esc is ledgered but never fails the steer command: the bounded gate
/// in `run` still delivers once the turn ends, and a harness that natively
/// absorbs text mid-turn queues it at the next tool boundary.
async fn arm_steer_interrupt(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    driver: &dyn Driver,
    steer: &PendingPrompt,
) -> Result<(), NodeError> {
    let mut related = std::collections::BTreeMap::new();
    related.insert(
        "origin".into(),
        format!("{:?}", steer.origin).to_lowercase(),
    );
    related.insert("reason".into(), "user-steer".into());
    related.insert("commandId".into(), steer.command_id.as_id().to_string());
    let (status, severity) = match tokio::time::timeout(
        Duration::from_secs(1),
        driver.execute(DriverRequest::Cancel),
    )
    .await
    {
        Ok(Ok(_)) => (
            "esc-dispatched".to_string(),
            remuda_protocol::Severity::Info,
        ),
        Ok(Err(error)) => (
            format!("esc-failed: {error}"),
            remuda_protocol::Severity::Warning,
        ),
        Err(_) => (
            "esc-timeout".to_string(),
            remuda_protocol::Severity::Warning,
        ),
    };
    let diagnostic = DriverEmission::NativeLifecycle {
        name: "turn-interrupted".into(),
        status,
        severity,
    };
    if let Ok(mut payload) = diagnostic.into_payload() {
        if let ObservationPayload::Lifecycle(lifecycle) = &mut payload
            && let remuda_protocol::LifecyclePayload::Native(native) = lifecycle.as_mut()
        {
            native.related_ids = related;
        }
        store.append_observation(instance_id, None, Completeness::Structured, payload)?;
    }
    Ok(())
}

/// Activity projected by the queue while prompts are waiting, keyed off the
/// head prompt's mode.
///
/// * a running turn stays `Working` — queued input never overwrites turn
///   evidence, and a steer gated behind the turn is part of that turn;
/// * a pending interaction (`WaitingInteraction`) is preserved: the composer
///   shows the question, the queue must not blur that into "work";
/// * a steer at the head keeps whatever non-working state the interrupt left
///   (`Idle`) — the Esc has freed the composer and the next tick writes the
///   prompt, so re-marking the instance blocked would stall it forever;
/// * any ordinary queued prompt with nothing running and no question pending
///   reads as `WaitingInteraction` ("blocked on the next turn boundary") —
///   the legacy meaning of a queued follow-up.
fn remark_after_enqueue(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    front_mode: Option<remuda_protocol::PromptMode>,
) -> Result<(), NodeError> {
    let activity = store.get_instance(instance_id)?.activity;
    if matches!(
        activity,
        Knowledge::Known {
            value: Activity::Working
        } | Knowledge::Known {
            value: Activity::WaitingInteraction
        }
    ) {
        return Ok(());
    }
    if front_mode == Some(remuda_protocol::PromptMode::Steer) {
        return Ok(());
    }
    store
        .set_instance_state(
            instance_id,
            None,
            Some(Knowledge::Known {
                value: Activity::WaitingInteraction,
            }),
        )
        .map(|_| ())
}

/// Re-project activity after the queue contents changed (reject, control op).
fn remark_after_queue_change(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    front: Option<&PendingPrompt>,
) -> Result<(), NodeError> {
    let Some(front) = front else {
        return Ok(());
    };
    remark_after_enqueue(store, instance_id, Some(front.mode))
}

async fn deliver(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    driver: &dyn Driver,
    interactions: &InteractionRuntime,
    prompt: &mut PendingPrompt,
    carrier: Option<&crate::carrier_recovery::CarrierSupervisor>,
    loader: &super::AttachmentLoader,
    prompts: &crate::prompt_correlation::PromptCorrelator,
) -> Result<bool, NodeError> {
    // D-027: bytes are pulled only when the prompt reaches the front of the
    // queue, so a slow Hub object store delays this prompt and nothing else.
    let attachments = match loader
        .resolve(instance_id, prompt.attachment_refs.clone())
        .await
    {
        Ok(attachments) => attachments,
        Err(error) => {
            update_message(store, instance_id, prompt, ContentStatus::Interrupted)?;
            prompts.cancel(&prompt.command_id);
            let command = store.get_command(&prompt.command_id)?;
            if command.state != CommandState::Settled {
                reject_before_dispatch(
                    store,
                    instance_id,
                    command,
                    &format!("attachment materialization failed: {error}"),
                )?;
            }
            return Ok(true);
        }
    };
    // Build journal metadata blocks before the send moves the materialized
    // vec. The completed user message (below) records the absolute paths.
    let landed_blocks = crate::attachments::content_blocks(&attachments);
    // Cancelling a readiness probe cannot submit input. Never time out or replay send().
    let execution =
        match tokio::time::timeout(Duration::from_millis(100), driver.wait_control()).await {
            Err(_) | Ok(Err(DriverError::ControlUnavailable)) => {
                // Transient probe failure: the prompt stays queued and the
                // projected activity already describes reality (a running turn
                // is Working; a dialog or queued follow-up was marked when it
                // entered the queue). Never mark anything here — a steer
                // waiting for its interrupted turn to end must stay gated.
                return Ok(false);
            }
            Ok(Err(error)) => Err(error),
            Ok(Ok(_)) => {
                store.set_instance_state(
                    instance_id,
                    None,
                    Some(Knowledge::Known {
                        value: Activity::Working,
                    }),
                )?;
                driver
                    .execute(DriverRequest::Send {
                        prompt: prompt.prompt.clone(),
                        attachments,
                        origin: prompt.origin,
                        mode: prompt.mode,
                    })
                    .await
            }
        };
    let mut command = store.get_command(&prompt.command_id)?;
    match execution {
        Err(DriverError::ControlUnavailable) => {
            remark_after_enqueue(store, instance_id, Some(prompt.mode))?;
            return Ok(false);
        }
        Ok(emissions) => {
            // D-027b: now that the bytes have landed, record the absolute
            // paths as journal metadata on the completed user message. The
            // queued placeholder (pre-resolve) deliberately stays text-only.
            if !landed_blocks.is_empty() {
                let text_blocks = std::mem::take(&mut prompt.message.blocks);
                prompt.message.blocks = landed_blocks;
                prompt.message.blocks.extend(text_blocks);
            }
            update_message(store, instance_id, prompt, ContentStatus::Complete)?;
            // c-steer: journal the interrupt-and-jump as its own ledger event so
            // the transcript can show「已打断」on the turn it replaced and
            // attribute the new prompt (origin + reason), independent of the
            // native harness emitting anything.
            if prompt.mode == remuda_protocol::PromptMode::Steer {
                let mut related = std::collections::BTreeMap::new();
                related.insert(
                    "origin".into(),
                    format!("{:?}", prompt.origin).to_lowercase(),
                );
                related.insert("reason".into(), "interrupted-current-turn".into());
                related.insert("commandId".into(), prompt.command_id.as_id().to_string());
                let steer_event = DriverEmission::NativeLifecycle {
                    name: "prompt-steer".into(),
                    status: "delivered-ahead-of-queue".into(),
                    severity: remuda_protocol::Severity::Info,
                };
                if let Ok(mut payload) = steer_event.into_payload() {
                    if let ObservationPayload::Lifecycle(lifecycle) = &mut payload
                        && let remuda_protocol::LifecyclePayload::Native(native) =
                            lifecycle.as_mut()
                    {
                        native.related_ids = related;
                    }
                    store.append_observation(
                        instance_id,
                        None,
                        Completeness::Structured,
                        payload,
                    )?;
                }
            }
            for emission in emissions {
                let observation = store.append_observation(
                    instance_id,
                    None,
                    Completeness::Structured,
                    emission.into_payload()?,
                )?;
                interactions.ingest(&observation).await?;
            }
            if command.state != CommandState::Settled {
                settle_command(
                    &mut command,
                    SettlementOutcome::Completed,
                    None,
                    remuda_protocol::ExecutionState::PossiblyDispatched,
                )?;
            }
        }
        Err(error) => {
            // A failed/uncertain send is never replayed and does not invalidate a live PTY.
            // The bytes may or may not have reached the harness, so drop the
            // correlation: claiming the later evidence would be guessing.
            prompts.cancel(&prompt.command_id);
            // A carrier loss is the exception worth acting on: the session
            // server, not this prompt, is what needs restarting.
            super::notify_carrier(carrier, &error);
            update_message(store, instance_id, prompt, ContentStatus::Interrupted)?;
            let diagnostic = DriverEmission::NativeLifecycle {
                name: "pty-prompt-error".into(),
                status: error.to_string(),
                severity: remuda_protocol::Severity::Error,
            };
            store.append_observation(
                instance_id,
                None,
                Completeness::Structured,
                diagnostic.into_payload()?,
            )?;
            store.set_instance_state(
                instance_id,
                None,
                Some(unknown("prompt-dispatch-uncertain")),
            )?;
            if command.state != CommandState::Settled {
                settle_command(
                    &mut command,
                    SettlementOutcome::Rejected,
                    Some(error.to_string()),
                    remuda_protocol::ExecutionState::PossiblyDispatched,
                )?;
            }
        }
    }
    // The already-settled create is immutable even if its optional prompt later fails.
    if command.operation != CommandOperation::InstanceCreate {
        store.save_command(command.clone())?;
        append_command_lifecycle(store, instance_id, &command, "settled")?;
    }
    Ok(true)
}

fn update_message(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    prompt: &mut PendingPrompt,
    status: ContentStatus,
) -> Result<(), NodeError> {
    prompt.message.mutation.operation = MutationOperation::Replace;
    prompt.message.mutation.base_revision = Some(prompt.message.mutation.revision);
    prompt.message.mutation.revision.0 += 1;
    prompt.message.status = status;
    store.append_observation(
        instance_id,
        None,
        Completeness::Structured,
        ObservationPayload::Message(prompt.message.clone()),
    )?;
    Ok(())
}

fn interrupt_pending(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    pending: &mut VecDeque<PendingPrompt>,
    reason: &str,
    prompts: &crate::prompt_correlation::PromptCorrelator,
) -> Result<(), NodeError> {
    while let Some(mut prompt) = pending.pop_front() {
        update_message(store, instance_id, &mut prompt, ContentStatus::Interrupted)?;
        // The prompt was never written, so later hook/transcript evidence for
        // this text belongs to whatever produced it, not this command.
        prompts.cancel(&prompt.command_id);
        let command = store.get_command(&prompt.command_id)?;
        if command.state != CommandState::Settled {
            reject_before_dispatch(store, instance_id, command, reason)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
