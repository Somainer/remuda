//! `remuda owner notify` — tell the owner something on their phone (D-060
//! §11.5, cb-owner-notify).
//!
//! The seat holding `address-owner` (Main) runs it with its own credential;
//! the owner's own device may too. Anyone else is refused by the Hub with a
//! reason and a next step. The Hub writes a claim row first, then pushes; a
//! repeat with the same `--dedupe-key` returns the original notice and pushes
//! nothing, so a retried call never notifies twice. A credential-shaped title,
//! body or key is refused (422) and never echoed. The PWA shows the notice as
//! a 「告诉你」 card until the owner acknowledges it.

use clap::{Args, Subcommand};
use serde_json::{Value, json};

use super::hub_client::{HubOpts, block_on, hub_http_error, print_json};

/// `remuda owner` subcommands.
#[derive(Args)]
#[command(about = "Address the owner directly (the address-owner seat or the owner's device).")]
pub(crate) struct OwnerArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: OwnerCommand,
}

#[derive(Subcommand)]
enum OwnerCommand {
    /// Push a notice to the owner and show it as a 「告诉你」 card.
    Notify(NotifyArgs),
}

#[derive(Args)]
struct NotifyArgs {
    /// One-line title (at most 120 characters).
    #[arg(long)]
    title: String,
    /// The notice text (at most 2000 characters).
    #[arg(long, group = "body-source")]
    body: Option<String>,
    /// Read the body from a file (`-` = stdin).
    #[arg(long, group = "body-source", value_name = "FILE")]
    body_file: Option<String>,
    /// A repeat with the same key returns the first notice and pushes
    /// nothing (at most 128 of A-Z a-z 0-9 . _ : / -).
    #[arg(long)]
    dedupe_key: Option<String>,
    /// Print the Hub's JSON response instead of text.
    #[arg(long)]
    json: bool,
}

impl super::registry::Entrypoint for OwnerArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await }).map(|()| 0)
    }

    fn tracing(&self) -> bool {
        false
    }
}

async fn run(args: OwnerArgs) -> anyhow::Result<()> {
    let client = args.hub.connect()?;
    match args.command {
        OwnerCommand::Notify(notify) => {
            let body = notify_body(&notify)?;
            let value = client
                .post("/v1/owner/notify", &body)
                .await
                .map_err(hub_http_error)?;
            if notify.json {
                return print_json(&value);
            }
            println!("{}", render(&value));
            Ok(())
        }
    }
}

fn notify_body(args: &NotifyArgs) -> anyhow::Result<Value> {
    let body = match (&args.body, &args.body_file) {
        (Some(body), _) => body.clone(),
        (None, Some(path)) if path == "-" => std::io::read_to_string(std::io::stdin())?,
        (None, Some(path)) => std::fs::read_to_string(path)?,
        (None, None) => anyhow::bail!("give the notice text with --body or --body-file"),
    };
    let mut value = json!({ "title": args.title, "body": body });
    if let Some(key) = &args.dedupe_key {
        value["dedupeKey"] = json!(key);
    }
    Ok(value)
}

/// One line: the notice id, and whether this call claimed it or found the
/// notice an earlier call with the same dedupe key wrote.
fn render(value: &Value) -> String {
    let id = value
        .pointer("/notice/noticeId")
        .and_then(Value::as_str)
        .unwrap_or("?");
    if value.get("created").and_then(Value::as_bool) == Some(false) {
        format!("already notified: {id} (same dedupe key; nothing pushed)")
    } else {
        format!("notified: {id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        owner: OwnerArgs,
    }

    #[test]
    fn notify_sends_title_body_and_key() {
        let cli = Cli::try_parse_from([
            "owner",
            "notify",
            "--title",
            "Gate red",
            "--body",
            "Lane 2 failed.",
            "--dedupe-key",
            "gate:nightly",
        ])
        .expect("parse");
        let OwnerCommand::Notify(args) = cli.owner.command;
        assert_eq!(
            notify_body(&args).expect("body"),
            json!({"title": "Gate red", "body": "Lane 2 failed.", "dedupeKey": "gate:nightly"})
        );
        assert!(
            Cli::try_parse_from([
                "owner",
                "notify",
                "--title",
                "t",
                "--body",
                "b",
                "--body-file",
                "f"
            ])
            .is_err(),
            "one body source"
        );
    }

    #[test]
    fn a_dedupe_hit_says_nothing_was_pushed() {
        assert_eq!(
            render(&json!({"notice": {"noticeId": "ntc_1"}, "created": true})),
            "notified: ntc_1"
        );
        assert_eq!(
            render(&json!({"notice": {"noticeId": "ntc_1"}, "created": false})),
            "already notified: ntc_1 (same dedupe key; nothing pushed)"
        );
    }
}
