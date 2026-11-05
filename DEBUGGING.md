# Remote Debugging with RustRover

This project is configured for RustRover's `Remote Debug` flow with OpenOCD acting as the GDB server for the RP2040.

Use the shared configuration named `Pico Debug`.

## Defaults

- Symbol file: `target/thumbv6m-none-eabi/debug/pico-fi`
- GDB server: `localhost:3333`
- Client debugger wrapper: `rustrover-gdb.sh`
- Pre-launch build: `Build Firmware`
- Flash helper: `flash-firmware.sh`

## Start debugging

1. Connect the Pico over SWD.
2. Flash the current ELF onto the RP2040:

```bash
./flash-firmware.sh
```

3. Start OpenOCD:

```bash
./start-debugger.sh
```

4. In RustRover, select `Pico Debug` and click `Debug`.

This is the stable split for this repo:

- `probe-rs` flashes the exact ELF you are debugging
- OpenOCD serves `localhost:3333`
- RustRover attaches using `arm-none-eabi-gdb`
- OpenOCD resets and halts the RP2040 on GDB attach so breakpoints in `main` can bind before execution continues

## Required probe firmware

`probe-rs 0.31.0` requires Raspberry Pi Debug Probe firmware version `2.2.0` or newer.

On this machine, `probe-rs` currently reports that the attached probe firmware is too old to use. Update the Debug Probe
firmware first, then retry.

Raspberry Pi documents the update process here:

- https://www.raspberrypi.com/documentation/microcontrollers/debug-probe.html

## Overrides

```bash
OPENOCD_GDB_PORT=3334 ./start-debugger.sh
OPENOCD_SPEED=1000 ./start-debugger.sh
PROBE_RS_PROBE=2e8a:000c:E6647C74037BAA31 ./flash-firmware.sh
GDB_BIN=/custom/path/arm-none-eabi-gdb ./rustrover-gdb.sh --version
```

## RustRover notes

RustRover can reliably do the build and attach parts, but for this target the flash step is best kept explicit. The
working sequence is:

1. `Build Firmware`
2. `./flash-firmware.sh`
3. `./start-debugger.sh`
4. `Pico Debug`

If breakpoints do not bind, rebuild and reflash once and confirm the RustRover symbol file still points at
`target/thumbv6m-none-eabi/debug/pico-fi`.

## Role helper with verified SWD download

For the role-based build command, `--probe` selects the debug probe and `--speed`
sets SWD kHz; for example:

```sh
PICO_FI_CONFIG=pico-fi-gateway-ethernet.json ./build-role.sh --flash --release --probe 2e8a:000c:YOUR_PROBE_SERIAL --speed 100 client
```

Both `build.py` and the role helper default to 100 kHz SWD and verify the ELF
download before resetting in a single `probe-rs download --verify --reset` session. Compiler and probe diagnostics remain visible. A reset command returning
success does not establish that the bridge started or connected; check its
network/UART behavior separately. Probe warnings remain warnings in the final
summary, and any nonzero probe command exits without a success summary.

## RP2040 reset-session cleanup diagnostics

The shared flash helper reports the exact probe-rs executable and its `--version`
before using the probe. Download, verification, or reset failures preserve the
combined command's nonzero exit status; zero-exit cleanup warnings are reported
separately. Raw
probe diagnostics stay visible and their original exit status is preserved.

`Could not clear all hardware breakpoints` after reset can come from probe-rs
session teardown: it attempts cleanup on every target core, including the RP2040
secondary debug port. `reset` already selects core 0 by default; adding `--core 0`
does not restrict that later cleanup to core 0. No supported cleanup-skip CLI
option was found in probe-rs 0.31.0, 0.32.0 or the inspected upstream Session API.
The helper therefore keeps the real reset and does not invent a single-core chip
definition, hide the warning, retry the flash or change target power/reset policy.

probe-rs 0.32.0 fixes a separate *spurious* DP-switch warning when its quick
connection fails but the fallback succeeds. It still reports genuine cleanup
failures. A verified download plus this warning does not establish that the
firmware is running, nor does it prove that flashing failed. Preserve the full
output and check the bridge's actual runtime behavior on the test hardware.
This script change identifies the uncertain stage; it does not claim to repair
the upstream/probe/target interaction without a hardware reproduction.

The 2026-11-05 gateway client reproduction emitted the secondary-port cleanup
warning at probe-rs's default speed. A combined verified download/reset at
100 kHz completed cleanly on the attached Debug Probe. Both build scripts now
use that conservative default and a single session. `--speed` or
`PROBE_RS_SPEED` can override it; a clean flash does not qualify bridge runtime.

Upstream references:
- [Reset command and selected core](https://github.com/probe-rs/probe-rs/blob/v0.32.0/probe-rs-tools/src/bin/probe-rs/cmd/reset.rs)
- [Session cleanup on all cores](https://github.com/probe-rs/probe-rs/blob/v0.32.0/probe-rs/src/session.rs)
- [0.32.0 changelog, RP2040 DP-switch warning fix #4133](https://github.com/probe-rs/probe-rs/blob/v0.32.0/CHANGELOG.md)
