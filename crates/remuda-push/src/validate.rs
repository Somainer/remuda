//! Subscription shape checks (browser keys + endpoint). Inspired by herdrx
//! `internal/push/outbound.go`, rewritten in Rust.

use crate::Error;
use crate::vapid::b64_decode;
use p256::elliptic_curve::sec1::FromEncodedPoint;
use p256::{EncodedPoint, PublicKey};
use reqwest::Url;
use std::net::IpAddr;

/// Whether loopback HTTP endpoints are accepted (tests / mock servers).
#[derive(Debug, Clone, Copy, Default)]
pub struct ValidateOpts {
    /// Allow `http://127.0.0.1` and `http://localhost` (never in production).
    pub allow_loopback: bool,
}

/// Reject malformed keys or unsafe endpoints before persistence.
pub fn validate_subscription(
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    opts: ValidateOpts,
) -> Result<(), Error> {
    validate_endpoint(endpoint, opts)?;
    if p256dh.len() > 88 || auth.len() > 24 {
        return Err(Error::InvalidSubscription);
    }
    let key = b64_decode(p256dh)?;
    if key.len() != 65 {
        return Err(Error::InvalidSubscription);
    }
    let encoded = EncodedPoint::from_bytes(&key).map_err(|_| Error::InvalidSubscription)?;
    if PublicKey::from_encoded_point(&encoded)
        .into_option()
        .is_none()
    {
        return Err(Error::InvalidSubscription);
    }
    let secret = b64_decode(auth)?;
    if secret.len() != 16 {
        return Err(Error::InvalidSubscription);
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str, opts: ValidateOpts) -> Result<(), Error> {
    if endpoint.len() > 4096 {
        return Err(Error::InvalidSubscription);
    }
    let url = Url::parse(endpoint).map_err(|_| Error::InvalidSubscription)?;
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err(Error::InvalidSubscription);
    }
    let host = url.host_str().ok_or(Error::InvalidSubscription)?;
    if host.is_empty() || host.contains(['%', '\\', ' ', '\t', '\r', '\n']) {
        return Err(Error::InvalidSubscription);
    }
    match url.scheme() {
        "https" => {
            if let Some(port) = url.port()
                && port != 443
            {
                return Err(Error::InvalidSubscription);
            }
            if let Ok(ip) = host.parse::<IpAddr>()
                && !public_ip(ip)
                && !(opts.allow_loopback && ip.is_loopback())
            {
                return Err(Error::InvalidSubscription);
            }
            Ok(())
        }
        "http" if opts.allow_loopback && is_loopback_host(host) => Ok(()),
        _ => Err(Error::InvalidSubscription),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 0)
                || (v4.octets()[0] == 192 && v4.octets()[1] == 0 && v4.octets()[2] == 2)
                || (v4.octets()[0] == 198 && (v4.octets()[1] == 18 || v4.octets()[1] == 19))
                || (v4.octets()[0] == 198 && v4.octets()[1] == 51 && v4.octets()[2] == 100)
                || (v4.octets()[0] == 203 && v4.octets()[1] == 0 && v4.octets()[2] == 113)
                || v4.octets()[0] >= 240)
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
                || v6.is_multicast())
        }
    }
}
