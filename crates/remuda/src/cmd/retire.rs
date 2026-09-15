//! `remuda retire <name|wkr_…> [--force]` — close the worker's herdr tab and
//! reclaim its worktree and per-worker target dir through the Node; M1 batch
//! 5a. Retire of a `working` worker is refused unless `--force`.

use clap::Args;
use serde_json::json;

use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

/// `remuda retire` arguments.
#[derive(Args)]
#[command(about = "Close a worker tab and reclaim its worktree and target dir.")]
pub(crate) struct RetireArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Active worker name or `wkr_…` roster id.
    worker: String,
    /// Reclaim even while the worker is still running.
    #[arg(long)]
    force: bool,
}

impl Entrypoint for RetireArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            let client = self.hub.connect()?;
            let value = client
                .post(
                    &format!("/v1/workers/{}/retire", self.worker),
                    &json!({ "force": self.force }),
                )
                .await?;
            print_json(&value)?;
            Ok(0)
        })
    }
}
