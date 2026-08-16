//! `cluster-common` - Shared contracts, safety guards, audit logging, device models, and election gates for pi-cluster-core.

pub mod audit;
pub mod crd;
pub mod device;
pub mod election;
pub mod error;
pub mod safety;

pub use audit::{AuditAction, AuditEntry, AuditLog, AuditPhase};
pub use crd::{
    ClusterTopology, ClusterTopologySpec, ClusterTopologyStatus, DiskState, NodePhase, NodeRole,
    PiNode, PiNodeSpec, PiNodeStatus, PromotionState, PromotionStep,
};
pub use device::{BlockDevice, MountEntry, MountTable};
pub use election::LeaseElector;
pub use error::{AuditError, ClusterError, DeviceError, ElectionError, SafetyError};
pub use safety::{FormatDecision, SafetyGuard, STATIC_BOOT_DENYLIST};
