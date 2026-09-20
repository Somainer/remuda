//! Pure policy for the D-047/D-048 model-API relay: bearer minting, header and
//! path allowlists, bind-address policy, and the listener's error responses.
//!
//! Nothing here performs I/O. Keeping the rules pure is what lets the tests
//! prove the security shape directly ("a wrong bearer is refused", "no public
//! bind exists by default") instead of inferring it from a running server.

use crate::NodeError;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use remuda_protocol::hubnode::{
    API_ERROR_CANCELLED, API_ERROR_HUB_LINK_LOST, API_ERROR_INSTANCE_GONE,
    API_ERROR_UPSTREAM_TIMEOUT, API_ERROR_VIA_HOST_OFFLINE,
};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use subtle::ConstantTimeEq;

/// Per-instance relay bearer size in raw bytes (§3, D4): 32 random bytes
/// minted at launch, carried only in the instance's 0600 overlay and in Node
/// memory, revoked when the instance exits.
pub(crate) const RELAY_BEARER_BYTES: usize = 32;

/// A minted relay bearer in its raw, comparable form.
///
/// The overlay value — and the only form that ever crosses a socket — is
/// [`Self::encoded`]. [`std::fmt::Debug`] prints a redaction so a log filter
/// mistake cannot copy a credential.
pub(crate) struct RelayBearer {
    secret: [u8; RELAY_BEARER_BYTES],
}

impl RelayBearer {
    /// Mint 32 random bytes. Two v4 UUIDs are used rather than another
    /// dependency: each carries 128 bits of `getrandom` output.
    pub(crate) fn mint() -> Self {
        let mut secret = [0u8; RELAY_BEARER_BYTES];
        secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self { secret }
    }

    /// Build a bearer holder from raw bytes (tests).
    #[cfg(test)]
    pub(crate) fn from_bytes(secret: [u8; RELAY_BEARER_BYTES]) -> Self {
        Self { secret }
    }

    /// Build a bearer holder from its standard-base64 overlay value (tests).
    #[cfg(test)]
    pub(crate) fn from_encoded(encoded: String) -> Self {
        let decoded = BASE64
            .decode(encoded.as_bytes())
            .expect("test bearer is standard base64");
        let mut secret = [0u8; RELAY_BEARER_BYTES];
        secret.copy_from_slice(&decoded);
        Self { secret }
    }

    /// Standard-base64 overlay value, e.g. the `ANTHROPIC_AUTH_TOKEN` written
    /// into the per-instance settings file.
    #[must_use]
    pub(crate) fn encoded(&self) -> String {
        BASE64.encode(self.secret)
    }

    /// Constant-time comparison against a presented bearer token.
    ///
    /// `subtle` rather than `==`: a token comparison on the hot path must not
    /// leak its prefix through timing. The length is compared in constant time
    /// too, so a short guess does not short-circuit before the compare.
    pub(crate) fn verify(&self, presented: &str) -> bool {
        let Ok(decoded) = BASE64.decode(presented.trim().as_bytes()) else {
            return false;
        };
        if decoded.len() != RELAY_BEARER_BYTES {
            return false;
        }
        self.secret.ct_eq(decoded.as_slice()).into()
    }
}

impl std::fmt::Debug for RelayBearer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RelayBearer(<redacted>)")
    }
}

