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
