//! HTTP and JSON-RPC errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

/// D-060 §9.1: the `next.action` vocabulary. `next` is only ever a hint; it
/// never grants anything and never adds a refusal path.
pub mod action {
    /// Owner-selected configuration: do not retry, report to the owner.
    pub const ASK_OWNER: &str = "ask-owner";
    /// Wait for a human to answer the interaction, then retry with approvalId.
    pub const AWAIT_APPROVAL: &str = "await-approval";
    /// Outcome unknown: read the command state before deciding (D-055).
    pub const READ_THEN_DECIDE: &str = "read-then-decide";
    // D-060 §9.1 also defines `retry-same-command` and `stop`. No refusal in
    // this crate emits them yet (NODE_BUSY is retried by `code`, and the
    // D-057 `fenced` refusals live in the writer path), so they are documented
    // in protocol §9.1 rather than declared unused here.
    /// The request itself is wrong.
    pub const FIX_REQUEST: &str = "fix-request";
}

/// The `next` step a refusal carries (D-060 §9.1): a stable `action`, one
/// human/agent-readable `hint`, and optional `grant` / `interactionId` /
/// `commandId` that name what the caller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextStep {
    /// One of [`action`].
    pub action: &'static str,
    /// What the caller should do instead of retrying blindly.
    pub hint: String,
    /// The grant to ask the owner for.
    pub grant: Option<String>,
    /// The pending interaction to answer.
    pub interaction_id: Option<String>,
    /// The command whose state answers an unknown delivery.
    pub command_id: Option<String>,
}

impl NextStep {
    /// A step with an action and a hint.
    pub fn new(action: &'static str, hint: impl Into<String>) -> Self {
        Self {
            action,
            hint: hint.into(),
            grant: None,
            interaction_id: None,
            command_id: None,
        }
    }

    /// Name the grant the caller needs.
    pub fn grant(mut self, grant: impl Into<String>) -> Self {
        self.grant = Some(grant.into());
        self
    }

    /// Name the pending interaction the caller waits on.
    pub fn interaction(mut self, id: impl Into<String>) -> Self {
        self.interaction_id = Some(id.into());
        self
    }

    /// Name the command whose state answers an unknown delivery.
    pub fn command(mut self, id: impl Into<String>) -> Self {
        self.command_id = Some(id.into());
        self
    }

    fn json(&self) -> serde_json::Value {
        let mut value = json!({ "action": self.action, "hint": self.hint });
        if let Some(object) = value.as_object_mut() {
            if let Some(grant) = &self.grant {
                object.insert("grant".into(), json!(grant));
            }
            if let Some(id) = &self.interaction_id {
                object.insert("interactionId".into(), json!(id));
            }
            if let Some(id) = &self.command_id {
                object.insert("commandId".into(), json!(id));
            }
        }
        value
    }
}

