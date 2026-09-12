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
