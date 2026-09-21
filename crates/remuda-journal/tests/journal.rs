//! Store, tail, and fold-consistency tests for `remuda-journal`.

use anyhow::Result;
use futures::StreamExt;
use remuda_journal::{
    ClaudeJsonlTailer, Envelope, FileTail, FsyncPolicy, Journal, JournalOptions, MapContext,
    NativeIds, OrderedFold, Projections, Source, WorkflowJournalTailer, fold_all,
    fold_prepend_backfill, map_claude_line,
};
use remuda_protocol::{
    ActorRef, ActorType, CommandId, Completeness, DeliveryState, EventId, FileCursor, HostId, Id,
    InstanceId, Interaction, InteractionAnsweredPayload, InteractionExpiredPayload,
    InteractionExpiredReason, InteractionRequestedPayload, ObservationPayload, SourceChannel, U64,
};
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

fn ctx(session: &str) -> (HostId, InstanceId, Id, MapContext) {
    let host = HostId::new();
    let instance = InstanceId::new();
    let journal_id = Id::new("obj").expect("obj");
    let map = MapContext::claude_file(
        instance.clone(),
        journal_id.clone(),
        host.clone(),
        session,
        SourceChannel::Transcript,
    );
    (host, instance, journal_id, map)
}

fn map_file(contents: &str, map: &MapContext) -> Result<Vec<Envelope>> {
    let mut ids = NativeIds::new(map.instance_id.as_id().as_str());
    let mut out = Vec::new();
    let mut offset = 0u64;
    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let cursor = FileCursor {
            file_identity: Id::new("obj")?,
            file_generation: U64(1),
            offset: U64(offset),
            length: U64(line.len() as u64),
            digest: remuda_journal::digest_of(line.as_bytes()),
        };
        offset += line.len() as u64 + 1;
        out.extend(map_claude_line(map, &mut ids, line.as_bytes(), &cursor)?);
    }
    Ok(out)
}

fn open_journal(dir: &Path) -> Result<Journal> {
    Ok(Journal::open_with(
        dir,
        JournalOptions {
            fsync: FsyncPolicy::Never,
            ..JournalOptions::default()
        },
    )?)
}

#[tokio::test]
async fn append_read_snapshot_and_recover() -> Result<()> {
    let tmp = TempDir::new()?;
    let (host, instance, journal_id, map) = ctx("sess-1");
    let _ = host;
    let _ = journal_id;
    let envelopes = map_file(include_str!("fixtures/claude-transcript-ok.jsonl"), &map)?;
    assert!(envelopes.len() >= 4);
    let journal = open_journal(tmp.path())?;
    let mut seqs = Vec::new();
    for envelope in envelopes {
        seqs.push(journal.append(&instance, envelope).await?);
    }
    assert_eq!(seqs.first().map(|s| s.0), Some(1));
    assert_eq!(seqs.last().map(|s| s.0), Some(seqs.len() as u64));
    let items = journal.read_range(&instance, U64(1), None).await?;
    assert_eq!(items.len(), seqs.len());
    assert_eq!(items[0].seq.0, 1);
    let snap = journal.snapshot(&instance).await?;
    assert_eq!(snap.as_of_seq.0, seqs.len() as u64);
    assert!(
        snap.projections
            .transcript
            .entries
            .iter()
            .any(|e| matches!(e, remuda_journal::TranscriptEntry::Message { text, .. } if text.contains("OK")))
    );
    assert!(
        snap.projections
            .transcript
            .tools
            .values()
            .any(|p| p.result_seq.is_some())
    );
    drop(journal);

    let journal = open_journal(tmp.path())?;
    let again = journal.read_range(&instance, U64(1), None).await?;
    assert_eq!(again.len(), items.len());
    assert_eq!(again[0].event_id, items[0].event_id);
    Ok(())
}

#[tokio::test]
async fn follow_subscribes_before_backfill() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, instance, _, map) = ctx("sess-follow");
    let journal = open_journal(tmp.path())?;
    let first = map_file(
        r#"{"type":"user","uuid":"u1","timestamp":"2026-09-12T10:00:00.000Z","message":{"role":"user","content":"one"}}"#,
        &map,
    )?
    .remove(0);
    journal.append(&instance, first).await?;
    let mut follow = journal.follow(&instance, U64(1)).await?;
    let seen = follow.next().await.expect("history")?;
    assert_eq!(seen.seq.0, 1);
    let second = map_file(
        r#"{"type":"user","uuid":"u2","timestamp":"2026-09-12T10:00:01.000Z","message":{"role":"user","content":"two"}}"#,
        &map,
    )?
    .remove(0);
    journal.append(&instance, second).await?;
    let live = follow.next().await.expect("live")?;
    assert_eq!(live.seq.0, 2);
    Ok(())
}