/// Extract the token from an `Authorization: Bearer …` header. The scheme
/// comparison is ASCII-case-insensitive, like HTTP itself.
#[must_use]
pub(crate) fn bearer_token(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// Request headers that may cross from the harness into the relay. Everything
/// else the client sends — notably `authorization`, `cookie`, `x-api-key` —
/// is dropped before the request reaches the proxy host, and the proxy
/// rebuilds the gateway credential itself (§3).
fn request_header_allowed(name: &str) -> bool {
    matches!(
        name,
        "content-type" | "accept" | "anthropic-version" | "anthropic-beta" | "accept-encoding"
    ) || name.starts_with("x-stainless-")
}

/// Response headers that may cross back from the gateway. `set-cookie` is
/// deliberately absent: a gateway cookie must not become authority on the
/// worker host, and the harness never needs one.
fn response_header_allowed(name: &str) -> bool {
    matches!(name, "content-type" | "retry-after" | "request-id") || name.starts_with("anthropic-")
}

/// Filter `(name, value)` header pairs through an allowlist, comparing names
/// ASCII-case-insensitively and preserving repeats (the wire carries headers
/// as a list, not a map, for exactly this reason).
pub(crate) fn filter_request_headers<I, S>(headers: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (S, S)>,
    S: AsRef<str>,
{
    headers
        .into_iter()
        .filter(|(name, _)| request_header_allowed(name.as_ref().to_ascii_lowercase().as_str()))
        .map(|(name, value)| (name.as_ref().to_owned(), value.as_ref().to_owned()))
        .collect()
}

/// Response-side counterpart of [`filter_request_headers`].
pub(crate) fn filter_response_headers<I, S>(headers: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (S, S)>,
    S: AsRef<str>,
{
    headers
        .into_iter()
        .filter(|(name, _)| response_header_allowed(name.as_ref().to_ascii_lowercase().as_str()))
        .map(|(name, value)| (name.as_ref().to_owned(), value.as_ref().to_owned()))
        .collect()
}

/// Normalize a URL path component for suffix comparison: trim a trailing
/// slash, keep everything else verbatim. The empty string means "origin root".
#[must_use]
pub(crate) fn normalize_base_path(path: &str) -> &str {
    path.trim_end_matches('/')
}

/// True when `requested` is the base path itself or a suffix beneath it.
///
/// Comparison is segment-wise: a base `/v1` must accept `/v1/messages` but not
/// `/v1alpha/x`. Both inputs are raw, percent-encoded paths — the relay never
/// decodes or re-encodes them, so the check runs on the wire spelling.
#[must_use]
pub(crate) fn path_is_within_base(requested: &str, base_path: &str) -> bool {
    let base = normalize_base_path(base_path);
    let requested = requested.split('?').next().unwrap_or(requested);
    if base.is_empty() {
        return requested.starts_with('/');
    }
    requested == base || requested.starts_with(&format!("{base}/"))
}

/// Strip the base prefix from an accepted listener path, yielding the `path`
/// carried on `api.open` ("path under the profile's base path").
#[must_use]
pub(crate) fn strip_base_prefix<'a>(requested: &'a str, base_path: &str) -> &'a str {
    let base = normalize_base_path(base_path);
    if base.is_empty() {
        return requested;
    }
    requested
        .strip_prefix(base)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .unwrap_or(requested)
}

/// Reject the paths that must never be rebuilt against the gateway even when
/// they sit under the base prefix: segment traversal and backslashes. The
/// proxy constructs the URL from the pinned origin plus this suffix, so
/// accepting `..` would be the one way to walk off it.
#[must_use]
pub(crate) fn relayed_path_is_safe(path: &str) -> bool {
    if path.contains('\\') || !path.starts_with('/') {
        return false;
    }
    path.split('/')
        .all(|segment| segment != "." && segment != "..")
}

/// One allowed peer range: an exact address or a CIDR block. `Host.relayBind`
/// `allowFrom` strings parse into these. Constructed for H's direct-net
/// listener (task-2 Hub wiring; tests today).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerRule {
    /// Single exact IP.
    Exact(IpAddr),
    /// IPv4 network.
    V4 { addr: Ipv4Addr, prefix: u8 },
    /// IPv6 network.
    V6 { addr: Ipv6Addr, prefix: u8 },
}

impl PeerRule {
    /// Parse `10.0.0.1` or `10.0.0.0/8` / `fd00::/16`.
    pub(crate) fn parse(raw: &str) -> Result<Self, NodeError> {
        let raw = raw.trim();
        if let Some((addr, prefix)) = raw.split_once('/') {
            let prefix: u8 = prefix.parse().map_err(|_| {
                NodeError::InvalidConfig(format!("relayBind allowFrom bad CIDR {raw:?}"))
            })?;
            match addr.parse::<IpAddr>() {
                Ok(IpAddr::V4(addr)) => {
                    if prefix > 32 {
                        return Err(NodeError::InvalidConfig(format!(
                            "relayBind allowFrom IPv4 prefix out of range in {raw:?}"
                        )));
                    }
                    Ok(Self::V4 { addr, prefix })
                }
                Ok(IpAddr::V6(addr)) => {
                    if prefix > 128 {
                        return Err(NodeError::InvalidConfig(format!(
                            "relayBind allowFrom IPv6 prefix out of range in {raw:?}"
                        )));
                    }
                    Ok(Self::V6 { addr, prefix })
                }
                Err(_) => Err(NodeError::InvalidConfig(format!(
                    "relayBind allowFrom bad address in {raw:?}"
                ))),
            }
        } else {
            raw.parse::<IpAddr>().map(Self::Exact).map_err(|_| {
                NodeError::InvalidConfig(format!("relayBind allowFrom bad address {raw:?}"))
            })
        }
    }

