//! Node instance management and outbound Hub connection.

mod error;
mod inventory;
mod transport;

pub use error::NodeError;
pub use inventory::{
    CLI_KINDS, CliAuth, CliEntry, CollectRequest, Collector, DEFAULT_TTL, HerdrReport, HostSnapshot,
    ProbeEnv, ResourceReport, collect, collect_fresh,
};
pub use transport::{
    Backoff, HubRequest, JournalSender, NodeTransport, WssCarrier, WssConfig, WssLink,
};
