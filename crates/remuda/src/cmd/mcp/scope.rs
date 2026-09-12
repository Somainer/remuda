//! Authenticated caller policy for instance-origin MCP tools.

use anyhow::{Result, bail};
use remuda_hub_client::{CallerContext, CallerOrigin};
use serde_json::{Value, json};

use super::{HubClient, args::{opt_str, reject_removed_args, required_str, string_list}};

pub(super) fn tool_schema(name: &str, schema: &mut Value) {
    if matches!(
        name,
        "remuda_instance_create"
            | "remuda_instance_send"
            | "remuda_instance_stop"
            | "remuda_instance_rm"
            | "remuda_instance_keys"
            | "remuda_fleet_run"
            | "remuda_fleet_send"
            | "remuda_fleet_keys"
    ) {
        schema["properties"]["approvalId"] = json!({"type":"string", "description":"Human-approved Interaction id; retry the exact action once after Allow once."});
    }
    if matches!(name, "remuda_fleet_send" | "remuda_fleet_keys") {
        schema["properties"]["confirm"] = json!({"type":"boolean", "description":"Explicit Human/Bot confirmation for all:true. Agent origin is forbidden."});
    }
}

pub(super) async fn call_tool(
    tool: &super::registry::Tool,
    mut args: Value,
    client: &HubClient,
) -> Value {
    match scoped_client(tool.name, &mut args, client).await {
        Ok(scoped) => tool.call(&scoped, args).await,
        Err(error) => super::tool_content(Err(error)),
    }
}

pub(super) async fn scoped_client(
    name: &str,
    args: &mut Value,
    client: &HubClient,
) -> Result<HubClient> {
    // Preserve the removed-argument errors before any Hub request (M4/M5).
    let removed: &[&str] = match name {
        "remuda_instance_create" => &["promptFile"],
        "remuda_instance_send" | "remuda_fleet_send" => &["file"],
        "remuda_worktree_create" => &["path", "repo"],
        "remuda_worktree_rm" => &["repo"],
        _ => &[],
    };
    reject_removed_args(args, removed)?;
    if name == "remuda_fleet_keys" {
        // Invalid key syntax is rejected without contacting any command sink.
        crate::cmd::instance::encode_keys(&string_list(args, "keys"))?;
    }
    let caller = client.caller_context().await?;
    let required = mcp_requires_approval(&caller, name, args)?;
    let scoped = client
        .clone()
        .with_approval(opt_str(args, "approvalId").map(str::to_string), required);
    if caller.origin == CallerOrigin::Agent
        && name == "remuda_instance_create"
        && opt_str(args, "host").is_none()
        && string_list(args, "labels").is_empty()
    {
        args["host"] = json!(caller.host_id);
    }
    Ok(scoped)
}

