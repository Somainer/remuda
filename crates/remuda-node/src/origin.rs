//! Authenticated Hub provenance. Missing/unknown wire values fail closed. SPEC-ONLY (unimplemented, 2026-09-22 c-nodecosign): the Node trusts the Hub-stamped `origin` on the carrier alone (see `wire_origin` below; attack chain in `docs/design/evidence/node-cosign-1.md` §1) — `docs/design/passkey-login.md` §6 specifies the device-passkey co-signature under which privileged frames (human/bot origin, bypass postures, non-empty capabilities, binary/args overrides; `gate.run`/`gate.then`/`gate.land`) refuse before dispatch when they carry no valid device assertion over the canonical frame.

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

/// Read the frame's claimed origin from the Hub envelope.
///
/// Only the Hub envelope is authoritative. Native input text cannot promote
/// itself by including input.origin or a nested spec actor.
///
/// SECURITY (spec only — `docs/design/protocol.md` §7.7): this authority runs
/// in one direction today. Nothing here proves *who the Hub is*: any peer that
/// completes the Hub↔Node handshake can send `origin: "human"`, because the
/// handshake authenticates Node→Hub only (bearer `node.auth`) and the
/// application layer has no Hub identity on the Node side. The Hub ed25519
/// identity key, the Node-side TOFU pin, and the signed-frame envelope that
/// close this are specified in §7.7 (mismatch fails loud per D-035). They are
/// not implemented yet; until they are, this function stays the chokepoint the
/// future signature check must sit in front of.
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

pub(crate) fn instance_mcp(
    launch: &crate::DriverLaunch,
) -> remuda_driver::agent_mcp::AgentMcpContext {
    let credential = launch.request.agent_credential.as_ref();
    remuda_driver::agent_mcp::AgentMcpContext::new(
        launch.instance.meta.id.clone(),
        launch.instance.host_id.clone(),
        credential
            .map(|value| value.token.clone())
            .unwrap_or_default(),
        credential.and_then(|value| value.hub.clone()),
    )
}

/// Keep the unredacted options map free of caller identity and credentials.
pub(crate) fn instance_env(extra_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    extra_env
        .iter()
        .filter(|(name, _)| !remuda_driver::child_env::is_denied(name))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
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
                registered_workspace_root: ".".into(),
                api_relay: None,
            };
            let env = instance_env(&inherited);
            assert!(!env.keys().any(|name| name.starts_with("REMUDA_")));
            let rendered = format!("{env:?} {:?}", instance_mcp(&launch));
            for secret in [
                "scoped-agent-token",
                "human-token",
                "pairing-code",
                "enrollment-token",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "credential leaked through Debug"
                );
            }
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
        for params in [json!({}), json!({"origin":"future"})] {
            let unknown: crate::CreateInstanceRequest = serde_json::from_value(params).unwrap();
            assert_eq!(unknown.origin, InputOrigin::Agent);
            assert_eq!(unknown.permission_mode, "manual");
        }
    }
}
