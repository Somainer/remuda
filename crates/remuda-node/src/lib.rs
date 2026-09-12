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
mod reclaim;
mod runtime;
mod runtime_link;
mod server;
mod service;
mod stdio;
mod store;
mod transport;
mod tty;
mod worktree;

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
pub use enroll::{
    Enrollment, apply_hello_result, default_data_dir, load_or_create as load_or_create_enrollment,
    save as save_enrollment,
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
pub use server::{DevServer, dev_router, dispatch_hub_rpc};
pub use service::{LocalDrivers, RunningNode, ServeConfig, compose, serve};
pub use stdio::{
    StdioOptions, run_stdio, run_stdio_opts, run_stdio_runtime_opts, run_stdio_with_node,
};
pub use store::{LocalStore, MemoryStore};
pub use transport::{
    Backoff, HubRequest, JournalSender, NodeTransport, TransportMetrics, TransportMetricsSnapshot,
    WssCarrier, WssConfig, WssLink,
};
pub use tty::{
    TTY_CHANNEL_INPUT, TTY_CHANNEL_OUTPUT, TTY_DEFAULT_COLS, TTY_DEFAULT_ROWS,
    TTY_FRAME_HEADER_BYTES, TTY_MAX_INPUT_BYTES, TtyAttach, TtyEvent, TtyRegistry,
    decode_tty_input, encode_tty_frame, encode_tty_input,
};
