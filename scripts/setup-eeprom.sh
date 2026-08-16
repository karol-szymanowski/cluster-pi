#!/usr/bin/env bash
set -euo pipefail

# ==============================================================================
# PI-CLUSTER-CORE — One-Time EEPROM Network-Boot Configuration Script
# Configures Raspberry Pi 4/5 bootloader EEPROM with BOOT_ORDER=0xf241
# (Network boot fallback / preference), verifies config, and extracts board serial.
# ==============================================================================

echo "=============================================================================="
echo " PI-CLUSTER-CORE EEPROM BOOTLOADER SETUP"
echo "=============================================================================="

# 1. Read Board Serial Number
SERIAL=""
if [[ -f /proc/cpuinfo ]]; then
    SERIAL=$(grep -i "Serial" /proc/cpuinfo | awk '{print $3}' || true)
fi

if [[ -z "$SERIAL" ]]; then
    echo "[-] WARNING: Could not read hardware serial from /proc/cpuinfo."
    SERIAL="unknown-serial"
fi

echo "[+] Detected Board Hardware Serial: $SERIAL"

# 2. Check for rpi-eeprom tools
if ! command -v rpi-eeprom-config >/dev/null 2>&1; then
    echo "[-] ERROR: 'rpi-eeprom-config' tool not found. Please run on a Raspberry Pi OS environment."
    exit 1
fi

# 3. Read current EEPROM configuration
CURRENT_CONFIG=$(rpi-eeprom-config)
echo "[+] Current Bootloader Config:"
echo "$CURRENT_CONFIG"
echo ""

# Desired boot order: 0xf241 (Try SD, NVMe, USB, then PXE network boot)
# On Pi 4/5:
# 1 = SD CARD
# 2 = NETWORK (PXE)
# 4 = USB MSD
# 6 = NVMe
# f = RESTART
DESIRED_BOOT_ORDER="0xf241"

if echo "$CURRENT_CONFIG" | grep -q "BOOT_ORDER=$DESIRED_BOOT_ORDER"; then
    echo "[+] EEPROM already configured with BOOT_ORDER=$DESIRED_BOOT_ORDER. No changes needed."
    exit 0
fi

echo "[+] Updating EEPROM bootloader config with BOOT_ORDER=$DESIRED_BOOT_ORDER..."
TMP_CONF=$(mktemp /tmp/bootloader-XXXXXX.conf)
echo "$CURRENT_CONFIG" | grep -v "^BOOT_ORDER=" > "$TMP_CONF"
echo "BOOT_ORDER=$DESIRED_BOOT_ORDER" >> "$TMP_CONF"

# Apply config update
sudo rpi-eeprom-config --apply "$TMP_CONF"
rm -f "$TMP_CONF"

# 4. Verify configuration by re-reading
UPDATED_CONFIG=$(rpi-eeprom-config)
if echo "$UPDATED_CONFIG" | grep -q "BOOT_ORDER=$DESIRED_BOOT_ORDER"; then
    echo "[+] SUCCESS: EEPROM verified with BOOT_ORDER=$DESIRED_BOOT_ORDER."
    echo "[+] Board ($SERIAL) is now configured for autonomous network boot."
    echo "[+] Audit Record: $(date -u +"%Y-%m-%dT%H:%M:%SZ") SERIAL=$SERIAL EEPROM_BOOT_ORDER=$DESIRED_BOOT_ORDER"
else
    echo "[-] ERROR: EEPROM verification failed. Read back did not match expected config."
    exit 1
fi
