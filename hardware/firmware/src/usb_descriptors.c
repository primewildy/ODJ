// USB descriptors for the ODJ MIDI controller.
//
// Single configuration, single MIDI interface, one IN + one OUT bulk
// endpoint. Serial number is derived from the Pico's unique flash ID so
// two boards on the same machine don't collide.

#include <string.h>

#include "bsp/board_api.h"
#include "pico/unique_id.h"
#include "tusb.h"

// ----- Device descriptor -----

tusb_desc_device_t const desc_device = {
    .bLength            = sizeof(tusb_desc_device_t),
    .bDescriptorType    = TUSB_DESC_DEVICE,
    .bcdUSB             = 0x0200,

    .bDeviceClass       = 0x00,
    .bDeviceSubClass    = 0x00,
    .bDeviceProtocol    = 0x00,

    .bMaxPacketSize0    = CFG_TUD_ENDPOINT0_SIZE,

    // TinyUSB test VID. Fine for personal use; would need a real VID/PID
    // pair from USB-IF (or a sub-licensed pair from pid.codes) before any
    // public release.
    .idVendor           = 0xCafe,
    .idProduct          = 0x4001,
    .bcdDevice          = 0x0100,

    .iManufacturer      = 0x01,
    .iProduct           = 0x02,
    .iSerialNumber      = 0x03,

    .bNumConfigurations = 0x01,
};

uint8_t const* tud_descriptor_device_cb(void) {
    return (uint8_t const*) &desc_device;
}

// ----- Configuration descriptor -----

enum {
    ITF_NUM_MIDI = 0,
    ITF_NUM_MIDI_STREAMING,
    ITF_NUM_TOTAL,
};

// Endpoint number (cable 0 for both directions). High bit is set on IN
// endpoints by the TUD_MIDI_DESCRIPTOR macro.
#define EPNUM_MIDI 0x01

#define CONFIG_TOTAL_LEN (TUD_CONFIG_DESC_LEN + TUD_MIDI_DESC_LEN)

uint8_t const desc_configuration[] = {
    TUD_CONFIG_DESCRIPTOR(1, ITF_NUM_TOTAL, 0, CONFIG_TOTAL_LEN, 0, 100),

    // (interface number, string idx, ep OUT, ep IN, ep size)
    TUD_MIDI_DESCRIPTOR(ITF_NUM_MIDI, 0, EPNUM_MIDI, 0x80 | EPNUM_MIDI, 64),
};

uint8_t const* tud_descriptor_configuration_cb(uint8_t index) {
    (void) index;
    return desc_configuration;
}

// ----- String descriptors -----

static char const* string_desc_arr[] = {
    (const char[]) { 0x09, 0x04 },  // 0: supported language = English (0x0409)
    "ODJ",                           // 1: manufacturer
    "ODJ Controller",                // 2: product
    NULL,                            // 3: serial — filled in dynamically
};

static uint16_t _desc_str[32];

uint16_t const* tud_descriptor_string_cb(uint8_t index, uint16_t langid) {
    (void) langid;
    uint8_t chr_count;

    if (index == 0) {
        memcpy(&_desc_str[1], string_desc_arr[0], 2);
        chr_count = 1;
    } else if (index == 3) {
        // Serial: hex of the Pico's unique board ID.
        pico_unique_board_id_t board_id;
        pico_get_unique_board_id(&board_id);
        chr_count = 0;
        static const char hex[] = "0123456789ABCDEF";
        for (size_t i = 0; i < PICO_UNIQUE_BOARD_ID_SIZE_BYTES && chr_count < 30; i++) {
            uint8_t b = board_id.id[i];
            _desc_str[1 + chr_count++] = hex[(b >> 4) & 0xF];
            _desc_str[1 + chr_count++] = hex[b & 0xF];
        }
    } else {
        if (index >= sizeof(string_desc_arr) / sizeof(string_desc_arr[0])) {
            return NULL;
        }
        const char* str = string_desc_arr[index];
        if (str == NULL) return NULL;
        chr_count = (uint8_t) strlen(str);
        if (chr_count > 31) chr_count = 31;
        for (uint8_t i = 0; i < chr_count; i++) {
            _desc_str[1 + i] = (uint16_t) str[i];
        }
    }

    _desc_str[0] = (uint16_t) ((TUSB_DESC_STRING << 8) | (2 * chr_count + 2));
    return _desc_str;
}
