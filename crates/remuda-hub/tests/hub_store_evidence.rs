//! Reproducible hub-store-1 evidence harness.
//!
//! Ignored by the normal test run (it seeds a 200k-event journal and takes
//! ~30s). Run it on demand and paste the `EVIDENCE` lines into
//! `docs/design/evidence/hub-store-1.md`:
//!
//! ```sh
//! cargo test -p remuda-hub --test hub_store_evidence -- --ignored --nocapture
//! ```
//!
//! Every "before" number routes through the pre-pool code path reconstructed in
//! `store_test_support` (single writer connection / unbounded read); every
//! "after" number uses the shipped reader-pool and windowed methods. Both sides
//! run in one process against one database so the pair is directly comparable.

use remuda_hub::store_test_support::{
    hold_reader, hold_writer, journal_tail_via_writer, open_with_host,
    pending_interactions_via_writer, read_all_journal_parsed,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "run on demand; seeds a 200k-event journal (~30s)"]
async fn hub_store_evidence() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("warn"))
        .with_test_writer()
        .try_init();

    let dir = tempfile::tempdir()?;
    let (store, host_id) = open_with_host(dir.path(), "evidence").await?;
    let instance_id = "ins_evidence".to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await?;

    // 200k events (busy demo shape).
    const TOTAL: i64 = 200_000;
    let mut next = 1;
    while next <= TOTAL {
        let end = (next - 1 + 2048).min(TOTAL);
        let events: Vec<Value> = (next..=end)
            .map(|n| json!({ "kind": "message", "payload": { "text": "x", "n": n } }))
            .collect();
        store
            .append_journal_batch(host_id.clone(), instance_id.clone(), None, events)
            .await?;
        next = end + 1;
    }

    // (1) Screen journal envelope: unbounded old read vs windowed new read.
    let t = Instant::now();
    let before_rows = read_all_journal_parsed(&store, &instance_id).await;
    let before_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let page = store.read_journal(instance_id.clone(), 0, None).await?;
    let after_ms = t.elapsed().as_millis();
    println!(
        "EVIDENCE envelope before={before_ms}ms rows={before_rows} \
         after={after_ms}ms rows={} durable={}",
        page.events.len(),
        page.durable_seq
    );

    // 1k pending interactions via the real projection.
    for n in 0..1000 {
        let iid = format!("int_{n:05}");
        store
            .append_journal_batch(
                host_id.clone(),
                instance_id.clone(),
                None,
                vec![json!({
                    "kind": "interaction.requested",
                    "interactionId": iid,
                    "payload": { "interactionId": iid, "kind": "permission" }
                })],
            )
            .await?;
    }

    // (2) The demo root cause: a long job on the single writer holding the web
    // boot list. Same 1.2s block each time.
    let blocker = {
        let store = store.clone();
        tokio::spawn(async move { hold_writer(&store, Duration::from_millis(1200)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    let blocked_rows = pending_interactions_via_writer(&store).await;
    let blocked_ms = t.elapsed().as_millis();
    blocker.await?;
    let blocker = {
        let store = store.clone();
        tokio::spawn(async move { hold_writer(&store, Duration::from_millis(1200)).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    let pool_rows = store.list_interactions(None, None, None, true).await?.len();
    let pool_ms = t.elapsed().as_millis();
    blocker.await?;
    println!(
        "EVIDENCE interactions vs blocked-writer before={blocked_ms}ms rows={blocked_rows} \
         after={pool_ms}ms rows={pool_rows}"
    );

    // (3) Observe fan-out under a fresh ingest burst: 20 tail reads before
    // (writer queue) vs after (reader pool).
    let mut handles = Vec::new();
    for h in 0..3 {
        let store = store.clone();
        let host = format!("hst_ingest_{h}");
        handles.push(tokio::spawn(async move {
            store
                .ensure_instance(host.clone(), format!("ins_{h}"))
                .await
                .unwrap();
            for b in 0..40 {
                let events: Vec<Value> = (0..64)
                    .map(|n| json!({ "kind": "message", "payload": { "n": b * 64 + n } }))
                    .collect();
                store
                    .append_journal_batch(host.clone(), format!("ins_{h}"), None, events)
                    .await
                    .unwrap();
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let t = Instant::now();
    let mut before_seen = 0;
    for _ in 0..20 {
        before_seen += journal_tail_via_writer(&store, &instance_id, 256).await;
    }
    let before_fan_ms = t.elapsed().as_millis();
    let t = Instant::now();
    let mut seen = 0;
    for _ in 0..20 {
        seen += store
            .read_journal_tail(instance_id.clone(), 256)
            .await?
            .len();
    }
    let fan_ms = t.elapsed().as_millis();
    for handle in handles {
        handle.await?;
    }
    println!(
        "EVIDENCE observe fanout 20x tail256 under ingest before={before_fan_ms}ms \
         rows={before_seen} after={fan_ms}ms rows={seen}"
    );

    // (4) A 1.5s read crosses the 1s slow-job budget and names itself.
    hold_reader(&store, Duration::from_millis(1500)).await;
    println!("EVIDENCE warn emitted above for a 1500ms read");

    Ok(())
}
