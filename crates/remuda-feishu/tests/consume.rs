//! Consume supervisor: ready marker, stdin keep-alive, SIGTERM, backoff restart.

use remuda_feishu::{Backoff, ConsumeEvent, ConsumeSettings, ConsumeSupervisor};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

fn fake_cli() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("fake-lark-cli.sh")
}

fn workdir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("remuda-feishu-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn settings(extra: &[(&str, String)]) -> ConsumeSettings {
    let mut settings = ConsumeSettings::new(fake_cli());
    settings.event_keys = vec!["im.message.receive_v1".into()];
    settings.backoff = Backoff {
        initial: Duration::from_millis(50),
        max: Duration::from_millis(200),
    };
    settings.extra_env = extra
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect();
    settings
}

async fn recv_until<F>(rx: &mut tokio::sync::mpsc::Receiver<ConsumeEvent>, pred: F) -> ConsumeEvent
where
    F: Fn(&ConsumeEvent) -> bool,
{
    timeout(Duration::from_secs(5), async {
        loop {
            let ev = rx.recv().await.expect("supervisor closed");
            if pred(&ev) {
                return ev;
            }
        }
    })
    .await
    .expect("timeout waiting for consume event")
}

#[tokio::test]
async fn ready_emits_fixture_event_and_sigterm_does_not_close_stdin_first() {
    let dir = workdir();
    let events = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join("im-message-p2p.jsonl");
    let signal = dir.join("signal");
    let mut cfg = settings(&[
        ("FAKE_LARK_EVENTS_FILE", events.display().to_string()),
        ("FAKE_LARK_SIGNAL_FILE", signal.display().to_string()),
    ]);
    cfg.event_keys = vec!["im.message.receive_v1".into()];
    let (sup, mut rx) = ConsumeSupervisor::start(cfg);
    recv_until(&mut rx, |e| matches!(e, ConsumeEvent::Ready { .. })).await;
    recv_until(&mut rx, |e| matches!(e, ConsumeEvent::Event { .. })).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!signal.exists() || std::fs::read_to_string(&signal).unwrap() != "eof");
    sup.shutdown().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    if signal.exists() {
        let why = std::fs::read_to_string(&signal).unwrap();
        assert_eq!(why.trim(), "term");
    }
}

#[tokio::test]
async fn backoff_restarts_after_failure() {
    let dir = workdir();
    let counter = dir.join("counter");
    let mut cfg = settings(&[
        ("FAKE_LARK_COUNTER", counter.display().to_string()),
        ("FAKE_LARK_FAIL_TIMES", "2".into()),
    ]);
    cfg.event_keys = vec!["im.message.receive_v1".into()];
    let (sup, mut rx) = ConsumeSupervisor::start(cfg);
    recv_until(&mut rx, |e| {
        matches!(e, ConsumeEvent::Restarting { attempt: 1, .. })
    })
    .await;
    recv_until(&mut rx, |e| {
        matches!(e, ConsumeEvent::Restarting { attempt: 2, .. })
    })
    .await;
    recv_until(&mut rx, |e| matches!(e, ConsumeEvent::Ready { .. })).await;
    sup.shutdown().await;
    let n: u32 = std::fs::read_to_string(counter)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(n >= 3);
}
