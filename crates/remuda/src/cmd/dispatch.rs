//! `remuda dispatch <task-id|--brief FILE> --project P` — provision and launch
//! one worker; M1 batch 5a.
//!
//! The Hub assigns name, branch (`wt/<name>/<slug>`), worktree, target dir and
//! port block; this verb only names intent (project, harness, model, host).
//! The brief is read from a file and linted locally before upload, so a bad
//! brief never reaches the Hub (and never rides an inline prompt).

use clap::Args;
use serde_json::json;

use super::brief::lint_brief;
use super::hub_client::{HubOpts, block_on, hub_http_error, print_json};
use super::registry::Entrypoint;

/// `remuda dispatch` arguments.
#[derive(Args)]
#[command(about = "Provision and launch a worker for a task or brief file.")]
pub(crate) struct DispatchArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Owning project `prj_…`.
    #[arg(long)]
    project: String,
    /// Brief file (utf8 markdown). Exactly one of <task-id> / --brief.
    #[arg(long)]
    brief: Option<String>,
    /// Bound task `tsk_…`. The task title becomes the branch slug and its
    /// brief (when present) can supply the brief file via --brief too.
    #[arg(value_name = "TASK_ID")]
    task: Option<String>,
    /// Harness: claude (default), codex, grok.
    #[arg(long)]
    harness: Option<String>,
    /// Explicit model id (pins supply admission).
    #[arg(long)]
    model: Option<String>,
    /// Explicit worker name (one safe lowercase segment).
    #[arg(long)]
    name: Option<String>,
    /// Place on this host (`hst_…`).
    #[arg(long)]
    host: Option<String>,
    /// Placement tendency: `local` or `remote` (project member latencyClass).
    #[arg(long)]
    placement: Option<String>,
    /// Carrier driver override (`shell-pty` / `claude-pty` / `claude-print`).
    /// Honoured verbatim or refused with a reason — never silently replaced.
    /// The default prefers the Node's native `shell-pty` carrier (screen-readable
    /// by `remuda watch`), then herdr's `claude-pty`; `claude-print` is never a
    /// default because print exits after one turn (D-028).
    #[arg(long)]
    driver: Option<String>,
    /// Carrier preference (batch 6): `native` (shell-pty), `herdr`, `print`.
    /// Default is native when the host Node advertises it, else herdr;
    /// `print` is used only on this explicit request.
    #[arg(long)]
    carrier: Option<String>,
    /// Skip the local brief lint gate (not recommended; gate still scans).
    #[arg(long)]
    force_lint: bool,
    /// Per-launch host capability grant (repeatable). Only `computer-use`
    /// exists; it targets a macOS host reporting the installed capability and
    /// runs with host-side approvals instead of the dispatcher's bypass default
    /// (D-045).
    #[arg(long = "capability")]
    capabilities: Vec<String>,
}

impl Entrypoint for DispatchArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }
}

async fn run(args: DispatchArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    super::capability::validate_requested(&args.capabilities)?;
    if super::capability::requests_computer_use(&args.capabilities)
        && !matches!(
            args.harness.as_deref().unwrap_or("claude"),
            "claude" | "codex"
        )
    {
        anyhow::bail!(
            "the \"computer-use\" capability is not supported for harness {:?} this batch; \
             supported harnesses are claude and codex",
            args.harness.as_deref().unwrap_or("claude")
        );
    }
    if args.task.is_none() && args.brief.is_none() {
        anyhow::bail!("dispatch needs a <task-id> or --brief FILE");
    }
    let brief_path = args.brief.clone().ok_or_else(|| {
        anyhow::anyhow!("--brief FILE is required (briefs always travel as files, never inline)")
    })?;
    let content = std::fs::read_to_string(&brief_path)
        .map_err(|err| anyhow::anyhow!("read brief {brief_path:?}: {err}"))?;
    let violations = lint_brief(&content);
    if !violations.is_empty() {
        if !args.force_lint {
            for violation in &violations {
                eprintln!(
                    "{}:{}: {}",
                    violation.rule, violation.line, violation.message
                );
            }
            anyhow::bail!("brief failed lint; fix it or pass --force-lint");
        }
        eprintln!(
            "warning: dispatching despite {} brief lint violations",
            violations.len()
        );
    }
    let brief_name = std::path::Path::new(&brief_path)
        .file_name()
        .map(|stem| stem.to_string_lossy().into_owned());
    // D-045 gate 2 at the CLI when the host is explicit; placement-resolved
    // dispatch is preflighted by the Hub after it picks the host.
    if super::capability::requests_computer_use(&args.capabilities)
        && let Some(host_id) = args.host.as_deref()
    {
        let host = client
            .get(&format!("/v1/hosts/{host_id}"))
            .await
            .map_err(super::hub_client::hub_http_error)?;
        super::capability::host_supports_computer_use(&host)?;
    }
    let body = json!({
        "projectId": args.project,
        "brief": content,
        "briefName": brief_name,
        "taskId": args.task,
        "harness": args.harness,
        "model": args.model,
        "name": args.name,
        "hostId": args.host,
        "placement": args.placement,
        "driver": args.driver,
        "carrier": args.carrier,
        "capabilities": args.capabilities,
    });
    let value = client
        .post("/v1/workers/dispatch", &body)
        .await
        .map_err(hub_http_error)?;
    print_json(&value)?;
    Ok(0)
}
