#!/usr/bin/env bash
# Rebuild + flash the ODJ firmware to a running Pico without touching BOOTSEL.
#
# Requires the Pico already running our firmware (a one-time manual BOOTSEL
# flash to enable the SysEx-reboot hook). For a virgin Pico, copy
# firmware/build/odj_controller.uf2 to the RPI-RP2 drive manually.
#
# Usage:
#   ./hardware/flash.sh                  # build + flash
#   ./hardware/flash.sh --no-build       # just flash whatever's in build/
set -euo pipefail

BUILD=true
if [[ "${1:-}" == "--no-build" ]]; then
    BUILD=false
fi

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"
FW="$HERE/firmware/build/odj_controller.uf2"

# Build step (default).
if $BUILD; then
    if [[ ! -d "$HERE/firmware/build" ]]; then
        echo "no build dir yet — run cmake first; see hardware/README.md"
        exit 1
    fi
    : "${PICO_SDK_PATH:=$HOME/pico-sdk}"
    export PICO_SDK_PATH
    (cd "$HERE/firmware/build" && make -j"$(nproc)") > /dev/null
fi

if [[ ! -f "$FW" ]]; then
    echo "firmware not built: $FW"
    exit 1
fi

# Find the running ODJ Controller MIDI port and ask it to reboot into BOOTSEL.
PORT="$(amidi -l | awk '/ODJ Controller/ { print $2; exit }')"
if [[ -z "$PORT" ]]; then
    echo "ODJ Controller MIDI port not found. Either:"
    echo "  - The Pico isn't running our firmware (hold BOOTSEL, plug in,"
    echo "    copy $FW to RPI-RP2 drive)."
    echo "  - It's already in BOOTSEL: udisksctl mount -b /dev/sda1 && cp"
    echo "    $FW /run/media/\$USER/RPI-RP2/"
    exit 1
fi

echo "rebooting Pico to BOOTSEL via SysEx on $PORT..."
amidi -p "$PORT" -S "F07D4F444A00F7"

# Wait for the device to come back up in BOOTSEL. The kernel can take a
# moment to enumerate /dev/sda → /dev/sda1; udisksctl can also report
# "already mounted" if a previous run left a stale mount. Handle both.
echo -n "waiting for RPI-RP2 drive"
DRIVE=""
RP2_NODE="/dev/sda1"
for _ in $(seq 1 40); do
    sleep 0.3
    # Need both: a BOOTSEL USB device AND the kernel-visible partition.
    if ! lsusb | grep -q "2e8a:0003"; then
        echo -n "."
        continue
    fi
    if [[ ! -b "$RP2_NODE" ]]; then
        echo -n "."
        continue
    fi
    # Try udisksctl mount; if it returns success, parse the path. If it
    # says "already mounted", find the existing mountpoint via findmnt.
    OUT="$(udisksctl mount -b "$RP2_NODE" 2>&1 || true)"
    if [[ "$OUT" == *"at "* ]]; then
        DRIVE="${OUT##*at }"
    elif [[ "$OUT" == *"already mounted"* ]]; then
        DRIVE="$(findmnt -n -o TARGET "$RP2_NODE" 2>/dev/null || true)"
    fi
    if [[ -n "$DRIVE" && -d "$DRIVE" ]]; then
        break
    fi
    echo -n "."
done
echo

if [[ -z "$DRIVE" || ! -d "$DRIVE" ]]; then
    echo "Pico didn't come back as a BOOTSEL drive."
    exit 1
fi

echo "copying $(basename "$FW") to $DRIVE..."
cp "$FW" "$DRIVE/"
sync

# Pico auto-reboots into the firmware after the .uf2 lands; the mount
# disappears on its own. Wait briefly and verify.
sleep 3
if amidi -l | grep -q "ODJ Controller"; then
    echo "ok — ODJ Controller back on MIDI"
else
    echo "warning: ODJ Controller didn't reappear on MIDI; check 'aconnect -i -l'"
fi
