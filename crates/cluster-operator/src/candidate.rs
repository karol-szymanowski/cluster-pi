use cluster_common::crd::{DiskState, NodePhase, NodeRole, PiNode};

pub struct CandidateSelector;

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub node: PiNode,
    pub score: i64,
}

impl CandidateSelector {
    /// Selects the best Worker PiNode candidate for promotion to Master.
    pub fn select_best_candidate(nodes: &[PiNode]) -> Option<PiNode> {
        let mut candidates = Vec::new();

        for node in nodes {
            // Must be a healthy, ready Worker
            if node.spec.desired_role != NodeRole::Worker {
                continue;
            }

            let phase = node
                .status
                .as_ref()
                .map(|s| &s.phase)
                .unwrap_or(&NodePhase::Discovered);
            if *phase != NodePhase::Ready && *phase != NodePhase::Discovered {
                continue;
            }

            let disk_state = node
                .status
                .as_ref()
                .map(|s| &s.disk_state)
                .unwrap_or(&DiskState::Unformatted);
            if matches!(disk_state, DiskState::Error(_)) {
                continue;
            }

            // Score candidate based on readiness and disk attributes
            let mut score: i64 = 100;

            if *phase == NodePhase::Ready {
                score += 50;
            }

            if *disk_state == DiskState::MountedGfs {
                score += 20;
            }

            candidates.push(ScoredCandidate {
                node: node.clone(),
                score,
            });
        }

        candidates.sort_by_key(|b| std::cmp::Reverse(b.score));
        candidates.into_iter().next().map(|sc| sc.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cluster_common::crd::{PiNodeSpec, PiNodeStatus};

    #[test]
    fn test_select_best_candidate() {
        let node1 = PiNode {
            metadata: kube::core::ObjectMeta {
                name: Some("node-1".into()),
                ..Default::default()
            },
            spec: PiNodeSpec {
                hardware_serial: "SERIAL1".into(),
                mac_address: "aa:bb:cc:dd:ee:01".into(),
                desired_role: NodeRole::Worker,
                target_disk_id: None,
                reformat_confirmed: false,
                ip_address: None,
                hostname: None,
            },
            status: Some(PiNodeStatus {
                phase: NodePhase::Ready,
                disk_state: DiskState::MountedGfs,
                last_heartbeat: None,
                promotion: None,
                active_mount: None,
                filesystem_uuid: None,
            }),
        };

        let node2 = PiNode {
            metadata: kube::core::ObjectMeta {
                name: Some("node-2".into()),
                ..Default::default()
            },
            spec: PiNodeSpec {
                hardware_serial: "SERIAL2".into(),
                mac_address: "aa:bb:cc:dd:ee:02".into(),
                desired_role: NodeRole::Master,
                target_disk_id: None,
                reformat_confirmed: false,
                ip_address: None,
                hostname: None,
            },
            status: Some(PiNodeStatus {
                phase: NodePhase::Ready,
                disk_state: DiskState::MountedEtcd,
                last_heartbeat: None,
                promotion: None,
                active_mount: None,
                filesystem_uuid: None,
            }),
        };

        let selected = CandidateSelector::select_best_candidate(&[node1.clone(), node2]);
        assert_eq!(selected.unwrap().spec.hardware_serial, "SERIAL1");
    }
}
