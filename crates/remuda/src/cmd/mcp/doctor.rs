//! MCP doctor tools: metadata and handler are registered together.

use super::Tool;
use anyhow::anyhow;
use serde_json::json;

pub(super) fn tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "remuda_doctor",
            "Preflight the local machine (default/local=true) or an active Hub host. Reports binary versions and login-marker states, data/identity, disk, ports, Hub reachability and registered host links. Nonzero exitCode means blockers. No credential values are returned.",
            json!({"type":"object","additionalProperties":false,"properties":{
                "host":{"type":"string"},"local":{"type":"boolean"},"dataDir":{"type":"string"}
            }}),
            |client, args| {
                Box::pin(async move {
                    let mut config = crate::config::Config::load(None)?;
                    let mut args = args;
                    if let Some(path) = args.as_object_mut().and_then(|map| map.remove("dataDir")) {
                        config.data_dir = std::path::PathBuf::from(
                            path.as_str().ok_or_else(|| anyhow!("dataDir must be a string"))?,
                        );
                    }
                    let options = serde_json::from_value(args)?;
                    crate::cmd::doctor::inspect(&config, &options, client).await
                })
            },
        ).report(),
    ]
}