/// Recoverable Hub failures.
#[derive(Debug, Error)]
pub enum HubError {
    /// Missing or invalid device / Node credential.
    #[error("unauthenticated")]
    Unauthenticated,
    /// Origin/Host mismatch or insufficient scope.
    #[error("forbidden")]
    Forbidden,
    /// Origin/scope refusal with a caller-readable reason; renders 403 with
    /// the same `FORBIDDEN` code as [`Self::Forbidden`].
    #[error("{0}")]
    ForbiddenReason(String),
    /// Agent operation is held until a human approves this exact action.
    #[error(
        "human approval required; answer interaction {interaction_id}, then retry with approvalId"
    )]
    ApprovalRequired { interaction_id: String },
    /// An existing refusal annotated with a stable lowercase `reason` and a
    /// `next` step (D-060 §9.1). Status, `code` and `error` text are exactly
    /// the wrapped error's; only the two additive body fields are new, so
    /// wrapping can never change an authorization decision.
    #[error("{source}")]
    Actionable {
        /// Stable lowercase reason (`grant-required`, `out-of-scope`, …).
        reason: &'static str,
        /// The next step for the caller (boxed: the variant must stay small).
        next: Box<NextStep>,
        /// The refusal being annotated.
        source: Box<HubError>,
    },
    /// Target does not exist.
    #[error("not found")]
    NotFound,
    /// Caller sent an unusable body or ID.
    #[error("{0}")]
    BadRequest(String),
    /// Idempotency key reused with a different payload.
    #[error("{0}")]
    Conflict(String),
    /// D-057 §7.3: an Agent-initiated write failed the commit-time authority
    /// check — the initiator was fenced/paused or its device row is gone.
    /// Maps to the existing 409 shape with reason code `fenced`; nothing was
    /// written.
    #[error("initiator fenced")]
    Fenced,
    /// D-057 §6.3/§7.6 (ma-fence): a lineage-state or fence-capability
    /// refusal. 409 with a stable lowercase `reason` (`not-paused`,
    /// `pending-interaction`, `approval-in-flight`, `superseded`,
    /// `node-lacks-fence`) and the code `LINEAGE_CONFLICT`, or
    /// `node-lacks-fence` for the capability refusal (protocol.md §9).
    #[error("{message}")]
    LineageConflict {
        /// Stable lowercase reason.
        reason: &'static str,
        /// Human-readable detail.
        message: String,
    },
    /// Interaction deadline already passed.
    #[error("interaction expired")]
    Expired,
    /// A Node hello lost to a newer hello for the same host: the newer link
    /// is installed and this socket is closed without installing (r41 item 1).
    #[error("node link superseded by a newer hello")]
    LinkSuperseded,
    /// A different commandId already committed the unique answer.
    #[error("interaction already answered")]
    Superseded {
        /// Winning command id.
        winner: String,
    },
    /// No host satisfied placement constraints (D-013).
    #[error("placement unsatisfiable")]
    Unsatisfiable {
        /// Human-readable rejection reasons, one per considered host or rule.
        reasons: Vec<String>,
    },
    /// An explicit gateway request has no configured provider on eligible hosts.
    #[error("provider not configured")]
    ProviderNotConfigured {
        /// Missing provider configuration and how the caller can fix it.
        reasons: Vec<String>,
    },
    /// Targeted host has no live Hub<->Node session.
    #[error("host {host_id} is offline")]
    HostOffline {
        /// `hst_…` that has no connected Node.
        host_id: String,
    },
    /// A `via` model-API delivery cannot be honoured (D-047 §B.5).
    ///
    /// Every variant is a refusal: there is deliberately no fallback error,
    /// because a proxied session must never silently fall back to direct
    /// delivery — that would push the request and the gateway credential onto
    /// a host the operator excluded (D-035).
    #[error("{message}")]
    ApiViaRefusal {
        /// Stable wire code (also the HTTP body `code`).
        code: remuda_protocol::ApiViaRefusal,
        /// Human-readable detail.
        message: String,
    },
    /// No supply candidate passed admission; the task is explicitly deferred
    /// rather than silently downgraded (coordinator §4.4 step 8). The carried
    /// JSON is the full supply decision (`reasons[]`/`rejected[]`).
    #[error("supply deferred")]
    SupplyDeferred {
        /// Machine-readable supply decision for the ledger/bot card.
        decision: serde_json::Value,
    },
    /// An explicit model/supply pin matched no configured candidate. A pin is
    /// a hard constraint (coordinator §4.3): the request fails instead of
    /// silently running a different model.
    #[error("pin refused: no listed model/supply matches the pin")]
    PinRefused {
        /// The rejected pin (`model` / `supplyId` / `harness`).
        pin: serde_json::Value,
        /// Human-readable reasons: the pin plus up to five `did you mean` ids.
        reasons: Vec<String>,
        /// Closest listed ids (≤5).
        suggestions: Vec<String>,
    },
    /// The Node RPC link is saturated and the call was refused **before it was
    /// sent**, so nothing happened on the Node. Retryable by construction:
    /// bulk reads draw from a smaller sub-budget than control RPCs (see
    /// `transport`), and `retryAfterMs` tells the client one poll cycle to
    /// back off. Never mapped to 500: a refused read must not look like a
    /// broken Hub (2026-09-19 demo: per-row screen reads surfaced INTERNAL).
    #[error("node busy: too many in-flight node rpcs; retry after {retry_after_ms} ms")]
    NodeBusy {
        /// Hint for the client's next attempt.
        retry_after_ms: u64,
    },
    /// SQLite or journal actor mailbox.
    #[error("store: {0}")]
    Store(#[from] crate::store::StoreError),
    /// Internal invariant.
    #[error("{0}")]
    Internal(String),
    /// A non-replayable command's already-persisted terminal outcome, replayed
    /// verbatim (D-055 round 2): the Hub must answer the retry with the exact
    /// HTTP status and JSON body the first attempt produced instead of
    /// inventing a fresh `replayed:true` success.
    #[error("stored command outcome {status}")]
    StoredOutcome {
        /// The status the original attempt answered with.
        status: u16,
        /// The exact JSON body the original attempt answered with.
        body: serde_json::Value,
    },
}

