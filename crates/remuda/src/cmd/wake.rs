//! `remuda wake add|list|rm` — durable wake-ups (D-060 §11, cb-wake-engine).
//!
//! A lineage manages its own wake-ups; the owner's device manages any
//! lineage's. With no `--lineage`, an agent session uses its own lineage
//! (named by `/v1/caller`). The Hub claims each due occurrence once and
//! delivers it through the inbox (`persistent`, the default) or creates a
//! short-lived child for it (`fresh`). `--notify` is required: the Hub never
//! picks whether a run pushes to the owner. `add --name` is idempotent: the
//! same add under the same name returns the stored wake-up.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubOpts, block_on, hub_http_error, print_json};

/// `remuda wake` subcommands.
#[derive(Args)]
#[command(about = "Wake a lineage at a time, every interval, or when a fact arrives.")]
pub(crate) struct WakeArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: WakeCommand,
}

#[derive(Subcommand)]
enum WakeCommand {
    /// Add a wake-up.
    Add(Box<AddArgs>),
    /// List the lineage's wake-ups and its latest runs.
    List(ListArgs),
    /// Remove a wake-up by id or name (a run already due still ends).
    Rm(RmArgs),
}

#[derive(Args)]
struct LineageArg {
    /// Lineage id (the first chapter's `ins_…`). Defaults to the caller's
    /// own lineage inside an agent session.
    #[arg(long)]
    lineage: Option<String>,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("trigger").required(true)
    .args(["at", "in_", "every", "when"])))]
struct AddArgs {
    #[command(flatten)]
    lineage: LineageArg,
    /// Unique in the lineage; repeating the same add under it returns the
    /// stored wake-up (at most 64 of A-Z a-z 0-9 . _ : / -).
    #[arg(long)]
    name: Option<String>,
    /// What the wake-up is for (at most 2000 characters).
    #[arg(long, group = "text-source")]
    text: Option<String>,
    /// Read the text from a file (`-` = stdin).
    #[arg(long, group = "text-source", value_name = "FILE")]
    text_file: Option<String>,
    /// Fire once at this time (RFC3339).
    #[arg(long)]
    at: Option<String>,
    /// Fire once after this long (`90s`, `2m`, `1h`, `1d`, or seconds).
    #[arg(long = "in", value_name = "DURATION")]
    in_: Option<String>,
    /// Fire at every interval (at least `1m`).
    #[arg(long, value_name = "DURATION")]
    every: Option<String>,
    /// With --every: the first occurrence (RFC3339); one interval from now
    /// when absent.
    #[arg(long, requires = "every")]
    start: Option<String>,
    /// Fire each time a fact of this kind reaches the lineage's inbox
    /// (`turn-done`, `gate`, `interaction`, …).
    #[arg(long, value_name = "FACT_KIND")]
    when: Option<String>,
    /// With --when: only facts about this instance.
    #[arg(long, requires = "when")]
    subject: Option<String>,
    /// `persistent` (the live chapter takes the run) or `fresh` (a
    /// short-lived child does).
    #[arg(long, default_value = "persistent")]
    mode: String,
    /// Which runs notify the owner: `never`, `failure` or `always`.
    #[arg(long)]
    notify: String,
    /// A shell check deciding whether the run happens (not supported yet;
    /// the Hub refuses it).
    #[arg(long)]
    check: Option<String>,
    /// Print the Hub's JSON response instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListArgs {
    #[command(flatten)]
    lineage: LineageArg,
    /// Print the Hub's JSON response instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct RmArgs {
    #[command(flatten)]
    lineage: LineageArg,
    /// The wake-up's id (`wak_…`) or name.
    wake: String,
}

impl super::registry::Entrypoint for WakeArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await }).map(|()| 0)
    }

    fn tracing(&self) -> bool {
        false
    }
}

async fn lineage_of(
    client: &remuda_hub_client::HubClient,
    lineage: Option<String>,
) -> anyhow::Result<String> {
    if let Some(lineage) = lineage {
        return Ok(lineage);
    }
    let caller = client.get("/v1/caller").await.map_err(hub_http_error)?;
    match caller.get("lineageId").and_then(Value::as_str) {
        Some(lineage) => Ok(lineage.to_owned()),
        None => {
            bail!("no lineage: this credential is not bound to an agent instance; pass --lineage")
        }
    }
}

