use cluster_common::election::LeaseElector;
use kube::Client;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Lease-based election gate specifically for ProxyDHCP listener exclusivity.
#[allow(dead_code)]
pub struct DhcpElectionGate {
    elector: LeaseElector,
}

#[allow(dead_code)]
impl DhcpElectionGate {
    pub fn new(
        client: Client,
        namespace: impl Into<String>,
        lease_name: impl Into<String>,
        pod_identity: impl Into<String>,
    ) -> Self {
        let elector = LeaseElector::new(
            client,
            namespace,
            lease_name,
            pod_identity,
            Duration::from_secs(10),
            Duration::from_secs(3),
        );
        Self { elector }
    }

    pub async fn run_gate<F>(&self, cancel_token: CancellationToken, on_leadership_changed: F)
    where
        F: FnMut(bool) + Send + 'static,
    {
        self.elector.run(cancel_token, on_leadership_changed).await;
    }
}
