//! Push triggers and Paseo-style follow suppression.

use crate::AppState;
use crate::store::{InteractionRecord, JournalRecord};
use remuda_protocol::Task;
use remuda_push::{Notification, PushTag};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
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
                    interaction_landing_url(id),
                ),
                AlertKind::TurnError => (
                    "Turn failed",
                    "The last turn ended with an error.".to_string(),
                    PushTag::Instance {
                        id: instance_id.clone(),
                    },
                    format!("/s/{instance_id}"),
                ),
                AlertKind::Exited => {
                    // D-057 §8.5: on a lineage that restarts on process loss
                    // the C1 push ("restarted" / "paused: restart cap")
                    // replaces the generic exited push.
                    if crate::restart::c1_replaces_exit_push(&state, &instance_id).await {
                        return;
                    }
                    (
                        "Session exited",
                        "An instance left the ready state.".to_string(),
                        PushTag::Instance {
                            id: instance_id.clone(),
                        },
                        format!("/s/{instance_id}"),
                    )
                }
                AlertKind::Waiting => unreachable!(),
            };
            fanout(&state, &instance_id, title, &body, tag, url).await;
        }
    }
}

/// What one fanout did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fanout {
    /// This many notifications were accepted by a push service (some
    /// endpoints may have failed or been pruned as gone).
    Sent(usize),
    /// Nothing to deliver: push is off, a device follows the instance, or no
    /// subscription accepted it because every endpoint was gone.
    Nothing,
    /// Delivery failed: the subscriptions could not be read, or every
    /// endpoint that was tried failed. A durable push is retried.
    Failed,
}

impl Fanout {
    /// Recipients to record when the push counts as delivered; `None` when it
    /// failed and stays unsent.
    pub(crate) fn recipients(self) -> Option<usize> {
        match self {
            Self::Sent(sent) => Some(sent),
            Self::Nothing => Some(0),
            Self::Failed => None,
        }
    }
}

/// D-057 §8.5: one C1 push about a continuity lineage (restarted, restart
/// cap, closed itself, paused by an ancestor, could not restart). Tagged by
/// the lineage, so a newer C1 push replaces an older one (and a resend after
/// a crash replaces itself); it links to `instance_id`.
pub(crate) async fn c1_push(
    state: &AppState,
    lineage_id: &str,
    instance_id: &str,
    title: &str,
    body: &str,
) -> Fanout {
    fanout(
        state,
        instance_id,
        title,
        body,
        PushTag::Instance {
            id: lineage_id.to_owned(),
        },
        format!("/s/{instance_id}"),
    )
    .await
}

/// Send one notification to every subscription unless a device follows the
/// instance.
async fn fanout(
    state: &AppState,
    instance_id: &str,
    title: &str,
    body: &str,
    tag: PushTag,
    url: String,
) -> Fanout {
    let Some(push) = state.push.as_ref() else {
        return Fanout::Nothing;
    };
    if state.followers.any_watching(instance_id).await {
        tracing::debug!(instance_id, "push suppressed: a device is following");
        return Fanout::Nothing;
    }
    let subs = match push.subscriptions() {
        Ok(subs) => subs,
        Err(error) => {
            tracing::warn!(%error, "could not read push subscriptions");
            return Fanout::Failed;
        }
    };
    // The badge is the owner's 「需要你」 N (home.md §5.1, ui-spec §4.5): the
    // interactions really waiting plus the owner-blocked tasks with no such
    // interaction. Pending is global durable state — every subscribed device
    // is shown the same queue — so the count is read once per fanout, durable
    // Hub truth only: a device live-following the instance got no push at
    // all thanks to the suppression above.
    let pending = owner_badge(state).await;
    let base = Notification::new(title, body, tag.clone(), url);
    let (mut sent, mut failed) = (0, 0);
    for sub in subs {
        let notification = base.clone().with_badge(pending);
        match push.notify(&sub, &notification, &tag).await {
            Ok(remuda_push::Delivery::Sent) => sent += 1,
            Ok(remuda_push::Delivery::Gone) => {}
            Err(err) => {
                failed += 1;
                tracing::debug!(error = %err, endpoint = %sub.endpoint, "push notify failed");
            }
        }
    }
    match (sent, failed) {
        (0, 0) => Fanout::Nothing,
        (0, _) => Fanout::Failed,
        (sent, _) => Fanout::Sent(sent),
    }
}