impl HubError {
    /// A D-057 lineage-state refusal (409 `LINEAGE_CONFLICT` + `reason`).
    pub(crate) fn lineage_conflict(reason: &'static str, message: impl Into<String>) -> Self {
        Self::LineageConflict {
            reason,
            message: message.into(),
        }
    }

    /// D-057 §7.6: work a continuity lineage initiated, addressed to a Node
    /// that did not advertise `lineageFence` (409, reason `node-lacks-fence`).
    pub(crate) fn node_lacks_fence(host_id: &str) -> Self {
        Self::LineageConflict {
            reason: remuda_protocol::NODE_LACKS_FENCE_REASON,
            message: format!(
                "host {host_id} does not support lineage fences; upgrade its Node first"
            ),
        }
    }

    /// Construct a D-047 delivery refusal.
    pub fn api_via(code: remuda_protocol::ApiViaRefusal, message: impl Into<String>) -> Self {
        Self::ApiViaRefusal {
            code,
            message: message.into(),
        }
    }

    /// Annotate this refusal with a machine `reason` and a `next` step.
    pub fn with_next(self, reason: &'static str, next: NextStep) -> Self {
        Self::Actionable {
            reason,
            next: Box::new(next),
            source: Box::new(self),
        }
    }

    /// 403 for an Agent seat that lacks `grant` (design §2.5, D-060).
    pub fn missing_grant(grant: &str) -> Self {
        Self::Forbidden.with_next(
            "grant-required",
            NextStep::new(
                action::ASK_OWNER,
                format!(
                    "this seat does not hold `{grant}`; do not retry: grants are \
                     fixed when a seat is launched. Report the target and action to \
                     the owner and ask for a seat created with `{grant}`"
                ),
            )
            .grant(grant),
        )
    }

    /// 403 for a target or placement outside the caller's delegation scope.
    pub fn out_of_scope() -> Self {
        Self::Forbidden.with_next(
            "out-of-scope",
            NextStep::new(
                action::ASK_OWNER,
                "the target is outside this seat's delegation scope (project, host or \
                 workspace); do not retry: the scope is fixed when a seat is launched. Use a \
                 target inside the scope, or ask the owner for a seat launched with a scope \
                 that covers the target",
            ),
        )
    }

    /// 403 for a target that is not in the caller's lineage.
    pub fn not_owner() -> Self {
        Self::Forbidden.with_next(
            "not-owner",
            NextStep::new(
                action::ASK_OWNER,
                "the target is not yours or a child of yours; do not retry — report \
                 what you needed to the owner",
            ),
        )
    }