#[tokio::test]
async fn fold_consistency_replay_append_prepend() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, instance, _, map) = ctx("sess-fold");
    let envelopes = map_file(include_str!("fixtures/claude-transcript-ok.jsonl"), &map)?;
    let journal = open_journal(tmp.path())?;
    for envelope in envelopes.clone() {
        journal.append(&instance, envelope).await?;
    }
    let items = journal.read_range(&instance, U64(1), None).await?;
    let full = fold_all(&items);
    let mut per_item = OrderedFold::<Projections>::new();
    for obs in &items {
        per_item.push(obs.clone());
    }
    let prepend = fold_prepend_backfill(&items);
    assert_eq!(full, per_item.inner);
    assert_eq!(full, prepend);
    assert_eq!(full, journal.snapshot(&instance).await?.projections);

    let snippet = map_file(include_str!("fixtures/claude-askuser-snippet.jsonl"), &map)?;
    assert!(
        snippet
            .iter()
            .any(|e| e.completeness == Completeness::Partial
                || matches!(e.body, ObservationPayload::Message(_)))
    );
    Ok(())
}

#[tokio::test]
async fn interaction_first_writer_wins() -> Result<()> {
    let tmp = TempDir::new()?;
    let (host, instance, journal_id, map) = ctx("sess-int");
    let mut interaction: Interaction =
        serde_json::from_str(include_str!("fixtures/interaction.json"))?;
    interaction.instance_id = instance.clone();
    interaction.host_id = host.clone();
    let journal = open_journal(tmp.path())?;
    let requested = Envelope {
        journal_id: journal_id.clone(),
        instance_id: instance.clone(),
        run_id: None,
        host_id: host.clone(),
        process_generation: U64(1),
        run_generation: None,
        observed_at: remuda_journal::timestamp_now()?,
        native_at: remuda_protocol::Knowledge::NotApplicable,
        source: map_file(
            r#"{"type":"user","message":{"role":"user","content":"q"}}"#,
            &map,
        )?
        .remove(0)
        .source,
        completeness: Completeness::Structured,
        evidence_event_ids: Vec::new(),
        event_id: None,
        body: ObservationPayload::InteractionRequested(Box::new(InteractionRequestedPayload {
            interaction: interaction.clone(),
        })),
        raw: None,
    };
    let mut requested_again = requested.clone();
    requested_again.body =
        ObservationPayload::InteractionRequested(Box::new(InteractionRequestedPayload {
            interaction: interaction.clone(),
        }));
    journal.append(&instance, requested).await?;
    journal.append(&instance, requested_again).await?;
    let answered = Envelope {
        journal_id: journal_id.clone(),
        instance_id: instance.clone(),
        run_id: None,
        host_id: host.clone(),
        process_generation: U64(1),
        run_generation: None,
        observed_at: remuda_journal::timestamp_now()?,
        native_at: remuda_protocol::Knowledge::NotApplicable,
        source: map_file(
            r#"{"type":"user","message":{"role":"user","content":"a"}}"#,
            &map,
        )?
        .remove(0)
        .source,
        completeness: Completeness::Structured,
        evidence_event_ids: Vec::new(),
        event_id: None,
        body: ObservationPayload::InteractionAnswered(Box::new(InteractionAnsweredPayload {
            interaction_id: interaction.meta.id.clone(),
            request_version: U64(1),
            answer_command_id: CommandId::new(),
            actor: ActorRef {
                principal_id: Id::new("prn")?,
                actor_type: ActorType::Human,
                device_id: None,
                instance_id: None,
            },
            answer_ref: Id::new("obj")?,
            delivery: DeliveryState::Confirmed,
        })),
        raw: None,
    };
    let mut answered_again = answered.clone();
    if let ObservationPayload::InteractionAnswered(payload) = &mut answered_again.body {
        payload.answer_command_id = CommandId::new();
    }
    journal.append(&instance, answered).await?;
    journal.append(&instance, answered_again).await?;
    let expired = Envelope {
        journal_id,
        instance_id: instance.clone(),
        run_id: None,
        host_id: host,
        process_generation: U64(1),
        run_generation: None,
        observed_at: remuda_journal::timestamp_now()?,
        native_at: remuda_protocol::Knowledge::NotApplicable,
        source: map_file(
            r#"{"type":"user","message":{"role":"user","content":"e"}}"#,
            &map,
        )?
        .remove(0)
        .source,
        completeness: Completeness::Structured,
        evidence_event_ids: Vec::new(),
        event_id: None,
        body: ObservationPayload::InteractionExpired(Box::new(InteractionExpiredPayload {
            interaction_id: interaction.meta.id.clone(),
            request_version: U64(1),
            reason: InteractionExpiredReason::Deadline,
            evidence_event_ids: Vec::<EventId>::new(),
        })),
        raw: None,
    };
    journal.append(&instance, expired).await?;
    let snap = journal.snapshot(&instance).await?;
    assert_eq!(snap.projections.interaction.by_key.len(), 1);
    let record = snap.projections.interaction.by_key.values().next().unwrap();
    assert_eq!(
        record.state,
        remuda_protocol::InteractionState::AnswerCommitted
    );
    assert_eq!(record.requested_seq.0, 1);
    assert_eq!(record.answered_seq, Some(U64(3)));
    assert_eq!(record.expired_seq, None);
    Ok(())
}

