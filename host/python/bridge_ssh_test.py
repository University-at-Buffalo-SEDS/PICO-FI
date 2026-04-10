#!/usr/bin/env python3
"""SSH-driven SPI/UART bridge test harness."""

from __future__ import annotations

import argparse
import collections
import queue
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
UART_TEST = REPO_ROOT / "host/python/uart/test.py"
SPI_TEST_DIR = REPO_ROOT / "host/python/spi"
REMOTE_SPI_STAGE = "/tmp/pico-fi-spi-test"
LINK_HANDSHAKE_MAGIC = b"PICOFI1"
FRAME_SIZE = 258
PAYLOAD_MAX = FRAME_SIZE - 2
REQ_DATA_MAGIC = 0xA5
REQ_COMMAND_MAGIC = 0xA6
RESP_DATA_MAGIC = 0x5A
RESP_COMMAND_MAGIC = 0x5B
ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")


def default_local_python() -> str:
    candidates = [
        REPO_ROOT / "venv/bin/python",
        REPO_ROOT / ".venv/bin/python",
        Path.home() / "venv/bin/python",
    ]
    for candidate in candidates:
        if candidate.exists():
            return str(candidate)
    return sys.executable


def run_checked(cmd: list[str], cwd: Path | None = None) -> int:
    print("+", " ".join(shlex.quote(part) for part in cmd))
    completed = subprocess.run(cmd, cwd=cwd)
    return completed.returncode


def format_bytes(data: bytes, limit: int = 16) -> str:
    return " ".join(f"{byte:02x}" for byte in data[:limit])


def build_frame(payload: bytes, magic: int) -> bytes:
    payload = payload[:PAYLOAD_MAX]
    frame = bytearray(FRAME_SIZE)
    frame[0] = magic
    frame[1] = len(payload)
    frame[2 : 2 + len(payload)] = payload
    return bytes(frame)


def parse_frame(frame: bytes) -> tuple[int, bytes]:
    if len(frame) != FRAME_SIZE:
        return 0, b""
    magic = frame[0]
    length = frame[1]
    if magic not in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC) or length > PAYLOAD_MAX:
        return 0, b""
    return magic, bytes(frame[2 : 2 + length])


