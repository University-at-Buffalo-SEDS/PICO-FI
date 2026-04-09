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


def run_remote_spi_probe(target: str, remote_root: str, count: int, speed: int) -> int:
    remote_root_expr = shell_quote_remote_path(remote_root)
    remote = (
        f"cd {remote_root_expr}/host/python/spi && "
        f"python3 test.py --verbose-raw probe --count {count} --speed {speed}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_remote_spi_command(target: str, remote_root: str, text: str, speed: int) -> int:
    remote_root_expr = shell_quote_remote_path(remote_root)
    remote = (
        f"cd {remote_root_expr}/host/python/spi && "
        f"python3 test.py --verbose-raw command {shlex.quote(text)} --speed {speed}"
    )
    return run_checked(build_ssh_command(target, remote))


def run_local_uart_probe(port: str, speed: int, count: int) -> int:
    return run_checked(
        [sys.executable, str(UART_TEST), "--port", port, "--speed", str(speed), "probe", "--count", str(count)]
    )


def run_local_uart_command(port: str, speed: int, text: str) -> int:
    return run_checked(
        [sys.executable, str(UART_TEST), "--port", port, "--speed", str(speed), "command", text]
    )


def run_local_uart_data(port: str, speed: int, text: str, expect: str) -> int:
    cmd = [sys.executable, str(UART_TEST), "--port", port, "--speed", str(speed), "data", text]
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
    proc = subprocess.Popen(
        ["ssh", target, "python3", "-u", "-c", server_code],
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
    rc = run_local_uart_probe(args.uart_port, args.uart_speed, args.probe_count)
    if rc != 0:
        return rc
    rc = run_local_uart_command(args.uart_port, args.uart_speed, "/ping")
    if rc != 0:
        return rc
    echo_proc = start_remote_echo_server(args.ssh_target, args.remote_bind, args.remote_port)
    try:
        return run_local_uart_data(args.uart_port, args.uart_speed, args.payload, args.payload)
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
    parser.add_argument("--uart-speed", type=int, default=115200)
    parser.add_argument("--spi-speed", type=int, default=100000)
    parser.add_argument("--probe-count", type=int, default=3)
    parser.add_argument("--remote-bind", default="10.8.0.5")
    parser.add_argument("--remote-port", type=int, default=4242)
    parser.add_argument("--payload", default="bridge-echo")

    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("full-smoke", help="Run SPI probe, UART probe/command, and UART data echo test")

    spi_probe = subparsers.add_parser("spi-probe", help="Run the remote SPI probe via SSH")
    spi_probe.add_argument("--count", type=int, default=None)

    spi_command = subparsers.add_parser("spi-command", help="Run the remote SPI command via SSH")
    spi_command.add_argument("text")

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
    if args.command == "uart-probe":
        return run_local_uart_probe(args.uart_port, args.uart_speed, args.probe_count)
    if args.command == "uart-command":
        return run_local_uart_command(args.uart_port, args.uart_speed, args.text)
    if args.command == "uart-data-echo":
        payload = args.text or args.payload
        echo_proc = start_remote_echo_server(args.ssh_target, args.remote_bind, args.remote_port)
        try:
            return run_local_uart_data(args.uart_port, args.uart_speed, payload, payload)
        finally:
            stop_process(echo_proc)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
