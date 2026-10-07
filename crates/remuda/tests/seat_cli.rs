//! `ma-seat-cli` (D-057 §3.2): `remuda instance create` seating flags against
//! a raw-TCP fake Hub. Every case uses an explicit `--host`, which is the only
//! request the CLI sends before the POST (no `/v1/caller`, no host listing),
//! so the captured request is deterministic — no sequence assertions.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

/// The exact request origin/main posts for this argument set; the seating
/// flags must not move, add or remove a single byte when none are given.
const BASELINE_BODY: &str = r#"{"kind":"claude","driver":"claude-print","hostId":"hst_seat","placement":{"host":"hst_seat"}}"#;

/// 403 body the real Hub returns when an Agent/Bot caller sets `restart`
/// (D-057 §6.1; wording mirrors `HubError::ForbiddenReason`).
const RESTART_FORBIDDEN: &str = "the restart policy can only be set by a Human creator; \
     Agent and Bot origins may not set it (D-057 §6.1)";

/// 409 body the real Hub returns for a second live `address-owner` holder.
const ADDRESS_OWNER_CONFLICT: &str =
    "an active instance already holds the address-owner grant (one per Hub)";

#[derive(Default)]
struct FakeState {
    /// Raw POST bodies, in arrival order.
    creates: Vec<String>,
    /// Number of TCP connections accepted (the garbage-`--restart` case
    /// expects zero).
    accepts: u64,
}

/// Spawn the fake Hub; returns (base URL, shared state).
fn spawn_fake() -> (String, Arc<Mutex<FakeState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let state = Arc::new(Mutex::new(FakeState::default()));
    let shared = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = shared.clone();
            thread::spawn(move || {
                let _ = handle(stream, shared);
            });
        }
    });
    (format!("http://{addr}"), state)
}

fn run_create(hub: &str, token: &str, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.args(["instance", "create"])
        .args(args)
        .env("REMUDA_HUB", hub)
        .env("REMUDA_TOKEN", token)
        .env_remove("REMUDA_BOOTSTRAP_TOKEN")
        .env_remove("REMUDA_DATA_DIR")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env_remove("http_proxy")
        .env_remove("https_proxy")
        .env_remove("all_proxy");
    cmd.output().expect("run remuda")
}

fn last_create_body(state: &Arc<Mutex<FakeState>>) -> Value {
    let state = state.lock().unwrap();
    let raw = state.creates.last().expect("a create was posted");
    serde_json::from_str(raw).expect("posted body is JSON")
}

fn handle(mut stream: TcpStream, state: Arc<Mutex<FakeState>>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let (method, path, authorization, body) = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break (
                "GET".to_string(),
                "/".to_string(),
                String::new(),
                String::new(),
            );
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(parsed) = parse_request(&buf) {
            break parsed;
        }
    };
    {
        let mut state = state.lock().unwrap();
        state.accepts += 1;
    }
    let path_only = path.split('?').next().unwrap_or(&path).to_string();
    let parsed: Value = serde_json::from_str(&body).unwrap_or(json!({}));

    let (status, payload) = if method == "POST" && path_only == "/v1/instances" {
        state.lock().unwrap().creates.push(body.clone());
        let is_agent = authorization.contains("agent-token");
        let holds_address_owner =
            parsed
                .get("grants")
                .and_then(Value::as_array)
                .is_some_and(|grants| {
                    grants
                        .iter()
                        .any(|verb| verb.as_str() == Some("address-owner"))
                });
        if is_agent && parsed.get("restart").is_some() {
            (
                403u16,
                json!({ "error": RESTART_FORBIDDEN, "code": "FORBIDDEN" }),
            )
        } else if holds_address_owner {
            // First holder is accepted; any further one conflicts.
            let owner_creates = state
                .lock()
                .unwrap()
                .creates
                .iter()
                .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
                .filter(|earlier| {
                    earlier
                        .get("grants")
                        .and_then(Value::as_array)
                        .is_some_and(|grants| {
                            grants
                                .iter()
                                .any(|verb| verb.as_str() == Some("address-owner"))
                        })
                })
                .count();
            if owner_creates >= 2 {
                (
                    409,
                    json!({ "error": ADDRESS_OWNER_CONFLICT, "code": "COMMAND_ID_CONFLICT" }),
                )
            } else {
                create_ok(&parsed, "ins_seat")
            }
        } else {
            create_ok(&parsed, "ins_test")
        }
    } else if method == "GET" && path_only.starts_with("/v1/instances/") {
        // Echo the most recent create as the stored projection, with the same
        // field names GET /v1/instances/{id} projects.
        let stored: Value = state
            .lock()
            .unwrap()
            .creates
            .last()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .unwrap_or(json!({}));
        let id = path_only
            .strip_prefix("/v1/instances/")
            .unwrap_or("ins_seat")
            .to_string();
        // The Hub derives `projectId` from a single-project scope when the
        // shortcut field is absent (InstanceRecord::project_id).
        let derived_project = stored
            .get("projectId")
            .filter(|value| value.is_string())
            .or_else(|| {
                stored
                    .pointer("/scope/projectIds")
                    .and_then(Value::as_array)
                    .and_then(|ids| (ids.len() == 1).then(|| &ids[0]))
            })
            .cloned();
        (
            200,
            json!({
                "instanceId": id,
                "hostId": stored.get("hostId").cloned().unwrap_or(json!("hst_seat")),
                "lifecycle": "ready",
                "activity": "idle",
                "grants": stored.get("grants").cloned().unwrap_or(json!([])),
                "scope": stored.get("scope").cloned().unwrap_or(json!({})),
                "projectId": derived_project,
                "permissionMode": stored.get("permissionMode"),
                "model": stored.get("model"),
                "restart": stored.get("restart"),
            }),
        )
    } else {
        (404, json!({ "code": "NOT_FOUND", "error": "not found" }))
    };

    let bytes = payload.to_string();
    let reason = match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{bytes}",
        bytes.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}

