//! Adapter failures. Policy drops are [`crate::inbound::DropReason`], not errors.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Failure parsing Feishu events, rendering cards, or talking to `lark-cli`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Blank NDJSON line from `event consume`.
    #[error("empty event line")]
    EmptyLine,
    /// JSON encode/decode of an event, card, or CLI payload.
    #[error("feishu JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `type` was neither `im.message.receive_v1` nor `card.action.trigger`.
    #[error("unknown event type {0}")]
    UnknownEventType(String),
    /// Required flattened consume field was missing or empty.
    #[error("missing field {0}")]
    MissingField(&'static str),
    /// Slash command could not be interpreted.
    #[error("invalid command: {0}")]
    InvalidCommand(String),
    /// Ticket TTL is outside the 10–15 minute window.
    #[error("interaction TTL must be 10–15 minutes (got {0:?})")]
    InvalidTtl(Duration),
    /// Card JSON 2.0 failed the structural checks from the create-lark-card skill.
    #[error("card JSON 2.0 invalid: {0}")]
    Card(String),
    /// Callback `tid` does not match an open ticket.
    #[error("ticket {0} not found")]
    TicketNotFound(String),
    /// First writer already committed an answer for this ticket.
    #[error("ticket {0} already answered")]
    TicketAnswered(String),
    /// Ticket is past its runtime deadline (10–15 min, shorter than the 30 min card token).
    #[error("ticket {0} expired")]
    TicketExpired(String),
    /// Answer arrived from a session/chat the ticket was never posted to.
    #[error("ticket {ticket_id} does not belong to {scope}")]
    TicketScope {
        /// Callback `tid` that was presented.
        ticket_id: String,
        /// Session key or chat id that tried to answer it.
        scope: String,
    },
    /// This Interaction kind has no Feishu card encoder yet.
    #[error("unsupported interaction kind for Feishu cards")]
    UnsupportedInteraction,
    /// Button/form payload failed InteractionAnswer validation.
    #[error("invalid card answer: {0}")]
    InvalidAnswer(String),
    /// Subprocess I/O (spawn, pipes, kill).
    #[error("lark-cli I/O: {0}")]
    Io(#[from] io::Error),
    /// Live `lark-cli` exited non-zero.
    #[error("lark-cli failed (status {status:?}): {stderr}")]
    Cli {
        /// Process exit code, if the OS reported one.
        status: Option<i32>,
        /// Captured stderr, already scrubbed and capped by
        /// [`crate::redact_cli_stderr`] — this value is logged.
        stderr: String,
    },
    /// Live `lark-cli` exceeded the outbound timeout.
    #[error("lark-cli timed out")]
    CliTimeout,
    /// Configured binary path does not exist.
    #[error("lark-cli binary not found: {0}")]
    BinaryNotFound(PathBuf),
    /// `--file` rejected an absolute path or `..` segment (lark-cli contract).
    #[error("file path must be cwd-relative without `..`: {0}")]
    UnsafePath(PathBuf),
    /// A consume stdout line exceeded the configured byte cap.
    #[error("event line exceeds {0} bytes")]
    LineTooLong(usize),
    /// SQLite session_key → instance map failed.
    #[error("session store: {0}")]
    SessionStore(String),
    /// Hub/runtime InstanceApi call failed.
    #[error("instance api: {0}")]
    InstanceApi(String),
    /// Prompt/command needs a live instance and none is mapped.
    #[error("no live instance for session {0}")]
    NoLiveInstance(String),
}
