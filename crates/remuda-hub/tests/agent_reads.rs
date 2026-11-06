//! ap-command-read and ap-subagent-read: the per-instance reads an Agent makes
//! on an instance it owns.
//!
//! Each newly admitted route is read by an Agent for its own instance and its
//! direct child (200), and refused for a sibling in the same project and for
//! an instance in another project (the middleware's `agent-route` 403, as for
//! every other per-instance read). A plain Human reads all of them, and a
//! Human token narrowed to the parent with `x-remuda-instance-id` gets exactly
//! the Agent's answers. Instances are store-inserted; the one Node read (the
//! subagent transcript) is answered by a scripted transport.

use anyhow::{Context, Result};
use remuda_hub::{HubConfig, spawn};
use serde_json::{Value, json};

struct Ctx {
    _dir: tempfile::TempDir,
    hub: remuda_hub::RunningHub,
    http: reqwest::Client,
    human: String,
    host: String,
}

struct Reply {
    status: u16,
    body: Value,
}

impl Ctx {
    async fn boot() -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let hub = spawn(HubConfig::for_test(dir.path().join("hub"))).await?;
        let human = hub.mint_device_token("reads-human").await?;
        let host = remuda_protocol::HostId::new().as_id().to_string();
        hub.test_insert_host(&host).await?;
        Ok(Self {
            _dir: dir,
            hub,
            http: reqwest::Client::new(),
            human,
            host,
        })
    }

    async fn project(&self, name: &str) -> Result<String> {
        let reply = self
            .call(
                "POST",
                "/v1/projects",
                &self.human,
                None,
                Some(json!({ "name": name })),
            )
            .await?;
        anyhow::ensure!(reply.status == 200, "create project: {}", reply.body);
        Ok(reply.body["id"].as_str().context("project id")?.to_owned())
    }

    /// A store-inserted instance in `project`, a child of `parent` when given.
    /// A `seat` holds `dispatch` (the parent edge needs it); the others are
    /// leaf workers, since the reads need no grant.
    async fn instance(&self, project: &str, parent: Option<&str>, seat: bool) -> Result<String> {
        let mut spec = json!({ "kind": "claude", "projectId": project });
        if let Some(parent) = parent {
            spec["parentInstanceId"] = json!(parent);
        }
        let delegation = if seat {
            remuda_hub::store_test_support::seat_delegation(
                remuda_protocol::InstanceScope {
                    project_ids: vec![remuda_protocol::ProjectId::try_from(project.to_owned())?],
                    ..Default::default()
                },
                vec!["dispatch".into()],
            )
        } else {
            remuda_hub::store_test_support::leaf_delegation(project).map_err(anyhow::Error::msg)?
        };
        let record = self
            .hub
            .store()
            .context("store")?
            .insert_instance_delegated(
                self.host.clone(),
                None,
                "claude".into(),
                "claude-sdk".into(),
                None,
                spec,
                delegation,
                remuda_hub::agent_scope_test_support::CallerAuthority::internal(),
            )
            .await?;
        Ok(record.instance_id)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        narrow_to: Option<&str>,
        body: Option<Value>,
    ) -> Result<Reply> {
        let mut request = self
            .http
            .request(method.parse()?, format!("http://{}{path}", self.hub.addr))
            .bearer_auth(token);
        if let Some(instance) = narrow_to {
            request = request.header("x-remuda-instance-id", instance);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let text = response.text().await?;
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok(Reply { status, body })
    }

    /// One queued `instance.send` on `instance` (its host has no Node, so the
    /// command stays queued); returns its commandId.
    async fn send(&self, instance: &str) -> Result<String> {
        let command_id = remuda_protocol::CommandId::new().as_id().to_string();
        let reply = self
            .call(
                "POST",
                &format!("/v1/instances/{instance}/commands"),
                &self.human,
                None,
                Some(json!({
                    "commandId": command_id,
                    "operation": "instance.send",
                    "payload": { "text": "status check" }
                })),
            )
            .await?;
        anyhow::ensure!(reply.status == 200, "send: {}", reply.body);
        Ok(command_id)
    }
}

/// Parent and child in project A, a sibling in A that the parent does not
/// own, and a foreign instance in project B.
struct Family {
    parent: String,
    child: String,
    sibling: String,
    foreign: String,
}

