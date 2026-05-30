#!/usr/bin/env bash
# Launch the DJ app and force the right routing in PipeWire:
#   master → Mono virtual sink (your mono-summed loopback) → laptop Speakers
#   cue    → UGREEN headphone DAC
#
# We do the moves with `pactl move-sink-input` AFTER the app starts so it
# overrides whatever WirePlumber's auto-routing tries to do. Your day-to-day
# mono-playback target is restored on exit (Ctrl-C / quit / kill — any path).
# Safe to interrupt; won't leave your audio in a weird state.
set -uo pipefail
cd "$(dirname "$0")"

# --- Discover sinks ----------------------------------------------------
SPEAKER_SINK="$(pactl list short sinks 2>/dev/null \
    | awk -F'\t' '/__Speaker__/ { print $2; exit }')"
UGREEN_SINK="$(pactl list short sinks 2>/dev/null \
    | awk -F'\t' '/KT_USB_Audio|usb-KTMicro/ { print $2; exit }')"
# Your "Mono" virtual sink (the loopback's capture side, usually
# node.name "mono-capture"). Master goes here and gets mono-summed
# by the loopback before reaching the speakers.
MONO_SINK="$(pactl list short sinks 2>/dev/null \
    | awk -F'\t' '$2 ~ /(^|\.)mono(-capture)?$/ { print $2; exit }')"

# --- Find the mono-playback loopback's current target (to save+restore) ---
MONO_PB_ID="$(pactl list sink-inputs 2>/dev/null | awk '
    /^Sink Input #/        { id = $3; gsub("#", "", id) }
    /node\.name = "mono-playback"/ { print id; exit }
')"
ORIG_LOOPBACK_TARGET=""
if [[ -n "$MONO_PB_ID" ]]; then
    ORIG_LOOPBACK_TARGET="$(pactl list short sink-inputs 2>/dev/null \
        | awk -F'\t' -v id="$MONO_PB_ID" '$1 == id { print $2; exit }')"
fi

cleanup() {
    if [[ -n "${DJ_PID:-}" ]]; then
        kill "$DJ_PID" 2>/dev/null || true
        wait "$DJ_PID" 2>/dev/null || true
    fi
    if [[ -n "${MONO_PB_ID:-}" && -n "${ORIG_LOOPBACK_TARGET:-}" ]]; then
        echo "[dj] restoring mono-playback → $ORIG_LOOPBACK_TARGET (day-to-day setup)"
        pactl move-sink-input "$MONO_PB_ID" "$ORIG_LOOPBACK_TARGET" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

# --- Step 1: point the loopback at the Speaker so master → Mono → loopback → Speakers ---
if [[ -n "$MONO_PB_ID" && -n "$SPEAKER_SINK" && "$ORIG_LOOPBACK_TARGET" != "$SPEAKER_SINK" ]]; then
    echo "[dj] mono-playback: $ORIG_LOOPBACK_TARGET → $SPEAKER_SINK"
    pactl move-sink-input "$MONO_PB_ID" "$SPEAKER_SINK"
elif [[ -z "$MONO_PB_ID" ]]; then
    echo "[dj] no mono-playback loopback found — master will go straight to Speaker"
fi

# --- Step 2: launch dj ----------------------------------------------------
cargo run --release -- \
    --cue-device "KT USB Audio" \
    --midi "ODJ" \
    "$@" &
DJ_PID=$!

# --- Step 3: wait for both dj sink-inputs to register, then force routes ---
echo "[dj] waiting for audio streams to appear..."
DJ_IDS=""
for _ in $(seq 1 50); do
    DJ_IDS=$(pactl list sink-inputs 2>/dev/null | awk '
        /^Sink Input #/ { id = $3; gsub("#", "", id); curr = id }
        /application\.name = "PipeWire ALSA \[dj\]"/ { print curr }
    ' | sort -n)
    if [[ "$(echo "$DJ_IDS" | grep -c .)" -ge 2 ]]; then
        break
    fi
    sleep 0.2
done

if [[ "$(echo "$DJ_IDS" | grep -c .)" -ge 2 ]]; then
    MASTER_ID=$(echo "$DJ_IDS" | sed -n '1p')   # lower id = master (built first)
    CUE_ID=$(echo "$DJ_IDS"    | sed -n '2p')   # second  = cue
    MASTER_TARGET="${MONO_SINK:-$SPEAKER_SINK}"
    if [[ -n "$MASTER_TARGET" ]]; then
        echo "[dj] master (sink-input $MASTER_ID) → $MASTER_TARGET"
        pactl move-sink-input "$MASTER_ID" "$MASTER_TARGET" 2>/dev/null \
            || echo "[dj]   (move failed — check pavucontrol)"
    fi
    if [[ -n "$UGREEN_SINK" ]]; then
        echo "[dj] cue    (sink-input $CUE_ID) → $UGREEN_SINK"
        pactl move-sink-input "$CUE_ID" "$UGREEN_SINK" 2>/dev/null \
            || echo "[dj]   (move failed — check pavucontrol)"
    fi
else
    echo "[dj] dj streams didn't appear within ~10 s — routing left untouched. Check pavucontrol."
fi

wait "$DJ_PID"
