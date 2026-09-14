//! Bounded PTY prompt delivery; control operations remain usable while input waits.

use super::*;
use crate::DriverError;
use remuda_protocol::{ContentStatus, DriverKind, MessagePayload, MutationOperation};
use std::{collections::VecDeque, time::Duration};

struct PendingPrompt {
    command_id: CommandId,
    prompt: String,
    /// Already on disk when the prompt was queued (D-027); a PTY agent reads
    /// these by path once the prompt is finally typed.
    attachments: Vec<crate::attachments::MaterializedAttachment>,
    origin: remuda_protocol::InputOrigin,
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
                // The create prompt stages no attachments (D-027).
                Vec::new(),
                crate::origin::input_origin(command.origin),
                prompts.as_ref(),
            )?);
        }
        // Creating the live runtime and delivering its first input are separate facts.
        settle_without_driver(store.as_ref(), &instance_id, &mut command)?;
    }
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            queued = receiver.recv() => {
                let Some(queued) = queued else {
                    interrupt_pending(store.as_ref(), &instance_id, &mut pending, "instance worker closed", prompts.as_ref())?;
                    return Err(NodeError::DriverUnavailable);
                };
                if let DriverRequest::Send { prompt, attachments, origin } = queued.request {
                    if pending.len() == capacity {
                        let command = store.get_command(&queued.command_id)?;
                        prompts.cancel(&queued.command_id);
                        reject_before_dispatch(store.as_ref(), &instance_id, command, "instance-queue-full")?;
                    } else {
                        pending.push_back(enqueue(store.as_ref(), &instance_id, queued.command_id, prompt, attachments, origin, prompts.as_ref())?);
                    }
                } else {
                    let close_after = queued.close_after;
                    if matches!(queued.request, DriverRequest::Cancel | DriverRequest::Close) {
                        interrupt_pending(store.as_ref(), &instance_id, &mut pending, "queued input cancelled", prompts.as_ref())?;
                    }
                    execute_queued(store.clone(), &instance_id, driver.clone(), queued, interactions.clone(), carrier.clone(), Arc::clone(&prompts)).await?;
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
                        mark_blocked(store.as_ref(), &instance_id)?;
                    }
                }
            }
            _ = tick.tick(), if !pending.is_empty() => {
                if store.get_instance(&instance_id)?.lifecycle != InstanceLifecycle::Ready {
                    interrupt_pending(store.as_ref(), &instance_id, &mut pending, "instance no longer ready", prompts.as_ref())?;
                    continue;
                }
                if let Some(prompt) = pending.front_mut()
                    && deliver(store.as_ref(), &instance_id, driver.as_ref(), &interactions, prompt, carrier.as_ref(), prompts.as_ref()).await?
                {
                    pending.pop_front();
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
    attachments: Vec<crate::attachments::MaterializedAttachment>,
    origin: remuda_protocol::InputOrigin,
    prompts: &crate::prompt_correlation::PromptCorrelator,
) -> Result<PendingPrompt, NodeError> {
    let ObservationPayload::Message(mut message) =
        crate::driver::message_payload(MessageRole::User, MessagePhase::Input, prompt.clone())?
    else {
        return Err(NodeError::InvalidRequest("expected user message".into()));
    };
    message.status = ContentStatus::Queued;
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
    mark_pending(store, instance_id)?;
    Ok(PendingPrompt {
        command_id,
        prompt,
        attachments,
        origin,
        message,
    })
}

fn mark_blocked(store: &dyn LocalStore, instance_id: &InstanceId) -> Result<(), NodeError> {
    if store.get_instance(instance_id)?.activity
        == (Knowledge::Known {
            value: Activity::WaitingInteraction,
        })
    {
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

fn mark_pending(store: &dyn LocalStore, instance_id: &InstanceId) -> Result<(), NodeError> {
    // A queued follow-up must not replace evidence of a currently running turn.
    if store.get_instance(instance_id)?.activity
        == (Knowledge::Known {
            value: Activity::Working,
        })
    {
        return Ok(());
    }
    mark_blocked(store, instance_id)
}

async fn deliver(
    store: &dyn LocalStore,
    instance_id: &InstanceId,
    driver: &dyn Driver,
    interactions: &InteractionRuntime,
    prompt: &mut PendingPrompt,
    carrier: Option<&crate::carrier_recovery::CarrierSupervisor>,
    prompts: &crate::prompt_correlation::PromptCorrelator,
) -> Result<bool, NodeError> {
    // Cancelling a readiness probe cannot submit input. Never time out or replay send().
    let execution =
        match tokio::time::timeout(Duration::from_millis(100), driver.wait_control()).await {
            Err(_) | Ok(Err(DriverError::ControlUnavailable)) => {
                mark_pending(store, instance_id)?;
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
                        attachments: prompt.attachments.clone(),
                        origin: prompt.origin,
                    })
                    .await
            }
        };
    let mut command = store.get_command(&prompt.command_id)?;
    match execution {
        Err(DriverError::ControlUnavailable) => {
            mark_blocked(store, instance_id)?;
            return Ok(false);
        }
        Ok(emissions) => {
            update_message(store, instance_id, prompt, ContentStatus::Complete)?;
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
