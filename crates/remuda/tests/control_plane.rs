//! Clap wiring and stdio MCP JSON-RPC against a mock Hub HTTP server.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

#[test]
fn instance_create_help_lists_host_and_labels() {
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
    assert!(stdout.contains("--host"), "{stdout}");
    assert!(stdout.contains("--labels"), "{stdout}");
}

#[test]
fn fleet_run_help_lists_hosts_and_labels() {
    let output = Command::new(bin())
        .args(["fleet", "run", "--help"])
        .output()
        .expect("help");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--hosts"), "{stdout}");
    assert!(stdout.contains("--labels"), "{stdout}");
}

#[test]
fn mcp_help_exists() {
    let output = Command::new(bin())
        .args(["mcp", "--help"])
        .output()
        .expect("help");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn mcp_stdio_initialize_list_and_call() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    thread::spawn(move || {
        while let Ok((stream, _)) = listener.accept() {
            let _ = handle_http(stream);
        }
    });

    let mut child = Command::new(bin())
        .arg("mcp")
        .env("REMUDA_HUB", format!("http://{addr}"))
        .env("REMUDA_TOKEN", "test-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn remuda mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");

    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"0"}}}}}}"#
    )
    .expect("write initialize");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{{}}}}"#
    )
    .expect("write list");
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"remuda_instance_create","arguments":{{"host":"hst_1","prompt":"cargo test"}}}}}}"#
    )
    .expect("write call");
    drop(stdin);

    let mut buf = String::new();
    let mut reader = stdout;
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while buf.lines().filter(|l| !l.is_empty()).count() < 3 && std::time::Instant::now() < deadline
    {
        let mut chunk = [0u8; 4096];
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.push_str(&String::from_utf8_lossy(&chunk[..n])),
            Err(_) => break,
        }
    }
    let mut err = Vec::new();
    if let Some(mut stderr) = child.stderr.take() {
        let _ = stderr.read_to_end(&mut err);
    }
    let _ = child.kill();
    let _ = child.wait();

    let lines: Vec<&str> = buf.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.len() >= 3,
        "expected 3 JSON-RPC lines, got {} from {buf:?} stderr={}",
        lines.len(),
        String::from_utf8_lossy(&err)
    );

    let init: Value = serde_json::from_str(lines[0]).expect("init");
    assert_eq!(init["result"]["serverInfo"]["name"], json!("remuda"));

    let list: Value = serde_json::from_str(lines[1]).expect("list");
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"remuda_instance_create".into()));
    assert!(names.contains(&"remuda_fleet_run".into()));

    let call: Value = serde_json::from_str(lines[2]).expect("call");
    assert_eq!(call["result"]["isError"], json!(false));
    let text = call["result"]["content"][0]["text"].as_str().expect("text");
    let body: Value = serde_json::from_str(text).expect("instance json");
    assert_eq!(body["instance"]["instanceId"], json!("ins_test"));
}

fn handle_http(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let text = String::from_utf8_lossy(&buf[..n]);
    let request_line = text.lines().next().unwrap_or("");
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let path_only = path.split('?').next().unwrap_or(path);
    let body_idx = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
    let req_body = &text[body_idx.min(text.len())..];

    let (status, payload) = if path_only == "/v1/hosts" {
        (
            200,
            json!({"items":[{"hostId":"hst_1","online":true,"labels":["region=sg"],"instanceCount":0,"maxInstances":4}],"nextCursor":null}),
        )
    } else if path_only == "/v1/instances" && request_line.starts_with("POST") {
        let parsed: Value =
            serde_json::from_str(req_body.trim_end_matches('\0')).unwrap_or(json!({}));
        let host_id = parsed
            .get("hostId")
            .and_then(Value::as_str)
            .unwrap_or("hst_1");
        (
            200,
            json!({
                "instance": {"instanceId":"ins_test","hostId":host_id,"lifecycle":"requested"},
                "command": {"commandId":"cmd_create","state":"accepted"}
            }),
        )
    } else if path_only.starts_with("/v1/fleet") {
        (404, json!({"code":"NOT_FOUND","error":"not found"}))
    } else if path_only == "/v1/instances" {
        (200, json!({"items":[],"nextCursor":null}))
    } else {
        (404, json!({"code":"NOT_FOUND"}))
    };
    let bytes = payload.to_string();
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{bytes}",
        bytes.len()
    );
    stream.write_all(response.as_bytes())?;
    Ok(())
}
