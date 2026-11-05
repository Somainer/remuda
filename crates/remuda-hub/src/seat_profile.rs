//! The lineage-level seat profile and the launch snapshot projection
//! (cb-seat-profile, D-063, main-agent.md §3.4.2; protocol.md §8.4).
//!
//! `lineage_profiles` holds one Human-edited document per lineage, with a
//! revision. Every edit is a compare-and-set on `expectedRevision`; only an
//! unnarrowed Human writes it, so a seat can never raise its own permission
//! mode (OA1, OA2). An edit takes effect at the next chapter: restart and
//! resume build the successor's spec from the profile in
//! [`crate::http::prepare_continuation`], and the successor row records the
//! applied revision and document. A live chapter is never touched.
//!
//! The Node journals one `launch.capabilities` event per agent launch; the
//! journal append projects the newest one onto its instance row
//! (`launchCapabilities` on the instance read) for `remuda instance get`,
//! and the profile read reports the lineage's newest.

use crate::AppState;
use crate::agent_scope::{authenticated_caller, origin};
use crate::auth::require_origin;
use crate::config::now_rfc3339;
use crate::error::{HubError, NextStep, action};
use crate::store::{Store, StoreError, ensure_column, immediate_tx};
use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use remuda_protocol::{
    InputOrigin, LaunchCapabilitiesPayload, SEAT_PROFILE_MAX_DISPLAY_NAME, SEAT_PROFILE_MAX_MODEL,
    SEAT_PROFILE_MAX_MODELS, SEAT_PROFILE_MAX_ROLE, SEAT_PROFILE_MAX_SKILLS, SeatProfile,
    SeatProfileChapter, SeatProfileFacts, SeatProfileLastLaunch, SeatProfilePutResult,
    SeatProfileView, U64, valid_skill_name,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The journal kind this module projects.
pub(crate) const LAUNCH_CAPABILITIES_KIND: &str = "launch.capabilities";

/// The audit action every edit writes.
pub(crate) const AUDIT_ACTION: &str = "lineage.profile.set";

/// Tables and columns; idempotent on every open.
pub(crate) fn migrate(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS lineage_profiles (
            lineage_id TEXT PRIMARY KEY,
            revision INTEGER NOT NULL,
            doc_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            updated_by TEXT NOT NULL
        );",
    )?;
    // The newest launch snapshot of the chapter, and the profile revision and
    // document its spec was built from (NULL: launched without a profile).
    ensure_column(conn, "instances", "launch_capabilities_json", "TEXT")?;
    ensure_column(conn, "instances", "profile_revision", "INTEGER")?;
    ensure_column(conn, "instances", "profile_json", "TEXT")?;
    Ok(())
}

/// Journal-append fold: the newest `launch.capabilities` payload becomes the
/// instance's `launchCapabilities`. Runs in the append transaction.
pub(crate) fn fold_append(
    conn: &Connection,
    instance_id: &str,
    event: &Value,
) -> Result<(), StoreError> {
    if event.get("kind").and_then(Value::as_str) != Some(LAUNCH_CAPABILITIES_KIND) {
        return Ok(());
    }
    let Some(payload) = event.get("payload") else {
        return Ok(());
    };
    conn.execute(
        "UPDATE instances SET launch_capabilities_json = ?1 WHERE id = ?2",
        params![payload.to_string(), instance_id],
    )?;
    Ok(())
}

/// The fields of a profile document as stored in `doc_json`.
#[derive(Clone, Debug, PartialEq)]
struct Doc {
    display_name: Option<String>,
    role: Option<String>,
    model_candidates: Vec<String>,
    effort: Option<Value>,
    permission_mode: String,
    skills: Vec<String>,
}

impl Doc {
    fn to_json(&self) -> Value {
        json!({
            "displayName": self.display_name,
            "role": self.role,
            "modelCandidates": self.model_candidates,
            "effort": self.effort,
            "permissionMode": self.permission_mode,
            "skills": self.skills,
        })
    }

    fn from_json(value: &Value) -> Self {
        let strings = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        Self {
            display_name: text("displayName"),
            role: text("role"),
            model_candidates: strings("modelCandidates"),
            effort: value
                .get("effort")
                .filter(|value| !value.is_null())
                .cloned(),
            permission_mode: text("permissionMode").unwrap_or_default(),
            skills: strings("skills"),
        }
    }
}

