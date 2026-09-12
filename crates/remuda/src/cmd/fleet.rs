//! `remuda fleet run|send` — Hub fleet HTTP (`docs/design/proposal.md` §4.6).

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, print_json};
use super::instance::encode_keys;

/// `remuda fleet` subcommands.
#[derive(clap::Args)]
#[command(about = "Run a spec on many hosts, or broadcast prompts and keys.")]
pub(crate) struct Args {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    pub(crate) command: FleetCommand,
}

impl super::registry::Entrypoint for Args {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        run(self.hub, self.command).map(|()| 0)
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum FleetCommand {
    /// Create one instance per selected host and return `fleetId`.
    Run {
        /// Explicit host ids (`hst_…`, comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        hosts: Vec<String>,
        /// Placement labels (`key=value`, comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Maximum number of instances (defaults to `hosts.len()` when set).
        #[arg(long)]
        max: Option<u32>,
        /// Agent kind.
        #[arg(long, default_value = "claude")]
        kind: String,
        /// Driver kind.
        #[arg(long, default_value = "claude-print")]
        driver: String,
        /// Optional workspace id.
        #[arg(long)]
        workspace_id: Option<String>,
        /// UI title.
        #[arg(long)]
        title: Option<String>,
        /// Initial prompt.
        #[arg(long)]
        prompt: Option<String>,
    },
    /// Broadcast a prompt to running instances (`--all` or a filter).
    Send {
        /// Every running instance.
        #[arg(long)]
        all: bool,
        /// Explicit Human/Bot confirmation for --all.
        #[arg(long)]
        confirm: bool,
        /// Instances whose host matches these labels.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Instances on these hosts (`hst_…`, comma-separated or repeated).
        #[arg(long = "host", value_delimiter = ',')]
        hosts: Vec<String>,
        /// Instances of these agent kinds (`claude`, `codex`, …).
        #[arg(long = "kind", value_delimiter = ',')]
        kinds: Vec<String>,
        /// Replay guard: retrying the same key will not double-send.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Read the prompt from a file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Prompt text after the flags.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Broadcast logical keys to running instances (`--all` or a filter).
    Keys {
        /// Every running instance.
        #[arg(long)]
        all: bool,
        /// Explicit Human/Bot confirmation for --all.
        #[arg(long)]
        confirm: bool,
        /// Instances whose host matches these labels.
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Instances on these hosts (`hst_…`, comma-separated or repeated).
        #[arg(long = "host", value_delimiter = ',')]
        hosts: Vec<String>,
        /// Instances of these agent kinds (`claude`, `codex`, …).
        #[arg(long = "kind", value_delimiter = ',')]
        kinds: Vec<String>,
        /// Replay guard: retrying the same key will not double-send.
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Logical keys. Validated before any bytes are sent.
        #[arg(required = true, num_args = 1.., allow_hyphen_values = true)]
        keys: Vec<String>,
    },
}

/// Inputs for [`fleet_run`].
#[derive(Debug, Clone)]
pub(crate) struct FleetRunOpts {
    pub hosts: Vec<String>,
    pub labels: Vec<String>,
    pub max: Option<u32>,
    pub kind: String,
    pub driver: String,
    pub workspace_id: Option<String>,
    pub title: Option<String>,
    pub prompt: Option<String>,
}

/// Run a `remuda fleet` subcommand.
pub(crate) fn run(hub: HubOpts, command: FleetCommand) -> Result<()> {
    block_on(async move {
        let client = hub.connect()?;
        match command {
            FleetCommand::Run {
                hosts,
                labels,
                max,
                kind,
                driver,
                workspace_id,
                title,
                prompt,
            } => {
                let value = fleet_run(
                    &client,
                    FleetRunOpts {
                        hosts,
                        labels,
                        max,
                        kind,
                        driver,
                        workspace_id,
                        title,
                        prompt,
                    },
                )
                .await?;
                print_json(&value)
            }
            FleetCommand::Send {
                all,
                confirm,
                labels,
                hosts,
                kinds,
                idempotency_key,
                file,
                text,
            } => {
                let prompt = load_fleet_text(file, text)?;
                let value = fleet_send_opts(
                    &client,
                    FleetSendOpts {
                        filter: FleetFilter {
                            all,
                            confirm,
                            labels,
                            hosts,
                            kinds,
                        },
                        idempotency_key,
                        text: prompt,
                    },
                )
                .await?;
                print_json(&value)
            }
            FleetCommand::Keys {
                all,
                confirm,
                labels,
                hosts,
                kinds,
                idempotency_key,
                keys,
            } => {
                let value = fleet_keys(
                    &client,
                    FleetKeysOpts {
                        filter: FleetFilter {
                            all,
                            confirm,
                            labels,
                            hosts,
                            kinds,
                        },
                        idempotency_key,
                        keys,
                    },
                )
                .await?;
                print_json(&value)
            }
        }
    })
}

pub(crate) async fn fleet_run(client: &HubClient, opts: FleetRunOpts) -> Result<Value> {
    if !opts.hosts.is_empty() && !opts.labels.is_empty() {
        bail!("use --hosts or --labels, not both");
    }
    let mut spec = json!({
        "kind": opts.kind,
        "driver": opts.driver,
    });
    if let Some(workspace_id) = &opts.workspace_id {
        spec["workspaceId"] = json!(workspace_id);
    }
    if let Some(title) = &opts.title {
        spec["title"] = json!(title);
    }
    if let Some(prompt) = &opts.prompt {
        spec["prompt"] = json!(prompt);
    }
    if opts.hosts.len() == 1 {
        spec["placement"] = json!({ "host": opts.hosts[0] });
    } else if !opts.labels.is_empty() {
        spec["placement"] = json!({ "labels": opts.labels });
    } else if opts.hosts.is_empty() {
        spec["placement"] = json!({ "kind": "any" });
    }

    let mut body = json!({ "spec": spec });
    if !opts.hosts.is_empty() {
        body["hosts"] = json!(opts.hosts);
    }
    if !opts.labels.is_empty() {
        body["labels"] = json!(opts.labels);
    }
    let max = opts
        .max
        .or((!opts.hosts.is_empty()).then_some(opts.hosts.len() as u32));
    if let Some(max) = max {
        body["max"] = json!(max);
    }

    Ok(client.create_fleet(&body).await?)
}

/// Instance selection shared by `fleet send` and `fleet keys`.
#[derive(Debug, Clone, Default)]
pub(crate) struct FleetFilter {
    /// Every running instance.
    pub all: bool,
    /// Explicit Human/Bot confirmation for all-target sends and keys.
    pub confirm: bool,
    /// Host labels (`key=value`).
    pub labels: Vec<String>,
    /// Explicit host ids.
    pub hosts: Vec<String>,
    /// Agent kinds.
    pub kinds: Vec<String>,
}

impl FleetFilter {
    /// True when at least one narrowing filter is set.
    fn is_narrowed(&self) -> bool {
        !self.labels.is_empty() || !self.hosts.is_empty() || !self.kinds.is_empty()
    }

    /// Reject the empty selection; `--all` and a filter may be combined.
    fn validate(&self) -> Result<()> {
        if !self.all && !self.is_narrowed() {
            bail!("provide --all or one of --labels / --host / --kind");
        }
        Ok(())
    }

    /// Filter half of the `/v1/fleet/broadcast` body.
    fn to_body(&self) -> Value {
        let mut body = json!({ "all": self.all, "confirm": self.confirm });
        if !self.hosts.is_empty() {
            body["hosts"] = json!(self.hosts);
        }
        if !self.labels.is_empty() {
            body["labels"] = json!(self.labels);
        }
        if !self.kinds.is_empty() {
            body["kinds"] = json!(self.kinds);
        }
        body
    }
}

/// Inputs for [`fleet_send_opts`].
#[derive(Debug, Clone)]
pub(crate) struct FleetSendOpts {
    pub filter: FleetFilter,
    pub idempotency_key: Option<String>,
    pub text: String,
}

/// Inputs for [`fleet_keys`].
#[derive(Debug, Clone)]
pub(crate) struct FleetKeysOpts {
    pub filter: FleetFilter,
    pub idempotency_key: Option<String>,
    pub keys: Vec<String>,
}

pub(crate) async fn fleet_send_opts(client: &HubClient, opts: FleetSendOpts) -> Result<Value> {
    opts.filter.validate()?;
    if opts.text.is_empty() {
        bail!("provide a prompt or --file");
    }
    client
        .caller_context()
        .await?
        .check_fleet_all(opts.filter.all, opts.filter.confirm)?;
    let mut body = opts.filter.to_body();
    body["operation"] = json!("instance.send");
    body["payload"] = json!({
        "input": {
            "type": "prompt",
            "mode": "new-turn",
            "blocks": [{ "type": "text", "text": opts.text }],
            "origin": "fleet",
        },
        "completionScope": "native-turn",
    });
    if let Some(key) = opts.idempotency_key.as_deref().filter(|s| !s.is_empty()) {
        body["idempotencyKey"] = json!(key);
    }
    let mut value = client.fleet_broadcast(&body).await?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("text".into(), json!(opts.text));
    }
    Ok(value)
}

pub(crate) async fn fleet_keys(client: &HubClient, opts: FleetKeysOpts) -> Result<Value> {
    opts.filter.validate()?;
    // Validate every key name before any bytes reach the Hub.
    let encoded = encode_keys(&opts.keys)?;
    client
        .caller_context()
        .await?
        .check_fleet_all(opts.filter.all, opts.filter.confirm)?;
    let mut body = opts.filter.to_body();
    body["operation"] = json!("tty.write");
    body["payload"] = json!({
        "keys": encoded.names,
        "dataBase64": encoded.data_base64,
        "source": "fleet",
    });
    if let Some(key) = opts.idempotency_key.as_deref().filter(|s| !s.is_empty()) {
        body["idempotencyKey"] = json!(key);
    }
    let mut value = client.fleet_broadcast(&body).await?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("keys".into(), json!(encoded.names));
        obj.insert(
            "driverHint".into(),
            json!("tty.write is delivered to a tty-attach driver (generic-pty / claude-pty)"),
        );
    }
    Ok(value)
}

