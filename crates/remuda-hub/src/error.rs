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
    /// Agent operation is held until a human approves this exact action.
    #[error(
        "human approval required; answer interaction {interaction_id}, then retry with approvalId"
    )]
    ApprovalRequired { interaction_id: String },
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
}

impl HubError {
    fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
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
            Self::Store(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
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
            Self::Store(_) | Self::Internal(_) => "INTERNAL",
        }
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut body = json!({
            "error": self.to_string(),
            "code": self.code(),
        });
        if let Self::ApprovalRequired { interaction_id } = &self {
            body["interactionId"] = json!(interaction_id);
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
