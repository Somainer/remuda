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
//! 1. **It never fails the agent.** A missing socket or a stopped Node prints
//!    `{}` and exits 0, which every hook event reads as "no opinion": the agent
//!    does exactly what it would have done without Remuda. The one deliberate
//!    exception is a *blocking* event that reached the Node and got no answer
//!    in time — see [`decide`], where §4.4's fail-closed rule makes that a deny
//!    rather than a shrug.
//! 2. **The wait is bounded.** Blocking events wait up to the interaction
//!    broker's TTL; everything else gets a short wait, because a
//!    fire-and-forget event has nothing worth waiting for.
//! 3. **No interpreter.** It is the `remuda` binary the Node already ships, so
//!    there is no python (or jq, or node) on the path between a hook and its
//!    answer — one fewer thing that can be absent on a user's machine, and one
//!    fewer script to keep in sync with the payload shape.

use anyhow::Result;
use clap::{Args, Subcommand};
use remuda_signal::{BLOCKING_WAIT, Delivery, HookDecision, HookEnvelope, deliver_event};
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
    /// Merge a hand-typed Claude's settings and exec it without changing pid.
    Launch(LaunchArgs),
}

#[derive(Args)]
struct LaunchArgs {
    #[arg(long)]
    overlay: PathBuf,
    /// Real executable followed by its original arguments.
    #[arg(last = true, required = true)]
    argv: Vec<String>,
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
        match self.command {
            HookCommand::Emit(args) => emit(args),
            HookCommand::Launch(mut args) => {
                let binary = args.argv.remove(0);
                remuda_driver::launch::overlay::merge_explicit_settings(
                    &args.overlay,
                    &mut args.argv,
                )?;
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    Err(std::process::Command::new(binary)
                        .args(args.argv)
                        .exec()
                        .into())
                }
                #[cfg(not(unix))]
                {
                    Ok(std::process::Command::new(binary)
                        .args(args.argv)
                        .status()?
                        .code()
                        .unwrap_or(1))
                }
            }
        }
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
        .unwrap_or_else(|| wait_for(&args.event, &payload));
    let event = args.event.clone();
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
    let delivery = runtime.block_on(deliver_event(&args.socket, &envelope, timeout));
    println!("{}", decide(&event, &envelope.payload, delivery));
    // Always 0: a non-zero exit from a hook is a signal to the harness, and
    // "Remuda could not be reached" is not something the agent should act on.
    Ok(0)
}

/// What the relay prints, given what became of the delivery.
///
/// The three outcomes are deliberately not the same answer:
///
/// - **Replied** — print it verbatim, whatever it is. A `{}` from the Node is
///   the Node declining to have an opinion, which is its right.
/// - **TimedOut on a blocking event** — print a deny. The Node was *there* and
///   a human may be staring at a card; §4.4 requires an approval nobody
///   answered in time to fail closed rather than hang or default to allow.
/// - **Unreachable** — print `{}`. Remuda is not in the loop, so the agent's
///   own dialog is the only place a decision can be made, and denying here
///   would break every tool call on a host whose Node merely stopped.
///
/// A non-blocking event never denies: there is nothing to refuse.
fn decide(event: &str, payload: &serde_json::Value, delivery: Delivery) -> serde_json::Value {
    match delivery {
        Delivery::Replied(reply) => reply.to_hook_json(),
        Delivery::TimedOut if remuda_signal::event::hooks_block(event, payload) => {
            HookDecision::timed_out().to_hook_json(event)
        }
        Delivery::TimedOut | Delivery::Unreachable => serde_json::json!({}),
    }
}

