//! Authenticated caller policy for instance-origin MCP tools.

use anyhow::{Result, bail};
use remuda_hub_client::{CallerContext, CallerOrigin};
use serde_json::{Value, json};

use super::{
    HubClient,
    args::{opt_str, reject_removed_args, required_str, string_list},
};

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
        "remuda_instance_list" | "remuda_fleet_send" => Ok(false),
        // D-028 §4.5: an instance reads the attachments staged for its own
        // session. Narrower than the read rule above, which also admits direct
        // children: an attachment belongs to exactly one `instance.send`, so a
        // child has no claim on its parent's images. The Hub pins the same
        // rule server-side; this is the earlier, louder half of it.
        "remuda_attachments_list" | "remuda_attachment" => {
            let target = opt_str(args, "instanceId");
            if target.is_some_and(|id| Some(id) != caller.instance_id.as_deref()) {
                bail!("{name} is limited to this Agent instance's own session");
            }
            Ok(false)
        }
        "remuda_instance_read" | "remuda_instance_wait" => {
            if !caller.owns(required_str(args, "instanceId")?) {
                bail!("{name} is limited to this Agent instance and its direct children");
            }
            Ok(false)
        }
        "remuda_instance_keys" | "remuda_fleet_keys" => Ok(true),
        // D-031 follow-up (host-search-1): the Hub decides whether an
        // instance.send needs a human Interaction — own instances and any
        // target inside the caller's project scope go straight through, and
        // the Hub answers 409 HUMAN_APPROVAL_REQUIRED for the rest. A
        // client-side owns() test here would force the require-approval
        // header for every cross-host sibling and veto the scope rule.
        "remuda_instance_send" => Ok(false),
        "remuda_instance_stop" | "remuda_instance_rm" => {
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
        "remuda_merge" | "remuda_worktree_create" | "remuda_worktree_rm" => {
            bail!("{name} requires a Human/Bot coordinator device")
        }
        "remuda_instance_respond" => bail!("an Agent instance cannot answer approval Interactions"),
        _ => bail!("{name} is unavailable to Agent instances"),
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

    #[tokio::test]
    async fn agent_tools_cannot_reach_unowned_reads_or_fleet_broadcast() {
        let mock = crate::cmd::test_hub::spawn_mock_hub_as(
            json!({"origin":"agent", "instanceId":"ins_test", "hostId":"hst_1", "children":[]}),
        )
        .await;
        let client = crate::cmd::hub_client::connect_for_test(
            format!("http://{}", mock.addr),
            "fixture".into(),
        )
        .unwrap();
        for (name, args) in [
            (
                "remuda_fleet_send",
                json!({"all":true,"confirm":true,"text":"forbidden"}),
            ),
            (
                "remuda_fleet_keys",
                json!({"all":true,"confirm":true,"keys":["enter"]}),
            ),
            ("remuda_instance_read", json!({"instanceId":"sibling"})),
            ("remuda_instance_wait", json!({"instanceId":"sibling"})),
            ("remuda_doctor", json!({"host":"hst_1"})),
            ("remuda_worktree_rm", json!({"name":"sibling","force":true})),
            ("remuda_worktree_create", json!({"name":"sibling"})),
        ] {
            let response = super::super::handle_rpc(
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                "params":{"name":name,"arguments":args}}),
                &client,
            )
            .await
            .unwrap();
            assert_eq!(response["result"]["isError"], true, "{name}: {response}");
        }
        assert!(
            mock.requests
                .lock()
                .await
                .iter()
                .all(|request| request == "GET /v1/caller")
        );
        let listed = crate::cmd::instance::list_instances(&client, None)
            .await
            .unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        for name in ["remuda_instance_read", "remuda_instance_wait"] {
            let response = super::super::handle_rpc(
                &json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{"name":name,"arguments":{"instanceId":"ins_test","until":"idle"}}}),
                &client,
            )
            .await
            .unwrap();
            assert_eq!(response["result"]["isError"], false, "{response}");
        }
        assert!(mock.requests.lock().await.iter().all(|request| matches!(
            request.as_str(),
            "GET /v1/caller" | "GET /v1/instances/ins_test" | "GET /v1/instances/ins_test/journal"
        )));
    }

    #[test]
    fn agent_reads_are_limited_to_self_and_direct_children() {
        for tool in ["remuda_instance_read", "remuda_instance_wait"] {
            for target in ["self", "child"] {
                assert!(
                    !mcp_requires_approval(&agent(), tool, &json!({"instanceId":target})).unwrap()
                );
            }
            for target in ["sibling", "parent", "grandchild"] {
                assert!(
                    mcp_requires_approval(&agent(), tool, &json!({"instanceId":target})).is_err()
                );
            }
        }
    }

    #[test]
    fn instance_scope_is_self_and_direct_children_only() {
        let caller = agent();
        // Stop/close keep the ownership-only client gate; an Agent cannot
        // cancel or close a sibling without a human Interaction.
        for tool in ["remuda_instance_stop", "remuda_instance_rm"] {
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
        // instance.send never forces approval client-side: the Hub owns the
        // owns/project-scope decision and answers 409 when an Interaction is
        // actually required.
        for target in ["self", "child", "sibling", "parent", "grandchild"] {
            assert!(
                !mcp_requires_approval(
                    &caller,
                    "remuda_instance_send",
                    &json!({"instanceId": target})
                )
                .unwrap()
            );
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
                "remuda_instance_stop",
                &json!({"origin":"human","instanceId":"sibling","parentInstanceId":"self"})
            )
            .unwrap()
        );
    }

    #[test]
    fn agent_cannot_mutate_local_worktrees_even_with_confirmation() {
        let args = json!({"name":"other", "force":true, "approvalId":"forged"});
        for tool in ["remuda_worktree_create", "remuda_worktree_rm"] {
            assert!(mcp_requires_approval(&agent(), tool, &args).is_err());
            for origin in [CallerOrigin::Human, CallerOrigin::Bot] {
                let caller = CallerContext {
                    origin,
                    ..CallerContext::default()
                };
                assert!(!mcp_requires_approval(&caller, tool, &args).unwrap());
            }
        }
    }
}
