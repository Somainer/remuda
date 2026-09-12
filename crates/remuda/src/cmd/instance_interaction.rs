//! List and answer the Node-owned interaction through the shared Hub endpoint.

use super::hub_client::HubClient;
use super::instance::resolve_instance_id;
use anyhow::{Result, bail};
use clap::Args;
use serde_json::{Value, json};

#[derive(Debug, Args)]
pub(crate) struct RespondOpts {
    /// Instance id or name. With no answer flags, list its pending prompts.
    pub instance_id: String,
    /// Explicit interaction id (required when more than one is pending).
    #[arg(long)]
    pub interaction_id: Option<String>,
    /// Displayed option ID, such as y, n, 1, or enter.
    #[arg(long, conflicts_with_all = ["text", "answer"])]
    pub option: Option<String>,
    /// Single-line text reply to a question.
    #[arg(long, conflicts_with = "answer")]
    pub text: Option<String>,
    /// Full InteractionAnswer JSON for multi-field/structured requests.
    #[arg(long)]
    pub answer: Option<String>,
    /// Stable command ID for first-answer-wins reconciliation.
    #[arg(long)]
    pub command_id: Option<String>,
}

pub(crate) async fn respond(client: &HubClient, opts: RespondOpts) -> Result<Value> {
    let instance = resolve_instance_id(client, &opts.instance_id).await?;
    let listed = client
        .get(&format!("/v1/interactions?instanceId={instance}"))
        .await?;
    if opts.option.is_none() && opts.text.is_none() && opts.answer.is_none() {
        return Ok(listed);
    }
    let items = listed["items"].as_array().cloned().unwrap_or_default();
    let candidates: Vec<_> = items
        .iter()
        .map(|row| row.get("interaction").unwrap_or(row))
        .filter(|row| {
            opts.interaction_id
                .as_deref()
                .is_none_or(|id| row["id"] == id)
        })
        .collect();
    if candidates.len() != 1 {
        bail!("expected one pending interaction; list prompts and select --interaction-id");
    }
    let interaction = candidates[0];
    if interaction["answerable"] != true {
        bail!("interaction is not answerable");
    }
    let answer = if let Some(raw) = opts.answer {
        serde_json::from_str(&raw)?
    } else {
        shortcut_answer(
            &interaction["request"],
            opts.option.as_deref(),
            opts.text.as_deref(),
        )?
    };
    let id = interaction["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("interaction id missing"))?;
    let command_id = opts
        .command_id
        .unwrap_or_else(|| remuda_protocol::CommandId::new().as_id().to_string());
    Ok(client
        .post(
            &format!("/v1/interactions/{id}/answer"),
            &json!({"commandId": command_id, "answer": answer}),
        )
        .await?)
}

fn shortcut_answer(request: &Value, option: Option<&str>, text: Option<&str>) -> Result<Value> {
    match request["kind"].as_str() {
        Some("approval") if text.is_none() => {
            let option = option.ok_or_else(|| anyhow::anyhow!("approval requires --option"))?;
            if !request["options"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["id"] == option))
            {
                bail!("unknown approval option");
            }
            Ok(json!({"kind":"approval", "optionId":option, "inputDigest":request["inputDigest"]}))
        }
        Some("question") => {
            let fields = request["fields"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("question fields missing"))?;
            if fields.len() != 1 {
                bail!("multi-field question requires --answer JSON");
            }
            let id = fields[0]["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("question field id missing"))?;
            Ok(
                json!({"kind":"question", "answers":{id:{"optionIds":option.into_iter().collect::<Vec<_>>(), "text":text}}}),
            )
        }
        _ => bail!("use --answer JSON matching the displayed request schema"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pty_shortcuts_preserve_option_digest_and_field() {
        let request =
            json!({"kind":"approval", "inputDigest":"screen-hash", "options":[{"id":"y"}]});
        assert_eq!(
            shortcut_answer(&request, Some("y"), None).unwrap()["inputDigest"],
            "screen-hash"
        );
        assert!(shortcut_answer(&request, Some("bogus"), None).is_err());
        let request = json!({"kind":"question", "fields":[{"id":"screen"}]});
        assert_eq!(
            shortcut_answer(&request, Some("2"), None).unwrap()["answers"]["screen"]["optionIds"],
            json!(["2"])
        );
    }
}
