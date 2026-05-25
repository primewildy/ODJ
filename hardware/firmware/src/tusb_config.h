#ifndef _TUSB_CONFIG_H_
#define _TUSB_CONFIG_H_

#ifdef __cplusplus
extern "C" {
#endif

// Board / OS
#define CFG_TUSB_MCU            OPT_MCU_RP2040
#define CFG_TUSB_OS             OPT_OS_PICO
#define CFG_TUSB_DEBUG          0

// Device-only — we are a USB MIDI device, not a host.
#define CFG_TUD_ENABLED         1
#define CFG_TUH_ENABLED         0

// Root-hub port 0 is in device mode (Pico has only one USB port).
#define CFG_TUSB_RHPORT0_MODE   OPT_MODE_DEVICE

// EP0 size — 64 bytes is the standard for full-speed USB devices.
#define CFG_TUD_ENDPOINT0_SIZE  64

// Class drivers — MIDI only. Everything else off.
#define CFG_TUD_CDC             0
#define CFG_TUD_MSC             0
#define CFG_TUD_HID             0
#define CFG_TUD_MIDI            1
#define CFG_TUD_VENDOR          0

// MIDI ring buffer sizes. 128 each is generous for a controller that
// emits at most a couple of dozen messages per scan period.
#define CFG_TUD_MIDI_RX_BUFSIZE 128
#define CFG_TUD_MIDI_TX_BUFSIZE 128
#define CFG_TUD_MIDI_EP_BUFSIZE 64

#ifdef __cplusplus
}
#endif

#endif
