//! Library entry for a long-running Feishu dispatcher.
//!
//! The `remuda` composition root (`codex-astra`) can wire this into a
//! subcommand later. This crate does not add a `remuda` CLI command.

use std::path::PathBuf;
use std::time::SystemTime;

use remuda_hub_client::HubClient;
use tokio::sync::mpsc;

use crate::consume::ConsumeEvent;
use crate::dispatcher::{DispatchReport, Dispatcher, RouteDefaults};
use crate::error::Error;
use crate::hub_api::HubInstanceApi;
use crate::inbound::InboundPolicy;
use crate::outbound::LarkCli;

/// Inputs for [`run_dispatcher`].
pub struct DispatcherRun {
    /// Authenticated Hub client.
    pub hub: HubClient,
    /// Outbound lark-cli wrapper (tests use DryRun).
    pub outbound: LarkCli,
    /// Owner / chat / mention policy.
    pub policy: InboundPolicy,
    /// Default host/agent/model pins.
    pub defaults: RouteDefaults,
    /// SQLite session map path. `None` uses an in-memory map.
    pub session_db: Option<PathBuf>,
}

/// Consume inbound events until the channel closes.
///
/// Intended as the body of a future `remuda feishu` / dispatcher subcommand.
pub async fn run_dispatcher(
    run: DispatcherRun,
    events: mpsc::Receiver<ConsumeEvent>,
) -> Result<Vec<DispatchReport>, Error> {
    let api = HubInstanceApi::new(run.hub);
    let mut dispatcher = match run.session_db {
        Some(path) => Dispatcher::open(path, api, run.outbound, run.policy, run.defaults)?,
        None => Dispatcher::memory(api, run.outbound, run.policy, run.defaults)?,
    };
    dispatcher.drive_consume(events, SystemTime::now()).await
}
