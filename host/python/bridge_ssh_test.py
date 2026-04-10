#!/usr/bin/env python3
"""SSH-driven SPI/UART bridge test harness."""

from __future__ import annotations

import argparse
import shlex
import signal
import subprocess
import sys
import time
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
UART_TEST = REPO_ROOT / "host/python/uart/test.py"
SPI_TEST_DIR = REPO_ROOT / "host/python/spi"
REMOTE_SPI_STAGE = "/tmp/pico-fi-spi-test"
LINK_HANDSHAKE_MAGIC = b"PICOFI1"


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

    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("full-smoke", help="Run SPI probe, UART probe/command, and UART data echo test")

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
