#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# PI-CLUSTER-CORE — Seed Board Bootstrap & Flash Script
# Flashes base Raspberry Pi OS image, injects cgroups into cmdline.txt,
# configures cluster-ldm role (Seed), and initializes K3s cluster.
# Includes automatic MicroSD / USB storage auto-discovery & interactive picker.
# ==============================================================================

DEVICE=""
HOSTNAME="pi-seed-01"
IMAGE_PATH=""
I_AM_SURE=false
LIST_ONLY=false

DISCOVERED_DEVICES=()
DISCOVERED_INFO=()

print_usage() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Options:"
    echo "  --device <dev>     Target block device (e.g. /dev/sdb, /dev/disk4)"
    echo "                     If omitted, the script automatically scans for connected SD cards"
    echo "  --list, -l         Scan and list all detected external SD cards/USB devices and exit"
    echo "  --hostname <name>  Seed node hostname (default: pi-seed-01)"
    echo "  --image <path>     Path to Raspberry Pi OS ARM64 image (optional; preserves existing OS if omitted)"
    echo "  --i-am-sure        Bypasses interactive confirmation prompt"
    echo "  -h, --help         Show this help message"
    echo ""
    exit 0
}

discover_devices() {
    DISCOVERED_DEVICES=()
    DISCOVERED_INFO=()

    # macOS discovery via diskutil
    if command -v diskutil >/dev/null 2>&1; then
        while IFS= read -r dev; do
            [[ -n "$dev" ]] || continue
            local info name size proto parts mounts

            info=$(diskutil info "$dev" 2>/dev/null || true)
            name=$(echo "$info" | awk -F: '/Device \/ Media Name:/ {gsub(/^[ \t]+/, "", $2); print $2; exit}')
            size=$(echo "$info" | awk -F: '/Disk Size:/ {gsub(/^[ \t]+/, "", $2); sub(/ \(.*/, "", $2); print $2; exit}')
            proto=$(echo "$info" | awk -F: '/Protocol:/ {gsub(/^[ \t]+/, "", $2); print $2; exit}')

            [[ -z "$name" ]] && name="Storage Device"
            [[ -z "$size" ]] && size="Unknown Size"
            [[ -z "$proto" ]] && proto="External"

            # Parse partition layout
            parts=$(diskutil list "$dev" 2>/dev/null | awk '/^[ ]+[1-9]:/ { 
                id = $NF
                sz = $(NF-2) " " $(NF-1)
                tname = ""
                for (i=2; i<=NF-3; i++) {
                    tname = (tname == "" ? $i : tname " " $i)
                }
                printf "%s: %s [%s], ", id, tname, sz
            }' | sed 's/, $//')
            [[ -z "$parts" ]] && parts="None or Unpartitioned"

            # Check mount points
            mounts=$(mount | grep "^$dev" | awk '{printf "%s on %s, ", $1, $3}' | sed 's/, $//' || true)
            [[ -z "$mounts" ]] && mounts="None"

            DISCOVERED_DEVICES+=("$dev")
            DISCOVERED_INFO+=("$dev|$size|$name|$proto|$parts|$mounts")
        done < <(diskutil list external physical 2>/dev/null | grep -E '^/dev/disk[0-9]+' | awk '{print $1}')
    fi

    # Linux discovery via lsblk / sysfs
    if [[ ${#DISCOVERED_DEVICES[@]} -eq 0 ]] && command -v lsblk >/dev/null 2>&1; then
        while IFS= read -r line; do
            local dev size type tran rm_flag model parts mounts
            dev=$(echo "$line" | awk '{print $1}')
            size=$(echo "$line" | awk '{print $2}')
            type=$(echo "$line" | awk '{print $3}')
            tran=$(echo "$line" | awk '{print $4}')
            rm_flag=$(echo "$line" | awk '{print $5}')
            model=$(echo "$line" | cut -d' ' -f6-)

            if [[ "$type" == "disk" ]] && ([[ "$tran" == "usb" ]] || [[ "$rm_flag" == "1" ]] || [[ "$dev" =~ mmcblk[0-9]+$ ]]); then
                # Exclude root/boot partitions
                if ! lsblk -n -o MOUNTPOINT "$dev" 2>/dev/null | grep -q -E '^/(boot|usr|etc)?$'; then
                    [[ -z "$model" ]] && model="Generic Storage"
                    [[ -z "$tran" ]] && tran="Removable"

                    parts=$(lsblk -n -o NAME,SIZE,FSTYPE,LABEL "$dev" 2>/dev/null | grep -v "^$(basename "$dev") " | awk '{printf "%s [%s %s %s], ", $1, $2, $3, $4}' | sed 's/, $//' || true)
                    [[ -z "$parts" ]] && parts="None or Unpartitioned"

                    mounts=$(lsblk -n -o MOUNTPOINT "$dev" 2>/dev/null | grep -v "^$" | paste -sd ", " - || true)
                    [[ -z "$mounts" ]] && mounts="None"

                    DISCOVERED_DEVICES+=("$dev")
                    DISCOVERED_INFO+=("$dev|$size|$model|$tran|$parts|$mounts")
                fi
            fi
        done < <(lsblk -d -n -p -o NAME,SIZE,TYPE,TRAN,RM,MODEL 2>/dev/null || true)
    fi

    # Fallback to /sys/block on Linux if lsblk is absent
    if [[ ${#DISCOVERED_DEVICES[@]} -eq 0 && -d /sys/block ]]; then
        for sysdev in /sys/block/sd* /sys/block/mmcblk*; do
            [[ -e "$sysdev" ]] || continue
            local dev_name dev_path is_removable size_sectors size_gb model
            dev_name=$(basename "$sysdev")
            dev_path="/dev/$dev_name"
            [[ -b "$dev_path" ]] || continue

            is_removable=0
            if [[ -f "$sysdev/removable" ]] && [[ "$(cat "$sysdev/removable" 2>/dev/null)" == "1" ]]; then
                is_removable=1
            elif [[ "$dev_name" =~ mmcblk ]]; then
                is_removable=1
            elif readlink -f "$sysdev" 2>/dev/null | grep -q "usb"; then
                is_removable=1
            fi

            if [[ "$is_removable" -eq 1 ]]; then
                size_sectors=$(cat "$sysdev/size" 2>/dev/null || echo 0)
                size_gb=$(awk "BEGIN {printf \"%.1f GB\", $size_sectors * 512 / 1000000000}")
                model=$(cat "$sysdev/device/model" 2>/dev/null || echo "Generic Storage")
                DISCOVERED_DEVICES+=("$dev_path")
                DISCOVERED_INFO+=("$dev_path|$size_gb|$model|Removable|Unknown|Unknown")
            fi
        done
    fi
}

select_device_interactive() {
    while true; do
        echo "[+] Auto-discovering MicroSD cards and removable storage devices..."
        discover_devices
        local count=${#DISCOVERED_DEVICES[@]}

        if [[ $count -eq 0 ]]; then
            echo ""
            echo "[-] No removable storage devices (SD cards/USB readers) detected."
            echo "    - Make sure your MicroSD card / USB adapter is firmly connected."
            echo "    - You can retry the scan [r], enter the device path manually, or quit [q]."
            echo ""
            read -r -p "Enter target device path (e.g. /dev/sdX, /dev/disk4), [r] to rescan, or [q] to cancel: " choice
            choice=$(echo "$choice" | xargs)
            if [[ "$choice" == "q" || "$choice" == "Q" ]]; then
                echo "[-] Aborted by operator."
                exit 0
            elif [[ "$choice" == "r" || "$choice" == "R" || -z "$choice" ]]; then
                echo ""
                continue
            elif [[ -b "$choice" || -c "$choice" ]]; then
                DEVICE="$choice"
                return 0
            else
                echo "[-] ERROR: Device '$choice' does not exist or is not a block/character device."
                echo ""
                continue
            fi
        fi

        echo ""
        echo "=============================================================================="
        echo " DISCOVERED STORAGE DEVICES ($count found)"
        echo "=============================================================================="
        for i in "${!DISCOVERED_DEVICES[@]}"; do
            IFS="|" read -r c_dev c_size c_name c_proto c_parts c_mounts <<< "${DISCOVERED_INFO[$i]}"
            echo "  [$((i+1))] $c_dev"
            echo "      Capacity:   $c_size"
            echo "      Media Name: $c_name ($c_proto)"
            echo "      Partitions: $c_parts"
            echo "      Mounted:    $c_mounts"
            echo ""
        done
        echo "=============================================================================="

        if [[ $count -eq 1 ]]; then
            read -r -p "Select device [1] (${DISCOVERED_DEVICES[0]}), enter custom path, [r] to rescan, or [q] to cancel [Default: 1]: " choice
            choice=$(echo "$choice" | xargs)
            if [[ -z "$choice" || "$choice" == "1" ]]; then
                DEVICE="${DISCOVERED_DEVICES[0]}"
                return 0
            fi
        else
            read -r -p "Select device [1-$count], enter custom path, [r] to rescan, or [q] to cancel: " choice
            choice=$(echo "$choice" | xargs)
        fi

        if [[ "$choice" == "q" || "$choice" == "Q" ]]; then
            echo "[-] Aborted by operator."
            exit 0
        elif [[ "$choice" == "r" || "$choice" == "R" ]]; then
            continue
        elif [[ "$choice" =~ ^[0-9]+$ ]] && (( choice >= 1 && choice <= count )); then
            DEVICE="${DISCOVERED_DEVICES[$((choice-1))]}"
            return 0
        elif [[ -b "$choice" || -c "$choice" ]]; then
            DEVICE="$choice"
            return 0
        else
            echo "[-] ERROR: Invalid selection '$choice'. Please select 1-$count, enter a valid device path, or [q] to quit."
            echo ""
        fi
    done
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
        --list|-l)
            LIST_ONLY=true
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

if [[ "$LIST_ONLY" == true ]]; then
    echo "[+] Auto-discovering MicroSD cards and removable storage devices..."
    discover_devices
    count=${#DISCOVERED_DEVICES[@]}
    if [[ $count -eq 0 ]]; then
        echo "[-] No removable storage devices (SD cards/USB readers) detected."
    else
        echo ""
        echo "=============================================================================="
        echo " DISCOVERED STORAGE DEVICES ($count found)"
        echo "=============================================================================="
        for i in "${!DISCOVERED_DEVICES[@]}"; do
            IFS="|" read -r c_dev c_size c_name c_proto c_parts c_mounts <<< "${DISCOVERED_INFO[$i]}"
            echo "  [$((i+1))] $c_dev"
            echo "      Capacity:   $c_size"
            echo "      Media Name: $c_name ($c_proto)"
            echo "      Partitions: $c_parts"
            echo "      Mounted:    $c_mounts"
            echo ""
        done
        echo "=============================================================================="
    fi
    exit 0
fi

if [[ -z "$DEVICE" ]]; then
    select_device_interactive
fi

if [[ ! -b "$DEVICE" && ! -c "$DEVICE" ]]; then
    echo "[-] ERROR: Device '$DEVICE' is not a valid block or character device."
    exit 1
fi

# Normalize raw and block devices for macOS/Linux compatibility
BLOCK_DEVICE="$DEVICE"
RAW_DEVICE="$DEVICE"

if [[ "$DEVICE" =~ ^/dev/disk([0-9]+)$ ]]; then
    disk_num="${BASH_REMATCH[1]}"
    BLOCK_DEVICE="/dev/disk${disk_num}"
    if [[ -c "/dev/rdisk${disk_num}" ]]; then
        RAW_DEVICE="/dev/rdisk${disk_num}"
    fi
elif [[ "$DEVICE" =~ ^/dev/rdisk([0-9]+)$ ]]; then
    disk_num="${BASH_REMATCH[1]}"
    BLOCK_DEVICE="/dev/disk${disk_num}"
    RAW_DEVICE="/dev/rdisk${disk_num}"
fi

echo "=============================================================================="
echo " PI-CLUSTER-CORE SEED NODE BOOTSTRAP"
echo "=============================================================================="
echo "Target Device:  $BLOCK_DEVICE (Raw: $RAW_DEVICE)"
echo "Seed Hostname:  $HOSTNAME"
echo ""

# Safety: Display resolved device info
if command -v lsblk >/dev/null 2>&1; then
    echo "Target Device Details:"
    lsblk -o NAME,SIZE,TYPE,MODEL,SERIAL,MOUNTPOINT "$BLOCK_DEVICE" || true
    echo ""
elif command -v diskutil >/dev/null 2>&1; then
    echo "Target Device Details:"
    diskutil info "$BLOCK_DEVICE" | grep -E "Device / Media Name|Disk Size|Device Identifier|Protocol" || true
    echo ""
fi

# Check currently mounted partitions
if mount | grep -q "$BLOCK_DEVICE"; then
    echo "[-] NOTE: Partitions on target device $BLOCK_DEVICE are currently mounted:"
    mount | grep "$BLOCK_DEVICE" || true
    echo ""
fi

if [[ "$I_AM_SURE" != true ]]; then
    echo "=============================================================================="
    echo " [!] CAUTION: THIS WILL COMPLETELY AND IRREVERSIBLY ERASE ALL DATA ON $BLOCK_DEVICE."
    echo "=============================================================================="
    read -r -p "Type 'FLASH-SEED' to confirm: " CONFIRMATION
    if [[ "$CONFIRMATION" != "FLASH-SEED" ]]; then
        echo "[-] Aborted by operator."
        exit 1
    fi
fi

if [[ -n "$IMAGE_PATH" ]]; then
    echo "[+] Unmounting partitions on $BLOCK_DEVICE prior to flashing..."
    if command -v diskutil >/dev/null 2>&1; then
        diskutil unmountDisk "$BLOCK_DEVICE" || true
    elif command -v umount >/dev/null 2>&1; then
        mount | grep "^$BLOCK_DEVICE" | awk '{print $1}' | xargs -r sudo umount || true
    fi

    echo "[+] Step 1: Flashing base OS to $BLOCK_DEVICE..."
    if command -v rpi-imager >/dev/null 2>&1; then
        echo "[+] Using rpi-imager CLI..."
        rpi-imager --cli "$IMAGE_PATH" "$BLOCK_DEVICE"
    else
        echo "[+] Writing image via dd to $RAW_DEVICE..."
        if [[ "$IMAGE_PATH" == *.xz ]]; then
            xzcat "$IMAGE_PATH" | sudo dd of="$RAW_DEVICE" bs=4M status=progress conv=fsync
        elif [[ "$IMAGE_PATH" == *.gz ]]; then
            zcat "$IMAGE_PATH" | sudo dd of="$RAW_DEVICE" bs=4M status=progress conv=fsync
        else
            sudo dd if="$IMAGE_PATH" of="$RAW_DEVICE" bs=4M status=progress conv=fsync
        fi
    fi
else
    echo "[+] Step 1: No image supplied (--image omitted); assuming base OS partition already written."
fi

# Locate and mount boot partition
echo "[+] Step 2: Ingesting boot configuration..."

if [[ -n "$IMAGE_PATH" ]]; then
    sleep 2
fi

# Detect boot partition
BOOT_PART=""
if [[ -b "${BLOCK_DEVICE}s1" || -c "${BLOCK_DEVICE}s1" ]]; then
    BOOT_PART="${BLOCK_DEVICE}s1"
elif [[ -b "${BLOCK_DEVICE}1" ]]; then
    BOOT_PART="${BLOCK_DEVICE}1"
elif [[ -b "${BLOCK_DEVICE}p1" ]]; then
    BOOT_PART="${BLOCK_DEVICE}p1"
fi

if [[ -z "$BOOT_PART" ]]; then
    if command -v diskutil >/dev/null 2>&1; then
        BOOT_PART=$(diskutil list "$BLOCK_DEVICE" 2>/dev/null | awk '/boot|FAT/ {print "/dev/" $NF}' | head -n1 || true)
    fi
fi

if [[ -n "$BOOT_PART" ]]; then
    echo "[+] Found boot partition: $BOOT_PART"

    MOUNT_DIR=""
    CLEANUP_TEMP_DIR=false

    # Check if already mounted
    EXISTING_MOUNT=""
    if command -v diskutil >/dev/null 2>&1; then
        EXISTING_MOUNT=$(diskutil info "$BOOT_PART" 2>/dev/null | awk -F: '/Mount Point:/ {gsub(/^[ \t]+/, "", $2); print $2}' || true)
    fi
    if [[ -z "$EXISTING_MOUNT" ]]; then
        EXISTING_MOUNT=$(mount | grep "^$BOOT_PART" | awk '{print $3}' | head -n 1 || true)
    fi

    if [[ -n "$EXISTING_MOUNT" && -d "$EXISTING_MOUNT" ]]; then
        echo "[+] Using active mount at $EXISTING_MOUNT"
        MOUNT_DIR="$EXISTING_MOUNT"
    else
        TEMP_DIR=$(mktemp -d /tmp/piseed-boot.XXXXXX)
        echo "[+] Mounting $BOOT_PART..."
        if command -v diskutil >/dev/null 2>&1; then
            diskutil mount "$BOOT_PART" >/dev/null 2>&1 || sudo mount -t msdos "$BOOT_PART" "$TEMP_DIR" || true
            MAC_MOUNT=$(diskutil info "$BOOT_PART" 2>/dev/null | awk -F: '/Mount Point:/ {gsub(/^[ \t]+/, "", $2); print $2}' || true)
            if [[ -n "$MAC_MOUNT" && -d "$MAC_MOUNT" ]]; then
                rmdir "$TEMP_DIR" 2>/dev/null || true
                MOUNT_DIR="$MAC_MOUNT"
            else
                MOUNT_DIR="$TEMP_DIR"
                CLEANUP_TEMP_DIR=true
            fi
        else
            sudo mount "$BOOT_PART" "$TEMP_DIR"
            MOUNT_DIR="$TEMP_DIR"
            CLEANUP_TEMP_DIR=true
        fi
    fi

    if [[ -d "$MOUNT_DIR" ]]; then
        # 1. Update cmdline.txt for memory cgroups
        CMDLINE_FILE="$MOUNT_DIR/cmdline.txt"
        if [[ -f "$CMDLINE_FILE" ]]; then
            CMDLINE_CONTENT=$(cat "$CMDLINE_FILE")
            if [[ "$CMDLINE_CONTENT" != *"cgroup_memory=1"* ]]; then
                echo "[+] Enabling cgroups in $CMDLINE_FILE..."
                sudo sed -i.bak 's/$/ cgroup_memory=1 cgroup_enable=memory/' "$CMDLINE_FILE" 2>/dev/null || sed -i '' 's/$/ cgroup_memory=1 cgroup_enable=memory/' "$CMDLINE_FILE"
            else
                echo "[+] Memory cgroups already present in cmdline.txt"
            fi
        else
            echo "[-] WARNING: cmdline.txt not found in $MOUNT_DIR"
        fi

        # 2. Enable SSH
        echo "[+] Enabling SSH..."
        sudo touch "$MOUNT_DIR/ssh" 2>/dev/null || touch "$MOUNT_DIR/ssh"

        # 3. Create role.json on boot partition for first-boot injector
        echo "[+] Writing cluster role configuration..."
        sudo mkdir -p "$MOUNT_DIR/cluster-init" 2>/dev/null || mkdir -p "$MOUNT_DIR/cluster-init"
        sudo tee "$MOUNT_DIR/cluster-init/role.json" > /dev/null <<EOF
{
  "role": "Seed",
  "target_disk_id": null,
  "reformat_confirmed": true
}
EOF

        # 4. First-boot script to initialize k3s server --cluster-init
        echo "[+] Writing first-boot initialization script..."
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
        sudo chmod +x "$MOUNT_DIR/cluster-init/firstboot.sh" 2>/dev/null || chmod +x "$MOUNT_DIR/cluster-init/firstboot.sh"

        sync

        if [[ "$CLEANUP_TEMP_DIR" == true ]]; then
            sudo umount "$MOUNT_DIR" 2>/dev/null || umount "$MOUNT_DIR" || true
            rm -rf "$MOUNT_DIR"
        fi

        echo "[+] Seed storage provisioned and configured successfully."
    else
        echo "[-] ERROR: Failed to access mount directory for $BOOT_PART"
    fi
else
    echo "[!] Could not automatically locate boot partition on $BLOCK_DEVICE; mount manually if needed."
fi

echo "=============================================================================="
echo "[+] Seed node setup complete. Insert media into the seed Raspberry Pi and power on."
echo "=============================================================================="

