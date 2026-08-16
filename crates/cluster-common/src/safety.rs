use crate::crd::NodeRole;
use crate::device::{BlockDevice, MountTable};
use crate::error::SafetyError;
use std::path::Path;

/// Static denylist of known Raspberry Pi boot device names, SD controllers, and virtual devices.
pub const STATIC_BOOT_DENYLIST: &[&str] = &[
    "mmcblk0",
    "mmcblk0boot0",
    "mmcblk0boot1",
    "mmcblk0rpmb",
    "mmcblk1",
    "mmcblk1boot0",
    "mmcblk1boot1",
    "ram0",
    "ram1",
    "loop0",
    "zram0",
];

/// Result of safety evaluation on a block device prior to format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatDecision {
    /// Device is safe to format with the requested filesystem parameters.
    ProceedWithFormat,
    /// Device is already formatted and matches the desired role's layout; formatting must be skipped.
    SkipFormatAlreadyMatching {
        existing_fs: String,
        target_mount: String,
    },
}

pub struct SafetyGuard;

impl SafetyGuard {
    /// Validates that a block device is safe for formatting, enforcing all Section 1.2 rules.
    pub fn evaluate_format(
        target: &BlockDevice,
        desired_role: &NodeRole,
        reformat_confirmed: bool,
        mount_table: &MountTable,
        all_devices: &[BlockDevice],
    ) -> Result<FormatDecision, SafetyError> {
        let dev_name = &target.name;

        // 1. Static denylist verification
        for denied in STATIC_BOOT_DENYLIST {
            if dev_name == *denied || dev_name.starts_with(denied) {
                return Err(SafetyError::BootDiskViolation {
                    device: target.dev_path.display().to_string(),
                    signal: format!("Matched static boot denylist pattern '{}'", denied),
                });
            }
        }

        // 2. Multi-signal live root mount verification (parsing /proc/mounts for /)
        if let Some(root_dev) = mount_table.get_root_device() {
            if Self::is_device_or_parent_match(target, root_dev, all_devices)? {
                return Err(SafetyError::BootDiskViolation {
                    device: target.dev_path.display().to_string(),
                    signal: format!(
                        "Device backs active root filesystem ('/') via '{}'",
                        root_dev
                    ),
                });
            }
        } else {
            return Err(SafetyError::AmbiguousDevice {
                device: target.dev_path.display().to_string(),
                reason: "Unable to resolve active root filesystem ('/') from mount table".into(),
            });
        }

        // 3. Multi-signal boot / firmware mount verification
        for boot_dev in mount_table.get_boot_devices() {
            if Self::is_device_or_parent_match(target, boot_dev, all_devices)? {
                return Err(SafetyError::BootDiskViolation {
                    device: target.dev_path.display().to_string(),
                    signal: format!(
                        "Device backs active boot partition ('/boot' or '/boot/firmware') via '{}'",
                        boot_dev
                    ),
                });
            }
        }

        // 4. Any active mounts currently on this device
        if !target.mount_points.is_empty() {
            // If it's already mounted, check if it matches the target role
            let expected_mount = match desired_role {
                NodeRole::Master | NodeRole::Seed => "/var/lib/rancher/k3s/server/db/etcd",
                NodeRole::Worker => "/mnt/gfs-storage",
                _ => {
                    return Err(SafetyError::AmbiguousDevice {
                        device: target.dev_path.display().to_string(),
                        reason: format!(
                            "Cannot format device for non-operational role {:?}",
                            desired_role
                        ),
                    })
                }
            };

            if target.mount_points.iter().any(|m| m == expected_mount) {
                if let Some(ref fs) = target.fs_type {
                    if fs == "ext4" {
                        return Ok(FormatDecision::SkipFormatAlreadyMatching {
                            existing_fs: fs.clone(),
                            target_mount: expected_mount.to_string(),
                        });
                    }
                }
            }
        }

        // 5. Idempotency inspection: existing filesystem vs reformat confirmation
        let has_existing_fs = target.fs_type.is_some() || target.uuid.is_some();
        if has_existing_fs && !reformat_confirmed {
            return Err(SafetyError::MissingConfirmation {
                device: target.dev_path.display().to_string(),
            });
        }

        Ok(FormatDecision::ProceedWithFormat)
    }

