//! Durable append-only observations and command delivery journal.
//!
//! Native sessions remain the **resume authority**. This crate is the
//! **observation authority** only: it records what was seen, in a monotonic
//! `seq`, so Hub/Web clients can snapshot and follow. It does not feed
//! journal records back into a native agent as if they were the session.

#![forbid(unsafe_code)]

mod blob;
mod claude;
mod envelope;
mod error;
mod projection;
mod source;
mod store;
mod subagent;
mod util;
mod workflow;

pub use claude::{ClaudeJsonlTailer, NativeIds, map_claude_line};
pub use envelope::{Envelope, RawPayload};
pub use error::Error;
pub use projection::{
    Fold, InteractionProjection, InteractionRecord, OrderedFold, Projections, StatusProjection,
    ToolPair, TranscriptEntry, TranscriptProjection, TurnBound, TurnKind, fold_all,
    fold_prepend_backfill,
};
pub use source::{ADAPTER_VERSION, FileTail, MapContext, Source, SourceResume};
pub use store::{Follow, FsyncPolicy, Journal, JournalOptions, MAX_PAGE, Page, Snapshot};
pub use subagent::{
    SubagentKind, SubagentTranscript, SubagentTranscriptMeta, locate_agent_file,
    read_subagent_transcript,
};
pub use util::{digest_of, timestamp_now};
pub use workflow::{
    AgentCallSite, MatchedCall, WorkflowJournalTailer, WorkflowLaunch, WorkflowScript,
};
