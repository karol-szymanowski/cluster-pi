use crate::error::DeviceError;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Information about a single filesystem mount entry parsed from /proc/mounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// Device or pseudo-filesystem specification (e.g. "/dev/mmcblk0p2", "UUID=...", "overlay").
    pub device: String,
    /// Absolute mount point directory (e.g. "/", "/boot", "/mnt/gfs-storage").
    pub mount_point: String,
    /// Filesystem type (e.g. "ext4", "vfat", "tmpfs").
    pub fs_type: String,
    /// Mount options (e.g. "rw", "noatime", "data=ordered").
    pub options: Vec<String>,
}

/// Parsed mount table with query utilities.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MountTable {
    pub entries: Vec<MountEntry>,
}

impl MountTable {
    /// Parses a `/proc/mounts` or `/etc/fstab` formatted string.
    pub fn parse(content: &str) -> Result<Self, DeviceError> {
        let mut entries = Vec::new();
        for (line_idx, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                return Err(DeviceError::InvalidMountTable(format!(
                    "Line {} has insufficient columns: '{}'",
                    line_idx + 1,
                    line
                )));
            }

            let options = if parts.len() >= 4 {
                parts[3].split(',').map(|s| s.to_string()).collect()
            } else {
                Vec::new()
            };

            entries.push(MountEntry {
                device: parts[0].to_string(),
                mount_point: parts[1].to_string(),
                fs_type: parts[2].to_string(),
                options,
            });
        }
        Ok(Self { entries })
    }

    /// Reads live `/proc/mounts`.
    pub fn read_live() -> Result<Self, DeviceError> {
        Self::from_file("/proc/mounts")
    }

    /// Reads and parses a specific mount file (e.g. for testing or non-standard paths).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, DeviceError> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).map_err(|e| DeviceError::SysfsRead {
            path: path.display().to_string(),
            source: e,
        })?;
        Self::parse(&content)
    }

    /// Finds the exact mount entry for a given mount point.
    pub fn find_by_mount_point(&self, target: &str) -> Option<&MountEntry> {
        self.entries.iter().find(|e| e.mount_point == target)
    }

    /// Returns the raw device path backing the root filesystem (`/`), if present.
    pub fn get_root_device(&self) -> Option<&str> {
        self.find_by_mount_point("/").map(|e| e.device.as_str())
    }

    /// Returns the raw device paths backing `/boot` or `/boot/firmware`.
    pub fn get_boot_devices(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| e.mount_point == "/boot" || e.mount_point == "/boot/firmware")
            .map(|e| e.device.as_str())
            .collect()
    }
}

/// Represents a block device discovered from sysfs and udev.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockDevice {
    /// Kernel device name (e.g. "nvme0n1", "sda", "mmcblk0").
    pub name: String,
    /// Absolute node path in /dev (e.g. "/dev/nvme0n1").
    pub dev_path: PathBuf,
    /// Path to the sysfs block directory (e.g. "/sys/class/block/nvme0n1").
    pub sysfs_path: PathBuf,
    /// Capacity in bytes.
    pub size_bytes: u64,
    /// Stable disk identifiers (e.g. ["/dev/disk/by-id/nvme-Samsung...", "/dev/disk/by-id/wwn-..."]).
    pub by_id_paths: Vec<PathBuf>,
    /// Filesystem UUID if probed.
    pub uuid: Option<String>,
    /// Filesystem Label if probed.
    pub label: Option<String>,
    /// Detected filesystem type (e.g. "ext4", "vfat").
    pub fs_type: Option<String>,
    /// Model string from sysfs/uevent if present.
    pub model: Option<String>,
    /// Serial number string from sysfs/uevent if present.
    pub serial: Option<String>,
    /// True if the device is a partition (e.g. "sda1", "mmcblk0p1").
    pub is_partition: bool,
    /// Parent disk name if this is a partition (e.g. "sda" for "sda1").
    pub parent_disk: Option<String>,
    /// Active mount points attached to this device.
    pub mount_points: Vec<String>,
}

impl BlockDevice {
    /// Resolves by-id symlinks to find which by-id path points to this device.
    pub fn discover_by_id_paths(dev_name: &str, by_id_dir: impl AsRef<Path>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let by_id = by_id_dir.as_ref();
        if !by_id.is_dir() {
            return paths;
        }

        if let Ok(entries) = fs::read_dir(by_id) {
            for entry in entries.flatten() {
                let link_path = entry.path();
                if let Ok(target) = fs::canonicalize(&link_path) {
                    if let Some(target_name) = target.file_name().and_then(|n| n.to_str()) {
                        if target_name == dev_name {
                            paths.push(link_path);
                        }
                    }
                }
            }
        }
        paths.sort();
        paths
    }

