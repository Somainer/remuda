//! `remuda hostcap <host>` — current cores/load/disk, free worktree slots,
//! and the port blocks already in use on a host; the dispatch precondition in
//! coordinator-hierarchy.md §7 #6 / §2.2.

use clap::Args;

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
            let value = client
                .get(&format!("/v1/hosts/{}/hostcap", self.host))
                .await?;
            print_json(&value)?;
            Ok(0)
        })
    }
}
