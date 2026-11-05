//! `remuda cmd ls|edit|withdraw|deliver` — the caller's own queued commands
//! (cb-command-queue, protocol.md §8.5).
//!
//! "Own" is decided by the Hub from the credential (an Agent's stamped
//! initiator, a Human or Bot device's own queue); "queued" means provably
//! undelivered. `deliver` is the D-055 same-id re-POST a client makes once the
//! host is back: the Hub forwards a still-queued command exactly once, under
//! the same commandId, with its edited text; it never creates a second copy.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, print_json};
use super::registry::Entrypoint;

/// `remuda cmd` arguments.
#[derive(Args)]
#[command(about = "List, edit, withdraw or deliver your own queued commands.")]
pub(crate) struct CmdArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: CmdCommand,
}

#[derive(Debug, Subcommand)]
enum CmdCommand {
    /// Your sends and configures that have not reached the harness, newest
    /// first, each with its `revision` and where it waits (`held`).
    Ls {
        /// Only commands for this instance (`ins_…`).
        #[arg(long)]
        instance: Option<String>,
        /// Owner only: every caller's queued commands, not just yours.
        #[arg(long)]
        all: bool,
    },
    /// Replace the text of a queued send; the commandId stays the same.
    Edit {
        /// `cmd_…`.
        command_id: String,
        /// The new text.
        #[arg(long)]
        text: String,
        /// The revision you read (`cmd ls`); defaults to the current one.
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Take back a queued command; it is never delivered.
    Withdraw {
        /// `cmd_…`.
        command_id: String,
        /// Refuse if the command was edited since this revision.
        #[arg(long)]
        revision: Option<u64>,
    },
    /// Forward a command the Hub still holds, now that its host is back:
    /// the same commandId, delivered at most once.
    Deliver {
        /// `cmd_…`.
        command_id: String,
    },
}

impl Entrypoint for CmdArgs {
    fn enter(self, _context: super::registry::Context) -> Result<i32> {
        block_on(async move {
            let client = self.hub.connect()?;
            let value = match self.command {
                CmdCommand::Ls { instance, all } => {
                    let mut path = String::from("/v1/commands?state=queued");
                    if !all {
                        path.push_str("&mine=1");
                    }
                    if let Some(instance) = instance {
                        path.push_str(&format!("&instanceId={instance}"));
                    }
                    client.get(&path).await?
                }
                CmdCommand::Edit {
                    command_id,
                    text,
                    revision,
                } => {
                    let revision = match revision {
                        Some(revision) => revision,
                        None => queued(&client, &command_id)
                            .await?
                            .get("revision")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    };
                    client
                        .patch(
                            &format!("/v1/commands/{command_id}"),
                            &json!({ "expectedRevision": revision, "text": text }),
                        )
                        .await?
                }
                CmdCommand::Withdraw {
                    command_id,
                    revision,
                } => {
                    let body = match revision {
                        Some(revision) => json!({ "expectedRevision": revision }),
                        None => json!({}),
                    };
                    client
                        .post(&format!("/v1/commands/{command_id}/withdraw"), &body)
                        .await?
                }
                CmdCommand::Deliver { command_id } => deliver(&client, &command_id).await?,
            };
            print_json(&value)?;
            Ok(0)
        })
    }
}

/// The caller's queued row for `command_id`.
async fn queued(client: &HubClient, command_id: &str) -> Result<Value> {
    let listing = client.get("/v1/commands?state=queued&mine=1").await?;
    listing
        .get("commands")
        .and_then(Value::as_array)
        .and_then(|rows| {
            rows.iter()
                .find(|row| row.get("commandId").and_then(Value::as_str) == Some(command_id))
        })
        .cloned()
        .with_context(|| {
            format!("{command_id} is not one of your queued commands (see `remuda cmd ls`)")
        })
}

/// The D-055 same-id re-POST of a Hub-held command.
async fn deliver(client: &HubClient, command_id: &str) -> Result<Value> {
    let row = queued(client, command_id).await?;
    if row.get("held").and_then(Value::as_str) != Some("hub") {
        bail!("{command_id} already left the Hub; its Node delivers it");
    }
    if row.get("operation").and_then(Value::as_str) != Some("instance.send") {
        bail!(
            "{command_id} is a queued configure, which is never replayed (D-055): withdraw it \
             and send a new one"
        );
    }
    let instance = row
        .get("instanceId")
        .and_then(Value::as_str)
        .context("queued command without an instance")?;
    let payload = row.get("payload").cloned().unwrap_or_else(|| json!({}));
    Ok(client
        .post_command(instance, "instance.send", payload, Some(command_id))
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Wrap {
        #[command(subcommand)]
        command: CmdCommand,
    }

    #[test]
    fn cmd_subcommands_parse() {
        let parsed = Wrap::try_parse_from(["cmd", "ls", "--all", "--instance", "ins_x"]).unwrap();
        assert!(matches!(
            parsed.command,
            CmdCommand::Ls {
                all: true,
                instance: Some(_)
            }
        ));
        let parsed =
            Wrap::try_parse_from(["cmd", "edit", "cmd_x", "--text", "t", "--revision", "2"])
                .unwrap();
        assert!(matches!(
            parsed.command,
            CmdCommand::Edit {
                revision: Some(2),
                ..
            }
        ));
        assert!(
            Wrap::try_parse_from(["cmd", "edit", "cmd_x"]).is_err(),
            "--text is required"
        );
        let parsed = Wrap::try_parse_from(["cmd", "withdraw", "cmd_x"]).unwrap();
        assert!(matches!(
            parsed.command,
            CmdCommand::Withdraw { revision: None, .. }
        ));
        let parsed = Wrap::try_parse_from(["cmd", "deliver", "cmd_x"]).unwrap();
        assert!(matches!(parsed.command, CmdCommand::Deliver { .. }));
    }
}
