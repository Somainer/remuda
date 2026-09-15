//! Loopback Hub HTTP used by control-plane unit tests.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// Bound mock Hub that answers the instance/host routes and 404s fleet.
pub(crate) struct MockHub {
    /// Listen address (`127.0.0.1:ephemeral`).
    pub addr: SocketAddr,
    handle: JoinHandle<()>,
    pub requests: Arc<Mutex<Vec<String>>>,
}

impl Drop for MockHub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(crate) async fn spawn_mock_hub() -> MockHub {
    spawn_mock_hub_as(json!({"origin":"human", "children":[]})).await
}

pub(crate) async fn spawn_mock_hub_as(caller: Value) -> MockHub {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock hub");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let handle = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let caller = caller.clone();
            let recorded = recorded.clone();
            tokio::spawn(async move {
                if let Err(err) = handle_conn(stream, &caller, &recorded).await {
                    tracing::debug!(error = %err, "mock hub connection");
                }
            });
        }
    });
    MockHub {
        addr,
        handle,
        requests,
    }
}

async fn handle_conn(
    mut stream: TcpStream,
    caller: &Value,
    requests: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let Some((method, path, body)) = read_http(&mut stream).await? else {
        return Ok(());
    };
    let path_only = path.split('?').next().unwrap_or(&path);
    requests.lock().await.push(format!("{method} {path_only}"));
    let (status, payload) = if method == "GET" && path_only == "/v1/caller" {
        (200, caller.clone())
    } else {
        route(&method, path_only, &body)
    };
    let bytes = payload.to_string();
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        401 => "Unauthorized",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}",
        bytes.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn route(method: &str, path: &str, body: &str) -> (u16, Value) {
    match (method, path) {
        ("GET", "/v1/caller") => (200, json!({"origin":"human", "children":[]})),
        ("GET", "/healthz") => (200, json!({ "ok": true })),
        ("GET", "/v1/instances/ins_test") => {
            (200, route("GET", "/v1/instances", "").1["items"][0].clone())
        }
        ("GET", "/v1/attachments") => (
            200,
            json!({
                "instanceId": "ins_test",
                "items": [attachment_indexed("obj_png", "image/png", 168, 1),
                          attachment_indexed("obj_txt", "text/plain", 11, 2)],
            }),
        ),
        ("GET", path) if path.starts_with("/v1/attachments/") => attachment_content(path),
        ("POST", "/v1/login") => (
            200,
            json!({ "deviceId": "dev_1", "token": "device-token", "name": "remuda-cli" }),
        ),
        ("GET", "/v1/hosts") => (
            200,
            json!({
                "items": [{
                    "hostId": "hst_1",
                    "label": "sg",
                    "online": true,
                    "state": "online",
                    "cli": [],
                    "capabilities": { "labels": { "region": "sg" } },
                    "instanceCount": 0,
                    "maxInstances": 4,
                    "labels": ["region=sg"],
                    "transport": "outbound-wss"
                }],
                "nextCursor": null
            }),
        ),
        ("GET", "/v1/instances") => (
            200,
            json!({
                "items": [{
                    "instanceId": "ins_test",
                    "hostId": "hst_1",
                    "kind": "claude",
                    "driver": "claude-print",
                    "lifecycle": "ready",
                    "activity": "idle",
                    "title": "reviewer",
                    "name": "reviewer",
                    "cwd": "/tmp/wt",
                    "workspaceId": "/tmp/wt"
                }],
                "nextCursor": null
            }),
        ),
        ("POST", "/v1/instances") => {
            let parsed: Value = serde_json::from_str(body).unwrap_or(json!({}));
            let host_id = parsed
                .get("hostId")
                .and_then(Value::as_str)
                .unwrap_or("hst_1");
            (
                200,
                json!({
                    "instance": {
                        "instanceId": "ins_test",
                        "hostId": host_id,
                        "kind": parsed.get("kind").cloned().unwrap_or(json!("claude")),
                        "driver": parsed.get("driver").cloned().unwrap_or(json!("claude-print")),
                        "lifecycle": "requested",
                        "activity": "idle",
                        "connectivity": "disconnected",
                        "durableSeq": "0",
                        "title": parsed.get("title").cloned().or_else(|| parsed.get("name").cloned()).unwrap_or(json!("ins_test")),
                        "name": parsed.get("name").cloned(),
                        "cwd": parsed.get("cwd").cloned(),
                        "workspaceId": parsed.get("workspaceId").cloned().or_else(|| parsed.get("cwd").cloned()),
                        "worktree": parsed.get("worktree").cloned()
                    },
                    "command": {
                        "commandId": "cmd_create",
                        "state": "accepted",
                        "operation": "instance.create"
                    }
                }),
            )
        }
        ("POST", path) if path.starts_with("/v1/instances/") && path.ends_with("/commands") => {
            let parsed: Value = serde_json::from_str(body).unwrap_or(json!({}));
            (
                200,
                json!({
                    "command": {
                        "commandId": parsed.get("commandId").cloned().unwrap_or(json!("cmd_op")),
                        "state": "accepted",
                        "operation": parsed.get("operation").cloned().unwrap_or(json!("instance.send"))
                    },
                    "replayed": false
                }),
            )
        }
        ("GET", path) if path.contains("/journal") => (
            200,
            json!({
                "instanceId": "ins_test",
                "durableSeq": "1",
                "events": [
                    { "seq": 1, "type": "run.terminal", "event": { "type": "run.terminal", "text": "DONE abc" } },
                    { "seq": 2, "type": "raw_tty", "event": { "type": "raw_tty", "text": "screen line" } }
                ]
            }),
        ),
        ("POST", "/v1/fleet/broadcast") => {
            let parsed: Value = serde_json::from_str(body).unwrap_or(json!({}));
            let operation = parsed
                .get("operation")
                .cloned()
                .unwrap_or(json!("instance.send"));
            let kinds: Vec<String> = parsed
                .get("kinds")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            // The single mock instance is `ins_test` on `hst_1`, kind claude.
            let selected = kinds.is_empty() || kinds.iter().any(|k| k == "claude");
            let results = if selected {
                json!([{
                    "instanceId": "ins_test",
                    "hostId": "hst_1",
                    "kind": "claude",
                    "ok": true,
                    "commandId": "cmd_broadcast",
                    "state": "accepted",
                    "forwarded": true,
                    "replayed": false
                }])
            } else {
                json!([])
            };
            (
                200,
                json!({
                    "operation": operation,
                    "accepted": if selected { 1 } else { 0 },
                    "failed": 0,
                    "skipped": if selected { 0 } else { 1 },
                    "selected": if selected { 1 } else { 0 },
                    "results": results
                }),
            )
        }
        (_, path) if path.starts_with("/v1/fleet") => {
            (404, json!({ "code": "NOT_FOUND", "error": "not found" }))
        }
        _ => (404, json!({ "code": "NOT_FOUND", "error": "not found" })),
    }
}

