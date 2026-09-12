//! Duplex fake agent for initialize / new / prompt / cancel.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use remuda_acp_wire::{
    SessionSpec, StopReason, client_capabilities_declare_fs_or_terminal, connect_byte_streams,
    encode_notification, encode_response,
};

async fn write_rpc<W: AsyncWriteExt + Unpin>(w: &mut W, line: String) {
    w.write_all(line.as_bytes()).await.unwrap();
    w.write_all(b"\n").await.unwrap();
    w.flush().await.unwrap();
}

async fn run_fake_agent(stream: tokio::io::DuplexStream) {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let mut pending_prompt: Option<Value> = None;
    let session_id = "sess-1";

    while let Ok(Some(line)) = lines.next_line().await {
        let rpc: Value = serde_json::from_str(&line).unwrap();
        let method = rpc.get("method").and_then(Value::as_str).unwrap_or("");
        let id = rpc.get("id").cloned();
        let params = rpc.get("params").cloned().unwrap_or(Value::Null);

        match method {
            "initialize" => {
                assert!(
                    !client_capabilities_declare_fs_or_terminal(&params),
                    "client declared fs/terminal: {params}"
                );
                assert_eq!(params["clientCapabilities"], json!({}));
                write_rpc(
                    &mut writer,
                    encode_response(
                        id.unwrap(),
                        json!({
                            "protocolVersion": 1,
                            "agentCapabilities": {}
                        }),
                    )
                    .unwrap(),
                )
                .await;
            }
            "session/new" => {
                assert_eq!(params["_meta"]["yoloMode"], true);
                write_rpc(
                    &mut writer,
                    encode_response(id.unwrap(), json!({ "sessionId": session_id })).unwrap(),
                )
                .await;
            }
            "session/prompt" => {
                let text = params
                    .pointer("/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                write_rpc(
                    &mut writer,
                    encode_notification(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_thought_chunk",
                                "content": { "type": "text", "text": "think" }
                            }
                        }),
                    )
                    .unwrap(),
                )
                .await;
                if text.contains("cancel-me") {
                    pending_prompt = id;
                    continue;
                }
                write_rpc(
                    &mut writer,
                    encode_notification(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "agent_message_chunk",
                                "content": { "type": "text", "text": "OK" }
                            }
                        }),
                    )
                    .unwrap(),
                )
                .await;
                write_rpc(
                    &mut writer,
                    encode_response(id.unwrap(), json!({ "stopReason": "end_turn" })).unwrap(),
                )
                .await;
            }
            "session/cancel" => {
                if let Some(prompt_id) = pending_prompt.take() {
                    write_rpc(
                        &mut writer,
                        encode_response(prompt_id, json!({ "stopReason": "cancelled" })).unwrap(),
                    )
                    .await;
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn fake_agent_lifecycle() {
    let (client_end, agent_end) = tokio::io::duplex(64 * 1024);
    let agent = tokio::spawn(run_fake_agent(agent_end));
    let (read, write) = tokio::io::split(client_end);

    let outcome = connect_byte_streams(write.compat_write(), read.compat(), async |conn| {
        let init = conn.initialize().await?;
        assert_eq!(init.protocol_version, remuda_acp_wire::ProtocolVersion::V1);

        let mut session = conn
            .new_session(SessionSpec::new(std::env::temp_dir()))
            .await?;
        assert_eq!(session.session_id().to_string(), "sess-1");

        let turn = session.prompt("Reply with exactly OK").await?;
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(turn.assistant_text(), "OK");

        let sid = session.session_id().clone();
        let (cancelled, cancel_sent) = tokio::join!(session.prompt("please cancel-me"), async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            conn.cancel(sid)
        });
        cancel_sent?;
        let cancelled = cancelled?;
        assert_eq!(cancelled.stop_reason, StopReason::Cancelled);
        Ok::<_, remuda_acp_wire::Error>(())
    })
    .await;

    outcome.unwrap();
    let _ = agent.await;
}
