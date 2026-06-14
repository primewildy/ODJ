// ODJ controller firmware — Pico SDK, TinyUSB MIDI device.
//
// Polled architecture:
//  - Encoder A/B are polled at the main-loop rate (~MHz with no other
//    work) via a 4-state Gray-code lookup table. Plenty fast for human
//    spin speeds on a 600 P/R encoder (max ~12 kHz transitions).
//  - Buttons are scanned every SCAN_INTERVAL_MS with a per-button
//    debounce window.
//  - 5 analog inputs are scanned through a 74HC4051 every
//    SCAN_INTERVAL_MS, one channel per pass with a brief mux+ADC
//    settling delay.
//  - Encoder accumulator is sent as relative-CC each scan tick.
//
// MIDI mapping is hardcoded to mirror the LPD8 PROG-1 default that the
// host app's src/midi.rs already handles — buttons emit notes 40-43
// (Deck A play/cue/nudge-down/nudge-up), pots/faders emit CC 1-4 + 9
// (pitch/vol/high/low/mid), encoder emits CC 16. The host app needs a
// CC 16 handler added for the jog — that's a host-side change.

#include <stdint.h>
#include <stdbool.h>
#include <string.h>

#include "bsp/board_api.h"
#include "hardware/adc.h"
#include "hardware/gpio.h"
#include "pico/bootrom.h"
#include "pico/stdlib.h"
#include "tusb.h"

// ===== Pin assignments (see hardware/pcb/odj_controller.py) =====
//
// BOTH decks now wired. Deck A is the LEFT half, Deck B is the RIGHT
// half (matching the case_half.scad asymmetry: LEFT = USB hole, RIGHT
// = heat-set inserts at the seam).

// Encoders — 4 pins, 2 per deck.
#define PIN_ENC_A_A    14  // Deck A encoder A phase
#define PIN_ENC_A_B    15  // Deck A encoder B phase
#define PIN_ENC_B_A    16  // Deck B encoder A phase
#define PIN_ENC_B_B    17  // Deck B encoder B phase

// Buttons — 8 total, 4 per deck (PLAY, CUE, 🎧-CUE/HPCUE, SYNC).
#define PIN_BTN_A_PLAY   10  // note 40
#define PIN_BTN_A_CUE    11  // note 41
#define PIN_BTN_A_HPCUE  12  // note 44
#define PIN_BTN_A_SYNC    2  // note 46
#define PIN_BTN_B_PLAY   13  // note 36
#define PIN_BTN_B_CUE    19  // note 37
#define PIN_BTN_B_HPCUE  20  // note 45
#define PIN_BTN_B_SYNC    3  // note 47

// Mux — shared select lines, separate output pins on different ADC channels.
#define PIN_MUX_S0        6
#define PIN_MUX_S1        7
#define PIN_MUX_S2        8
#define PIN_MUX_A_OUT    26  // GP26 / ADC0 — Deck A mux
#define PIN_MUX_B_OUT    27  // GP27 / ADC1 — Deck B mux
#define ADC_INPUT_A       0
#define ADC_INPUT_B       1

// LEDs — host emits note_on/off and we mirror onto the relevant pin.
//   note 40 → Deck A PLAY      (lit while Deck A is playing)
//   note 36 → Deck B PLAY
//   note 44 → Deck A 🎧-CUE     (lit while Deck A is on the cue bus)
//   note 45 → Deck B 🎧-CUE
#define PIN_LED_A_PLAY    18
#define PIN_LED_A_HPCUE   22
#define PIN_LED_B_PLAY    21
#define PIN_LED_B_HPCUE    9

// ===== MIDI mapping =====

#define MIDI_CHANNEL    0   // MIDI channel 1
// Jog encoders use Pioneer relative-CC (value = 64 + signed delta).
#define ENCODER_CC_A   16   // Deck A jog
#define ENCODER_CC_B   17   // Deck B jog (host needs a CC 17 handler)

// Mux channel → CC mapping mirrors src/midi.rs.
//   Deck A: pitch=1, gain=2, high=3, low=4, mid=9
//   Deck B: pitch=5, gain=6, high=7, low=8, mid=10
// On THIS controller the physical HIGH and MID pots ended up on Y1/Y2
// (swapped vs the "natural" channel order); the CC assignments below
// account for that so no rewiring is needed.
#define MUX_CHANNELS    5
static const uint8_t MUX_CC_A[MUX_CHANNELS] = {
    4,   // Y0 — Deck A EQ low
    9,   // Y1 — physical MID pot wired here  → Deck A EQ mid
    3,   // Y2 — physical HIGH pot wired here → Deck A EQ high
    2,   // Y3 — Deck A volume
    1,   // Y4 — Deck A pitch
};
static const uint8_t MUX_CC_B[MUX_CHANNELS] = {
    8,   // Y0 — Deck B EQ low
    7,   // Y1 — Deck B EQ high
    10,  // Y2 — Deck B EQ mid
    6,   // Y3 — Deck B volume
    5,   // Y4 — Deck B pitch
};

