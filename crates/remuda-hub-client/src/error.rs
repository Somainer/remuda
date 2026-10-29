//! Failures talking to Hub HTTP or follow WS.

use serde::Deserialize;
use thiserror::Error;

/// The structured part of a Hub refusal body.
///
/// Every field is optional so a body from an older Hub (which sends only
/// `error` and `code`), or one that is not JSON at all, still parses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Refusal {
    /// Human-readable message (`error`).
    #[serde(default, rename = "error", deserialize_with = "lenient_string")]
    pub message: Option<String>,
    /// Coarse stable code (`FORBIDDEN`, `HUMAN_APPROVAL_REQUIRED`, …).
    #[serde(default, deserialize_with = "lenient_string")]
    pub code: Option<String>,
    /// Stable lowercase reason (`grant-required`, `out-of-scope`, …).
    #[serde(default, deserialize_with = "lenient_string")]
    pub reason: Option<String>,
    /// The next step (`{action, hint, …}`), when the Hub supplied one.
    #[serde(default, deserialize_with = "lenient_next")]
    pub next: Option<NextStep>,
}

/// `next` (D-060 §9.1): a stable `action` and one human/agent-readable
/// `hint`, plus whatever the caller needs to act on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct NextStep {
    /// `ask-owner`, `await-approval`, `read-then-decide`, …
    #[serde(default, deserialize_with = "lenient_string")]
    pub action: Option<String>,
    /// What to do instead of retrying blindly.
    #[serde(default, deserialize_with = "lenient_string")]
    pub hint: Option<String>,
    /// Grant the caller must ask the owner for.
    #[serde(default, deserialize_with = "lenient_string")]
    pub grant: Option<String>,
    /// Pending interaction to wait on.
    #[serde(default, rename = "interactionId", deserialize_with = "lenient_string")]
    pub interaction_id: Option<String>,
    /// Command whose state answers an unknown delivery.
    #[serde(default, rename = "commandId", deserialize_with = "lenient_string")]
    pub command_id: Option<String>,
}

impl NextStep {
    /// The line the CLI prints: `action — hint` (either part may be absent).
    pub fn render(&self) -> String {
        match (self.action.as_deref(), self.hint.as_deref()) {
            (Some(action), Some(hint)) => format!("{action} — {hint}"),
            (Some(action), None) => action.to_string(),
            (None, Some(hint)) => hint.to_string(),
            (None, None) => String::new(),
        }
    }
}

impl Refusal {
    /// Parse a Hub error body. `None` when the body is not a JSON object.
    pub fn parse(body: &str) -> Option<Self> {
        serde_json::from_str(body).ok()
    }
}

/// Render an HTTP refusal for humans and agents: the message, then the
/// `reason` and `next.hint` the Hub supplied (nothing extra for an older Hub).
pub fn render_http(status: u16, body: &str) -> String {
    let Some(refusal) = Refusal::parse(body) else {
        return format!("hub HTTP {status}: {body}");
    };
    let Some(message) = refusal.message.as_deref() else {
        return format!("hub HTTP {status}: {body}");
    };
    let next = refusal
        .next
        .as_ref()
        .map(NextStep::render)
        .unwrap_or_default();
    if refusal.reason.is_none() && next.is_empty() {
        // An older Hub body (or a non-refusal error): unchanged rendering.
        return format!("hub HTTP {status}: {body}");
    }
    let mut rendered = format!("hub HTTP {status}: {message}");
    if let Some(reason) = refusal.reason.as_deref() {
        rendered.push_str(&format!("\n  reason: {reason}"));
    }
    if !next.is_empty() {
        rendered.push_str(&format!("\n  next: {next}"));
    }
    rendered
}

/// `next` as an object, or as the bare hint string an early build sent; any
/// other shape reads as absent instead of failing the whole parse.
fn lenient_next<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<NextStep>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::Object(map) => Some(
            serde_json::from_value(serde_json::Value::Object(map))
                .unwrap_or_else(|_| NextStep::default()),
        ),
        serde_json::Value::String(hint) => Some(NextStep {
            hint: Some(hint),
            ..NextStep::default()
        }),
        _ => None,
    })
}

/// A non-string value (some bodies reuse `reason` for objects) reads as absent
/// rather than failing the whole parse.
fn lenient_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    })
}