async fn family(ctx: &Ctx) -> Result<Family> {
    let project_a = ctx.project("alpha").await?;
    let project_b = ctx.project("beta").await?;
    let parent = ctx.instance(&project_a, None, true).await?;
    let child = ctx.instance(&project_a, Some(&parent), false).await?;
    let sibling = ctx.instance(&project_a, None, false).await?;
    let foreign = ctx.instance(&project_b, None, false).await?;
    Ok(Family {
        parent,
        child,
        sibling,
        foreign,
    })
}

/// Read `paths(target)` for every target as the Agent bound to the parent and
/// as a Human narrowed to it: 200 for the owned targets, the middleware's 403
/// otherwise. A plain Human reads every target.
async fn assert_owned_reads(
    ctx: &Ctx,
    family: &Family,
    paths: impl Fn(&str) -> Vec<String>,
    check: impl Fn(&str, &Value),
) -> Result<()> {
    let agent = ctx
        .hub
        .test_mint_agent_token("reads-parent", &family.parent)
        .await?;
    let owned = [&family.parent, &family.child];
    let unowned = [&family.sibling, &family.foreign];
    for (token, narrow_to, who) in [
        (&agent, None, "agent"),
        (&ctx.human, Some(family.parent.as_str()), "narrowed human"),
    ] {
        for target in owned {
            for path in paths(target) {
                let reply = ctx.call("GET", &path, token, narrow_to, None).await?;
                assert_eq!(reply.status, 200, "{who} {path}: {}", reply.body);
                check(target, &reply.body);
            }
        }
        for target in unowned {
            for path in paths(target) {
                let reply = ctx.call("GET", &path, token, narrow_to, None).await?;
                assert_eq!(reply.status, 403, "{who} {path}: {}", reply.body);
                assert_eq!(reply.body["reason"], "agent-route", "{who} {path}");
                assert!(reply.body["next"]["action"].is_string(), "{}", reply.body);
            }
        }
    }
    for target in owned.into_iter().chain(unowned) {
        for path in paths(target) {
            let reply = ctx.call("GET", &path, &ctx.human, None, None).await?;
            assert_eq!(reply.status, 200, "human {path}: {}", reply.body);
            check(target, &reply.body);
        }
    }
    Ok(())
}

#[tokio::test]
async fn an_agent_reads_the_commands_of_what_it_owns_only() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let family = family(&ctx).await?;
    let mut commands = std::collections::HashMap::new();
    for target in [
        &family.parent,
        &family.child,
        &family.sibling,
        &family.foreign,
    ] {
        commands.insert(target.clone(), ctx.send(target).await?);
    }
    assert_owned_reads(
        &ctx,
        &family,
        |target| {
            vec![
                format!("/v1/instances/{target}/commands"),
                format!("/v1/instances/{target}/commands/{}", commands[target]),
            ]
        },
        |target, body| {
            let row = body["commands"].get(0).unwrap_or(body);
            assert_eq!(row["commandId"], json!(commands[target]), "{body}");
            assert_eq!(row["instanceId"], json!(target), "{body}");
        },
    )
    .await?;

    // The handler's own check holds when the middleware admits the path: a
    // command id read through an owned instance's path is still 404 when the
    // row belongs to an instance the caller does not own.
    let agent = ctx
        .hub
        .test_mint_agent_token("reads-cross", &family.parent)
        .await?;
    let reply = ctx
        .call(
            "GET",
            &format!(
                "/v1/instances/{}/commands/{}",
                family.parent, commands[&family.foreign]
            ),
            &agent,
            None,
            None,
        )
        .await?;
    assert_eq!(reply.status, 404, "{}", reply.body);
    Ok(())
}

#[tokio::test]
async fn an_agent_reads_the_subagent_transcripts_of_what_it_owns_only() -> Result<()> {
    let ctx = Ctx::boot().await?;
    let family = family(&ctx).await?;
    ctx.hub
        .test_set_node_reply(
            &ctx.host,
            Some(json!({
                "jsonrpc": "2.0",
                "id": "1",
                "result": { "agentId": "a1b2", "available": true, "events": [] }
            })),
        )
        .await;
    assert_owned_reads(
        &ctx,
        &family,
        |target| vec![format!("/v1/instances/{target}/subagents/a1b2/transcript")],
        |_, body| assert_eq!(body["available"], true, "{body}"),
    )
    .await
}
