# UART Protocol

This project uses `UART0` as a raw binary upstream transport after the short boot-time shell exits.

## Physical link

Default UART settings:

- `UART0`
- `115200` baud
- `8N1`

Pins:

- `GPIO0` = UART TX from Pico
- `GPIO1` = UART RX into Pico

## Runtime behavior

Once the bridge is running, the Pico does not parse UART payloads. It relays bytes between UART and the active bridged TCP socket.

Rules:

- no ASCII command mode
- no slash-command parsing
- no newline framing
- binary bytes are forwarded unchanged
- data received from the remote peer is written back to UART unchanged

Actual relay behavior in firmware:

- UART RX from the host is read in chunks of up to `1024` bytes and written to the TCP socket until all bytes are sent
- TCP RX from the remote peer is read in chunks of up to `256` bytes
- TCP-to-UART traffic is queued in a `2048` byte overwrite ring
- UART TX drains that queue in chunks of up to `256` bytes

If the UART egress side falls behind, the firmware does not apply backpressure to the network side. Instead it drops the oldest queued TCP-to-UART bytes when the `2048` byte ring overflows.

Implication for host drivers:

- host-to-Pico UART writes are lossless as long as the UART link itself is healthy
- Pico-to-host UART reads can lose old data if the host stops reading for too long
- a host driver should read continuously or from a dedicated RX thread/task
- if your application needs message boundaries, sequence numbers, CRCs, or retransmission, add that above UART in your own protocol

## Boot behavior

During early boot, UART is temporarily attached to the configuration shell for `3000 ms`.

During that window the Pico can emit ASCII lines such as:

- `pico-fi uart bridge`
- `booting with compiled config`
- config command help

After that startup/config window ends, UART switches to pure binary relay mode.

Implication for host drivers:

- if the Pico was just reset or powered up, do not treat the first received bytes as binary payload without checking for the boot shell window
- either wait at least `3` seconds after reset before starting binary traffic, or explicitly send `start\r\n` during the shell window
- if your host opens the port after the bridge is already running, no startup text is expected

## C driver requirements

A correct C driver for Pico-Fi UART mode should do all of the following:

- open the serial device in raw mode
- configure `115200`, `8` data bits, no parity, `1` stop bit
- disable canonical mode, echo, software flow control, and output post-processing
- treat TX and RX as opaque `uint8_t` buffers, not C strings
- allow partial `read()` and `write()` results
- loop until the full TX buffer has been written
- run RX continuously and deliver bytes upward exactly as received
- ignore or explicitly drain the boot shell text when the Pico has just booted

The driver should not:

- append `'\n'`, `'\r'`, or `'\0'` to binary payloads
- stop reading at `0x00`
- assume one `write()` maps to one remote packet
- assume one `read()` returns a complete application message

## Minimal POSIX shape

On Linux or other POSIX systems, a binary-safe driver should look like this:

```c
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

static int pico_uart_open(const char *path) {
    int fd = open(path, O_RDWR | O_NOCTTY);
    if (fd < 0) {
        return -1;
    }

    struct termios tio;
    if (tcgetattr(fd, &tio) != 0) {
        close(fd);
        return -1;
    }

    cfmakeraw(&tio);
    cfsetispeed(&tio, B115200);
    cfsetospeed(&tio, B115200);

    tio.c_cflag |= (CLOCAL | CREAD);
    tio.c_cflag &= ~PARENB;
    tio.c_cflag &= ~CSTOPB;
    tio.c_cflag &= ~CSIZE;
    tio.c_cflag |= CS8;
    tio.c_cflag &= ~CRTSCTS;

    tio.c_cc[VMIN] = 0;
    tio.c_cc[VTIME] = 1; /* 100 ms read timeout */

    if (tcsetattr(fd, TCSANOW, &tio) != 0) {
        close(fd);
        return -1;
    }

    tcflush(fd, TCIOFLUSH);
    return fd;
}

static int pico_uart_write_all(int fd, const uint8_t *buf, size_t len) {
    while (len > 0) {
        ssize_t n = write(fd, buf, len);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            return -1;
        }
        buf += (size_t)n;
        len -= (size_t)n;
    }
    return 0;
}

static ssize_t pico_uart_read_some(int fd, uint8_t *buf, size_t cap) {
    for (;;) {
        ssize_t n = read(fd, buf, cap);
        if (n < 0 && errno == EINTR) {
            continue;
        }
        return n;
    }
}
```

