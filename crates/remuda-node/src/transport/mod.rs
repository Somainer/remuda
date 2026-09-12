//! Hub JSON-RPC control-plane transports.
//!
//! [`crate::HubCarrier`] / [`crate::StdioCarrier`] speak SSH-friendly NDJSON
//! application frames. [`NodeTransport`] is the Hub `/v1/node` JSON-RPC 2.0
//! carrier used by outbound WSS. A stdio JSON-RPC adapter is not defined here.

use crate::NodeError;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

mod wss;

pub use wss::{HubRequest, JournalSender, WssCarrier, WssConfig, WssLink};

/// Hub JSON-RPC control-plane carrier (one JSON object per message).
pub trait NodeTransport: Send {
    /// Stable diagnostic label (`outbound-wss`).
    fn kind(&self) -> &'static str;

    /// Send one JSON object.
    fn send_json(&mut self, value: &Value) -> impl Future<Output = Result<(), NodeError>> + Send;

    /// Receive one JSON object. `Ok(None)` is a clean close.
    fn recv_json(&mut self) -> impl Future<Output = Result<Option<Value>, NodeError>> + Send;

    /// Close the current connection.
    fn close(&mut self) -> impl Future<Output = Result<(), NodeError>> + Send;
}

/// Exponential backoff used after a Hub disconnect. Commands are never replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first disconnect.
    pub initial: Duration,
    /// Upper bound.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    /// Delay before `attempt` (0-based) reconnect.
    #[must_use]
    pub fn delay(self, attempt: u32) -> Duration {
        let mut current = self.initial;
        for _ in 0..attempt {
            current = current.saturating_mul(2).min(self.max);
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let backoff = Backoff {
            initial: Duration::from_millis(10),
            max: Duration::from_millis(80),
        };
        assert_eq!(backoff.delay(0), Duration::from_millis(10));
        assert_eq!(backoff.delay(1), Duration::from_millis(20));
        assert_eq!(backoff.delay(2), Duration::from_millis(40));
        assert_eq!(backoff.delay(3), Duration::from_millis(80));
        assert_eq!(backoff.delay(4), Duration::from_millis(80));
    }
}
