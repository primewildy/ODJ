# Pico C/C++ SDK build for the ODJ controller firmware.
#
# Pre-req: pico-sdk cloned, PICO_SDK_PATH env var set to its location.
# See ../README.md for setup steps.

cmake_minimum_required(VERSION 3.13)

# Pull in PICO SDK (must be done before project())
include($ENV{PICO_SDK_PATH}/external/pico_sdk_import.cmake)

project(odj_controller C CXX ASM)
set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)

pico_sdk_init()

add_executable(odj_controller
    src/main.c
    src/usb_descriptors.c
)

target_include_directories(odj_controller PRIVATE
    src
)

target_link_libraries(odj_controller
    pico_stdlib
    pico_unique_id
    hardware_adc
    hardware_gpio
    tinyusb_device
    tinyusb_board
)

# USB CDC stdio off — we use USB for MIDI, not console. UART stdio off too.
pico_enable_stdio_usb(odj_controller 0)
pico_enable_stdio_uart(odj_controller 0)

# Generate .uf2 + .bin + .hex outputs alongside the .elf
pico_add_extra_outputs(odj_controller)
