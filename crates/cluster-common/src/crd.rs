use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Custom resource definition representing a physical or virtual Raspberry Pi node.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq)]
#[kube(
    group = "cluster.pi.io",
    version = "v1",
    kind = "PiNode",
    status = "PiNodeStatus",
    namespaced,
    shortname = "pinode",
    printcolumn = r#"{"name":"Role", "type":"string", "jsonPath":".spec.desired_role"}"#,
    printcolumn = r#"{"name":"Phase", "type":"string", "jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Disk", "type":"string", "jsonPath":".status.disk_state"}"#,
    printcolumn = r#"{"name":"Hardware Serial", "type":"string", "jsonPath":".spec.hardware_serial"}"#
)]
pub struct PiNodeSpec {
    /// Pi CPU serial number (e.g. "10000000abcdef12"), stable across reboots and reflash.
    pub hardware_serial: String,

    /// MAC address for cloud-init/PXE lookup by cluster-netboot.
    pub mac_address: String,

    /// Desired cluster role assigned to this node.
    pub desired_role: NodeRole,

    /// Target disk identifier by-id path or partition UUID (e.g. "/dev/disk/by-id/nvme-Samsung...").
    /// Never use volatile names like "/dev/sda".
    pub target_disk_id: Option<String>,

    /// Explicit confirmation flag required for any destructive format/reformat.
    #[serde(default)]
    pub reformat_confirmed: bool,

    /// Optional IP address assigned to or reported by the node.
    pub ip_address: Option<String>,

    /// Hostname assigned to the node.
    pub hostname: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
pub enum NodeRole {
    #[default]
    Pending,
    Seed,
    Master,
    Worker,
    Decommissioned,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
pub enum NodePhase {
    #[default]
    Discovered,
    Provisioning,
    Ready,
    Promoting,
    Draining,
    Decommissioning,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq, Default)]
pub enum DiskState {
    #[default]
    Unformatted,
    Formatting,
    MountedEtcd,
    MountedGfs,
    Error(String),
}

/// Status of the PiNode, tracked and updated by controllers and daemons.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Default)]
pub struct PiNodeStatus {
    /// Current lifecycle phase.
    pub phase: NodePhase,

    /// Current storage state of the assigned block device.
    pub disk_state: DiskState,

    /// Timestamp of last observed heartbeat.
    pub last_heartbeat: Option<Time>,

    /// Resumable promotion FSM snapshot if promotion/demotion is in flight.
    pub promotion: Option<PromotionState>,

    /// Mount path currently in use (e.g. "/var/lib/rancher/k3s/server/db/etcd" or "/mnt/gfs-storage").
    pub active_mount: Option<String>,

    /// Filesystem UUID currently formatted on the disk.
    pub filesystem_uuid: Option<String>,
}

/// Resumable promotion FSM snapshot persisted into PiNode.status.promotion.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
pub struct PromotionState {
    pub candidate_node: String,
    pub current_step: PromotionStep,
    pub started_at: String,
    pub updated_at: String,
    pub drain_handle: Option<String>,
    pub failure_reason: Option<String>,
    pub attempt_count: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq, Default)]
pub enum PromotionStep {
    #[default]
    Detecting,
    SelectingCandidate,
    EvacuatingGfs,
    PreparingDisk,
    ReformattingDisk,
    PromotingK3s,
    JoiningEtcd,
    Verifying,
    Complete,
    Failed,
}

/// Custom resource definition representing the cluster topology and quorum targets.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq)]
#[kube(
    group = "cluster.pi.io",
    version = "v1",
    kind = "ClusterTopology",
    status = "ClusterTopologyStatus"
)]
pub struct ClusterTopologySpec {
    /// Hardware serial of the original seed node.
    pub seed_node: String,

    /// Target number of K3s control-plane / etcd masters (default 3).
    #[serde(default = "default_target_master_count")]
    pub target_master_count: u8,

    /// Target replication factor for GFS chunks (default 3).
    #[serde(default = "default_target_gfs_replication")]
    pub target_gfs_replication: u8,

    /// Virtual IP managed by kube-vip for control plane access and PXE netboot.
    #[serde(default = "default_vip")]
    pub cluster_vip: String,
}

fn default_target_master_count() -> u8 {
    3
}

fn default_target_gfs_replication() -> u8 {
    3
}

fn default_vip() -> String {
    "192.168.1.200".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Default)]
pub struct ClusterTopologyStatus {
    pub current_master_count: u8,
    pub current_worker_count: u8,
    pub quorum_healthy: bool,
    pub seed_retired: bool,
    pub active_mutation: Option<String>,
    pub conditions: Vec<TopologyCondition>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq)]
pub struct TopologyCondition {
    pub r#type: String,
    pub status: String,
    pub last_transition_time: String,
    pub reason: String,
    pub message: String,
}
