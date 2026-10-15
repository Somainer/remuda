//! hub-store-1: long reads must not queue in the Hub's single SQLite writer.
//!
//! Three guarantees:
//! 1. A read sleeping on a reader-pool connection delays neither a write nor
//!    an unrelated short read by more than 500 ms.
//! 2. The journal endpoint over a 200k-event journal answers within 2 s,
//!    returns the bounded tail window (row cap and byte cap), and the last
//!    event is the durable tail.
//! 3. A 256-event ingest is folded into bounded writer jobs the writer yields
//!    between, so a concurrent point read lands within 200 ms and every event
//!    is durable with contiguous seq.

use anyhow::{Context, Result};
use remuda_hub::store_test_support::{
    APPEND_CHUNK_MAX, JOURNAL_WINDOW_BYTES, JOURNAL_WINDOW_ROWS, hold_reader, open_with_host,
};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Bounds taken from the task statement; kept local so a deliberate change in
/// `store.rs` forces a deliberate change here.
const READ_HEADROOM: Duration = Duration::from_millis(500);
const POINT_READ_BUDGET: Duration = Duration::from_millis(200);
const JOURNAL_HTTP_BUDGET: Duration = Duration::from_millis(2_000);

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect(addr).await?;
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, rest.to_string()))
}

fn cookie_from(head: &str) -> Option<String> {
    for line in head.lines() {
        if line.to_ascii_lowercase().starts_with("set-cookie:") {
            let value = line.split_once(':')?.1.trim();
            return Some(value.split(';').next()?.trim().to_string());
        }
    }
    None
}

