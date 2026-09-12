//! Synthetic loopback HTTP requests exercise the trusted reverse-proxy boundary.

use anyhow::Result;
use remuda_hub::{HubConfig, spawn};
use reqwest::{Client, StatusCode};
use serde_json::json;
use std::net::{IpAddr, Ipv4Addr};

#[tokio::test]
async fn only_trusted_https_or_configured_https_upgrades_cookies() -> Result<()> {
    for (trusted, origin, forwarded, expected_secure) in [
        (false, None, "https", false),
        (true, None, "https", true),
        (true, None, "https,http", false),
        (false, Some("https://hub.invalid"), "http", true),
    ] {
        let dir = tempfile::tempdir()?;
        let mut config = HubConfig::for_test(dir.path().into());
        config.public_origin = origin.map(str::to_owned);
        if trusted {
            config.trusted_proxies.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
        }
        let hub = spawn(config).await?;
        let client = Client::builder().no_proxy().build()?;
        let response = client
            .post(format!("http://{}/v1/login", hub.addr))
            .header("x-forwarded-proto", forwarded)
            .json(&json!({ "bootstrapToken": hub.bootstrap_token, "deviceName": "test" }))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()["set-cookie"].to_str()?;
        assert_eq!(
            cookie.split(';').any(|flag| flag.trim() == "Secure"),
            expected_secure
        );
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        hub.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn trusted_proxy_login_budgets_are_per_forwarded_client() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let mut config = HubConfig::for_test(dir.path().into());
    config.trusted_proxies.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let hub = spawn(config).await?;
    let client = Client::builder().no_proxy().build()?;
    for forwarded in ["192.0.2.1", "192.0.2.2"] {
        for _ in 0..10 {
            let response = client
                .post(format!("http://{}/v1/login", hub.addr))
                .header("x-forwarded-for", forwarded)
                .json(&json!({ "bootstrapToken": "invalid", "deviceName": "test" }))
                .send()
                .await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = client
            .post(format!("http://{}/v1/login", hub.addr))
            .header("x-forwarded-for", forwarded)
            .json(&json!({ "bootstrapToken": "invalid", "deviceName": "test" }))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }
    hub.shutdown().await;
    Ok(())
}
