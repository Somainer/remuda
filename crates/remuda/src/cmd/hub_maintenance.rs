//! Bounded local probe usable in the shell-free musl container.

use anyhow::{Context, ensure};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) async fn healthcheck(mut address: SocketAddr) -> anyhow::Result<()> {
    if address.ip().is_unspecified() {
        address.set_ip(match address.ip() {
            IpAddr::V4(_) => Ipv4Addr::LOCALHOST.into(),
            IpAddr::V6(_) => Ipv6Addr::LOCALHOST.into(),
        });
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = tokio::net::TcpStream::connect(address).await?;
        stream
            .write_all(
                format!("GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut response = Vec::new();
        stream.take(8192).read_to_end(&mut response).await?;
        let response = std::str::from_utf8(&response)?;
        let (headers, body) = response
            .split_once("\r\n\r\n")
            .context("incomplete health response")?;
        ensure!(
            headers
                .lines()
                .next()
                .is_some_and(|line| line == "HTTP/1.1 200 OK" || line == "HTTP/1.0 200 OK"),
            "Hub health returned non-200 status"
        );
        ensure!(
            serde_json::from_str::<serde_json::Value>(body)?["ok"] == true,
            "Hub health response is not ok"
        );
        Ok(())
    })
    .await
    .context("Hub health probe timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_requires_success_status_and_health_payload() {
        for (response, success) in [
            ("HTTP/1.1 200 OK\r\n\r\n{\"ok\":true}", true),
            (
                "HTTP/1.1 503 Service Unavailable\r\n\r\n{\"ok\":true}",
                false,
            ),
            ("HTTP/1.1 200 OK\r\n\r\n{\"ok\":false}", false),
            ("HTTP/1.1 200 OK\r\n\r\n<html>fallback</html>", false),
        ] {
            let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 256];
                let received = socket.read(&mut request).await.unwrap();
                assert!(received > 0);
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            assert_eq!(healthcheck(address).await.is_ok(), success);
            server.await.unwrap();
        }
    }
}
