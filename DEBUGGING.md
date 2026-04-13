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
