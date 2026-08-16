# cluster-netboot — Resilient PXE & Netboot Engine

Host-networked Kubernetes deployment running on the cluster control-plane nodes, managing network boot for unconfigured Raspberry Pi boards.

---

## 1. Network Architecture & Election Model

```
                    kube-vip (L2 ARP Virtual IP: 192.168.1.200)
                                          |
             +----------------------------+----------------------------+
             |                                                         |
     [Pod: netboot-0]                                          [Pod: netboot-1]
   - HTTP server (Active-Active)                             - HTTP server (Active-Active)
   - TFTP server (Active-Active)                             - TFTP server (Active-Active)
   - GFS asset syncer (Active-Active)                        - GFS asset syncer (Active-Active)
   - ProxyDHCP (LEADER: Port 67 open)                        - ProxyDHCP (STANDBY: Port 67 closed)
```

* **Active-Active Read-Only Services**: HTTP (port 8080) and TFTP (port 69) serve read-only kernel/initramfs assets and dynamic cloud-init templates concurrently across all replicas behind the VIP.
* **Single-Active ProxyDHCP Listener**: Only the replica holding the `coordination.k8s.io` Lease `cluster-netboot-dhcp` opens UDP port 67/4011. Standby replicas keep port 67 closed, eliminating race conditions or duplicate PXE offers.
* **RFC 4578 Conformance**: ProxyDHCP strictly leaves `yiaddr = 0.0.0.0`, returns Option 60 `"PXEClient"`, and supplies Option 43 PXE vendor options.

---

## 2. GFS Asset Synchronization & Seed Failover

When the original seed node is retired or disconnected:
1. Canonical OS bootloader images, kernels (`vmlinuz`), and initramfs files are mirrored to the shared GFS volume (`/mnt/gfs/netboot-assets`).
2. Each `cluster-netboot` instance syncs changes from the GFS volume into its local cache `/var/lib/cluster-netboot/assets`.
3. If the seed node shuts down, any promoted master node running `cluster-netboot` continues serving netboot assets to new and existing boards without data loss.