fn load_profile(conn: &Connection, lineage_id: &str) -> Result<Option<SeatProfile>, StoreError> {
    conn.query_row(
        "SELECT revision, doc_json, created_at, updated_at, updated_by
         FROM lineage_profiles WHERE lineage_id = ?1",
        params![lineage_id],
        |row| {
            let revision: i64 = row.get(0)?;
            let raw: String = row.get(1)?;
            Ok((revision, raw, row.get(2)?, row.get(3)?, row.get(4)?))
        },
    )
    .optional()?
    .map(|(revision, raw, created_at, updated_at, updated_by)| {
        let doc = Doc::from_json(&serde_json::from_str(&raw).unwrap_or_else(|_| json!({})));
        Ok(profile_of(
            lineage_id,
            u64::try_from(revision).unwrap_or(0),
            &doc,
            created_at,
            updated_at,
            updated_by,
        ))
    })
    .transpose()
}

fn profile_of(
    lineage_id: &str,
    revision: u64,
    doc: &Doc,
    created_at: String,
    updated_at: String,
    updated_by: String,
) -> SeatProfile {
    SeatProfile {
        lineage_id: lineage_id.to_owned(),
        revision,
        display_name: doc.display_name.clone(),
        role: doc.role.clone(),
        model_candidates: doc.model_candidates.clone(),
        effort: doc.effort.clone(),
        permission_mode: doc.permission_mode.clone(),
        skills: doc.skills.clone(),
        created_at,
        updated_at,
        updated_by,
    }
}

fn doc_of(profile: &SeatProfile) -> Doc {
    Doc {
        display_name: profile.display_name.clone(),
        role: profile.role.clone(),
        model_candidates: profile.model_candidates.clone(),
        effort: profile.effort.clone(),
        permission_mode: profile.permission_mode.clone(),
        skills: profile.skills.clone(),
    }
}

/// What a chapter launched with: the profile facts the Hub stamps in
/// `skillContext.profile`, read from the chapter's own record.
pub(crate) fn facts_of(
    revision: Option<i64>,
    profile_json: Option<&Value>,
) -> Option<SeatProfileFacts> {
    let revision = u64::try_from(revision?).ok()?;
    let doc = Doc::from_json(profile_json?);
    Some(SeatProfileFacts {
        revision: U64(revision),
        display_name: doc.display_name,
        role: doc.role,
        model_candidates: doc.model_candidates,
    })
}

/// The profile a successor applies: its revision and the stored document,
/// handed to the continuation transaction to record on the new chapter.
#[derive(Clone, Debug)]
pub struct AppliedProfile {
    /// The profile revision the spec was built from.
    pub revision: i64,
    /// The document at that revision.
    pub doc_json: Value,
}

/// Build the next chapter's spec from the lineage profile, if it has one
/// (main-agent.md §3.4.2, §6.3.1): `model` is the first candidate; `effort`,
/// `permissionMode` and `skills` replace the predecessor's. Returns the
/// applied revision, or `None` (and the spec untouched) without a profile.
pub(crate) fn apply_to_spec(spec: &mut Value, profile: &SeatProfile) -> AppliedProfile {
    let doc = doc_of(profile);
    if let Some(object) = spec.as_object_mut() {
        if let Some(model) = doc.model_candidates.first() {
            object.insert("model".into(), json!(model));
        }
        object.remove("effortIndex");
        match &doc.effort {
            Some(effort) => {
                object.insert("effort".into(), effort.clone());
                if let Some(name) = effort.get("name") {
                    object.insert("effortName".into(), name.clone());
                }
            }
            None => {
                object.remove("effort");
                object.remove("effortName");
            }
        }
        object.insert("permissionMode".into(), json!(doc.permission_mode));
        if doc.skills.is_empty() {
            object.remove("skills");
        } else {
            object.insert("skills".into(), json!(doc.skills));
        }
    }
    AppliedProfile {
        revision: i64::try_from(profile.revision).unwrap_or(i64::MAX),
        doc_json: doc.to_json(),
    }
}

