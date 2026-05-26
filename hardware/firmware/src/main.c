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

// ===== Pin assignments (see ../SCHEMATIC.md) =====

#define PIN_ENC_A      14
#define PIN_ENC_B      15

#define PIN_BTN_1      10  // play / pause toggle
#define PIN_BTN_2      11  // CUE (Pioneer state machine)
#define PIN_BTN_3      12  // nudge − (while held)
#define PIN_BTN_4      13  // nudge + (while held)

#define PIN_MUX_S0      6
#define PIN_MUX_S1      7
#define PIN_MUX_S2      8
#define PIN_MUX_OUT    26  // ADC0
#define ADC_INPUT       0  // ADC channel for GP26

// LED feedback. Host sends note_on 40 → LED on, note_off 40 → LED off,
// mirroring Deck A's playing state.
#define PIN_LED_PLAY_A 18

// ===== MIDI mapping =====

#define MIDI_CHANNEL    0   // MIDI channel 1
#define ENCODER_CC     16

#define MUX_CHANNELS    5
static const uint8_t MUX_CC[MUX_CHANNELS] = {
    4,   // Y0 — EQ low
    9,   // Y1 — EQ mid (no host engine handler yet)
    3,   // Y2 — EQ high
    2,   // Y3 — volume
    1,   // Y4 — pitch
};

#define NUM_BUTTONS 4
static const uint8_t BUTTON_PIN[NUM_BUTTONS] = {
    PIN_BTN_1, PIN_BTN_2, PIN_BTN_3, PIN_BTN_4,
};
static const uint8_t BUTTON_NOTE[NUM_BUTTONS] = { 40, 41, 42, 43 };

#define DEBOUNCE_MS         5
#define SCAN_INTERVAL_MS    5
#define MUX_SETTLE_US       5

// 7-bit deadband for the mux scan. Required change before a CC is emitted.
// Silences ±1-LSB ADC jitter on properly-wired pots AND keeps
// unconnected/floating mux inputs quiet during prototype assembly. Real
// pot movement easily exceeds 2 steps per scan tick.
#define MUX_DEADBAND        2

// ===== State =====

// Quadrature accumulator. Updated on every poll; drained at MIDI send time.
static volatile int32_t encoder_accum = 0;
static uint8_t encoder_last_state = 0;

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

static bool button_state[NUM_BUTTONS] = { false, false, false, false };
static absolute_time_t button_debounce_until[NUM_BUTTONS];

// Last reported 7-bit value per mux channel. 0xFF forces an initial send.
static uint8_t mux_last_value[MUX_CHANNELS] = { 0xFF, 0xFF, 0xFF, 0xFF, 0xFF };

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

// ===== Encoder =====

static inline void encoder_poll(void) {
    uint8_t a = (uint8_t) gpio_get(PIN_ENC_A);
    uint8_t b = (uint8_t) gpio_get(PIN_ENC_B);
    uint8_t state = (uint8_t) ((a << 1) | b);
    encoder_accum += QDEC_TABLE[(encoder_last_state << 2) | state];
    encoder_last_state = state;
}

static void send_encoder_delta(void) {
    int32_t delta = encoder_accum;
    if (delta == 0) {
        return;
    }
    encoder_accum -= delta;  // drain

    // Pioneer-style relative-CC encoding: value = 64 + signed delta.
    // Clamp to 7-bit signed range so we always have a valid CC value.
    if (delta > 63)  delta = 63;
    if (delta < -63) delta = -63;
    midi_send_cc(MIDI_CHANNEL, ENCODER_CC, (uint8_t) (64 + delta));
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

static void scan_mux(void) {
    for (uint8_t ch = 0; ch < MUX_CHANNELS; ch++) {
        mux_select(ch);
        sleep_us(MUX_SETTLE_US);
        uint16_t raw = adc_read();          // 0..4095 (12-bit)
        uint8_t value = (uint8_t) (raw >> 5); // 0..127 (7-bit)

#if MUX_DEBUG
        // Stream the full mux→ADC path (deadband ignored) at ~20 Hz per
        // channel so we can watch the pots/faders move.
        static uint32_t dbg = 0;
        if (ch == 0) {
            dbg++;
        }
        if (dbg % 10 == 0) {
            midi_send_cc(MIDI_CHANNEL, MUX_CC[ch], value);
        }
        continue;
#else
        // Force the first reading through (last_value starts at 0xFF) so
        // the host gets an initial position. After that, gate small
        // changes via the deadband.
        if (mux_last_value[ch] != 0xFF) {
            int diff = (int) value - (int) mux_last_value[ch];
            if (diff > -MUX_DEADBAND && diff < MUX_DEADBAND) {
                continue;
            }
        }
        mux_last_value[ch] = value;
        midi_send_cc(MIDI_CHANNEL, MUX_CC[ch], value);
#endif
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
    if (note == 40) {
        gpio_put(PIN_LED_PLAY_A, on);
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

static void init_gpio(void) {
    gpio_init(PIN_ENC_A);
    gpio_set_dir(PIN_ENC_A, GPIO_IN);
    gpio_pull_up(PIN_ENC_A);

    gpio_init(PIN_ENC_B);
    gpio_set_dir(PIN_ENC_B, GPIO_IN);
    gpio_pull_up(PIN_ENC_B);

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

    gpio_init(PIN_LED_PLAY_A);
    gpio_set_dir(PIN_LED_PLAY_A, GPIO_OUT);
    gpio_put(PIN_LED_PLAY_A, 0);

    // Seed the encoder's "previous" state from current pins to avoid a
    // spurious count on boot.
    uint8_t a = (uint8_t) gpio_get(PIN_ENC_A);
    uint8_t b = (uint8_t) gpio_get(PIN_ENC_B);
    encoder_last_state = (uint8_t) ((a << 1) | b);
}

static void init_adc(void) {
    adc_init();
#if MUX_DEBUG
    // Enable the on-chip temperature sensor (ADC channel 4) so the debug
    // build can prove the ADC core is converting at all, and init the
    // other analog-capable GPIOs so we can probe them directly.
    adc_set_temp_sensor_enabled(true);
    adc_gpio_init(27);
    adc_gpio_init(28);
#endif
    adc_gpio_init(PIN_MUX_OUT);
    adc_select_input(ADC_INPUT);
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
