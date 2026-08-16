use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Background task that continuously synchronizes boot assets from the GFS distributed volume
/// (`/mnt/gfs/netboot-assets`) into the local on-disk TFTP and HTTP cache.
pub struct AssetSyncer {
    gfs_source_dir: PathBuf,
    local_cache_dir: PathBuf,
    poll_interval: Duration,
}

impl AssetSyncer {
    pub fn new(
        gfs_source_dir: impl AsRef<Path>,
        local_cache_dir: impl AsRef<Path>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            gfs_source_dir: gfs_source_dir.as_ref().to_path_buf(),
            local_cache_dir: local_cache_dir.as_ref().to_path_buf(),
            poll_interval,
        }
    }

    pub async fn run(&self, cancel_token: CancellationToken) {
        tracing::info!(
            gfs_source = %self.gfs_source_dir.display(),
            cache_dir = %self.local_cache_dir.display(),
            poll_interval = ?self.poll_interval,
            "Starting GFS netboot asset synchronization background loop"
        );

        while !cancel_token.is_cancelled() {
            if let Err(e) = self.sync_once() {
                tracing::warn!(error = %e, "Asset sync cycle encountered error");
            }

            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!("Asset syncer stopping on cancellation");
                    break;
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
        }
    }

    /// Performs one sync pass from GFS volume to local cache.
    pub fn sync_once(&self) -> Result<(), std::io::Error> {
        if !self.gfs_source_dir.exists() {
            tracing::trace!(
                path = %self.gfs_source_dir.display(),
                "GFS asset mount path not yet available; skipping sync cycle"
            );
            return Ok(());
        }

        fs::create_dir_all(&self.local_cache_dir)?;
        self.copy_recursive(&self.gfs_source_dir, &self.local_cache_dir)?;

        Ok(())
    }

    fn copy_recursive(&self, src: &Path, dst: &Path) -> Result<(), std::io::Error> {
        if !dst.exists() {
            fs::create_dir_all(dst)?;
        }

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if file_type.is_dir() {
                self.copy_recursive(&src_path, &dst_path)?;
            } else if file_type.is_file() {
                let should_copy = if dst_path.exists() {
                    let src_meta = fs::metadata(&src_path)?;
                    let dst_meta = fs::metadata(&dst_path)?;
                    src_meta.len() != dst_meta.len()
                        || src_meta
                            .modified()
                            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                            > dst_meta
                                .modified()
                                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                } else {
                    true
                };

                if should_copy {
                    tracing::info!(
                        from = %src_path.display(),
                        to = %dst_path.display(),
                        "Syncing updated boot asset from GFS"
                    );
                    fs::copy(&src_path, &dst_path)?;
                }
            }
        }

        Ok(())
    }
}