/// D-028 §4.5 staged attachment metadata, as `GET /v1/attachments` returns it.
fn attachment(object_id: &str, media_type: &str, size: u64) -> Value {
    json!({
        "objectId": object_id,
        "instanceId": "ins_test",
        "mediaType": media_type,
        "name": format!("{object_id}.bin"),
        "size": size,
        "digest": "0".repeat(64),
        "expiresAt": "2126-01-01T00:00:00.000Z",
    })
}

/// Same metadata plus the `[Image #n]` anchor a numbered send recorded.
fn attachment_indexed(object_id: &str, media_type: &str, size: u64, index: i64) -> Value {
    let mut value = attachment(object_id, media_type, size);
    value["index"] = json!(index);
    value
}

/// `GET /v1/attachments/{objectId}/content` for the fixture objects.
///
/// `obj_other` belongs to a different session, so the mock answers 403 exactly
/// as the Hub does — the wrong-session path is a real HTTP refusal, not a
/// client-side guess.
fn attachment_content(path: &str) -> (u16, Value) {
    use base64::Engine as _;
    let object_id = path
        .strip_prefix("/v1/attachments/")
        .and_then(|rest| rest.strip_suffix("/content"))
        .unwrap_or_default();
    let base64 = base64::engine::general_purpose::STANDARD;
    // A 1x1 red PNG signature plus filler; the Hub sniffed the type already.
    let (media_type, size, data) = match object_id {
        "obj_png" => ("image/png", 168, base64.encode(png_fixture())),
        "obj_txt" => ("text/plain", 11, base64.encode("hello agent")),
        "obj_pdf" => ("application/pdf", 512, base64.encode("%PDF-1.7 fake")),
        "obj_big" => ("image/png", 3_584 * 1024 + 1, base64.encode(png_fixture())),
        "obj_other" => {
            return (403, json!({"code":"FORBIDDEN", "error":"forbidden"}));
        }
        _ => return (404, json!({"code":"NOT_FOUND", "error":"not found"})),
    };
    let mut body = attachment(object_id, media_type, size);
    body["encoding"] = json!("base64");
    body["data"] = json!(data);
    (200, body)
}

fn png_fixture() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.resize(168, 0x5A);
    bytes
}

async fn read_http(stream: &mut TcpStream) -> std::io::Result<Option<(String, String, String)>> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(parsed) = try_parse(&buf) {
            return Ok(Some(parsed));
        }
        if buf.len() > 1_000_000 {
            break;
        }
    }
    Ok(try_parse(&buf))
}

fn try_parse(buf: &[u8]) -> Option<(String, String, String)> {
    let text = std::str::from_utf8(buf).ok()?;
    let idx = text.find("\r\n\r\n")?;
    let head = &text[..idx];
    let rest = &text[idx + 4..];
    let mut lines = head.lines();
    let req = lines.next()?;
    let mut parts = req.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_len = 0usize;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.eq_ignore_ascii_case("content-length") {
            content_len = value.trim().parse().ok()?;
        }
    }
    if rest.len() < content_len {
        return None;
    }
    Some((method, path, rest[..content_len].to_string()))
}
