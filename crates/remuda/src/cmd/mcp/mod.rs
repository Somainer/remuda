//! stdio MCP server (`remuda mcp`) exposing instance/fleet tools over JSON-RPC 2.0.
//!
//! Framing: Claude Code / LSP `Content-Length` headers, or NDJSON (one JSON
//! object per line) for tests and curl-style clients. Hand-written rather than
//! the `rmcp` crate so the CLI crate stays a thin Hub HTTP client.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};

use super::hub_client::{HubClient, HubOpts, block_on};
use registry::Tool;

mod args;
mod registry;

macro_rules! tool_groups {
    ($($group:ident),* $(,)?) => {
        $(mod $group;)*
        fn tools() -> Vec<Tool> {
            [$($group::tools()),*].into_iter().flatten().collect()
        }
    };
}
// Add tools inside their group; a new group needs just one entry here.
tool_groups!(instance, worktree, fleet, merge, doctor);

pub(crate) fn tools_catalog() -> Vec<Value> {
    tools().iter().map(Tool::catalog).collect()
}

const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Unknown,
    Ndjson,
    Lsp,
}

/// Run `remuda mcp` on stdio until stdin EOF.
pub(crate) fn run(hub: HubOpts) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        let stdin = BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        serve_rpc(stdin, stdout, client).await
    })
}

pub(crate) async fn serve_rpc<R, W>(mut reader: R, mut writer: W, client: HubClient) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut framing = Framing::Unknown;
    loop {
        let Some(msg) = read_rpc(&mut reader, &mut framing).await? else {
            return Ok(());
        };
        if let Some(response) = handle_rpc(&msg, &client).await {
            write_rpc(&mut writer, framing, &response).await?;
        }
    }
}

pub(crate) async fn handle_rpc(msg: &Value, client: &HubClient) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let Some(method) = method else {
        return Some(rpc_error(id, -32600, "invalid request"));
    };
    match method {
        "initialize" => Some(rpc_ok(id, initialize_result(&params))),
        "ping" => Some(rpc_ok(id, json!({}))),
        "tools/list" => Some(rpc_ok(id, json!({ "tools": tools_catalog() }))),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let content = match tools().iter().find(|tool| tool.name == name) {
                Some(tool) => tool.call(client, args).await,
                None => tool_content(Err(anyhow!("unknown tool: {name}"))),
            };
            Some(rpc_ok(id, content))
        }
        "notifications/initialized" | "initialized" | "notifications/cancelled" => None,
        other => Some(rpc_error(id, -32601, &format!("method not found: {other}"))),
    }
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    let version = if requested == PROTOCOL_VERSION || requested == "2025-03-26" {
        requested
    } else {
        PROTOCOL_VERSION
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "remuda",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn tool_content(result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "isError": false,
        }),
        Err(err) => json!({
            "content": [{ "type": "text", "text": err.to_string() }],
            "isError": true,
        }),
    }
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

async fn read_rpc<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    framing: &mut Framing,
) -> Result<Option<Value>> {
    match *framing {
        Framing::Lsp => read_lsp(reader, None).await,
        Framing::Ndjson => read_ndjson(reader).await,
        Framing::Unknown => loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                return Ok(None);
            }
            if line.to_ascii_lowercase().starts_with("content-length:") {
                *framing = Framing::Lsp;
                return read_lsp(reader, Some(line)).await;
            }
            if line.trim().is_empty() {
                continue;
            }
            *framing = Framing::Ndjson;
            return Ok(Some(serde_json::from_str(line.trim())?));
        },
    }
}

async fn read_ndjson<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(Some(serde_json::from_str(trimmed)?));
    }
}

