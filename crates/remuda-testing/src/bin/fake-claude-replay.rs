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
    match run() {
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

/// Deep-compare the COMPLETE permission-response frame the driver sent with
/// the recorded one. The expected frame has only its curation-only `_dir`
/// marker removed; everything else must be byte-for-byte equal as JSON:
/// the root `type`, the EXACT root key set (no added fields), the `response`
/// envelope (`subtype`, `request_id`, inner payload key set), exact
/// `updatedInput`, and the absence of extras such as `updatedPermissions`.
/// A changed input, an added permission, a wrong subtype, or an extra ROOT
/// field is a mismatch.
fn verify_permission(expected: &Value, got: &Value) -> Result<(), io::Error> {
    let mut expected = expected.clone();
    if let Some(obj) = expected.as_object_mut() {
        obj.remove("_dir");
    }
    let expected_keys = root_key_set(&expected);
    let got_keys = root_key_set(got);
    if expected_keys != got_keys {
        return Err(mismatch(format!(
            "permission root key set mismatch:\n recorded: {expected_keys:?}\n host: {got_keys:?}"
        )));
    }
    if got != &expected {
        return Err(mismatch(format!(
            "permission frame mismatch:\n recorded: {expected}\n host sent: {got}"
        )));
    }
    Ok(())
}

/// Sorted top-level keys of a JSON value (None unless it is an object).
fn root_key_set(value: &Value) -> Option<Vec<String>> {
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
