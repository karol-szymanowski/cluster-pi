use cluster_common::audit::{AuditAction, AuditEntry, AuditLog, AuditPhase};
use cluster_common::crd::NodeRole;
use cluster_common::device::BlockDevice;
use cluster_common::error::ClusterError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Formatter {
    audit_log: AuditLog,
    dry_run: bool,
}

impl Formatter {
    pub fn new(audit_log_path: impl AsRef<Path>, dry_run: bool) -> Result<Self, ClusterError> {
        let audit_log = AuditLog::new(audit_log_path)?;
        Ok(Self { audit_log, dry_run })
    }

    /// Formats a block device for the specified cluster role.
    pub async fn format_device(
        &self,
        device: &BlockDevice,
        role: &NodeRole,
        requested_by: &str,
    ) -> Result<String, ClusterError> {
        let dev_path_str = device.dev_path.display().to_string();
        let serial = device.serial.clone().unwrap_or_else(|| device.name.clone());

        // 1. Write-ahead audit entry BEFORE executing destructive format
        let pre_entry = AuditEntry::builder(&serial, AuditAction::Format, requested_by)
            .device_path(&dev_path_str)
            .phase(AuditPhase::PreMutation)
            .before_state(serde_json::json!({
                "fs_type": device.fs_type,
                "uuid": device.uuid,
                "label": device.label,
                "target_role": role,
            }))
            .after_state(serde_json::json!({
                "target_role": role,
                "dry_run": self.dry_run
            }))
            .build();

        self.audit_log.record(&pre_entry)?;

        if self.dry_run {
            tracing::info!(
                device = %dev_path_str,
                role = ?role,
                "[DRY RUN] Would execute mkfs.ext4 format"
            );
            return Ok("dry-run-uuid-0000-0000".to_string());
        }

        // 2. Build mkfs command based on role
        let mut cmd = Command::new("mkfs.ext4");
        cmd.arg("-F"); // Force format

        match role {
            NodeRole::Master | NodeRole::Seed => {
                cmd.args(["-b", "4096", "-L", "ETCD_STORAGE"]);
                // Configure systemd drop-in for k3s I/O scheduling class (realtime/high priority)
                self.write_k3s_io_priority_dropin()?;
            }
            NodeRole::Worker => {
                cmd.args(["-T", "largefile4", "-L", "GFS_STORAGE"]);
            }
            _ => {
                return Err(ClusterError::Internal(format!(
                    "Invalid role {:?} for disk formatting",
                    role
                )))
            }
        }

        cmd.arg(&dev_path_str);

        tracing::info!(
            device = %dev_path_str,
            role = ?role,
            "Executing mkfs.ext4 format"
        );

        let output = cmd.output().map_err(|e| {
            let _ = self.audit_log.record(
                &AuditEntry::builder(&serial, AuditAction::Format, requested_by)
                    .device_path(&dev_path_str)
                    .phase(AuditPhase::Failed)
                    .details(serde_json::json!({ "error": e.to_string() }))
                    .build(),
            );
            ClusterError::Internal(format!("Failed to execute mkfs.ext4: {}", e))
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = self.audit_log.record(
                &AuditEntry::builder(&serial, AuditAction::Format, requested_by)
                    .device_path(&dev_path_str)
                    .phase(AuditPhase::Failed)
                    .details(serde_json::json!({ "stderr": stderr.to_string() }))
                    .build(),
            );
            return Err(ClusterError::Internal(format!(
                "mkfs.ext4 failed with exit code {:?}: {}",
                output.status.code(),
                stderr
            )));
        }

        // 3. Query new UUID using blkid
        let new_uuid =
            Self::query_uuid(&device.dev_path).unwrap_or_else(|_| "unknown-uuid".to_string());

        // 4. Post-mutation audit entry AFTER successful format
        let post_entry = AuditEntry::builder(&serial, AuditAction::Format, requested_by)
            .device_path(&dev_path_str)
            .phase(AuditPhase::PostMutation)
            .after_state(serde_json::json!({
                "fs_type": "ext4",
                "uuid": new_uuid,
                "role": role,
            }))
            .build();

        self.audit_log.record(&post_entry)?;

        Ok(new_uuid)
    }

    /// Queries the UUID of a block device via blkid.
    pub fn query_uuid(dev_path: &Path) -> Result<String, ClusterError> {
        let output = Command::new("blkid")
            .args(["-s", "UUID", "-o", "value"])
            .arg(dev_path)
            .output()
            .map_err(|e| ClusterError::Internal(format!("Failed to run blkid: {}", e)))?;

        if output.status.success() {
            let uuid = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !uuid.is_empty() {
                return Ok(uuid);
            }
        }

        Err(ClusterError::Internal(format!(
            "Failed to retrieve UUID for {:?}",
            dev_path
        )))
    }

    /// Writes systemd drop-in for k3s server to set IOSchedulingClass=1 (Realtime)
    fn write_k3s_io_priority_dropin(&self) -> Result<(), ClusterError> {
        let dropin_dir = PathBuf::from("/etc/systemd/system/k3s.service.d");
        let dropin_file = dropin_dir.join("io-priority.conf");

        let content = "[Service]\nIOSchedulingClass=1\nIOSchedulingPriority=0\n";

        if self.dry_run {
            tracing::info!(
                path = %dropin_file.display(),
                "[DRY RUN] Would write k3s systemd I/O priority drop-in"
            );
            return Ok(());
        }

        fs::create_dir_all(&dropin_dir).map_err(|e| {
            ClusterError::Internal(format!("Failed to create systemd dropin directory: {}", e))
        })?;

        fs::write(&dropin_file, content).map_err(|e| {
            ClusterError::Internal(format!("Failed to write k3s io-priority dropin: {}", e))
        })?;

        // Try reloading systemd daemon
        let _ = Command::new("systemctl").arg("daemon-reload").status();

        Ok(())
    }
}
