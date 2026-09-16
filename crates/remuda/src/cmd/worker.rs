//! `remuda worker` — intervene on one roster worker (M1 batch 5b).
//!
//! Every action is a Hub route that drives Hub → Node → `instance.send` /
//! `tty.write`; the CLI never opens an ssh session. The verbs productise the
//! coordinator handback practice:
//! - `nudge`  — file-transport "continue where you left off" prompt;
//! - `answer` — answer a dialog with enter / esc / a digit / free text;
//! - `switch-model` — the `/model` + confirmation key dance, gated on the
//!   screen actually showing a confirmation;
//! - `resume` — respawn the agent in its same worktree and re-send the brief;
//! - `replace` — retire and re-dispatch the same brief;
//! - `stop`   — close the running instance without reclaiming its worktree.

use clap::{Args, Subcommand};
use serde_json::json;

use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

#[derive(Args)]
#[command(about = "Nudge, answer, switch model, resume, replace, or stop a worker.")]
pub(crate) struct WorkerArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: WorkerCommand,
}

#[derive(Subcommand)]
enum WorkerCommand {
    /// Send a continue prompt (delivered as a file attachment).
    Nudge {
        /// Worker name or `wkr_…` id.
        worker: String,
        /// Custom nudge text instead of the default continue prompt.
        #[arg(long)]
        text: Option<String>,
    },
    /// Answer a dialog: enter, esc, a digit 1..9, or free text (submitted).
    Answer {
        /// Worker name or `wkr_…` id.
        worker: String,
        /// `enter` | `esc` | `1`..`9` | the text to type.
        key: String,
    },
    /// Switch the live model with the /model + confirm key dance.
    SwitchModel {
        /// Worker name or `wkr_…` id.
        worker: String,
        /// Model id to switch to.
        model: String,
    },
    /// Respawn the agent in its same worktree and re-send the last brief.
    Resume {
        /// Worker name or `wkr_…` id.
        worker: String,
        /// Override the state-loss handback note.
        #[arg(long)]
        handback: Option<String>,
    },
    /// Retire the worker and dispatch the same brief again (fresh branch).
    Replace {
        /// Worker name or `wkr_…` id.
        worker: String,
    },
    /// Close the running instance without reclaiming its worktree.
    Stop {
        /// Worker name or `wkr_…` id.
        worker: String,
    },
}

impl Entrypoint for WorkerArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            let client = self.hub.connect()?;
            let (worker, path, body) = match self.command {
                WorkerCommand::Nudge { worker, text } => (worker, "nudge", json!({ "text": text })),
                WorkerCommand::Answer { worker, key } => (worker, "answer", json!({ "key": key })),
                WorkerCommand::SwitchModel { worker, model } => {
                    (worker, "switch-model", json!({ "model": model }))
                }
                WorkerCommand::Resume { worker, handback } => {
                    (worker, "resume", json!({ "handback": handback }))
                }
                WorkerCommand::Replace { worker } => (worker, "replace", json!({})),
                WorkerCommand::Stop { worker } => (worker, "stop", json!({})),
            };
            let value = client
                .post(&format!("/v1/workers/{worker}/{path}"), &body)
                .await?;
            print_json(&value)?;
            Ok(0)
        })
    }
}