    fn contains(self, ip: IpAddr) -> bool {
        match (self, ip) {
            (Self::Exact(allowed), ip) => allowed == ip,
            (Self::V4 { addr, prefix }, IpAddr::V4(ip)) => {
                prefix_match(addr.octets(), ip.octets(), prefix)
            }
            (Self::V6 { addr, prefix }, IpAddr::V6(ip)) => {
                prefix_match(addr.octets(), ip.octets(), prefix)
            }
            _ => false,
        }
    }
}

/// First `prefix` bits equal (any byte array width).
fn prefix_match<const N: usize>(allowed: [u8; N], got: [u8; N], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    allowed[..full] == got[..full]
        && if prefix.is_multiple_of(8) {
            true
        } else {
            let mask = 0xff_u8 << (8 - prefix % 8);
            allowed[full] & mask == got[full] & mask
        }
}

/// Validate the address an optional direct-net relay listener may bind
/// (D-047 Amendment A1, D-031).
///
/// * the worker-side listener never calls this: it is hard-bound to
///   `127.0.0.1:0`;
/// * `0.0.0.0` / `::` are refused — there is no "listen on everything" spelling;
/// * a globally reachable address is refused; private and loopback addresses
///   are allowed only because the operator named this address explicitly.
///
/// A port is required because `Host.relayBind.addr` is a concrete `host:port`.
pub(crate) fn validate_proxy_bind(addr: &str) -> Result<SocketAddr, NodeError> {
    let parsed: SocketAddr = addr.parse().map_err(|_| {
        NodeError::InvalidConfig(format!("relayBind addr must be host:port, got {addr:?}"))
    })?;
    let ip = parsed.ip();
    if ip.is_unspecified() {
        return Err(NodeError::InvalidConfig(
            "relayBind must not bind every interface (0.0.0.0/::)".into(),
        ));
    }
    if is_private_named_ip(ip) {
        return Ok(parsed);
    }
    Err(NodeError::InvalidConfig(
        "relayBind must name a loopback or private address, never a public one".into(),
    ))
}