    /// 403 for an operator-only action or route reaching an Agent seat.
    pub fn operator_only() -> Self {
        Self::Forbidden.with_next(
            "operator-only",
            NextStep::new(
                action::ASK_OWNER,
                "this action is operator-only and no Agent seat holds it; do not \
                 retry — report what you needed to the owner so a person can do it",
            ),
        )
    }

    /// 403 for an Agent seat asking for an operator-level spec field.
    pub fn operator_field(hint: impl Into<String>) -> Self {
        Self::Forbidden.with_next("operator-only", NextStep::new(action::FIX_REQUEST, hint))
    }

    /// 403 for a route the Agent middleware does not admit.
    pub fn agent_route() -> Self {
        Self::Forbidden.with_next(
            "agent-route",
            NextStep::new(
                action::ASK_OWNER,
                "this route is not opened to Agent seats; do not retry — report the \
                 action you needed to the owner",
            ),
        )
    }

    /// 403 for a caller whose instance row is gone (retired or replaced).
    pub fn stale_caller() -> Self {
        Self::Forbidden.with_next(
            "stale-caller",
            NextStep::new(
                action::ASK_OWNER,
                "the instance behind this credential no longer exists (retired or \
                 replaced); do not retry with it — stop and report so a live seat can \
                 continue",
            ),
        )
    }

    /// 403 for an approvalId that cannot authorize this call (D-017).
    pub fn approval_invalid() -> Self {
        Self::Forbidden.with_next(
            "approval-invalid",
            NextStep::new(
                action::FIX_REQUEST,
                "this approvalId is unknown, expired, already used, not yet answered, \
                 or for a different action; do not reuse it — repeat the call without \
                 approvalId to open a new approval, then wait for the owner",
            ),
        )
    }

    /// 409 for an `instance.configure` replay whose delivery is not yet known
    /// (D-055): the caller reads the command row instead of re-sending.
    pub fn delivery_unknown(command_id: &str) -> Self {
        // Byte-identical to the pre-D-060 body: the command id belongs in
        // `next.commandId` only, never in the message an old client may match.
        Self::Conflict(
            "instance.configure is still in flight; poll GET \
             /v1/instances/{id}/commands/{commandId} for its outcome"
                .to_string(),
        )
        .with_next(
            "delivery-unknown",
            NextStep::new(
                action::READ_THEN_DECIDE,
                "delivery is not yet known and the Node may still be applying it; do \
                 not re-send — read the command state until it settles, then decide",
            )
            .command(command_id),
        )
    }