#[tokio::test]
async fn claude_jsonl_tailer_offset_resume_and_rotation() -> Result<()> {
    let tmp = TempDir::new()?;
    let path = tmp.path().join("session.jsonl");
    fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"a\"}}\n",
    )?;
    let (_, _, _, map) = ctx("11111111-1111-4111-8111-111111111111");
    let mut tailer = ClaudeJsonlTailer::new(&path, map.clone())?;
    let first = tailer.poll()?;
    assert_eq!(first.len(), 1);
    assert!(tailer.tail().offset() > 0);
    let mut file = fs::OpenOptions::new().append(true).open(&path)?;
    writeln!(
        file,
        r#"{{"type":"user","message":{{"role":"user","content":"b"}}}}"#
    )?;
    drop(file);
    let second = tailer.poll()?;
    assert_eq!(second.len(), 1);
    // prefix rewrite / rotation
    fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"z\"}}\n",
    )?;
    let rotated = tailer.poll()?;
    assert_eq!(rotated.len(), 1);
    assert!(tailer.tail().generation() >= 2);

    let mut incomplete = FileTail::new(tmp.path().join("partial.jsonl"))?;
    fs::write(incomplete.path(), "{\"type\":\"user\"")?;
    assert!(incomplete.poll()?.is_empty());
    let mut f = fs::OpenOptions::new()
        .append(true)
        .open(incomplete.path())?;
    writeln!(f, r#","message":{{"role":"user","content":"c"}}}}"#)?;
    drop(f);
    assert_eq!(incomplete.poll()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn workflow_journal_tailer_maps_launched_started_result() -> Result<()> {
    let tmp = TempDir::new()?;
    let wf_dir = tmp
        .path()
        .join("session/subagents/workflows/wf_d20c2ed9-2cd");
    fs::create_dir_all(&wf_dir)?;
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workflow-journal.jsonl"),
        wf_dir.join("journal.jsonl"),
    )?;
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-a199316cfcd1e1850.meta.json"),
        wf_dir.join("agent-a199316cfcd1e1850.meta.json"),
    )?;
    let (_, instance, _, mut map) = ctx("efa36071-18af-490c-acb5-92700a06d969");
    map.channel = SourceChannel::WorkflowJournal;
    let mut tailer = WorkflowJournalTailer::new(tmp.path().join("session"), map);
    let journal = open_journal(&tmp.path().join("data"))?;
    let seqs = tailer.ingest(&journal).await?;
    assert!(seqs.len() >= 4);
    let snap = journal.snapshot(&instance).await?;
    let kinds: Vec<_> = journal
        .read_range(&instance, U64(1), None)
        .await?
        .into_iter()
        .map(|o| o.body.kind())
        .collect();
    assert!(kinds.contains(&remuda_protocol::ObservationKind::WorkflowRun));
    assert!(
        kinds
            .iter()
            .filter(|k| **k == remuda_protocol::ObservationKind::WorkflowMember)
            .count()
            >= 3
    );
    assert_eq!(snap.as_of_seq.0, seqs.len() as u64);
    Ok(())
}

