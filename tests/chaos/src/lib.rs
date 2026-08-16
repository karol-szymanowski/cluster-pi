#[cfg(test)]
mod tests {
    use cluster_common::crd::{
        DiskState, NodePhase, NodeRole, PiNode, PiNodeSpec, PiNodeStatus, PromotionState,
        PromotionStep,
    };
    use cluster_operator::etcd_health::EtcdHealthChecker;
    use cluster_operator::gfs_admin::MockGfsAdminClient;
    use cluster_operator::promotion_fsm::PromotionEngine;
    use std::sync::Arc;

    fn create_test_node(serial: &str, role: NodeRole, phase: NodePhase) -> PiNode {
        PiNode {
            metadata: kube::core::ObjectMeta {
                name: Some(format!("node-{}", serial)),
                ..Default::default()
            },
            spec: PiNodeSpec {
                hardware_serial: serial.into(),
                mac_address: format!("aa:bb:cc:dd:ee:{}", serial),
                desired_role: role,
                target_disk_id: None,
                reformat_confirmed: false,
                ip_address: None,
                hostname: None,
            },
            status: Some(PiNodeStatus {
                phase,
                disk_state: DiskState::MountedGfs,
                last_heartbeat: None,
                promotion: None,
                active_mount: None,
                filesystem_uuid: None,
            }),
        }
    }

    #[tokio::test]
    async fn test_full_promotion_fsm_with_mock_gfs() {
        let mock_gfs = Arc::new(MockGfsAdminClient::new());
        mock_gfs.set_simulated_chunks("WORKER-01", 2).await;

        let etcd_checker = EtcdHealthChecker::new(
            "https://127.0.0.1:2379",
            None::<&str>,
            None::<&str>,
            None::<&str>,
        );
        let engine = PromotionEngine::new(mock_gfs, etcd_checker);

        let mut candidate = create_test_node("WORKER-01", NodeRole::Worker, NodePhase::Ready);
        let all_nodes = vec![
            create_test_node("MASTER-01", NodeRole::Master, NodePhase::Ready),
            candidate.clone(),
        ];

        // Step 1: Detecting -> SelectingCandidate (simulated master quorum down)
        // Set initial step to SelectingCandidate
        candidate.status.as_mut().unwrap().promotion = Some(PromotionState {
            candidate_node: "WORKER-01".into(),
            current_step: PromotionStep::SelectingCandidate,
            started_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            drain_handle: None,
            failure_reason: None,
            attempt_count: 1,
        });

        let s1 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s1, PromotionStep::EvacuatingGfs);

        // Step 2: EvacuatingGfs (chunk 2 -> 1)
        let s2 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s2, PromotionStep::EvacuatingGfs);

        // Step 3: EvacuatingGfs (chunk 1 -> 0 -> Complete)
        let s3 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s3, PromotionStep::PreparingDisk);

        // Step 4: PreparingDisk -> ReformattingDisk
        let s4 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s4, PromotionStep::ReformattingDisk);

        // Step 5: ReformattingDisk -> PromotingK3s
        let s5 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s5, PromotionStep::PromotingK3s);
        assert_eq!(candidate.spec.desired_role, NodeRole::Master);
        assert!(candidate.spec.reformat_confirmed);

        // Step 6: PromotingK3s -> JoiningEtcd
        let s6 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s6, PromotionStep::JoiningEtcd);

        // Step 7: JoiningEtcd -> Verifying
        let s7 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s7, PromotionStep::Verifying);

        // Step 8: Verifying -> Complete
        let s8 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s8, PromotionStep::Complete);
        assert_eq!(candidate.status.as_ref().unwrap().phase, NodePhase::Ready);
        assert_eq!(
            candidate.status.as_ref().unwrap().disk_state,
            DiskState::MountedEtcd
        );
    }

    #[tokio::test]
    async fn test_fsm_resumes_after_simulated_crash_mid_reformat() {
        let mock_gfs = Arc::new(MockGfsAdminClient::new());
        let etcd_checker = EtcdHealthChecker::new(
            "https://127.0.0.1:2379",
            None::<&str>,
            None::<&str>,
            None::<&str>,
        );
        let engine = PromotionEngine::new(mock_gfs, etcd_checker);

        let mut candidate = create_test_node("WORKER-02", NodeRole::Worker, NodePhase::Promoting);
        // Simulate crashed operator that left candidate in ReformattingDisk state
        candidate.status.as_mut().unwrap().promotion = Some(PromotionState {
            candidate_node: "WORKER-02".into(),
            current_step: PromotionStep::ReformattingDisk,
            started_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            drain_handle: Some("mock-drain-WORKER-02".into()),
            failure_reason: None,
            attempt_count: 1,
        });

        let all_nodes = vec![candidate.clone()];

        // On reboot, engine resumes directly from ReformattingDisk rather than restart
        let s1 = engine.step(&mut candidate, &all_nodes).await.unwrap();
        assert_eq!(s1, PromotionStep::PromotingK3s);
        assert_eq!(candidate.spec.desired_role, NodeRole::Master);
    }
}
