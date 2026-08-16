use crate::discover::DeviceDiscovery;
use crate::format::Formatter;
use crate::mount::Mounter;
use cluster_common::crd::NodeRole;
use cluster_common::device::MountTable;
use cluster_common::safety::{FormatDecision, SafetyGuard};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

#[allow(clippy::enum_variant_names)]
pub mod proto {
    tonic::include_proto!("cluster.ldm.v1");
}

use proto::local_disk_manager_server::{LocalDiskManager, LocalDiskManagerServer};
use proto::*;

pub struct LdmIpcService {
    audit_log_path: PathBuf,
    fstab_path: PathBuf,
    sysfs_root: PathBuf,
    by_id_dir: PathBuf,
    dry_run: bool,
}

impl LdmIpcService {
    pub fn new(
        audit_log_path: impl AsRef<Path>,
        fstab_path: impl AsRef<Path>,
        sysfs_root: impl AsRef<Path>,
        by_id_dir: impl AsRef<Path>,
        dry_run: bool,
    ) -> Self {
        Self {
            audit_log_path: audit_log_path.as_ref().to_path_buf(),
            fstab_path: fstab_path.as_ref().to_path_buf(),
            sysfs_root: sysfs_root.as_ref().to_path_buf(),
            by_id_dir: by_id_dir.as_ref().to_path_buf(),
            dry_run,
        }
    }
}

#[tonic::async_trait]
impl LocalDiskManager for LdmIpcService {
    async fn assign_role(
        &self,
        request: Request<AssignRoleRequest>,
    ) -> Result<Response<ProvisionResult>, Status> {
        let req = request.into_inner();
        let desired_role = match req.desired_role() {
            NodeRoleProto::RoleMaster => NodeRole::Master,
            NodeRoleProto::RoleSeed => NodeRole::Seed,
            NodeRoleProto::RoleWorker => NodeRole::Worker,
            NodeRoleProto::RoleDecommissioned => NodeRole::Decommissioned,
            _ => {
                return Err(Status::invalid_argument(
                    "Invalid or unspecified desired role",
                ));
            }
        };

        let target_id = if req.target_disk_id.is_empty() {
            None
        } else {
            Some(req.target_disk_id.as_str())
        };

        let is_dry_run = req.dry_run || self.dry_run;
        let mount_table = MountTable::read_live().unwrap_or_else(|_| MountTable::default());

        // 1. Discover target device
        let target_device = DeviceDiscovery::resolve_target_disk(
            target_id,
            &self.sysfs_root,
            &self.by_id_dir,
            &mount_table,
            &desired_role,
        )
        .map_err(|e| Status::not_found(format!("Target device resolution failed: {}", e)))?;

        // 2. Apply safety check
        let all_devices = cluster_common::device::BlockDevice::scan_all(
            &self.sysfs_root,
            &self.by_id_dir,
            &mount_table,
        )
        .map_err(|e| Status::internal(format!("Failed to scan devices: {}", e)))?;

        let decision = SafetyGuard::evaluate_format(
            &target_device,
            &desired_role,
            req.reformat_confirmed,
            &mount_table,
            &all_devices,
        )
        .map_err(|e| Status::failed_precondition(format!("Safety guard check failed: {}", e)))?;

        let formatter = Formatter::new(&self.audit_log_path, is_dry_run)
            .map_err(|e| Status::internal(e.to_string()))?;
        let mounter = Mounter::new(&self.audit_log_path, &self.fstab_path, is_dry_run)
            .map_err(|e| Status::internal(e.to_string()))?;

        match decision {
            FormatDecision::ProceedWithFormat => {
                let uuid = formatter
                    .format_device(&target_device, &desired_role, "cluster-ldm-ipc")
                    .await
                    .map_err(|e| Status::internal(format!("Formatting failed: {}", e)))?;

                let mount_point = mounter
                    .mount_role_storage(&target_device, &uuid, &desired_role, "cluster-ldm-ipc")
                    .await
                    .map_err(|e| Status::internal(format!("Mount failed: {}", e)))?;

                Ok(Response::new(ProvisionResult {
                    success: true,
                    message: "Disk formatted and mounted successfully".into(),
                    formatted_device: target_device.dev_path.display().to_string(),
                    filesystem_uuid: uuid,
                    mount_point,
                }))
            }
            FormatDecision::SkipFormatAlreadyMatching {
                target_mount,
                existing_fs: _,
            } => Ok(Response::new(ProvisionResult {
                success: true,
                message: "Disk already matches desired role layout, formatting skipped".into(),
                formatted_device: target_device.dev_path.display().to_string(),
                filesystem_uuid: target_device.uuid.unwrap_or_default(),
                mount_point: target_mount,
            })),
        }
    }

