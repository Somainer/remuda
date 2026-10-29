//! HTTP and JSON-RPC errors.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use thiserror::Error;

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
    /// An existing refusal annotated with a stable machine `reason` and a
    /// `next` step for the caller. Status, `code` and `error` text are exactly
    /// the wrapped error's; only the two additive body fields are new, so
    /// wrapping can never change an authorization decision.
    #[error("{source}")]
    Actionable {
        /// Stable machine reason (`MISSING_GRANT`, `OUT_OF_SCOPE`, …).
        reason: &'static str,
        /// What the refused caller should do instead of retrying.
        next: String,
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
    /// Interaction deadline already passed.
    #[error("interaction expired")]
    Expired,
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
    /// Construct a D-047 delivery refusal.
    pub fn api_via(code: remuda_protocol::ApiViaRefusal, message: impl Into<String>) -> Self {
        Self::ApiViaRefusal {
            code,
            message: message.into(),
        }
    }

    /// Annotate this refusal with a machine `reason` and a `next` step.
    pub fn with_next(self, reason: &'static str, next: impl Into<String>) -> Self {
        Self::Actionable {
            reason,
            next: next.into(),
            source: Box::new(self),
        }
    }

    /// 403 for an Agent seat that lacks `grant` (design §2.5).
    pub fn missing_grant(grant: &str) -> Self {
        Self::Forbidden.with_next(
            "MISSING_GRANT",
            format!(
                "This seat does not hold the `{grant}` grant. Do not retry: grants are fixed \
                 when a seat is launched. Report the blocked action to the agent or person \
                 that launched you and ask for a seat created with `{grant}`."
            ),
        )
    }

    /// 403 for a target or placement outside the caller's delegation scope.
    pub fn out_of_scope() -> Self {
        Self::Forbidden.with_next(
            "OUT_OF_SCOPE",
            "The target is outside this seat's delegation scope (project, host or workspace). \
             Do not retry. Use a target inside your scope, or ask the owner to widen it.",
        )
    }

    /// 403 for an action Agent seats are never given.
    pub fn agent_not_permitted() -> Self {
        Self::Forbidden.with_next(
            "AGENT_NOT_PERMITTED",
            "Agent seats cannot do this, or the target is not in your lineage. Do not retry; \
             report to the owner what you needed so they can do it or change the plan.",
        )
    }

    /// 403 for a caller whose instance no longer exists (retired or stale).
    pub fn caller_retired() -> Self {
        Self::Forbidden.with_next(
            "CALLER_RETIRED",
            "The instance behind this credential no longer exists (retired or replaced). \
             Do not retry with this credential; stop and report so a live seat can continue.",
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
            body["next"] = json!(next);
            return body;
        }
        let mut body = json!({
            "error": self.to_string(),
            "code": self.code(),
        });
        if let Self::ApprovalRequired { interaction_id } = &self {
            body["interactionId"] = json!(interaction_id);
            body["reason"] = json!("HUMAN_APPROVAL_REQUIRED");
            body["next"] = json!(
                "An approval is already pending for this exact action. Do not resubmit or \
                 retry: wait for the owner to answer it, then repeat the same call with \
                 the approvalId."
            );
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
        assert_eq!(body["reason"], "MISSING_GRANT");
        assert!(body["next"].as_str().unwrap().contains("`land`"));
    }

    #[test]
    fn caller_retired_and_delivery_unknown_carry_next() {
        let (_, body) = HubError::caller_retired().status_code_and_body();
        assert_eq!(body["reason"], "CALLER_RETIRED");
        assert!(body["next"].as_str().unwrap().contains("Do not retry"));

        let (status, body) = crate::http::configure_in_flight().status_code_and_body();
        assert_eq!(status, 409);
        assert_eq!(body["code"], "COMMAND_ID_CONFLICT");
        assert_eq!(body["reason"], "DELIVERY_UNKNOWN");
        assert!(body["next"].as_str().unwrap().contains("Do not re-send"));
    }

    #[test]
    fn errors_without_guidance_keep_the_old_shape() {
        let (_, body) = HubError::NotFound.status_code_and_body();
        assert!(body.get("next").is_none() && body.get("reason").is_none());
    }
}