    /// Determines whether `target` is the same device, partition, or parent of `mounted_spec`.
    fn is_device_or_parent_match(
        target: &BlockDevice,
        mounted_spec: &str,
        all_devices: &[BlockDevice],
    ) -> Result<bool, SafetyError> {
        let mounted_path = Path::new(mounted_spec);

        // Direct path match (/dev/sda == /dev/sda)
        if target.dev_path == mounted_path {
            return Ok(true);
        }

        // By-id symlink match
        if target.by_id_paths.iter().any(|p| p == mounted_path) {
            return Ok(true);
        }

        // Check if mounted_spec is a partition of this target device
        // e.g. target = "nvme0n1", mounted_spec = "/dev/nvme0n1p2"
        let mounted_name = mounted_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if !mounted_name.is_empty() {
            if mounted_name.starts_with(&target.name) {
                return Ok(true);
            }

            // Check if target is a partition of mounted device or vice versa via parent_disk
            if let Some(ref parent) = target.parent_disk {
                if mounted_name == parent || mounted_name.starts_with(parent) {
                    return Ok(true);
                }
            }

            // Cross-reference with all discovered devices
            if let Some(mounted_dev) = all_devices.iter().find(|d| d.name == mounted_name) {
                if let Some(ref parent) = mounted_dev.parent_disk {
                    if parent == &target.name {
                        return Ok(true);
                    }
                }
            }
        }

        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::MountEntry;
    use std::path::PathBuf;

    fn sample_mount_table() -> MountTable {
        MountTable {
            entries: vec![
                MountEntry {
                    device: "/dev/mmcblk0p2".into(),
                    mount_point: "/".into(),
                    fs_type: "ext4".into(),
                    options: vec!["rw".into()],
                },
                MountEntry {
                    device: "/dev/mmcblk0p1".into(),
                    mount_point: "/boot/firmware".into(),
                    fs_type: "vfat".into(),
                    options: vec!["rw".into()],
                },
            ],
        }
    }

    fn make_test_device(name: &str, is_partition: bool, parent: Option<&str>) -> BlockDevice {
        BlockDevice {
            name: name.to_string(),
            dev_path: PathBuf::from(format!("/dev/{}", name)),
            sysfs_path: PathBuf::from(format!("/sys/class/block/{}", name)),
            size_bytes: 1_000_000_000,
            by_id_paths: vec![PathBuf::from(format!("/dev/disk/by-id/test-{}", name))],
            uuid: None,
            label: None,
            fs_type: None,
            model: Some("TestDrive".into()),
            serial: Some(format!("SER-{}", name)),
            is_partition,
            parent_disk: parent.map(|s| s.to_string()),
            mount_points: Vec::new(),
        }
    }

    #[test]
    fn test_refuse_sd_boot_devices() {
        let mounts = sample_mount_table();
        let sd_disk = make_test_device("mmcblk0", false, None);
        let sd_part = make_test_device("mmcblk0p2", true, Some("mmcblk0"));

        let res1 = SafetyGuard::evaluate_format(
            &sd_disk,
            &NodeRole::Worker,
            true,
            &mounts,
            &[sd_disk.clone(), sd_part.clone()],
        );
        assert!(matches!(res1, Err(SafetyError::BootDiskViolation { .. })));

        let res2 = SafetyGuard::evaluate_format(
            &sd_part,
            &NodeRole::Worker,
            true,
            &mounts,
            &[sd_disk.clone(), sd_part.clone()],
        );
        assert!(matches!(res2, Err(SafetyError::BootDiskViolation { .. })));
    }

    #[test]
    fn test_refuse_nvme_when_mounted_as_root() {
        let mut mounts = MountTable::default();
        mounts.entries.push(MountEntry {
            device: "/dev/nvme0n1p2".into(),
            mount_point: "/".into(),
            fs_type: "ext4".into(),
            options: vec![],
        });

        let nvme_disk = make_test_device("nvme0n1", false, None);
        let nvme_part = make_test_device("nvme0n1p2", true, Some("nvme0n1"));

        // Whole disk formatting must be blocked because its partition is root
        let res = SafetyGuard::evaluate_format(
            &nvme_disk,
            &NodeRole::Master,
            true,
            &mounts,
            &[nvme_disk.clone(), nvme_part.clone()],
        );
        assert!(matches!(res, Err(SafetyError::BootDiskViolation { .. })));
    }

    #[test]
    fn test_require_confirmation_for_existing_filesystem() {
        let mounts = sample_mount_table();
        let mut data_disk = make_test_device("nvme1n1", false, None);
        data_disk.fs_type = Some("ext4".into());
        data_disk.uuid = Some("abcd-1234".into());

        // Without confirmation -> Err
        let res_no_confirm = SafetyGuard::evaluate_format(
            &data_disk,
            &NodeRole::Worker,
            false,
            &mounts,
            &[data_disk.clone()],
        );
        assert!(matches!(
            res_no_confirm,
            Err(SafetyError::MissingConfirmation { .. })
        ));

        // With confirmation -> Proceed
        let res_confirm = SafetyGuard::evaluate_format(
            &data_disk,
            &NodeRole::Worker,
            true,
            &mounts,
            &[data_disk.clone()],
        );
        assert_eq!(res_confirm.unwrap(), FormatDecision::ProceedWithFormat);
    }

    #[test]
    fn test_skip_format_if_already_matching_mounted_role() {
        let mut mounts = sample_mount_table();
        mounts.entries.push(MountEntry {
            device: "/dev/nvme1n1".into(),
            mount_point: "/mnt/gfs-storage".into(),
            fs_type: "ext4".into(),
            options: vec![],
        });

        let mut data_disk = make_test_device("nvme1n1", false, None);
        data_disk.fs_type = Some("ext4".into());
        data_disk.mount_points = vec!["/mnt/gfs-storage".into()];

        let res = SafetyGuard::evaluate_format(
            &data_disk,
            &NodeRole::Worker,
            false,
            &mounts,
            &[data_disk.clone()],
        );
        assert_eq!(
            res.unwrap(),
            FormatDecision::SkipFormatAlreadyMatching {
                existing_fs: "ext4".into(),
                target_mount: "/mnt/gfs-storage".into(),
            }
        );
    }
}
