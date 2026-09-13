//! `remuda hook emit` — the relay a harness hook runs (D-028 §4.2).
//!
//! A hook fires as a short-lived child of the agent. It gets the hook payload
//! on stdin and must print the harness's decision JSON on stdout. This relay is
//! the whole of that: read stdin, forward it over the instance's hook socket,
//! print whatever comes back.
//!
//! Three properties matter more than anything else here, because this process
//! sits between a human's keystroke and the agent responding to it:
//!
//! 1. **It never fails the agent.** A missing socket, a stopped Node, a Node
//!    that never answers — all print `{}` and exit 0, which every hook event
//!    reads as "no opinion". The agent then does exactly what it would have
//!    done without Remuda.
//! 2. **The wait is bounded.** Blocking events wait up to the interaction
//!    broker's TTL; everything else gets a short wait, because a
//!    fire-and-forget event has nothing worth waiting for.
//! 3. **No interpreter.** It is the `remuda` binary the Node already ships, so
//!    there is no python (or jq, or node) on the path between a hook and its
//!    answer — one fewer thing that can be absent on a user's machine, and one
//!    fewer script to keep in sync with the payload shape.

use anyhow::Result;
use clap::{Args, Subcommand};
use remuda_signal::{BLOCKING_WAIT, HookEnvelope, event::is_blocking, send_event};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

/// Wait for a non-blocking event.
///
/// The agent is not waiting on the answer, so this only has to cover a Node
/// that is briefly busy. Anything longer would add latency to every streamed
/// line for no benefit.
const NONBLOCKING_WAIT: Duration = Duration::from_secs(5);

/// Largest hook payload the relay will forward.
const MAX_STDIN: u64 = 4 * 1024 * 1024;

#[derive(Args)]
#[command(about = "Relay a harness hook event to the Node that launched this agent.")]
pub(crate) struct CommandArgs {
    #[command(subcommand)]
    command: HookCommand,
}

#[derive(Subcommand)]
enum HookCommand {
    /// Forward one hook event and print the harness decision.
    Emit(EmitArgs),
}

#[derive(Args)]
pub(crate) struct EmitArgs {
    /// Instance hook socket (`<instance dir>/hook.sock`).
    #[arg(long)]
    socket: PathBuf,
    /// Hook event name, e.g. `SessionStart`.
    #[arg(long)]
    event: String,
    /// Per-instance credential. Defaults to `$REMUDA_HOOK_CREDENTIAL`.
    #[arg(long)]
    credential: Option<String>,
    /// Override the wait in milliseconds. Diagnostics only.
    #[arg(long)]
    timeout_ms: Option<u64>,
}

impl super::registry::Entrypoint for CommandArgs {
    fn enter(self, _context: super::registry::Context) -> Result<i32> {
        let HookCommand::Emit(args) = self.command;
        emit(args)
    }

    /// A hook runs inside the agent's own process tree and its stderr can end
    /// up on the user's terminal. Stay silent.
    fn tracing(&self) -> bool {
        false
    }
}

fn emit(args: EmitArgs) -> Result<i32> {
    let payload = read_payload();
    let credential = args
        .credential
        .or_else(|| std::env::var("REMUDA_HOOK_CREDENTIAL").ok())
        .unwrap_or_default();
    let timeout = args
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or_else(|| wait_for(&args.event));
    let envelope = HookEnvelope {
        credential,
        event: args.event,
        // The agent that ran this hook. The Node binds a promoted terminal to
        // its session by matching this against the PTY's foreground process
        // group leader, so it has to be the parent, not `id()`.
        ppid: parent_pid(),
        payload,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let reply = runtime.block_on(send_event(&args.socket, &envelope, timeout));
    println!("{}", reply.to_hook_json());
    // Always 0: a non-zero exit from a hook is a signal to the harness, and
    // "Remuda could not be reached" is not something the agent should act on.
    Ok(0)
}

/// Wait budget for one event.
fn wait_for(event: &str) -> Duration {
    if is_blocking(event) {
        BLOCKING_WAIT
    } else {
        NONBLOCKING_WAIT
    }
}

/// Read the hook payload from stdin, capped.
///
/// Anything unreadable or unparseable becomes `{}` rather than an error: the
/// relay's job is to forward what it can, and a malformed payload is the
/// harness's business, not a reason to break the turn.
fn read_payload() -> serde_json::Value {
    let mut body = String::new();
    if std::io::stdin()
        .take(MAX_STDIN)
        .read_to_string(&mut body)
        .is_err()
    {
        return serde_json::json!({});
    }
    serde_json::from_str(body.trim()).unwrap_or_else(|_| serde_json::json!({}))
}

/// Pid of the process that ran this hook — the agent.
fn parent_pid() -> i32 {
    #[cfg(unix)]
    {
        nix::unistd::getppid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocking_event_waits_as_long_as_the_broker_would_hold_the_ticket() {
        // Giving up sooner than the broker retires the ticket would turn a
        // decision the user did make into a silent fallback.
        assert_eq!(wait_for("PermissionRequest"), BLOCKING_WAIT);
        assert_eq!(wait_for("Elicitation"), BLOCKING_WAIT);
    }

    #[test]
    fn a_fire_and_forget_event_does_not_hold_the_agent_for_minutes() {
        for event in ["SessionStart", "UserPromptSubmit", "MessageDisplay", "Stop"] {
            let wait = wait_for(event);
            assert_eq!(wait, NONBLOCKING_WAIT, "{event}");
            assert!(wait < BLOCKING_WAIT);
        }
    }
}