    fn status(&self) -> StatusCode {
        match self {
            Self::Actionable { source, .. } => source.status(),
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::ForbiddenReason(_) => StatusCode::FORBIDDEN,
            Self::ApprovalRequired { .. } => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::Fenced => StatusCode::CONFLICT,
            Self::LineageConflict { .. } => StatusCode::CONFLICT,
            Self::LinkSuperseded => StatusCode::CONFLICT,
            Self::Expired => StatusCode::GONE,
            Self::Superseded { .. } => StatusCode::CONFLICT,
            Self::Unsatisfiable { .. } | Self::ProviderNotConfigured { .. } => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            Self::SupplyDeferred { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::PinRefused { .. } => StatusCode::CONFLICT,
            Self::HostOffline { .. } => StatusCode::CONFLICT,
            Self::NodeBusy { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::ApiViaRefusal { code, .. } => StatusCode::from_u16(code.status()).unwrap(),
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::StoredOutcome { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }

    /// The HTTP status and JSON body this error renders — the pair a
    /// non-replayable command persists with its terminal outcome so a replay
    /// can reproduce the original answer byte-for-byte.
    pub(crate) fn status_code_and_body(&self) -> (u16, serde_json::Value) {
        if let Self::StoredOutcome { status, body } = self {
            return (*status, body.clone());
        }
        (self.status().as_u16(), self.response_body())
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Actionable { source, .. } => source.code(),
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
            Self::ForbiddenReason(_) => "FORBIDDEN",
            Self::ApprovalRequired { .. } => "HUMAN_APPROVAL_REQUIRED",
            Self::NotFound => "NOT_FOUND",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Conflict(_) => "COMMAND_ID_CONFLICT",
            Self::Fenced => "fenced",
            Self::LineageConflict { reason, .. }
                if *reason == remuda_protocol::NODE_LACKS_FENCE_REASON =>
            {
                remuda_protocol::NODE_LACKS_FENCE_REASON
            }
            Self::LineageConflict { .. } => "LINEAGE_CONFLICT",
            Self::LinkSuperseded => "LINK_SUPERSEDED",
            Self::Expired => "INTERACTION_EXPIRED",
            Self::Superseded { .. } => "INTERACTION_SUPERSEDED",
            Self::Unsatisfiable { .. } => "PLACEMENT_UNSATISFIABLE",
            Self::ProviderNotConfigured { .. } => "PROVIDER_NOT_CONFIGURED",
            Self::SupplyDeferred { .. } => "SUPPLY_DEFERRED",
            Self::PinRefused { .. } => "PIN_REFUSED",
            Self::HostOffline { .. } => "HOST_OFFLINE",
            Self::NodeBusy { .. } => "NODE_BUSY",
            // D-047: the stable lowercase refusal vocabulary is the contract
            // (`api-via-unknown-host` / `-host-offline` / `-unsupported` /
            // `-unreachable`), not a SCREAMING_SNAKE hub code.
            Self::ApiViaRefusal { code, .. } => code.as_str(),
            Self::Store(_) | Self::Internal(_) => "INTERNAL",
            // A replayed stored outcome keeps the body's own code; this arm is
            // only reached by internal callers that never render it directly.
            Self::StoredOutcome { .. } => "STORED_OUTCOME",
        }
    }

    /// The JSON body paired with [`Self::status`] (see
    /// [`Self::status_code_and_body`]).
    fn response_body(&self) -> serde_json::Value {
        if let Self::Actionable {
            reason,
            next,
            source,
        } = self
        {
            let mut body = source.response_body();
            body["reason"] = json!(reason);
            body["next"] = next.json();
            return body;
        }
        let mut body = json!({
            "error": self.to_string(),
            "code": self.code(),
        });
        if let Self::ApprovalRequired { interaction_id } = &self {
            body["interactionId"] = json!(interaction_id);
            body["reason"] = json!("approval-required");
            body["next"] = NextStep::new(
                action::AWAIT_APPROVAL,
                "a human must answer the approval; do not resubmit — then retry the same \
                 request with approvalId",
            )
            .interaction(interaction_id)
            .json();
        }
        if let Self::Unsatisfiable { reasons } | Self::ProviderNotConfigured { reasons } = &self
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("reasons".into(), json!(reasons));
        }
        if let Self::SupplyDeferred { decision } = &self
            && let Some(obj) = body.as_object_mut()
            && let Some(decision) = decision.as_object()
        {
            // The decision IS the response body: caller/bot/CLI need the
            // ranked list and rejection reasons verbatim.
            for (key, value) in decision {
                obj.insert(key.clone(), value.clone());
            }
        }
        if let Self::PinRefused {
            pin,
            reasons,
            suggestions,
        } = &self
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("pin".into(), pin.clone());
            obj.insert("reasons".into(), json!(reasons));
            obj.insert("suggestions".into(), json!(suggestions));
        }
        if let Self::Superseded { winner } = &self
            && !winner.is_empty()
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("winner".into(), json!(winner));
        }
        if let Self::HostOffline { host_id } = &self
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("hostId".into(), json!(host_id));
        }
        if let Self::LineageConflict { reason, .. } = &self
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("reason".into(), json!(reason));
        }
        if let Self::NodeBusy { retry_after_ms } = &self
            && let Some(obj) = body.as_object_mut()
        {
            obj.insert("retryAfterMs".into(), json!(retry_after_ms));
            obj.insert("retryable".into(), json!(true));
        }
        body
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        if let Self::StoredOutcome { status, body } = &self {
            let status = StatusCode::from_u16(*status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return (status, Json(body.clone())).into_response();
        }
        let status = self.status();
        let body = self.response_body();
        (status, Json(body)).into_response()
    }
}

