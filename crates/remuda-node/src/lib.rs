//! Node instance management and outbound Hub connection.

mod carrier;
mod config;
mod driver;
mod enroll;
mod entity;
mod error;
mod identity;
mod interactions;
mod inventory;
mod model;
mod native;
mod runtime;
mod runtime_link;
mod server;
mod service;
mod stdio;
mod store;
mod transport;
mod tty;

pub use carrier::{
    AuthState, CarrierFuture, CarrierKind, CliInventory, HerdrInventory, HostInventory,
    HostInventoryConfig, HubCarrier, NodeHello, NodeHelloParams, NodeHelloProtocol,
    OutboundWssCarrier, StdioCarrier,
};
pub use config::{DEFAULT_DEV_PORT, DevServerConfig};
pub use driver::{
    Driver, DriverEmission, DriverError, DriverFactory, DriverFuture, DriverLaunch, DriverRegistry,
    DriverRequest, DriverStartFuture, FakeDriver,
};
pub use error::NodeError;
pub use identity::load_or_create_host_id;
pub use interactions::{InteractionRuntime, PendingInteraction};
pub use inventory::{
    CLI_KINDS, CliAuth, CliEntry, CollectRequest, Collector, DEFAULT_TTL, HerdrReport,
    HostSnapshot, ProbeEnv, ResourceReport, collect, collect_fresh,
};
pub use model::{
    CommandAction, CreateInstanceRequest, CreateInstanceResponse, InstanceCommandRequest,
};
pub use native::{NativeDriverConfig, native_driver_registry};
pub use runtime::DevNode;
pub use runtime_link::attach_runtime;
pub use server::{DevServer, dev_router};
pub use service::{LocalDrivers, RunningNode, ServeConfig, compose, serve};
pub use stdio::{StdioOptions, run_stdio, run_stdio_opts};
pub use store::{LocalStore, MemoryStore};
pub use transport::{
    Backoff, HubRequest, JournalSender, NodeTransport, WssCarrier, WssConfig, WssLink,
};
pub use tty::{TTY_CHANNEL_OUTPUT, TTY_FRAME_HEADER_BYTES, encode_tty_frame};
