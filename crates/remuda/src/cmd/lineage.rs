//! `remuda lineage profile show|set` — the lineage's seat profile
//! (cb-seat-profile, D-063, main-agent.md §3.4.2).
//!
//! `show` reads `GET /v1/lineages/{id}/profile`: a seat reads its own (the
//! lineage defaults to the caller's own, from `/v1/caller`), the owner any.
//! `set` is the owner's edit: a `PUT` with `expectedRevision`. Without
//! `--expected-revision` it reads the current revision first; a 409 means
//! someone saved in between, and the CLI says to reload. An edit takes
//! effect at the lineage's next chapter (restart or resume), never in the
//! running one, and only an unnarrowed Human may make it.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde_json::{Map, Value, json};

use super::hub_client::{HubClient, HubOpts, block_on, hub_http_error, print_json};
use remuda_hub_client::ClientError;

/// `remuda lineage` arguments.
#[derive(Args)]
#[command(about = "Read and edit a lineage's seat profile (takes effect at the next chapter).")]
pub(crate) struct LineageArgs {
    #[command(flatten)]
    hub: HubOpts,
    #[command(subcommand)]
    command: LineageCommand,
}

#[derive(Subcommand)]
enum LineageCommand {
    /// The seat profile: identity, model candidates, effort, permission
    /// mode and skills, with its revision.
    #[command(subcommand)]
    Profile(ProfileCommand),
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// Show the profile, whether an edit waits for the next chapter, and the
    /// last launch snapshot's unavailable items.
    Show {
        /// Lineage id (the first chapter's `ins_…`). Defaults to the caller's
        /// own lineage when run from inside an agent session.
        lineage: Option<String>,
        /// Print the Hub's JSON response instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Edit the profile (owner only). Unnamed fields keep their value.
    Set(SetArgs),
}

#[derive(Args)]
struct SetArgs {
    /// Lineage id (the first chapter's `ins_…`).
    lineage: String,
    /// The revision being replaced; read from the Hub when omitted. `0`
    /// creates the profile from the live chapter's spec.
    #[arg(long)]
    expected_revision: Option<u64>,
    /// Display name (`""` clears it).
    #[arg(long)]
    display_name: Option<String>,
    /// One-line role (`""` clears it).
    #[arg(long)]
    role: Option<String>,
    /// Model candidates, in order (the first is the default); replaces the
    /// list. Repeat for more, at most 5.
    #[arg(long = "model", value_name = "MODEL")]
    models: Vec<String>,
    /// Append one model candidate to the current list.
    #[arg(long = "add-model", value_name = "MODEL")]
    add_models: Vec<String>,
    /// Effort level (e.g. `high`).
    #[arg(long)]
    effort: Option<String>,
    /// With `--effort`: turn on ultracode.
    #[arg(long, requires = "effort")]
    ultracode: bool,
    /// Permission mode the next chapter launches with.
    #[arg(long)]
    permission_mode: Option<String>,
    /// Host-absolute skill directory; replaces the list. Repeat for more.
    #[arg(long = "skill", value_name = "DIR")]
    skills: Vec<String>,
    /// Remove every configured skill.
    #[arg(long, conflicts_with = "skills")]
    clear_skills: bool,
    /// Print the Hub's JSON response instead of text.
    #[arg(long)]
    json: bool,
}

impl super::registry::Entrypoint for LineageArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move { run(self).await })
    }

    fn tracing(&self) -> bool {
        false
    }
}

async fn run(args: LineageArgs) -> anyhow::Result<i32> {
    let client = args.hub.connect()?;
    match args.command {
        LineageCommand::Profile(ProfileCommand::Show { lineage, json }) => {
            let lineage = own_lineage(&client, lineage).await?;
            let view = client
                .get(&format!("/v1/lineages/{lineage}/profile"))
                .await
                .map_err(hub_http_error)
                .with_context(|| format!("profile of lineage {lineage}"))?;
            if json {
                print_json(&view)?;
            } else {
                print!("{}", render(&view));
            }
            Ok(0)
        }
        LineageCommand::Profile(ProfileCommand::Set(set)) => edit(&client, set).await,
    }
}