/// Failures talking to Hub HTTP / WS.
#[derive(Debug, Error)]
pub enum ClientError {
    /// Neither a device token nor a bootstrap token was provided.
    #[error("set a device token or bootstrap token (REMUDA_TOKEN / REMUDA_BOOTSTRAP_TOKEN)")]
    NoCredentials,
    /// Hub returned a non-success status.
    #[error("{}", render_http(*status, body))]
    Http {
        /// Status code.
        status: u16,
        /// Response body (no secrets).
        body: String,
    },
    /// `POST /v1/fleet/*` is specified in proposal.md §4.6 but not on this Hub.
    #[error(
        "Hub fleet HTTP is not deployed yet (HTTP {status} on {path}). \
         TODO: POST /v1/fleet/instances, GET /v1/fleet/:id, POST /v1/fleet/:id/commands \
         as specified in docs/design/proposal.md §4.6"
    )]
    FleetUnavailable {
        /// Status code (typically 404).
        status: u16,
        /// Request path.
        path: String,
    },
    /// Client-side placement found no online host.
    #[error("PLACEMENT_UNSATISFIABLE: {0}")]
    Placement(String),
    /// Outbound HTTP.
    #[error("hub request: {0}")]
    Transport(#[from] reqwest::Error),
    /// Response was not JSON.
    #[error("hub JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// WebSocket upgrade or frame.
    #[error("hub websocket: {0}")]
    Websocket(String),
    /// Internal lock / invariant.
    #[error("{0}")]
    Internal(String),
}

impl ClientError {
    /// The structured refusal fields of an HTTP error body, tolerating their
    /// absence (older Hub) and non-JSON bodies. `None` for non-HTTP errors.
    pub fn refusal(&self) -> Option<Refusal> {
        let Self::Http { body, .. } = self else {
            return None;
        };
        Refusal::parse(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http(body: &str) -> ClientError {
        ClientError::Http {
            status: 403,
            body: body.into(),
        }
    }

    #[test]
    fn parses_code_reason_and_next() {
        let refusal = http(
            r#"{"error":"forbidden","code":"FORBIDDEN","reason":"grant-required",
                "next":{"action":"ask-owner","grant":"land","hint":"ask the launcher"}}"#,
        )
        .refusal()
        .unwrap();
        assert_eq!(refusal.code.as_deref(), Some("FORBIDDEN"));
        assert_eq!(refusal.reason.as_deref(), Some("grant-required"));
        let next = refusal.next.unwrap();
        assert_eq!(next.action.as_deref(), Some("ask-owner"));
        assert_eq!(next.grant.as_deref(), Some("land"));
        assert_eq!(next.render(), "ask-owner — ask the launcher");
    }

    #[test]
    fn approval_and_delivery_next_carry_their_ids() {
        let approval = http(
            r#"{"error":"x","code":"HUMAN_APPROVAL_REQUIRED","reason":"approval-required",
                "interactionId":"int_1",
                "next":{"action":"await-approval","interactionId":"int_1","hint":"wait"}}"#,
        )
        .refusal()
        .unwrap()
        .next
        .unwrap();
        assert_eq!(approval.interaction_id.as_deref(), Some("int_1"));
        let delivery = http(
            r#"{"error":"x","code":"COMMAND_ID_CONFLICT","reason":"delivery-unknown",
                "next":{"action":"read-then-decide","commandId":"cmd_9","hint":"read it"}}"#,
        )
        .refusal()
        .unwrap()
        .next
        .unwrap();
        assert_eq!(delivery.command_id.as_deref(), Some("cmd_9"));
    }

    #[test]
    fn old_hub_body_without_new_fields_still_parses() {
        let refusal = http(r#"{"error":"forbidden","code":"FORBIDDEN"}"#)
            .refusal()
            .unwrap();
        assert_eq!(refusal.message.as_deref(), Some("forbidden"));
        assert_eq!(refusal.reason, None);
        assert_eq!(refusal.next, None);
    }

    #[test]
    fn display_puts_next_under_the_message() {
        let error = http(
            r#"{"error":"forbidden","code":"FORBIDDEN","reason":"grant-required",
                "next":{"action":"ask-owner","hint":"ask the launcher"}}"#,
        );
        assert_eq!(
            error.to_string(),
            "hub HTTP 403: forbidden\n  reason: grant-required\n  next: ask-owner — ask the launcher"
        );
        assert_eq!(
            http(r#"{"error":"forbidden","code":"FORBIDDEN"}"#).to_string(),
            r#"hub HTTP 403: {"error":"forbidden","code":"FORBIDDEN"}"#
        );
    }

    #[test]
    fn non_json_and_odd_shapes_are_tolerated() {
        assert_eq!(http("Forbidden").refusal(), None);
        let refusal = http(r#"{"error":"x","reason":{"state":"unknown"},"next":7}"#)
            .refusal()
            .unwrap();
        assert_eq!(refusal.reason, None);
        assert_eq!(refusal.next, None);
        // A bare string `next` (an unreleased build) reads as its hint.
        let bare = http(r#"{"error":"x","reason":"grant-required","next":"do not retry"}"#)
            .refusal()
            .unwrap();
        assert_eq!(bare.next.unwrap().render(), "do not retry");
        assert_eq!(ClientError::NoCredentials.refusal(), None);
    }
}
