//! Local development server configuration.

use crate::NodeError;
use std::{fmt, net::SocketAddr, path::PathBuf, sync::Arc};

/// Default port used by `remuda dev`.
pub const DEFAULT_DEV_PORT: u16 = 8787;

/// Configuration for the local Node HTTP and WebSocket surface.
#[derive(Clone)]
pub struct DevServerConfig {
    /// Address on which the development server listens.
    pub bind_addr: SocketAddr,
    /// Origins allowed to make browser CORS requests.
    pub allowed_origins: Vec<String>,
    /// Workspace exposed by the single development registry entry.
    pub workspace_root: PathBuf,
    /// Capacity of each instance's independent driver command queue.
    pub instance_queue_capacity: usize,
    /// Capacity of each instance's journal broadcast ring.
    pub follow_buffer_capacity: usize,
    access_code: Option<Arc<str>>,
}

impl DevServerConfig {
    /// Construct a loopback-only configuration with no access code.
    pub fn loopback(port: u16) -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            allowed_origins: vec![
                "http://localhost:5173".to_owned(),
                "http://127.0.0.1:5173".to_owned(),
            ],
            workspace_root: PathBuf::from("."),
            instance_queue_capacity: 32,
            follow_buffer_capacity: 256,
            access_code: None,
        }
    }

    /// Bind to all IPv4 interfaces and require the supplied non-empty access code.
    pub fn lan(port: u16, access_code: String) -> Result<Self, NodeError> {
        let mut config = Self::loopback(port);
        config.bind_addr = SocketAddr::from(([0, 0, 0, 0], port));
        config.set_access_code(access_code)?;
        Ok(config)
    }

    /// Replace the browser origin allowlist.
    pub fn with_allowed_origins(mut self, origins: Vec<String>) -> Result<Self, NodeError> {
        if origins.is_empty() || origins.iter().any(|origin| origin.trim().is_empty()) {
            return Err(NodeError::InvalidConfig(
                "at least one non-empty web origin is required".to_owned(),
            ));
        }
        self.allowed_origins = origins;
        Ok(self)
    }

    /// Set the development workspace root advertised by the local registry.
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = root;
        self
    }

    /// Enable access-code authentication.
    pub fn with_access_code(mut self, access_code: String) -> Result<Self, NodeError> {
        self.set_access_code(access_code)?;
        Ok(self)
    }

    pub(crate) fn access_code(&self) -> Option<&str> {
        self.access_code.as_deref()
    }

    fn set_access_code(&mut self, access_code: String) -> Result<(), NodeError> {
        let access_code = access_code.trim();
        if access_code.is_empty() {
            return Err(NodeError::InvalidConfig(
                "the access code file must contain a non-empty value".to_owned(),
            ));
        }
        self.access_code = Some(Arc::<str>::from(access_code));
        Ok(())
    }
}

impl Default for DevServerConfig {
    fn default() -> Self {
        Self::loopback(DEFAULT_DEV_PORT)
    }
}

impl fmt::Debug for DevServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DevServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("allowed_origins", &self.allowed_origins)
            .field("workspace_root", &self.workspace_root)
            .field("instance_queue_capacity", &self.instance_queue_capacity)
            .field("follow_buffer_capacity", &self.follow_buffer_capacity)
            .field("access_code_configured", &self.access_code.is_some())
            .finish()
    }
}