/// The live chapter's spec as the base a first edit starts from.
fn doc_from_spec(kind: &str, spec: &Value) -> Doc {
    let model = spec
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_owned);
    let effort = spec
        .get("effort")
        .filter(|value| value.is_object())
        .cloned()
        .or_else(|| {
            spec.get("effortName")
                .and_then(Value::as_str)
                .map(|name| json!({"name": name, "ultracode": false}))
        });
    let permission_mode = spec
        .get("permissionMode")
        .and_then(Value::as_str)
        .map(|word| remuda_protocol::creator_permission::launched_mode(kind, Some(word)))
        .unwrap_or_else(|| remuda_protocol::creator_permission::launched_mode(kind, None))
        .to_owned();
    let skills = spec
        .get("skills")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Doc {
        display_name: spec
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .map(str::to_owned),
        role: None,
        model_candidates: model.into_iter().collect(),
        effort,
        permission_mode,
        skills,
    }
}

// ── store ────────────────────────────────────────────────────────────────

/// One edit, as the writer applies it.
pub(crate) struct ProfileEdit {
    pub lineage_id: String,
    pub expected_revision: u64,
    pub patch: Patch,
    pub device_id: String,
}

/// The fields a PUT names; an absent field keeps its current value.
#[derive(Default)]
pub(crate) struct Patch {
    display_name: Option<Option<String>>,
    role: Option<Option<String>>,
    model_candidates: Option<Vec<String>>,
    effort: Option<Option<Value>>,
    permission_mode: Option<String>,
    skills: Option<Vec<String>>,
}

/// What an edit came to.
pub(crate) enum EditOutcome {
    Done {
        profile: Box<SeatProfile>,
        changed: Vec<String>,
        pending: bool,
    },
    Stale {
        current: u64,
    },
    NoLineage,
    Invalid(String),
}

impl Store {
    /// The profile read: the document, the live chapter, `pending` and the
    /// lineage's newest launch snapshot.
    pub(crate) async fn seat_profile_view(
        &self,
        lineage_id: String,
    ) -> Result<Option<SeatProfileView>, StoreError> {
        self.read("seat_profile_view", move |conn| view(conn, &lineage_id))
            .await
    }

    /// The lineage's profile, for building a successor spec.
    pub(crate) async fn seat_profile(
        &self,
        lineage_id: String,
    ) -> Result<Option<SeatProfile>, StoreError> {
        self.read("seat_profile", move |conn| load_profile(conn, &lineage_id))
            .await
    }

    /// One CAS edit: the revision check, the write and its audit row in one
    /// writer transaction.
    pub(crate) async fn edit_seat_profile(
        &self,
        edit: ProfileEdit,
    ) -> Result<EditOutcome, StoreError> {
        self.run_named("edit_seat_profile", move |conn| {
            let tx = immediate_tx(conn)?;
            let outcome = edit_tx(&tx, edit)?;
            if matches!(outcome, EditOutcome::Done { .. }) {
                tx.commit()?;
            }
            Ok(outcome)
        })
        .await
    }
}

/// The lineage's current chapter: id, generation, applied profile revision,
/// kind and spec.
type LiveChapter = (String, i64, Option<i64>, String, String);

fn live_chapter(conn: &Connection, lineage_id: &str) -> Result<Option<LiveChapter>, StoreError> {
    conn.query_row(
        "SELECT i.id, i.generation, i.profile_revision, i.kind, i.spec_json
         FROM lineages l JOIN instances i ON i.id = l.current_instance_id
         WHERE l.lineage_id = ?1",
        params![lineage_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )
    .optional()
    .map_err(StoreError::from)
}

fn pending_of(profile: Option<&SeatProfile>, applied: Option<i64>) -> bool {
    profile.is_some_and(|profile| {
        i64::try_from(profile.revision).unwrap_or(i64::MAX) > applied.unwrap_or(0)
    })
}

fn view(conn: &Connection, lineage_id: &str) -> Result<Option<SeatProfileView>, StoreError> {
    let Some((instance_id, generation, applied, _, _)) = live_chapter(conn, lineage_id)? else {
        return Ok(None);
    };
    let profile = load_profile(conn, lineage_id)?;
    let last_launch = conn
        .query_row(
            "SELECT id, generation, launch_capabilities_json FROM instances
             WHERE COALESCE(lineage_id, id) = ?1 AND launch_capabilities_json IS NOT NULL
             ORDER BY generation DESC, created_at DESC LIMIT 1",
            params![lineage_id],
            |row| {
                let raw: String = row.get(2)?;
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, raw))
            },
        )
        .optional()?
        .and_then(|(instance_id, generation, raw)| {
            let payload: LaunchCapabilitiesPayload = serde_json::from_str(&raw).ok()?;
            Some(SeatProfileLastLaunch {
                instance_id,
                generation: u64::try_from(generation).unwrap_or(0),
                payload,
            })
        });
    Ok(Some(SeatProfileView {
        lineage_id: lineage_id.to_owned(),
        pending: pending_of(profile.as_ref(), applied),
        profile,
        live_chapter: SeatProfileChapter {
            instance_id,
            generation: u64::try_from(generation).unwrap_or(0),
            applied_revision: applied.and_then(|revision| u64::try_from(revision).ok()),
        },
        last_launch,
    }))
}