#define NUM_BUTTONS 8
static const uint8_t BUTTON_PIN[NUM_BUTTONS] = {
    PIN_BTN_A_PLAY, PIN_BTN_A_CUE, PIN_BTN_A_HPCUE, PIN_BTN_A_SYNC,
    PIN_BTN_B_PLAY, PIN_BTN_B_CUE, PIN_BTN_B_HPCUE, PIN_BTN_B_SYNC,
};
static const uint8_t BUTTON_NOTE[NUM_BUTTONS] = {
    40, 41, 44, 46,   // Deck A: PLAY, CUE, HPCUE, SYNC
    36, 37, 45, 47,   // Deck B: PLAY, CUE, HPCUE, SYNC
};

#define DEBOUNCE_MS         5
#define SCAN_INTERVAL_MS    5
#define MUX_SETTLE_US       5

// 7-bit deadband for the mux scan. Required change before a CC is emitted.
// Silences ±1-LSB ADC jitter on properly-wired pots AND keeps
// unconnected/floating mux inputs quiet during prototype assembly. Real
// pot movement easily exceeds 2 steps per scan tick.
#define MUX_DEADBAND        2

// ===== State =====

// Quadrature accumulator per deck. Updated every poll; drained at MIDI send.
static volatile int32_t encoder_accum_a = 0;
static volatile int32_t encoder_accum_b = 0;
static uint8_t encoder_last_state_a = 0;
static uint8_t encoder_last_state_b = 0;

// 4-state Gray-code transition table.
// Index: (prev_AB << 2) | curr_AB. Value: +1 / 0 / -1.
// Invalid transitions (two bits flipped at once) return 0.
// Sign convention matches the user's "spin → forward feels forward" — flip
// every non-zero entry to invert the direction.
static const int8_t QDEC_TABLE[16] = {
    0, +1, -1,  0,
   -1,  0,  0, +1,
   +1,  0,  0, -1,
    0, -1, +1,  0,
};

static bool button_state[NUM_BUTTONS] = {
    false, false, false, false, false, false, false, false,
};
static absolute_time_t button_debounce_until[NUM_BUTTONS];

// Last reported 7-bit value per mux channel, per deck. 0xFF forces initial send.
static uint8_t mux_last_a[MUX_CHANNELS] = { 0xFF, 0xFF, 0xFF, 0xFF, 0xFF };
static uint8_t mux_last_b[MUX_CHANNELS] = { 0xFF, 0xFF, 0xFF, 0xFF, 0xFF };

// SysEx accumulator — used to receive control messages from the host (e.g.,
// "reboot to BOOTSEL"). USB MIDI packs SysEx into 4-byte packets with CIN
// nibbles 0x4-0x7, so we have to reassemble the inner byte stream.
#define SYSEX_BUF_MAX 32
static uint8_t sysex_buf[SYSEX_BUF_MAX];
static uint8_t sysex_len = 0;

// Magic SysEx: F0 7D 'O' 'D' 'J' 00 F7 → reboot to BOOTSEL.
// Manufacturer ID 0x7D is reserved for non-commercial / educational use.
static const uint8_t MAGIC_REBOOT[] = {
    0xF0, 0x7D, 'O', 'D', 'J', 0x00, 0xF7,
};

// ===== MIDI send helpers =====

static inline void midi_send_note(uint8_t channel, uint8_t note, uint8_t vel, bool on) {
    uint8_t packet[4] = {
        (uint8_t) (on ? 0x09 : 0x08),                    // CIN: note-on / off, cable 0
        (uint8_t) ((on ? 0x90 : 0x80) | (channel & 0x0F)),
        note,
        vel,
    };
    tud_midi_packet_write(packet);
}

static inline void midi_send_cc(uint8_t channel, uint8_t cc, uint8_t value) {
    uint8_t packet[4] = {
        0x0B,                                            // CIN: control change, cable 0
        (uint8_t) (0xB0 | (channel & 0x0F)),
        cc,
        value,
    };
    tud_midi_packet_write(packet);
}

// ===== Encoders =====

static inline void encoder_poll(void) {
    // Deck A
    {
        uint8_t a = (uint8_t) gpio_get(PIN_ENC_A_A);
        uint8_t b = (uint8_t) gpio_get(PIN_ENC_A_B);
        uint8_t state = (uint8_t) ((a << 1) | b);
        encoder_accum_a += QDEC_TABLE[(encoder_last_state_a << 2) | state];
        encoder_last_state_a = state;
    }
    // Deck B
    {
        uint8_t a = (uint8_t) gpio_get(PIN_ENC_B_A);
        uint8_t b = (uint8_t) gpio_get(PIN_ENC_B_B);
        uint8_t state = (uint8_t) ((a << 1) | b);
        encoder_accum_b += QDEC_TABLE[(encoder_last_state_b << 2) | state];
        encoder_last_state_b = state;
    }
}

