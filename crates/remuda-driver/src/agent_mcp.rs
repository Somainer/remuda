//! Instance-bound MCP identity supplied by the authenticated Hub/Node path.
//!
//! This channel is separate from inherited or caller-supplied environment
//! overlays. It can carry only the instance context, never Node credentials.

use std::{collections::BTreeMap, sync::Arc};

use remuda_protocol::{HostId, InstanceId};

use crate::{DriverResult, Secret};

/// Trusted instance context. Deliberately not serializable into launch recipes.
#[derive(Clone, Debug)]
pub struct AgentMcpContext {
    instance_id: InstanceId,
    host_id: HostId,
    token: Arc<Secret>,
    hub: Option<String>,
}

impl AgentMcpContext {
    /// Bind the Hub-issued Agent token to the Node's allocated instance.
    /// An empty token still narrows callers to this instance; it grants nothing.
    pub fn new(
        instance_id: InstanceId,
        host_id: HostId,
        token: String,
        hub: Option<String>,
    ) -> Self {
        Self {
            instance_id,
            host_id,
            token: Arc::new(Secret::new(token.into_bytes())),
            hub,
        }
    }

    pub(crate) fn environment(&self) -> DriverResult<BTreeMap<String, String>> {
        let mut env = BTreeMap::from([
            (
                "REMUDA_INSTANCE_ID".into(),
                self.instance_id.as_id().as_str().into(),
            ),
            (
                "REMUDA_HOST_ID".into(),
                self.host_id.as_id().as_str().into(),
            ),
            ("REMUDA_TOKEN".into(), self.token.expose_str()?.into()),
        ]);
        if let Some(hub) = &self.hub {
            env.insert("REMUDA_HUB".into(), hub.clone());
        }
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trusted_context_reaches_the_print_child_and_overrides_overlay_identity() {
        use crate::claude_print::{ClaudePrintDriver, ClaudePrintOptions};
        use crate::{
            BinarySource, Delegation, Driver, ProviderHealth, ProviderKind, ProviderProfile,
        };
        use remuda_protocol::InstanceSpec;

        let dir = tempfile::tempdir().unwrap();
        let capture = dir.path().join("mcp-context.txt");
        let binary = remuda_testing::install_executable(
            dir.path(),
            "claude-context",
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '2.1.268 (Claude Code)'; exit 0; fi\nprintf '%s\\n' \"$REMUDA_TOKEN\" \"$REMUDA_INSTANCE_ID\" \"$REMUDA_HOST_ID\" \"$REMUDA_HUB\" > '{}'\n",
                capture.display()
            ),
        );
        let instance = InstanceId::new();
        let host = HostId::new();
        let mut spec: InstanceSpec =
            serde_json::from_str(include_str!("../tests/fixtures/instance-spec.json")).unwrap();
        spec.cwd = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        spec.host = host.clone();
        let profile = ProviderProfile {
            id: "pvp_01993ab0-0000-7000-8000-000000000001".parse().unwrap(),
            kind: ProviderKind::Anthropic,
            base_url: String::new(),
            delegation: Delegation::None,
            secret_ref: None,
            models: vec!["haiku".into()],
            health: ProviderHealth::Healthy,
        };
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut options = ClaudePrintOptions::new(
            profile,
            dir.path().join("launch"),
            home,
            BinarySource::Command(binary.to_string_lossy().into_owned()),
        );
        for key in [
            "REMUDA_TOKEN",
            "REMUDA_INSTANCE_ID",
            "REMUDA_HOST_ID",
            "REMUDA_HUB",
        ] {
            options
                .extra_env
                .insert(key.into(), "forged-overlay".into());
        }
        options.agent_mcp = Some(AgentMcpContext::new(
            instance.clone(),
            host.clone(),
            "scoped-process-token".into(),
            Some("http://hub.example".into()),
        ));
        options.handshake_timeout = std::time::Duration::from_secs(3);
        let driver = ClaudePrintDriver::new(options);
        // The fixture exits after capturing the context, before a native handshake.
        let _ = driver.start(spec).await;
        let captured = std::fs::read_to_string(capture).expect("the child must have executed");
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            [
                "scoped-process-token",
                instance.as_id().as_str(),
                host.as_id().as_str(),
                "http://hub.example"
            ]
        );
        driver.close().await.unwrap();
    }

    #[test]
    fn scoped_credential_is_redacted_and_has_no_operator_credential_names() {
        let context = AgentMcpContext::new(
            InstanceId::new(),
            HostId::new(),
            "scoped-secret".into(),
            Some("http://hub.example".into()),
        );
        assert!(!format!("{context:?}").contains("scoped-secret"));
        let env = context.environment().unwrap();
        assert_eq!(env["REMUDA_TOKEN"], "scoped-secret");
        assert_eq!(
            env["REMUDA_INSTANCE_ID"],
            context.instance_id.as_id().as_str()
        );
        assert_eq!(env["REMUDA_HOST_ID"], context.host_id.as_id().as_str());
        assert_eq!(env["REMUDA_HUB"], "http://hub.example");
        assert_eq!(env.len(), 4);
    }
}
