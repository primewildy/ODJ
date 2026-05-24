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

#include "bsp/board_api.h"
#include "hardware/adc.h"
#include "hardware/gpio.h"
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

// ===== State =====

// Quadrature accumulator. Updated on every poll; drained at MIDI send time.
static volatile int32_t encoder_accum = 0;
static uint8_t encoder_last_state = 0;

// 4-state Gray-code transition table.
// Index: (prev_AB << 2) | curr_AB. Value: +1 / 0 / -1.
// Invalid transitions (two bits flipped at once) return 0.
static const int8_t QDEC_TABLE[16] = {
    0, -1, +1,  0,
   +1,  0,  0, -1,
   -1,  0,  0, +1,
    0, +1, -1,  0,
};

static bool button_state[NUM_BUTTONS] = { false, false, false, false };
static absolute_time_t button_debounce_until[NUM_BUTTONS];

// Last reported 7-bit value per mux channel. 0xFF forces an initial send.
static uint8_t mux_last_value[MUX_CHANNELS] = { 0xFF, 0xFF, 0xFF, 0xFF, 0xFF };

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

static void scan_mux(void) {
    for (uint8_t ch = 0; ch < MUX_CHANNELS; ch++) {
        mux_select(ch);
        sleep_us(MUX_SETTLE_US);
        uint16_t raw = adc_read();          // 0..4095 (12-bit)
        uint8_t value = (uint8_t) (raw >> 5); // 0..127 (7-bit)
        if (value == mux_last_value[ch]) {
            continue;
        }
        mux_last_value[ch] = value;
        midi_send_cc(MIDI_CHANNEL, MUX_CC[ch], value);
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

    // Seed the encoder's "previous" state from current pins to avoid a
    // spurious count on boot.
    uint8_t a = (uint8_t) gpio_get(PIN_ENC_A);
    uint8_t b = (uint8_t) gpio_get(PIN_ENC_B);
    encoder_last_state = (uint8_t) ((a << 1) | b);
}

static void init_adc(void) {
    adc_init();
    adc_gpio_init(PIN_MUX_OUT);
    adc_select_input(ADC_INPUT);
}

int main(void) {
    board_init();
    tud_init(BOARD_TUD_RHPORT);
    init_gpio();
    init_adc();

    absolute_time_t next_scan = make_timeout_time_ms(SCAN_INTERVAL_MS);

    while (true) {
        tud_task();
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