#[tokio::test]
async fn source_trait_object_and_opaque_unknown_types() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, _, _, map) = ctx("sess-src");
    let mut tailer = ClaudeJsonlTailer::new(tmp.path().join("unused.jsonl"), map)?;
    let source: &mut dyn Source = &mut tailer;
    let cursor = FileCursor {
        file_identity: Id::new("obj")?,
        file_generation: U64(1),
        offset: U64(0),
        length: U64(16),
        digest: remuda_journal::digest_of(b"{\"type\":\"nope\"}"),
    };
    let mapped = source.map_line(br#"{"type":"nope"}"#, cursor)?;
    assert_eq!(mapped.len(), 1);
    assert!(matches!(mapped[0].body, ObservationPayload::Opaque(_)));
    assert_eq!(source.name(), "claude-jsonl");
    Ok(())
}

#[test]
fn claude_transcript_effort_records_map_to_effort_observations() -> Result<()> {
    let (_, _, _, map) = ctx("effort-session");
    let lines = [
        serde_json::json!({
            "type":"user","uuid":"u1","sessionId":"effort-session",
            "message":{"role":"user","content":"say ok"}
        }),
        serde_json::json!({
            "type":"assistant","uuid":"a1","sessionId":"effort-session",
            "message":{"id":"m1","role":"assistant","type":"message",
                "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn"},
            "effort":"high","perTurnEffort":null
        }),
        serde_json::json!({
            "type":"user","uuid":"c1","sessionId":"effort-session",
            "message":{"role":"user","content":
                "<command-name>/effort</command-name><command-args>max</command-args>"}
        }),
        serde_json::json!({
            "type":"assistant","uuid":"a2","sessionId":"effort-session",
            "message":{"id":"m2","role":"assistant","type":"message",
                "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn"},
            "effort":"max","perTurnEffort":null
        }),
        serde_json::json!({
            "type":"assistant","uuid":"a3","sessionId":"effort-session",
            "message":{"id":"m3","role":"assistant","type":"message",
                "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn"},
            "effort":"max","perTurnEffort":null
        }),
    ];
    let contents = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let envelopes = map_file(&contents, &map)?;
    let effort: Vec<_> = envelopes
        .iter()
        .filter_map(|env| match &env.body {
            ObservationPayload::Effort(payload) => Some(payload),
            _ => None,
        })
        .collect();
    // high (first sight), max (after the slash), then the second max is deduped.
    assert_eq!(effort.len(), 2, "{effort:?}");
    // The effort edge event id is deterministic: the same (instance, record,
    // level) from the live channel and this tailer converge on one identity.
    let id_a = envelopes
        .iter()
        .find_map(|env| match &env.body {
            ObservationPayload::Effort(payload)
                if payload.effective.name == remuda_protocol::EffortName::Max =>
            {
                Some(env.event_id.clone())
            }
            _ => None,
        })
        .expect("max effort envelope")
        .expect("effort envelope carries a derived event_id");
    let mut ids2 = NativeIds::new(map.instance_id.as_id().as_str());
    let remapped: Vec<Envelope> = {
        let mut out = Vec::new();
        let mut offset = 0u64;
        for line in contents.lines() {
            if line.is_empty() {
                continue;
            }
            let cursor = FileCursor {
                file_identity: Id::new("obj")?,
                file_generation: U64(1),
                offset: U64(offset),
                length: U64(line.len() as u64),
                digest: remuda_journal::digest_of(line.as_bytes()),
            };
            offset += line.len() as u64 + 1;
            out.extend(map_claude_line(&map, &mut ids2, line.as_bytes(), &cursor)?);
        }
        out
    };
    let id_b = remapped
        .iter()
        .find_map(|env| match &env.body {
            ObservationPayload::Effort(payload)
                if payload.effective.name == remuda_protocol::EffortName::Max =>
            {
                Some(env.event_id.clone())
            }
            _ => None,
        })
        .expect("max effort envelope on remap")
        .expect("derived again");
    assert_eq!(id_a, id_b, "effort event id is stable across mappings");

    assert_eq!(effort[0].effective.name, remuda_protocol::EffortName::High);
    assert_eq!(
        effort[0].effective.source,
        remuda_protocol::EffortSource::Unknown
    );
    assert_eq!(effort[1].effective.name, remuda_protocol::EffortName::Max);
    assert_eq!(
        effort[1].effective.source,
        remuda_protocol::EffortSource::Slash
    );
    Ok(())
}

