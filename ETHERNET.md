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

## UART / Ethernet dual-core bridge

UART transport now uses both RP2040 cores by default in client and server roles.
Core 0 owns W5500 SPI/DMA, TCP, DHCP/link-local and discovery. Core 1 owns UART
framing, RX/TX and the selected UART interrupt. The boot configuration shell
runs first, then UART ownership and its interrupt move to core 1. Other upstream
transports retain their existing execution path.

The cores exchange complete frames through two 16 KiB byte-backed, 128-frame
mailboxes protected by the RP2040 multicore critical-section implementation.
Accepted frames remain FIFO; there is no heap allocation or eviction of accepted
frames. A full UART-to-network mailbox rejects new whole frames and counts the
loss while UART reception continues. The reverse direction applies TCP
backpressure. TCP RX and TX run concurrently, so blocking either direction does
not block the other. UART RX buffering is 4 KiB. Ordinary small packets still
write immediately without a batching delay.

The compiled UART speed is unchanged by using two cores. Keep the 1 Mbaud build
command above for the gateway. `/cores` on the local framed UART command channel
reports core ownership and frame/drop/error counters; `/queues` reports shared
mailbox diagnostics. These are local bridge commands and do not enter SEDSnet.
A sustained upstream rate above available Ethernet/UART capacity can still lose
data; dual-core execution cannot repair CAN packets already dropped at gateway.

Host qualification includes 20,000 concurrent producer/consumer transfers,
maximum-sized packets across ring wrap, full-queue preservation, and production
TCP-pump tests with blocked upload, blocked downlink and deliberately partial
writes. Run `python3 build.py --test`. Physical installation and sustained
end-to-end results are recorded separately; a host test is not proof of board
IRQ wiring or radio-link performance.


### Stack and reconnect qualification

Core 1 reserves 32 KiB. `build.py` checks the linked Cortex-M0+ task prologue
before producing a UF2: the UART task's fixed frame plus 8 KiB of headroom must
fit, and the largest other linked frame plus headroom must fit the main stack
space above BSS. This is a regression guard, not a full call-graph stack proof.
The original 16 KiB reservation failed on hardware because the compiler's UART
poll frame exceeded it. Borrowed enqueue avoids full-size packet return copies;
the qualified build uses a 21,344-byte UART frame. Tests include the original
oversized ARM prologue and fail the old reservation.

TCP/DHCP state receives fresh RP2040 oscillator entropy each boot, keeping the
hardware-derived MAC and link-local identity stable. GroundStation must include
its half-open-session replacement fix: a validated new PICOFI1 connection replaces
an old connection that a restarted bridge could not close.

The 2026-10-20 gateway-role image was programmed and verified through the Debug
Probe at 1 Mbaud UART. Earlier read-only RAM captures confirmed network core 0
and UART core 1 were running; host qualification passed 50 Rust tests plus the
Python framing/stack/soak wrappers. These checks do not establish zero gateway
CAN loss or sustained end-to-end throughput; measure those separately after the
matching GroundStation service is restarted.