async fn login(addr: std::net::SocketAddr, bootstrap: &str) -> Result<String> {
    let body = json!({
        "bootstrapToken": bootstrap,
        "deviceName": "hub-store-phone"
    })
    .to_string();
    let mut stream = TcpStream::connect(addr).await?;
    let req = format!(
        "POST /v1/login HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    let head = text.split_once("\r\n\r\n").map_or(&*text, |(h, _)| h);
    cookie_from(head).context("login set-cookie")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn long_reader_does_not_block_writer_or_unrelated_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, _host) = open_with_host(dir.path(), "reader-isolation").await?;
    store
        .insert_device("seed".into(), "hash-seed".into(), "devseed".into())
        .await?;

    // Occupy one reader-pool connection long enough that everything below is
    // guaranteed to be scheduled while it is still held.
    let slow = {
        let store = store.clone();
        tokio::spawn(async move { hold_reader(&store, Duration::from_millis(1_500)).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;

    // A write goes to the writer thread's own connection: it must not queue
    // behind the sleeping read.
    let write_started = Instant::now();
    store
        .insert_device(
            "parallel-write".into(),
            "hash-write".into(),
            "devwrit".into(),
        )
        .await?;
    assert!(
        write_started.elapsed() < READ_HEADROOM,
        "write waited {} ms behind a long read",
        write_started.elapsed().as_millis()
    );

    // An unrelated read lands on another pool connection, not the held one.
    let read_started = Instant::now();
    let devices = store.list_devices().await?;
    assert!(
        read_started.elapsed() < READ_HEADROOM,
        "short read waited {} ms behind a long read",
        read_started.elapsed().as_millis()
    );
    assert_eq!(devices.len(), 2);

    slow.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_endpoint_windows_a_200k_event_journal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir.clone());
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let addr = hub.addr;
    let cookie = login(addr, &bootstrap).await?;
    let store = hub.store().expect("hub has a store");

    let host_id = "hst_window200k".to_string();
    let instance_id = "ins_window200k".to_string();
    hub.test_insert_host(&host_id).await?;
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await?;

    // Synthetic 200k-event journal. Chunked transactions bound each writer
    // job; the chunk size here is a seeding detail, not the production cap.
    const TOTAL: i64 = 200_000;
    const SEED_CHUNK: usize = 2_048;
    let seeded = Instant::now();
    let mut start = Some(1_i64);
    let mut next: i64 = 1;
    while next <= TOTAL {
        let end = (next - 1 + SEED_CHUNK as i64).min(TOTAL);
        let events: Vec<Value> = (next..=end)
            .map(|n| json!({ "kind": "message", "payload": { "text": "x", "n": n } }))
            .collect();
        store
            .append_journal_batch(host_id.clone(), instance_id.clone(), start, events)
            .await?;
        start = None;
        next = end + 1;
    }
    eprintln!("seeded {TOTAL} events in {:?}", seeded.elapsed());

    let path = format!("/v1/instances/{instance_id}/journal");
    let started = Instant::now();
    let (status, body) = http(addr, "GET", &path, &[("Cookie", cookie.as_str())]).await?;
    let elapsed = started.elapsed();
    assert_eq!(status, 200, "{body}");
    assert!(
        elapsed < JOURNAL_HTTP_BUDGET,
        "journal GET took {elapsed:?} over a {TOTAL}-event journal"
    );

    let journal: Value = serde_json::from_str(body.trim())?;
    assert_eq!(journal["instanceId"], json!(instance_id));
    assert_eq!(journal["durableSeq"], json!(TOTAL.to_string()));
    let events = journal["events"].as_array().context("events array")?;
    assert_eq!(
        events.len() as i64,
        JOURNAL_WINDOW_ROWS,
        "row cap must bound the window, not the 200k journal"
    );
    // The window is the tail nearest durableSeq. The events are serialized
    // JournalRecords: top-level `seq` is an i64.
    assert_eq!(events[0]["seq"], json!(TOTAL - JOURNAL_WINDOW_ROWS + 1));
    assert_eq!(events[events.len() - 1]["seq"], json!(TOTAL));
    for (i, event) in events.iter().enumerate() {
        let want = TOTAL - JOURNAL_WINDOW_ROWS + 1 + i as i64;
        assert_eq!(event["seq"], json!(want), "gap at offset {i}");
    }

    // Byte cap: large rows trip it long before the row cap.
    let big_instance = "ins_window_bytes".to_string();
    store
        .ensure_instance(host_id.clone(), big_instance.clone())
        .await?;
    const BIG_ROWS: i64 = 200;
    let big: Vec<Value> = (1..=BIG_ROWS)
        .map(|n| json!({ "kind": "message", "payload": { "text": "q".repeat(64_000), "n": n } }))
        .collect();
    store
        .append_journal_batch(host_id, big_instance.clone(), Some(1), big)
        .await?;
    let page = store.read_journal(big_instance.clone(), 0, None).await?;
    let events = page.events;
    let durable = page.durable_seq;
    assert_eq!(durable, BIG_ROWS);
    assert!(
        (events.len() as i64) < BIG_ROWS,
        "byte cap must trip before the 200 rows (got {})",
        events.len()
    );
    assert!(
        (events.len() as i64) < JOURNAL_WINDOW_ROWS,
        "byte cap must trip before the row cap"
    );
    let byte_sizes: Vec<usize> = events
        .iter()
        .map(|record| serde_json::to_vec(&record.event).map_or(0, |bytes| bytes.len()))
        .collect();
    let total_bytes: usize = byte_sizes.iter().sum();
    assert!(
        total_bytes <= JOURNAL_WINDOW_BYTES + *byte_sizes.iter().max().unwrap_or(&0),
        "window carried {total_bytes} bytes"
    );
    // Tail anchored, contiguous.
    assert_eq!(events.last().unwrap().seq, BIG_ROWS);
    for (i, record) in events.iter().enumerate() {
        let want = BIG_ROWS - events.len() as i64 + 1 + i as i64;
        assert_eq!(record.seq, want, "gap at offset {i}");
    }

    hub.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_ingest_yields_to_a_concurrent_point_read() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, host_id) = open_with_host(dir.path(), "ingest-bound").await?;
    let instance_id = "ins_ingest_bound".to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await?;

    // A 256-event frame, the Node's REPLAY_PAGE, folded into 64-event chunks
    // exactly as the ws handler folds it.
    const BATCH: i64 = 256;
    let events: Vec<Value> = (1..=BATCH)
        .map(|n| json!({ "kind": "message", "payload": { "text": "ingest", "n": n } }))
        .collect();
    let writer = {
        let store = store.clone();
        let host_id = host_id.clone();
        let instance_id = instance_id.clone();
        tokio::spawn(async move {
            let mut first = Some(1_i64);
            for chunk in events.chunks(APPEND_CHUNK_MAX) {
                store
                    .append_journal_batch(
                        host_id.clone(),
                        instance_id.clone(),
                        first,
                        chunk.to_vec(),
                    )
                    .await
                    .expect("append batch");
                first = None;
            }
        })
    };
    // Let the first chunk land in the writer queue, then queue a point read:
    // it may wait for at most the one chunk already running.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let probe = {
        let store = store.clone();
        let instance_id = instance_id.clone();
        tokio::spawn(async move { store.get_instance(instance_id).await })
    };
    let started = Instant::now();
    let record = probe
        .await?
        .expect("point read")
        .context("instance exists")?;
    assert!(
        started.elapsed() < POINT_READ_BUDGET,
        "point read waited {:?} while ingest landed",
        started.elapsed()
    );
    assert_eq!(record.instance_id, instance_id);
    writer.await?;

    // Every one of the 256 events is durable, contiguous from seq 1.
    let page = store.read_journal(instance_id, 0, None).await?;
    let durable_events = page.events;
    let durable = page.durable_seq;
    assert_eq!(durable, BATCH);
    assert_eq!(durable_events.len() as i64, BATCH);
    assert_eq!(page.from_seq, Some(1), "full journal reaches seq 1");
    assert!(page.reached_after_seq);
    for (i, event) in durable_events.iter().enumerate() {
        assert_eq!(event.seq, i as i64 + 1, "seq gap at {}", i);
    }
    Ok(())
}