/// Client-side policy is enforced before any tool side effect; the Hub repeats
/// the check against its authenticated device and consumes the exact-action
/// approval. Setting required routes the request to that broker gate, never to
/// an unchecked command sink.
pub(super) fn mcp_requires_approval(
    caller: &CallerContext,
    name: &str,
    args: &Value,
) -> Result<bool> {
    if matches!(name, "remuda_fleet_send" | "remuda_fleet_keys") {
        caller.check_fleet_all(
            args.get("all").and_then(Value::as_bool).unwrap_or(false),
            args.get("confirm")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )?;
    }
    if caller.origin != CallerOrigin::Agent {
        return Ok(false);
    }
    if caller.instance_id.is_none() || caller.host_id.is_none() {
        bail!("Agent MCP caller requires an instance-bound credential and a registered host");
    }
    match name {
        "remuda_instance_keys" | "remuda_fleet_keys" => Ok(true),
        "remuda_instance_send" | "remuda_instance_stop" | "remuda_instance_rm" => {
            Ok(!caller.owns(required_str(args, "instanceId")?))
        }
        "remuda_instance_create" if opt_str(args, "worktree").is_some() => bail!(
            "Agent create requires an existing workspace; local worktree creation requires a Human/Bot coordinator"
        ),
        "remuda_instance_create" => Ok(opt_str(args, "host")
            .is_some_and(|host| Some(host) != caller.host_id.as_deref())
            || !string_list(args, "labels").is_empty()),
        "remuda_fleet_run" => Ok(true),
        // These local mutations have no instance-scoped execution sink.
        "remuda_merge" | "remuda_worktree_create" => {
            bail!("{name} requires a Human/Bot coordinator device")
        }
        "remuda_instance_respond" => bail!("an Agent instance cannot answer approval Interactions"),
        _ => Ok(false),
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;
    use serde_json::json;

    fn agent() -> CallerContext {
        CallerContext {
            origin: CallerOrigin::Agent,
            instance_id: Some("self".into()),
            host_id: Some("host-a".into()),
            children: vec!["child".into()],
        }
    }

    #[test]
    fn instance_scope_is_self_and_direct_children_only() {
        let caller = agent();
        for tool in [
            "remuda_instance_send",
            "remuda_instance_stop",
            "remuda_instance_rm",
        ] {
            for target in ["self", "child"] {
                assert!(
                    !mcp_requires_approval(&caller, tool, &json!({"instanceId":target})).unwrap()
                );
            }
            for target in ["sibling", "parent", "grandchild"] {
                assert!(
                    mcp_requires_approval(&caller, tool, &json!({"instanceId":target})).unwrap()
                );
            }
        }
        assert!(
            !mcp_requires_approval(&caller, "remuda_instance_create", &json!({"host":"host-a"}))
                .unwrap()
        );
        assert!(!mcp_requires_approval(&caller, "remuda_instance_create", &json!({})).unwrap());
        assert!(
            mcp_requires_approval(&caller, "remuda_instance_create", &json!({"host":"host-b"}))
                .unwrap()
        );
        for target in ["self", "child", "sibling"] {
            assert!(
                mcp_requires_approval(
                    &caller,
                    "remuda_instance_keys",
                    &json!({"instanceId":target})
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn agent_fleet_all_is_forbidden_even_with_confirmation_or_approval_id() {
        for tool in ["remuda_fleet_send", "remuda_fleet_keys"] {
            for confirm in [false, true] {
                let error = mcp_requires_approval(
                    &agent(),
                    tool,
                    &json!({"all":true,"confirm":confirm,"approvalId":"forged"}),
                )
                .unwrap_err();
                assert!(error.to_string().contains("forbidden from Agent origin"));
            }
        }
        assert!(
            mcp_requires_approval(
                &agent(),
                "remuda_fleet_keys",
                &json!({"labels":["region=sg"]})
            )
            .unwrap()
        );
        for origin in [CallerOrigin::Human, CallerOrigin::Bot] {
            let caller = CallerContext {
                origin,
                ..CallerContext::default()
            };
            assert!(
                mcp_requires_approval(&caller, "remuda_fleet_send", &json!({"all":true})).is_err()
            );
            assert!(
                !mcp_requires_approval(
                    &caller,
                    "remuda_fleet_send",
                    &json!({"all":true,"confirm":true})
                )
                .unwrap()
            );
            for name in [
                "remuda_instance_send",
                "remuda_instance_keys",
                "remuda_instance_create",
            ] {
                assert!(
                    !mcp_requires_approval(
                        &caller,
                        name,
                        &json!({"host":"elsewhere","instanceId":"any"})
                    )
                    .unwrap()
                );
            }
        }
    }

    #[test]
    fn caller_identity_cannot_be_supplied_in_tool_arguments() {
        let unknown = CallerContext::default();
        assert!(
            mcp_requires_approval(
                &unknown,
                "remuda_instance_create",
                &json!({"origin":"human","instanceId":"self","host":"host-a"})
            )
            .is_err()
        );
        assert!(
            mcp_requires_approval(
                &agent(),
                "remuda_instance_send",
                &json!({"origin":"human","instanceId":"sibling","parentInstanceId":"self"})
            )
            .unwrap()
        );
    }
}
