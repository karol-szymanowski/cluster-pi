# Proposed Upstream Interface: `gfs-rs` AdminService

## Background & Motivation

In the `pi-cluster-core` autonomous platform, Raspberry Pi worker nodes host GFS chunkservers. When a master node fails, the Quorum Auto-Healer in `cluster-operator` needs to dynamically promote a surviving worker node to a master (etcd quorum) node.

Prior to reformatting the worker's storage device for etcd use (`/var/lib/rancher/k3s/server/db/etcd`), all GFS chunks hosted on that worker's chunkserver must be safely evacuated (drained) and re-replicated to other surviving chunkservers in the cluster.

Currently, `gfs-rs`'s `ClientMasterService` does not expose a node evacuation or chunk drain endpoint. This document specifies the proposed gRPC contract to be added to `gfs-proto` (and implemented in `gfs-master`).

---

## Proposed Protocol Buffer Definition (`gfs-proto/proto/admin.proto`)

```protobuf
syntax = "proto3";

package gfs.admin.v1;

service AdminService {
    // Initiates draining of a chunkserver node.
    // The master marks the node as draining, ceases placing new chunk allocations on it,
    // and schedules replication tasks to copy all existing chunks to other active chunkservers.
    rpc DrainNode(DrainNodeRequest) returns (DrainNodeResponse);

    // Queries the status of an ongoing or completed drain operation.
    rpc GetDrainStatus(GetDrainStatusRequest) returns (GetDrainStatusResponse);

    // Cancels an in-progress node drain operation if needed.
    rpc CancelDrain(CancelDrainRequest) returns (CancelDrainResponse);
}

message DrainNodeRequest {
    // Unique identifier of the node / chunkserver to drain
    string node_id = 1;
    // Timeout in seconds before drain is considered stalled (optional, 0 = default)
    uint64 timeout_seconds = 2;
    // Reason for draining (for audit and telemetry logs)
    string reason = 3;
}

message DrainNodeResponse {
    // Unique handle or task ID for polling drain status
    string drain_handle = 1;
    // Estimated number of chunks to re-replicate
    uint32 total_chunks = 2;
}

message GetDrainStatusRequest {
    string drain_handle = 1;
    string node_id = 2;
}

enum DrainState {
    DRAIN_STATE_UNSPECIFIED = 0;
    DRAIN_STATE_IN_PROGRESS = 1;
    DRAIN_STATE_COMPLETE = 2;
    DRAIN_STATE_FAILED = 3;
    DRAIN_STATE_CANCELLED = 4;
}

message GetDrainStatusResponse {
    string drain_handle = 1;
    string node_id = 2;
    DrainState state = 3;
    uint32 chunks_remaining = 4;
    uint32 chunks_completed = 5;
    string failure_reason = 6;
}

message CancelDrainRequest {
    string drain_handle = 1;
    string reason = 2;
}

message CancelDrainResponse {
    bool cancelled = 1;
    string message = 2;
}
```

---

## Rust Client Trait in `cluster-operator`

Inside `pi-cluster-core`, `cluster-operator` interfaces with GFS through the `GfsAdminClient` trait:

```rust
use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainHandle(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainStatus {
    InProgress { chunks_remaining: u32 },
    Complete,
    Failed(String),
}

#[async_trait]
pub trait GfsAdminClient: Send + Sync {
    /// Instructs the GFS master to stop placing new chunks on `node` and
    /// begin re-replicating its existing chunks elsewhere. Returns immediately;
    /// poll `drain_status` for completion.
    async fn drain_node(&self, node: &str) -> Result<DrainHandle, GfsAdminError>;

    /// Queries the status of an ongoing drain handle.
    async fn drain_status(&self, handle: &DrainHandle) -> Result<DrainStatus, GfsAdminError>;
}
```

Both `GrpcGfsAdminClient` (calling the gRPC endpoint above) and `MockGfsAdminClient` (for offline unit and chaos testing) are implemented in `crates/cluster-operator/src/gfs_admin.rs`.
