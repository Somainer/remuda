//! D-028 §4.5: the attachment tool schemas an agent sees are a stable contract.
//!
//! `remuda_attachment(objectId)` is named in prompt mentions the composer
//! writes and in the driver-side contract recorded in
//! `docs/design/evidence/attachments-2.md`. Renaming the tool or its one
//! required argument silently breaks every mention already queued, so the
//! schema is pinned here rather than merely exercised.
//!
//! Regenerate after an intentional change:
//!   echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
//!     | cargo run -q -p remuda -- mcp \
//!     | python3 -c 'import json,sys; ...'   # see the doc's §5

use serde_json::Value;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_remuda")
}

/// `tools/list` from a real `remuda mcp` process over NDJSON.
///
/// No Hub is reachable, which is the point: listing tools must not require
/// one, so a client can introspect before any credential resolves.
fn listed_tools() -> Vec<Value> {
    use std::io::Write as _;
    let mut child = Command::new(bin())
        .arg("mcp")
        .env("REMUDA_HUB", "http://127.0.0.1:1")
        .env("REMUDA_TOKEN", "golden-fixture")
        .env_remove("REMUDA_BOOTSTRAP_TOKEN")
        .env_remove("REMUDA_INSTANCE_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn remuda mcp");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}\n")
        .expect("write tools/list");
    let output = child.wait_with_output().expect("wait");
    let text = String::from_utf8(output.stdout).expect("utf8");
    let line = text.lines().find(|line| !line.trim().is_empty());
    let reply: Value = serde_json::from_str(line.expect("a reply line")).expect("json reply");
    reply["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone()
}

#[test]
fn the_attachment_tool_schemas_match_their_golden() {
    let mut listed: Vec<Value> = listed_tools()
        .into_iter()
        .filter(|tool| {
            matches!(
                tool["name"].as_str(),
                Some("remuda_attachment" | "remuda_attachments_list")
            )
        })
        .collect();
    listed.sort_by_key(|tool| tool["name"].as_str().unwrap_or_default().to_owned());
    assert_eq!(listed.len(), 2, "both attachment tools must be registered");

    let golden: Value =
        serde_json::from_str(include_str!("mcp_golden/attachment-tools.json")).expect("golden");
    assert_eq!(
        Value::Array(listed),
        golden,
        "attachment tool schemas drifted from tests/mcp_golden/attachment-tools.json; \
         if the change is intended, regenerate the golden and update the mention \
         contract in docs/design/evidence/attachments-2.md"
    );
}

/// The mention the composer writes quotes the tool name and the objectId. Both
/// halves must stay callable exactly as written, so assert the shape of the
/// contract rather than only that the tool exists.
#[test]
fn the_tool_accepts_exactly_the_argument_a_mention_quotes() {
    let golden: Value =
        serde_json::from_str(include_str!("mcp_golden/attachment-tools.json")).expect("golden");
    let fetch = golden
        .as_array()
        .expect("array")
        .iter()
        .find(|tool| tool["name"] == "remuda_attachment")
        .expect("remuda_attachment");
    assert_eq!(
        fetch["inputSchema"]["required"],
        serde_json::json!(["objectId"])
    );
    assert_eq!(
        fetch["inputSchema"]["properties"]["objectId"]["type"],
        serde_json::json!("string")
    );
    // An attachment fetch has no reason to take a path, a file, or a session
    // it could be pointed at.
    for banned in ["file", "path", "instanceId", "hostId"] {
        assert!(
            fetch["inputSchema"]["properties"].get(banned).is_none(),
            "remuda_attachment must not accept {banned}"
        );
    }
}