/// `lineage`, else the caller's own (an agent session).
async fn own_lineage(client: &HubClient, lineage: Option<String>) -> anyhow::Result<String> {
    if let Some(lineage) = lineage.filter(|value| !value.trim().is_empty()) {
        return Ok(lineage);
    }
    let caller = client.get("/v1/caller").await.map_err(hub_http_error)?;
    match caller.get("lineageId").and_then(Value::as_str) {
        Some(lineage) => Ok(lineage.to_owned()),
        None => bail!(
            "no lineage to show: this credential is not bound to an agent instance; pass a \
             lineage id"
        ),
    }
}

async fn edit(client: &HubClient, set: SetArgs) -> anyhow::Result<i32> {
    let path = format!("/v1/lineages/{}/profile", set.lineage);
    let needs_read = set.expected_revision.is_none() || !set.add_models.is_empty();
    let current = if needs_read {
        Some(client.get(&path).await.map_err(hub_http_error)?)
    } else {
        None
    };
    let expected = match set.expected_revision {
        Some(revision) => revision,
        None => current
            .as_ref()
            .and_then(|view| view.pointer("/profile/revision"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    };
    let body = body_of(&set, expected, current.as_ref());
    match client.put(&path, &body).await {
        Ok(result) => {
            if set.json {
                print_json(&result)?;
            } else {
                let revision = result
                    .pointer("/profile/revision")
                    .and_then(Value::as_u64)
                    .unwrap_or_default();
                let changed = result["changed"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                println!(
                    "profile revision {revision} saved (changed: {})",
                    if changed.is_empty() {
                        "nothing"
                    } else {
                        &changed
                    }
                );
                if result["pending"] == json!(true) {
                    println!("takes effect at the next chapter (restart or resume)");
                }
            }
            Ok(0)
        }
        Err(ClientError::Http { status: 409, body }) => {
            let current = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| value.get("currentRevision").and_then(Value::as_u64));
            match current {
                Some(current) => bail!(
                    "the profile changed since it was read (now revision {current}); reload it \
                     with `remuda lineage profile show {}` and apply your change again",
                    set.lineage
                ),
                None => Err(hub_http_error(ClientError::Http { status: 409, body })),
            }
        }
        Err(error) => Err(hub_http_error(error)),
    }
}

/// The PUT body: only the fields the flags name.
fn body_of(set: &SetArgs, expected: u64, current: Option<&Value>) -> Value {
    let mut body = Map::new();
    body.insert("expectedRevision".into(), json!(expected));
    if let Some(name) = &set.display_name {
        body.insert("displayName".into(), clearable(name));
    }
    if let Some(role) = &set.role {
        body.insert("role".into(), clearable(role));
    }
    if !set.models.is_empty() || !set.add_models.is_empty() {
        let mut models: Vec<String> = if set.models.is_empty() {
            current
                .and_then(|view| view.pointer("/profile/modelCandidates"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        } else {
            set.models.clone()
        };
        for model in &set.add_models {
            if !models.contains(model) {
                models.push(model.clone());
            }
        }
        body.insert("modelCandidates".into(), json!(models));
    }
    if let Some(effort) = &set.effort {
        body.insert(
            "effort".into(),
            json!({"name": effort, "ultracode": set.ultracode}),
        );
    }
    if let Some(mode) = &set.permission_mode {
        body.insert("permissionMode".into(), json!(mode));
    }
    if set.clear_skills {
        body.insert("skills".into(), json!([]));
    } else if !set.skills.is_empty() {
        body.insert("skills".into(), json!(set.skills));
    }
    Value::Object(body)
}

fn clearable(text: &str) -> Value {
    if text.trim().is_empty() {
        Value::Null
    } else {
        json!(text)
    }
}

/// The text rendering of a profile read.
fn render(view: &Value) -> String {
    let mut out = String::new();
    let lineage = view["lineageId"].as_str().unwrap_or("?");
    match view.get("profile").filter(|profile| !profile.is_null()) {
        None => out.push_str(&format!(
            "lineage {lineage}: no profile; the spec stays creation-time configuration\n"
        )),
        Some(profile) => {
            let text = |key: &str| profile[key].as_str().unwrap_or("—").to_owned();
            out.push_str(&format!(
                "lineage {lineage}, profile revision {}\n",
                profile["revision"]
            ));
            out.push_str(&format!("  name: {}\n", text("displayName")));
            out.push_str(&format!("  role: {}\n", text("role")));
            let models = list(&profile["modelCandidates"]);
            out.push_str(&format!(
                "  model candidates: {models} (the first is the default; switching is the \
                 agent's decision)\n"
            ));
            let effort = profile["effort"]["name"]
                .as_str()
                .unwrap_or("harness default");
            out.push_str(&format!("  effort: {effort}\n"));
            out.push_str(&format!("  permission mode: {}\n", text("permissionMode")));
            out.push_str(&format!("  skills: {}\n", list(&profile["skills"])));
        }
    }
    let applied = view["liveChapter"]["appliedRevision"]
        .as_u64()
        .map_or_else(|| "none".to_owned(), |revision| revision.to_string());
    out.push_str(&format!(
        "live chapter {} (generation {}), applied revision {applied}\n",
        view["liveChapter"]["instanceId"].as_str().unwrap_or("?"),
        view["liveChapter"]["generation"]
    ));
    if view["pending"] == json!(true) {
        out.push_str("pending: takes effect at the next chapter (待下一章节生效)\n");
    }
    if let Some(launch) = view.get("lastLaunch").filter(|launch| !launch.is_null()) {
        let unavailable = launch["payload"]["unavailable"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if unavailable.is_empty() {
            out.push_str("last launch: nothing unavailable\n");
        } else {
            out.push_str("last launch, unavailable:\n");
            for item in unavailable {
                out.push_str(&format!(
                    "  {} {}: {}\n",
                    item["class"].as_str().unwrap_or("?"),
                    item["name"].as_str().unwrap_or("?"),
                    item["reason"].as_str().unwrap_or("?")
                ));
            }
        }
        if let Some(refusal) = launch["payload"].get("refusal").filter(|r| !r.is_null()) {
            out.push_str(&format!(
                "  refused: {}\n",
                refusal["next"]["hint"].as_str().unwrap_or("")
            ));
        }
    }
    out
}

fn list(value: &Value) -> String {
    let items: Vec<&str> = value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: LineageCommand,
    }

    fn set(argv: &[&str]) -> SetArgs {
        let mut full = vec!["x", "profile", "set"];
        full.extend_from_slice(argv);
        match Cli::parse_from(full).command {
            LineageCommand::Profile(ProfileCommand::Set(set)) => set,
            _ => unreachable!(),
        }
    }

    #[test]
    fn set_sends_only_the_named_fields() {
        let args = set(&["ins_x", "--effort", "high"]);
        assert_eq!(
            body_of(&args, 3, None),
            json!({"expectedRevision": 3, "effort": {"name": "high", "ultracode": false}})
        );
        let args = set(&["ins_x", "--role", "", "--clear-skills"]);
        assert_eq!(
            body_of(&args, 1, None),
            json!({"expectedRevision": 1, "role": null, "skills": []})
        );
    }

    #[test]
    fn add_model_appends_to_the_current_list() {
        let args = set(&["ins_x", "--add-model", "model-c"]);
        let current =
            json!({"profile": {"revision": 2, "modelCandidates": ["model-a", "model-b"]}});
        assert_eq!(
            body_of(&args, 2, Some(&current))["modelCandidates"],
            json!(["model-a", "model-b", "model-c"])
        );
        let args = set(&["ins_x", "--model", "model-x", "--add-model", "model-y"]);
        assert_eq!(
            body_of(&args, 2, Some(&current))["modelCandidates"],
            json!(["model-x", "model-y"])
        );
    }

    #[test]
    fn show_renders_pending_and_unavailable_items() {
        let text = render(&json!({
            "lineageId": "ins_x",
            "profile": {"revision": 2, "displayName": "Main", "role": null,
                        "modelCandidates": ["model-a", "model-b"],
                        "effort": {"name": "high", "ultracode": false},
                        "permissionMode": "manual", "skills": []},
            "liveChapter": {"instanceId": "ins_y", "generation": 2, "appliedRevision": 1},
            "pending": true,
            "lastLaunch": {"instanceId": "ins_y", "generation": 2, "payload": {
                "unavailable": [{"class": "skill", "name": "kit", "reason": "unreadable"}],
                "refusal": {"reason": "launch-capability-unreadable",
                            "next": {"action": "ask-owner", "hint": "make it readable"}}}}
        }));
        assert!(
            text.contains("model candidates: model-a, model-b"),
            "{text}"
        );
        assert!(text.contains("待下一章节生效"), "{text}");
        assert!(text.contains("skill kit: unreadable"), "{text}");
        assert!(text.contains("applied revision 1"), "{text}");
    }
}