static inline void send_one_encoder(volatile int32_t* accum, uint8_t cc) {
    int32_t delta = *accum;
    if (delta == 0) return;
    // Clamp first so the residual (= raw - sent) carries into the
    // next tick. Otherwise a very fast spin (≥64 ticks per 5 ms scan)
    // would clamp the value sent but still subtract the *full* raw
    // delta, silently dropping the overflow.
    if (delta > 63)  delta = 63;
    if (delta < -63) delta = -63;
    *accum -= delta;
    // Pioneer-style relative-CC: value = 64 + signed delta.
    midi_send_cc(MIDI_CHANNEL, cc, (uint8_t)(64 + delta));
}

static void send_encoder_delta(void) {
    send_one_encoder(&encoder_accum_a, ENCODER_CC_A);
    send_one_encoder(&encoder_accum_b, ENCODER_CC_B);
}

// ===== Buttons =====

static void scan_buttons(absolute_time_t now) {
    for (int i = 0; i < NUM_BUTTONS; i++) {
        // Internal pull-ups → idle HIGH → pressed = active LOW.
        bool pressed = !gpio_get(BUTTON_PIN[i]);
        if (pressed == button_state[i]) {
            continue;
        }
        if (absolute_time_diff_us(button_debounce_until[i], now) < 0) {
            continue;  // still inside the debounce window
        }
        button_state[i] = pressed;
        button_debounce_until[i] = delayed_by_ms(now, DEBOUNCE_MS);
        midi_send_note(MIDI_CHANNEL, BUTTON_NOTE[i], pressed ? 127 : 0, pressed);
    }
}

// ===== Multiplexer + ADC =====

static inline void mux_select(uint8_t channel) {
    gpio_put(PIN_MUX_S0, (channel >> 0) & 1u);
    gpio_put(PIN_MUX_S1, (channel >> 1) & 1u);
    gpio_put(PIN_MUX_S2, (channel >> 2) & 1u);
}

// Set to 1 to stream every mux channel every scan, ignoring the
// deadband — for bringing up / debugging the analog wiring. Revert to 0
// for normal use.
#define MUX_DEBUG 0

static inline void scan_mux_one(uint8_t adc_input, uint8_t ch, uint8_t cc,
                                uint8_t* last) {
    adc_select_input(adc_input);
    uint16_t raw = adc_read();
    uint8_t value = (uint8_t)(raw >> 5);  // 12-bit → 7-bit
    if (*last != 0xFF) {
        int diff = (int)value - (int)*last;
        if (diff > -MUX_DEADBAND && diff < MUX_DEADBAND) return;
    }
    *last = value;
    midi_send_cc(MIDI_CHANNEL, cc, value);
}

static void scan_mux(void) {
    // One select op drives BOTH muxes (they share S0/S1/S2). Then read
    // each ADC in turn; the second adc_select_input adds ~2 µs which is
    // longer than the mux settle, so no extra delay needed.
    for (uint8_t ch = 0; ch < MUX_CHANNELS; ch++) {
        mux_select(ch);
        sleep_us(MUX_SETTLE_US);
        scan_mux_one(ADC_INPUT_A, ch, MUX_CC_A[ch], &mux_last_a[ch]);
        scan_mux_one(ADC_INPUT_B, ch, MUX_CC_B[ch], &mux_last_b[ch]);
    }
}

// ===== MIDI input (SysEx → reboot) =====

static void handle_sysex(const uint8_t* buf, uint8_t len) {
    if (len == sizeof(MAGIC_REBOOT) &&
        memcmp(buf, MAGIC_REBOOT, sizeof(MAGIC_REBOOT)) == 0) {
        // Reboot straight into the bootloader (BOOTSEL mass storage mode).
        // First arg = LED mask for activity (0 = use default), second = disable interfaces.
        reset_usb_boot(0, 0);
    }
}

static void handle_note(uint8_t note, bool on) {
    // Mirror host's deck-state notes back onto button LEDs.
    switch (note) {
        case 40: gpio_put(PIN_LED_A_PLAY,  on); break;  // Deck A playing
        case 44: gpio_put(PIN_LED_A_HPCUE, on); break;  // Deck A 🎧-cue active
        case 36: gpio_put(PIN_LED_B_PLAY,  on); break;  // Deck B playing
        case 45: gpio_put(PIN_LED_B_HPCUE, on); break;  // Deck B 🎧-cue active
        default: break;
    }
}