/// Where a push about interaction `id` lands (home.md §9 角标与落点): the
/// desktop home with the card focused. A phone keeps the query through the
/// `/sessions` → `/m` redirect, so the same URL lands on `/m?focus=<id>`.
pub(crate) fn interaction_landing_url(id: &str) -> String {
    format!("/sessions?focus={id}")
}

/// Read the three inputs of [`owner_queue_count`] and count. A read failure
/// leaves that part at zero rather than dropping the push.
async fn owner_badge(state: &AppState) -> u64 {
    let pending = state
        .store
        .list_interactions(None, None, None, true)
        .await
        .unwrap_or_default();
    let tasks = state.store.list_tasks(None).await.unwrap_or_default();
    let task_of_instance = state.store.instance_task_ids().await.unwrap_or_default();
    owner_queue_count(
        &pending,
        &tasks,
        &task_of_instance,
        OffsetDateTime::now_utc(),
    )
}

/// home.md §5.1 N = A + B, the count the web ① row, the ④ queue header and
/// the OS badge all show (the web twin is `glanceCounts()` in
/// `web/src/features/home/glance.ts`).
///
/// * A: pending interactions whose known deadline has not passed (a ghost card
///   past its deadline is not shown, so it is not counted either).
/// * B: unarchived tasks with a non-blank `blocked_reason` none of whose
///   sessions (`instances.task_id`) has an interaction counted in A — the
///   interaction card and the task card are never both shown (§4.6).
///
/// Done-unseen, notices and failed turns are not part of N.
pub(crate) fn owner_queue_count(
    pending: &[InteractionRecord],
    tasks: &[Task],
    task_of_instance: &HashMap<String, String>,
    now: OffsetDateTime,
) -> u64 {
    let waiting: Vec<&InteractionRecord> = pending
        .iter()
        .filter(|row| row.state == "pending" && !deadline_passed(&row.payload, now))
        .collect();
    let covered: HashSet<&str> = waiting
        .iter()
        .filter_map(|row| task_of_instance.get(&row.instance_id).map(String::as_str))
        .collect();
    let blocked = tasks
        .iter()
        .filter(|task| task.archived_at.is_none())
        .filter(|task| {
            task.blocked_reason
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty())
        })
        .filter(|task| !covered.contains(task.meta.id.as_id().as_str()))
        .count();
    u64::try_from(waiting.len() + blocked).unwrap_or(u64::MAX)
}

/// A durable interaction row whose wire entity carries a known deadline that
/// is already in the past (the web's `deadlinePassed`).
fn deadline_passed(payload: &Value, now: OffsetDateTime) -> bool {
    let Some(entity) = payload
        .pointer("/payload/interaction")
        .or_else(|| payload.pointer("/payload/entity"))
    else {
        return false;
    };
    if entity.pointer("/deadline/state").and_then(Value::as_str) != Some("known") {
        return false;
    }
    entity
        .pointer("/deadline/value")
        .and_then(Value::as_str)
        .and_then(|raw| OffsetDateTime::parse(raw, &Rfc3339).ok())
        .is_some_and(|deadline| deadline < now)
}

