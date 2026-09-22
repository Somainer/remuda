//! Hub JSON-RPC control-plane transports.
//!
//! [`crate::HubCarrier`] / [`crate::StdioCarrier`] speak SSH-friendly NDJSON
//! application frames. [`NodeTransport`] is the Hub `/v1/node` JSON-RPC 2.0
//! carrier used by outbound WSS. A stdio JSON-RPC adapter is not defined here.
//!
//! RESILIENCE (spec only — `docs/design/hub-resilience.md`, 2026-09-22
//! c-hubresil): this module's [`Backoff`] is the entire client-side reconnect
//! policy today, and the Hub hello handler has no `retry_after` reply. §2.1 of
//! that doc records the deployed behavior (infinite redial, commands never
//! replayed, pending appends failed and replayed from the durable journal);
//! §4 specifies the storm-tuned replacement (2–8 s first window, 60 s cap,
//! per-host random seed, honored server `hello.retry_after`). Unimplemented.

use crate::NodeError;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;

pub(crate) mod hubnode;
mod hubnode_codec;
mod wss;

pub use wss::{HubRequest, JournalSender, WssCarrier, WssConfig, WssLink};

mod metrics;
pub use metrics::{TransportMetrics, TransportMetricsSnapshot};

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
///
/// SPEC-ONLY (unimplemented, 2026-09-22 c-hubresil): the defaults below are
/// what ships — 1 s start, 30 s cap, ±25 % jitter — and both real call sites
/// seed the jitter deterministically (the frame-id counter in the WSS session,
/// the process id in the daemon supervisor), so simultaneously woken Nodes can
/// align. `docs/design/hub-resilience.md` §4 specifies a 2–8 s randomized first
/// window, a 60 s cap, a per-host random seed, and honoring a Hub
/// `hello.retry_after`; do not widen anything here until that spec lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first disconnect.
    pub initial: Duration,
    /// Upper bound.
    pub max: Duration,
    /// Symmetric jitter as parts-per-thousand of the base delay (`250` = ±25%).
    pub jitter_ppt: u16,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            jitter_ppt: 250,
        }
    }
}

impl Backoff {
    /// Delay before `attempt` (0-based) reconnect, without jitter.
    #[must_use]
    pub fn delay(self, attempt: u32) -> Duration {
        let mut current = self.initial;
        for _ in 0..attempt {
            current = current.saturating_mul(2).min(self.max);
        }
        current
    }

    /// [`Self::delay`] with deterministic jitter from `seed` (reconnects must not align).
    #[must_use]
    pub fn jittered_delay(self, attempt: u32, seed: u64) -> Duration {
        let base = self.delay(attempt);
        if self.jitter_ppt == 0 {
            return base;
        }
        let span = base.as_nanos().saturating_mul(u128::from(self.jitter_ppt)) / 1000;
        let mix = seed
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(u64::from(attempt));
        let unit = u128::from(mix % 10_001);
        let signed = unit as i128 - 5_000;
        let delta = if span == 0 {
            0
        } else {
            (span as i128).saturating_mul(signed) / 5_000
        };
        let nanos = (base.as_nanos() as i128 + delta).max(0) as u128;
        let capped = nanos.min(self.max.as_nanos());
        Duration::from_nanos(u64::try_from(capped).unwrap_or(u64::MAX))
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
            jitter_ppt: 0,
        };
        assert_eq!(backoff.delay(0), Duration::from_millis(10));
        assert_eq!(backoff.delay(1), Duration::from_millis(20));
        assert_eq!(backoff.delay(2), Duration::from_millis(40));
        assert_eq!(backoff.delay(3), Duration::from_millis(80));
        assert_eq!(backoff.delay(4), Duration::from_millis(80));
    }

    #[test]
    fn jittered_delay_stays_within_band_and_is_deterministic() {
        let backoff = Backoff {
            initial: Duration::from_millis(100),
            max: Duration::from_millis(100),
            jitter_ppt: 250,
        };
        let first = backoff.jittered_delay(0, 7);
        let again = backoff.jittered_delay(0, 7);
        assert_eq!(first, again);
        for seed in 0..64 {
            let delay = backoff.jittered_delay(0, seed);
            assert!(delay >= Duration::from_millis(75), "{delay:?}");
            assert!(delay <= Duration::from_millis(125), "{delay:?}");
        }
        let exact = Backoff {
            jitter_ppt: 0,
            ..backoff
        };
        assert_eq!(exact.jittered_delay(0, 99), Duration::from_millis(100));
    }
}
