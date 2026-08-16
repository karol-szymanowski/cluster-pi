# cluster-ldm — Local Disk Manager Daemon

Host-level systemd daemon for bare-metal Raspberry Pi nodes. `cluster-ldm` executes before `k3s.service` / `k3s-agent.service` starts to discover, safety-evaluate, format, and mount the dedicated NVMe/SSD data volume.

---

## 1. Systemd Unit Ordering

`cluster-ldm` must run and exit successfully before Kubernetes / K3s begins:

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

---

## 2. Storage Roles & Mount Configurations

| Role | Filesystem Format | Mount Target | Options | Notes |
|---|---|---|---|---|
| `Master` / `Seed` | `mkfs.ext4 -b 4096` | `/var/lib/rancher/k3s/server/db/etcd` | `data=ordered,barrier=1,noatime` | Installs `/etc/systemd/system/k3s.service.d/io-priority.conf` with `IOSchedulingClass=1` |
| `Worker` | `mkfs.ext4 -T largefile4` | `/mnt/gfs-storage` | `commit=60,noatime` | Dedicated storage volume for GFS chunkserver |

---

## 3. Host-Local IPC Service (UDS)

`cluster-ldm` provides a host-only Unix Domain Socket at `/run/cluster-ldm.sock` for runtime orchestration:
* `AssignRole(RoleAssignment)`: Runtime disk reformatting and mounting during operator promotion/demotion.
* `GetDiskState(())`: Reports active disk layout and mount points.
* `PrepareEvacuation(())`: Flushes writes and unmounts `/mnt/gfs-storage`.
* `ConfirmUnmounted(())`: Verifies mount release prior to reformat.
