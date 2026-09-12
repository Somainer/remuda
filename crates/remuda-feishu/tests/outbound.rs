//! Outbound DryRun argv contract and live execution against the local test double.

use remuda_feishu::{LarkCli, OutboundBody, idempotency_key, render_progress_card};
use std::path::PathBuf;

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("fake-lark-cli.sh")
}

#[tokio::test]
async fn dry_run_records_idempotency_key_and_does_not_execute() {
    let mut cli = LarkCli::dry_run().with_profile("remuda-bot");
    let receipt = cli
        .send_text("oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa", "hello", "om_seed_1")
        .await
        .unwrap();
    assert!(!receipt.executed);
    assert_eq!(receipt.message_id.as_deref(), Some("om_dry_run"));
    let argv = &receipt.planned.argv;
    assert!(argv.contains(&"+messages-send".into()));
    assert!(argv.contains(&"--chat-id".into()));
    assert!(argv.contains(&"--as".into()));
    assert!(argv.contains(&"bot".into()));
    assert!(argv.contains(&"--idempotency-key".into()));
    assert!(argv.contains(&"--profile".into()));
    assert!(argv.contains(&"remuda-bot".into()));
    let key = idempotency_key("om_seed_1");
    assert!(argv.contains(&key));
    assert!(key.len() <= 50);
}

#[tokio::test]
async fn reply_in_thread_flag() {
    let mut cli = LarkCli::dry_run();
    let receipt = cli
        .reply_in_thread(
            "om_parent",
            OutboundBody::Text("continues the topic".into()),
            "reply-seed",
        )
        .await
        .unwrap();
    let argv = &receipt.planned.argv;
    assert!(argv.contains(&"+messages-reply".into()));
    assert!(argv.contains(&"--reply-in-thread".into()));
    assert!(argv.contains(&"--message-id".into()));
}

#[tokio::test]
async fn send_card_uses_interactive_content_without_envelope() {
    let card = render_progress_card("Working", "Read", "opening README", 3).unwrap();
    let mut cli = LarkCli::dry_run();
    let receipt = cli
        .send_card("oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa", &card, "card-seed")
        .await
        .unwrap();
    let argv = &receipt.planned.argv;
    assert!(argv.contains(&"--msg-type".into()));
    assert!(argv.contains(&"interactive".into()));
    let content = argv
        .windows(2)
        .find(|w| w[0] == "--content")
        .map(|w| w[1].as_str())
        .unwrap();
    assert!(!content.contains("msg_type"));
    assert!(content.contains("\"schema\":\"2.0\"") || content.contains("\"schema\": \"2.0\""));
}

#[tokio::test]
async fn file_path_must_be_relative() {
    let mut cli = LarkCli::dry_run();
    let err = cli
        .send_file("oc_x", std::path::Path::new("/tmp/out.md"), "f")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cwd-relative"));
}

#[tokio::test]
async fn live_mode_runs_test_double() {
    let mut cli = LarkCli::live(fake_cli());
    let receipt = cli
        .send_text("oc_p2p_aaaaaaaaaaaaaaaaaaaaaaaaaaaa", "hello", "live-seed")
        .await
        .unwrap();
    assert!(receipt.executed);
    assert_eq!(receipt.message_id.as_deref(), Some("om_fake_outbound"));
}

// ---- F14: lark-cli stderr is scrubbed before it reaches Error::Cli (and the log) ----

/// `Error::Cli` is rendered by Debug/Display and logged by `consume.rs` and
/// `cmd/dispatcher.rs`. A `tenant_access_token` printed on a lark-cli failure
/// path must not survive into the log line.
#[test]
fn cli_stderr_redacts_named_credentials() {
    let raw = concat!(
        "error: refresh failed\n",
        "tenant_access_token=t-fake-G1044qeGEDNWCOKCVJHJIEUFSJ2AAZ5\n",
        "app_secret: fake-N4kNWiVFx1PkjQ6bTuTMIeCmV1UbZmQP\n",
        "Authorization: Bearer u-fake-7f8aB9cD0eF1gH2iJ3kL4mN5oP6qR7\n",
    );
    let out = remuda_feishu::redact_cli_stderr(raw);
    assert!(out.contains("refresh failed"), "context is kept: {out}");
    for secret in [
        "t-fake-G1044qeGEDNWCOKCVJHJIEUFSJ2AAZ5",
        "fake-N4kNWiVFx1PkjQ6bTuTMIeCmV1UbZmQP",
        "u-fake-7f8aB9cD0eF1gH2iJ3kL4mN5oP6qR7",
    ] {
        assert!(!out.contains(secret), "leaked {secret} in: {out}");
    }
    assert!(out.contains("[redacted]"), "{out}");
}

/// A credential printed with no name at all is still caught by its shape.
#[test]
fn cli_stderr_redacts_bare_token_blobs() {
    let raw = "unexpected response t-fake-G1044qeGEDNWCOKCVJHJIEUFSJ2AAZ5 from api";
    let out = remuda_feishu::redact_cli_stderr(raw);
    assert!(
        !out.contains("t-fake-G1044qeGEDNWCOKCVJHJIEUFSJ2AAZ5"),
        "{out}"
    );
    assert!(out.contains("unexpected response"), "{out}");
    assert!(out.contains("from api"), "{out}");
}

/// Ordinary diagnostics survive — redaction must not make failures unreadable.
#[test]
fn cli_stderr_keeps_ordinary_diagnostics() {
    let raw = "Error: chat oc_abc not found (code 230002)";
    let out = remuda_feishu::redact_cli_stderr(raw);
    assert!(out.contains("oc_abc"), "{out}");
    assert!(out.contains("230002"), "{out}");
}

/// Unbounded stderr must not flood the log.
#[test]
fn cli_stderr_is_truncated() {
    let raw = "spam line here\n".repeat(10_000);
    let out = remuda_feishu::redact_cli_stderr(&raw);
    assert!(
        out.len() <= remuda_feishu::MAX_CLI_STDERR_BYTES + 32,
        "{} bytes",
        out.len()
    );
}
