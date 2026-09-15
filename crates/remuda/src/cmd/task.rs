//! `remuda task` — the task ledger; design §2.2, §2.5, §8.1 row 4.
//!
//! Every task lives in one project's ledger slice, carries its inherited
//! mandate chain, and unlocks its dependency edges only from a *landed sha*
//! (`remuda task land` is batch 6; the state machine already refuses to treat
//! a worker's `done` status as an unlock).

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubOpts, block_on, print_json};

/// `remuda task` subcommands.
#[derive(Args)]
#[command(about = "Manage the Hub task ledger (state machine, deps, mandate chain).")]
pub(crate) struct TaskArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: TaskCommand,
}

impl super::registry::Entrypoint for TaskArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

#[derive(Args)]
struct BudgetFlags {
    /// Estimated USD cap.
    #[arg(long)]
    max_usd: Option<String>,
    /// Turn cap.
    #[arg(long)]
    max_turns: Option<i64>,
    /// Wall-clock cap, minutes.
    #[arg(long)]
    max_wall_mins: Option<i64>,
}

impl BudgetFlags {
    fn attach(&self, body: &mut Value) {
        let mut budget = json!({});
        put(&mut budget, "maxUsd", self.max_usd.clone());
        put(&mut budget, "maxTurns", self.max_turns);
        put(&mut budget, "maxWallMins", self.max_wall_mins);
        if budget.as_object().is_some_and(|map| !map.is_empty()) {
            body["budget"] = budget;
        }
    }
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Add a root task to a project's ledger.
    Add {
        /// `prj_…` project id.
        #[arg(long)]
        project: String,
        /// Short title.
        #[arg(long)]
        title: String,
        /// The owner's/coordinator's words for this task (use the file form for
        /// long text); the root link of the mandate chain.
        #[arg(long, group = "intent-source")]
        intent: Option<String>,
        /// Read the intent text from a file (`-` = stdin).
        #[arg(long, group = "intent-source")]
        intent_file: Option<String>,
        /// Task class (`research|implement|review|test|merge-gate|triage|docs`).
        #[arg(long)]
        class: Option<String>,
        /// Ownership glob claimed at add time; repeat for more.
        #[arg(long = "owns", value_name = "GLOB")]
        owns: Vec<String>,
        /// Dependency task id; repeat for more (unlocks on landed sha only).
        #[arg(long = "dep", value_name = "TSK")]
        deps: Vec<String>,
        #[command(flatten)]
        budget: BudgetFlags,
    },
    /// List tasks (optionally one project and/or state).
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        state: Option<String>,
    },
    /// Show one task (with its full mandate chain) as JSON.
    Show {
        /// `tsk_…` id.
        id: String,
    },
    /// Split a child task off a parent; inherits the mandate chain.
    Split {
        /// Parent `tsk_…` id.
        id: String,
        #[arg(long)]
        title: String,
        #[arg(long, group = "split-intent-source")]
        intent: Option<String>,
        #[arg(long = "intent-file", group = "split-intent-source")]
        intent_file: Option<String>,
        #[arg(long)]
        class: Option<String>,
        #[arg(long = "owns", value_name = "GLOB")]
        owns: Vec<String>,
        #[arg(long = "dep", value_name = "TSK")]
        deps: Vec<String>,
        #[command(flatten)]
        budget: BudgetFlags,
    },
    /// Move a task along the ledger state machine; illegal jumps are refused.
    SetState {
        /// `tsk_…` id.
        id: String,
        /// Target state (`pending|placed|running|stalled|done|failed|parked|deferred`).
        #[arg(long)]
        state: String,
        /// Reason, notably for `failed` (`BLOCKED <reason>`).
        #[arg(long)]
        reason: Option<String>,
    },
}

fn run(hub: HubOpts, command: TaskCommand) -> anyhow::Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        let value = match command {
            TaskCommand::Add {
                project,
                title,
                intent,
                intent_file,
                class,
                owns,
                deps,
                budget,
            } => {
                let mut body = json!({
                    "projectId": project,
                    "title": title,
                    "intent": read_intent(intent, intent_file)?,
                });
                put(&mut body, "class", class);
                add_path_array(&mut body, "owns", owns);
                add_deps(&mut body, deps);
                budget.attach(&mut body);
                client.post("/v1/tasks", &body).await?
            }
            TaskCommand::List { project, state } => {
                let mut query = Vec::new();
                if let Some(value) = project {
                    query.push(format!("project={}", urlencode(&value)));
                }
                if let Some(value) = state {
                    query.push(format!("state={}", urlencode(&value)));
                }
                let suffix = if query.is_empty() {
                    String::new()
                } else {
                    format!("?{}", query.join("&"))
                };
                client.get(&format!("/v1/tasks{suffix}")).await?
            }
            TaskCommand::Show { id } => client.get(&format!("/v1/tasks/{id}")).await?,
            TaskCommand::Split {
                id,
                title,
                intent,
                intent_file,
                class,
                owns,
                deps,
                budget,
            } => {
                let mut body = json!({
                    "title": title,
                    "intent": read_intent(intent, intent_file)?,
                });
                put(&mut body, "class", class);
                add_path_array(&mut body, "owns", owns);
                add_deps(&mut body, deps);
                budget.attach(&mut body);
                client.post(&format!("/v1/tasks/{id}/split"), &body).await?
            }
            TaskCommand::SetState { id, state, reason } => {
                let mut body = json!({ "state": state });
                put(&mut body, "reason", reason);
                client.patch(&format!("/v1/tasks/{id}"), &body).await?
            }
        };
        print_json(&value)
    })
}

fn add_path_array(body: &mut Value, key: &str, paths: Vec<String>) {
    if !paths.is_empty() {
        body[key] = json!(paths);
    }
}

fn add_deps(body: &mut Value, deps: Vec<String>) {
    if !deps.is_empty() {
        body["deps"] = json!(
            deps.into_iter()
                .map(|id| json!({ "taskId": id }))
                .collect::<Vec<_>>()
        );
    }
}

fn read_intent(inline: Option<String>, file: Option<String>) -> anyhow::Result<String> {
    match (inline, file) {
        (Some(text), _) => Ok(text),
        (None, Some(path)) => {
            if path == "-" {
                std::io::read_to_string(std::io::stdin()).map_err(Into::into)
            } else {
                std::fs::read_to_string(&path)
                    .map_err(|err| anyhow::anyhow!("read intent file {path:?}: {err}"))
            }
        }
        (None, None) => anyhow::bail!("one of --intent or --intent-file is required"),
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

fn put<T: Into<Value>>(body: &mut Value, key: &str, value: Option<T>) {
    if let Some(value) = value
        && let Some(object) = body.as_object_mut()
    {
        object.insert(key.into(), value.into());
    }
}
