#!/usr/bin/env python3

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path

import sys

ROOT = Path(__file__).resolve().parent
TARGET = "thumbv6m-none-eabi"
BIN_NAME = "pico-fi"
CHIP = "RP2040"

ROLE_CONFIG = {
    "server": "pico-fi-server.json",
    "client": "pico-fi-client.json",
}


def run(cmd: list[str], *, cwd: Path, env: dict[str, str] | None = None) -> None:
    try:
        subprocess.run(cmd, cwd=cwd, env=env, check=True)
    except subprocess.CalledProcessError as exc:
        raise SystemExit(exc.returncode) from exc


def find_tool(name: str) -> str | None:
    direct = shutil.which(name)
    if direct:
        return direct

    cargo_home = os.environ.get("CARGO_HOME")
    home = os.environ.get("HOME") or os.environ.get("USERPROFILE")
    candidates: list[Path] = []
    if cargo_home:
        candidates.append(Path(cargo_home) / "bin" / name)
        if os.name == "nt":
            candidates.append(Path(cargo_home) / "bin" / f"{name}.exe")
    if home:
        candidates.append(Path(home) / ".cargo" / "bin" / name)
        if os.name == "nt":
            candidates.append(Path(home) / ".cargo" / "bin" / f"{name}.exe")

    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def flash_with_probe(elf: Path, probe: str | None) -> None:
    probe_rs = find_tool("probe-rs")
    if probe_rs is None:
        fail("`probe-rs` not found. Install it first.")

    download_cmd = [probe_rs, "download", str(elf), "--chip", CHIP]
    reset_cmd = [probe_rs, "reset", "--chip", CHIP]
    if probe:
        download_cmd.extend(["--probe", probe])
        reset_cmd.extend(["--probe", probe])

    run(download_cmd, cwd=ROOT)
    run(reset_cmd, cwd=ROOT)
    print("Flashed over SWD with probe-rs.")


def build_role(role: str, *, release: bool, flash: bool, probe: str | None) -> int:
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
        flash_with_probe(elf, probe)

    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and optionally flash the server or client firmware."
    )
    parser.add_argument("role", choices=sorted(ROLE_CONFIG))
    parser.add_argument("--release", action="store_true", help="Build release firmware instead of debug.")
    parser.add_argument("--flash", action="store_true", help="Flash the built ELF with probe-rs.")
    parser.add_argument("--probe", help="Specific probe selector, for example 2e8a:000c:E664....")
    return parser.parse_args(argv)


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    return build_role(args.role, release=args.release, flash=args.flash, probe=args.probe)


if __name__ == "__main__":
    raise SystemExit(main())
