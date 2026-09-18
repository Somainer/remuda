//! Static files are reached without authentication; exercise raw request paths.

use anyhow::Result;
use remuda_hub::{HubConfig, spawn};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

async fn get(addr: std::net::SocketAddr, path: &str) -> Result<(u16, String)> {
    // Do not use a URL client: it would normalize away the traversal under test.
    let mut socket = TcpStream::connect(addr).await?;
    socket
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await?;
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        socket.read_to_end(&mut bytes),
    )
    .await??;
    let response = String::from_utf8(bytes)?;
    let status = response.split_whitespace().nth(1).unwrap().parse()?;
    Ok((status, response))
}

#[tokio::test]
async fn raw_static_traversals_return_404_without_leaking_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("web");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("index.html"), "safe index")?;
    std::fs::write(root.join("app.js"), "safe script")?;
    std::fs::write(dir.path().join("secret"), "outside secret marker")?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.web_root = Some(root);
    let hub = spawn(config).await?;

    for path in [
        "/../../etc/passwd",
        "/../secret",
        "/assets/../../secret",
        "//etc/passwd",
        "/..\\secret",
    ] {
        let (status, response) = get(hub.addr, path).await?;
        assert_eq!(status, 404, "{path}: {response}");
        assert!(!response.contains("outside secret marker"));
        assert!(!response.contains("root:"));
        assert!(!response.contains("safe index"));
    }
    for (path, content) in [
        ("/", "safe index"),
        ("/sessions/example", "safe index"),
        ("/app.js", "safe script"),
    ] {
        let (status, response) = get(hub.addr, path).await?;
        assert_eq!(status, 200);
        assert!(response.ends_with(content));
    }
    hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cache_headers_match_asset_class_and_missing_assets_404() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("web");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(root.join("assets"))?;
    std::fs::write(root.join("index.html"), "shell")?;
    std::fs::write(root.join("sw.js"), "worker")?;
    std::fs::write(root.join("assets/index-abc123.js"), "hashed module")?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.web_root = Some(root);
    let hub = spawn(config).await?;

    fn headers_of(response: &str) -> String {
        response
            .split_once("\r\n\r\n")
            .unwrap()
            .0
            .to_ascii_lowercase()
    }

    // index.html and sw.js revalidate every load so a redeploy is seen.
    for path in ["/", "/index.html", "/sw.js"] {
        let (status, response) = get(hub.addr, path).await?;
        assert_eq!(status, 200, "{path}: {response}");
        assert!(
            headers_of(&response).contains("cache-control: no-cache"),
            "{path}: {response}"
        );
    }

    // Hashed assets are immutable for a year.
    let (status, response) = get(hub.addr, "/assets/index-abc123.js").await?;
    assert_eq!(status, 200);
    assert!(
        headers_of(&response).contains("cache-control: public, max-age=31536000, immutable"),
        "{response}"
    );

    // A missing hashed asset is a hard 404, never the SPA HTML fallback.
    let (status, response) = get(hub.addr, "/assets/index-deadbe.js").await?;
    assert_eq!(status, 404, "{response}");
    assert!(
        !response.to_ascii_lowercase().contains("text/html"),
        "{response}"
    );
    assert!(!response.contains("shell"), "{response}");

    hub.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn static_symlinks_cannot_escape_even_via_spa_fallback() -> Result<()> {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir()?;
    let root = dir.path().join("web");
    let outside = dir.path().join("web-private");
    std::fs::create_dir(&root)?;
    std::fs::create_dir(&outside)?;
    std::fs::write(outside.join("secret"), "outside secret marker")?;
    std::fs::write(root.join("safe.txt"), "safe content")?;
    symlink(outside.join("secret"), root.join("secret"))?;
    symlink(&outside, root.join("nested"))?;
    symlink(outside.join("secret"), root.join("index.html"))?;
    symlink(root.join("safe.txt"), root.join("inside.txt"))?;
    let mut config = HubConfig::for_test(dir.path().join("data"));
    config.web_root = Some(root);
    let hub = spawn(config).await?;

    for path in ["/secret", "/nested/secret", "/", "/missing"] {
        let (status, response) = get(hub.addr, path).await?;
        assert_eq!(status, 404, "{path}: {response}");
        assert!(!response.contains("outside secret marker"));
    }
    let (status, response) = get(hub.addr, "/inside.txt").await?;
    assert_eq!(status, 200);
    assert!(response.ends_with("safe content"));
    hub.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn security_headers_cover_assets_api_errors_and_upgrade_rejections() -> Result<()> {
    for disk_assets in [false, true] {
        let dir = tempfile::tempdir()?;
        let mut config = HubConfig::for_test(dir.path().join("data"));
        if disk_assets {
            let root = dir.path().join("web");
            std::fs::create_dir(&root)?;
            std::fs::write(root.join("index.html"), "index")?;
            std::fs::write(root.join("font.woff2"), b"font fixture")?;
            config.web_root = Some(root);
        }
        let hub = spawn(config).await?;
        for path in [
            "/",
            "/font.woff2",
            "/healthz",
            "/v1/hosts",
            "/v1/login",
            "/v1/follow",
            "/push/subscriptions",
            "/../secret",
        ] {
            let (_, response) = get(hub.addr, path).await?;
            let headers = response
                .split_once("\r\n\r\n")
                .unwrap()
                .0
                .to_ascii_lowercase();
            assert!(
                headers.contains("content-security-policy: default-src 'self';"),
                "{path}: {headers}"
            );
            assert!(
                headers.contains("frame-ancestors 'none'"),
                "{path}: {headers}"
            );
            for header in [
                "x-content-type-options: nosniff",
                "x-frame-options: deny",
                "referrer-policy: no-referrer",
            ] {
                assert!(headers.contains(header), "{path}: {headers}");
            }
            if disk_assets && path == "/font.woff2" {
                assert!(headers.contains("content-type: font/woff2"));
            }
        }
        hub.shutdown().await;
    }
    Ok(())
}