fn edit_tx(conn: &Connection, edit: ProfileEdit) -> Result<EditOutcome, StoreError> {
    let Some((_, _, applied, kind, spec_raw)) = live_chapter(conn, &edit.lineage_id)? else {
        return Ok(EditOutcome::NoLineage);
    };
    let current = load_profile(conn, &edit.lineage_id)?;
    let current_revision = current.as_ref().map_or(0, |profile| profile.revision);
    if current_revision != edit.expected_revision {
        return Ok(EditOutcome::Stale {
            current: current_revision,
        });
    }
    // The first edit starts from the live chapter's spec (main-agent.md
    // §3.4.2); later ones from the stored document.
    let base = match &current {
        Some(profile) => doc_of(profile),
        None => doc_from_spec(
            &kind,
            &serde_json::from_str(&spec_raw).unwrap_or_else(|_| json!({})),
        ),
    };
    let mut next = base.clone();
    let patch = edit.patch;
    if let Some(value) = patch.display_name {
        next.display_name = value;
    }
    if let Some(value) = patch.role {
        next.role = value;
    }
    if let Some(value) = patch.model_candidates {
        next.model_candidates = value;
    }
    if let Some(value) = patch.effort {
        next.effort = value;
    }
    if let Some(value) = patch.permission_mode {
        next.permission_mode = value;
    }
    if let Some(value) = patch.skills {
        next.skills = value;
    }
    if let Err(message) = validate(&kind, &next) {
        return Ok(EditOutcome::Invalid(message));
    }
    let mut changed = Vec::new();
    for (name, differs) in [
        ("displayName", next.display_name != base.display_name),
        ("role", next.role != base.role),
        (
            "modelCandidates",
            next.model_candidates != base.model_candidates,
        ),
        ("effort", next.effort != base.effort),
        (
            "permissionMode",
            next.permission_mode != base.permission_mode,
        ),
        ("skills", next.skills != base.skills),
    ] {
        if differs {
            changed.push(name.to_owned());
        }
    }
    let revision = current_revision + 1;
    let now = now_rfc3339();
    let created_at = current
        .as_ref()
        .map(|profile| profile.created_at.clone())
        .unwrap_or_else(|| now.clone());
    conn.execute(
        "INSERT INTO lineage_profiles
            (lineage_id, revision, doc_json, created_at, updated_at, updated_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(lineage_id) DO UPDATE SET
            revision = excluded.revision, doc_json = excluded.doc_json,
            updated_at = excluded.updated_at, updated_by = excluded.updated_by",
        params![
            edit.lineage_id,
            i64::try_from(revision).unwrap_or(i64::MAX),
            next.to_json().to_string(),
            created_at,
            now,
            edit.device_id,
        ],
    )?;
    // Who, the revision, and the changed field names: never their values.
    conn.execute(
        "INSERT INTO audit_log (device_id, action, subject, detail_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            edit.device_id,
            AUDIT_ACTION,
            edit.lineage_id,
            json!({"revision": revision, "changed": changed}).to_string(),
            now,
        ],
    )?;
    let profile = profile_of(
        &edit.lineage_id,
        revision,
        &next,
        created_at,
        now,
        edit.device_id.clone(),
    );
    Ok(EditOutcome::Done {
        pending: pending_of(Some(&profile), applied),
        profile: Box::new(profile),
        changed,
    })
}