/// The explicit set of address families an operator-named `relayBind` may
/// use. `IpAddr::is_global` is still unstable in the standard library, and an
/// allowlist is the safer reading anyway: a newly allocated special-purpose
/// range must be reviewed before it becomes a place a relay can bind.
fn is_private_named_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) if ip.is_loopback() => true,
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            ip.is_private() // RFC 1918
                || ip.is_link_local() // 169.254/16
                || octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127 // CGNAT 100.64/10
        }
        IpAddr::V6(ip) if ip.is_loopback() => true,
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            (segments[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// Whether a connected peer may talk to a listener. The worker-side listener
/// requires loopback; a proxy listener accepts loopback or an address matched
/// by its configured `allowFrom` rules (an empty list defers to the operator's
/// firewall, as the wire contract states).
#[must_use]
pub(crate) fn peer_allowed(peer: IpAddr, rules: &[PeerRule]) -> bool {
    if peer.is_loopback() {
        return true;
    }
    rules.iter().any(|rule| rule.contains(peer))
}

/// HTTP status the worker listener answers for a terminal `api.end` error
/// before the response head committed. Timeouts are a gateway-class 504; a
/// gone route or lost link is 503 so the harness sees a retriable upstream
/// failure rather than a local transport fault (§7.6).
#[must_use]
pub(crate) fn status_for_error(code: &str) -> u16 {
    match code {
        API_ERROR_UPSTREAM_TIMEOUT => 504,
        remuda_protocol::hubnode::API_ERROR_UPSTREAM_FAILED => 502,
        API_ERROR_VIA_HOST_OFFLINE | API_ERROR_HUB_LINK_LOST | API_ERROR_INSTANCE_GONE => 503,
        API_ERROR_CANCELLED => 502,
        _ => 502,
    }
}

/// Anthropic-shaped error body for a listener-synthesized failure. The CLI
/// renders this as a model-API error page; the message carries the stable code
/// but never a header, body, or credential.
///
/// Built with `serde_json::json!` rather than hand-quoted formatting so a
/// backslash or control character in either field is escaped correctly and
/// can never produce invalid JSON for the harness to parse.
#[must_use]
pub(crate) fn anthropic_error_body(code: &str, message: &str) -> String {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!("{code}: {message}"),
        },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_roundtrip_and_wrong_value_fail() {
        let bearer = RelayBearer::from_bytes([7u8; RELAY_BEARER_BYTES]);
        let encoded = bearer.encoded();
        assert!(bearer.verify(&encoded));
        assert!(bearer.verify(&format!("  {encoded} ")));
        let mut wrong = RelayBearer::from_bytes([7u8; RELAY_BEARER_BYTES]);
        wrong.secret[31] = 8;
        assert!(!wrong.verify(&encoded));
        assert!(!bearer.verify("not-base64!!"));
        assert!(!bearer.verify(&BASE64.encode([0u8; 8])));
    }

    #[test]
    fn bearer_header_parsing() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer xyz")), Some("xyz"));
        assert_eq!(bearer_token(Some("Token abc")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn request_header_allowlist_drops_credentials_and_cookies() {
        let kept = filter_request_headers([
            ("Content-Type", "application/json"),
            ("X-Stainless-Lang", "js"),
            ("Authorization", "Bearer real-token"),
            ("X-Api-Key", "real-key"),
            ("Cookie", "session=1"),
            ("anthropic-beta", "f"),
        ]);
        let names: Vec<_> = kept.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(
            names,
            vec!["Content-Type", "X-Stainless-Lang", "anthropic-beta"]
        );
    }

    #[test]
    fn response_header_allowlist_drops_set_cookie() {
        let kept = filter_response_headers([
            ("content-type", "text/event-stream"),
            ("anthropic-organization-id", "o"),
            ("set-cookie", "sid=leak"),
            ("x-custom", "v"),
        ]);
        let names: Vec<_> = kept.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["content-type", "anthropic-organization-id"]);
    }

    #[test]
    fn base_path_suffix_is_segment_wise() {
        assert!(path_is_within_base("/v1/messages", "/v1"));
        assert!(path_is_within_base("/v1", "/v1"));
        assert!(!path_is_within_base("/v1alpha/x", "/v1"));
        assert!(path_is_within_base("/v1/messages?stream=true", "/v1"));
        assert!(path_is_within_base("/anything", ""));
        assert_eq!(strip_base_prefix("/v1/messages", "/v1"), "/messages");
        assert_eq!(strip_base_prefix("/v1", "/v1"), "");
        assert_eq!(strip_base_prefix("/v1/messages", ""), "/v1/messages");
    }

    #[test]
    fn traversal_and_backslash_are_refused() {
        assert!(!relayed_path_is_safe("/v1/../etc"));
        assert!(!relayed_path_is_safe("/v1/a/./b"));
        assert!(!relayed_path_is_safe("v1/messages"));
        assert!(!relayed_path_is_safe("/v1\\messages"));
        assert!(relayed_path_is_safe("/v1/messages"));
    }

    #[test]
    fn bind_policy_refuses_any_and_public_and_accepts_private_explicit() {
        assert!(validate_proxy_bind("0.0.0.0:8443").is_err());
        assert!(validate_proxy_bind("[::]:8443").is_err());
        assert!(validate_proxy_bind("8.8.8.8:8443").is_err());
        assert!(validate_proxy_bind("127.0.0.1:8443").is_ok());
        assert!(validate_proxy_bind("10.20.30.40:8443").is_ok());
        assert!(validate_proxy_bind("[fd00::1]:8443").is_ok());
        assert!(validate_proxy_bind("no-port").is_err());
    }

    #[test]
    fn peer_rules_match_cidr_and_exact() {
        assert!(peer_allowed(IpAddr::V4(Ipv4Addr::LOCALHOST), &[]));
        let rules = vec![
            PeerRule::parse("10.0.0.5").unwrap(),
            PeerRule::parse("192.168.1.0/24").unwrap(),
        ];
        assert!(peer_allowed(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)), &rules));
        assert!(peer_allowed(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 77)),
            &rules
        ));
        assert!(!peer_allowed(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6)),
            &rules
        ));
        assert!(!peer_allowed(
            IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1)),
            &rules
        ));
        assert!(prefix_match([0xfd; 16], [0xfd; 16], 8));
        assert!(!prefix_match([0xfd; 16], [0xfc; 16], 8));
    }

    #[test]
    fn error_status_mapping() {
        assert_eq!(status_for_error(API_ERROR_UPSTREAM_TIMEOUT), 504);
        assert_eq!(status_for_error(API_ERROR_VIA_HOST_OFFLINE), 503);
        assert_eq!(status_for_error(API_ERROR_HUB_LINK_LOST), 503);
        assert_eq!(status_for_error("anything-else"), 502);
    }

    #[test]
    fn anthropic_error_body_is_valid_json_under_hostile_inputs() {
        // Quotes, backslashes and a control character must all round-trip
        // through a JSON parser instead of breaking the document.
        let body = anthropic_error_body("bad\"code", "line1\\n\"line2\u{0007}");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        let message = value["error"]["message"].as_str().expect("message");
        assert!(message.starts_with("bad\"code: line1\\n\"line2"));
        // And never hand-rolled HTML-escaped or quote-stripped.
        assert!(body.contains("bad\\\"code"));
    }
}
