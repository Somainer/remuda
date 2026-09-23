//! VCR peer for recorded `claude -p` stream-json sessions.
//!
//! Replays a sanitized capture (the same NDJSON shape
//! `crates/remuda-claude-wire/tests/fixtures/*.jsonl` uses): lines WITHOUT
//! `_dir` are CLI output emitted to stdout in order; a line carrying
//! `_dir:"in"` is a checkpoint — the peer blocks until the driver writes that
//! inbound frame on stdin and verifies it.
//!
//! Verification is deliberately strict:
//! - the initialize checkpoint accepts the driver's own generated request id
//!   (correlated with the rewritten init response), but still requires the
//!   control_request/initialize shape;
//! - the ExitPlanMode permission checkpoint deep-compares the WHOLE success
//!   payload against the recording (`behavior`, exact `updatedInput`, exact
//!   deny `message`, and NO extra fields such as `updatedPermissions`);
//! - EOF before the permission checkpoint is a failure.
//!
//! So a driver that silently escalates permissions, mutates the approved
//! input, swaps the verdict, or hangs before answering cannot pass.
//!
//! `FAKE_CLAUDE_FIXTURE` selects the capture; `FAKE_CLAUDE_STRICT=1`
//! additionally compares frame type/request-id at the non-permission
//! checkpoints.

use serde_json::Value;
use std::io::{self, BufRead, Write};

fn main() {
    let result = run();
    // Test-only side channel (does not touch stdout): a driver-driven test
    // sets FAKE_CLAUDE_RESULT_FILE and reads the replay's real exit
    // disposition (0 ok / 1 failure) after the run. Published ATOMICALLY via a
    // temp-file rename so a polling reader never observes a created-but-empty
    // file before the final status is on disk.
    if let Ok(status_path) = std::env::var("FAKE_CLAUDE_RESULT_FILE") {
        let code = if result.is_ok() { "0" } else { "1" };
        let path = std::path::PathBuf::from(status_path);
        if let Some(parent) = path.parent() {
            let tmp = parent.join(format!(
                ".{}.tmp",
                path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("replay-status")
            ));
            if std::fs::write(&tmp, code)
                .and_then(|()| std::fs::rename(&tmp, &path))
                .is_err()
            {
                // Best effort: fall back to a direct write if rename fails.
                let _ = std::fs::write(&path, code);
            }
        } else {
            let _ = std::fs::write(&path, code);
        }
    }
    match result {
        Ok(()) => std::process::exit(0),
        Err(err) => {
            eprintln!("fake-claude-replay: {err}");
            std::process::exit(1);
        }
    }
}

/// Which checkpoint kind a frame is.
#[derive(Debug)]
enum Checkpoint {
    /// Driver writes its initialize control request (dynamic id).
    Initialize,
    /// Driver writes the user prompt.
    UserPrompt,
    /// Driver writes the ExitPlanMode permission response.
    Permission,
    /// Any other inbound frame.
    Other,
}

fn checkpoint_of(frame: &Value) -> Option<Checkpoint> {
    if frame.get("_dir").and_then(Value::as_str) != Some("in") {
        return None;
    }
    if frame.get("type").and_then(Value::as_str) == Some("control_request")
        && frame.pointer("/request/subtype").and_then(Value::as_str) == Some("initialize")
    {
        return Some(Checkpoint::Initialize);
    }
    if frame.pointer("/response/response/behavior").is_some() {
        return Some(Checkpoint::Permission);
    }
    if frame.get("type").and_then(Value::as_str) == Some("user") {
        return Some(Checkpoint::UserPrompt);
    }
    Some(Checkpoint::Other)
}