/// JSON-RPC 2.0 error object for the Node socket.
pub fn rpc_error(id: serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// JSON-RPC 2.0 success object.
pub fn rpc_ok(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actionable_wrapper_keeps_status_code_and_message() {
        let plain = HubError::Forbidden;
        let wrapped = HubError::missing_grant("land");
        assert_eq!(wrapped.status(), plain.status());
        assert_eq!(wrapped.code(), plain.code());
        assert_eq!(wrapped.to_string(), plain.to_string());
        let (status, body) = wrapped.status_code_and_body();
        assert_eq!(status, 403);
        assert_eq!(body["code"], "FORBIDDEN");
        assert_eq!(body["error"], "forbidden");
        assert_eq!(body["reason"], "grant-required");
        assert_eq!(body["next"]["action"], "ask-owner");
        assert_eq!(body["next"]["grant"], "land");
        assert!(
            body["next"]["hint"].as_str().unwrap().contains("`land`"),
            "{body}"
        );
    }

    #[test]
    fn refusal_vocabulary_is_lowercase_and_hyphenated() {
        let bodies = [
            HubError::out_of_scope(),
            HubError::not_owner(),
            HubError::operator_only(),
            HubError::operator_field("hint"),
            HubError::agent_route(),
            HubError::stale_caller(),
            HubError::approval_invalid(),
        ];
        for error in bodies {
            let (_, body) = error.status_code_and_body();
            assert_eq!(body["code"], "FORBIDDEN", "{body}");
            let reason = body["reason"].as_str().unwrap();
            assert!(
                reason.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "reason {reason} is a stable lowercase reason (D-060 §9.1)"
            );
            let action = body["next"]["action"].as_str().unwrap();
            assert!(
                [
                    action::ASK_OWNER,
                    action::AWAIT_APPROVAL,
                    action::READ_THEN_DECIDE,
                    action::FIX_REQUEST,
                ]
                .contains(&action),
                "action {action} is in the D-060 vocabulary"
            );
            assert!(!body["next"]["hint"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn approval_required_names_the_pending_interaction() {
        let (status, body) = HubError::ApprovalRequired {
            interaction_id: "int_1".into(),
        }
        .status_code_and_body();
        assert_eq!(status, 409);
        assert_eq!(body["code"], "HUMAN_APPROVAL_REQUIRED");
        assert_eq!(body["interactionId"], "int_1");
        assert_eq!(body["reason"], "approval-required");
        assert_eq!(body["next"]["action"], "await-approval");
        assert_eq!(body["next"]["interactionId"], "int_1");
    }

    #[test]
    fn delivery_unknown_names_the_command_to_read() {
        let (status, body) = crate::http::configure_in_flight("cmd_9").status_code_and_body();
        assert_eq!(status, 409);
        assert_eq!(body["code"], "COMMAND_ID_CONFLICT");
        // The message is pinned to the pre-D-060 bytes: the id lives only in
        // `next.commandId`.
        assert_eq!(
            body["error"],
            json!(
                "instance.configure is still in flight; poll GET \
                 /v1/instances/{id}/commands/{commandId} for its outcome"
            )
        );
        assert_eq!(body["reason"], "delivery-unknown");
        assert_eq!(body["next"]["action"], "read-then-decide");
        assert_eq!(body["next"]["commandId"], "cmd_9");
        assert!(
            body["next"]["hint"]
                .as_str()
                .unwrap()
                .contains("do not re-send")
        );
    }

    #[test]
    fn errors_without_guidance_keep_the_old_shape() {
        let (_, body) = HubError::NotFound.status_code_and_body();
        assert!(body.get("next").is_none() && body.get("reason").is_none());
    }
}
