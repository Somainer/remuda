//! Composed local Node service with durable observations and selectable drivers.

use crate::{
    DevNode, DevServerConfig, DriverRegistry, MemoryStore, NativeDriverConfig, NodeError,
    dev_router, native_driver_registry,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::{sync::oneshot, task::JoinHandle};

/// Driver set installed in a composed local Node.
#[derive(Debug, Clone)]
pub enum LocalDrivers {
    /// Deterministic in-process driver used by API tests.
    Fake,
    /// Real `claude-print`, `claude-pty`, and `claude-bg` adapters.
    Native(NativeDriverConfig),
}

/// Inputs for [`serve`].
#[derive(Debug, Clone)]
pub struct ServeConfig {
    /// HTTP/WebSocket listener and local workspace policy.
    pub http: DevServerConfig,
    /// Node-owned directory containing the SQLite journal and launch state.
    pub data_dir: PathBuf,
    /// Per-instance driver factories exposed by this process.
    pub drivers: LocalDrivers,
}

impl ServeConfig {
    /// Compose a durable Node with all three native Claude drivers.
    pub fn native(http: DevServerConfig, data_dir: PathBuf) -> Self {
        Self {
            http,
            drivers: LocalDrivers::Native(NativeDriverConfig::new(data_dir.clone())),
            data_dir,
        }
    }

    /// Compose a durable Node with the deterministic fake driver.
    pub fn fake(http: DevServerConfig, data_dir: PathBuf) -> Self {
        Self {
            http,
            data_dir,
            drivers: LocalDrivers::Fake,
        }
    }
}

/// Bound local Node and its graceful-shutdown handle.
pub struct RunningNode {
    addr: SocketAddr,
    node: DevNode,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), NodeError>>>,
}

impl RunningNode {
    /// Actual bound address; an input port of zero is replaced by this port.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Clone the application runtime for an outbound Hub carrier.
    pub fn node(&self) -> DevNode {
        self.node.clone()
    }

    /// Wait for an unexpected listener exit without requesting shutdown.
    pub async fn wait(&mut self) -> Result<(), NodeError> {
        let result = match self.task.as_mut() {
            Some(task) => task
                .await
                .map_err(|error| NodeError::Driver(format!("Node server task failed: {error}")))?,
            None => return Ok(()),
        };
        self.task = None;
        result
    }

    /// Stop accepting new connections and wait for upgraded connections to drain.
    pub async fn shutdown(mut self) -> Result<(), NodeError> {
        self.node.shutdown().await?;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.task.take() {
            Some(task) => task
                .await
                .map_err(|error| NodeError::Driver(format!("Node server task failed: {error}")))?,
            None => Ok(()),
        }
    }
}

impl Drop for RunningNode {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Compose the durable Node runtime without binding an HTTP listener.
pub fn compose(config: &ServeConfig) -> Result<DevNode, NodeError> {
    let store = Arc::new(MemoryStore::open_journaled(
        &config.data_dir,
        config.http.follow_buffer_capacity,
    )?);
    let drivers = match &config.drivers {
        LocalDrivers::Fake => DriverRegistry::with_fake()?,
        LocalDrivers::Native(native) => native_driver_registry(native.clone())?,
    };
    let host_id = crate::enroll::load_or_create(&config.data_dir)?.host_id;
    let node = DevNode::with_parts_on_host(&config.http, store, drivers, host_id)?;
    node.configure_doctor(crate::DoctorContext {
        data_dir: Some(config.data_dir.clone()),
        listeners: Vec::new(),
    })?;
    Ok(match &config.drivers {
        LocalDrivers::Native(native) => node.with_herdr_config(native.clone())?,
        LocalDrivers::Fake => node,
    })
}

/// Bind and spawn a durable local Node service.
pub async fn serve(config: ServeConfig) -> Result<RunningNode, NodeError> {
    let node = compose(&config)?;
    node.reconcile_herdr().await?;
    let listener = tokio::net::TcpListener::bind(config.http.bind_addr).await?;
    let addr = listener.local_addr()?;
    node.configure_doctor(crate::DoctorContext {
        data_dir: Some(config.data_dir.clone()),
        listeners: vec![addr],
    })?;
    let app = dev_router(node.clone(), &config.http);
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
            .map_err(NodeError::from)
    });
    tracing::info!(%addr, "local Node listening");
    Ok(RunningNode {
        addr,
        node,
        stop: Some(stop),
        task: Some(task),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serve_binds_ephemeral_port_and_stops_cleanly() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let running = serve(ServeConfig::fake(
            DevServerConfig::loopback(0),
            data_dir.path().to_path_buf(),
        ))
        .await
        .expect("running Node");
        assert!(running.addr().ip().is_loopback());
        assert_ne!(running.addr().port(), 0);
        assert_eq!(
            running
                .node()
                .list_instances()
                .expect("instances")
                .items
                .len(),
            0
        );
        running.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn compose_reuses_the_persisted_host_identity() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let config = ServeConfig::fake(DevServerConfig::loopback(0), data_dir.path().to_path_buf());
        let first = compose(&config).expect("first Node").host().meta.id;
        let second = compose(&config).expect("second Node").host().meta.id;
        assert_eq!(first, second);
        let enrollment = crate::enroll::load_or_create(data_dir.path()).expect("enrollment");
        assert_eq!(first, enrollment.host_id);
    }
}
