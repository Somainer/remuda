//! `remuda hostcap <host>` — current cores/load/disk, free worktree slots,
//! and the port blocks already in use on a host; the dispatch precondition in
//! coordinator-hierarchy.md §7 #6 / §2.2.

use clap::Args;
use serde_json::Value;

use super::capability::COMPUTER_USE;
use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

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
            //
            // A failed *second* read must not leave the key absent: a consumer
            // reading `null` cannot tell "the host does not report this" from
            // "remuda could not ask", and those call for different operator
            // action. So the error becomes an explicit, reported state carrying
            // its text, and the capacity payload — already fetched — still
            // prints.
            value["computerUse"] = capability_state(
                client.get(&format!("/v1/hosts/{}", self.host)).await,
            );
            print_json(&value)?;
            Ok(0)
        })
    }
}

/// The capability block for `remuda hostcap`, from the host-row read.
///
/// Split out so the failure path is testable: a dropped error would leave the
/// key absent, and a consumer reading `null` cannot tell "the host does not
/// report this capability" from "remuda could not ask the Hub" — two states
/// that call for different operator action.
fn capability_state(host: Result<Value, impl std::fmt::Display>) -> Value {
    match host {
        Ok(host) => computer_use_row(&host),
        Err(error) => serde_json::json!({
            "reported": false,
            "error": error.to_string(),
        }),
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
        .and_then(|cli| cli.iter().find(|row| row["kind"] == COMPUTER_USE));
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

    /// A failed host-row read must say so, not vanish into a missing key.
    #[test]
    fn a_failed_host_read_reports_the_error_instead_of_dropping_the_key() {
        let state = capability_state(Err::<Value, _>("hub timed out"));
        assert_eq!(state["reported"], false);
        assert_eq!(
            state["error"], "hub timed out",
            "the operator needs the reason, not a bare null: {state}"
        );
        assert!(
            state.get("installed").is_none(),
            "a read failure claims nothing about the host: {state}"
        );

        // ...and a successful read still produces the ordinary row.
        let ok = capability_state(Ok::<Value, String>(json!({
            "cli": [{"kind": "computer-use", "installed": true, "auth": "unknown"}]
        })));
        assert_eq!(ok["reported"], true);
        assert_eq!(ok["installed"], true);
        assert!(ok.get("error").is_none(), "{ok}");
    }
}
