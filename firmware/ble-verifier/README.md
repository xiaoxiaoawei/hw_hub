# ESP32-S3 BLE verifier 0.1.0

This is a separate test-station firmware, not the bracelet firmware. It checks
one host-selected over-the-air BLE address after a physical button release.
No customer document, captured packet, or production device ID is bundled.

## Board and wiring assumptions

- ESP32-S3, at least 4 MiB SPI flash, DIO at 40 MHz, no PSRAM requirement.
- GPIO7: momentary button to GND, internal pull-up, active low. A debounced
  40–1000 ms press must begin after the station is armed; release starts scanning.
  Long presses and presses begun while busy/unconfigured do not start a test.
- GPIO1: 4 kHz, 50% duty LEDC buzzer output. Use a suitable buzzer/driver circuit;
  GPIO output is not a high-current speaker power supply.
- Protocol transports: UART0 TX GPIO43/RX GPIO44 at 115200 8N1, and the native
  USB Serial/JTAG interface on GPIO20 D+/GPIO19 D-. Use one controlling host.
  Configure no flow control. Native USB-OTG CDC is a different interface.
- One short beep: scan started; two beeps: PASS; three beeps: FAIL/unconfigured.

The flash layout is an explicit common-board configuration, not an automatic
measurement of your board. Do not flash this station image onto ESP32-C3 targets.
Secure-boot/encrypted stations need a separately provisioned build.

## Test behavior

The station starts unarmed after every reset. Target configuration is held only
in RAM. Configure it from FlashTool, then press/release the button. An attempt
actively scans for up to 45 seconds: standard scan requests are necessary because
the diagnostic fields are in the scan response. The station never pairs,
connects, accesses GATT, changes the bracelet, or performs OTA.

It aggregates main advertisements and scan responses only for the configured
address and address type. PASS requires the exact 128-bit service UUID
`4fafc201-1fb5-459e-8fcc-c5c9c331914b` in main ADV; complete printable-ASCII name
`Lumysi-` plus a non-space suffix; manufacturer payload of exactly 12 bytes:

```
FF FF 4C [faults:4 bytes big endian] [major:1] [minor:2 big endian] [battery:2 big endian]
```

All 32 fault bits must be zero, including reserved bits, and version must be
exactly 1.188. Battery mV is informational, with no voltage threshold. No UID is
present in this documented payload; the verifier never guesses one from MAC or
device name. Conflicting faults/version/name, ambiguous address types, malformed
data and incomplete responses cannot pass. A valid complete result ends scanning
immediately; otherwise the timeout reports the missing/failed condition.

Every result consumes the armed target. A fresh host session is required for the
next board; late retries of the consumed session cannot rearm it. Commands during
scanning are ignored. If a laptop disconnects mid-scan, wait for completion before
reconnecting or reset the station. No address is changed silently during a scan.

## Serial protocol

Send exactly this ASCII command followed by LF (CRLF is also accepted):

```
BLE_TARGET <64 lowercase hexadecimal session characters> <12 uppercase MAC hex characters>
```

The all-zero and all-FF addresses are rejected. Protocol output is UTF-8, one JSON
object per line, prefixed exactly `BLE_VERIFY `. Events are:

- `ready`: `event`, `session`, `mac`.
- `start`: those fields plus `attempt` (positive, increasing until reset).
- `result`: those fields plus `status` (`PASS！`, with U+FF01, or `FAIL`). PASS also
  contains `observed_mac`, `name`, `firmware`, `fault_bits`, `battery_mv`. FAIL may
  contain available observations and `reason`. `mac` always remains the requested
  target; `observed_mac` is emitted only after that address was actually observed.

Ordinary boot/debug lines do not have the protocol prefix. This protocol binds
records to a selected address/session; BLE advertisements are not authenticated
proof against another transmitter deliberately cloning an address and payload.

## Build and flash

Install/activate official ESP-IDF **v5.3.2**, with its ESP32-S3 tools, plus host
`cc`. Run from Linux/macOS with Python and Bash:

```sh
source /path/to/esp-idf/export.sh
bash firmware/ble-verifier/scripts/build.sh /tmp/ble-verifier-build /tmp/ble-verifier-export
```

All generated files stay outside the repository. The script compiles portable
parser tests, builds bootloader/partition table/app with the actual SDK, obtains
offsets from generated `flasher_args.json`, merges with esptool, verifies each
segment and FF padding, checks bootloader/app image checksums, and emits
`manifest.json` with binary and public source hashes. SDK central/broadcaster
roles are compiled to work around an observer-only conditional-build defect in
5.3.2; the application uses only scan/cancel APIs, never those radio operations.

With FlashTool choose **ESP32-S3**, select the exported merged BIN and address
**0x0**. Alternatively, with the exported directory as the current directory:

```sh
python -m esptool --chip esp32s3 --port COMx --baud 460800 write_flash --flash_mode dio --flash_size 4MB --flash_freq 40m 0x0 ESP32S3-BLE-Verifier-v0.1.0-merged.bin
```

The actual generated segments are bootloader at 0x0, partition table at 0x8000,
and app at 0x10000. `manifest.json` is authoritative for that build. Compilation
and parser tests do not establish RF range, button wiring, buzzer current, or
behavior on a physical board; commission the station with known passing/failing
targets before production use.