/// Verbatim replay of the real claude 2.1.272 PTY walk captured by
/// `crates/remuda-driver/examples/effort_probe.rs` (evidence effort-sync-2.md).
#[test]
fn real_21272_walk_settles_effort_from_the_stdout_verdict() -> Result<()> {
    let (_, _, _, map) = ctx("effort-session-21272");
    let contents = include_str!("fixtures/effort-21272/effort-walk-21272.jsonl");
    let envelopes = map_file(contents, &map)?;
    let edges: Vec<_> = envelopes
        .iter()
        .filter_map(|env| match &env.body {
            ObservationPayload::Effort(payload) => Some((
                payload.effective.name,
                payload.effective.source,
                payload.effective.ultracode,
            )),
            _ => None,
        })
        .collect();
    // The stdout verdict — not the next assistant turn — emits the xhigh edge,
    // the ultracode edge carries Some(true), and high clears the flag.
    assert!(
        edges.iter().any(
            |(name, _, ultra)| *name == remuda_protocol::EffortName::Xhigh && *ultra == Some(false)
        ),
        "xhigh accepted from stdout: {edges:?}"
    );
    assert!(
        edges.iter().any(
            |(name, _, ultra)| *name == remuda_protocol::EffortName::Xhigh && *ultra == Some(true)
        ),
        "ultracode accepted from stdout: {edges:?}"
    );
    assert!(
        edges.iter().any(
            |(name, _, ultra)| *name == remuda_protocol::EffortName::High && *ultra == Some(false)
        ),
        "high accept clears the flag: {edges:?}"
    );
    // The dismissed-dialog max never becomes an edge.
    assert!(
        !edges
            .iter()
            .any(|(name, _, _)| *name == remuda_protocol::EffortName::Max),
        "Esc-on-dialog max must not settle: {edges:?}"
    );
    Ok(())
}

#[test]
fn real_21272_reject_records_map_to_no_effort_envelopes() -> Result<()> {
    let (_, _, _, map) = ctx("effort-session-21272-reject");
    let contents = include_str!("fixtures/effort-21272/effort-reject-21272.jsonl");
    let envelopes = map_file(contents, &map)?;
    assert!(
        envelopes
            .iter()
            .all(|env| !matches!(env.body, ObservationPayload::Effort(_))),
        "Kept/Invalid verdicts emit no effort edges"
    );
    Ok(())
}

#[test]
fn claude_permission_mode_records_map_to_permission_observations() -> Result<()> {
    let (_, _, _, map) = ctx("perm-session");
    let lines = [
        serde_json::json!({"type":"mode","mode":"normal","sessionId":"perm-session"}),
        serde_json::json!({"type":"permission-mode","permissionMode":"default",
                          "sessionId":"perm-session"}),
        serde_json::json!({"type":"user","uuid":"p1","sessionId":"perm-session",
            "message":{"role":"user","content":
                "<command-name>/plan</command-name><command-message>plan</command-message>"}}),
        serde_json::json!({"type":"permission-mode","permissionMode":"plan",
                          "sessionId":"perm-session"}),
        serde_json::json!({"type":"permission-mode","permissionMode":"auto",
                          "sessionId":"perm-session"}),
        serde_json::json!({"type":"permission-mode","permissionMode":"auto",
                          "sessionId":"perm-session"}),
    ];
    let contents = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let envelopes = map_file(&contents, &map)?;
    let edges: Vec<_> = envelopes
        .iter()
        .filter_map(|env| match &env.body {
            ObservationPayload::Permission(payload) => {
                Some((payload.effective.mode.clone(), payload.effective.source))
            }
            _ => None,
        })
        .collect();
    // default(manual) — the `mode: normal` render record is not an edge —
    // plan after /plan is slash-attributed, then auto; the duplicate auto
    // record is deduped.
    assert_eq!(
        edges,
        vec![
            (
                "manual".to_string(),
                remuda_protocol::PermissionSource::Unknown
            ),
            ("plan".to_string(), remuda_protocol::PermissionSource::Slash),
            (
                "auto".to_string(),
                remuda_protocol::PermissionSource::Unknown
            ),
        ]
    );
    Ok(())
}