class LocalUartSession:
    def __init__(self, port: str, baud: int) -> None:
        try:
            import serial
        except ModuleNotFoundError as exc:
            raise RuntimeError(
                "pyserial is required for local UART monitoring; use a Python with serial installed"
            ) from exc
        self.port = port
        self.baud = baud
        self.ser = serial.Serial(
            port=port,
            baudrate=baud,
            timeout=0.1,
            bytesize=serial.EIGHTBITS,
            parity=serial.PARITY_NONE,
            stopbits=serial.STOPBITS_ONE,
            xonxoff=False,
            rtscts=False,
            dsrdtr=False,
        )
        self.lock = threading.Lock()
        self.log: list[str] = []

    def close(self) -> None:
        self.ser.close()

    def _record(self, line: str) -> None:
        stamped = f"[uart] {line}"
        self.log.append(stamped)
        print(stamped, flush=True)

    def dump_log(self) -> str:
        return "\n".join(self.log[-80:])

    def _read_frame(self, timeout_s: float) -> bytes | None:
        deadline = time.monotonic() + timeout_s
        buf = bytearray()
        while time.monotonic() < deadline and len(buf) < FRAME_SIZE:
            chunk = self.ser.read(FRAME_SIZE - len(buf))
            if chunk:
                buf.extend(chunk)
        if len(buf) == FRAME_SIZE:
            return bytes(buf)
        if buf:
            self._record(f"short frame {len(buf)} bytes: {format_bytes(bytes(buf))}")
        return None

    def _send_frame(self, magic: int, payload: bytes) -> None:
        tx = build_frame(payload, magic)
        self.ser.write(tx)
        self.ser.flush()
        self._record(f"tx magic=0x{magic:02x} len={len(payload)} raw={format_bytes(tx)}")

    def _drain_input(self) -> None:
        waiting = self.ser.in_waiting
        if waiting:
            drained = self.ser.read(waiting)
            if drained:
                self._record(f"drain {len(drained)} bytes: {format_bytes(drained)}")

    def command(self, text: str, timeout_s: float = 2.0) -> str:
        with self.lock:
            self._drain_input()
            self._send_frame(REQ_COMMAND_MAGIC, (text + "\n").encode("utf-8"))
            deadline = time.monotonic() + timeout_s
            while time.monotonic() < deadline:
                frame = self._read_frame(min(0.25, max(0.01, deadline - time.monotonic())))
                if frame is None:
                    continue
                magic, payload = parse_frame(frame)
                self._record(
                    f"rx magic=0x{magic:02x} len={len(payload)} raw={format_bytes(frame)}"
                )
                if magic == RESP_DATA_MAGIC and payload:
                    self._record(f"data {payload.decode('utf-8', errors='replace')!r}")
                    continue
                if magic == RESP_COMMAND_MAGIC:
                    text_payload = payload.decode("utf-8", errors="replace")
                    self._record(f"cmd {text_payload!r}")
                    return text_payload
            raise RuntimeError(f"timed out waiting for UART command reply to {text!r}")

    def send_data(self, text: str, timeout_s: float = 2.0) -> None:
        with self.lock:
            self._drain_input()
            self._send_frame(REQ_DATA_MAGIC, text.encode("utf-8"))
            deadline = time.monotonic() + timeout_s
            while time.monotonic() < deadline:
                frame = self._read_frame(min(0.25, max(0.01, deadline - time.monotonic())))
                if frame is None:
                    continue
                magic, payload = parse_frame(frame)
                self._record(
                    f"rx magic=0x{magic:02x} len={len(payload)} raw={format_bytes(frame)}"
                )
                if magic in (RESP_DATA_MAGIC, RESP_COMMAND_MAGIC):
                    if payload:
                        self._record(f"payload {payload.decode('utf-8', errors='replace')!r}")
                    return
            raise RuntimeError(f"timed out waiting for UART send ack for {text!r}")

    def recv_data(self, expect: str, timeout_s: float = 10.0, poll_s: float = 0.05) -> str:
        with self.lock:
            deadline = time.monotonic() + timeout_s
            while time.monotonic() < deadline:
                self._send_frame(REQ_DATA_MAGIC, b"")
                frame = self._read_frame(min(0.5, max(0.05, deadline - time.monotonic())))
                if frame is None:
                    time.sleep(poll_s)
                    continue
                magic, payload = parse_frame(frame)
                self._record(
                    f"rx magic=0x{magic:02x} len={len(payload)} raw={format_bytes(frame)}"
                )
                if magic == RESP_DATA_MAGIC and payload:
                    text = payload.decode("utf-8", errors="replace")
                    self._record(f"data {text!r}")
                    if expect in text:
                        return text
                elif magic == RESP_COMMAND_MAGIC and payload:
                    self._record(f"cmd {payload.decode('utf-8', errors='replace')!r}")
                time.sleep(poll_s)
            raise RuntimeError(f"timed out waiting for UART data containing {expect!r}")


def build_ssh_command(target: str, remote_command: str) -> list[str]:
    return ["ssh", target, remote_command]


def shell_quote_remote_path(path: str) -> str:
    if path == "~":
        return '"$HOME"'
    if path.startswith("~/"):
        suffix = path[2:]
        if not suffix:
            return '"$HOME"'
        return f'"$HOME"/{shlex.quote(suffix)}'
    return shlex.quote(path)


