//! Authenticated Hub provenance. Missing/unknown wire values fail closed.

use std::collections::BTreeMap;

use remuda_protocol::{CommandOrigin, InputOrigin};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

pub(crate) fn agent_origin() -> InputOrigin {
    InputOrigin::Agent
}

pub(crate) fn deserialize_origin<'de, D: Deserializer<'de>>(d: D) -> Result<InputOrigin, D::Error> {
    Ok(parse_origin(&Value::deserialize(d)?))
}

fn parse_origin(value: &Value) -> InputOrigin {
    match value.as_str() {
        Some("human") => InputOrigin::Human,
        Some("bot") => InputOrigin::Bot,
        _ => InputOrigin::Agent,
    }
}

pub(crate) fn wire_origin(params: &Value) -> InputOrigin {
    // Only the Hub envelope is authoritative. Native input text cannot promote
    // itself by including input.origin or a nested spec actor.
    parse_origin(params.get("origin").unwrap_or(&Value::Null))
}

pub(crate) fn command_origin(origin: InputOrigin) -> CommandOrigin {
    match origin {
        InputOrigin::Human => CommandOrigin::Ui,
        InputOrigin::Bot => CommandOrigin::Bot,
        InputOrigin::Agent => CommandOrigin::Mcp,
    }
}

pub(crate) fn input_origin(origin: CommandOrigin) -> InputOrigin {
    match origin {
        CommandOrigin::Ui | CommandOrigin::Cli => InputOrigin::Human,
        CommandOrigin::Bot => InputOrigin::Bot,
        CommandOrigin::Mcp | CommandOrigin::System => InputOrigin::Agent,
    }
}

pub(crate) fn instance_env(
    launch: &crate::DriverLaunch,
    extra_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = extra_env.clone();
    env.insert(
        "REMUDA_INSTANCE_ID".into(),
        launch.instance.meta.id.as_id().as_str().to_string(),
    );
    env.insert(
        "REMUDA_HOST_ID".into(),
        launch.instance.host_id.as_id().as_str().to_string(),
    );
    // An instance's future calls are Agent even when its creator was Human.
    // Never leave an inherited operator token in the launched process.
    env.insert(
        "REMUDA_TOKEN".into(),
        launch
            .request
            .agent_credential
            .as_ref()
            .map(|credential| credential.token.clone())
            .unwrap_or_default(),
    );
    env.insert("REMUDA_BOOTSTRAP_TOKEN".into(), String::new());
    env.insert("REMUDA_ENROLL_TOKEN".into(), String::new());
    if let Some(hub) = launch
        .request
        .agent_credential
        .as_ref()
        .and_then(|credential| credential.hub.as_ref())
    {
        env.insert("REMUDA_HUB".into(), hub.clone());
    }
    env
}

/// Ephemeral instance-bound device token, received only on the authenticated
/// carrier. Never serialized into a request digest, journal, or launch recipe.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCredential {
    pub(crate) token: String,
    #[serde(default)]
    pub(crate) hub: Option<String>,
}

impl std::fmt::Debug for AgentCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AgentCredential([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn child_environment_replaces_operator_credentials_and_caller_identity() {
        use remuda_protocol::{DriverKind, HostId, InstanceId, WorkspaceId};

        let inherited = BTreeMap::from([
            ("REMUDA_TOKEN".into(), "human-token".into()),
            ("REMUDA_BOOTSTRAP_TOKEN".into(), "pairing-code".into()),
            ("REMUDA_ENROLL_TOKEN".into(), "enrollment-token".into()),
            ("REMUDA_INSTANCE_ID".into(), "other-instance".into()),
            ("REMUDA_HOST_ID".into(), "other-host".into()),
            ("FAKE_CLAUDE_SCRIPT".into(), "ok".into()),
        ]);
        for credential in [None, Some(json!({"token":"scoped-agent-token"}))] {
            let launch = crate::DriverLaunch {
                instance: crate::runtime::fixture_instance(
                    InstanceId::new(),
                    HostId::new(),
                    WorkspaceId::new(),
                    DriverKind::ClaudePrint,
                )
                .unwrap(),
                request: serde_json::from_value(json!({"agentCredential":credential})).unwrap(),
                workspace_root: ".".into(),
            };
            let env = instance_env(&launch, &inherited);
            assert_eq!(env["REMUDA_BOOTSTRAP_TOKEN"], "");
            assert_eq!(env["REMUDA_ENROLL_TOKEN"], "");
            assert_eq!(
                env["REMUDA_TOKEN"],
                credential.as_ref().map_or("", |_| "scoped-agent-token")
            );
            assert_eq!(
                env["REMUDA_INSTANCE_ID"],
                launch.instance.meta.id.as_id().as_str()
            );
            assert_eq!(
                env["REMUDA_HOST_ID"],
                launch.instance.host_id.as_id().as_str()
            );
            assert_eq!(env["FAKE_CLAUDE_SCRIPT"], "ok");
        }
    }

    #[test]
    fn only_hub_envelope_sets_origin() {
        for (wire, expected) in [
            ("human", InputOrigin::Human),
            ("bot", InputOrigin::Bot),
            ("agent", InputOrigin::Agent),
            ("mcp", InputOrigin::Agent),
            ("future", InputOrigin::Agent),
        ] {
            assert_eq!(wire_origin(&json!({"origin":wire})), expected);
        }
        assert_eq!(
            wire_origin(&json!({"input":{"origin":"human"}, "spec":{"origin":"human"}})),
            InputOrigin::Agent
        );
        let unknown: crate::CreateInstanceRequest =
            serde_json::from_value(json!({"origin":"future"})).unwrap();
        assert_eq!(unknown.origin, InputOrigin::Agent);
    }
}