async fn run(args: WakeArgs) -> anyhow::Result<()> {
    let client = args.hub.connect()?;
    match args.command {
        WakeCommand::Add(add) => {
            let body = add_body(&add)?;
            let lineage = lineage_of(&client, add.lineage.lineage.clone()).await?;
            let value = client
                .post(&format!("/v1/lineages/{lineage}/wake-ups"), &body)
                .await
                .map_err(hub_http_error)?;
            if add.json {
                return print_json(&value);
            }
            println!("{}", render_added(&value));
            Ok(())
        }
        WakeCommand::List(list) => {
            let lineage = lineage_of(&client, list.lineage.lineage).await?;
            let value = client
                .get(&format!("/v1/lineages/{lineage}/wake-ups"))
                .await
                .map_err(hub_http_error)?;
            if list.json {
                return print_json(&value);
            }
            println!("{}", render_list(&value));
            Ok(())
        }
        WakeCommand::Rm(rm) => {
            let lineage = lineage_of(&client, rm.lineage.lineage).await?;
            let value = client
                .delete(
                    &format!("/v1/lineages/{lineage}/wake-ups/{}", rm.wake),
                    &Value::Null,
                )
                .await
                .map_err(hub_http_error)?;
            println!(
                "removed {} ({})",
                value["wakeId"].as_str().unwrap_or("?"),
                value["name"].as_str().unwrap_or("?")
            );
            Ok(())
        }
    }
}

/// `90s`, `2m`, `1h`, `1d` or plain seconds, as seconds.
fn parse_duration(raw: &str) -> anyhow::Result<i64> {
    let raw = raw.trim();
    let (number, unit) = match raw.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => raw.split_at(index),
        None => (raw, "s"),
    };
    let value: i64 = number
        .parse()
        .with_context(|| format!("not a duration: {raw:?}"))?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => bail!("unknown duration unit {other:?} in {raw:?}; use s, m, h or d"),
    };
    value
        .checked_mul(scale)
        .with_context(|| format!("duration {raw:?} is too long"))
}

fn add_body(args: &AddArgs) -> anyhow::Result<Value> {
    let text = match (&args.text, &args.text_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) if path == "-" => std::io::read_to_string(std::io::stdin())?,
        (None, Some(path)) => std::fs::read_to_string(path)?,
        (None, None) => bail!("give the wake-up's text with --text or --text-file"),
    };
    let trigger = if let Some(at) = &args.at {
        json!({ "kind": "at", "at": at })
    } else if let Some(after) = &args.in_ {
        json!({ "kind": "at", "afterSecs": parse_duration(after)? })
    } else if let Some(every) = &args.every {
        let mut trigger = json!({ "kind": "every", "everySecs": parse_duration(every)? });
        if let Some(start) = &args.start {
            trigger["startAt"] = json!(start);
        }
        trigger
    } else if let Some(kind) = &args.when {
        let mut trigger = json!({ "kind": "when", "factKind": kind });
        if let Some(subject) = &args.subject {
            trigger["subjectInstanceId"] = json!(subject);
        }
        trigger
    } else {
        bail!("give one trigger: --at, --in, --every or --when");
    };
    let mut body = json!({
        "text": text,
        "trigger": trigger,
        "mode": args.mode,
        "notify": args.notify,
    });
    if let Some(name) = &args.name {
        body["name"] = json!(name);
    }
    if let Some(check) = &args.check {
        body["check"] = json!({ "command": check });
    }
    Ok(body)
}

fn render_added(value: &Value) -> String {
    let wake = &value["wakeUp"];
    let id = wake["wakeId"].as_str().unwrap_or("?");
    let name = wake["name"].as_str().unwrap_or("?");
    let when = match wake["nextFireAt"].as_str() {
        Some(at) => format!("next at {at}"),
        None => "fires when its fact arrives".to_owned(),
    };
    if value.get("created").and_then(Value::as_bool) == Some(false) {
        format!("already added: {id} ({name}), {when}")
    } else {
        format!("added {id} ({name}), {when}")
    }
}

fn trigger_label(trigger: &Value) -> String {
    match trigger["kind"].as_str() {
        Some("at") => format!("at {}", trigger["at"].as_str().unwrap_or("?")),
        Some("every") => format!("every {}s", trigger["everySecs"].as_i64().unwrap_or(0)),
        Some("when") => format!("when {}", trigger["factKind"].as_str().unwrap_or("?")),
        _ => "?".to_owned(),
    }
}