fn load_fleet_text(file: Option<PathBuf>, text: Vec<String>) -> Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(&path)
            .map_err(|err| anyhow::anyhow!("read {}: {err}", path.display()));
    }
    if text.is_empty() {
        bail!("provide a prompt or --file");
    }
    Ok(text.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::hub_client::connect_for_test;
    use crate::cmd::test_hub::spawn_mock_hub;

    fn filter(all: bool) -> FleetFilter {
        FleetFilter {
            all,
            confirm: true,
            ..Default::default()
        }
    }

    #[test]
    fn empty_selection_is_rejected() {
        let err = filter(false).validate().expect_err("no selection");
        assert!(err.to_string().contains("--all"), "{err}");
    }

    #[test]
    fn any_single_filter_selects_without_all() {
        for narrowed in [
            FleetFilter {
                labels: vec!["region=sg".into()],
                ..Default::default()
            },
            FleetFilter {
                hosts: vec!["hst_1".into()],
                ..Default::default()
            },
            FleetFilter {
                kinds: vec!["codex".into()],
                ..Default::default()
            },
        ] {
            narrowed.validate().expect("narrowed filter is a selection");
        }
        filter(true).validate().expect("--all is a selection");
    }

    #[test]
    fn filter_body_omits_unset_keys() {
        let body = filter(true).to_body();
        assert_eq!(body["all"], json!(true));
        assert!(body.get("hosts").is_none(), "{body}");
        assert!(body.get("labels").is_none(), "{body}");
        assert!(body.get("kinds").is_none(), "{body}");

        let body = FleetFilter {
            all: false,
            confirm: false,
            labels: vec!["region=sg".into()],
            hosts: vec!["hst_1".into()],
            kinds: vec!["claude".into(), "codex".into()],
        }
        .to_body();
        assert_eq!(body["all"], json!(false));
        assert_eq!(body["hosts"], json!(["hst_1"]));
        assert_eq!(body["labels"], json!(["region=sg"]));
        assert_eq!(body["kinds"], json!(["claude", "codex"]));
    }

    #[tokio::test]
    async fn send_posts_prompt_and_echoes_text() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let value = fleet_send_opts(
            &client,
            FleetSendOpts {
                filter: filter(true),
                idempotency_key: Some("pause-1".into()),
                text: "PAUSE git commits".into(),
            },
        )
        .await
        .expect("fleet send");
        assert_eq!(value["operation"], json!("instance.send"));
        assert_eq!(value["accepted"], json!(1));
        assert_eq!(value["failed"], json!(0));
        assert_eq!(value["text"], json!("PAUSE git commits"));
        assert_eq!(value["results"][0]["ok"], json!(true));
    }

    #[tokio::test]
    async fn send_requires_text() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let err = fleet_send_opts(
            &client,
            FleetSendOpts {
                filter: filter(true),
                idempotency_key: None,
                text: String::new(),
            },
        )
        .await
        .expect_err("empty prompt");
        assert!(err.to_string().contains("prompt"), "{err}");
    }

    #[tokio::test]
    async fn keys_broadcast_uses_tty_write() {
        let mock = spawn_mock_hub().await;
        let client = connect_for_test(format!("http://{}", mock.addr), "t".into()).expect("client");
        let value = fleet_keys(
            &client,
            FleetKeysOpts {
                filter: FleetFilter {
                    all: true,
                    confirm: true,
                    kinds: vec!["claude".into()],
                    ..Default::default()
                },
                idempotency_key: None,
                keys: vec!["enter".into()],
            },
        )
        .await
        .expect("fleet keys");
        assert_eq!(value["operation"], json!("tty.write"));
        assert_eq!(value["accepted"], json!(1));
        assert_eq!(value["keys"], json!(["enter"]));
    }

    #[tokio::test]
    async fn keys_validates_names_before_any_hub_call() {
        // No mock Hub: an unknown key must fail before the request is built.
        let client = connect_for_test("http://127.0.0.1:1".into(), "t".into()).expect("client");
        let err = fleet_keys(
            &client,
            FleetKeysOpts {
                filter: filter(true),
                idempotency_key: None,
                keys: vec!["nope".into()],
            },
        )
        .await
        .expect_err("unknown key");
        assert!(err.to_string().contains("unknown key"), "{err}");
    }

    #[test]
    fn load_text_joins_args_and_reads_file() {
        let text = load_fleet_text(None, vec!["PAUSE".into(), "now".into()]).expect("join");
        assert_eq!(text, "PAUSE now");
        assert!(load_fleet_text(None, Vec::new()).is_err());

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("brief.md");
        std::fs::write(&path, "from file").expect("write");
        let text = load_fleet_text(Some(path), Vec::new()).expect("read");
        assert_eq!(text, "from file");
    }
}
