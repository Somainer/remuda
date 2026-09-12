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
    let mut ids = NativeIds::new();
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
