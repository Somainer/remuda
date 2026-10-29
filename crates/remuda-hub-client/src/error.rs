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
    /// Fine-grained stable machine reason (`MISSING_GRANT`, `OUT_OF_SCOPE`, …).
    #[serde(default, deserialize_with = "lenient_string")]
    pub reason: Option<String>,
    /// What the refused caller should do instead of retrying blindly.
    #[serde(default, deserialize_with = "lenient_string")]
    pub next: Option<String>,
}

impl Refusal {
    /// Parse a Hub error body. `None` when the body is not a JSON object.
    pub fn parse(body: &str) -> Option<Self> {
        serde_json::from_str(body).ok()
    }
}

/// Render an HTTP refusal for humans and agents: the message, then the
/// `reason` and `next` the Hub supplied (nothing extra for an older Hub).
pub fn render_http(status: u16, body: &str) -> String {
    let Some(refusal) = Refusal::parse(body) else {
        return format!("hub HTTP {status}: {body}");
    };
    let (Some(message), Some(next)) = (refusal.message.as_deref(), refusal.next.as_deref()) else {
        return format!("hub HTTP {status}: {body}");
    };
    let mut rendered = format!("hub HTTP {status}: {message}");
    if let Some(reason) = refusal.reason.as_deref() {
        rendered.push_str(&format!("\n  reason: {reason}"));
    }
    rendered.push_str(&format!("\n  next: {next}"));
    rendered
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
            r#"{"error":"forbidden","code":"FORBIDDEN","reason":"MISSING_GRANT","next":"ask"}"#,
        )
        .refusal()
        .unwrap();
        assert_eq!(refusal.code.as_deref(), Some("FORBIDDEN"));
        assert_eq!(refusal.reason.as_deref(), Some("MISSING_GRANT"));
        assert_eq!(refusal.next.as_deref(), Some("ask"));
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
            r#"{"error":"forbidden","code":"FORBIDDEN","reason":"MISSING_GRANT","next":"ask"}"#,
        );
        assert_eq!(
            error.to_string(),
            "hub HTTP 403: forbidden\n  reason: MISSING_GRANT\n  next: ask"
        );
        assert_eq!(
            http(r#"{"error":"forbidden","code":"FORBIDDEN"}"#).to_string(),
            r#"hub HTTP 403: {"error":"forbidden","code":"FORBIDDEN"}"#
        );
    }

    #[test]
    fn non_json_and_non_string_fields_are_tolerated() {
        assert_eq!(http("Forbidden").refusal(), None);
        let refusal = http(r#"{"error":"x","reason":{"state":"unknown"},"next":7}"#)
            .refusal()
            .unwrap();
        assert_eq!(refusal.reason, None);
        assert_eq!(refusal.next, None);
        assert_eq!(ClientError::NoCredentials.refusal(), None);
    }
}
