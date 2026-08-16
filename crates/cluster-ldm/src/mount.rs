use crate::fstab::FstabManager;
use cluster_common::audit::{AuditAction, AuditEntry, AuditLog, AuditPhase};
use cluster_common::crd::NodeRole;
use cluster_common::device::BlockDevice;
use cluster_common::error::ClusterError;
use std::fs;
use std::path::Path;
use std::process::Command;

pub struct Mounter {
    audit_log: AuditLog,
    fstab_manager: FstabManager,
    dry_run: bool,
}

impl Mounter {
    pub fn new(
        audit_log_path: impl AsRef<Path>,
        fstab_path: impl AsRef<Path>,
        dry_run: bool,
    ) -> Result<Self, ClusterError> {
        let audit_log = AuditLog::new(audit_log_path)?;
        let fstab_manager = FstabManager::new(fstab_path);
        Ok(Self {
            audit_log,
            fstab_manager,
            dry_run,
        })
    }

    /// Determines mount destination and options for a given node role.
    pub fn get_mount_config(role: &NodeRole) -> (&'static str, &'static str) {
        match role {
            NodeRole::Master | NodeRole::Seed => (
                "/var/lib/rancher/k3s/server/db/etcd",
                "data=ordered,barrier=1,noatime",
            ),
            NodeRole::Worker => ("/mnt/gfs-storage", "commit=60,noatime"),
            _ => ("/mnt/storage", "noatime"),
        }
    }

    /// Mounts the block device by UUID, updates fstab, and writes audit records.
    pub async fn mount_role_storage(
        &self,
        device: &BlockDevice,
        uuid: &str,
        role: &NodeRole,
        requested_by: &str,
    ) -> Result<String, ClusterError> {
        let (mount_point, mount_opts) = Self::get_mount_config(role);
        let serial = device.serial.clone().unwrap_or_else(|| device.name.clone());
        let dev_path_str = device.dev_path.display().to_string();

        // 1. Audit pre-mutation
        let pre_entry = AuditEntry::builder(&serial, AuditAction::Mount, requested_by)
            .device_path(&dev_path_str)
            .phase(AuditPhase::PreMutation)
            .before_state(serde_json::json!({
                "uuid": uuid,
                "current_mounts": device.mount_points,
            }))
            .after_state(serde_json::json!({
                "mount_point": mount_point,
                "options": mount_opts,
                "role": role,
            }))
            .build();

        self.audit_log.record(&pre_entry)?;

        if self.dry_run {
            tracing::info!(
                mount_point = %mount_point,
                options = %mount_opts,
                "[DRY RUN] Would mount UUID={} to {}",
                uuid, mount_point
            );
            return Ok(mount_point.to_string());
        }

        // 2. Ensure mount directory exists
        fs::create_dir_all(mount_point).map_err(|e| {
            ClusterError::Internal(format!("Failed to create mount dir {}: {}", mount_point, e))
        })?;

        // 3. Execute mount command: mount -o <opts> UUID=<uuid> <mount_point>
        let uuid_arg = if uuid.starts_with("dry-run") || uuid == "unknown-uuid" {
            dev_path_str.clone()
        } else {
            format!("UUID={}", uuid)
        };

        let status = Command::new("mount")
            .args(["-o", mount_opts, &uuid_arg, mount_point])
            .status()
            .map_err(|e| ClusterError::Internal(format!("Failed to invoke mount: {}", e)))?;

        if !status.success() {
            let _ = self.audit_log.record(
                &AuditEntry::builder(&serial, AuditAction::Mount, requested_by)
                    .device_path(&dev_path_str)
                    .phase(AuditPhase::Failed)
                    .details(serde_json::json!({ "exit_code": status.code() }))
                    .build(),
            );
            return Err(ClusterError::Internal(format!(
                "Mount command failed for UUID={} to {}",
                uuid, mount_point
            )));
        }

        // 4. Update /etc/fstab idempotently
        self.fstab_manager
            .upsert_entry(uuid, mount_point, "ext4", mount_opts)?;

        // 5. Audit post-mutation
        let post_entry = AuditEntry::builder(&serial, AuditAction::Mount, requested_by)
            .device_path(&dev_path_str)
            .phase(AuditPhase::PostMutation)
            .after_state(serde_json::json!({
                "mount_point": mount_point,
                "uuid": uuid,
                "status": "mounted",
            }))
            .build();

        self.audit_log.record(&post_entry)?;

        Ok(mount_point.to_string())
    }

    /// Unmounts a path cleanly during node evacuation or demotion.
    pub async fn unmount_path(
        &self,
        mount_point: &str,
        requested_by: &str,
    ) -> Result<(), ClusterError> {
        let pre_entry = AuditEntry::builder("UNMOUNT", AuditAction::Unmount, requested_by)
            .phase(AuditPhase::PreMutation)
            .before_state(serde_json::json!({ "mount_point": mount_point }))
            .build();

        self.audit_log.record(&pre_entry)?;

        if self.dry_run {
            tracing::info!(mount_point = %mount_point, "[DRY RUN] Would unmount");
            return Ok(());
        }

        let status = Command::new("umount")
            .arg(mount_point)
            .status()
            .map_err(|e| ClusterError::Internal(format!("Failed to execute umount: {}", e)))?;

        if !status.success() {
            return Err(ClusterError::Internal(format!(
                "umount failed for {}",
                mount_point
            )));
        }

        // Remove from fstab
        let _ = self.fstab_manager.remove_entry(mount_point);

        let post_entry = AuditEntry::builder("UNMOUNT", AuditAction::Unmount, requested_by)
            .phase(AuditPhase::PostMutation)
            .after_state(serde_json::json!({ "mount_point": mount_point, "status": "unmounted" }))
            .build();

        self.audit_log.record(&post_entry)?;

        Ok(())
    }
}