#[test]
fn real_21273_permission_walk_replays_every_wheel_mode() -> Result<()> {
    let (_, _, _, map) = ctx("perm-session-21273");
    let contents = include_str!("fixtures/permission-21273/permission-walk-21273.jsonl");
    let envelopes = map_file(contents, &map)?;
    let modes: Vec<_> = envelopes
        .iter()
        .filter_map(|env| match &env.body {
            ObservationPayload::Permission(payload) => Some(payload.effective.mode.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        modes,
        vec!["manual", "plan", "acceptEdits", "auto", "manual"]
    );
    // Every permission envelope carries a deterministic event id.
    assert!(
        envelopes
            .iter()
            .filter(|env| matches!(env.body, ObservationPayload::Permission(_)))
            .all(|env| env.event_id.is_some())
    );
    Ok(())
}

/// `read_page` bounds the work in the reader, not at its callers.
///
/// This is the property the Node's flush depends on: an unbounded range read
/// deserializes every row from `from_seq` to the tail, so a 256-event page off
/// a large journal pays for the whole journal. `Page::bytes_read` is the
/// reader's own accounting, which is what makes the claim checkable without a
/// profiler.
#[tokio::test]
async fn read_page_touches_only_its_limit() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, instance, _, map) = ctx("page-session");
    let journal = open_journal(tmp.path())?;

    // Two events to page over, so the bound is observable at a small size.
    let envelopes = map_file(include_str!("fixtures/claude-transcript-ok.jsonl"), &map)?;
    for envelope in envelopes {
        journal.append(&instance, envelope).await?;
    }
    let durable = journal.durable_seq(&instance).await?;
    assert!(durable.0 >= 4, "fixture has {durable:?} events");

    // One event, from the floor. The byte count is that event's JSONL line,
    // which the page also reports as the only row it touched.
    let page = journal.read_page(&instance, U64(1), None, 1).await?;
    assert_eq!(page.observations.len(), 1);
    assert_eq!(page.last_seq, Some(U64(1)));
    assert_eq!(page.durable_seq, durable);
    let whole = journal.read_page(&instance, U64(1), None, 256).await?;
    assert!(page.bytes_read > 0);
    assert!(
        page.bytes_read < whole.bytes_read,
        "a one-event page ({}) must read less than the whole journal ({})",
        page.bytes_read,
        whole.bytes_read
    );

    // A page from the tail reads one event even though the journal is larger.
    let tail = journal.read_page(&instance, durable, None, 256).await?;
    assert_eq!(tail.observations.len(), 1, "the last event only");
    assert_eq!(tail.last_seq, Some(durable));

    // Past the tail is an empty page, not an error, and reads nothing.
    let past = journal
        .read_page(&instance, U64(durable.0 + 10), None, 256)
        .await?;
    assert!(past.observations.is_empty());
    assert_eq!(past.bytes_read, 0);
    assert_eq!(past.last_seq, None);
    assert_eq!(past.durable_seq, durable);

    // An explicit `to_seq` still bounds the range.
    let bounded = journal
        .read_page(&instance, U64(1), Some(U64(2)), 256)
        .await?;
    assert_eq!(bounded.observations.len(), 2);

    // The unbounded range read is unchanged, so existing callers keep working.
    let all = journal.read_range(&instance, U64(1), None).await?;
    assert_eq!(all.len(), durable.0 as usize);
    Ok(())
}

// --- torn-tail recovery and the single-writer lock ---

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tracing::field::{Field, Visit};
use tracing::span::{Id as SpanId, Record as SpanRecord};
use tracing::{Event, Level, Metadata};

#[derive(Clone, Debug)]
#[allow(dead_code)] // level/target are recorded for failure diagnostics
struct Captured {
    level: Level,
    target: String,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct CaptureVisitor {
    fields: Vec<(String, String)>,
}

impl Visit for CaptureVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .push((field.name().to_owned(), format!("{value:?}")));
    }
}

struct CaptureSubscriber;