    async fn get_disk_state(
        &self,
        _request: Request<GetDiskStateRequest>,
    ) -> Result<Response<DiskStateReport>, Status> {
        let mount_table = MountTable::read_live().unwrap_or_default();
        let devices = cluster_common::device::BlockDevice::scan_all(
            &self.sysfs_root,
            &self.by_id_dir,
            &mount_table,
        )
        .unwrap_or_default();

        for dev in devices {
            for mp in &dev.mount_points {
                if mp == "/var/lib/rancher/k3s/server/db/etcd" {
                    return Ok(Response::new(DiskStateReport {
                        state: "MountedEtcd".into(),
                        device_path: dev.dev_path.display().to_string(),
                        filesystem_uuid: dev.uuid.unwrap_or_default(),
                        mount_point: mp.clone(),
                        error_message: String::new(),
                    }));
                }
                if mp == "/mnt/gfs-storage" {
                    return Ok(Response::new(DiskStateReport {
                        state: "MountedGfs".into(),
                        device_path: dev.dev_path.display().to_string(),
                        filesystem_uuid: dev.uuid.unwrap_or_default(),
                        mount_point: mp.clone(),
                        error_message: String::new(),
                    }));
                }
            }
        }

        Ok(Response::new(DiskStateReport {
            state: "Unformatted".into(),
            device_path: String::new(),
            filesystem_uuid: String::new(),
            mount_point: String::new(),
            error_message: String::new(),
        }))
    }

    async fn prepare_evacuation(
        &self,
        request: Request<PrepareEvacuationRequest>,
    ) -> Result<Response<PrepareEvacuationResponse>, Status> {
        let req = request.into_inner();
        let mount_point = if req.mount_point.is_empty() {
            "/mnt/gfs-storage"
        } else {
            &req.mount_point
        };

        let mounter = Mounter::new(&self.audit_log_path, &self.fstab_path, self.dry_run)
            .map_err(|e| Status::internal(e.to_string()))?;

        mounter
            .unmount_path(mount_point, "cluster-ldm-ipc-evacuate")
            .await
            .map_err(|e| Status::internal(format!("Failed to unmount {}: {}", mount_point, e)))?;

        Ok(Response::new(PrepareEvacuationResponse {
            ready: true,
            message: format!("Mount {} unmounted successfully", mount_point),
        }))
    }

    async fn confirm_unmounted(
        &self,
        request: Request<ConfirmUnmountedRequest>,
    ) -> Result<Response<ConfirmUnmountedResponse>, Status> {
        let req = request.into_inner();
        let mount_point = if req.mount_point.is_empty() {
            "/mnt/gfs-storage"
        } else {
            &req.mount_point
        };

        let mount_table = MountTable::read_live().unwrap_or_default();
        let is_unmounted = mount_table.find_by_mount_point(mount_point).is_none();

        Ok(Response::new(ConfirmUnmountedResponse {
            is_unmounted,
            message: if is_unmounted {
                format!("{} is confirmed unmounted", mount_point)
            } else {
                format!("{} is still mounted!", mount_point)
            },
        }))
    }
}

/// Starts the Unix Domain Socket IPC server on the host.
pub async fn start_uds_server(
    service: Arc<LdmIpcService>,
    socket_path: impl AsRef<Path>,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = socket_path.as_ref();
    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let uds = UnixListener::bind(socket_path)?;
    let uds_stream = UnixListenerStream::new(uds);

    tracing::info!(
        socket = %socket_path.display(),
        "Starting cluster-ldm UDS IPC server"
    );

    tonic::transport::Server::builder()
        .add_service(LocalDiskManagerServer::from_arc(service))
        .serve_with_incoming_shutdown(uds_stream, cancel_token.cancelled())
        .await?;

    if socket_path.exists() {
        let _ = std::fs::remove_file(socket_path);
    }

    Ok(())
}