/// Seed `total` message events for one instance via bounded batches.
async fn seed_events(
    store: &remuda_hub::store_test_support::Store,
    host_id: &str,
    instance_id: &str,
    total: i64,
) -> Result<()> {
    let mut next = 1;
    while next <= total {
        let end = (next - 1 + 2_048).min(total);
        let events: Vec<Value> = (next..=end)
            .map(|n| json!({ "kind": "message", "payload": { "text": "x", "n": n } }))
            .collect();
        store
            .append_journal_batch(host_id.into(), instance_id.into(), None, events)
            .await?;
        next = end + 1;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn window_flags_and_before_seq_descend_to_full_history() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, host_id) = open_with_host(dir.path(), "window-flags").await?;
    let instance_id = "ins_window_flags".to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await?;
    const TOTAL: i64 = 5_000;
    seed_events(&store, &host_id, &instance_id, TOTAL).await?;

    // Default tail window: floor reveals the cut, the flag says it is partial.
    let tail = store.read_journal(instance_id.clone(), 0, None).await?;
    assert_eq!(tail.durable_seq, TOTAL);
    assert_eq!(tail.events.len() as i64, JOURNAL_WINDOW_ROWS);
    assert_eq!(tail.from_seq, Some(TOTAL - JOURNAL_WINDOW_ROWS + 1));
    assert!(
        !tail.reached_after_seq,
        "tail over a longer journal is a window"
    );
    assert_eq!(tail.events.last().unwrap().seq, TOTAL);

    // A cursor inside the window reaches its cursor.
    let in_window = store.read_journal(instance_id.clone(), 4_000, None).await?;
    assert_eq!(in_window.events.len() as i64, 1_000);
    assert_eq!(in_window.from_seq, Some(4_001));
    assert!(in_window.reached_after_seq);
    assert_eq!(in_window.events.last().unwrap().seq, TOTAL);

    // A cursor at the tail returns an empty, complete page.
    let caught_up = store.read_journal(instance_id.clone(), TOTAL, None).await?;
    assert!(caught_up.events.is_empty());
    assert_eq!(caught_up.from_seq, None);
    assert!(caught_up.reached_after_seq);

    // Descend with beforeSeq from the tail floor until the whole 1..TOTAL range
    // is covered with no gaps and no overlaps — catchup after a long
    // disconnect must converge rather than re-fetch the same tail forever.
    let mut covered: Vec<(i64, i64)> = Vec::new();
    let mut before = Some(tail.from_seq.unwrap() - 1);
    let mut guard = 0;
    covered.push((
        tail.events.first().unwrap().seq,
        tail.events.last().unwrap().seq,
    ));
    while let Some(high) = before {
        let page = store
            .read_journal(instance_id.clone(), 0, Some(high))
            .await?;
        assert!(
            !page.events.is_empty(),
            "beforeSeq={high} must page older rows"
        );
        covered.push((
            page.events.first().unwrap().seq,
            page.events.last().unwrap().seq,
        ));
        before = if page.reached_after_seq {
            None
        } else {
            Some(page.from_seq.unwrap() - 1)
        };
        guard += 1;
        assert!(guard < 100, "beforeSeq descent did not converge");
    }
    covered.sort();
    assert_eq!(covered.first().unwrap().0, 1);
    assert_eq!(covered.last().unwrap().1, TOTAL);
    for pair in covered.windows(2) {
        assert_eq!(pair[0].1 + 1, pair[1].0, "gap or overlap in paged history");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_window_and_durable_seq_share_one_snapshot() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (store, host_id) = open_with_host(dir.path(), "snapshot").await?;
    let instance_id = "ins_snapshot".to_string();
    store
        .ensure_instance(host_id.clone(), instance_id.clone())
        .await?;
    seed_events(&store, &host_id, &instance_id, 1_000).await?;

    // Hammer appends on the writer while reading windows from the pool. Every
    // returned event must be <= the durable seq reported beside it: with two
    // independent autocommit reads an append could land between them and leak a
    // newer event past that watermark; the shared read transaction forbids it.
    let writer = {
        let store = store.clone();
        let host_id = host_id.clone();
        let instance_id = instance_id.clone();
        tokio::spawn(async move {
            for batch in 0..40 {
                let events: Vec<Value> = (0..64)
                    .map(|n| json!({ "kind": "message", "payload": { "n": batch * 64 + n } }))
                    .collect();
                if store
                    .append_journal_batch(host_id.clone(), instance_id.clone(), None, events)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    };
    for _ in 0..400 {
        let page = store.read_journal(instance_id.clone(), 0, None).await?;
        if let Some(last) = page.events.last() {
            assert!(
                last.seq <= page.durable_seq,
                "event {} past durable {} returned beside it",
                last.seq,
                page.durable_seq
            );
        }
        if let Some(first) = page.from_seq {
            assert!(first <= page.durable_seq);
        }
    }
    writer.await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn journal_http_response_exposes_window_metadata() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let data_dir = dir.path().join("data");
    let config = HubConfig::for_test(data_dir);
    let bootstrap = config.bootstrap_token.clone();
    let hub = spawn(config).await?;
    let cookie = login(hub.addr, &bootstrap).await?;
    let store = hub.store().expect("store");
    let host_id = "hst_windowmeta".to_string();
    let instance_id = "ins_windowmeta".to_string();
    hub.test_insert_host(&host_id).await?;
    store.ensure_instance(host_id, instance_id.clone()).await?;
    seed_events(store, "hst_windowmeta", &instance_id, 5_000).await?;

    let base = format!("/v1/instances/{instance_id}/journal");
    let (status, body) = http(hub.addr, "GET", &base, &[("Cookie", &cookie)]).await?;
    assert_eq!(status, 200, "{body}");
    let tail: Value = serde_json::from_str(body.trim())?;
    assert_eq!(
        tail["fromSeq"],
        json!((5_000 - JOURNAL_WINDOW_ROWS + 1).to_string())
    );
    assert_eq!(tail["reachedAfterSeq"], json!(false));

    let (status, body) = http(
        hub.addr,
        "GET",
        &format!("{base}?afterSeq=0&beforeSeq=3000"),
        &[("Cookie", &cookie)],
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let older: Value = serde_json::from_str(body.trim())?;
    assert_eq!(older["fromSeq"], json!("1001"));
    assert_eq!(older["reachedAfterSeq"], json!(false));
    assert_eq!(older["events"].as_array().map(Vec::len), Some(2000));

    let (status, body) = http(
        hub.addr,
        "GET",
        &format!("{base}?afterSeq=4000"),
        &[("Cookie", &cookie)],
    )
    .await?;
    assert_eq!(status, 200, "{body}");
    let reached: Value = serde_json::from_str(body.trim())?;
    assert_eq!(reached["fromSeq"], json!("4001"));
    assert_eq!(reached["reachedAfterSeq"], json!(true));

    hub.shutdown().await;
    Ok(())
}
