use thiserror::Error;

#[derive(Error, Debug)]
pub enum SafetyError {
    #[error("Target device '{device}' is a protected boot device (signal: {signal})")]
    BootDiskViolation { device: String, signal: String },

    #[error("Safety check produced ambiguous result for device '{device}': {reason}")]
    AmbiguousDevice { device: String, reason: String },

    #[error("Destructive reformat of device '{device}' rejected: explicit 'reformat_confirmed' flag is false")]
    MissingConfirmation { device: String },

    #[error("Device '{device}' not found or could not be resolved")]
    DeviceNotFound { device: String },

    #[error("Failed to inspect live mounts: {0}")]
    MountInspectionFailed(String),

    #[error("I/O error during safety evaluation: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum AuditError {
    #[error("Audit log I/O failed: {0}")]
    Io(#[from] std::io::Error),

    #[error("Audit log serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Failed to fsync audit record: {0}")]
    FsyncFailed(String),
}

#[derive(Error, Debug)]
pub enum DeviceError {
    #[error("Device '{0}' was not found in sysfs or disk inventory")]
    NotFound(String),

    #[error("Failed to read sysfs entry at '{path}': {source}")]
    SysfsRead {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to parse mount table: {0}")]
    InvalidMountTable(String),

    #[error("Device resolution error: {0}")]
    Resolution(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Error, Debug)]
pub enum ElectionError {
    #[error("Kubernetes client error: {0}")]
    Kube(#[from] kube::Error),

    #[error("Lease '{lease_name}' was lost or stolen")]
    LeaseLost { lease_name: String },

    #[error("Election operation was cancelled")]
    Cancelled,

    #[error("Election timed out: {0}")]
    Timeout(String),
}

#[derive(Error, Debug)]
pub enum ClusterError {
    #[error(transparent)]
    Safety(#[from] SafetyError),

    #[error(transparent)]
    Audit(#[from] AuditError),

    #[error(transparent)]
    Device(#[from] DeviceError),

    #[error(transparent)]
    Election(#[from] ElectionError),

    #[error("Internal cluster error: {0}")]
    Internal(String),
}