/// The bounds of protocol.md §8.4.
fn validate(kind: &str, doc: &Doc) -> Result<(), String> {
    let models = &doc.model_candidates;
    if models.is_empty() || models.len() > SEAT_PROFILE_MAX_MODELS {
        return Err(format!(
            "modelCandidates must list 1 to {SEAT_PROFILE_MAX_MODELS} models, the first being the \
             default (got {})",
            models.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for model in models {
        let trimmed = model.trim();
        if trimmed.is_empty()
            || trimmed != model
            || model.chars().count() > SEAT_PROFILE_MAX_MODEL
            || model.chars().any(char::is_control)
        {
            return Err(format!(
                "model candidate {model:?} must be a non-empty name of at most \
                 {SEAT_PROFILE_MAX_MODEL} characters"
            ));
        }
        if !seen.insert(model.as_str()) {
            return Err(format!("model candidate {model:?} is listed twice"));
        }
    }
    if let Some(name) = &doc.display_name
        && (name.chars().count() > SEAT_PROFILE_MAX_DISPLAY_NAME
            || name.chars().any(char::is_control))
    {
        return Err(format!(
            "displayName must be one line of at most {SEAT_PROFILE_MAX_DISPLAY_NAME} characters"
        ));
    }
    if let Some(role) = &doc.role
        && (role.chars().count() > SEAT_PROFILE_MAX_ROLE || role.chars().any(char::is_control))
    {
        return Err(format!(
            "role must be one line of at most {SEAT_PROFILE_MAX_ROLE} characters"
        ));
    }
    if let Some(effort) = &doc.effort {
        let name = effort
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ultracode_ok = effort
            .get("ultracode")
            .is_none_or(|value| value.is_boolean());
        if name.is_empty() || name.len() > 16 || !ultracode_ok || !effort.is_object() {
            return Err("effort must be {name, ultracode?}, as on a create body".into());
        }
    }
    let vocabulary = remuda_protocol::creator_permission::vocabulary(kind);
    if !vocabulary.contains(&doc.permission_mode.as_str()) {
        return Err(format!(
            "permissionMode {:?} is not a {kind} mode; use one of {}",
            doc.permission_mode,
            vocabulary.join(", ")
        ));
    }
    validate_skills(&doc.skills)
}

/// `skills` on a profile or a create body: at most 16 host-absolute
/// directories with valid, unique base names.
pub(crate) fn validate_skills(skills: &[String]) -> Result<(), String> {
    if skills.len() > SEAT_PROFILE_MAX_SKILLS {
        return Err(format!(
            "skills lists at most {SEAT_PROFILE_MAX_SKILLS} directories (got {})",
            skills.len()
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for skill in skills {
        let path = std::path::Path::new(skill);
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !path.is_absolute() || skill.contains('\0') || !valid_skill_name(&name) {
            return Err(format!(
                "skill directory {skill:?} must be a host-absolute path whose base name is \
                 [A-Za-z0-9._-]{{1,64}} and not remuda or codex-computer-use"
            ));
        }
        if !seen.insert(name.clone()) {
            return Err(format!("two skill directories are named {name:?}"));
        }
    }
    Ok(())
}

// ── routes ───────────────────────────────────────────────────────────────

/// `GET` and `PUT /v1/lineages/{id}/profile`.
pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/v1/lineages/{id}/profile",
        get(get_profile).put(put_profile),
    )
}

/// 403 for anything but an unnarrowed Human writing the profile.
fn human_only() -> HubError {
    HubError::Forbidden.with_next(
        "profile-human-only",
        NextStep::new(
            action::ASK_OWNER,
            "only the owner edits a seat profile, from a Human device: it holds the permission \
             mode the next chapter launches with, so no seat (this one included), bot or \
             narrowed token may change it; do not retry, ask the owner",
        ),
    )
}

async fn get_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
) -> Result<Json<SeatProfileView>, HubError> {
    require_origin(&headers, &state.config)?;
    let (authenticated, narrowed) = authenticated_caller(&state, &headers).await?;
    if origin(&authenticated) == InputOrigin::Bot {
        return Err(HubError::Forbidden);
    }
    match origin(&narrowed) {
        InputOrigin::Human => {}
        InputOrigin::Agent => {
            // The seat reads its own profile, and only its own.
            let bound = narrowed.instance_id.clone().ok_or(HubError::Forbidden)?;
            let own = state
                .store
                .lineage_id_for_instance(bound)
                .await?
                .ok_or(HubError::Forbidden)?;
            if own != lineage_id {
                return Err(HubError::Forbidden);
            }
        }
        _ => return Err(HubError::Forbidden),
    }
    state
        .store
        .seat_profile_view(lineage_id)
        .await?
        .map(Json)
        .ok_or(HubError::NotFound)
}

/// A PUT body. Every field but `expectedRevision` is optional; an absent one
/// keeps its current value, and `displayName`, `role` and `effort` take
/// `null` to clear.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutBody {
    expected_revision: Option<u64>,
    #[serde(default, deserialize_with = "present")]
    display_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    role: Option<Option<String>>,
    #[serde(default)]
    model_candidates: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    effort: Option<Option<Value>>,
    #[serde(default)]
    permission_mode: Option<String>,
    #[serde(default)]
    skills: Option<Vec<String>>,
}

/// A field that is present (even as `null`) is `Some`.
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

/// A refusal of a profile edit.
enum PutError {
    Hub(HubError),
    Stale(u64),
    Secret {
        field: &'static str,
        rule: &'static str,
    },
}

impl From<HubError> for PutError {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

impl From<StoreError> for PutError {
    fn from(error: StoreError) -> Self {
        Self::Hub(error.into())
    }
}

impl IntoResponse for PutError {
    fn into_response(self) -> Response {
        match self {
            Self::Hub(error) => error.into_response(),
            // Never echo the text: only the field and the shape that matched.
            Self::Secret { field, rule } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": format!("{field} looks like it holds a credential; nothing was stored"),
                    "code": "PROFILE_SECRET_REJECTED",
                    "field": field,
                    "rule": rule,
                    "reason": "profile-secret-rejected",
                    "next": {
                        "action": action::FIX_REQUEST,
                        "hint": "remove the credential and save again",
                    },
                })),
            )
                .into_response(),
            Self::Stale(current) => (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "the profile changed since you read it; it is at revision {current}"
                    ),
                    "code": "PROFILE_REVISION_CONFLICT",
                    "currentRevision": current,
                    "reason": "stale-revision",
                    "next": {
                        "action": action::READ_THEN_DECIDE,
                        "hint": format!(
                            "reload the profile (revision {current}), reapply your change, \
                             then save with expectedRevision {current}"
                        ),
                    },
                })),
            )
                .into_response(),
        }
    }
}

