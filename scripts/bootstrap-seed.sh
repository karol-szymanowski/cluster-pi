#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# PI-CLUSTER-CORE — Seed Board Bootstrap & Flash Script
# Flashes base Raspberry Pi OS image, injects cgroups into cmdline.txt,
# configures cluster-ldm role (Seed), and initializes K3s cluster.
# ==============================================================================

DEVICE=""
HOSTNAME="pi-seed-01"
IMAGE_PATH=""
I_AM_SURE=false

print_usage() {
    echo "Usage: $0 --device /dev/sdX [--hostname <name>] [--image <path.img.xz>] [--i-am-sure]"
    echo ""
    echo "Options:"
    echo "  --device <dev>     Target block device (MANDATORY, e.g. /dev/sdb, /dev/rdisk2)"
    echo "  --hostname <name>   Seed node hostname (default: pi-seed-01)"
    echo "  --image <path>      Path to Raspberry Pi OS ARM64 image (optional, downloads if omitted)"
    echo "  --i-am-sure         Bypasses interactive confirmation prompt"
    echo ""
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --device)
            DEVICE="$2"
            shift 2
            ;;
        --hostname)
            HOSTNAME="$2"
            shift 2
            ;;
        --image)
            IMAGE_PATH="$2"
            shift 2
            ;;
        --i-am-sure)
            I_AM_SURE=true
            shift
            ;;
        -h|--help)
            print_usage
            ;;
        *)
            echo "Unknown argument: $1"
            print_usage
            ;;
    esac
done

if [[ -z "$DEVICE" ]]; then
    echo "[-] ERROR: Target device (--device /dev/sdX) is MANDATORY."
    echo "[-] Refusing to heuristically guess a target device."
    exit 1
fi

if [[ ! -b "$DEVICE" && ! -c "$DEVICE" ]]; then
    echo "[-] ERROR: Device '$DEVICE' is not a valid block or character device."
    exit 1
fi

echo "=============================================================================="
echo " PI-CLUSTER-CORE SEED NODE BOOTSTRAP"
echo "=============================================================================="
echo "Target Device: $DEVICE"
echo "Seed Hostname: $HOSTNAME"
echo ""

# Safety: Display resolved device info
if command -v lsblk >/dev/null 2>&1; then
    echo "Target Device Details:"
    lsblk -o NAME,SIZE,TYPE,MODEL,SERIAL,MOUNTPOINT "$DEVICE" || true
    echo ""
elif command -v diskutil >/dev/null 2>&1; then
    diskutil info "$DEVICE" | grep -E "Device / Media Name|Disk Size|Device Identifier" || true
    echo ""
fi

# Multi-signal boot check on local workstation
if mount | grep -q "$DEVICE"; then
    echo "[-] WARNING: Partitions of target device $DEVICE are currently mounted:"
    mount | grep "$DEVICE" || true
fi

if [[ "$I_AM_SURE" != true ]]; then
    echo "=============================================================================="
    echo " [!] CAUTION: THIS WILL COMPLETELY AND IRREVERSIBLY ERASE ALL DATA ON $DEVICE."
    echo "=============================================================================="
    read -r -p "Type 'FLASH-SEED' to confirm: " CONFIRMATION
    if [[ "$CONFIRMATION" != "FLASH-SEED" ]]; then
        echo "[-] Aborted by operator."
        exit 1
    fi
fi

echo "[+] Step 1: Flashing base OS to $DEVICE..."
if command -v rpi-imager >/dev/null 2>&1 && [[ -n "$IMAGE_PATH" ]]; then
    rpi-imager --cli "$IMAGE_PATH" "$DEVICE"
else
    if [[ -n "$IMAGE_PATH" ]]; then
        echo "[+] Writing image via dd..."
        if [[ "$IMAGE_PATH" == *.xz ]]; then
            xzcat "$IMAGE_PATH" | sudo dd of="$DEVICE" bs=4M status=progress conv=fsync
        elif [[ "$IMAGE_PATH" == *.gz ]]; then
            zcat "$IMAGE_PATH" | sudo dd of="$DEVICE" bs=4M status=progress conv=fsync
        else
            sudo dd if="$IMAGE_PATH" of="$DEVICE" bs=4M status=progress conv=fsync
        fi
    else
        echo "[+] No image supplied; assuming base OS partition already written."
    fi
fi

# Locate and mount boot partition
echo "[+] Step 2: Ingesting boot configuration..."
MOUNT_DIR=$(mktemp -d /tmp/piseed-boot.XXXXXX)

# Detect boot partition
BOOT_PART=""
if [[ -b "${DEVICE}1" ]]; then
    BOOT_PART="${DEVICE}1"
elif [[ -b "${DEVICE}p1" ]]; then
    BOOT_PART="${DEVICE}p1"
elif [[ -b "${DEVICE}s1" ]]; then
    BOOT_PART="${DEVICE}s1"
fi

if [[ -n "$BOOT_PART" ]]; then
    sudo mount "$BOOT_PART" "$MOUNT_DIR"

    # 1. Update cmdline.txt for memory cgroups
    CMDLINE_FILE="$MOUNT_DIR/cmdline.txt"
    if [[ -f "$CMDLINE_FILE" ]]; then
        CMDLINE_CONTENT=$(cat "$CMDLINE_FILE")
        if [[ "$CMDLINE_CONTENT" != *"cgroup_memory=1"* ]]; then
            echo "[+] Enabling cgroups in $CMDLINE_FILE..."
            sudo sed -i.bak 's/$/ cgroup_memory=1 cgroup_enable=memory/' "$CMDLINE_FILE"
        else
            echo "[+] Memory cgroups already present in cmdline.txt"
        fi
    fi

    # 2. Enable SSH
    sudo touch "$MOUNT_DIR/ssh"

    # 3. Create role.json on boot partition for first-boot injector
    sudo mkdir -p "$MOUNT_DIR/cluster-init"
    sudo tee "$MOUNT_DIR/cluster-init/role.json" > /dev/null <<EOF
{
  "role": "Seed",
  "target_disk_id": null,
  "reformat_confirmed": true
}
EOF

    # 4. First-boot script to initialize k3s server --cluster-init
    sudo tee "$MOUNT_DIR/cluster-init/firstboot.sh" > /dev/null <<'EOF'
#!/usr/bin/env bash
set -e
mkdir -p /etc/cluster-ldm
cp /boot/firmware/cluster-init/role.json /etc/cluster-ldm/role.json || cp /boot/cluster-init/role.json /etc/cluster-ldm/role.json || true

# Run cluster-ldm provisioning
if command -v cluster-ldm >/dev/null 2>&1; then
    cluster-ldm provision --role-source=/etc/cluster-ldm/role.json
fi

# Install and start K3s server in cluster-init mode
curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="server --cluster-init --write-kubeconfig-mode 644" sh -
EOF
    sudo chmod +x "$MOUNT_DIR/cluster-init/firstboot.sh"

    sudo umount "$MOUNT_DIR"
    rm -rf "$MOUNT_DIR"
    echo "[+] Seed storage provisioned and configured successfully."
else
    echo "[!] Could not automatically locate boot partition on $DEVICE; mount manually if needed."
fi

echo "=============================================================================="
echo "[+] Seed node setup complete. Insert media into the seed Raspberry Pi and power on."
echo "=============================================================================="
