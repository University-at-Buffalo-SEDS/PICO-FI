# Gateway Ethernet bridge

The default `pico-fi.json` and `pico-fi-gateway-ethernet.json` connect UART1 to GroundStation over the main Ethernet LAN. GroundStation hosts TCP on an automatically selected port; Pico-Fi discovers it. No static address is needed.

```sh
PICO_FI_UART_BAUD_RATE=1000000 python3 build.py --config pico-fi-gateway-ethernet.json --release
```

Match the gateway UART baud. This command builds a UF2 without flashing. Use `pico-fi-gateway-ethernet-server.json` for the reverse role, with GroundStation client discovery enabled. Explicit older client/server profiles remain available for older deployments.

Automatic addressing tries DHCP for ten seconds, then probes for an unused IPv4 link-local address. It detects conflicts, retries while disconnected and leaves healthy TCP sessions alone. Each device gets a MAC derived from its physical flash identity. The Pi must enable DHCP plus IPv4 link-local addressing on its wired interface; Ubuntu setup instructions and a preview-only helper are in GroundStation's `docs/picofi-ethernet.md`.

Compatible peers exchange versioned 32-byte UDP messages on port 49361. Disconnected clients query once per second; servers reply at most four times per second. Healthy TCP sessions send no discovery queries. Discovery stays on the Ethernet LAN and never enters SEDSnet/CAN/RFD. It identifies compatible peers without providing authentication.

Saved shell overrides survive a firmware update. `reset` restores compiled defaults. Alternatively select `set auto`, `set discover`, and `set upstream uart`, settings persist automatically; use `start` to begin bridging. Use `show` to inspect the selected configuration.