/// Global event queue. The torn-tail warning fires on the journal's writer
/// thread, so a thread-local subscriber would never see it; tests filter the
/// queue by their unique instance id and drain their own entries.
static CAPTURED: OnceLock<Mutex<VecDeque<Captured>>> = OnceLock::new();

fn captured() -> &'static Mutex<VecDeque<Captured>> {
    CAPTURED.get_or_init(|| Mutex::new(VecDeque::new()))
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= &Level::WARN && metadata.target().starts_with("remuda_journal")
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> SpanId {
        SpanId::from_u64(1)
    }

    fn record(&self, _: &SpanId, _: &SpanRecord<'_>) {}

    fn record_follows_from(&self, _: &SpanId, _: &SpanId) {}

    fn enter(&self, _: &SpanId) {}

    fn exit(&self, _: &SpanId) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = CaptureVisitor::default();
        event.record(&mut visitor);
        if let Ok(mut queue) = captured().lock() {
            queue.push_back(Captured {
                level: *event.metadata().level(),
                target: event.metadata().target().to_owned(),
                fields: visitor.fields,
            });
        }
    }
}

static CAPTURE_ONCE: OnceLock<()> = OnceLock::new();

/// Install the process-global warning capture once per test binary. Events are
/// buffered globally and drained per instance id, so parallel tests stay
/// isolated.
async fn with_warning_capture<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    CAPTURE_ONCE.get_or_init(|| {
        tracing::subscriber::set_global_default(CaptureSubscriber)
            .expect("global warning subscriber");
    });
    f.await
}

fn take_captured(instance: &str) -> Vec<Captured> {
    let mut queue = captured().lock().expect("captured");
    let matches = |captured: &Captured| {
        captured
            .fields
            .iter()
            .any(|(key, value)| key == "instance" && value.contains(instance))
    };
    let found = queue.iter().filter(|c| matches(c)).cloned().collect();
    queue.retain(|c| !matches(c));
    found
}

