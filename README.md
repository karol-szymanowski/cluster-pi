# pi-cluster-core

**Autonomous, self-bootstrapping, self-replicating bare-metal K3s platform for Raspberry Pi (ARM64)**

`pi-cluster-core` turns an unconfigured cluster of Raspberry Pi boards into a self-assembling, self-healing K3s cluster with zero manual intervention after the initial seed flash and per-board EEPROM network-boot configuration.

It operates beneath and around `gfs-rs` (the distributed filesystem in Rust), managing disk provisioning, ProxyDHCP/TFTP/HTTP network booting, and autonomous quorum lifecycle management (promotion, demotion, and seed retirement).

---

## 1. Architecture Overview

```
                      +---------------------------------------+
                      |         K3s Control Plane / VIP       |
                      |           (kube-vip L2 ARP)           |
                      +-------------------+-------------------+
                                          |
            +-----------------------------+-----------------------------+
            |                                                           |
+-----------v-----------+                                   +-----------v-----------+
|   cluster-operator    |                                   |    cluster-netboot    |
| - Leader-elected      |                                   | - Leader ProxyDHCP    |
| - Quorum Auto-Healer  |                                   | - Active-Active TFTP  |
| - Resumable FSM       |                                   | - Dynamic cloud-init  |
| - GfsAdminClient      |                                   | - GFS Asset Sync      |
+-----------+-----------+                                   +-----------+-----------+
            |                                                           |
            | (gRPC over host UDS)                                      | (PXE / HTTP / TFTP)
            v                                                           v
+-----------------------+                                   +-----------------------+
|      cluster-ldm      |                                   |  Unconfigured Pi Node  |
| (Host systemd daemon) |                                   | (EEPROM: Netboot 1st) |
| - Root-disk safety    |                                   | - Discovers via PXE   |
| - Formats etcd / GFS  |                                   | - Fetches bootloader  |
| - Manages /etc/fstab  |                                   | - Mounts root & joins |
+-----------------------+                                   +-----------------------+
```

### Core Components

1. **`cluster-common`**: Shared CRDs (`PiNode`, `ClusterTopology`), multi-signal boot-disk exclusion safety guards, immutable write-ahead `fsync` audit logs, block device introspection, and leader election.
2. **`cluster-ldm`**: Local Disk Manager — a pre-K3s host systemd daemon that prepares and mounts `/var/lib/rancher/k3s/server/db/etcd` (for Masters) or `/mnt/gfs-storage` (for Workers) before `k3s.service` boots, and exposes a Unix Domain Socket gRPC IPC service.
3. **`cluster-netboot`**: Resilient PXE engine featuring a strictly conforming RFC 4578 ProxyDHCP listener (never allocates IP leases, sets `yiaddr = 0.0.0.0`), TFTP boot asset server, and an Axum HTTP server generating per-node `cmdline.txt` and `cloud-init` user data.
4. **`cluster-operator`**: Kubernetes controller running a state-machine driven Quorum Auto-Healer. Upon master failure or loss of quorum, it selects a healthy worker, drains its GFS chunks, triggers remote disk reformatting via `cluster-ldm` IPC, promotes the node to etcd master, and manages seed retirement.

---

## 2. Irreversible Operations & Destructive Safety Protocol

`pi-cluster-core` enforces strict defense-in-depth safety protocols (Section 1.2) for any action capable of destroying data or disrupting the local network:

### 2.1 Disk Formatting & Partitioning
* **No Heuristic Targets**: Disks are identified solely by stable paths (`/dev/disk/by-id/*` or partition UUIDs), never volatile `/dev/sdX` handles.
* **Multi-Signal Boot-Disk Exclusion**: Before formatting, the target device is checked against:
  1. Static denylist (`mmcblk0`, `mmcblk0boot0`, `mmcblk0boot1`, known boot media).
  2. Live root filesystem device parsed directly from `/proc/mounts`.
  3. Live `/boot` and `/boot/firmware` backing devices.
  If **any** signal matches, formatting is aborted unconditionally with a fatal error.
* **Idempotency & Reformat Confirmation**: If a partition already contains the expected filesystem and label, formatting is skipped. Formatting an existing filesystem requires `reformat_confirmed: true` in the specification.
* **Write-Ahead Audit Trail**: Every disk mutation writes a structured JSON-lines record to `/var/log/cluster-ldm-audit.jsonl` with an explicit `fsync()` before the syscall is executed.
* **Global Dry-Run**: Supported via `--dry-run` flag on all binaries.

### 2.2 Network Safety (ProxyDHCP)
* **Zero Rogue DHCP**: The ProxyDHCP listener in `cluster-netboot` never acts as a standard DHCP server. It strictly:
  - Only replies to requests carrying DHCP Option 60 = `"PXEClient"`.
  - Always leaves `yiaddr` set to `0.0.0.0` (never assigns an IP).
  - Listens passively at startup to log surrounding DHCP servers.

### 2.3 Single-Flight Promotion Leases
* All topology mutations (promotions, demotions, seed eviction) require holding a dedicated Kubernetes `coordination.k8s.io` Lease (`cluster-topology-mutation`) to prevent split-brain reconcile races across operator pod restarts.

---

## 3. Physical Bootstrap Runbook

### Step 1: Flash Seed Node
On the provisioning workstation, run:
```bash
./scripts/bootstrap-seed.sh --device /dev/sdX --hostname pi-seed-01
```
The script inspects the device model/serial, prompts for confirmation, writes the base OS image, injects `cgroup_memory=1 cgroup_enable=memory` into `cmdline.txt`, and prepares the first-boot `cluster-ldm` role configuration (`Seed`).

### Step 2: Configure Worker EEPROM (One-Time per Board)
Connect each worker board and configure it for network boot preference:
```bash
./scripts/setup-eeprom.sh
```
This updates the bootloader EEPROM with `BOOT_ORDER=0xf241` (trying SD, NVMe, USB, then falling back to Network Boot / PXE), re-reads to verify, and outputs the hardware serial.

### Step 3: Boot Cluster
1. Power on `pi-seed-01`. It starts `cluster-ldm`, boots `k3s server --cluster-init`, and deploys the platform via Helm:
```bash
helm upgrade --install pi-cluster-core deploy/helm/pi-cluster-core -n kube-system --create-namespace
```
2. Power on the remaining Raspberry Pi worker boards.
3. The worker boards broadcast DHCPDISCOVER with option 60 `"PXEClient"`, receive the ProxyDHCP offer from `cluster-netboot`, download the ARM64 kernel/initramfs via TFTP, and fetch their customized `cloud-init` configuration over HTTP.
4. Each worker's `cluster-ldm` formats and mounts `/mnt/gfs-storage`, starts `k3s-agent`, and registers as a worker `PiNode`.
5. Monitor cluster self-assembly:
```bash
kubectl get pinodes -A -w
kubectl get clustertopologies
```

---

## 4. Development & Testing

```bash
# Run workspace test suite
make test

# Run clippy and format checks
make lint

# Run chaos tests (simulated loop devices and mock GFS admin)
make test-chaos

# Run network namespace tests (ProxyDHCP conformance)
make test-netns
```