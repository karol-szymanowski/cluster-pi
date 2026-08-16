use crate::candidate::CandidateSelector;
use crate::etcd_health::EtcdHealthChecker;
use crate::gfs_admin::GfsAdminClient;
use crate::promotion_fsm::PromotionEngine;
use cluster_common::crd::{
    ClusterTopology, ClusterTopologyStatus, NodeRole, PiNode, PromotionStep,
};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct ControllerContext {
    pub client: Client,
    pub namespace: String,
    pub gfs_admin: Arc<dyn GfsAdminClient>,
    pub etcd_checker: EtcdHealthChecker,
}

pub struct TopologyReconciler;

impl TopologyReconciler {
    /// Main reconcile pass for ClusterTopology and Quorum Auto-Healing
    pub async fn reconcile_topology(
        ctx: Arc<ControllerContext>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let topo_api: Api<ClusterTopology> = Api::all(ctx.client.clone());
        let topologylist = match topo_api.list(&ListParams::default()).await {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!("ClusterTopology list error: {}", e);
                return Ok(());
            }
        };

        let node_api: Api<PiNode> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
        let nodelist = match node_api.list(&ListParams::default()).await {
            Ok(list) => list,
            Err(e) => {
                tracing::debug!("PiNode list error: {}", e);
                return Ok(());
            }
        };

        for topo in topologylist.items {
            let mut master_count = 0u8;
            let mut worker_count = 0u8;

            for node in &nodelist.items {
                match node.spec.desired_role {
                    NodeRole::Master | NodeRole::Seed => master_count += 1,
                    NodeRole::Worker => worker_count += 1,
                    _ => {}
                }
            }

            let quorum_healthy = ctx.etcd_checker.check_quorum_health().await.unwrap_or(true);
            let target_masters = topo.spec.target_master_count;

            // Check if promotion is needed (masters < target)
            if master_count < target_masters && quorum_healthy {
                tracing::info!(
                    current = master_count,
                    target = target_masters,
                    "Quorum Auto-Healer: Master count below target; selecting worker candidate"
                );

                if let Some(candidate) = CandidateSelector::select_best_candidate(&nodelist.items) {
                    let engine =
                        PromotionEngine::new(ctx.gfs_admin.clone(), ctx.etcd_checker.clone());
                    let mut candidate_clone = candidate.clone();
                    let nodes_snapshot = nodelist.items.clone();

                    tokio::spawn(async move {
                        let mut current_step = PromotionStep::SelectingCandidate;
                        while current_step != PromotionStep::Complete
                            && current_step != PromotionStep::Failed
                        {
                            match engine.step(&mut candidate_clone, &nodes_snapshot).await {
                                Ok(next) => current_step = next,
                                Err(e) => {
                                    tracing::error!("Promotion FSM step failed: {}", e);
                                    break;
                                }
                            }
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    });
                }
            }

            // Check if seed eviction is ready
            let seed_retired = if master_count >= target_masters {
                if let Some(seed_node) = nodelist
                    .items
                    .iter()
                    .find(|n| n.spec.hardware_serial == topo.spec.seed_node)
                {
                    seed_node.spec.desired_role == NodeRole::Decommissioned
                } else {
                    true
                }
            } else {
                false
            };

            // Update status
            let status_patch = serde_json::json!({
                "status": ClusterTopologyStatus {
                    current_master_count: master_count,
                    current_worker_count: worker_count,
                    quorum_healthy,
                    seed_retired,
                    active_mutation: None,
                    conditions: Vec::new(),
                }
            });

            if let Some(name) = topo.metadata.name {
                let _ = topo_api
                    .patch_status(&name, &PatchParams::default(), &Patch::Merge(&status_patch))
                    .await;
            }
        }

        Ok(())
    }

    /// Reconciles individual PiNodes (resuming in-flight promotions)
    pub async fn reconcile_nodes(
        ctx: Arc<ControllerContext>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let node_api: Api<PiNode> = Api::namespaced(ctx.client.clone(), &ctx.namespace);
        let nodelist = node_api.list(&ListParams::default()).await?;

        let engine = PromotionEngine::new(ctx.gfs_admin.clone(), ctx.etcd_checker.clone());

        for node in &nodelist.items {
            let in_flight = node
                .status
                .as_ref()
                .and_then(|s| s.promotion.as_ref())
                .map(|p| {
                    p.current_step != PromotionStep::Complete
                        && p.current_step != PromotionStep::Failed
                })
                .unwrap_or(false);

            if in_flight {
                tracing::info!(
                    node = %node.spec.hardware_serial,
                    "Resuming in-flight promotion FSM from persisted status"
                );
                let mut node_clone = node.clone();
                let _ = engine.step(&mut node_clone, &nodelist.items).await;
            }
        }

        Ok(())
    }
}