    /// Reads a single block device from its sysfs directory.
    pub fn from_sysfs(
        name: &str,
        sysfs_block_dir: impl AsRef<Path>,
        by_id_dir: impl AsRef<Path>,
        mount_table: &MountTable,
    ) -> Result<Self, DeviceError> {
        let sysfs_path = sysfs_block_dir.as_ref().join(name);
        let dev_path = PathBuf::from(format!("/dev/{}", name));

        // Read size (sysfs reports 512-byte sectors)
        let size_sectors = read_sysfs_u64(&sysfs_path.join("size")).unwrap_or(0);
        let size_bytes = size_sectors * 512;

        let is_partition = sysfs_path.join("partition").exists();
        let parent_disk = if is_partition {
            // Find parent device by parsing sysfs parent directory or naming convention
            determine_parent_disk(name)
        } else {
            None
        };

        // Parse uevent for model/serial/fs details
        let uevent_map = parse_uevent(&sysfs_path.join("uevent")).unwrap_or_default();
        let model = uevent_map
            .get("ID_MODEL")
            .cloned()
            .or_else(|| read_sysfs_string(&sysfs_path.join("device/model")).ok());
        let serial = uevent_map
            .get("ID_SERIAL_SHORT")
            .cloned()
            .or_else(|| uevent_map.get("ID_SERIAL").cloned())
            .or_else(|| read_sysfs_string(&sysfs_path.join("device/serial")).ok());

        let fs_type = uevent_map.get("ID_FS_TYPE").cloned();
        let uuid = uevent_map.get("ID_FS_UUID").cloned();
        let label = uevent_map.get("ID_FS_LABEL").cloned();

        let by_id_paths = Self::discover_by_id_paths(name, by_id_dir);

        // Find active mount points
        let dev_path_str = dev_path.display().to_string();
        let mut mount_points = Vec::new();
        for entry in &mount_table.entries {
            if entry.device == dev_path_str
                || entry.device.ends_with(&format!("/{}", name))
                || by_id_paths
                    .iter()
                    .any(|p| p.display().to_string() == entry.device)
            {
                mount_points.push(entry.mount_point.clone());
            }
        }

        Ok(Self {
            name: name.to_string(),
            dev_path,
            sysfs_path,
            size_bytes,
            by_id_paths,
            uuid,
            label,
            fs_type,
            model,
            serial,
            is_partition,
            parent_disk,
            mount_points,
        })
    }

    /// Scans all block devices in `/sys/class/block`.
    pub fn scan_all(
        sysfs_root: impl AsRef<Path>,
        by_id_dir: impl AsRef<Path>,
        mount_table: &MountTable,
    ) -> Result<Vec<Self>, DeviceError> {
        let block_dir = sysfs_root.as_ref();
        if !block_dir.exists() {
            return Ok(Vec::new());
        }

        let mut devices = Vec::new();
        let entries = fs::read_dir(block_dir).map_err(|e| DeviceError::SysfsRead {
            path: block_dir.display().to_string(),
            source: e,
        })?;

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            // Skip virtual/pseudo devices
            if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
                continue;
            }

            if let Ok(device) =
                Self::from_sysfs(&name, sysfs_root.as_ref(), by_id_dir.as_ref(), mount_table)
            {
                devices.push(device);
            }
        }

        devices.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(devices)
    }

    /// Checks if this device matches a given stable identifier (by-id path, uuid, or exact dev_path).
    pub fn matches_identifier(&self, id: &str) -> bool {
        let target_path = Path::new(id);

        if self.dev_path == target_path {
            return true;
        }

        if let Some(ref uuid) = self.uuid {
            if uuid == id || id == format!("UUID={}", uuid) {
                return true;
            }
        }

        for by_id in &self.by_id_paths {
            if by_id == target_path || by_id.ends_with(id) {
                return true;
            }
        }

        false
    }
}

fn read_sysfs_u64(path: &Path) -> Result<u64, DeviceError> {
    let content = fs::read_to_string(path).map_err(|e| DeviceError::SysfsRead {
        path: path.display().to_string(),
        source: e,
    })?;
    content
        .trim()
        .parse::<u64>()
        .map_err(|_| DeviceError::Resolution(format!("Failed to parse integer from {:?}", path)))
}

fn read_sysfs_string(path: &Path) -> Result<String, DeviceError> {
    let content = fs::read_to_string(path).map_err(|e| DeviceError::SysfsRead {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(content.trim().to_string())
}

fn parse_uevent(path: &Path) -> Result<HashMap<String, String>, DeviceError> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = fs::read_to_string(path).map_err(|e| DeviceError::SysfsRead {
        path: path.display().to_string(),
        source: e,
    })?;
    let mut map = HashMap::new();
    for line in content.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    Ok(map)
}

fn determine_parent_disk(partition_name: &str) -> Option<String> {
    // For mmcblk0p1 -> mmcblk0, nvme0n1p1 -> nvme0n1
    if partition_name.contains('p')
        && (partition_name.starts_with("mmcblk") || partition_name.starts_with("nvme"))
    {
        if let Some(idx) = partition_name.rfind('p') {
            return Some(partition_name[..idx].to_string());
        }
    }

    // For sda1 -> sda, vda2 -> vda
    let stripped = partition_name.trim_end_matches(char::is_numeric);
    if stripped != partition_name && !stripped.is_empty() {
        return Some(stripped.to_string());
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mount_table_parsing() {
        let sample_mounts = r#"
/dev/mmcblk0p2 / ext4 rw,noatime,data=ordered 0 0
/dev/mmcblk0p1 /boot/firmware vfat rw,relatime 0 0
/dev/nvme0n1 /mnt/gfs-storage ext4 rw,noatime,commit=60 0 0
tmpfs /run tmpfs rw,nosuid,nodev 0 0
"#;
        let table = MountTable::parse(sample_mounts).unwrap();
        assert_eq!(table.get_root_device(), Some("/dev/mmcblk0p2"));
        assert_eq!(table.get_boot_devices(), vec!["/dev/mmcblk0p1"]);
        assert!(table.find_by_mount_point("/mnt/gfs-storage").is_some());
    }

    #[test]
    fn test_parent_disk_determination() {
        assert_eq!(
            determine_parent_disk("mmcblk0p1"),
            Some("mmcblk0".to_string())
        );
        assert_eq!(
            determine_parent_disk("nvme0n1p2"),
            Some("nvme0n1".to_string())
        );
        assert_eq!(determine_parent_disk("sda1"), Some("sda".to_string()));
        assert_eq!(determine_parent_disk("vda3"), Some("vda".to_string()));
        assert_eq!(determine_parent_disk("sda"), None);
    }
}
