use clap::Parser;
use cluster_common::election::LeaseElector;
use kube::Client;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use cluster_operator::etcd_health::EtcdHealthChecker;
use cluster_operator::gfs_admin::{GfsAdminClient, GrpcGfsAdminClient, MockGfsAdminClient};
use cluster_operator::reconcile::{ControllerContext, TopologyReconciler};

#[derive(Parser, Debug)]
#[command(
    name = "cluster-operator",
    about = "Autonomous Pi Cluster Promotion and Topology Controller"
)]
struct Cli {
    /// Kubernetes namespace to watch
    #[arg(long, env = "POD_NAMESPACE", default_value = "kube-system")]
    namespace: String,

    /// Leader election lease name
    #[arg(long, default_value = "cluster-operator-leader")]
    lease_name: String,

    /// Pod or hostname identity
    #[arg(long, env = "POD_NAME")]
    pod_name: Option<String>,

    /// GFS Master gRPC endpoint
    #[arg(
        long,
        env = "GFS_MASTER_ENDPOINT",
        default_value = "http://gfs-master:9000"
    )]
    gfs_master_endpoint: String,

    /// Use in-memory Mock GFS client instead of real gRPC (for testing)
    #[arg(long)]
    use_mock_gfs: bool,

    /// Embedded etcd health endpoint
    #[arg(long, env = "ETCD_ENDPOINT", default_value = "https://127.0.0.1:2379")]
    etcd_endpoint: String,

    /// Path to etcd client certificate
    #[arg(
        long,
        default_value = "/var/lib/rancher/k3s/server/tls/etcd/client.crt"
    )]
    etcd_cert: PathBuf,

    /// Path to etcd client private key
    #[arg(
        long,
        default_value = "/var/lib/rancher/k3s/server/tls/etcd/client.key"
    )]
    etcd_key: PathBuf,

    /// Path to etcd server CA
    #[arg(
        long,
        default_value = "/var/lib/rancher/k3s/server/tls/etcd/server-ca.crt"
    )]
    etcd_ca: PathBuf,

    /// Reconcile interval in seconds
    #[arg(long, default_value = "5")]
    reconcile_interval_secs: u64,

    /// Dry run mode
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,cluster_operator=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();
    let pod_id = cli.pod_name.unwrap_or_else(|| {
        std::env::var("HOSTNAME").unwrap_or_else(|_| format!("operator-{}", uuid::Uuid::new_v4()))
    });

    let cancel_token = CancellationToken::new();

    let kube_client = match Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to initialize Kubernetes client: {}", e);
            return Err(anyhow::anyhow!(
                "Kubernetes client initialization required for cluster-operator"
            ));
        }
    };

    let gfs_admin: Arc<dyn GfsAdminClient> = if cli.use_mock_gfs {
        tracing::info!("Initializing MockGfsAdminClient");
        Arc::new(MockGfsAdminClient::new())
    } else {
        tracing::info!(endpoint = %cli.gfs_master_endpoint, "Initializing GrpcGfsAdminClient");
        Arc::new(GrpcGfsAdminClient::new(cli.gfs_master_endpoint))
    };

    let etcd_checker = EtcdHealthChecker::new(
        cli.etcd_endpoint,
        Some(cli.etcd_cert),
        Some(cli.etcd_key),
        Some(cli.etcd_ca),
    );

    let ctx = Arc::new(ControllerContext {
        client: kube_client.clone(),
        namespace: cli.namespace.clone(),
        gfs_admin,
        etcd_checker,
    });

    let elector = LeaseElector::new(
        kube_client,
        &cli.namespace,
        &cli.lease_name,
        &pod_id,
        Duration::from_secs(15),
        Duration::from_secs(4),
    );

    let reconcile_interval = Duration::from_secs(cli.reconcile_interval_secs);
    let elector_cancel = cancel_token.clone();

    tokio::spawn(async move {
        let active_reconcile_token: Arc<parking_lot::Mutex<Option<CancellationToken>>> =
            Arc::new(parking_lot::Mutex::new(None));

        let token_holder = active_reconcile_token.clone();
        let ctx_clone = ctx.clone();

        elector
            .run(elector_cancel, move |is_leader| {
                let mut lock = token_holder.lock();
                if is_leader {
                    tracing::info!("Operator acquired leadership; starting reconcile loop");
                    let sub_token = CancellationToken::new();
                    *lock = Some(sub_token.clone());

                    let loop_ctx = ctx_clone.clone();
                    tokio::spawn(async move {
                        while !sub_token.is_cancelled() {
                            if let Err(e) =
                                TopologyReconciler::reconcile_topology(loop_ctx.clone()).await
                            {
                                tracing::error!("Topology reconcile pass error: {}", e);
                            }
                            if let Err(e) =
                                TopologyReconciler::reconcile_nodes(loop_ctx.clone()).await
                            {
                                tracing::error!("Node reconcile pass error: {}", e);
                            }

                            tokio::select! {
                                _ = sub_token.cancelled() => break,
                                _ = tokio::time::sleep(reconcile_interval) => {}
                            }
                        }
                    });
                } else {
                    tracing::warn!("Operator lost leadership; pausing reconcile loop");
                    if let Some(token) = lock.take() {
                        token.cancel();
                    }
                }
            })
            .await;
    });

    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutdown signal received; stopping cluster-operator");
    cancel_token.cancel();

    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}
