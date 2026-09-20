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
    /// D-047 per-dispatch model-API delivery override: a proxy host id
    /// (`hst_…`), `self` (the Hub host), or `none` (force direct for this one
    /// dispatch). A target the Hub cannot honour is a non-zero refusal, never
    /// a silent reroute.
    #[arg(long = "api-via", value_name = "HOST_ID|self|none")]
    api_via: Option<String>,
    /// D-047 route between the worker and the proxy host: `auto` (default),
    /// `hub-relay` (always in-band), or `direct-net` (refuse if unreachable).
    /// Requires `--api-via`.
    #[arg(long = "api-route")]
    api_route: Option<String>,
}

impl Entrypoint for DispatchArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }
}

async fn run(args: DispatchArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    super::capability::validate_requested(&args.capabilities)?;
    if super::capability::requests_computer_use(&args.capabilities) {
        // D-045 Q4: dispatch workers are bot-originated and run unattended
        // (every harness); desktop control without a human approving each
        // action is refused for all of them, never silently downgraded.
        anyhow::bail!(
            "refusing --capability computer-use on dispatch for harness {:?}: dispatched \
             workers run unattended, and desktop control without per-action human approvals \
             has no recovery path; use `remuda instance create --capability computer-use` \
             for an attended launch",
            args.harness.as_deref().unwrap_or("claude")
        );
    }
    if args.task.is_none() && args.brief.is_none() {
        anyhow::bail!("dispatch needs a <task-id> or --brief FILE");
    }
    // D-047: validate the `--api-via` / `--api-route` pair before reading the
    // brief or posting anything, so a typo fails here with the same vocabulary
    // the Hub refuses with instead of reading as "no override". The Hub
    // re-validates; these parses only mirror its rules.
    if let Some(raw) = args.api_via.as_deref() {
        remuda_protocol::ApiViaOverride::parse(raw)
            .map_err(|err| anyhow::anyhow!("--api-via {raw:?}: {err}"))?;
    }
    if let Some(raw) = args.api_route.as_deref() {
        if args.api_via.is_none() {
            anyhow::bail!("--api-route requires --api-via to name a proxy host");
        }
        parse_route_mode(raw)?;
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
    // D-045: dispatch refuses every computer-use grant above, so no host
    // preflight is reachable here; the Hub repeats the refusal server-side.
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
        "apiVia": args.api_via,
        "apiRoute": args.api_route,
    });
    let value = client
        .post("/v1/workers/dispatch", &body)
        .await
        .map_err(hub_http_error)?;
    print_json(&value)?;
    Ok(0)
}

/// Parse the D-047 route sub-mode spelling shared with `remuda profile`.
pub(super) fn parse_route_mode(raw: &str) -> anyhow::Result<remuda_protocol::ApiRouteMode> {
    serde_json::from_value::<remuda_protocol::ApiRouteMode>(json!(raw))
        .map_err(|err| anyhow::anyhow!("--api-route must be auto, hub-relay or direct-net: {err}"))
}