/// Map a journal event to its outward alert kind.
///
/// RESTORE-SUPPRESS SITE (spec only — `docs/design/hub-topology.md` §6.3):
/// this classification feeds the only two push-bearing effects the spec
/// names — interaction wakes (`AlertKind::Interaction`) and push fanout
/// (`dispatch`/`fanout` above, including blocked reminders and badges) — and
/// it is reached solely through `observe` from the journal append path,
/// which already skips rows with `appended.replayed`
/// (`crates/remuda-hub/src/ws.rs:634`). Catch-up rows above a restored
/// snapshot are fresh inserts, so the implementation batch must gate that
/// call site on a per-instance restore watermark; the third suppressed
/// effect is `api.egress` reinstall (`crates/remuda-hub/src/api_relay.rs:415`).
/// Durable projections keep applying; only outward effects are suppressed.
/// No behavior changes in this comment.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::InteractionRecord;
    use remuda_protocol::{ProjectId, TaskBudget, TaskClass, Timestamp};
    use serde_json::json;

    const NOW: &str = "2026-11-01T10:00:00.000Z";

    fn now() -> OffsetDateTime {
        OffsetDateTime::parse(NOW, &Rfc3339).expect("now")
    }

    fn pending(id: &str, instance: &str, deadline: Option<&str>) -> InteractionRecord {
        let deadline = match deadline {
            Some(value) => json!({ "state": "known", "value": value }),
            None => json!({ "state": "unknown" }),
        };
        InteractionRecord {
            interaction_id: id.into(),
            instance_id: instance.into(),
            host_id: "hst_1".into(),
            kind: "approval".into(),
            state: "pending".into(),
            blocking: true,
            payload: json!({
                "kind": "interaction.requested",
                "payload": { "interaction": { "id": id, "deadline": deadline } }
            }),
            created_at: NOW.into(),
            updated_at: NOW.into(),
        }
    }

    fn task(blocked: Option<&str>, archived: bool) -> Task {
        let stamp = Timestamp::try_from(NOW.to_string()).expect("timestamp");
        let mut task = Task::new_root(
            stamp.clone(),
            ProjectId::new(),
            "t".into(),
            "intent".into(),
            TaskClass::Implement,
            Vec::new(),
            Vec::new(),
            TaskBudget::default(),
        );
        task.blocked_reason = blocked.map(str::to_string);
        task.archived_at = archived.then_some(stamp);
        task
    }

    #[test]
    fn interaction_pushes_land_on_the_home_with_the_card_focused() {
        assert_eq!(
            interaction_landing_url("int_0193a5b2c3d4"),
            "/sessions?focus=int_0193a5b2c3d4"
        );
    }

    #[test]
    fn the_badge_is_waiting_interactions_plus_owner_blocked_tasks_without_one() {
        let lone = task(Some("等你确认验收范围"), false);
        let covered = task(Some("等你看 gate"), false);
        let archived = task(Some("x"), true);
        let blank = task(Some("   "), false);
        let fine = task(None, false);
        let mut task_of_instance = HashMap::new();
        task_of_instance.insert(
            "ins_covered".to_string(),
            covered.meta.id.as_id().to_string(),
        );
        let rows = vec![
            pending("int_a", "ins_a", None),
            pending("int_b", "ins_covered", Some("2999-01-01T00:00:00.000Z")),
            // A ghost card past its known deadline counts for nothing.
            pending("int_ghost", "ins_c", Some("2026-11-01T09:59:59.000Z")),
        ];
        let tasks = vec![lone, covered, archived, blank, fine];
        // A = int_a + int_b; B = the lone blocked task only.
        assert_eq!(
            owner_queue_count(&rows, &tasks, &task_of_instance, now()),
            3
        );
    }

    #[test]
    fn a_task_whose_only_interaction_is_a_ghost_counts_as_blocked() {
        let blocked = task(Some("等你"), false);
        let mut task_of_instance = HashMap::new();
        task_of_instance.insert("ins_t".to_string(), blocked.meta.id.as_id().to_string());
        let rows = vec![pending(
            "int_ghost",
            "ins_t",
            Some("2026-01-01T00:00:00.000Z"),
        )];
        assert_eq!(
            owner_queue_count(&rows, &[blocked], &task_of_instance, now()),
            1
        );
    }

    #[test]
    fn nothing_waiting_clears_the_badge() {
        assert_eq!(owner_queue_count(&[], &[], &HashMap::new(), now()), 0);
    }
}
