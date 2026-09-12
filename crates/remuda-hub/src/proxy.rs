//! Forwarding headers are authoritative only from explicitly trusted TCP peers.

use crate::{AppState, HubConfig};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use std::net::{IpAddr, SocketAddr};

pub(crate) fn configure_public_origin(config: &mut HubConfig) -> anyhow::Result<()> {
    if let Some(raw) = &config.public_origin {
        let url = reqwest::Url::parse(raw)
            .map_err(|_| anyhow::anyhow!("public origin must be an HTTP(S) origin"))?;
        anyhow::ensure!(
            (raw.starts_with("http://") || raw.starts_with("https://"))
                && matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none()
                && !raw.contains('\\')
                && !raw.chars().any(|c| c.is_control() || c.is_whitespace()),
            "public origin must contain only an HTTP(S) scheme and authority"
        );
        config.cookie_secure |= url.scheme() == "https";
        config.public_origin = Some(url.origin().ascii_serialization());
    }
    Ok(())
}

fn normalized_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(ip) => ip
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(ip)),
        ip => ip,
    }
}

fn trusted(peer: IpAddr, config: &HubConfig) -> bool {
    config
        .trusted_proxies
        .iter()
        .any(|ip| normalized_ip(*ip) == normalized_ip(peer))
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value.trim())
}

pub(crate) fn client_ip(peer: IpAddr, headers: &HeaderMap, config: &HubConfig) -> IpAddr {
    if trusted(peer, config)
        && let Some(ip) =
            single_header(headers, "x-forwarded-for").and_then(|value| value.parse::<IpAddr>().ok())
    {
        return normalized_ip(ip);
    }
    normalized_ip(peer)
}

fn forwarded_https(peer: Option<IpAddr>, headers: &HeaderMap, config: &HubConfig) -> bool {
    peer.is_some_and(|peer| trusted(peer, config))
        && single_header(headers, "x-forwarded-proto") == Some("https")
}

pub(crate) async fn secure_cookies(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip());
    let secure =
        state.config.cookie_secure || forwarded_https(peer, request.headers(), &state.config);
    let mut response = next.run(request).await;
    if secure {
        // Cover login, pairing, and revocation without dropping other Set-Cookie values.
        let cookies: Vec<_> = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| {
                let Some(cookie) = value.to_str().ok().filter(|cookie| {
                    cookie.starts_with("remuda_device=")
                        && !cookie
                            .split(';')
                            .skip(1)
                            .any(|flag| flag.trim().eq_ignore_ascii_case("secure"))
                }) else {
                    return value.clone();
                };
                format!("{cookie}; Secure")
                    .parse()
                    .unwrap_or_else(|_| value.clone())
            })
            .collect();
        response.headers_mut().remove(header::SET_COOKIE);
        for cookie in cookies {
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::origin_allowed;
    use axum::http::HeaderValue;

    fn config() -> HubConfig {
        let mut config = HubConfig::for_test("./data".into());
        config.trusted_proxies.push(IpAddr::from([192, 0, 2, 1]));
        config
    }

    #[test]
    fn public_origin_pins_origin_and_cannot_disable_secure_cookies() {
        let mut config = config();
        config.public_origin = Some("https://hub.invalid/".into());
        configure_public_origin(&mut config).unwrap();
        assert!(config.cookie_secure);
        assert_eq!(config.public_origin.as_deref(), Some("https://hub.invalid"));
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://hub.invalid"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("internal.invalid"));
        assert!(origin_allowed(&headers, &config));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.invalid"),
        );
        headers.insert(header::HOST, HeaderValue::from_static("other.invalid"));
        assert!(!origin_allowed(&headers, &config));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://hub.invalid"),
        );
        assert!(!origin_allowed(&headers, &config));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://hub.invalid:9443"),
        );
        assert!(!origin_allowed(&headers, &config));
        for raw in [
            "https://user:secret@hub.invalid",
            "https://hub.invalid/path",
            "https://hub.invalid/?query",
            "https://hub.invalid/#fragment",
            "wss://hub.invalid",
            " https://hub.invalid",
            "https://hub.invalid\\",
        ] {
            config.public_origin = Some(raw.into());
            assert!(configure_public_origin(&mut config).is_err());
        }
    }

    #[test]
    fn forwarding_requires_exact_peer_and_unambiguous_headers() {
        let mut config = config();
        config.public_origin = Some("https://hub.invalid".into());
        let proxy = IpAddr::from([192, 0, 2, 1]);
        let untrusted = IpAddr::from([192, 0, 2, 2]);
        let client = IpAddr::from([198, 51, 100, 1]);
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(client_ip(proxy, &headers, &config), client);
        assert!(forwarded_https(Some(proxy), &headers, &config));
        assert_eq!(client_ip(untrusted, &headers, &config), untrusted);
        assert!(!forwarded_https(Some(untrusted), &headers, &config));
        assert!(!forwarded_https(None, &headers, &config));
        for raw in ["198.51.100.1, 198.51.100.2", "unknown", "198.51.100.1:1234"] {
            headers.insert("x-forwarded-for", HeaderValue::from_str(raw).unwrap());
            assert_eq!(client_ip(proxy, &headers, &config), proxy);
        }
        headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
        headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.2"));
        assert_eq!(client_ip(proxy, &headers, &config), proxy);
        for raw in ["https,http", "HTTPS", "invalid"] {
            headers.insert("x-forwarded-proto", HeaderValue::from_str(raw).unwrap());
            assert!(!forwarded_https(Some(proxy), &headers, &config));
        }
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.append("x-forwarded-proto", HeaderValue::from_static("http"));
        assert!(!forwarded_https(Some(proxy), &headers, &config));
    }
}
