//! `remuda hostcap <host>` — current cores/load/disk, free worktree slots,
//! and the port blocks already in use on a host; the dispatch precondition in
//! coordinator-hierarchy.md §7 #6 / §2.2.

use clap::Args;
use serde_json::Value;

use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

/// `cli[]` kind the Computer Use presence row carries.
const COMPUTER_USE_KIND: &str = "computer-use";

/// `remuda hostcap` arguments.
#[derive(Args)]
#[command(about = "Show one host's capacity and allocated worker resources.")]
pub(crate) struct HostcapArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Host `hst_…`.
    host: String,
}

impl Entrypoint for HostcapArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            let client = self.hub.connect()?;
            let mut value = client
                .get(&format!("/v1/hosts/{}/hostcap", self.host))
                .await?;
            // Capacity and inventory are two Hub reads with different shapes:
            // `/hostcap` is the placement arithmetic, the host row carries
            // `cli[]`. The capability row belongs to the second, so it is
            // attached here rather than by widening the capacity endpoint.
            if let Ok(host) = client.get(&format!("/v1/hosts/{}", self.host)).await {
                value["computerUse"] = computer_use_row(&host);
            }
            print_json(&value)?;
            Ok(0)
        })
    }
}

/// The `computer-use` `cli[]` row, or an explicit "not reported".
///
/// Three states must stay distinguishable (ui-spec §2.6 / D-045 §3.4):
/// reported-and-installed, reported-and-absent, and not reported at all by an
/// older Node. Collapsing the last into `false` would read as "this host
/// cannot" when the truth is "nobody asked it".
fn computer_use_row(host: &Value) -> Value {
    let row = host
        .get("cli")
        .and_then(Value::as_array)
        .and_then(|cli| cli.iter().find(|row| row["kind"] == COMPUTER_USE_KIND));
    match row {
        Some(row) => serde_json::json!({
            "installed": row["installed"].as_bool().unwrap_or(false),
            "version": row["version"].clone(),
            "path": row["path"].clone(),
            "auth": "unknown",
            "reported": true,
        }),
        None => serde_json::json!({ "reported": false }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_reported_installed_row_is_surfaced_with_its_path() {
        let host = json!({
            "cli": [
                {"kind": "claude", "installed": true, "path": "/usr/bin/claude"},
                {"kind": "computer-use", "installed": true, "version": "1.2.3",
                 "path": "/home/x/.codex/computer-use/…/SkyComputerUseClient", "auth": "unknown"},
            ]
        });
        let row = computer_use_row(&host);
        assert_eq!(row["reported"], true);
        assert_eq!(row["installed"], true);
        assert_eq!(row["version"], "1.2.3");
        assert_eq!(row["auth"], "unknown");
        assert!(
            row["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("SkyComputerUseClient"))
        );
    }

    #[test]
    fn a_reported_absent_row_is_installed_false_not_unreported() {
        let host = json!({
            "cli": [{"kind": "computer-use", "installed": false, "auth": "unknown"}]
        });
        let row = computer_use_row(&host);
        assert_eq!(row["reported"], true, "the Node answered, and said no");
        assert_eq!(row["installed"], false);
        assert!(row["path"].is_null());
    }

    #[test]
    fn an_older_node_that_omits_the_row_is_unreported() {
        let host = json!({"cli": [{"kind": "claude", "installed": true}]});
        let row = computer_use_row(&host);
        assert_eq!(
            row["reported"], false,
            "absent row is 'not reported', never 'unsupported'"
        );
        assert!(
            row.get("installed").is_none(),
            "no claim either way when nobody reported: {row}"
        );

        let no_cli = json!({});
        assert_eq!(computer_use_row(&no_cli)["reported"], false);
    }
}
