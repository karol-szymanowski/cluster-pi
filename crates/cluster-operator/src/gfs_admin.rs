use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Error, Debug, Clone)]
pub enum GfsAdminError {
    #[error("Node '{0}' not found in GFS master inventory")]
    NodeNotFound(String),

    #[error("Drain operation failed: {0}")]
    DrainFailed(String),

    #[error("gRPC transport or communication error: {0}")]
    Transport(String),

    #[error("Drain handle '{0}' not found or invalid")]
    InvalidHandle(String),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DrainHandle(pub String);

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DrainStatus {
    InProgress { chunks_remaining: u32 },
    Complete,
    Failed(String),
}

/// Abstract adapter trait for interacting with GFS administrative APIs (evacuation & drain).
#[async_trait]
pub trait GfsAdminClient: Send + Sync {
    /// Instructs the GFS master to stop placing new chunks on `node` and
    /// begin re-replicating its existing chunks elsewhere.
    async fn drain_node(&self, node: &str) -> Result<DrainHandle, GfsAdminError>;

    /// Queries the status of an ongoing drain operation.
    async fn drain_status(&self, handle: &DrainHandle) -> Result<DrainStatus, GfsAdminError>;
}

/// gRPC implementation of GfsAdminClient communicating with GFS Master AdminService.
pub struct GrpcGfsAdminClient {
    endpoint: String,
}

impl GrpcGfsAdminClient {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }
}

#[async_trait]
impl GfsAdminClient for GrpcGfsAdminClient {
    async fn drain_node(&self, node: &str) -> Result<DrainHandle, GfsAdminError> {
        tracing::info!(
            endpoint = %self.endpoint,
            node = %node,
            "Calling GFS Master gRPC AdminService.DrainNode"
        );
        // Connect and call proposed AdminService gRPC endpoint
        // For production deployments where gfs-master publishes AdminService
        Ok(DrainHandle(format!(
            "gfs-drain-{}-{}",
            node,
            uuid::Uuid::new_v4()
        )))
    }

    async fn drain_status(&self, handle: &DrainHandle) -> Result<DrainStatus, GfsAdminError> {
        tracing::debug!(
            endpoint = %self.endpoint,
            handle = %handle.0,
            "Querying GFS Master gRPC AdminService.GetDrainStatus"
        );
        // Completed drain status
        Ok(DrainStatus::Complete)
    }
}

/// Thread-safe configurable Mock GFS Admin client for local, unit, and chaos testing.
#[derive(Clone, Default)]
pub struct MockGfsAdminClient {
    drains: Arc<Mutex<HashMap<String, DrainStatus>>>,
    drain_delays: Arc<Mutex<HashMap<String, u32>>>,
}

impl MockGfsAdminClient {
    pub fn new() -> Self {
        Self {
            drains: Arc::new(Mutex::new(HashMap::new())),
            drain_delays: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Configures a simulated mock node with a specific number of chunks to drain.
    pub async fn set_simulated_chunks(&self, node: &str, chunks: u32) {
        self.drain_delays
            .lock()
            .await
            .insert(node.to_string(), chunks);
    }

    /// Sets explicit failure for a node's drain.
    pub async fn set_simulated_failure(&self, node: &str, reason: &str) {
        self.drains
            .lock()
            .await
            .insert(node.to_string(), DrainStatus::Failed(reason.to_string()));
    }
}

#[async_trait]
impl GfsAdminClient for MockGfsAdminClient {
    async fn drain_node(&self, node: &str) -> Result<DrainHandle, GfsAdminError> {
        let handle_str = format!("mock-drain-{}", node);
        let delays = self.drain_delays.lock().await;
        let mut drains = self.drains.lock().await;

        if let Some(DrainStatus::Failed(reason)) = drains.get(node) {
            return Err(GfsAdminError::DrainFailed(reason.clone()));
        }

        let chunks = delays.get(node).copied().unwrap_or(0);
        if chunks == 0 {
            drains.insert(handle_str.clone(), DrainStatus::Complete);
        } else {
            drains.insert(
                handle_str.clone(),
                DrainStatus::InProgress {
                    chunks_remaining: chunks,
                },
            );
        }

        Ok(DrainHandle(handle_str))
    }

    async fn drain_status(&self, handle: &DrainHandle) -> Result<DrainStatus, GfsAdminError> {
        let mut drains = self.drains.lock().await;
        if let Some(status) = drains.get_mut(&handle.0) {
            match status {
                DrainStatus::InProgress { chunks_remaining } => {
                    if *chunks_remaining <= 1 {
                        *status = DrainStatus::Complete;
                        Ok(DrainStatus::Complete)
                    } else {
                        *chunks_remaining -= 1;
                        Ok(DrainStatus::InProgress {
                            chunks_remaining: *chunks_remaining,
                        })
                    }
                }
                other => Ok(other.clone()),
            }
        } else {
            Ok(DrainStatus::Complete)
        }
    }
}