async fn read_lsp<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    first_line: Option<String>,
) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    if let Some(line) = first_line.as_deref() {
        parse_lsp_header(line, &mut content_length);
    }
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        parse_lsp_header(&line, &mut content_length);
    }
    let len = content_length.ok_or_else(|| anyhow!("MCP Content-Length missing"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

fn parse_lsp_header(line: &str, content_length: &mut Option<usize>) {
    let Some((key, value)) = line.split_once(':') else {
        return;
    };
    if key.trim().eq_ignore_ascii_case("content-length") {
        *content_length = value.trim().parse().ok();
    }
}

async fn write_rpc<W: AsyncWrite + Unpin>(
    writer: &mut W,
    framing: Framing,
    msg: &Value,
) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    match framing {
        Framing::Lsp => {
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            writer.write_all(header.as_bytes()).await?;
            writer.write_all(&body).await?;
        }
        Framing::Ndjson | Framing::Unknown => {
            writer.write_all(&body).await?;
            writer.write_all(b"\n").await?;
        }
    }
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::hub_client::connect_for_test;
    use crate::cmd::test_hub::spawn_mock_hub;

    fn dummy_client() -> HubClient {
        connect_for_test("http://127.0.0.1:1".into(), "t".into()).expect("client")
    }

    #[tokio::test]
    async fn initialize_and_tools_list() {
        let client = dummy_client();
        let init = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        });
        let resp = handle_rpc(&init, &client).await.expect("response");
        assert_eq!(resp["result"]["serverInfo"]["name"], json!("remuda"));
        assert_eq!(resp["result"]["protocolVersion"], json!(PROTOCOL_VERSION));

        let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}});
        let resp = handle_rpc(&list, &client).await.expect("response");
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        for expected in [
            "remuda_instance_create",
            "remuda_instance_list",
            "remuda_instance_send",
            "remuda_instance_wait",
            "remuda_instance_read",
            "remuda_instance_keys",
            "remuda_instance_respond",
            "remuda_instance_stop",
            "remuda_instance_rm",
            "remuda_worktree_create",
            "remuda_fleet_run",
            "remuda_fleet_send",
            "remuda_fleet_keys",
            "remuda_merge",
            "remuda_doctor",
            "remuda_worktree_rm",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn unknown_method_is_json_rpc_error() {
        let client = dummy_client();
        let msg = json!({"jsonrpc":"2.0","id":9,"method":"nope"});
        let resp = handle_rpc(&msg, &client).await.expect("response");
        assert_eq!(resp["error"]["code"], json!(-32601));
    }

    #[tokio::test]
    async fn tools_call_list_keys_and_fleet_send_against_mock_hub() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let list = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "remuda_instance_list", "arguments": {} }
        });
        let resp = handle_rpc(&list, &client).await.expect("list");
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["items"][0]["name"], json!("reviewer"));
        assert_eq!(body["items"][0]["cwd"], json!("/tmp/wt"));
        assert_eq!(body["items"][0]["host"], json!("sg"));

        let keys = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "remuda_instance_keys",
                "arguments": { "instanceId": "ins_test", "keys": ["enter"] }
            }
        });
        let resp = handle_rpc(&keys, &client).await.expect("keys");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");

        let send = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_send",
                "arguments": { "all": true, "text": "PAUSE git commits" }
            }
        });
        let resp = handle_rpc(&send, &client).await.expect("fleet send");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["accepted"], json!(1));
        assert_eq!(body["failed"], json!(0));
        assert_eq!(body["results"][0]["instanceId"], json!("ins_test"));
        assert_eq!(body["text"], json!("PAUSE git commits"));

        let keys = json!({
            "jsonrpc": "2.0",
            "id": 13,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_keys",
                "arguments": { "all": true, "kind": "claude", "keys": ["esc"] }
            }
        });
        let resp = handle_rpc(&keys, &client).await.expect("fleet keys");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["operation"], json!("tty.write"));
        assert_eq!(body["accepted"], json!(1));
        assert_eq!(body["keys"], json!(["esc"]));

        // A kind filter that matches nothing still returns a summary, not an error.
        let miss = json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_send",
                "arguments": { "all": true, "kinds": ["codex"], "text": "hi" }
            }
        });
        let resp = handle_rpc(&miss, &client).await.expect("fleet send miss");
        assert_eq!(resp["result"]["isError"], json!(false), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json");
        assert_eq!(body["accepted"], json!(0));
        assert_eq!(body["skipped"], json!(1));
    }

    #[tokio::test]
    async fn fleet_keys_rejects_unknown_key_before_hub_call() {
        let client = dummy_client();
        let call = json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_keys",
                "arguments": { "all": true, "keys": ["nope"] }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(true), "{resp}");
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("unknown key"), "{text}");
    }

    #[tokio::test]
    async fn tools_call_create_against_mock_hub() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "remuda_instance_create",
                "arguments": { "host": "hst_1", "prompt": "cargo test" }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().expect("text");
        let body: Value = serde_json::from_str(text).expect("json tool body");
        assert_eq!(body["instance"]["instanceId"], json!("ins_test"));
        assert_eq!(body["instance"]["hostId"], json!("hst_1"));
    }

    #[tokio::test]
    async fn tools_call_fleet_run_is_error_when_hub_404() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let call = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "remuda_fleet_run",
                "arguments": { "hosts": ["hst_1"], "prompt": "cargo test" }
            }
        });
        let resp = handle_rpc(&call, &client).await.expect("response");
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("not deployed") || text.contains("/v1/fleet"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn serve_rpc_ndjson_roundtrip() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
            "\n",
        );
        let mut out = Vec::new();
        serve_rpc(BufReader::new(input.as_bytes()), &mut out, client)
            .await
            .expect("serve");
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .expect("utf8")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);
        let list: Value = serde_json::from_str(lines[1]).expect("list json");
        assert!(
            list["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .any(|t| t["name"] == "remuda_instance_create")
        );
    }

    #[tokio::test]
    async fn serve_rpc_lsp_framing() {
        let client = dummy_client();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        let input = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut out = Vec::new();
        serve_rpc(BufReader::new(input.as_bytes()), &mut out, client)
            .await
            .expect("serve");
        let text = std::str::from_utf8(&out).expect("utf8");
        assert!(text.to_ascii_lowercase().contains("content-length:"));
        assert!(text.contains("\"jsonrpc\":\"2.0\""));
    }
}