def stage_remote_spi_tools(target: str) -> int:
    mkdir_cmd = build_ssh_command(target, f"mkdir -p {shlex.quote(REMOTE_SPI_STAGE)}")
    rc = run_checked(mkdir_cmd)
    if rc != 0:
        return rc
    files = [
        SPI_TEST_DIR / "__init__.py",
        SPI_TEST_DIR / "link_terminal_driver.py",
        SPI_TEST_DIR / "link_terminal.py",
        SPI_TEST_DIR / "raw.py",
        SPI_TEST_DIR / "test.py",
    ]
    return run_checked(["scp", *map(str, files), f"{target}:{REMOTE_SPI_STAGE}/"])


def run_remote_spi_probe(target: str, remote_root: str, count: int, speed: int) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} probe --count {count}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_command(target: str, remote_root: str, text: str, speed: int) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} command {shlex.quote(text)}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_echo(target: str, remote_root: str, text: str, speed: int) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} echo {shlex.quote(text)}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_data(target: str, remote_root: str, text: str, speed: int, expect: str | None) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} data {shlex.quote(text)}"
    )
    if expect:
        remote += f" --expect {shlex.quote(expect)}"
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_send(target: str, remote_root: str, text: str, speed: int) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} send {shlex.quote(text)}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_recv(target: str, remote_root: str, speed: int, expect: str | None) -> int:
    rc = stage_remote_spi_tools(target)
    if rc != 0:
        return rc
    remote = (
        f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
        f"python3 test.py --verbose-raw --speed {speed} recv"
    )
    if expect:
        remote += f" --expect {shlex.quote(expect)}"
    return run_checked(build_ssh_command(target, remote))


def run_local_uart_probe(python_bin: str, port: str, speed: int, count: int) -> int:
    return run_checked(
        [python_bin, str(UART_TEST), "--port", port, "--speed", str(speed), "probe", "--count", str(count)]
    )


def run_local_uart_command(python_bin: str, port: str, speed: int, text: str) -> int:
    return run_checked(
        [python_bin, str(UART_TEST), "--port", port, "--speed", str(speed), "command", text]
    )


def run_local_uart_data(python_bin: str, port: str, speed: int, text: str, expect: str) -> int:
    cmd = [python_bin, str(UART_TEST), "--port", port, "--speed", str(speed), "data", text]
    if expect:
        cmd.extend(["--expect", expect])
    return run_checked(cmd)


def run_local_uart_send(python_bin: str, port: str, speed: int, text: str) -> int:
    return run_checked(
        [python_bin, str(UART_TEST), "--port", port, "--speed", str(speed), "send", text]
    )


def run_local_uart_recv(python_bin: str, port: str, speed: int, expect: str | None) -> int:
    cmd = [python_bin, str(UART_TEST), "--port", port, "--speed", str(speed), "recv"]
    if expect:
        cmd.extend(["--expect", expect])
    return run_checked(cmd)


