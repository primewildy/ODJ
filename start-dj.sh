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
# Build first (blocking) so we don't start the 10 s stream-discovery poll
# while cargo is still compiling. A cold build is ~30 s; the poll would
# always time out before the binary even started.
echo "[dj] building (this is fast if nothing changed)..."
if ! cargo build --release --quiet; then
    echo "[dj] build failed, bailing"
    exit 1
fi
target/release/dj \
    --cue-device "KT USB Audio" \
    --midi "ODJ" \
    "$@" &
DJ_PID=$!

# --- Step 3: wait for "DJ Master" + "DJ Cue" streams, then force routes ---
# The app names each stream via PIPEWIRE_PROPS so we identify by name
# rather than ordering — more reliable.
echo "[dj] waiting for audio streams to appear..."
MASTER_ID=""
CUE_ID=""
find_sink_input() {  # arg: application.name to match
    pactl list sink-inputs 2>/dev/null | awk -v want="$1" '
        /^Sink Input #/ { id = $3; gsub("#", "", id); curr = id }
        $0 ~ "application\\.name = \"" want "\"" { print curr; exit }
    '
}
for _ in $(seq 1 50); do
    MASTER_ID="$(find_sink_input "DJ Master")"
    CUE_ID="$(find_sink_input "DJ Cue")"
    if [[ -n "$MASTER_ID" && -n "$CUE_ID" ]]; then break; fi
    sleep 0.2
done

if [[ -n "$MASTER_ID" && -n "$CUE_ID" ]]; then
    MASTER_TARGET="${MONO_SINK:-$SPEAKER_SINK}"
    if [[ -n "$MASTER_TARGET" ]]; then
        echo "[dj] DJ Master (sink-input $MASTER_ID) → $MASTER_TARGET"
        pactl move-sink-input "$MASTER_ID" "$MASTER_TARGET" 2>/dev/null \
            || echo "[dj]   (move failed — check pavucontrol)"
    fi
    if [[ -n "$UGREEN_SINK" ]]; then
        echo "[dj] DJ Cue    (sink-input $CUE_ID) → $UGREEN_SINK"
        pactl move-sink-input "$CUE_ID" "$UGREEN_SINK" 2>/dev/null \
            || echo "[dj]   (move failed — check pavucontrol)"
    fi
else
    echo "[dj] DJ Master / DJ Cue streams didn't appear within ~10 s — check pavucontrol"
    echo "[dj]   master=$MASTER_ID  cue=$CUE_ID"
fi

wait "$DJ_PID"
