//! `remuda own` — the path-ownership map and the gate-time diff scope check;
//! design §4.3 (`scopeCheck.diffMustStayWithin: owns`) and §7 #3.
//!
//! `own check` is the single highest-value new primitive from the playbook:
//! it turns "the diff grew past the files the task named" — the most common
//! rejection reason no gate can catch — into an executable check. It judges
//! *paths* only; semantic diff review stays the LLM's job. Exit status is
//! non-zero when the diff leaves the task's `owns[]`, so the merge gate (batch
//! 6) can call it directly.

use clap::{Args, Subcommand};
use serde_json::json;

use super::hub_client::{HubOpts, block_on, print_json};

/// `remuda own` subcommands.
#[derive(Args)]
#[command(about = "Claim and check repo-path ownership against the task ledger.")]
pub(crate) struct OwnArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: OwnCommand,
}

impl super::registry::Entrypoint for OwnArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        // The check subcommand maps an out-of-scope diff to exit 2.
        block_on(async move { run(self.hub, self.command).await })
    }
}

#[derive(Subcommand)]
enum OwnCommand {
    /// Claim repo-path globs for a task; conflicting active claims are refused.
    Claim {
        /// `tsk_…` task id.
        task: String,
        /// Repo-relative glob (`crates/remuda-hub/src/**`); repeat for more.
        #[arg(long = "path", value_name = "GLOB", required = true)]
        paths: Vec<String>,
    },
    /// Release claimed globs (no `--path` releases everything for the task).
    Release {
        /// `tsk_…` task id.
        task: String,
        #[arg(long = "path", value_name = "GLOB")]
        paths: Vec<String>,
    },
    /// Reconcile a diff (or explicit paths) against the task's `owns[]`.
    Check {
        /// `tsk_…` task id.
        task: String,
        /// Read a unified diff / `--stat` / `--name-only` / NUL-separated list
        /// from a file (`-` = stdin); e.g. `git diff main...br | remuda own check T -`.
        #[arg(long = "diff-file", value_name = "FILE")]
        diff_file: Option<String>,
        /// Inline diff text.
        #[arg(long)]
        diff: Option<String>,
        /// Bypass diff parsing and check explicit paths; repeat for more.
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<String>,
    },
    /// List active claims (the ownership map), optionally one project.
    List {
        #[arg(long)]
        project: Option<String>,
    },
}

async fn run(hub: HubOpts, command: OwnCommand) -> anyhow::Result<i32> {
    let client = hub.connect()?;
    match command {
        OwnCommand::Claim { task, paths } => {
            let value = client
                .post(&format!("/v1/tasks/{task}/own"), &json!({ "paths": paths }))
                .await?;
            print_json(&value)?;
            Ok(0)
        }
        OwnCommand::Release { task, paths } => {
            let value = client
                .delete(&format!("/v1/tasks/{task}/own"), &json!({ "paths": paths }))
                .await?;
            print_json(&value)?;
            Ok(0)
        }
        OwnCommand::Check {
            task,
            diff_file,
            diff,
            paths,
        } => {
            let mut body = json!({ "taskId": task });
            if !paths.is_empty() {
                body["paths"] = json!(paths);
            } else if let Some(file) = diff_file {
                let text = if file == "-" {
                    std::io::read_to_string(std::io::stdin())?
                } else {
                    std::fs::read_to_string(&file)
                        .map_err(|err| anyhow::anyhow!("read diff file {file:?}: {err}"))?
                };
                body["diff"] = json!(text);
            } else if let Some(text) = diff {
                body["diff"] = json!(text);
            } else {
                anyhow::bail!("own check needs --diff-file, --diff, or --path");
            }
            let value = client.post("/v1/own/check", &body).await?;
            print_json(&value)?;
            if value["within"].as_bool() == Some(true) {
                Ok(0)
            } else {
                Ok(2)
            }
        }
        OwnCommand::List { project } => {
            let suffix = project
                .map(|value| format!("?project={}", urlencode(&value)))
                .unwrap_or_default();
            let value = client.get(&format!("/v1/own{suffix}")).await?;
            print_json(&value)?;
            Ok(0)
        }
    }
}

fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}
