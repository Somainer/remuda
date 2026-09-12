//! Bounded authentication attempt budgets, keyed by the actual TCP peer.

use crate::AppState;
use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const IP_BURST: f64 = 10.0;
const IP_REFILL_PER_SEC: f64 = 0.1;
const GLOBAL_BURST: f64 = 64.0;
const GLOBAL_REFILL_PER_SEC: f64 = 8.0;
const MAX_BUCKETS: usize = 4096;
const IDLE_TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Hash, Eq, PartialEq)]
enum Endpoint {
    Login,
    Pair,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn full(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            updated: now,
        }
    }

    fn take(&mut self, capacity: f64, refill: f64, now: Instant) -> bool {
        self.tokens = (self.tokens
            + now.saturating_duration_since(self.updated).as_secs_f64() * refill)
            .min(capacity);
        self.updated = now;
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

struct Budgets {
    peers: HashMap<(IpAddr, Endpoint), Bucket>,
    global: Bucket,
}

#[derive(Clone)]
pub(crate) struct AuthRateLimits(Arc<Mutex<Budgets>>);

impl Default for AuthRateLimits {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Budgets {
            peers: HashMap::new(),
            global: Bucket::full(GLOBAL_BURST, Instant::now()),
        })))
    }
}

impl AuthRateLimits {
    fn admit(&self, peer: IpAddr, endpoint: Endpoint, now: Instant) -> bool {
        let Ok(mut budgets) = self.0.lock() else {
            return false;
        };
        if !budgets
            .global
            .take(GLOBAL_BURST, GLOBAL_REFILL_PER_SEC, now)
        {
            return false;
        }
        let peer = match peer {
            IpAddr::V6(ip) => ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(peer),
            peer => peer,
        };
        if budgets.peers.len() >= MAX_BUCKETS {
            budgets
                .peers
                .retain(|_, bucket| now.saturating_duration_since(bucket.updated) < IDLE_TTL);
            if !budgets.peers.contains_key(&(peer, endpoint)) && budgets.peers.len() >= MAX_BUCKETS
            {
                return false;
            }
        }
        budgets
            .peers
            .entry((peer, endpoint))
            .or_insert_with(|| Bucket::full(IP_BURST, now))
            .take(IP_BURST, IP_REFILL_PER_SEC, now)
    }
}

pub(crate) async fn limit_auth_attempts(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let endpoint = match (request.method(), request.uri().path()) {
        (&Method::POST, "/v1/login") => Endpoint::Login,
        (&Method::POST, "/v1/devices/pair") => Endpoint::Pair,
        _ => return next.run(request).await,
    };
    // Do not trust Forwarded / X-Forwarded-For from callers. A reverse proxy
    // shares its own peer budget unless a trusted-proxy policy is added.
    let admitted = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| {
            state.auth_limits.admit(peer.ip(), endpoint, Instant::now())
        });
    if !admitted {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "10")],
            Json(json!({"code": "RATE_LIMITED", "error": "too many authentication attempts"})),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_and_endpoints_have_independent_budgets_that_refill() {
        let limits = AuthRateLimits::default();
        let now = Instant::now();
        let peer = "127.0.0.1".parse().unwrap();
        for _ in 0..10 {
            assert!(limits.admit(peer, Endpoint::Login, now));
        }
        assert!(!limits.admit(peer, Endpoint::Login, now));
        assert!(!limits.admit("::ffff:127.0.0.1".parse().unwrap(), Endpoint::Login, now));
        assert!(limits.admit(peer, Endpoint::Pair, now));
        assert!(limits.admit("127.0.0.2".parse().unwrap(), Endpoint::Login, now));
        assert!(!limits.admit(peer, Endpoint::Login, now + Duration::from_secs(9)));
        assert!(limits.admit(peer, Endpoint::Login, now + Duration::from_secs(10)));
        assert!(!limits.admit(peer, Endpoint::Login, now + Duration::from_secs(10)));
    }

    #[test]
    fn global_budget_bounds_distributed_attempts() {
        let limits = AuthRateLimits::default();
        let now = Instant::now();
        for i in 1..=64 {
            assert!(limits.admit(IpAddr::from([192, 0, 2, i]), Endpoint::Login, now));
        }
        assert!(!limits.admit("198.51.100.1".parse().unwrap(), Endpoint::Pair, now));
        assert!(limits.admit(
            "198.51.100.1".parse().unwrap(),
            Endpoint::Pair,
            now + Duration::from_secs(1)
        ));
    }

    #[test]
    fn bucket_storage_is_bounded_and_idle_entries_are_reclaimed() {
        let limits = AuthRateLimits::default();
        let now = Instant::now();
        let mut budgets = limits.0.lock().unwrap();
        for i in 0..MAX_BUCKETS as u32 {
            budgets.peers.insert(
                (IpAddr::V4(i.into()), Endpoint::Login),
                Bucket::full(IP_BURST, now),
            );
        }
        drop(budgets);
        let peer = "203.0.113.1".parse().unwrap();
        assert!(!limits.admit(peer, Endpoint::Login, now));
        assert!(limits.admit(peer, Endpoint::Login, now + IDLE_TTL));
        assert_eq!(limits.0.lock().unwrap().peers.len(), 1);
    }
}