Usage model:

- call `pico_uart_open()`
- if the Pico has just booted, wait `3` seconds or send `start\r\n`
- use `pico_uart_write_all(fd, tx, tx_len)` for binary TX
- call `pico_uart_read_some(fd, rx, sizeof(rx))` in a loop
- pass every received byte to your upper-layer parser or stream consumer

## Embedded C shape

If the host side is another MCU instead of Linux:

- configure the UART peripheral for `115200 8N1`
- use interrupt-driven or DMA-driven RX if possible
- push received bytes into a ring buffer that is larger than your worst-case service latency
- do not parse on the ISR boundary unless the parser is already binary-safe and bounded
- provide a `write_all()` style function that waits until every byte is queued or transmitted

Recommended abstraction:

```c
struct pico_fi_uart {
    int (*write_all)(const uint8_t *buf, size_t len, uint32_t timeout_ms);
    ssize_t (*read_some)(uint8_t *buf, size_t cap, uint32_t timeout_ms);
};
```

That keeps Pico-Fi transport handling separate from your application framing.

## ThreadX + STM32G4 HAL example

On STM32G4 with ThreadX, a practical driver shape is:

- configure one USART for `115200 8N1`
- start interrupt-driven single-byte RX with `HAL_UART_Receive_IT()`
- push RX bytes into a ring buffer from `HAL_UART_RxCpltCallback()`
- have one ThreadX thread drain the RX ring and hand bytes to your application
- have a separate ThreadX thread or mutex-protected function serialize TX calls

Example skeleton:

```c
#include "tx_api.h"
#include "stm32g4xx_hal.h"
#include <stdint.h>
#include <string.h>

#define PICO_RX_RING_SIZE 1024u
#define PICO_RX_EVENT      (1u << 0)

typedef struct {
    UART_HandleTypeDef *huart;
    TX_MUTEX tx_mutex;
    TX_EVENT_FLAGS_GROUP events;
    uint8_t rx_byte;
    uint8_t rx_ring[PICO_RX_RING_SIZE];
    volatile uint16_t rx_head;
    volatile uint16_t rx_tail;
    volatile uint32_t rx_overflows;
} pico_fi_uart_t;

static pico_fi_uart_t g_pico;

static uint16_t ring_next(uint16_t idx) {
    return (uint16_t)((idx + 1u) % PICO_RX_RING_SIZE);
}

static void ring_push_isr(pico_fi_uart_t *ctx, uint8_t byte) {
    uint16_t next = ring_next(ctx->rx_head);
    if (next == ctx->rx_tail) {
        ctx->rx_overflows++;
        return;
    }
    ctx->rx_ring[ctx->rx_head] = byte;
    ctx->rx_head = next;
}

static size_t ring_pop_many(pico_fi_uart_t *ctx, uint8_t *out, size_t cap) {
    size_t count = 0;

    __disable_irq();
    while (count < cap && ctx->rx_tail != ctx->rx_head) {
        out[count++] = ctx->rx_ring[ctx->rx_tail];
        ctx->rx_tail = ring_next(ctx->rx_tail);
    }
    __enable_irq();

    return count;
}

UINT pico_fi_uart_init(pico_fi_uart_t *ctx, UART_HandleTypeDef *huart) {
    memset(ctx, 0, sizeof(*ctx));
    ctx->huart = huart;

    if (tx_mutex_create(&ctx->tx_mutex, "pico_tx") != TX_SUCCESS) {
        return TX_NOT_DONE;
    }
    if (tx_event_flags_create(&ctx->events, "pico_evt") != TX_SUCCESS) {
        return TX_NOT_DONE;
    }

    if (HAL_UART_Receive_IT(ctx->huart, &ctx->rx_byte, 1u) != HAL_OK) {
        return TX_NOT_DONE;
    }
    return TX_SUCCESS;
}

UINT pico_fi_uart_write_all(pico_fi_uart_t *ctx,
                            const uint8_t *buf,
                            size_t len,
                            ULONG timeout_ticks) {
    if (tx_mutex_get(&ctx->tx_mutex, timeout_ticks) != TX_SUCCESS) {
        return TX_NOT_AVAILABLE;
    }

    while (len > 0u) {
        uint16_t chunk = (len > 65535u) ? 65535u : (uint16_t)len;
        if (HAL_UART_Transmit(ctx->huart, (uint8_t *)buf, chunk, HAL_MAX_DELAY) != HAL_OK) {
            tx_mutex_put(&ctx->tx_mutex);
            return TX_NOT_DONE;
        }
        buf += chunk;
        len -= chunk;
    }

    tx_mutex_put(&ctx->tx_mutex);
    return TX_SUCCESS;
}

size_t pico_fi_uart_read_some(pico_fi_uart_t *ctx,
                              uint8_t *buf,
                              size_t cap,
                              ULONG timeout_ticks) {
    ULONG actual;
    size_t count = ring_pop_many(ctx, buf, cap);
    if (count > 0u) {
        return count;
    }

    if (tx_event_flags_get(&ctx->events,
                           PICO_RX_EVENT,
                           TX_OR_CLEAR,
                           &actual,
                           timeout_ticks) != TX_SUCCESS) {
        return 0u;
    }

    return ring_pop_many(ctx, buf, cap);
}

void HAL_UART_RxCpltCallback(UART_HandleTypeDef *huart) {
    if (huart == g_pico.huart) {
        ring_push_isr(&g_pico, g_pico.rx_byte);
        tx_event_flags_set(&g_pico.events, PICO_RX_EVENT, TX_OR);
        (void)HAL_UART_Receive_IT(g_pico.huart, &g_pico.rx_byte, 1u);
    }
}

void HAL_UART_ErrorCallback(UART_HandleTypeDef *huart) {
    if (huart == g_pico.huart) {
        (void)HAL_UART_AbortReceive(huart);
        (void)HAL_UART_Receive_IT(g_pico.huart, &g_pico.rx_byte, 1u);
    }
}
```

