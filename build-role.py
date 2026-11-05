#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
from pathlib import Path

import sys

from build_support import DEFAULT_SWD_SPEED, fail, find_tool, flash_with_probe, positive_speed, run

ROOT = Path(__file__).resolve().parent
TARGET = "thumbv6m-none-eabi"
BIN_NAME = "pico-fi"

ROLE_CONFIG = {
    "server": "pico-fi-server.json",
    "client": "pico-fi-client.json",
}


def build_role(role: str, *, release: bool, flash: bool, probe: str | None, speed: int | None = None) -> int:
    cargo = find_tool("cargo")
    if cargo is None:
        fail("`cargo` not found. Install Rust/Cargo first.")
    elf2uf2 = find_tool("elf2uf2-rs")
    if elf2uf2 is None:
        fail("`elf2uf2-rs` not found.\nInstall it with: cargo install elf2uf2-rs")

    role_config = os.environ.get("PICO_FI_CONFIG") or ROLE_CONFIG[role]
    target_dir = ROOT / "target" / role
    profile = "release" if release else "debug"
    elf = target_dir / TARGET / profile / BIN_NAME
    uf2 = elf.with_suffix(".uf2")

    env = os.environ.copy()
    role_config_path = Path(role_config)
    if not role_config_path.is_absolute():
        role_config_path = (ROOT / role_config_path).resolve()
    env["PICO_FI_CONFIG"] = str(role_config_path)
    env["CARGO_TARGET_DIR"] = str(target_dir)

    cmd = [cargo, "build", "--package", BIN_NAME, "--bin", BIN_NAME]
    if release:
        cmd.append("--release")
    run(cmd, cwd=ROOT, env=env)

    if not elf.is_file():
        fail(f"expected ELF was not created: {elf}")

    run([elf2uf2, str(elf), str(uf2)], cwd=ROOT)

    print(f"built {role} firmware:")
    print(f"ELF: {elf}")
    print(f"UF2: {uf2}")

    if flash:
        flash_with_probe(elf, probe, speed)

    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and optionally flash the server or client firmware."
    )
    parser.add_argument("role", choices=sorted(ROLE_CONFIG))
    parser.add_argument("--release", action="store_true", help="Build release firmware instead of debug.")
    parser.add_argument("--flash", action="store_true", help="Flash the built ELF with probe-rs.")
    parser.add_argument("--probe", default=os.environ.get("PROBE_RS_PROBE"),
                        help="Probe selector, for example 2e8a:000c:E664... (or PROBE_RS_PROBE).")
    parser.add_argument("--speed", type=positive_speed, default=os.environ.get("PROBE_RS_SPEED", DEFAULT_SWD_SPEED),
                        help="SWD clock speed in kHz (default: 100; or PROBE_RS_SPEED).")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    return build_role(args.role, release=args.release, flash=args.flash, probe=args.probe, speed=args.speed)


if __name__ == "__main__":
    raise SystemExit(main())