static void process_midi_packet(const uint8_t* packet) {
    uint8_t cin = packet[0] & 0x0F;

    // Note-on (0x9) / note-off (0x8) from host → button LEDs.
    if (cin == 0x9) {
        uint8_t note = packet[2];
        uint8_t vel  = packet[3];
        handle_note(note, vel > 0);
        return;
    }
    if (cin == 0x8) {
        handle_note(packet[2], false);
        return;
    }

    // SysEx fragments — used for the reboot-to-BOOTSEL magic.
    uint8_t take = 0;
    switch (cin) {
        case 0x4: take = 3; break;  // start / continue
        case 0x5: take = 1; break;  // end with 1 byte
        case 0x6: take = 2; break;  // end with 2 bytes
        case 0x7: take = 3; break;  // end with 3 bytes
        default:  return;            // ignore other CINs (CC etc. are device→host only)
    }
    for (uint8_t i = 1; i <= take && sysex_len < SYSEX_BUF_MAX; i++) {
        sysex_buf[sysex_len++] = packet[i];
    }
    if (cin == 0x5 || cin == 0x6 || cin == 0x7) {
        handle_sysex(sysex_buf, sysex_len);
        sysex_len = 0;
    }
}

static void drain_midi_input(void) {
    while (tud_midi_available()) {
        uint8_t packet[4];
        if (!tud_midi_packet_read(packet)) {
            break;
        }
        process_midi_packet(packet);
    }
}

// ===== Setup =====

static const uint8_t ENC_PINS[4] = {
    PIN_ENC_A_A, PIN_ENC_A_B, PIN_ENC_B_A, PIN_ENC_B_B,
};
static const uint8_t LED_PINS[4] = {
    PIN_LED_A_PLAY, PIN_LED_A_HPCUE, PIN_LED_B_PLAY, PIN_LED_B_HPCUE,
};

static void init_gpio(void) {
    for (int i = 0; i < 4; i++) {
        gpio_init(ENC_PINS[i]);
        gpio_set_dir(ENC_PINS[i], GPIO_IN);
        gpio_pull_up(ENC_PINS[i]);
    }

    absolute_time_t boot_zero = make_timeout_time_ms(0);
    for (int i = 0; i < NUM_BUTTONS; i++) {
        gpio_init(BUTTON_PIN[i]);
        gpio_set_dir(BUTTON_PIN[i], GPIO_IN);
        gpio_pull_up(BUTTON_PIN[i]);
        button_debounce_until[i] = boot_zero;
    }

    gpio_init(PIN_MUX_S0); gpio_set_dir(PIN_MUX_S0, GPIO_OUT); gpio_put(PIN_MUX_S0, 0);
    gpio_init(PIN_MUX_S1); gpio_set_dir(PIN_MUX_S1, GPIO_OUT); gpio_put(PIN_MUX_S1, 0);
    gpio_init(PIN_MUX_S2); gpio_set_dir(PIN_MUX_S2, GPIO_OUT); gpio_put(PIN_MUX_S2, 0);

    for (int i = 0; i < 4; i++) {
        gpio_init(LED_PINS[i]);
        gpio_set_dir(LED_PINS[i], GPIO_OUT);
        gpio_put(LED_PINS[i], 0);
    }

    // Seed each encoder's "previous" state from current pins so we don't
    // emit a spurious count on boot.
    {
        uint8_t a = (uint8_t) gpio_get(PIN_ENC_A_A);
        uint8_t b = (uint8_t) gpio_get(PIN_ENC_A_B);
        encoder_last_state_a = (uint8_t)((a << 1) | b);
    }
    {
        uint8_t a = (uint8_t) gpio_get(PIN_ENC_B_A);
        uint8_t b = (uint8_t) gpio_get(PIN_ENC_B_B);
        encoder_last_state_b = (uint8_t)((a << 1) | b);
    }
}

static void init_adc(void) {
    adc_init();
    adc_gpio_init(PIN_MUX_A_OUT);
    adc_gpio_init(PIN_MUX_B_OUT);
    // scan_mux() re-selects per channel; this is just an initial pick.
    adc_select_input(ADC_INPUT_A);
}

int main(void) {
    board_init();
    tusb_init();
    init_gpio();
    init_adc();

    absolute_time_t next_scan = make_timeout_time_ms(SCAN_INTERVAL_MS);

    while (true) {
        tud_task();
        drain_midi_input();
        encoder_poll();

        if (time_reached(next_scan)) {
            absolute_time_t now = get_absolute_time();
            scan_buttons(now);
            scan_mux();
            send_encoder_delta();
            next_scan = delayed_by_ms(next_scan, SCAN_INTERVAL_MS);
        }
    }
}