fn create_ok(parsed: &Value, id: &str) -> (u16, Value) {
    (
        200,
        json!({
            "instance": {
                "instanceId": id,
                "hostId": parsed.get("hostId").cloned().unwrap_or(json!("hst_seat")),
                "kind": parsed.get("kind").cloned().unwrap_or(json!("claude")),
                "driver": parsed.get("driver").cloned().unwrap_or(json!("claude-print")),
                "lifecycle": "requested"
            },
            "command": { "commandId": "cmd_create", "state": "accepted" }
        }),
    )
}

fn parse_request(buf: &[u8]) -> Option<(String, String, String, String)> {
    let text = std::str::from_utf8(buf).ok()?;
    let header_end = text.find("\r\n\r\n")?;
    let head = &text[..header_end];
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut authorization = String::new();
    let mut content_len = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if key == "content-length" {
                content_len = value.parse().ok()?;
            } else if key == "authorization" {
                authorization = value.to_string();
            }
        }
    }
    let rest = &text[header_end + 4..];
    if rest.len() < content_len {
        return None;
    }
    Some((method, path, authorization, rest[..content_len].to_string()))
}

// ── help ───────────────────────────────────────────────────────────────────

#[test]
fn instance_create_help_lists_seating_flags() {
    let output = Command::new(bin())
        .args(["instance", "create", "--help"])
        .output()
        .expect("help");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for flag in [
        "--role",
        "--grant",
        "--scope-project",
        "--scope-host",
        "--scope-workspace",
        "--project",
        "--permission-mode",
        "--model",
        "--restart",
    ] {
        assert!(stdout.contains(flag), "help missing {flag}: {stdout}");
    }
}

// ── byte-identical baseline ─────────────────────────────────────────────────

