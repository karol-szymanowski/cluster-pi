use clap::Parser;
use cluster_common::election::LeaseElector;
use kube::Client;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use cluster_netboot::asset_sync::AssetSyncer;
use cluster_netboot::dhcp::ProxyDhcpServer;
use cluster_netboot::http::{create_router, HttpState};
use cluster_netboot::tftp::TftpServer;

#[derive(Parser, Debug)]
#[command(
    name = "cluster-netboot",
    about = "Resilient PXE, ProxyDHCP, TFTP, and HTTP Boot Engine"
)]
struct Cli {
    /// Virtual IP assigned to kube-vip for PXE offers and cluster join endpoint
    #[arg(long, env = "CLUSTER_VIP", default_value = "192.168.1.200")]
    vip: String,

    /// ProxyDHCP listening port
    #[arg(long, default_value = "67")]
    dhcp_port: u16,

    /// TFTP listening port
    #[arg(long, default_value = "69")]
    tftp_port: u16,

    /// HTTP asset and cloud-init server port
    #[arg(long, default_value = "8080")]
    http_port: u16,

    /// Local asset cache directory
    #[arg(long, default_value = "/var/lib/cluster-netboot/assets")]
    assets_dir: PathBuf,

    /// GFS mount path for source netboot assets
    #[arg(long, default_value = "/mnt/gfs/netboot-assets")]
    gfs_source: PathBuf,

    /// K3s join token
    #[arg(long, env = "K3S_TOKEN", default_value = "cluster-join-token-secret")]
    k3s_token: String,

    /// Kubernetes namespace
    #[arg(long, env = "POD_NAMESPACE", default_value = "kube-system")]
    namespace: String,

    /// Name of the leader election lease for DHCP single-active gating
    #[arg(long, default_value = "cluster-netboot-dhcp")]
    lease_name: String,

    /// Pod or hostname identity
    #[arg(long, env = "POD_NAME")]
    pod_name: Option<String>,

    /// Run passive network self-check on startup
    #[arg(long, default_value_t = true)]
    passive_check: bool,

    /// Dry run mode
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cluster_netboot=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let vip_ip = Ipv4Addr::from_str(&cli.vip)?;
    let pod_id = cli.pod_name.unwrap_or_else(|| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| format!("netboot-{}", uuid::Uuid::new_v4()))
    });

    let cancel_token = CancellationToken::new();

    // Ensure assets directory exists
    std::fs::create_dir_all(&cli.assets_dir)?;

    // Passive DHCP check on startup
    if cli.passive_check {
        ProxyDhcpServer::passive_sniff_self_check(Ipv4Addr::UNSPECIFIED, Duration::from_secs(2))
            .await;
    }

    // Try initializing Kubernetes client (optional in standalone testing)
    let kube_client = match Client::try_default().await {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(
                "Kubernetes client unavailable ({}); running in offline/standalone mode",
                e
            );
            None
        }
    };

    // 1. Start Active-Active HTTP Server
    let http_state = Arc::new(HttpState {
        assets_dir: cli.assets_dir.clone(),
        vip: cli.vip.clone(),
        http_port: cli.http_port,
        k3s_token: cli.k3s_token.clone(),
        kube_client: kube_client.clone(),
        namespace: cli.namespace.clone(),
    });

    let router = create_router(http_state);
    let http_addr = SocketAddr::from(([0, 0, 0, 0], cli.http_port));
    let http_listener = tokio::net::TcpListener::bind(http_addr).await?;
    tracing::info!(addr = %http_addr, "HTTP server listening");

    let http_cancel = cancel_token.clone();
    tokio::spawn(async move {
        axum::serve(
            http_listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            http_cancel.cancelled().await;
        })
        .await
        .ok();
    });

    // 2. Start Active-Active TFTP Server
    let tftp_server = Arc::new(TftpServer::new(&cli.assets_dir, cli.tftp_port));
    let tftp_cancel = cancel_token.clone();
    tokio::spawn(async move {
        if let Err(e) = tftp_server.run(tftp_cancel).await {
            tracing::error!("TFTP server exited with error: {}", e);
        }
    });

    // 3. Start GFS Asset Synchronizer background task
    let asset_syncer = Arc::new(AssetSyncer::new(
        &cli.gfs_source,
        &cli.assets_dir,
        Duration::from_secs(30),
    ));
    let syncer_cancel = cancel_token.clone();
    let syncer_task = asset_syncer.clone();
    tokio::spawn(async move {
        syncer_task.run(syncer_cancel).await;
    });

    // 4. Start Single-Active ProxyDHCP Listener gated by Lease Election
    let dhcp_cancel = cancel_token.clone();
    let dhcp_vip = vip_ip;
    let dhcp_port = cli.dhcp_port;
    let dhcp_client = kube_client.clone();
    let dhcp_ns = cli.namespace.clone();

    if let Some(client) = kube_client {
        let elector = LeaseElector::new(
            client,
            &cli.namespace,
            &cli.lease_name,
            &pod_id,
            Duration::from_secs(10),
            Duration::from_secs(3),
        );

        let elector_cancel = cancel_token.clone();
        tokio::spawn(async move {
            let active_dhcp_token: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
                Arc::new(parking_lot::Mutex::new(None));

            let token_holder = active_dhcp_token.clone();
            elector
                .run(elector_cancel, move |is_leader| {
                    let mut lock = token_holder.lock();
                    if is_leader {
                        tracing::info!(
                            "Acquired DHCP lease leadership; starting ProxyDHCP listener"
                        );
                        let sub_token = CancellationToken::new();
                        *lock = Some(sub_token.clone());

                        let server = Arc::new(ProxyDhcpServer::new(
                            dhcp_vip,
                            dhcp_port,
                            dhcp_client.clone(),
                            dhcp_ns.clone(),
                            "bootcode.bin",
                        ));

                        tokio::spawn(async move {
                            if let Err(e) = server.run(sub_token).await {
                                tracing::error!("ProxyDHCP listener error: {}", e);
                            }
                        });
                    } else {
                        tracing::warn!("Lost DHCP lease leadership; stopping ProxyDHCP listener");
                        if let Some(token) = lock.take() {
                            token.cancel();
                        }
                    }
                })
                .await;
        });
    } else {
        // Offline / standalone mode: run DHCP listener directly
        tracing::info!("Running standalone ProxyDHCP listener (no Kubernetes lease gate)");
        let server = Arc::new(ProxyDhcpServer::new(
            dhcp_vip,
            dhcp_port,
            None,
            cli.namespace,
            "bootcode.bin",
        ));
        tokio::spawn(async move {
            if let Err(e) = server.run(dhcp_cancel).await {
                tracing::error!("ProxyDHCP server error: {}", e);
            }
        });
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutdown signal received; terminating cluster-netboot tasks");
    cancel_token.cancel();

    // Allow graceful loop closures
    tokio::time::sleep(Duration::from_millis(500)).await;

    Ok(())
}
