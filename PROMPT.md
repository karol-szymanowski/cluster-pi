# PI-CLUSTER-CORE — Antigravity Agent Build Charter
### Autonomous, self-bootstrapping, self-replicating bare-metal K3s platform for Raspberry Pi (ARM64)

---

## 0. ROLE & MISSION

You are operating as an **autonomous Principal Distributed Systems / Infrastructure Rust Engineer** inside the Antigravity agent framework. Your mission is to design, implement, containerize, and test **`pi-cluster-core`** — the platform layer that turns a pile of unconfigured Raspberry Pi boards into a self-assembling, self-healing K3s cluster, with zero manual intervention after the one-time seed flash and per-board EEPROM configuration.

This repository is a **sibling** to the existing `gfs-rs` repository (the GFS-in-Rust distributed filesystem). `pi-cluster-core` does not reimplement GFS — it *consumes* `gfs-rs` as a set of published container images and client libraries, and it is responsible for the layer *beneath and around* GFS: disk provisioning, network boot, and cluster topology lifecycle (promotion, demotion, seed retirement).

You will work through **six sequential phases** (Section 4). Each phase has a hard **exit gate**: it must compile, pass `cargo clippy -- -D warnings`, and pass its own test suite before you proceed. Where this document is silent, make the most operationally-safe, Pi-cluster-appropriate decision, document it in the relevant crate's `README.md`, and proceed — do not stall waiting for clarification, **except** where Section 1.2 requires an explicit stop.

**No placeholder code.** Every function body is real, compilable, and behaviorally correct. `todo!()`, `unimplemented!()`, and stub logic are forbidden outside `#[cfg(test)]`.

---

## 1. NON-NEGOTIABLE ENGINEERING & SAFETY STANDARDS

### 1.1 General Rust Hygiene (consistent with `gfs-rs`)

| Rule | Requirement |
|---|---|
| Panics | No `unwrap()`/`expect()`/`panic!()` on I/O, device, network, or lock paths. Everything fallible returns `Result`. |
| Errors | Library crates use `thiserror`-typed errors; binaries aggregate with `anyhow` at `main()` only. |
| Concurrency | `tokio` multi-threaded runtime; every background loop (heartbeat watch, DHCP listener, reconcile loop, TFTP/HTTP servers) is spawned with a `tokio_util::sync::CancellationToken` and cleanly joined on shutdown. |
| Logging | `tracing` + `tracing-subscriber`, JSON formatting in release builds, `#[tracing::instrument]` on every reconcile/RPC/device-action function. Every **irreversible** action additionally writes a structured audit record (Section 1.2). |
| Config | All binaries configured via `clap` + env override. No hardcoded device paths, ports, VIPs, or tokens — these are cluster-specific and must be injectable. |
| Clippy/fmt | `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean at every phase gate. |
| Unsafe | Zero `unsafe` outside narrowly-scoped, `// SAFETY:`-commented device-ioctl or raw-socket code in `cluster-ldm`/`cluster-netboot`. |

### 1.2 Destructive-Operation Safety Protocol — MANDATORY, applies to `cluster-ldm` and `cluster-operator`

This repository can irreversibly destroy data (disk format) and disrupt a shared physical network (rogue DHCP). Treat every function capable of either as requiring **defense in depth**, not a single guard clause:

