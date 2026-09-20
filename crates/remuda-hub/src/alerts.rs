//! Push triggers and Paseo-style follow suppression.

use crate::AppState;
use crate::store::JournalRecord;
use remuda_push::{Notification, PushTag};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Devices currently watching an instance over `/v1/follow`.
#[derive(Clone, Default)]
pub struct Followers {
    inner: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

impl Followers {
    /// Record that `device_id` is following `instance_id`.
    pub async fn watch(&self, device_id: String, instance_id: String) {
        self.inner
            .lock()
            .await
            .entry(instance_id)
            .or_default()
            .insert(device_id);
    }

    /// Drop watches for this device on the given instances.
    pub async fn unwatch(&self, device_id: &str, instance_ids: &[String]) {
        let mut guard = self.inner.lock().await;
        for id in instance_ids {
            if let Some(set) = guard.get_mut(id) {
                set.remove(device_id);
                if set.is_empty() {
                    guard.remove(id);
                }
            }
        }
    }

    /// True when any device is following the instance (Paseo attention suppress).
    pub async fn any_watching(&self, instance_id: &str) -> bool {
        self.inner
            .lock()
            .await
            .get(instance_id)
            .is_some_and(|set| !set.is_empty())
    }
}

/// Generation counter so stale blocked timers do not fire.
#[derive(Clone, Default)]
pub struct BlockedWatch {
    inner: Arc<Mutex<HashMap<String, u64>>>,
}

impl BlockedWatch {
    async fn bump(&self, instance_id: &str) -> u64 {
        let mut guard = self.inner.lock().await;
        let slot = guard.entry(instance_id.to_string()).or_insert(0);
        *slot += 1;
        *slot
    }

    async fn clear(&self, instance_id: &str) {
        self.inner.lock().await.remove(instance_id);
    }

    async fn still(&self, instance_id: &str, generation: u64) -> bool {
        self.inner.lock().await.get(instance_id).copied() == Some(generation)
    }
}

/// Inspect a journal row and maybe wake subscribed devices.
pub fn observe(state: &AppState, record: &JournalRecord) {
    let Some(kind) = classify(&record.event) else {
        return;
    };
    let instance_id = record.instance_id.clone();
    let state = state.clone();
    tokio::spawn(async move {
        dispatch(state, instance_id, kind).await;
    });
}

enum AlertKind {
    Interaction { id: String },
    TurnError,
    Exited,
    Waiting,
}

async fn dispatch(state: AppState, instance_id: String, kind: AlertKind) {
    match &kind {
        AlertKind::Waiting => {
            let generation = state.blocked.bump(&instance_id).await;
            let delay = std::time::Duration::from_millis(state.config.push_block_ms.max(1));
            tokio::time::sleep(delay).await;
            if !state.blocked.still(&instance_id, generation).await {
                return;
            }
            fanout(
                &state,
                &instance_id,
                "Still waiting",
                "This session has been blocked for more than 30s.",
                PushTag::Instance {
                    id: instance_id.clone(),
                },
                format!("/s/{instance_id}"),
            )
            .await;
        }
        other => {
            if !matches!(other, AlertKind::Interaction { .. }) {
                state.blocked.clear(&instance_id).await;
            }
            let (title, body, tag, url) = match other {
                AlertKind::Interaction { id } => (
                    "Need your input",
                    "An approval or question is waiting.".to_string(),
                    PushTag::Interaction { id: id.clone() },
                    format!("/approvals?focus={id}"),
                ),
                AlertKind::TurnError => (
                    "Turn failed",
                    "The last turn ended with an error.".to_string(),
                    PushTag::Instance {
                        id: instance_id.clone(),
                    },
                    format!("/s/{instance_id}"),
                ),
                AlertKind::Exited => (
                    "Session exited",
                    "An instance left the ready state.".to_string(),
                    PushTag::Instance {
                        id: instance_id.clone(),
                    },
                    format!("/s/{instance_id}"),
                ),
                AlertKind::Waiting => unreachable!(),
            };
            fanout(&state, &instance_id, title, &body, tag, url).await;
        }
    }
}

async fn fanout(
    state: &AppState,
    instance_id: &str,
    title: &str,
    body: &str,
    tag: PushTag,
    url: String,
) {
    let Some(push) = state.push.as_ref() else {
        return;
    };
    if state.followers.any_watching(instance_id).await {
        tracing::debug!(instance_id, "push suppressed: a device is following");
        return;
    }
    let Ok(subs) = push.subscriptions() else {
        return;
    };
    // The badge is this device's pending interaction count (ui-spec §4.5 /
    // D-049). Pending is global durable state — every subscribed device is
    // shown the same queue — so the count is read once per fanout, durable
    // Hub truth only: a device live-following the instance got no push at
    // all thanks to the suppression above. The durable index is the same
    // list the web boot gate and `/v1/interactions` serve.
    let pending = state
        .store
        .list_interactions(None, None, None, true)
        .await
        .map(|rows| u64::try_from(rows.len()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let base = Notification::new(title, body, tag.clone(), url);
    for sub in subs {
        let notification = base.clone().with_badge(pending);
        if let Err(err) = push.notify(&sub, &notification, &tag).await {
            tracing::debug!(error = %err, endpoint = %sub.endpoint, "push notify failed");
        }
    }
}

fn classify(event: &Value) -> Option<AlertKind> {
    let kind = event
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| event.get("subtype").and_then(Value::as_str))
        .unwrap_or("");
    let activity = event
        .get("activity")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/payload/activity").and_then(Value::as_str))
        .unwrap_or("");
    let lifecycle = event
        .get("lifecycle")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/payload/lifecycle").and_then(Value::as_str))
        .unwrap_or("");
    if kind == "interaction.requested" || kind == "interactionRequested" {
        let id = event
            .get("interactionId")
            .and_then(Value::as_str)
            .or_else(|| {
                event
                    .pointer("/payload/interactionId")
                    .and_then(Value::as_str)
            })
            .unwrap_or("")
            .to_string();
        return Some(AlertKind::Interaction { id });
    }
    if kind == "turn_done" || kind == "turn.done" {
        let err = event
            .get("isError")
            .or_else(|| event.get("is_error"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if err {
            return Some(AlertKind::TurnError);
        }
    }
    if lifecycle == "exited"
        || (kind == "lifecycle"
            && event
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|s| s == "exited"))
    {
        return Some(AlertKind::Exited);
    }
    if activity == "waiting-interaction" {
        return Some(AlertKind::Waiting);
    }
    None
}
