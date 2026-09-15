//! Dump the Workflow card observations a real on-disk run produces.
//!
//! Evidence helper for `docs/design/evidence/workflow-producer-1.md`. Runs the
//! real [`WorkflowJournalTailer`] against one captured session directory and
//! prints one JSON object per emitted observation, in fold order:
//!
//! ```sh
//! cargo run -p remuda-journal --example workflow_live_dump -- \
//!   --session-dir ~/.claude/projects/-tmp-x/<sid> \
//!   --run-id wf_xxxx --task-id wnxxxx --tool-call-id toolu_xxxx \
//!   --scan-transcript
//! ```
//!
//! `--scan-transcript` tails the main transcript from offset zero (a completed
//! capture); without it only bytes appended after startup are read (live).

use remuda_journal::{MapContext, WorkflowJournalTailer, WorkflowLaunch};
use remuda_protocol::{HostId, Id, InstanceId, ObservationPayload, RunId, SourceChannel};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Args {
    session_dir: PathBuf,
    run_id: String,
    task_id: Option<String>,
    tool_call_id: Option<String>,
    scan_transcript: bool,
    timeout: Duration,
}

fn parse_args() -> Args {
    let mut args = Args {
        session_dir: PathBuf::new(),
        run_id: String::new(),
        task_id: None,
        tool_call_id: None,
        scan_transcript: false,
        timeout: Duration::from_secs(20),
    };
    let mut iter = std::env::args().skip(1);
    macro_rules! take {
        () => {
            iter.next().expect("flag needs a value")
        };
    }
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--session-dir" => args.session_dir = take!().into(),
            "--run-id" => args.run_id = take!(),
            "--task-id" => args.task_id = Some(take!()),
            "--tool-call-id" => args.tool_call_id = Some(take!()),
            "--scan-transcript" => args.scan_transcript = true,
            "--timeout-secs" => {
                args.timeout = Duration::from_secs(take!().parse().unwrap());
            }
            other => panic!("unknown arg {other}"),
        }
    }
    args
}

fn main() {
    let args = parse_args();
    let mut ctx = MapContext::claude_file(
        InstanceId::new(),
        Id::new("obj").unwrap(),
        HostId::new(),
        "workflow-evidence",
        SourceChannel::WorkflowJournal,
    );
    ctx.run_id = Some(RunId::new());
    let mut tailer = WorkflowJournalTailer::new(&args.session_dir, ctx);
    let transcript = args.session_dir.parent().map(|parent| {
        parent.join(format!(
            "{}.jsonl",
            args.session_dir.file_name().unwrap().to_string_lossy()
        ))
    });
    if args.scan_transcript
        && let Some(transcript) = &transcript
        && transcript.is_file()
    {
        // Read the whole transcript (completed capture), including its
        // terminal <task-notification>.
        tailer.set_main_transcript_scan(transcript).unwrap();
    }
    let envelopes = tailer
        .launch(WorkflowLaunch {
            run_id: args.run_id.clone(),
            task_id: args.task_id,
            tool_call_id: args.tool_call_id,
            transcript_dir: Some(
                args.session_dir
                    .join("subagents/workflows")
                    .join(&args.run_id),
            ),
            script_path: None,
            script_source: None,
        })
        .unwrap();
    let mut emitted = 0usize;
    emitted += dump(envelopes);
    let started = Instant::now();
    while started.elapsed() < args.timeout {
        std::thread::sleep(Duration::from_millis(100));
        emitted += dump(tailer.poll().unwrap());
        if tailer.is_finished() {
            break;
        }
    }
    eprintln!("emitted {emitted} observations");
}

fn dump(envelopes: Vec<remuda_journal::Envelope>) -> usize {
    let n = envelopes.len();
    for envelope in envelopes {
        // Emit the complete payload so evidence can render it verbatim through
        // the production card component.
        let line = match envelope.body {
            ObservationPayload::WorkflowRun(payload) => {
                serde_json::to_value(payload.as_ref()).unwrap()
            }
            ObservationPayload::WorkflowPhase(payload) => {
                serde_json::to_value(payload.as_ref()).unwrap()
            }
            ObservationPayload::WorkflowMember(payload) => {
                serde_json::to_value(payload.as_ref()).unwrap()
            }
            other => serde_json::json!({"kind": format!("{other:?}")}),
        };
        println!("{}", serde_json::to_string(&line).unwrap());
    }
    n
}
