# cluster-operator — Dynamic Promotion & Quorum Auto-Healer

Kubernetes operator (`kube-rs`) responsible for maintaining control-plane etcd quorum, selecting candidate worker nodes for dynamic promotion, executing GFS chunk evacuation, and managing seed retirement.

---

## 1. Resumable Promotion State Machine

The Quorum Auto-Healer executes an explicit 9-state machine with every step persisted to `PiNode.status.promotion`:

```
[Detecting]
     │
     ▼
[SelectingCandidate] ── (Hold single-flight lease)
     │
     ▼
[EvacuatingGfs] ─────── (Call GfsAdminClient::drain_node)
     │
     ▼
[PreparingDisk] ─────── (cluster-ldm PrepareEvacuation)
     │
     ▼
[ReformattingDisk] ──── (cluster-ldm AssignRole(Master))
     │
     ▼
[PromotingK3s] ──────── (Restart k3s in server mode)
     │
     ▼
[JoiningEtcd] ───────── (Poll etcd member list)
     │
     ▼
[Verifying] ─────────── (Confirm quorum >= target_master_count)
     │
     ▼
[Complete] ──────────── (Release single-flight lease)
```

---

## 2. Cross-Repo Integration with `gfs-rs`

`cluster-operator` integrates with `gfs-rs` via `GfsAdminClient` to drain chunks before disk reformat. It provides:
- `GrpcGfsAdminClient`: Calls `AdminService.DrainNode` over gRPC in production.
- `MockGfsAdminClient`: Pluggable in-memory mock for automated chaos testing.
