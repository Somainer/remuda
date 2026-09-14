//! Latency from a `MessageDisplay` hook delta to a readable journal message.
//!
//! D-028 P3's evidence requirement is «first text visible < 300 ms after the
//! hook delta». This measures the part Remuda owns — relay payload in, journal
//! message out — on the recorded live payloads, so the number is reproducible
//! rather than a one-off stopwatch reading.
//!
//! Run with `cargo test -p remuda-node --test hook_latency -- --nocapture` to
//! print the measurement.

use remuda_node::MessageAssembler;
use remuda_protocol::{ObservationPayload, SourceChannel};
use remuda_signal::{BusContext, HookEnvelope, SignalBus};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;
use tokio::sync::mpsc;

/// The budget from D-028 P3. Generous on purpose: the point is to catch a
/// regression that makes the path slow, not to benchmark the machine.
const BUDGET_MS: u128 = 300;

#[tokio::test]
async fn a_hook_delta_becomes_readable_text_well_inside_the_budget() {
    let (tx, mut rx) = mpsc::channel(64);
    let bus = SignalBus::new(
        BusContext {
            instance_id: remuda_protocol::InstanceId::new(),
            host_id: remuda_protocol::HostId::new(),
            journal_id: remuda_protocol::Id::new("obj").unwrap(),
            run_id: remuda_protocol::RunId::new(),
            driver_kind: remuda_protocol::DriverKind::ShellPty,
            adapter_version: "test".into(),
        },
        tx,
        Arc::new(AtomicU64::new(0)),
    );

    let mut assembler = MessageAssembler::new();
    let mut first_text: Option<(u128, String)> = None;
    for line in remuda_testing::hook_message_stream_fixture().lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if value["event"] != "MessageDisplay" {
            continue;
        }
        // The clock starts where the relay hands the payload over, which is the
        // first moment Remuda could possibly know about the text.
        let started = Instant::now();
        bus.handle(HookEnvelope {
            credential: "fixture".into(),
            event: "MessageDisplay".into(),
            ppid: 4242,
            payload: value["payload"].clone(),
        })
        .await;
        let observation = rx.try_recv().expect("the hook reached the journal");
        assert_eq!(observation.source.channel, SourceChannel::Hook);
        let Some(delta) = remuda_node::message_delta(&observation) else {
            continue;
        };
        let Some(payload) = assembler.fold(&delta) else {
            continue;
        };
        let elapsed = started.elapsed().as_millis();
        let text = payload
            .blocks
            .iter()
            .filter_map(|block| match block {
                remuda_protocol::ContentBlock::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect::<String>();
        let message = MessageAssembler::observation(&observation, payload);
        assert!(matches!(message.body, ObservationPayload::Message(_)));
        if first_text.is_none() {
            first_text = Some((elapsed, text));
        }
    }

    let (elapsed, text) = first_text.expect("the recording contains a delta");
    println!("first text after hook delta: {elapsed} ms — {text:?}");
    assert!(
        elapsed < BUDGET_MS,
        "first text took {elapsed} ms, over the {BUDGET_MS} ms budget"
    );
    assert!(
        !text.trim().is_empty(),
        "the measured message must actually carry text"
    );
}