Minimal RX thread:

```c
void pico_rx_thread(ULONG arg) {
    pico_fi_uart_t *ctx = (pico_fi_uart_t *)arg;
    uint8_t buf[128];

    for (;;) {
        size_t n = pico_fi_uart_read_some(ctx, buf, sizeof(buf), TX_WAIT_FOREVER);
        if (n == 0u) {
            continue;
        }
        app_consume_pico_bytes(buf, n);
    }
}
```

HAL UART setup should match:

```c
huart2.Instance = USART2;
huart2.Init.BaudRate = 115200;
huart2.Init.WordLength = UART_WORDLENGTH_8B;
huart2.Init.StopBits = UART_STOPBITS_1;
huart2.Init.Parity = UART_PARITY_NONE;
huart2.Init.Mode = UART_MODE_TX_RX;
huart2.Init.HwFlowCtl = UART_HWCONTROL_NONE;
huart2.Init.OverSampling = UART_OVERSAMPLING_16;
HAL_UART_Init(&huart2);
```

Notes for this STM32G4/ThreadX shape:

- `HAL_UART_Transmit()` is blocking, so keep TX out of time-critical threads
- if throughput matters, switch TX and RX to DMA but keep the same `write_all()` and `read_some()` interface
- the RX ring should be sized for the longest period your RX thread may be delayed
- this driver treats Pico-Fi UART as a byte stream only; your packet parser belongs in `app_consume_pico_bytes()`
- after Pico reset, wait `3` seconds or send `start\r\n` before treating the link as binary-only

## Binary protocol guidance above UART

Because the Pico-Fi UART transport is only a byte stream, application framing is your responsibility. A practical binary packet format is:

- `sync`
- `length`
- `type`
- `payload`
- `crc`

At minimum, add:

- a length field so the receiver can reassemble messages from arbitrary `read()` chunking
- a checksum or CRC so corruption can be detected
- a sequence number if dropped outbound bytes would be a problem for your application

Without that extra layer, your code can still stream raw bytes, but it cannot reliably detect truncation or dropped old bytes on the Pico-to-host direction.

## References

- Firmware UART bridge: [src/bridge/uart.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/bridge/uart.rs)
- Boot/config shell: [src/shell.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/shell.rs)
- UART setup in firmware: [src/main.rs](/Users/rylan/Documents/GitKraken/pico-fi/src/main.rs)
- Host UART examples: [host/python/uart/test.py](/Users/rylan/Documents/GitKraken/pico-fi/host/python/uart/test.py)
