#!/usr/bin/env bash
# Launch the DJ app with the right cue/MIDI parameters and route master
# to the laptop speakers instead of the headphone DAC. Your day-to-day
# mono-summed-to-headphones routing is RESTORED on exit (Ctrl-C / quit),
# so this is safe to run and won't leave your audio in a weird state.
#
# What it does:
#   1. Finds your "mono-playback" loopback (from `pactl list sink-inputs`).
#      Saves its current target sink (typically the UGREEN headphones).
#   2. Moves the loopback's output to the laptop's built-in Speaker so the
#      master mix (which routes through Mono → loopback) goes to speakers.
#   3. Launches dj with --cue-device pointing at the UGREEN DAC and
#      --midi matching the ODJ Controller.
#   4. On exit (any reason), restores the loopback's target. Daily setup
#      is back to normal.
#
# If you don't have a mono-playback loopback (e.g. someone else running
# this), the routing tweak is silently skipped — the app still launches.
set -uo pipefail
cd "$(dirname "$0")"

# --- Find the mono-playback sink-input (the loopback's output stream). ---
MONO_PB_ID="$(pactl list sink-inputs 2>/dev/null | awk '
    /^Sink Input #/ { id = $3; gsub("#", "", id) }
    /node\.name = "mono-playback"/ { print id; exit }
')"

# --- Find the laptop Speaker sink (target for "main out"). ---
SPEAKER_SINK="$(pactl list short sinks 2>/dev/null \
    | awk -F'\t' '/__Speaker__/ { print $2; exit }')"

# --- Remember the loopback's current target so we can put it back. ---
ORIG_SINK=""
if [[ -n "$MONO_PB_ID" ]]; then
    ORIG_SINK="$(pactl list short sink-inputs 2>/dev/null \
        | awk -F'\t' -v id="$MONO_PB_ID" '$1 == id { print $2; exit }')"
fi

cleanup() {
    if [[ -n "${MONO_PB_ID:-}" && -n "${ORIG_SINK:-}" ]]; then
        echo "[dj] restoring mono-playback → sink $ORIG_SINK (day-to-day setup)"
        pactl move-sink-input "$MONO_PB_ID" "$ORIG_SINK" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if [[ -n "$MONO_PB_ID" && -n "$SPEAKER_SINK" && -n "$ORIG_SINK" ]]; then
    if [[ "$ORIG_SINK" != "$SPEAKER_SINK" ]]; then
        echo "[dj] mono-playback (input $MONO_PB_ID): $ORIG_SINK → $SPEAKER_SINK"
        pactl move-sink-input "$MONO_PB_ID" "$SPEAKER_SINK"
    else
        echo "[dj] mono-playback already on Speaker — no routing change needed"
    fi
else
    echo "[dj] mono-playback or Speaker sink not detected — skipping routing tweak"
fi

# --- Launch the app. ---
cargo run --release -- \
    --cue-device "KT USB Audio" \
    --midi "ODJ" \
    "$@"