fn bad(message: impl Into<String>) -> HubError {
    HubError::BadRequest(message.into()).with_next(
        "invalid-profile",
        NextStep::new(
            action::FIX_REQUEST,
            "fix the named field and save again with the same expectedRevision",
        ),
    )
}

async fn put_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lineage_id): Path<String>,
    Json(body): Json<PutBody>,
) -> Result<Json<SeatProfilePutResult>, PutError> {
    require_origin(&headers, &state.config)?;
    let (authenticated, narrowed) = authenticated_caller(&state, &headers).await?;
    // An unnarrowed Human only: narrowing turns a Human token into the
    // chapter's Agent, and a Bot relays other people's words.
    if origin(&authenticated) != InputOrigin::Human || origin(&narrowed) != InputOrigin::Human {
        return Err(human_only().into());
    }
    let expected_revision = body
        .expected_revision
        .ok_or_else(|| bad("expectedRevision is required (0 creates the profile)"))?;
    // The secret lint covers the free text before anything is stored.
    for (field, text) in [
        ("displayName", body.display_name.clone().flatten()),
        ("role", body.role.clone().flatten()),
    ] {
        if let Some(text) = text
            && let Some(rule) = crate::secret_lint::first_secret(&text)
        {
            return Err(PutError::Secret { field, rule });
        }
    }
    let normalize = |value: Option<Option<String>>| {
        value.map(|inner| {
            inner
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        })
    };
    let effort = body.effort.map(|inner| {
        inner.map(|value| match value {
            Value::String(name) => json!({"name": name, "ultracode": false}),
            Value::Object(mut map) => {
                map.entry("ultracode").or_insert(Value::Bool(false));
                Value::Object(Map::from_iter(map))
            }
            other => other,
        })
    });
    let outcome = state
        .store
        .edit_seat_profile(ProfileEdit {
            lineage_id,
            expected_revision,
            patch: Patch {
                display_name: normalize(body.display_name),
                role: normalize(body.role),
                model_candidates: body.model_candidates,
                effort,
                permission_mode: body.permission_mode,
                skills: body.skills,
            },
            device_id: authenticated.id.clone(),
        })
        .await?;
    match outcome {
        EditOutcome::Done {
            profile,
            changed,
            pending,
        } => Ok(Json(SeatProfilePutResult {
            profile: *profile,
            pending,
            changed,
        })),
        EditOutcome::Stale { current } => Err(PutError::Stale(current)),
        EditOutcome::NoLineage => Err(HubError::NotFound.into()),
        EditOutcome::Invalid(message) => Err(bad(message).into()),
    }
}