/// The gate failure, made deterministic: a JSONL file whose last record was
/// cut mid-string by an unclean shutdown. Reopening must truncate the torn
/// bytes with one warning, keep every complete record, pin the watermark down,
/// and accept appends at the correct next seq.
#[tokio::test]
async fn reopen_truncates_a_torn_tail_and_keeps_complete_records() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, instance, _, map) = ctx("sess-torn");
    let envelopes = map_file(include_str!("fixtures/claude-transcript-ok.jsonl"), &map)?;
    assert!(envelopes.len() >= 4);
    let surviving = envelopes.len() - 1;
    let journal = open_journal(tmp.path())?;
    for envelope in envelopes {
        journal.append(&instance, envelope).await?;
    }
    let before = journal.read_range(&instance, U64(1), None).await?;
    drop(journal);

    // Tear the last record in half, including its newline: exactly what a
    // writer killed between write and the next append's read leaves behind.
    let path = tmp
        .path()
        .join("journal")
        .join(format!("{}.jsonl", instance.as_id()));
    let bytes = fs::read(&path)?;
    let last_newline = bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .expect("the last record is newline-terminated");
    let previous_newline = bytes[..last_newline]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |pos| pos + 1);
    // Halfway through the final record, with no terminating newline. After
    // `set_len(cut)` the file ends `cut - previous_newline` bytes past its last
    // newline: that is exactly the fragment recovery measures and reports.
    let cut = previous_newline + (last_newline - previous_newline) / 2;
    let torn_bytes = cut as u64 - previous_newline as u64;
    let file = fs::OpenOptions::new().write(true).open(&path)?;
    file.set_len(cut as u64)?;
    drop(file);

    with_warning_capture(async {
        let journal = open_journal(tmp.path())?;
        // Watermark follows the JSONL, not the stale SQLite row.
        assert_eq!(
            journal.durable_seq(&instance).await?.0,
            surviving as u64,
            "the torn seq must not count as durable"
        );
        let kept = journal.read_range(&instance, U64(1), None).await?;
        assert_eq!(kept.len(), surviving, "complete records survive");
        assert_eq!(
            kept.first().map(|o| o.event_id.clone()),
            before.first().map(|o| o.event_id.clone())
        );
        assert_eq!(
            kept.last().map(|o| o.seq.0),
            Some(surviving as u64),
            "no gap before the torn tail"
        );
        // The next append takes the recycled seq and reads back whole: the
        // gate symptom was this append's read dying on the torn JSONL line.
        let repair = map_file(
            r#"{"type":"user","uuid":"u-repair","timestamp":"2026-09-12T10:00:09.000Z","message":{"role":"user","content":"repaired"}}"#,
            &map,
        )?
        .remove(0);
        let seq = journal.append(&instance, repair).await?;
        assert_eq!(seq.0, surviving as u64 + 1);
        let read_back = journal.read_range(&instance, seq, Some(seq)).await?;
        assert_eq!(
            read_back.len(),
            1,
            "the repaired line parses, unlike the gate's json: EOF"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await?;

    let warnings = take_captured(instance.as_id().as_str());
    assert_eq!(warnings.len(), 1, "exactly one torn-tail warning");
    let warning = &warnings[0];
    assert!(
        warning
            .fields
            .iter()
            .any(|(key, value)| key == "torn_bytes" && value == &torn_bytes.to_string()),
        "warning must name the dropped byte count: {warning:?}"
    );

    // The torn fragment is physically gone and replaced by one complete
    // record; reopening again is a clean no-op.
    let repaired_bytes = fs::read(&path)?;
    assert_eq!(
        repaired_bytes.iter().filter(|b| **b == b'\n').count(),
        surviving + 1,
        "the torn line is gone, its seq holds one complete repair record"
    );
    assert!(
        repaired_bytes.iter().rposition(|b| *b == b'\n') == Some(repaired_bytes.len() - 1),
        "the file ends on a complete record again"
    );
    for line in repaired_bytes
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        serde_json::from_slice::<serde_json::Value>(line).expect("every line is valid JSON");
    }
    let journal = open_journal(tmp.path())?;
    assert!(take_captured(instance.as_id().as_str()).is_empty());
    assert_eq!(
        journal.durable_seq(&instance).await?.0,
        surviving as u64 + 1
    );
    Ok(())
}

/// A malformed record that *is* newline-terminated is corruption in the
/// committed prefix, not a torn append: recovery must fail rather than discard
/// an acknowledged observation.
#[tokio::test]
async fn a_corrupt_complete_record_fails_recovery() -> Result<()> {
    let tmp = TempDir::new()?;
    let (_, instance, _, map) = ctx("sess-corrupt");
    let envelopes = map_file(include_str!("fixtures/claude-transcript-ok.jsonl"), &map)?;
    let journal = open_journal(tmp.path())?;
    for envelope in envelopes {
        journal.append(&instance, envelope).await?;
    }
    drop(journal);

    let path = tmp
        .path()
        .join("journal")
        .join(format!("{}.jsonl", instance.as_id()));
    let mut bytes = fs::read(&path)?;
    bytes[0] = b'X'; // destroy the opening brace of record 1
    fs::write(&path, bytes)?;

    assert!(
        open_journal(tmp.path()).is_err(),
        "a corrupt complete record must not be trimmed away"
    );
    Ok(())
}

/// Two openers of one data dir are never writers at once. A second opener
/// waits (bounded) for a writer that is closing, but fails fast with a clear
/// [`remuda_journal::Error::Locked`] when the first writer is genuinely still
/// alive — it must never block a process startup forever.
#[test]
fn a_live_writer_makes_a_second_open_fail_locked_then_succeeds_on_close() -> Result<()> {
    let tmp = TempDir::new()?;
    let quick = JournalOptions {
        fsync: FsyncPolicy::Never,
        writer_lock_wait: Duration::from_millis(100),
    };
    let held = Journal::open_with(tmp.path(), quick.clone())?;

    // The live writer survives the bounded wait: the open fails Locked rather
    // than hanging. The typed error itself proves the wait is bounded, so no
    // wall-clock ceiling is asserted.
    let err = match Journal::open_with(tmp.path(), quick.clone()) {
        Ok(_) => panic!("a concurrent writer must make the second open fail"),
        Err(err) => err,
    };
    assert!(
        matches!(err, remuda_journal::Error::Locked { .. }),
        "expected Error::Locked, got {err:?}"
    );

    // Once the first writer closes, a concurrent opener acquires the lock on
    // the next poll instead of racing it.
    drop(held);
    let reopened = Journal::open_with(
        tmp.path(),
        JournalOptions {
            fsync: FsyncPolicy::Never,
            writer_lock_wait: Duration::from_secs(5),
        },
    )?;
    drop(reopened);
    Ok(())
}
