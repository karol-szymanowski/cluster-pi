use clap::{Parser, Subcommand};
use cluster_common::crd::NodeRole;
use cluster_common::device::MountTable;
use cluster_common::safety::{FormatDecision, SafetyGuard};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod discover;
mod format;
mod fstab;
mod ipc;
mod mount;

use discover::DeviceDiscovery;
use format::Formatter;
use ipc::{start_uds_server, LdmIpcService};
use mount::Mounter;

#[derive(Parser, Debug)]
#[command(name = "cluster-ldm", about = "Raspberry Pi Local Disk Manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable dry run mode (perform safety evaluation, zero mutating syscalls)
    #[arg(long, global = true)]
    dry_run: bool,

    /// Audit log file location
    #[arg(
        long,
        global = true,
        default_value = "/var/log/cluster-ldm-audit.jsonl"
    )]
    audit_log: PathBuf,

    /// Path to system fstab
    #[arg(long, global = true, default_value = "/etc/fstab")]
    fstab: PathBuf,

    /// Path to sysfs root
    #[arg(long, global = true, default_value = "/sys/class/block")]
    sysfs_root: PathBuf,

    /// Path to /dev/disk/by-id directory
    #[arg(long, global = true, default_value = "/dev/disk/by-id")]
    by_id_dir: PathBuf,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute pre-boot disk provisioning according to local role configuration
    Provision {
        /// Path to local role configuration JSON
        #[arg(long, default_value = "/etc/cluster-ldm/role.json")]
        role_source: PathBuf,

        /// Override target disk identifier
        #[arg(long)]
        target_disk: Option<String>,

        /// Explicit reformat confirmation
        #[arg(long)]
        reformat_confirmed: bool,
    },

    /// Run the host-local Unix Domain Socket IPC server
    IpcServe {
        /// Path to Unix Domain Socket
        #[arg(long, default_value = "/run/cluster-ldm.sock")]
        socket: PathBuf,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RoleConfigFile {
    role: NodeRole,
    target_disk_id: Option<String>,
    reformat_confirmed: Option<bool>,
}

struct ProvisionParams<'a> {
    role_source: &'a Path,
    target_disk_override: Option<&'a str>,
    cli_reformat_confirmed: bool,
    audit_log: &'a Path,
    fstab: &'a Path,
    sysfs_root: &'a Path,
    by_id_dir: &'a Path,
    dry_run: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,cluster_ldm=debug".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Provision {
            role_source,
            target_disk,
            reformat_confirmed,
        } => {
            let params = ProvisionParams {
                role_source: &role_source,
                target_disk_override: target_disk.as_deref(),
                cli_reformat_confirmed: reformat_confirmed,
                audit_log: &cli.audit_log,
                fstab: &cli.fstab,
                sysfs_root: &cli.sysfs_root,
                by_id_dir: &cli.by_id_dir,
                dry_run: cli.dry_run,
            };
            run_provision(params).await?;
        }
        Commands::IpcServe { socket } => {
            let cancel_token = CancellationToken::new();
            let service = Arc::new(LdmIpcService::new(
                &cli.audit_log,
                &cli.fstab,
                &cli.sysfs_root,
                &cli.by_id_dir,
                cli.dry_run,
            ));

            let token_clone = cancel_token.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                tracing::info!("Received shutdown signal");
                token_clone.cancel();
            });

            start_uds_server(service, &socket, cancel_token)
                .await
                .map_err(|e| anyhow::anyhow!("{}", e))?;
        }
    }

    Ok(())
}

async fn run_provision(params: ProvisionParams<'_>) -> anyhow::Result<()> {
    tracing::info!(
        role_source = %params.role_source.display(),
        "Executing pre-boot disk provisioning"
    );

    let role_config = if params.role_source.exists() {
        let content = std::fs::read_to_string(params.role_source)?;
        serde_json::from_str::<RoleConfigFile>(&content)?
    } else {
        tracing::warn!(
            "Role config file '{}' not found; defaulting to Pending",
            params.role_source.display()
        );
        RoleConfigFile {
            role: NodeRole::Pending,
            target_disk_id: None,
            reformat_confirmed: None,
        }
    };

    let desired_role = role_config.role;
    if desired_role == NodeRole::Pending || desired_role == NodeRole::Decommissioned {
        tracing::info!(
            role = ?desired_role,
            "Node role is non-operational, skipping provisioning"
        );
        return Ok(());
    }

    let target_id = params
        .target_disk_override
        .or(role_config.target_disk_id.as_deref());
    let reformat_confirmed =
        params.cli_reformat_confirmed || role_config.reformat_confirmed.unwrap_or(false);

    let mount_table = MountTable::read_live().unwrap_or_default();
    let target_device = DeviceDiscovery::resolve_target_disk(
        target_id,
        params.sysfs_root,
        params.by_id_dir,
        &mount_table,
        &desired_role,
    )?;

    let all_devices = cluster_common::device::BlockDevice::scan_all(
        params.sysfs_root,
        params.by_id_dir,
        &mount_table,
    )?;

    let decision = SafetyGuard::evaluate_format(
        &target_device,
        &desired_role,
        reformat_confirmed,
        &mount_table,
        &all_devices,
    )?;

    let formatter = Formatter::new(params.audit_log, params.dry_run)?;
    let mounter = Mounter::new(params.audit_log, params.fstab, params.dry_run)?;

    match decision {
        FormatDecision::ProceedWithFormat => {
            let uuid = formatter
                .format_device(&target_device, &desired_role, "cluster-ldm-provision")
                .await?;
            let mount_point = mounter
                .mount_role_storage(
                    &target_device,
                    &uuid,
                    &desired_role,
                    "cluster-ldm-provision",
                )
                .await?;
            tracing::info!(
                device = %target_device.dev_path.display(),
                mount_point = %mount_point,
                uuid = %uuid,
                "Provisioning completed successfully"
            );
        }
        FormatDecision::SkipFormatAlreadyMatching {
            target_mount,
            existing_fs: _,
        } => {
            tracing::info!(
                device = %target_device.dev_path.display(),
                mount_point = %target_mount,
                "Storage layout already matching desired role; format skipped"
            );
        }
    }

    Ok(())
}