fn mismatch(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Deep-compare the WHOLE permission-response envelope the driver sent with
/// the one recorded: top-level type, the `response` object's exact key set
/// (`type`, `subtype`, `request_id`, `response` — nothing added), and a
/// deep-equal inner payload. An added `updatedPermissions`, a changed
/// `updatedInput`, a wrong `subtype`, or an altered envelope is a mismatch.
fn verify_permission(expected: &Value, got: &Value) -> Result<(), io::Error> {
    if got.get("type").and_then(Value::as_str) != Some("control_response") {
        return Err(mismatch(format!(
            "permission frame type must be control_response, got: {got}"
        )));
    }
    let expected_response = expected
        .get("response")
        .ok_or_else(|| mismatch("fixture permission frame missing /response"))?;
    let got_response = got
        .get("response")
        .ok_or_else(|| mismatch("host frame missing /response"))?;
    let expected_keys = response_key_set(expected_response);
    let got_keys = response_key_set(got_response);
    if got_keys != expected_keys {
        return Err(mismatch(format!(
            "permission envelope key set mismatch:\n recorded: {expected_keys:?}\n host: {got_keys:?}"
        )));
    }
    if got_response.get("subtype").and_then(Value::as_str) != Some("success") {
        return Err(mismatch(format!(
            "permission response subtype must be success, got: {got_response}"
        )));
    }
    // Exact key set on the inner payload (behavior/updatedInput/message only).
    let expected_inner = expected_response.get("response").and_then(Value::as_object);
    let got_inner = got_response.get("response").and_then(Value::as_object);
    match (expected_inner, got_inner) {
        (Some(e), Some(g)) if e.len() == g.len() && e.keys().eq(g.keys()) => {}
        (Some(e), Some(g)) => {
            return Err(mismatch(format!(
                "permission inner payload key set mismatch:\n recorded: {:?}\n host: {:?}",
                e.keys().collect::<Vec<_>>(),
                g.keys().collect::<Vec<_>>()
            )));
        }
        _ => return Err(mismatch("permission inner payload missing")),
    }
    if got_response != expected_response {
        return Err(mismatch(format!(
            "permission envelope mismatch:\n recorded: {expected_response}\n host sent: {got_response}"
        )));
    }
    Ok(())
}

/// Sorted top-level keys of a JSON object, for exact-envelope comparison.
fn response_key_set(value: &Value) -> Option<Vec<String>> {
    value.as_object().map(|map| {
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        keys
    })
}

fn run() -> io::Result<()> {
    let path = std::env::var("FAKE_CLAUDE_FIXTURE")
        .map_err(|_| io::Error::other("FAKE_CLAUDE_FIXTURE is required"))?;
    let strict = std::env::var("FAKE_CLAUDE_STRICT").as_deref() == Ok("1");
    let raw = std::fs::read_to_string(path)?;
    let frames: Vec<Value> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    let stdin = io::stdin();
    let mut incoming = stdin.lock();
    let mut stdout = io::stdout().lock();
    // The driver generates the initialize request id; the emitted init
    // response is rewritten to echo it.
    let mut init_request_id: Option<String> = None;
    let mut permission_seen = false;

    for mut frame in frames {
        let Some(checkpoint) = checkpoint_of(&frame) else {
            // CLI output frame. Rewrite the init success response request id
            // to the one the driver actually generated.
            let is_init_success = frame.get("type").and_then(Value::as_str)
                == Some("control_response")
                && frame.pointer("/response/response/commands").is_some();
            if is_init_success
                && let Some(id) = init_request_id.take()
                && let Some(response) = frame
                    .pointer_mut("/response")
                    .and_then(Value::as_object_mut)
            {
                response.insert("request_id".into(), serde_json::json!(id));
            }
            if let Some(obj) = frame.as_object_mut() {
                obj.remove("_dir");
            }
            writeln!(stdout, "{}", frame)?;
            stdout.flush()?;
            continue;
        };

        // Checkpoint: park until the driver writes the inbound frame.
        let mut line = String::new();
        if incoming.read_line(&mut line)? == 0 {
            // EOF before the verdict is a failure, not a clean stop: otherwise
            // a driver that never answered could pass.
            if !permission_seen {
                return Err(mismatch(format!(
                    "stdin closed at a {checkpoint:?} checkpoint before the plan verdict"
                )));
            }
            return Ok(());
        }
        let got: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);

        match checkpoint {
            Checkpoint::Initialize => {
                // Shape must be an initialize control request; the id is the
                // driver's own and is recorded for the response correlation.
                if got.get("type").and_then(Value::as_str) != Some("control_request")
                    || got.pointer("/request/subtype").and_then(Value::as_str) != Some("initialize")
                {
                    return Err(mismatch(format!(
                        "initialize checkpoint: expected control_request/initialize, got {got}"
                    )));
                }
                init_request_id = got
                    .get("request_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string);
                if init_request_id.is_none() {
                    return Err(mismatch("initialize request carried no request_id"));
                }
            }
            Checkpoint::Permission => {
                verify_permission(&frame, &got)?;
                // The response must echo the recorded permission request id
                // (the CLI-generated can_use_tool id), proving correlation.
                let recorded_rid = frame
                    .pointer("/response/request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if got.pointer("/response/request_id").and_then(Value::as_str) != Some(recorded_rid)
                {
                    return Err(mismatch(format!(
                        "permission response request_id mismatch: recorded {recorded_rid:?}"
                    )));
                }
                permission_seen = true;
            }
            Checkpoint::UserPrompt | Checkpoint::Other => {
                if strict
                    && (got.get("type") != frame.get("type")
                        || got.get("request_id") != frame.get("request_id"))
                {
                    return Err(mismatch(format!(
                        "strict checkpoint mismatch:\n fixture: {frame}\n host:    {got}"
                    )));
                }
            }
        }
    }
    // A recording that never reached a verdict is itself broken.
    if !permission_seen {
        return Err(mismatch(
            "fixture contains no permission verdict checkpoint",
        ));
    }
    Ok(())
}
