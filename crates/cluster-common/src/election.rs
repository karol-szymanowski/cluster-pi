use crate::error::ElectionError;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Canonical Kubernetes Lease-based leader elector used across cluster-netboot and cluster-operator.
#[derive(Clone)]
pub struct LeaseElector {
    client: Client,
    namespace: String,
    lease_name: String,
    holder_identity: String,
    lease_duration: Duration,
    renew_interval: Duration,
}

impl LeaseElector {
    pub fn new(
        client: Client,
        namespace: impl Into<String>,
        lease_name: impl Into<String>,
        holder_identity: impl Into<String>,
        lease_duration: Duration,
        renew_interval: Duration,
    ) -> Self {
        Self {
            client,
            namespace: namespace.into(),
            lease_name: lease_name.into(),
            holder_identity: holder_identity.into(),
            lease_duration,
            renew_interval,
        }
    }

    /// Runs the continuous leader election loop until `cancel_token` is cancelled.
    pub async fn run<F>(&self, cancel_token: CancellationToken, mut on_leadership_change: F)
    where
        F: FnMut(bool) + Send + 'static,
    {
        let is_leader = Arc::new(AtomicBool::new(false));
        let leases: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);

        tracing::info!(
            lease = %self.lease_name,
            holder = %self.holder_identity,
            namespace = %self.namespace,
            "Starting leader election loop"
        );

        while !cancel_token.is_cancelled() {
            let step_result = self.try_acquire_or_renew(&leases).await;
            let currently_leader = is_leader.load(Ordering::SeqCst);

            match step_result {
                Ok(true) => {
                    if !currently_leader {
                        is_leader.store(true, Ordering::SeqCst);
                        tracing::info!(
                            lease = %self.lease_name,
                            holder = %self.holder_identity,
                            "Leadership acquired"
                        );
                        on_leadership_change(true);
                    }
                }
                Ok(false) => {
                    if currently_leader {
                        is_leader.store(false, Ordering::SeqCst);
                        tracing::warn!(
                            lease = %self.lease_name,
                            holder = %self.holder_identity,
                            "Leadership lost"
                        );
                        on_leadership_change(false);
                    }
                }
                Err(err) => {
                    tracing::error!(
                        lease = %self.lease_name,
                        error = %err,
                        "Error during leader election cycle"
                    );
                    if currently_leader {
                        is_leader.store(false, Ordering::SeqCst);
                        on_leadership_change(false);
                    }
                }
            }

            tokio::select! {
                _ = cancel_token.cancelled() => {
                    tracing::info!(lease = %self.lease_name, "Leader elector cancellation requested");
                    break;
                }
                _ = tokio::time::sleep(self.renew_interval) => {}
            }
        }

        // On shutdown, if leader, attempt graceful release
        if is_leader.load(Ordering::SeqCst) {
            let _ = self.release_lease(&leases).await;
            on_leadership_change(false);
        }
    }

    async fn try_acquire_or_renew(&self, leases: &Api<Lease>) -> Result<bool, ElectionError> {
        let now = chrono::Utc::now();
        let now_micro = MicroTime(now);

        let existing = match leases.get_opt(&self.lease_name).await? {
            Some(l) => l,
            None => {
                // Try creating lease
                let new_lease = Lease {
                    metadata: kube::core::ObjectMeta {
                        name: Some(self.lease_name.clone()),
                        namespace: Some(self.namespace.clone()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(self.holder_identity.clone()),
                        lease_duration_seconds: Some(self.lease_duration.as_secs() as i32),
                        acquire_time: Some(now_micro.clone()),
                        renew_time: Some(now_micro),
                        lease_transitions: Some(1),
                    }),
                };

                match leases.create(&PostParams::default(), &new_lease).await {
                    Ok(_) => return Ok(true),
                    Err(kube::Error::Api(ae)) if ae.code == 409 => {
                        // Race on create, fetch existing below
                        leases.get(&self.lease_name).await?
                    }
                    Err(e) => return Err(ElectionError::Kube(e)),
                }
            }
        };

        let spec = existing.spec.clone().unwrap_or_default();
        let holder = spec.holder_identity.as_deref().unwrap_or("");
        let renew_time = spec
            .renew_time
            .map(|t| t.0)
            .unwrap_or(chrono::DateTime::UNIX_EPOCH);
        let duration_secs = spec.lease_duration_seconds.unwrap_or(15) as i64;
        let is_expired = (now - renew_time).num_seconds() > duration_secs;

        if holder == self.holder_identity {
            // We hold it, renew
            let patch = serde_json::json!({
                "spec": {
                    "renewTime": MicroTime(now),
                    "leaseDurationSeconds": self.lease_duration.as_secs() as i32
                }
            });
            leases
                .patch(
                    &self.lease_name,
                    &PatchParams::default(),
                    &Patch::Merge(&patch),
                )
                .await?;
            Ok(true)
        } else if is_expired || holder.is_empty() {
            // Lease expired, acquire
            let transitions = spec.lease_transitions.unwrap_or(0) + 1;
            let patch = serde_json::json!({
                "spec": {
                    "holderIdentity": self.holder_identity,
                    "leaseDurationSeconds": self.lease_duration.as_secs() as i32,
                    "acquireTime": MicroTime(now),
                    "renewTime": MicroTime(now),
                    "leaseTransitions": transitions
                }
            });
            leases
                .patch(
                    &self.lease_name,
                    &PatchParams::default(),
                    &Patch::Merge(&patch),
                )
                .await?;
            Ok(true)
        } else {
            // Held by someone else and still valid
            Ok(false)
        }
    }

    async fn release_lease(&self, leases: &Api<Lease>) -> Result<(), ElectionError> {
        let patch = serde_json::json!({
            "spec": {
                "holderIdentity": null
            }
        });
        let _ = leases
            .patch(
                &self.lease_name,
                &PatchParams::default(),
                &Patch::Merge(&patch),
            )
            .await;
        Ok(())
    }
}
