# Gateway Ethernet bridge

The default `pico-fi.json` and `pico-fi-gateway-ethernet.json` connect UART1 to GroundStation over the main Ethernet LAN. GroundStation hosts TCP on an automatically selected port; Pico-Fi discovers it. No static address is needed.

```sh
PICO_FI_UART_BAUD_RATE=1000000 python3 build.py --config pico-fi-gateway-ethernet.json --release
```

Match the gateway UART baud. This command builds a UF2 without flashing. Use `pico-fi-gateway-ethernet-server.json` for the reverse role, with GroundStation client discovery enabled. Explicit older client/server profiles remain available for older deployments.

Automatic addressing tries DHCP for ten seconds, then probes for an unused IPv4 link-local address. It detects conflicts and probes for DHCP availability while link-local is active, including during a healthy TCP session. A valid offer matching the current MAC/transaction triggers normal DHCP acquisition; the bridge then drops its old socket, clears discovered peer state, and reconnects with the new address. Probes without an offer do not interrupt TCP. Failed migrations back off before trying again. Each device gets a MAC derived from its physical flash identity. The Pi must enable DHCP plus IPv4 link-local addressing on its wired interface; Ubuntu setup instructions and a preview-only helper are in GroundStation's `docs/picofi-ethernet.md`.

Compatible peers exchange versioned 32-byte UDP messages on port 49361. Disconnected clients query once per second; servers reply at most four times per second. Healthy TCP sessions send no discovery queries. Discovery stays on the Ethernet LAN and never enters SEDSnet/CAN/RFD. It identifies compatible peers without providing authentication.

Saved shell overrides survive a firmware update. `reset` restores compiled defaults. Alternatively select `set auto`, `set discover`, and `set upstream uart`, settings persist automatically; use `start` to begin bridging. Use `show` to inspect the selected configuration.

## Router arrival and removal

`Auto` addressing works for both TCP roles. Link-local connections remain usable
without a router; a small DHCP-presence probe runs roughly every 15 seconds, with
per-device jitter, while using link-local. Only a fresh validated offer starts
the normal DHCP client. Ethernet link loss or lease expiry drops the old socket
and restarts discovery. Same-address DHCP renewals preserve a live TCP session.
Two Pico-Fi devices use the same policy: select opposite TCP roles, discovery
on, and Auto addressing on both. A Pi must also enable DHCP plus link-local on
its physical Ethernet interface.

The firmware never runs two lease clients, does not allocate packet-sized heap
buffers for presence probes, and does not change the compiled UART speed when
Ethernet addressing changes. Gateway builds must retain `PICO_FI_UART_BAUD_RATE=1000000`.

W5500-EVB-Pico SWD (components facing you, USB at top and Ethernet at bottom):
the vertical debug pads are **SWDIO, GND, SWCLK** from top to bottom. Use the
Raspberry Pi Debug Probe D port and power the board separately. Validate with:

```sh
probe-rs info --probe 2e8a:000c --chip RP2040 --protocol swd --speed 100 --non-interactive
PICO_FI_UART_BAUD_RATE=1000000 python3 build.py --config pico-fi-gateway-ethernet.json --release
probe-rs download target/pico-fi-gateway-ethernet/thumbv6m-none-eabi/release/pico-fi --probe 2e8a:000c --chip RP2040 --protocol swd --speed 100 --non-interactive --verify
probe-rs reset --probe 2e8a:000c --chip RP2040 --protocol swd --speed 100 --non-interactive
```

Use a full probe serial selector when multiple matching Debug Probes are attached.
`--speed 100` is SWD clock speed in kHz; it does not affect the 1 Mbaud UART.