def start_remote_echo_server(target: str, bind_host: str, bind_port: int) -> subprocess.Popen[str]:
    server_code = (
        "import socket\n"
        f"s=socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(({bind_host!r}, {bind_port})); s.listen(1)\n"
        "print('READY', flush=True)\n"
        "conn, _ = s.accept()\n"
        "conn.settimeout(10.0)\n"
        "try:\n"
        "    while True:\n"
        "        data = conn.recv(4096)\n"
        "        if not data:\n"
        "            break\n"
        "        conn.sendall(data)\n"
        "finally:\n"
        "    conn.close(); s.close()\n"
    )
    remote_command = f"python3 -u -c {shlex.quote(server_code)}"
    proc = subprocess.Popen(
        ["ssh", target, remote_command],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    assert proc.stdout is not None
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        if not line:
            if proc.poll() is not None:
                raise RuntimeError("remote echo server exited before becoming ready")
            continue
        print(f"[remote] {line.rstrip()}")
        if line.rstrip() == "READY":
            return proc
    proc.terminate()
    raise RuntimeError("timed out waiting for remote echo server")


def start_remote_bridge_echo_client(
    target: str,
    pico_host: str,
    pico_port: int,
    handshake_magic: bytes = LINK_HANDSHAKE_MAGIC,
) -> subprocess.Popen[str]:
    server_code = (
        "import socket, sys, time\n"
        f"HOST={pico_host!r}\n"
        f"PORT={pico_port}\n"
        f"MAGIC={handshake_magic!r}\n"
        "deadline=time.time()+10.0\n"
        "while True:\n"
        "    try:\n"
        "        s=socket.create_connection((HOST, PORT), timeout=1.0)\n"
        "        break\n"
        "    except OSError:\n"
        "        if time.time() > deadline:\n"
        "            raise\n"
        "        time.sleep(0.2)\n"
        "s.settimeout(10.0)\n"
        "hello=s.recv(len(MAGIC))\n"
        "if hello != MAGIC:\n"
        "    raise SystemExit(f'bad handshake: {hello!r}')\n"
        "s.sendall(MAGIC)\n"
        "print('READY', flush=True)\n"
        "try:\n"
        "    while True:\n"
        "        data=s.recv(4096)\n"
        "        if not data:\n"
        "            break\n"
        "        s.sendall(data)\n"
        "finally:\n"
        "    s.close()\n"
    )
    remote_command = f"python3 -u -c {shlex.quote(server_code)}"
    proc = subprocess.Popen(
        ["ssh", target, remote_command],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    assert proc.stdout is not None
    deadline = time.monotonic() + 12.0
    while time.monotonic() < deadline:
        line = proc.stdout.readline()
        if not line:
            if proc.poll() is not None:
                raise RuntimeError("remote bridge echo client exited before becoming ready")
            continue
        print(f"[remote] {line.rstrip()}")
        if line.rstrip() == "READY":
            return proc
    proc.terminate()
    raise RuntimeError("timed out waiting for remote bridge echo client")


def stop_process(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=3)


def run_and_capture(
    cmd: list[str],
    cwd: Path | None = None,
    timeout_s: float | None = None,
) -> tuple[int, str]:
    print("+", " ".join(shlex.quote(part) for part in cmd))
    completed = subprocess.run(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        timeout=timeout_s,
    )
    output = completed.stdout or ""
    if output:
        print(output, end="" if output.endswith("\n") else "\n")
    return completed.returncode, output


def wait_checked(proc: subprocess.Popen[str], timeout_s: float, label: str) -> tuple[int, str]:
    try:
        stdout, _ = proc.communicate(timeout=timeout_s)
    except subprocess.TimeoutExpired:
        stop_process(proc)
        raise RuntimeError(f"{label} timed out after {timeout_s:.1f}s")
    output = stdout or ""
    if output:
        print(output, end="" if output.endswith("\n") else "\n")
    return proc.returncode, output


def spawn_remote_spi_recv(
    target: str,
    speed: int,
    expect: str,
) -> subprocess.Popen[str]:
    return subprocess.Popen(
        build_ssh_command(
            target,
            (
                f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
                f"python3 test.py --verbose-raw --speed {speed} recv --expect {shlex.quote(expect)}"
            ),
        ),
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )


def run_remote_spi_send_checked(target: str, remote_root: str, text: str, speed: int) -> str:
    rc, output = run_and_capture(
        build_ssh_command(
            target,
            (
                f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
                f"python3 test.py --verbose-raw --speed {speed} send {shlex.quote(text)}"
            ),
        ),
        timeout_s=15.0,
    )
    if rc != 0:
        raise RuntimeError(f"remote SPI send failed for {text!r}")
    return output


def run_remote_spi_command_checked(target: str, remote_root: str, text: str, speed: int) -> str:
    rc, output = run_and_capture(
        build_ssh_command(
            target,
            (
                f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
                f"python3 test.py --verbose-raw --speed {speed} command {shlex.quote(text)}"
            ),
        ),
        timeout_s=15.0,
    )
    if rc != 0:
        raise RuntimeError(f"remote SPI command failed for {text!r}")
    return output


class RemoteSpiLinkTerminalSession:
    def __init__(self, target: str, speed: int, poll_ms: int) -> None:
        self.proc = subprocess.Popen(
            build_ssh_command(
                target,
                (
                    f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
                    f"python3 -u link_terminal_driver.py --speed {speed} --poll-ms {poll_ms}"
                ),
            ),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1,
        )
        self.lines: collections.deque[str] = collections.deque(maxlen=200)
        self.queue: queue.Queue[str] = queue.Queue()
        self.reader = threading.Thread(target=self._reader_loop, daemon=True)
        self.reader.start()

    def _reader_loop(self) -> None:
        assert self.proc.stdout is not None
        for raw_line in self.proc.stdout:
            line = ANSI_ESCAPE_RE.sub("", raw_line.replace("\r", "")).rstrip("\n")
            self.lines.append(line)
            print(f"[spi-link] {line}", flush=True)
            self.queue.put(line)

    def send_line(self, text: str) -> None:
        if self.proc.poll() is not None:
            raise RuntimeError("remote SPI link terminal exited")
        assert self.proc.stdin is not None
        self.proc.stdin.write(text + "\n")
        self.proc.stdin.flush()

    def wait_for(self, expected: str, timeout_s: float) -> str:
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if self.proc.poll() is not None and self.queue.empty():
                raise RuntimeError(
                    f"remote SPI link terminal exited before emitting {expected!r}"
                )
            try:
                line = self.queue.get(timeout=min(0.25, max(0.01, deadline - time.monotonic())))
            except queue.Empty:
                continue
            if expected in line:
                return line
        raise RuntimeError(f"timed out waiting for remote SPI link output containing {expected!r}")

    def dump_output(self) -> str:
        return "\n".join(self.lines)

    def close(self) -> None:
        stop_process(self.proc)


def full_smoke(args: argparse.Namespace) -> int:
    rc = run_remote_spi_probe(args.ssh_target, args.remote_root, args.probe_count, args.spi_speed)
    if rc != 0:
        return rc
    rc = run_local_uart_probe(args.local_python, args.uart_port, args.uart_speed, args.probe_count)
    if rc != 0:
        return rc
    rc = run_local_uart_command(args.local_python, args.uart_port, args.uart_speed, "/ping")
    if rc != 0:
        return rc
    echo_proc = start_remote_echo_server(args.ssh_target, args.remote_bind, args.remote_port)
    try:
        return run_local_uart_data(
            args.local_python,
            args.uart_port,
            args.uart_speed,
            args.payload,
            args.payload,
        )
    finally:
        stop_process(echo_proc)


def ensure_substring(output: str, expected: str, label: str) -> None:
    if expected not in output:
        raise RuntimeError(f"{label} missing expected text {expected!r}")


def wait_for_local_link_up(session: LocalUartSession, timeout_s: float, poll_s: float = 0.5) -> str:
    deadline = time.time() + timeout_s
    last = ""
    while time.time() < deadline:
        last = session.command("/link").strip()
        print(f"[soak] /link -> {last}")
        if last == "link up":
            return last
        time.sleep(poll_s)
    raise RuntimeError(f"timed out waiting for link up, last status was {last!r}")


def run_spi_uart_soak(args: argparse.Namespace) -> int:
    rc = stage_remote_spi_tools(args.ssh_target)
    if rc != 0:
        return rc

    session = LocalUartSession(args.uart_port, args.uart_speed)
    try:
        initial_link = wait_for_local_link_up(session, timeout_s=args.recv_timeout_s)
        print(f"[soak] initial /link -> {initial_link}")

        for iteration in range(1, args.iterations + 1):
            uart_to_spi_payload = f"uart-to-spi-{iteration}"
            spi_to_uart_payload = f"spi-to-uart-{iteration}"
            print(f"[soak] iteration {iteration}/{args.iterations}")

            remote_recv = spawn_remote_spi_recv(
                args.ssh_target,
                args.spi_speed,
                uart_to_spi_payload,
            )
            time.sleep(0.2)
            try:
                session.send_data(uart_to_spi_payload)
                rc, output = wait_checked(
                    remote_recv,
                    args.recv_timeout_s,
                    f"remote SPI recv iteration {iteration}",
                )
            finally:
                stop_process(remote_recv)
            if rc != 0:
                raise RuntimeError(f"remote SPI recv failed on iteration {iteration}")
            ensure_substring(
                output,
                uart_to_spi_payload,
                f"remote SPI recv iteration {iteration}",
            )

            run_remote_spi_send_checked(
                args.ssh_target,
                args.remote_root,
                spi_to_uart_payload,
                args.spi_speed,
            )
            seen = session.recv_data(spi_to_uart_payload, timeout_s=args.recv_timeout_s)
            ensure_substring(
                seen,
                spi_to_uart_payload,
                f"local UART recv iteration {iteration}",
            )

            if args.check_commands:
                spi_diag = run_remote_spi_command_checked(
                    args.ssh_target,
                    args.remote_root,
                    "/spi",
                    args.spi_speed,
                )
                ensure_substring(spi_diag, "Magic: 0x5b", f"remote /spi iteration {iteration}")
                ensure_substring(spi_diag, "spi kind=", f"remote /spi iteration {iteration}")
                link = session.command("/link").strip()
                print(f"[soak] post-iteration /link -> {link}")

        print(f"[soak] passed {args.iterations} iterations")
        return 0
    except Exception as exc:
        print(f"[soak] FAIL: {exc}", file=sys.stderr)
        uart_log = session.dump_log()
        if uart_log:
            print("[soak] recent UART log:", file=sys.stderr)
            print(uart_log, file=sys.stderr)
        try:
            spi_diag = run_remote_spi_command_checked(
                args.ssh_target,
                args.remote_root,
                "/spi",
                args.spi_speed,
            )
            print("[soak] remote /spi:", file=sys.stderr)
            print(spi_diag, file=sys.stderr, end="" if spi_diag.endswith("\n") else "\n")
        except Exception as diag_exc:
            print(f"[soak] remote /spi failed: {diag_exc}", file=sys.stderr)
        try:
            link = session.command("/link").strip()
            print(f"[soak] local /link: {link}", file=sys.stderr)
        except Exception as link_exc:
            print(f"[soak] local /link failed: {link_exc}", file=sys.stderr)
        return 1
    finally:
        session.close()


def run_spi_link_terminal_soak(args: argparse.Namespace) -> int:
    rc = stage_remote_spi_tools(args.ssh_target)
    if rc != 0:
        return rc

    session = LocalUartSession(args.uart_port, args.uart_speed)
    remote = RemoteSpiLinkTerminalSession(args.ssh_target, args.spi_speed, args.poll_ms)
    try:
        wait_for_local_link_up(session, timeout_s=args.recv_timeout_s)
        remote.wait_for("READY", timeout_s=10.0)
        remote.send_line("command /link")
        remote.wait_for("link ", timeout_s=6.0)
        remote.wait_for("OK command /link", timeout_s=6.0)

        for iteration in range(1, args.iterations + 1):
            spi_to_uart_payload = f"spi-link-to-uart-{iteration}"
            uart_to_spi_payload = f"uart-to-spi-link-{iteration}"
            print(f"[link-soak] iteration {iteration}/{args.iterations}")

            remote.send_line(f"send {spi_to_uart_payload}")
            remote.wait_for("OK send", timeout_s=6.0)
            seen = session.recv_data(spi_to_uart_payload, timeout_s=args.recv_timeout_s)
            ensure_substring(seen, spi_to_uart_payload, f"local UART recv iteration {iteration}")

            remote.send_line("command /link")
            remote.wait_for("link ", timeout_s=6.0)
            remote.wait_for("OK command /link", timeout_s=6.0)

            session.send_data(uart_to_spi_payload)
            remote.send_line(f"recv {uart_to_spi_payload}")
            matched = remote.wait_for("MATCH ", timeout_s=args.recv_timeout_s)
            ensure_substring(matched, uart_to_spi_payload, f"remote SPI recv iteration {iteration}")

            remote.send_line("command /link")
            remote.wait_for("link ", timeout_s=6.0)
            remote.wait_for("OK command /link", timeout_s=6.0)

        print(f"[link-soak] passed {args.iterations} iterations")
        return 0
    except Exception as exc:
        print(f"[link-soak] FAIL: {exc}", file=sys.stderr)
        uart_log = session.dump_log()
        if uart_log:
            print("[link-soak] recent UART log:", file=sys.stderr)
            print(uart_log, file=sys.stderr)
        remote_output = remote.dump_output()
        if remote_output:
            print("[link-soak] recent SPI link output:", file=sys.stderr)
            print(remote_output, file=sys.stderr)
        try:
            spi_diag = run_remote_spi_command_checked(
                args.ssh_target,
                args.remote_root,
                "/spi",
                args.spi_speed,
            )
            print("[link-soak] remote /spi:", file=sys.stderr)
            print(spi_diag, file=sys.stderr, end="" if spi_diag.endswith("\n") else "\n")
        except Exception as diag_exc:
            print(f"[link-soak] remote /spi failed: {diag_exc}", file=sys.stderr)
        try:
            link = session.command("/link").strip()
            print(f"[link-soak] local /link: {link}", file=sys.stderr)
        except Exception as link_exc:
            print(f"[link-soak] local /link failed: {link_exc}", file=sys.stderr)
        return 1
    finally:
        remote.close()
        session.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="Run remote SPI and local UART bridge tests.")
    parser.add_argument("--ssh-target", required=True, help="SSH target for the Pi connected to the Pico")
    parser.add_argument(
        "--remote-root",
        default="~/Documents/Git/PICO-FI",
        help="Repository root on the remote Pi",
    )
    parser.add_argument("--uart-port", required=True, help="Local UART device path")
    parser.add_argument(
        "--local-python",
        default=default_local_python(),
        help="Python interpreter to use for local UART tests",
    )
    parser.add_argument("--uart-speed", type=int, default=115200)
    parser.add_argument("--spi-speed", type=int, default=100000)
    parser.add_argument("--probe-count", type=int, default=3)
    parser.add_argument("--remote-bind", default="10.8.0.5")
    parser.add_argument("--remote-port", type=int, default=4242)
    parser.add_argument("--bridge-peer-target", default="rylan@10.8.0.5")
    parser.add_argument("--pico-net-host", default="192.168.7.2")
    parser.add_argument("--pico-net-port", type=int, default=5000)
    parser.add_argument("--payload", default="bridge-echo")
    parser.add_argument("--recv-timeout-s", type=float, default=12.0)
    parser.add_argument("--poll-ms", type=int, default=50)

    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("full-smoke", help="Run SPI probe, UART probe/command, and UART data echo test")
    soak = subparsers.add_parser(
        "spi-uart-soak",
        help="Open the local USB UART, then run repeated UART->SPI and SPI->UART end-to-end checks",
    )
    soak.add_argument("--iterations", type=int, default=10)
    soak.add_argument(
        "--check-commands",
        action="store_true",
        help="Also verify remote /spi and local /link after each iteration.",
    )
    link_soak = subparsers.add_parser(
        "spi-link-terminal-soak",
        help="Drive the remote SPI link terminal and verify repeated send/receive plus immediate /link commands",
    )
    link_soak.add_argument("--iterations", type=int, default=10)

    spi_probe = subparsers.add_parser("spi-probe", help="Run the remote SPI probe via SSH")
    spi_probe.add_argument("--count", type=int, default=None)

    spi_command = subparsers.add_parser("spi-command", help="Run the remote SPI command via SSH")
    spi_command.add_argument("text")

    spi_echo = subparsers.add_parser("spi-echo", help="Run the remote SPI echo diagnostic via SSH")
    spi_echo.add_argument("text", nargs="?", default="/ping")

    spi_data = subparsers.add_parser("spi-data-echo", help="Run an end-to-end SPI data echo test through the Ethernet bridge")
    spi_data.add_argument("--text", default=None)
    uart_to_spi = subparsers.add_parser("uart-to-spi", help="Send framed data into the local UART client Pico and receive it from the remote SPI server Pico")
    uart_to_spi.add_argument("--text", default=None)
    spi_to_uart = subparsers.add_parser("spi-to-uart", help="Send framed data into the remote SPI server Pico and receive it from the local UART client Pico")
    spi_to_uart.add_argument("--text", default=None)

    subparsers.add_parser("uart-probe", help="Run the local UART probe")

    uart_command = subparsers.add_parser("uart-command", help="Run a local UART command")
    uart_command.add_argument("text")

    uart_data = subparsers.add_parser("uart-data-echo", help="Start a remote TCP echo server and verify UART data loops back")
    uart_data.add_argument("--text", default=None)

    args = parser.parse_args()

    if args.command == "full-smoke":
        return full_smoke(args)
    if args.command == "spi-uart-soak":
        return run_spi_uart_soak(args)
    if args.command == "spi-link-terminal-soak":
        return run_spi_link_terminal_soak(args)
    if args.command == "spi-probe":
        return run_remote_spi_probe(
            args.ssh_target,
            args.remote_root,
            args.count or args.probe_count,
            args.spi_speed,
        )
    if args.command == "spi-command":
        return run_remote_spi_command(args.ssh_target, args.remote_root, args.text, args.spi_speed)
    if args.command == "spi-echo":
        return run_remote_spi_echo(args.ssh_target, args.remote_root, args.text, args.spi_speed)
    if args.command == "spi-data-echo":
        payload = args.text or args.payload
        echo_proc = start_remote_bridge_echo_client(
            args.bridge_peer_target,
            args.pico_net_host,
            args.pico_net_port,
        )
        try:
            return run_remote_spi_data(
                args.ssh_target,
                args.remote_root,
                payload,
                args.spi_speed,
                payload,
            )
        finally:
            stop_process(echo_proc)
    if args.command == "uart-to-spi":
        payload = args.text or args.payload
        rc = stage_remote_spi_tools(args.ssh_target)
        if rc != 0:
            return rc
        recv_proc = subprocess.Popen(
            build_ssh_command(
                args.ssh_target,
                (
                    f"cd {shlex.quote(REMOTE_SPI_STAGE)} && "
                    f"python3 test.py --verbose-raw --speed {args.spi_speed} recv --expect {shlex.quote(payload)}"
                ),
            )
        )
        time.sleep(0.2)
        try:
            rc = run_local_uart_send(args.local_python, args.uart_port, args.uart_speed, payload)
            if rc != 0:
                return rc
            return recv_proc.wait(timeout=10.0)
        finally:
            stop_process(recv_proc)
    if args.command == "spi-to-uart":
        payload = args.text or args.payload
        recv_proc = subprocess.Popen(
            [args.local_python, str(UART_TEST), "--port", args.uart_port, "--speed", str(args.uart_speed), "recv", "--expect", payload]
        )
        time.sleep(0.2)
        try:
            rc = run_remote_spi_send(args.ssh_target, args.remote_root, payload, args.spi_speed)
            if rc != 0:
                return rc
            return recv_proc.wait(timeout=10.0)
        finally:
            stop_process(recv_proc)
    if args.command == "uart-probe":
        return run_local_uart_probe(args.local_python, args.uart_port, args.uart_speed, args.probe_count)
    if args.command == "uart-command":
        return run_local_uart_command(args.local_python, args.uart_port, args.uart_speed, args.text)
    if args.command == "uart-data-echo":
        payload = args.text or args.payload
        echo_proc = start_remote_echo_server(args.ssh_target, args.remote_bind, args.remote_port)
        try:
            return run_local_uart_data(args.local_python, args.uart_port, args.uart_speed, payload, payload)
        finally:
            stop_process(echo_proc)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