/// Wait budget for one event.
fn wait_for(event: &str, payload: &serde_json::Value) -> std::time::Duration {
    if remuda_signal::event::hooks_block(event, payload) {
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
        let empty = serde_json::json!({});
        assert_eq!(wait_for("PermissionRequest", &empty), BLOCKING_WAIT);
        assert_eq!(wait_for("Elicitation", &empty), BLOCKING_WAIT);
    }

    #[test]
    fn an_auto_mode_askuserquestion_pretooluse_gets_the_blocking_budget() {
        // The whole c-askq gap: this pairing is the owner's main path, and a
        // 5 s fire-and-forget budget would deny every phone answer.
        let question = serde_json::json!({"tool_name": "AskUserQuestion"});
        assert_eq!(
            wait_for("PreToolUse", &question),
            BLOCKING_WAIT,
            "an auto-mode question parks like a PermissionRequest"
        );
        assert_eq!(
            wait_for("PreToolUse", &serde_json::json!({"tool_name": "Bash"})),
            NONBLOCKING_WAIT
        );
    }

    #[test]
    fn a_fire_and_forget_event_does_not_hold_the_agent_for_minutes() {
        let empty = serde_json::json!({});
        for event in ["SessionStart", "UserPromptSubmit", "MessageDisplay", "Stop"] {
            let wait = wait_for(event, &empty);
            assert_eq!(wait, NONBLOCKING_WAIT, "{event}");
            assert!(wait < BLOCKING_WAIT);
        }
    }

    #[test]
    fn a_reply_is_printed_verbatim_including_an_empty_one() {
        // `{}` from the Node is the Node declining to have an opinion.
        let empty = serde_json::json!({});
        let reply = remuda_signal::HookReply {
            decision: Some(serde_json::json!({"hookSpecificOutput": {"x": 1}})),
        };
        assert_eq!(
            decide("PermissionRequest", &empty, Delivery::Replied(reply)),
            serde_json::json!({"hookSpecificOutput": {"x": 1}})
        );
        assert_eq!(
            decide(
                "PermissionRequest",
                &empty,
                Delivery::Replied(remuda_signal::HookReply::empty())
            ),
            serde_json::json!({})
        );
    }

    #[test]
    fn an_approval_nobody_answered_in_time_denies() {
        // §4.4 fail-closed. Printing `{}` here would drop the agent onto its
        // own dialog with no one watching, and an allow would run a tool
        // nobody approved.
        let json = decide(
            "PermissionRequest",
            &serde_json::json!({}),
            Delivery::TimedOut,
        );
        assert_eq!(
            json["hookSpecificOutput"]["decision"]["behavior"], "deny",
            "{json}"
        );
        assert_eq!(
            json["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
    }

    #[test]
    fn a_timed_out_elicitation_also_denies() {
        let json = decide("Elicitation", &serde_json::json!({}), Delivery::TimedOut);
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "Elicitation");
        assert_eq!(json["hookSpecificOutput"]["decision"]["behavior"], "deny");
    }

    #[test]
    fn a_timed_out_auto_mode_question_denies_in_pretooluse_shape() {
        // The fail-closed deadline for the auto-mode path must read as a
        // PreToolUse permissionDecision deny; a PermissionRequest-shaped body
        // is dropped on this event.
        let json = decide(
            "PreToolUse",
            &serde_json::json!({"tool_name": "AskUserQuestion"}),
            Delivery::TimedOut,
        );
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecision"],
            serde_json::json!("deny")
        );
        assert!(
            json["hookSpecificOutput"].get("decision").is_none(),
            "{json}"
        );
    }

    #[test]
    fn an_absent_node_leaves_the_decision_to_the_agents_own_dialog() {
        // Remuda is not in the loop; denying every tool call because the Node
        // stopped would make a dead Node look like a hostile one.
        assert_eq!(
            decide(
                "PermissionRequest",
                &serde_json::json!({}),
                Delivery::Unreachable
            ),
            serde_json::json!({})
        );
    }

    #[test]
    fn a_non_blocking_event_never_denies() {
        // There is nothing to refuse, and a deny-shaped reply to `Stop` would
        // be read as a decision about something.
        for event in ["SessionStart", "Stop", "MessageDisplay"] {
            for delivery in [Delivery::TimedOut, Delivery::Unreachable] {
                assert_eq!(
                    decide(event, &serde_json::json!({}), delivery),
                    serde_json::json!({}),
                    "{event}"
                );
            }
        }
    }

    #[test]
    fn an_ordinary_pretooluse_timeout_stays_no_opinion() {
        // Only AskUserQuestion turns PreToolUse into a blocking pairing; a
        // Bash timing out on the relay keeps the old fire-and-forget behavior.
        assert_eq!(
            decide(
                "PreToolUse",
                &serde_json::json!({"tool_name": "Bash"}),
                Delivery::TimedOut
            ),
            serde_json::json!({})
        );
    }
}