#[test]
fn create_without_seating_flags_is_byte_identical_to_baseline() {
    let (hub, state) = spawn_fake();
    let output = run_create(&hub, "human-token", &["--host", "hst_seat"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = state.lock().unwrap();
    assert_eq!(state.creates.len(), 1, "exactly one create request");
    assert_eq!(
        state.creates[0], BASELINE_BODY,
        "omitted seating flags must leave the request byte-identical to today"
    );
}

// ── every flag maps one-to-one onto its body field ──────────────────────────

#[test]
fn seating_flags_round_trip_into_the_request_body() {
    let (hub, state) = spawn_fake();
    let output = run_create(
        &hub,
        "human-token",
        &[
            "--host",
            "hst_seat",
            "--driver",
            "claude-sdk",
            "--name",
            "main",
            "--role",
            "top-coordinator",
            "--grant",
            "address-owner",
            "--grant",
            "dispatch",
            "--scope-project",
            "prj_one",
            "--scope-project",
            "prj_two",
            "--scope-host",
            "hst_seat",
            "--scope-workspace",
            "wsp_persistent",
            "--project",
            "prj_one",
            "--permission-mode",
            "acceptEdits",
            "--model",
            "claude-model-1",
            "--restart",
            "process-loss:3",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = last_create_body(&state);
    assert_eq!(body["role"], json!("top-coordinator"));
    assert_eq!(body["grants"], json!(["address-owner", "dispatch"]));
    assert_eq!(
        body["scope"],
        json!({
            "projectIds": ["prj_one", "prj_two"],
            "hostIds": ["hst_seat"],
            "workspaceIds": ["wsp_persistent"],
        })
    );
    assert_eq!(body["projectId"], json!("prj_one"));
    assert_eq!(body["permissionMode"], json!("acceptEdits"));
    assert_eq!(body["model"], json!("claude-model-1"));
    assert_eq!(
        body["restart"],
        json!({ "onProcessLoss": true, "maxPerHour": 3 })
    );
}

#[test]
fn restart_none_and_partial_scope_omit_their_fields() {
    let (hub, state) = spawn_fake();
    let output = run_create(
        &hub,
        "human-token",
        &[
            "--host",
            "hst_seat",
            "--scope-host",
            "hst_seat",
            "--restart",
            "none",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = last_create_body(&state);
    assert_eq!(
        body["scope"],
        json!({ "hostIds": ["hst_seat"] }),
        "only the named dimension is sent"
    );
    assert!(
        body.get("restart").is_none(),
        "`--restart none` omits the field"
    );
    assert!(body.get("grants").is_none());
    assert!(body.get("role").is_none());
    assert!(body.get("projectId").is_none());
    assert!(body.get("permissionMode").is_none());
    assert!(body.get("model").is_none());
}

// ── GET projection is displayed, not filtered ──────────────────────────────

#[test]
fn show_displays_the_stored_seat_fields() {
    let (hub, state) = spawn_fake();
    let create = run_create(
        &hub,
        "human-token",
        &[
            "--host",
            "hst_seat",
            "--grant",
            "address-owner",
            "--scope-project",
            "prj_one",
            "--permission-mode",
            "manual",
            "--model",
            "claude-model-1",
            "--restart",
            "process-loss:3",
        ],
    );
    assert!(
        create.status.success(),
        "{}",
        String::from_utf8_lossy(&create.stderr)
    );

    let output = Command::new(bin())
        .args(["instance", "show", "ins_seat"])
        .env("REMUDA_HUB", &hub)
        .env("REMUDA_TOKEN", "human-token")
        .env_remove("REMUDA_BOOTSTRAP_TOKEN")
        .output()
        .expect("show");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let view: Value = serde_json::from_slice(&output.stdout).expect("stdout is JSON");
    assert_eq!(view["grants"], json!(["address-owner"]));
    assert_eq!(view["scope"], json!({ "projectIds": ["prj_one"] }));
    assert_eq!(view["projectId"], json!("prj_one"));
    assert_eq!(view["permissionMode"], json!("manual"));
    assert_eq!(view["model"], json!("claude-model-1"));
    assert_eq!(
        view["restart"],
        json!({ "onProcessLoss": true, "maxPerHour": 3 })
    );
    drop(state);
}

// ── Hub refusals surface the Hub's own reason text ─────────────────────────

#[test]
fn restart_from_an_agent_credential_prints_the_hubs_403_reason() {
    let (hub, state) = spawn_fake();
    let output = run_create(
        &hub,
        "agent-token",
        &[
            "--host",
            "hst_seat",
            "--restart",
            "process-loss:3",
            "--prompt",
            "a successor",
        ],
    );
    assert!(!output.status.success());
    assert_ne!(
        output.status.code(),
        Some(2),
        "a Hub 403 is not a usage error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("403"), "{stderr}");
    assert!(
        stderr.contains("only be set by a Human creator"),
        "CLI must print the Hub's reason, not dump opaque JSON: {stderr}"
    );
    assert_eq!(
        state.lock().unwrap().creates.len(),
        1,
        "the request reached the Hub"
    );
}

#[test]
fn a_second_address_owner_holder_prints_the_conflict_text() {
    let (hub, _state) = spawn_fake();
    let first = run_create(
        &hub,
        "human-token",
        &["--host", "hst_seat", "--grant", "address-owner"],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run_create(
        &hub,
        "human-token",
        &["--host", "hst_seat", "--grant", "address-owner"],
    );
    assert!(!second.status.success());
    assert_eq!(
        second.status.code(),
        Some(1),
        "a Hub 409 exits 1, not the usage code 2"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(stderr.contains("409"), "{stderr}");
    assert!(
        stderr.contains("address-owner grant"),
        "CLI must surface the Hub conflict text: {stderr}"
    );
}

// ── client-side usage validation ───────────────────────────────────────────

#[test]
fn a_garbage_restart_value_is_a_usage_error_that_sends_nothing() {
    // The listener exists only to count connections: clap must reject the
    // value before the CLI opens one.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let accepts = Arc::new(Mutex::new(0u64));
    let counter = accepts.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            *counter.lock().unwrap() += 1;
            drop(stream);
        }
    });
    for bad in ["bananas", "process-loss:0", "process-loss:abc"] {
        let output = run_create(
            &format!("http://{addr}"),
            "human-token",
            &["--host", "hst_seat", "--restart", bad],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{bad:?} must be a clap usage error (exit 2)"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--restart"),
            "error names the flag: {stderr}"
        );
    }
    assert_eq!(*accepts.lock().unwrap(), 0, "no request may be sent");
}
