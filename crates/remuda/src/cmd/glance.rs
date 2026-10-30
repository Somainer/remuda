//! `remuda glance [<lineage>]` — the workers a lineage dispatched that are
//! waiting on someone or working, plus today's DONE count (cb-digest).
//!
//! One read-only call to `GET /v1/lineages/{id}/glance`. With no argument an
//! agent glances its own lineage (named by `/v1/caller`); an operator names
//! one explicitly. Rows are capped at 50 per list; `more.waiting` and
//! `more.working` carry the overflow. Row `question` text is untrusted data.

use anyhow::{Context, bail};
use clap::Args;

use super::hub_client::{HubOpts, block_on, print_json};
use super::registry::Entrypoint;

/// `remuda glance` arguments.
#[derive(Args)]
#[command(about = "Show a lineage's waiting and working workers and today's DONE count.")]
pub(crate) struct GlanceArgs {
    #[command(flatten)]
    hub: HubOpts,
    /// Lineage id (the first chapter's `ins_…`). Defaults to the caller's own
    /// lineage when run from inside an agent session.
    lineage: Option<String>,
}

impl Entrypoint for GlanceArgs {
    fn enter(self, _context: super::registry::Context) -> anyhow::Result<i32> {
        block_on(async move {
            let client = self.hub.connect()?;
            let lineage = match self.lineage {
                Some(lineage) => lineage,
                None => {
                    let caller = client.get("/v1/caller").await?;
                    match caller.get("lineageId").and_then(|value| value.as_str()) {
                        Some(lineage) => lineage.to_owned(),
                        None => bail!(
                            "no lineage to glance: this credential is not bound to an agent \
                             instance; pass a lineage id"
                        ),
                    }
                }
            };
            let value = client
                .get(&format!("/v1/lineages/{lineage}/glance"))
                .await
                .with_context(|| format!("glance of lineage {lineage}"))?;
            print_json(&value)?;
            Ok(0)
        })
    }
}
