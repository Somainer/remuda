//! WebSocket transport for `grok agent serve` (`/ws`).
//!
//! Auth is `Authorization: Bearer <secret>`. The `server-key` query parameter
//! is stripped if present so the secret is not logged in the URL.

use std::io;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tracing::debug;

use crate::error::Error;
use crate::types::ServeSpec;

/// Handshake headers and a cleaned URL (no `server-key` query).
pub fn websocket_request(
    spec: &ServeSpec,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, Error> {
    let url = strip_server_key_query(&spec.url);
    let mut request = url
        .into_client_request()
        .map_err(|err| Error::WebSocket(err.to_string()))?;
    let value = format!("Bearer {}", spec.secret);
    request.headers_mut().insert(
        AUTHORIZATION,
        value.parse().map_err(
            |err: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                Error::WebSocket(format!("authorization header: {err}"))
            },
        )?,
    );
    Ok(request)
}

fn strip_server_key_query(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|part| {
            let name = part.split('=').next().unwrap_or("");
            name != "server-key" && name != "server_key"
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Connect and wrap the socket as an ACP [`agent_client_protocol::Lines`] transport.
pub async fn connect_ws_transport(
    spec: &ServeSpec,
) -> Result<impl agent_client_protocol::ConnectTo<agent_client_protocol::Client>, Error> {
    let request = websocket_request(spec)?;
    debug!(url = %request.uri(), "connecting grok acp websocket");
    let (ws, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|err| Error::WebSocket(err.to_string()))?;
    let (sink, stream) = ws.split();

    let incoming = stream.filter_map(|item| async move {
        match item {
            Ok(Message::Text(text)) => Some(Ok(text.to_string())),
            Ok(Message::Binary(bytes)) => Some(Ok(String::from_utf8_lossy(&bytes).into_owned())),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => None,
            Ok(Message::Close(_)) => Some(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "websocket closed",
            ))),
            Err(err) => Some(Err(io::Error::other(err))),
        }
    });

    let outgoing = futures::sink::unfold(sink, async |mut sink, line: String| {
        sink.send(Message::text(line))
            .await
            .map_err(io::Error::other)?;
        Ok::<_, io::Error>(sink)
    });

    Ok(agent_client_protocol::Lines::new(outgoing, incoming))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_server_key_and_sets_bearer() {
        let spec = ServeSpec::new("ws://127.0.0.1:9/ws?server-key=secret&x=1", "secret");
        let request = websocket_request(&spec).unwrap();
        assert_eq!(request.uri().query(), Some("x=1"));
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer secret");
        assert!(!request.uri().to_string().contains("server-key"));
    }
}
