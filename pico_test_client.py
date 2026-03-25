#!/usr/bin/env python3

from __future__ import annotations

import argparse
import socket
import sys
import threading
import tkinter as tk
from tkinter import messagebox, scrolledtext


DEFAULT_HOST = "192.168.7.2"
DEFAULT_PORT = 5000


def recv_available(sock: socket.socket) -> str:
    sock.settimeout(0.2)
    chunks: list[bytes] = []
    while True:
        try:
            data = sock.recv(4096)
        except TimeoutError:
            break
        if not data:
            break
        chunks.append(data)
        if len(data) < 4096:
            break
    return b"".join(chunks).decode("utf-8", errors="replace")


def send_command(sock: socket.socket, command: str) -> str:
    sock.sendall(command.encode("utf-8") + b"\n")
    return recv_available(sock)


def run_cli(host: str, port: int, commands: list[str]) -> int:
    with socket.create_connection((host, port), timeout=5) as sock:
        banner = recv_available(sock)
        if banner:
            print(banner, end="")

        for command in commands:
            print(f"> {command}")
            response = send_command(sock, command)
            print(response, end="" if response.endswith("\n") else "\n")
    return 0


class PicoTestGui:
    def __init__(self, root: tk.Tk, host: str, port: int) -> None:
        self.root = root
        self.root.title("Pico Test Client")
        self.sock: socket.socket | None = None
        self.sock_lock = threading.Lock()

        self.host_var = tk.StringVar(value=host)
        self.port_var = tk.StringVar(value=str(port))
        self.command_var = tk.StringVar()

        self._build_ui()
        self.root.protocol("WM_DELETE_WINDOW", self.on_close)

    def _build_ui(self) -> None:
        top = tk.Frame(self.root, padx=12, pady=12)
        top.pack(fill="both", expand=True)

        conn = tk.Frame(top)
        conn.pack(fill="x")

        tk.Label(conn, text="Host").grid(row=0, column=0, sticky="w")
        tk.Entry(conn, textvariable=self.host_var, width=18).grid(row=0, column=1, padx=(6, 12))
        tk.Label(conn, text="Port").grid(row=0, column=2, sticky="w")
        tk.Entry(conn, textvariable=self.port_var, width=8).grid(row=0, column=3, padx=(6, 12))
        tk.Button(conn, text="Connect", command=self.connect).grid(row=0, column=4, padx=(0, 6))
        tk.Button(conn, text="Disconnect", command=self.disconnect).grid(row=0, column=5)

        buttons = tk.Frame(top, pady=10)
        buttons.pack(fill="x")

        for idx, (label, command) in enumerate(
            [
                ("Ping", "ping"),
                ("LED On", "led on"),
                ("LED Off", "led off"),
                ("Toggle", "led toggle"),
                ("Status", "led status"),
                ("Blink 100", "led blink 100"),
                ("Help", "help"),
            ]
        ):
            tk.Button(
                buttons,
                text=label,
                width=12,
                command=lambda cmd=command: self.send_async(cmd),
            ).grid(row=0, column=idx, padx=2, pady=2)

        custom = tk.Frame(top)
        custom.pack(fill="x", pady=(0, 10))
        tk.Entry(custom, textvariable=self.command_var).pack(side="left", fill="x", expand=True)
        tk.Button(custom, text="Send", command=self.send_custom).pack(side="left", padx=(8, 0))

        self.log = scrolledtext.ScrolledText(top, width=100, height=24, state="disabled")
        self.log.pack(fill="both", expand=True)

    def append_log(self, text: str) -> None:
        self.log.configure(state="normal")
        self.log.insert("end", text)
        if not text.endswith("\n"):
            self.log.insert("end", "\n")
        self.log.see("end")
        self.log.configure(state="disabled")

    def connect(self) -> None:
        if self.sock is not None:
            self.append_log("already connected")
            return

        try:
            host = self.host_var.get().strip()
            port = int(self.port_var.get().strip())
            sock = socket.create_connection((host, port), timeout=5)
        except Exception as exc:
            messagebox.showerror("Connect failed", str(exc))
            return

        self.sock = sock
        self.append_log(f"connected to {host}:{port}")
        banner = recv_available(sock)
        if banner:
            self.append_log(banner.rstrip())

    def disconnect(self) -> None:
        with self.sock_lock:
            sock = self.sock
            self.sock = None
        if sock is not None:
            try:
                sock.close()
            except OSError:
                pass
            self.append_log("disconnected")

    def send_custom(self) -> None:
        command = self.command_var.get().strip()
        if not command:
            return
        self.command_var.set("")
        self.send_async(command)

    def send_async(self, command: str) -> None:
        thread = threading.Thread(target=self._send_worker, args=(command,), daemon=True)
        thread.start()

    def _send_worker(self, command: str) -> None:
        with self.sock_lock:
            sock = self.sock
        if sock is None:
            self.root.after(0, lambda: messagebox.showwarning("Not connected", "Connect to the Pico first."))
            return

        try:
            response = send_command(sock, command)
        except Exception as exc:
            self.root.after(0, lambda: self.append_log(f"> {command}\nerror: {exc}"))
            return

        def update() -> None:
            self.append_log(f"> {command}")
            self.append_log(response.rstrip() or "<no response>")

        self.root.after(0, update)

    def on_close(self) -> None:
        self.disconnect()
        self.root.destroy()


def main() -> int:
    parser = argparse.ArgumentParser(description="Talk to pico-fi test mode over TCP.")
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--cli", action="store_true", help="Run in CLI mode instead of GUI.")
    parser.add_argument("commands", nargs="*", help="Commands to send in CLI mode.")
    args = parser.parse_args()

    if args.cli:
        if not args.commands:
            parser.error("--cli requires at least one command")
        return run_cli(args.host, args.port, args.commands)

    root = tk.Tk()
    PicoTestGui(root, args.host, args.port)
    root.mainloop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
