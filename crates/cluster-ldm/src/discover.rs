use cluster_common::crd::NodeRole;
use cluster_common::device::{BlockDevice, MountTable};
use cluster_common::error::DeviceError;
use cluster_common::safety::SafetyGuard;
use std::path::Path;

/// Discovers and evaluates eligible block storage devices on the local Raspberry Pi board.
pub struct DeviceDiscovery;

impl DeviceDiscovery {
    /// Scans `/sys/class/block` and `/dev/disk/by-id` and filters out root/boot disks.
    pub fn scan_eligible_devices(
        sysfs_root: impl AsRef<Path>,
        by_id_dir: impl AsRef<Path>,
        mount_table: &MountTable,
        desired_role: &NodeRole,
    ) -> Result<Vec<BlockDevice>, DeviceError> {
        let all_devices = BlockDevice::scan_all(sysfs_root, by_id_dir, mount_table)?;
        let mut eligible = Vec::new();

        for dev in &all_devices {
            // Only look at whole disks or explicitly unmounted data partitions
            if dev.is_partition {
                continue;
            }

            // Test safety filter without reformat confirmation initially to detect boot conflicts
            match SafetyGuard::evaluate_format(dev, desired_role, true, mount_table, &all_devices) {
                Ok(_) => {
                    eligible.push(dev.clone());
                }
                Err(err) => {
                    tracing::debug!(
                        device = %dev.name,
                        reason = %err,
                        "Skipping ineligible or protected device"
                    );
                }
            }
        }

        Ok(eligible)
    }

    /// Resolves a specific requested disk identifier against discovered devices.
    pub fn resolve_target_disk(
        target_id: Option<&str>,
        sysfs_root: impl AsRef<Path>,
        by_id_dir: impl AsRef<Path>,
        mount_table: &MountTable,
        desired_role: &NodeRole,
    ) -> Result<BlockDevice, DeviceError> {
        let eligible =
            Self::scan_eligible_devices(sysfs_root, by_id_dir, mount_table, desired_role)?;

        if let Some(target) = target_id {
            for dev in eligible {
                if dev.matches_identifier(target) {
                    return Ok(dev);
                }
            }
            return Err(DeviceError::NotFound(format!(
                "Requested target disk '{}' was not found among eligible non-boot devices",
                target
            )));
        }

        // If no explicit target was supplied and exactly one eligible data drive exists, pick it
        if eligible.len() == 1 {
            return Ok(eligible.into_iter().next().unwrap());
        }

        if eligible.is_empty() {
            Err(DeviceError::NotFound(
                "No eligible non-boot block storage devices found on this node".into(),
            ))
        } else {
            Err(DeviceError::Resolution(format!(
                "Multiple eligible data disks found ({}); an explicit 'target_disk_id' is required",
                eligible
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }
}