/// One line per wake-up, then one per recent run.
fn render_list(value: &Value) -> String {
    let mut lines = Vec::new();
    let items = value["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        lines.push("no wake-ups".to_owned());
    }
    for wake in &items {
        lines.push(format!(
            "{} {} [{}] {} mode={} notify={} next={} missed={}",
            wake["wakeId"].as_str().unwrap_or("?"),
            wake["name"].as_str().unwrap_or("?"),
            wake["state"].as_str().unwrap_or("?"),
            trigger_label(&wake["trigger"]),
            wake["mode"].as_str().unwrap_or("?"),
            wake["notify"].as_str().unwrap_or("?"),
            wake["nextFireAt"].as_str().unwrap_or("-"),
            wake["missedCount"].as_i64().unwrap_or(0),
        ));
    }
    let runs = value["runs"].as_array().cloned().unwrap_or_default();
    if !runs.is_empty() {
        lines.push("runs (newest first):".to_owned());
    }
    for run in &runs {
        let mut line = format!(
            "  {} {} {} fired={} missed={}",
            run["runId"].as_str().unwrap_or("?"),
            run["name"].as_str().unwrap_or("?"),
            run["state"].as_str().unwrap_or("?"),
            run["firedAt"].as_str().unwrap_or("?"),
            run["missedBefore"].as_i64().unwrap_or(0),
        );
        if let Some(kind) = run["failureKind"].as_str() {
            line.push_str(&format!(" failure={kind}"));
        }
        if let Some(child) = run["childInstanceId"].as_str() {
            line.push_str(&format!(" child={child}"));
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        wake: WakeArgs,
    }

    fn add(argv: &[&str]) -> anyhow::Result<Value> {
        let mut full = vec!["wake", "add"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(full)?;
        let WakeCommand::Add(args) = cli.wake.command else {
            bail!("not an add");
        };
        add_body(&args)
    }

    #[test]
    fn add_sends_the_trigger_mode_notify_and_name() {
        assert_eq!(
            add(&[
                "--name", "standup", "--text", "Post it.", "--in", "2m", "--notify", "never"
            ])
            .unwrap(),
            json!({"name": "standup", "text": "Post it.", "mode": "persistent", "notify": "never",
                   "trigger": {"kind": "at", "afterSecs": 120}})
        );
        assert_eq!(
            add(&[
                "--text", "t", "--every", "3m", "--notify", "failure", "--mode", "fresh"
            ])
            .unwrap()["trigger"],
            json!({"kind": "every", "everySecs": 180})
        );
        assert_eq!(
            add(&[
                "--text",
                "t",
                "--when",
                "gate",
                "--subject",
                "ins_1",
                "--notify",
                "always"
            ])
            .unwrap()["trigger"],
            json!({"kind": "when", "factKind": "gate", "subjectInstanceId": "ins_1"})
        );
        assert_eq!(
            add(&[
                "--text",
                "t",
                "--at",
                "2026-11-04T10:00:00Z",
                "--notify",
                "never",
                "--check",
                "true"
            ])
            .unwrap()["check"],
            json!({"command": "true"})
        );
    }

    #[test]
    fn add_needs_exactly_one_trigger_and_a_declared_notify() {
        assert!(
            add(&["--text", "t", "--notify", "never"]).is_err(),
            "no trigger"
        );
        assert!(
            add(&[
                "--text", "t", "--in", "1m", "--every", "1m", "--notify", "never"
            ])
            .is_err(),
            "two triggers"
        );
        assert!(
            add(&["--text", "t", "--in", "1m"]).is_err(),
            "notify is required"
        );
        assert!(add(&["--text", "t", "--in", "1x", "--notify", "never"]).is_err());
    }

    #[test]
    fn durations_parse_with_units_and_never_overflow() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("2m").unwrap(), 120);
        assert_eq!(parse_duration("1h").unwrap(), 3600);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
        assert!(parse_duration(&format!("{}d", i64::MAX)).is_err());
        assert!(parse_duration("m").is_err());
    }

    #[test]
    fn a_repeated_add_says_so() {
        let value = json!({"created": false, "wakeUp": {"wakeId": "wak_1", "name": "standup",
                                                        "nextFireAt": "2026-11-04T10:00:00.000Z"}});
        assert_eq!(
            render_added(&value),
            "already added: wak_1 (standup), next at 2026-11-04T10:00:00.000Z"
        );
        let list = json!({"items": [{"wakeId": "wak_1", "name": "beat", "state": "active",
            "trigger": {"kind": "every", "everySecs": 180}, "mode": "persistent",
            "notify": "failure", "nextFireAt": "2026-11-04T10:03:00.000Z", "missedCount": 2}],
            "runs": [{"runId": "wrn_1", "name": "beat", "state": "failed",
                      "firedAt": "2026-11-04T10:00:00.000Z", "missedBefore": 0,
                      "failureKind": "turn-failed"}]});
        let rendered = render_list(&list);
        assert!(rendered.contains("wak_1 beat [active] every 180s mode=persistent notify=failure"));
        assert!(rendered.contains("wrn_1 beat failed") && rendered.contains("failure=turn-failed"));
    }
}
