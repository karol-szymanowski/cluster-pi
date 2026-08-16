use cluster_common::election::LeaseElector;
use kube::Client;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
pub struct OperatorLeaderElector {
    elector: LeaseElector,
}

#[allow(dead_code)]
impl OperatorLeaderElector {
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
            Duration::from_secs(15),
            Duration::from_secs(4),
        );
        Self { elector }
    }

    pub async fn run_election<F>(&self, cancel_token: CancellationToken, on_leadership_changed: F)
    where
        F: FnMut(bool) + Send + 'static,
    {
        self.elector.run(cancel_token, on_leadership_changed).await;
    }
}
