//! SSH remote transport for Remuda nodes.
//!
//! Uses the system `ssh` binary (OpenSSH honours `~/.ssh/config`, ProxyJump,
//! and ssh-agent). Do not substitute russh: jump hosts and aliases would
//! diverge from the user's interactive `ssh`.
//!
//! Carriers implement [`NodeTransport`]:
//! - [`StdioTransport`]: `ssh <alias> -- <remote_bin> node --stdio` with
//!   NDJSON frames (same as `remuda-node` `StdioCarrier`)
//! - [`WssTransport`]: one WebSocket message per JSON value
//!
//! [`enroll_stdio`] translates Node NDJSON `node.hello` onto Hub `GET /v1/node`
//! JSON-RPC so an SSH stdio Node can appear in `GET /v1/hosts`.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]

mod bootstrap;
/// Clap entry used by `remuda ssh` and the `remuda-ssh` binary.
pub mod cli;
mod client;
mod enroll;
mod error;
mod frame;
mod probe;
mod target;
mod transport;

pub use bootstrap::{
    BootstrapResult, DEFAULT_REMOTE_BIN, bootstrap, default_local_musl, sha256_file,
};
pub use cli::{SshArgs, run as run_cli, run_blocking};
pub use client::{
    CONTROL_PERSIST_SECS, ExecOutput, SERVER_ALIVE_COUNT_MAX, SERVER_ALIVE_INTERVAL, SshClient,
    SshOptions, default_runtime_dir, sh_single_quote,
};
pub use enroll::{
    EnrollResult, HubEnroll, adapt_hello_for_hub, bridge_until_close, enroll_stdio, node_socket_url,
};
pub use error::Error;
pub use frame::{MAX_JSON_FRAME_BYTES, encode_json_frame, read_json_frame, write_json_frame};
pub use probe::{BinStatus, ProbeReport, probe};
pub use target::{SshTarget, list_config_hosts, list_user_hosts};
pub use transport::{Backoff, NodeTransport, StdioTransport, WssTransport, node_stdio_argv};