1. **Never infer destructiveness heuristically.** A disk is only eligible for formatting if an explicit, externally-supplied role assignment (from a `PiNode` CR or a signed local bootstrap manifest) names that exact device by a stable identifier (by-id path or partition UUID, never `/dev/sdX` — those enumerate non-deterministically on reboot).
2. **Multi-signal boot-disk exclusion.** Before any format/wipe, cross-check the target device against *all* of: (a) a static denylist of known boot device patterns (`mmcblk0`, `mmcblk0boot0/1`, the NVMe/USB device presented as boot media on Pi 4/5 depending on boot mode), (b) the live root device resolved by parsing `/proc/mounts` for the device backing `/`, (c) any device currently backing `/boot` or `/boot/firmware`. If **any** signal says "this is the boot device," refuse unconditionally — do not let a passed-in role override this.
3. **Idempotency before irreversibility.** Before formatting, inspect the target device's existing filesystem/UUID/label. If it already matches the desired role's expected layout, skip formatting and just (re)mount. Only format a device that is blank or explicitly marked `reformat: true` with a second, distinct confirmation field in the request (not just the role field) — this prevents a single buggy CR write from wiping a live node.
4. **Audit trail.** Every format/mount/unmount/EEPROM write appends an immutable, `fsync`ed, append-only JSON-lines record (`device_serial`, `action`, `requested_by`, `before_state`, `after_state`, `timestamp`) to a local audit log **before** executing the action, and a completion record after. This log must survive the action it describes (write-ahead, not write-after).
5. **Global dry-run.** Every binary in this repo supports `--dry-run`, which executes the full decision logic (discovery, safety checks, role resolution) and logs exactly what *would* happen, performing zero mutating syscalls.
6. **Network safety for `cluster-netboot`.** The ProxyDHCP responder must be structurally incapable of acting as a full DHCP server: it must never populate the `yiaddr` field, never maintain a lease table, and must only ever respond to DHCPDISCOVER/DHCPREQUEST packets carrying DHCP option 60 = `"PXEClient"`. On startup it performs a passive listen-only self-check for existing DHCP traffic on the segment and logs what it observes, but does not require an existing server to be present to proceed (some sites intentionally have PXE-only segments).
7. **Single-flight on irreversible cluster operations.** `cluster-operator` must hold a dedicated `coordination.k8s.io` Lease (distinct from its leader-election lease) for the duration of any promotion/demotion/seed-eviction workflow, so two reconcile loops (even across a leader failover) can never race on the same physical disk or quorum change.
8. **Explicit stop condition.** If, at any point, the safety checks in this section produce an ambiguous result (e.g., a device matches the denylist pattern by string but its `/proc/mounts` cross-check disagrees), the agent must implement this as a hard `Err` that blocks the action and surfaces a clear operator-facing alert — this is the one category of ambiguity you do **not** resolve by guessing.

---

## 2. CROSS-REPO INTEGRATION CONTRACT WITH `gfs-rs`

`pi-cluster-core` depends on `gfs-rs` in three concrete ways. Treat these as an external API contract — implement `pi-cluster-core`'s side against a trait/adapter so it can be built and tested independently of `gfs-rs`'s exact release state:

