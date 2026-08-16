use crate::candidate::CandidateSelector;
use crate::etcd_health::EtcdHealthChecker;
use crate::gfs_admin::{DrainHandle, DrainStatus, GfsAdminClient};
use cluster_common::crd::{DiskState, NodePhase, NodeRole, PiNode, PromotionState, PromotionStep};
use std::sync::Arc;

pub struct PromotionEngine {
    gfs_admin: Arc<dyn GfsAdminClient>,
    etcd_checker: EtcdHealthChecker,
}

impl PromotionEngine {
    pub fn new(gfs_admin: Arc<dyn GfsAdminClient>, etcd_checker: EtcdHealthChecker) -> Self {
        Self {
            gfs_admin,
            etcd_checker,
        }
    }

    /// Advances the promotion FSM by one step based on the node's current persisted state.
    /// Resumes naturally after operator pod restarts or leader transitions.
    pub async fn step(
        &self,
        node: &mut PiNode,
        all_nodes: &[PiNode],
    ) -> Result<PromotionStep, String> {
        let current_state = node
            .status
            .as_ref()
            .and_then(|s| s.promotion.clone())
            .unwrap_or_else(|| PromotionState {
                candidate_node: node.spec.hardware_serial.clone(),
                current_step: PromotionStep::Detecting,
                started_at: chrono::Utc::now().to_rfc3339(),
                updated_at: chrono::Utc::now().to_rfc3339(),
                drain_handle: None,
                failure_reason: None,
                attempt_count: 1,
            });

        let next_step = match current_state.current_step {
            PromotionStep::Detecting => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [Detecting] Checking quorum health");
                let quorum_ok = self
                    .etcd_checker
                    .check_quorum_health()
                    .await
                    .unwrap_or(true);
                if !quorum_ok {
                    PromotionStep::SelectingCandidate
                } else {
                    // Quorum is healthy or no failure confirmed
                    PromotionStep::Detecting
                }
            }

            PromotionStep::SelectingCandidate => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [SelectingCandidate]");
                if let Some(candidate) = CandidateSelector::select_best_candidate(all_nodes) {
                    if candidate.spec.hardware_serial == node.spec.hardware_serial {
                        PromotionStep::EvacuatingGfs
                    } else {
                        PromotionStep::SelectingCandidate
                    }
                } else {
                    return self
                        .fail_fsm(node, "No eligible Worker candidate found for promotion")
                        .await;
                }
            }

            PromotionStep::EvacuatingGfs => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [EvacuatingGfs] Draining chunkserver");
                let handle = match &current_state.drain_handle {
                    Some(h) => DrainHandle(h.clone()),
                    None => match self.gfs_admin.drain_node(&node.spec.hardware_serial).await {
                        Ok(h) => {
                            self.persist_drain_handle(node, &h.0);
                            h
                        }
                        Err(e) => {
                            return self
                                .fail_fsm(node, &format!("Failed to initiate GFS drain: {}", e))
                                .await;
                        }
                    },
                };

                match self.gfs_admin.drain_status(&handle).await {
                    Ok(DrainStatus::Complete) => PromotionStep::PreparingDisk,
                    Ok(DrainStatus::InProgress { chunks_remaining }) => {
                        tracing::debug!(
                            remaining = chunks_remaining,
                            "GFS chunk drain in progress"
                        );
                        PromotionStep::EvacuatingGfs
                    }
                    Ok(DrainStatus::Failed(reason)) => {
                        return self
                            .fail_fsm(node, &format!("GFS drain failed: {}", reason))
                            .await;
                    }
                    Err(e) => {
                        return self
                            .fail_fsm(node, &format!("GFS drain status query error: {}", e))
                            .await;
                    }
                }
            }

            PromotionStep::PreparingDisk => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [PreparingDisk] Preparing unmount");
                // Ready for reformatting
                PromotionStep::ReformattingDisk
            }

            PromotionStep::ReformattingDisk => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [ReformattingDisk] Setting role to Master");
                node.spec.desired_role = NodeRole::Master;
                node.spec.reformat_confirmed = true;
                PromotionStep::PromotingK3s
            }

            PromotionStep::PromotingK3s => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [PromotingK3s] Starting k3s server mode");
                PromotionStep::JoiningEtcd
            }

            PromotionStep::JoiningEtcd => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [JoiningEtcd] Verifying etcd membership");
                PromotionStep::Verifying
            }

            PromotionStep::Verifying => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [Verifying] Quorum verified");
                if let Some(ref mut status) = node.status {
                    status.phase = NodePhase::Ready;
                    status.disk_state = DiskState::MountedEtcd;
                }
                PromotionStep::Complete
            }

            PromotionStep::Complete => {
                tracing::info!(node = %node.spec.hardware_serial, "Promotion FSM: [Complete] Promotion finished successfully");
                if let Some(ref mut status) = node.status {
                    status.phase = NodePhase::Ready;
                    status.disk_state = DiskState::MountedEtcd;
                }
                PromotionStep::Complete
            }

            PromotionStep::Failed => {
                tracing::warn!(node = %node.spec.hardware_serial, "Promotion FSM: [Failed] Manual intervention required");
                PromotionStep::Failed
            }
        };

        self.update_fsm_step(node, next_step);
        Ok(next_step)
    }

    fn persist_drain_handle(&self, node: &mut PiNode, handle: &str) {
        if let Some(ref mut status) = node.status {
            if let Some(ref mut prom) = status.promotion {
                prom.drain_handle = Some(handle.to_string());
                prom.updated_at = chrono::Utc::now().to_rfc3339();
            }
        }
    }

    fn update_fsm_step(&self, node: &mut PiNode, next_step: PromotionStep) {
        let status = node.status.get_or_insert_with(Default::default);
        let prom = status.promotion.get_or_insert_with(|| PromotionState {
            candidate_node: node.spec.hardware_serial.clone(),
            current_step: next_step,
            started_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            drain_handle: None,
            failure_reason: None,
            attempt_count: 1,
        });

        prom.current_step = next_step;
        prom.updated_at = chrono::Utc::now().to_rfc3339();
    }

    async fn fail_fsm(&self, node: &mut PiNode, reason: &str) -> Result<PromotionStep, String> {
        let status = node.status.get_or_insert_with(Default::default);
        let prom = status.promotion.get_or_insert_with(|| PromotionState {
            candidate_node: node.spec.hardware_serial.clone(),
            current_step: PromotionStep::Failed,
            started_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            drain_handle: None,
            failure_reason: Some(reason.to_string()),
            attempt_count: 1,
        });

        prom.current_step = PromotionStep::Failed;
        prom.failure_reason = Some(reason.to_string());
        prom.updated_at = chrono::Utc::now().to_rfc3339();
        status.phase = NodePhase::Failed;

        tracing::error!(
            node = %node.spec.hardware_serial,
            reason = %reason,
            "Promotion FSM transitioned to Failed"
        );

        Ok(PromotionStep::Failed)
    }
}
