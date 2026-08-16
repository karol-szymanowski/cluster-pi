use cluster_common::error::ClusterError;
use std::fs;
use std::path::{Path, PathBuf};

pub struct FstabManager {
    fstab_path: PathBuf,
}

impl FstabManager {
    pub fn new(fstab_path: impl AsRef<Path>) -> Self {
        Self {
            fstab_path: fstab_path.as_ref().to_path_buf(),
        }
    }

    #[allow(dead_code)]
    pub fn default_system() -> Self {
        Self::new("/etc/fstab")
    }

    /// Idempotently updates or appends a mount entry by filesystem UUID.
    /// If an entry for this mount point or UUID already exists, it is replaced cleanly.
    pub fn upsert_entry(
        &self,
        uuid: &str,
        mount_point: &str,
        fs_type: &str,
        options: &str,
    ) -> Result<(), ClusterError> {
        let new_line = format!(
            "UUID={}\t{}\t{}\t{}\t0\t2",
            uuid, mount_point, fs_type, options
        );

        if !self.fstab_path.exists() {
            fs::write(&self.fstab_path, format!("{}\n", new_line)).map_err(|e| {
                ClusterError::Internal(format!("Failed to write initial fstab: {}", e))
            })?;
            return Ok(());
        }

        let content = fs::read_to_string(&self.fstab_path).map_err(|e| {
            ClusterError::Internal(format!("Failed to read {:?}: {}", self.fstab_path, e))
        })?;

        let mut lines: Vec<String> = Vec::new();
        let mut replaced = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                lines.push(line.to_string());
                continue;
            }

            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                let spec = parts[0];
                let mp = parts[1];

                // Match either the same UUID or the same target mount point
                if spec == format!("UUID={}", uuid) || mp == mount_point {
                    if !replaced {
                        lines.push(new_line.clone());
                        replaced = true;
                    }
                    continue;
                }
            }

            lines.push(line.to_string());
        }

        if !replaced {
            lines.push(new_line);
        }

        let updated_content = format!("{}\n", lines.join("\n"));
        fs::write(&self.fstab_path, updated_content)
            .map_err(|e| ClusterError::Internal(format!("Failed to write updated fstab: {}", e)))?;

        Ok(())
    }

    /// Removes an entry for a given mount point or UUID.
    pub fn remove_entry(&self, mount_point_or_uuid: &str) -> Result<(), ClusterError> {
        if !self.fstab_path.exists() {
            return Ok(());
        }

        let content = fs::read_to_string(&self.fstab_path).map_err(|e| {
            ClusterError::Internal(format!("Failed to read {:?}: {}", self.fstab_path, e))
        })?;

        let mut lines = Vec::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                lines.push(line.to_string());
                continue;
            }

            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                let spec = parts[0];
                let mp = parts[1];
                if spec == format!("UUID={}", mount_point_or_uuid)
                    || spec == mount_point_or_uuid
                    || mp == mount_point_or_uuid
                {
                    continue;
                }
            }
            lines.push(line.to_string());
        }

        let updated_content = format!("{}\n", lines.join("\n"));
        fs::write(&self.fstab_path, updated_content)
            .map_err(|e| ClusterError::Internal(format!("Failed to write updated fstab: {}", e)))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_fstab_upsert_and_replace() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path().to_path_buf();
        fs::write(
            &path,
            "# System fstab\nUUID=root-1234 / ext4 defaults 0 1\nUUID=old-gfs /mnt/gfs-storage ext4 defaults 0 2\n",
        )
        .unwrap();

        let manager = FstabManager::new(&path);
        manager
            .upsert_entry(
                "new-uuid-5678",
                "/mnt/gfs-storage",
                "ext4",
                "commit=60,noatime",
            )
            .unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old-gfs"));
        assert!(
            content.contains("UUID=new-uuid-5678\t/mnt/gfs-storage\text4\tcommit=60,noatime\t0\t2")
        );
        assert!(content.contains("UUID=root-1234"));
    }
}