1. **Container images.** `deploy/k8s/gfs-integration/*.yaml` in this repo reference `gfs-rs`'s published `gfs-master`, `gfs-chunkserver` images by tag *and* digest (never `:latest`). Vendor only the minimal manifest fields this repo needs to override (node selectors, VIP-relative service names) — do not fork `gfs-rs`'s manifests wholesale.
2. **Boot-asset sync via `gfs-fuse`.** `cluster-netboot` mounts a GFS path (e.g. `/mnt/gfs/netboot-assets`) using the `gfs-fuse` binary from `gfs-rs` as an external dependency (invoked as a subprocess or, if `gfs-rs` publishes `gfs-client` as a library crate, linked directly — prefer the library path if available, document whichever you use). This is how any surviving master can serve netboot assets after the seed node is gone.
3. **Node evacuation for promotion/demotion.** `cluster-operator`'s Quorum Auto-Healer must be able to ask GFS to drain a node's chunks before that node's disk is reformatted for etcd use. **`gfs-rs`'s `ClientMasterService` as specified does not yet expose this.** Define the expected contract explicitly and build against it via an adapter trait:
   ```rust
   #[async_trait]
   pub trait GfsAdminClient: Send + Sync {
       /// Instructs the GFS master to stop placing new chunks on `node` and
       /// begin re-replicating its existing chunks elsewhere. Returns immediately;
       /// poll `drain_status` for completion.
       async fn drain_node(&self, node: NodeId) -> Result<DrainHandle, GfsAdminError>;
       async fn drain_status(&self, handle: &DrainHandle) -> Result<DrainStatus, GfsAdminError>;
   }
   pub enum DrainStatus { InProgress { chunks_remaining: u32 }, Complete, Failed(String) }
   ```
   Ship a real `GrpcGfsAdminClient` implementation that calls a *proposed* `AdminService.DrainNode` RPC (document the exact `.proto` addition needed in `gfs-rs`'s `gfs-proto` crate as a `PROPOSED_UPSTREAM.md` note in this repo), **and** a `MockGfsAdminClient` used in this repo's own test suite so `cluster-operator`'s promotion state machine is fully testable today without blocking on the other repo shipping the endpoint. Wire `cluster-operator` to select the implementation via config, defaulting to the gRPC one in production.

---

## 3. CARGO WORKSPACE LAYOUT

```
pi-cluster-core/
├── Cargo.toml
├── Makefile
├── README.md
├── PROPOSED_UPSTREAM.md          # documents the gfs-rs AdminService.DrainNode ask
├── crates/
│   ├── cluster-common/            # shared CRDs, safety-guard, audit log, device model
│   │   └── src/{crd.rs, safety.rs, audit.rs, device.rs, error.rs}
│   ├── cluster-ldm/                # Local Disk Manager Daemon (systemd, pre-boot)
│   │   └── src/{main.rs, discover.rs, format.rs, mount.rs, fstab.rs, ipc.rs}
│   ├── cluster-netboot/            # PXE / ProxyDHCP / TFTP / HTTP netboot engine
│   │   └── src/{main.rs, dhcp.rs, tftp.rs, http.rs, election.rs, asset_sync.rs, cloudinit.rs}
│   └── cluster-operator/           # kube-rs controller: auto-heal, promote, evict
│       └── src/{main.rs, election.rs, reconcile.rs, promotion_fsm.rs, candidate.rs, etcd_health.rs, gfs_admin.rs}
├── scripts/
│   ├── bootstrap-seed.sh
│   └── setup-eeprom.sh
├── deploy/
│   ├── docker/
│   │   ├── Dockerfile.netboot
│   │   └── Dockerfile.operator     # cluster-ldm is NOT containerized — see Phase 1
│   └── k8s/
│       ├── crds/{pinode.yaml, clustertopology.yaml}
│       ├── rbac/{operator-rbac.yaml, kubevip-rbac.yaml}
│       ├── kube-vip-daemonset.yaml
│       ├── netboot-deployment.yaml
│       ├── operator-deployment.yaml
│       └── gfs-integration/        # thin overlays on gfs-rs manifests
└── tests/
    ├── chaos/                      # multi-node promotion/eviction simulation
    └── netns/                      # network-namespace DHCP conformance tests
```

---

## 4. PHASE-BY-PHASE ROADMAP

### PHASE 0 — `cluster-common`: Shared Contracts

Define once, share everywhere:

**CRDs** (via `kube::CustomResource` derive):
```rust
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(group = "cluster.pi.io", version = "v1", kind = "PiNode",
       status = "PiNodeStatus", namespaced)]
pub struct PiNodeSpec {
    pub hardware_serial: String,      // Pi's CPU serial, stable across reboots/reflash
    pub mac_address: String,          // for cloud-init/PXE lookup by cluster-netboot
    pub desired_role: NodeRole,       // Seed | Master | Worker | Pending | Decommissioned
    pub target_disk_id: Option<String>, // by-id path or partition UUID, never /dev/sdX
    pub reformat_confirmed: bool,     // second, distinct confirmation for destructive format
}
pub struct PiNodeStatus {
    pub phase: NodePhase,             // Discovered|Provisioning|Ready|Promoting|Draining|Decommissioning|Failed
    pub disk_state: DiskState,        // Unformatted|Formatting|MountedEtcd|MountedGfs|Error(String)
    pub last_heartbeat: Option<Time>,
    pub promotion: Option<PromotionState>,   // resumable FSM snapshot, see Phase 3
}

#[derive(CustomResource, ...)]
#[kube(group = "cluster.pi.io", version = "v1", kind = "ClusterTopology", cluster)]
pub struct ClusterTopologySpec {
    pub seed_node: String,            // hardware_serial of the original seed
    pub target_master_count: u8,      // default 3
    pub target_gfs_replication: u8,   // default 3, informational — enforced by gfs-master
}
```

**`safety.rs`**: the single shared implementation of the Section 1.2 multi-signal boot-disk exclusion and idempotency check, used identically by `cluster-ldm` (local) and `cluster-operator` (via `cluster-ldm`'s IPC, never bypassing it).

**`audit.rs`**: `AuditLog` type — append-only JSONL writer with `fsync`-before-return semantics, plus a `AuditEntry` builder used at every call site described in 1.2.4.

**`device.rs`**: `BlockDevice` model wrapping `sysfs` reads (`/sys/class/block/*/size`, `/uevent`, `/dev/disk/by-id/*` resolution), and root-device resolution via `/proc/mounts` parsing — no shelling out to `lsblk`/`findmnt`, parse directly for determinism and testability (inject a fake `/proc/mounts`-like reader in tests).

---

### PHASE 1 — `cluster-ldm`: Local Disk Manager Daemon

`cluster-ldm` is a **host-level systemd daemon**, not a Kubernetes workload — it must be running and have provisioned `/var/lib/rancher/k3s/server/db/etcd` or `/mnt/gfs-storage` *before* `k3s.service` starts. Document this explicitly in the crate README with the required unit ordering:

```ini
# /etc/systemd/system/cluster-ldm.service
[Unit]
Description=Pi Cluster Local Disk Manager
After=local-fs-pre.target
Before=k3s.service k3s-agent.service
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/cluster-ldm provision --role-source=/etc/cluster-ldm/role.json
ExecStartPost=/usr/local/bin/cluster-ldm ipc-serve --socket=/run/cluster-ldm.sock &
[Install]
WantedBy=multi-user.target
```

**Requirements:**
1. `discover.rs`: enumerate `/sys/class/block/*`, skip `loop*`/`ram*`/partitions-of-already-seen-disks, resolve each to a `BlockDevice`, apply `cluster-common::safety` root-device exclusion.
2. `format.rs`: role-specific `mkfs.ext4` invocations via `tokio::process::Command` (never a hand-rolled ext4 formatter — shell out to `mkfs.ext4`, but validate its exit status and stderr, do not assume success):
   - **Master (etcd)**: `mkfs.ext4 -b 4096 -O ^has_journal? — NO, keep journal for `data=ordered`` → concretely: `mkfs.ext4 -b 4096 <device>`, mount options `data=ordered,barrier=1,noatime`, and post-mount set `ionice -c 1 -p <pid-of-etcd-owning-process>` is not directly settable per-mount — instead apply `ionice -c 1` to the k3s server process launch (document this handoff: `cluster-ldm` cannot set I/O class on a process it doesn't own; it writes the required `IOSchedulingClass=1` into a systemd drop-in for `k3s.service` as part of provisioning, which is the correct mechanism).
   - **Worker (GFS)**: `mkfs.ext4 -T largefile4 <device>`, mount options `commit=60,noatime`, target `/mnt/gfs-storage`.
3. `mount.rs` + `fstab.rs`: mount by UUID (`blkid`-parsed, not assumed), write idempotent `/etc/fstab` entries (check for existing entry matching the UUID before appending; replace stale entries rather than duplicating).
4. `ipc.rs`: local Unix Domain Socket `tonic` service (UDS transport, not TCP — this is host-local only) exposing:
   - `AssignRole(RoleAssignment) -> ProvisionResult` — used by `cluster-operator` for runtime role transitions (promotion/demotion), routes through the exact same `safety.rs` + idempotency path as boot-time provisioning.
   - `GetDiskState(()) -> DiskStateReport`
   - `PrepareEvacuation(())` / `ConfirmUnmounted(())` — two-phase unmount so `cluster-operator` can be certain the GFS mount is fully released before triggering reformat.
5. Role source at boot (`/etc/cluster-ldm/role.json`) is written once by `bootstrap-seed.sh` (for the seed) or by `cluster-netboot`'s generated `cloud-init` (for netbooted workers) — `cluster-ldm` itself never reaches out to the network to determine its own role at first boot; this keeps it dependency-free during the earliest boot phase.

---

### PHASE 2 — `cluster-netboot`: Resilient PXE & Netboot Engine

Runs as a **host-networked** Kubernetes `Deployment` (not DaemonSet — you want it schedulable onto master nodes generally, not on every node), bound to the `kube-vip`-managed VIP (`192.168.1.200`, configurable).

1. **`election.rs`**: a `kube-rs` Lease election gate, identical pattern to `gfs-master`'s election (reuse the design for consistency, do not invent a second pattern) — but scoped *only* to the ProxyDHCP UDP listener. TFTP and HTTP asset serving are stateless/read-only and safe to run **active-active** across all replicas sitting behind the VIP; only the DHCP responder must be single-active to avoid duplicate/conflicting PXE offers hitting the same booting board.
2. **`dhcp.rs`**: raw UDP socket bound to `67`/`4011`, packet parsing via `dhcproto` (or equivalent). Strict ProxyDHCP semantics (RFC 4578):
   | Field | Value |
   |---|---|
   | Trigger | Only DHCPDISCOVER/DHCPREQUEST with option 60 = `"PXEClient"` |
   | `yiaddr` | **Always 0.0.0.0** — never assign an IP lease |
   | `siaddr` | VIP address (this server) |
   | `file` | ARM64 bootloader path (`bootcode.bin` / TFTP-served chainload target) resolved from the requesting `chaddr` (MAC) via a `PiNode` CRD lookup |
   | Option 43 | PXE vendor-specific: boot server discovery + multicast disabled |
   | Option 60 (reply) | `"PXEClient"` echoed back |

   Startup self-check: passively sniff for existing DHCPOFFER traffic on the interface for a few seconds, log server IPs observed (informational only — does not block startup, since PXE-only segments legitimately have no other DHCP server).
3. **`tftp.rs`**: serves `bootcode.bin`, `start4.elf`, `fixup4.dat`, `config.txt`, `vmlinuz`, `initramfs` from a local on-disk cache (kept warm by `asset_sync.rs`). Use `async-tftp` or a hand-rolled `tokio::net::UdpSocket` RRQ/DATA/ACK state machine if a suitable crate isn't available — either is acceptable, but it must handle concurrent transfers to multiple booting boards without head-of-line blocking.
4. **`http.rs`** (via `axum`): serves larger assets and, critically, **dynamically generates** per-node `cmdline.txt` and `cloud-init` `user-data` at request time by looking up the requesting node (by MAC, passed as a query param the initramfs/bootloader is configured to send, or by source IP correlated to a DHCP transaction log) against its `PiNode` CR — injecting `cgroup_memory=1 cgroup_enable=memory`, the resolved `desired_role`, the k3s join token (from a `Secret`, never hardcoded), and the VIP address of the join target.
5. **`asset_sync.rs`**: background task mounting/reading the GFS path per Section 2.2, syncing the canonical OS image set into the local TFTP/HTTP cache, refreshing on a poll interval and on-demand when an operator pushes a new image. This is what makes any surviving master able to serve netboot after the seed disconnects — document this failover property explicitly in the README with a sequence description.
6. **VIP/election interaction**: document explicitly that `kube-vip` (L2 ARP mode, Phase 4 manifest) owns VIP failover at the network layer, while this crate's internal Lease owns *which pod* is allowed to bind the DHCP socket — these are intentionally decoupled: the VIP can move to any replica, but only the DHCP-election leader replica actually opens port 67/4011 (others hold the socket closed), preventing a window where two pods both think they own the VIP and both answer DHCP.

---

### PHASE 3 — `cluster-operator`: Self-Replication & Dynamic Promotion Operator

A `kube-rs` controller, itself leader-elected (reuse the `gfs-master`/`cluster-netboot` election pattern — third and final reuse, confirming this is the one canonical pattern across the whole platform).

**Quorum Auto-Healer — implemented as an explicit, persisted, resumable state machine** (not an imperative function — a crash or leader failover mid-promotion must resume from the last completed step, never restart blindly or leave a node half-provisioned):

| State | Entry Condition | Action | Exit Condition | Failure Handling |
|---|---|---|---|---|
| `Detecting` | Master node's heartbeat stale beyond threshold **and** etcd member list confirms it unreachable | Confirm via `etcd_health.rs` (query embedded etcd member health, not just K8s `Node` status — a `Node` can look `NotReady` while etcd is still fine, and vice versa) | Failure confirmed on both signals | Abort, log, no state change |
| `SelectingCandidate` | Entered `Detecting` confirmed | Score eligible `Worker`-role `PiNode`s by free disk, current GFS chunk load (via `gfs_admin.rs`), heartbeat freshness | Candidate selected, acquire single-flight Lease (1.2.7) | If no eligible candidate, transition to `Failed` with alert, do not retry-loop silently |
| `EvacuatingGfs` | Candidate locked | Call `GfsAdminClient::drain_node` (Section 2.3) | `DrainStatus::Complete` | On `Failed(reason)`, abort to `Failed`, release single-flight lease |
| `PreparingDisk` | Drain complete | Call candidate's `cluster-ldm` IPC `PrepareEvacuation` then `ConfirmUnmounted` | Unmount confirmed | Timeout → `Failed` |
| `ReformattingDisk` | Unmount confirmed | Call `cluster-ldm` IPC `AssignRole(Master)` with `reformat_confirmed=true` | `ProvisionResult::Success` | Format failure → `Failed`, node marked `Error` disk state, **do not retry automatically** — requires operator ack |
| `PromotingK3s` | Disk ready for etcd | Rewrite the node's k3s systemd unit from agent→server mode (via SSH/agent-side helper, or a pre-installed local `k3s-role-switch` script this repo also ships) with the join token and VIP target, restart `k3s.service` | k3s process reports `Ready` in K8s `Node` status | Restart failure → `Failed` |
| `JoiningEtcd` | k3s server started | Poll embedded etcd member list for the new member in `started` state | Member started and healthy | Timeout beyond a generous bound → `Failed`, flag for manual etcd member remove |
| `Verifying` | Member started | Re-check overall quorum size meets `ClusterTopology.target_master_count` | Verified | — |
| `Complete` | Verified | Update `PiNode.status.phase = Ready`, release single-flight lease, emit K8s `Event` | — | — |
| `Failed` | Any step failed | Persist failure reason to `PiNode.status.promotion`, release single-flight lease, emit a loud K8s `Event`/log at `error` level | Requires human/operator intervention to clear before another promotion targeting the same node is attempted | — |

Persist `PromotionState` (current state name + relevant IDs) into `PiNode.status.promotion` after **every** transition, so `reconcile.rs`'s normal reconcile loop — not a separate recovery code path — naturally resumes an in-flight promotion from wherever it left off on next reconcile, including after an operator pod restart.

**Seed Eviction Routine**: once `ClusterTopology` shows `target_master_count` promoted masters healthy in quorum, expose a controller-driven trigger (a `PiNode.spec.desired_role` edit on the seed to `Decommissioned`, or a `gfs-cli`-style admin command) that: etcd-member-removes the seed, then either fully powers it down (documented manual step) or chains into `cluster-ldm`'s `AssignRole(Worker)` to repurpose it — reusing the exact same promotion-style FSM pattern in reverse, do not write a second bespoke code path.

**`etcd_health.rs`**: talks to K3s's embedded etcd health/member-list endpoint (document the exact endpoint and auth/cert requirements you use — K3s exposes this over the same client-cert-secured port as the API server's etcd proxy; do not assume plaintext access).

---

### PHASE 4 — Bootstrapping Scripts & Manifests

1. **`scripts/bootstrap-seed.sh`**: flashes the seed board's storage (SD/SSD) via `rpi-imager --cli` (preferred, falls back to documented `dd` invocation with an explicit `--i-am-sure` confirmation flag since `dd` to the wrong device is catastrophic), mounts the resulting boot partition, appends `cgroup_memory=1 cgroup_enable=memory` to `cmdline.txt` (idempotent — check it isn't already present), and injects a first-boot systemd unit that installs `cluster-ldm` + writes `/etc/cluster-ldm/role.json` with `role: Seed`, and runs `k3s server --cluster-init`. Script must refuse to run without an explicit `--device /dev/sdX` argument (never auto-detect a target for a destructive flash) and must print the resolved device's model/serial for the operator to visually confirm before proceeding.
2. **`scripts/setup-eeprom.sh`**: per-board, one-time, run manually with the board connected — uses `rpi-eeprom-config`/`rpi-eeprom-update` to set `BOOT_ORDER` to prefer network boot, validates the change was applied by re-reading the config, and logs the board's serial for audit correlation with its eventual `PiNode` CR. Idempotent: no-ops if the config already matches.
3. **`deploy/k8s/crds/`**: the `PiNode` and `ClusterTopology` CRD YAMLs matching Phase 0's Rust types exactly (generate via `kube::CustomResourceExt::crd()` at build time or a small `xtask` to avoid drift between Rust structs and the applied CRD).
4. **`deploy/k8s/kube-vip-daemonset.yaml`**: L2 ARP mode DaemonSet, RBAC for `kube-vip` to manage `services`/`endpoints`/`leases`.
5. **`deploy/k8s/netboot-deployment.yaml`** / **`operator-deployment.yaml`**: host-networked for netboot; standard pod networking + RBAC for the operator (CRUD on `pinodes`/`clustertopologies`, read on core `Node`/`Event`, CRUD on `leases.coordination.k8s.io`, read on `secrets` for join tokens).
6. **`deploy/k8s/gfs-integration/`**: thin overlay referencing `gfs-rs`'s images per Section 2.1 — this repo does not own or duplicate `gfs-rs`'s core manifests, only the glue needed to schedule them correctly relative to this platform's node roles.

---

### PHASE 5 — Verification & Chaos Testing

1. **Safety-guard exhaustive tests** (`cluster-common`): table-driven tests attempting to trick the boot-disk exclusion via symlinked paths, partition-vs-whole-disk confusion, by-label vs by-UUID mismatches, and a device that only *becomes* the root device after a simulated late remount — all must be refused.
2. **`tests/netns/`**: DHCP conformance tests using Linux network namespaces (`ip netns`) to create an isolated virtual link, run the real `dhcp.rs` responder against synthetic DHCPDISCOVER packets, and assert byte-for-byte that `yiaddr` is always zero, non-PXEClient discovers get zero response, and PXEClient discovers get a spec-correct offer.
3. **`tests/chaos/`**: spawn real `cluster-ldm` and `cluster-operator` binaries against loopback block devices created via `losetup` + sparse files (never real disks in CI), run a full promotion end-to-end using the `MockGfsAdminClient`, then `SIGKILL` the operator process mid-`ReformattingDisk` and assert that on restart it resumes from persisted `PiNode.status.promotion` state rather than re-entering `Detecting` or double-formatting. Also cover the seed eviction routine end-to-end once 3 mock masters are "promoted."
4. Optional but recommended: a `qemu-system-aarch64`-based smoke test booting a minimal ARM64 image against `cluster-netboot`'s TFTP/HTTP servers to validate the actual netboot chain, gated behind a `--features qemu-smoke` flag since it's slow and environment-dependent.

---

## 5. DELIVERABLE CONSTRAINTS

- Root `Cargo.toml`: workspace with all four crates as members, `[workspace.dependencies]` pinning `tokio`, `tonic`, `kube`, `k8s-openapi`, `dhcproto` (or chosen DHCP crate), `axum`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `serde`, `schemars`, `clap`, `tokio-util`. `rustls`-based TLS throughout (consistent with `gfs-rs`), musl + `aarch64`/`x86_64` cross targets via `cargo-zigbuild`.
- `Makefile` targets: `build`, `build-arm64`, `test`, `test-chaos`, `test-netns` (may require `sudo`/`CAP_NET_ADMIN` — document this), `lint`, `docker-build` (netboot + operator only — `cluster-ldm` ships as a static binary + systemd unit, not a container), `k3s-deploy`, `clean`.
- `README.md`: full physical bootstrap runbook end-to-end — flash seed (`bootstrap-seed.sh`) → power on seed, confirm `k3s server --cluster-init` healthy → run `setup-eeprom.sh` once per worker board → power on workers, confirm they netboot and self-assign roles → confirm quorum via `kubectl get pinodes` / `clustertopology` status. Plus a **dedicated "Irreversible Operations" section** enumerating every action in the repo capable of destroying data or disrupting the network, cross-referenced to Section 1.2's guards.
- `PROPOSED_UPSTREAM.md`: the exact `.proto` addition requested of `gfs-rs` (Section 2.3), so the two repos' interface expectations stay traceable even though they're developed independently.
- No placeholders anywhere in `crates/*/src`.

---

## 6. DEFINITION OF DONE — ACCEPTANCE CHECKLIST

- [ ] `cargo build --workspace` succeeds for `aarch64-unknown-linux-musl` and `x86_64-unknown-linux-musl`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.
- [ ] Safety-guard test suite passes, including every disguised-boot-device adversarial case.
- [ ] `dhcp.rs` never sets `yiaddr`, never responds to non-PXEClient discovers, verified in `tests/netns/`.
- [ ] A simulated seed flash + two simulated netbooted workers self-assemble into a 3-node cluster in the chaos harness, with `ClusterTopology.status` reflecting quorum reached.
- [ ] Killing a simulated master in the chaos harness results in the operator's promotion FSM completing end-to-end against `MockGfsAdminClient`, landing in `Complete`.
- [ ] Killing `cluster-operator` mid-`ReformattingDisk` and restarting it resumes from persisted state rather than re-running or skipping steps.
- [ ] Seed eviction routine completes end-to-end once mock quorum of 3 is reached.
- [ ] `cluster-ldm` refuses to format a device matching any Section 1.2.2 boot-device signal, under every adversarial test case.
- [ ] `deploy/k8s/` applies cleanly (CRDs, RBAC, kube-vip, netboot, operator) to a `k3d`/`kind`-simulated or real ARM64-labeled cluster.
- [ ] `PROPOSED_UPSTREAM.md` accurately reflects the exact interface `cluster-operator` expects from `gfs-rs`.

---

## 7. EXECUTION DIRECTIVES FOR THE AGENT

1. Work phase-by-phase in order; Phase 0 (`cluster-common`) blocks all others since every other crate depends on its CRDs, safety guard, and audit log.
2. Section 1.2 is the highest-priority section in this entire document — when in doubt about any code path touching disks, DHCP, or quorum membership, re-read it before writing the code, not after.
3. Do not implement a second, different leader-election pattern for `cluster-netboot`'s DHCP gate or `cluster-operator`'s controller — reuse one canonical `kube-rs` Lease-election module (put it in `cluster-common` if it isn't already, refactoring the earlier `gfs-master` pattern's design intent into shared code here since this repo owns three separate election use sites).
4. Where this repo's correctness depends on `gfs-rs` shipping something it doesn't have yet (Section 2.3), build fully against the adapter trait and mock — do not block progress on the other repo, and do not silently downgrade the feature to something weaker without flagging it in `PROPOSED_UPSTREAM.md`.
5. Every destructive action must be traceable end-to-end: which `PiNode` CR field authorized it, which safety checks it passed, and its audit log entry. If you can't answer "why was this device formatted" from logs alone after the fact, the implementation isn't done.
6. Stop and flag only for genuine physical-safety ambiguity (e.g., an EEPROM write whose outcome can't be verified by re-read) — everything else, decide per this document's stated defaults and proceed.
