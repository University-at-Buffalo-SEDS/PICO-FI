#!/usr/bin/env python3

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path
import argparse


ROOT = Path(__file__).resolve().parent
TARGET = "thumbv6m-none-eabi"
BIN_NAME = "pico-fi"
ROLE_CONFIG = {
    "server": "pico-fi-server.json",
    "client": "pico-fi-client.json",
}


def main() -> int:
    args = parse_args(sys.argv[1:])
    cargo_args = args.cargo_args
    flash = args.flash
    probe = args.probe
    profile = "release" if "--release" in cargo_args else "debug"

    cargo = find_tool("cargo")
    if cargo is None:
        fail("`cargo` not found. Install Rust/Cargo first.")

    elf2uf2 = find_tool("elf2uf2-rs")
    if elf2uf2 is None:
        fail("`elf2uf2-rs` not found.\nInstall it with: cargo install elf2uf2-rs")

    env = os.environ.copy()
    config_path = resolve_config_path(args)
    if config_path is not None:
        env["PICO_FI_CONFIG"] = config_path
        env["CARGO_TARGET_DIR"] = str(ROOT / "target" / config_target_dir(config_path))
        print(f"Config: {config_path}")

    run([cargo, "build", *cargo_args], cwd=ROOT, env=env)

    target_root = Path(env.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    elf = target_root / TARGET / profile / BIN_NAME
    uf2 = elf.with_suffix(".uf2")

    if not elf.is_file():
        fail(f"expected ELF was not created: {elf}")

    run([elf2uf2, str(elf), str(uf2)], cwd=ROOT)

    print(f"ELF: {elf}")
    print(f"UF2: {uf2}")

    if flash:
        flash_with_probe(elf, probe)

    return 0


def run(cmd: list[str], cwd: Path, env: dict[str, str] | None = None) -> None:
    try:
        subprocess.run(cmd, cwd=cwd, env=env, check=True)
    except subprocess.CalledProcessError as exc:
        raise SystemExit(exc.returncode) from exc


def find_tool(name: str) -> str | None:
    direct = shutil.which(name)
    if direct:
        return direct

    cargo_home = os.environ.get("CARGO_HOME")
    home = os.environ.get("HOME")
    candidates = []
    if cargo_home:
        candidates.append(Path(cargo_home) / "bin" / name)
    if home:
        candidates.append(Path(home) / ".cargo" / "bin" / name)

    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    return None


def flash_with_probe(elf: Path, probe: str | None) -> None:
    probe_rs = find_tool("probe-rs")
    if probe_rs is None:
        fail(
            "`probe-rs` not found.\n"
            "Install it with: cargo install probe-rs --features cli"
        )

    cmd = [probe_rs, "download", str(elf), "--chip", "RP2040"]
    if probe:
        cmd.extend(["--probe", probe])

    run(cmd, cwd=ROOT)

    reset_cmd = [probe_rs, "reset", "--chip", "RP2040"]
    if probe:
        reset_cmd.extend(["--probe", probe])

    run(reset_cmd, cwd=ROOT)
    print("Flashed over SWD with probe-rs.")


def parse_args(args: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build the firmware and optionally flash it."
    )
    parser.add_argument("--flash", action="store_true", help="Flash the built ELF with probe-rs.")
    parser.add_argument("--probe", help="Specific probe selector, for example 2e8a:000c.")
    parser.add_argument("--role", choices=sorted(ROLE_CONFIG), help="Build using the named board profile.")
    parser.add_argument("--config", help="Build using an explicit JSON config file.")
    parsed, cargo_args = parser.parse_known_args(args)
    parsed.cargo_args = cargo_args
    if parsed.role and parsed.config:
        fail("use either --role or --config, not both")
    return parsed


def resolve_config_path(args: argparse.Namespace) -> str | None:
    if args.config:
        return args.config
    if args.role:
        return ROLE_CONFIG[args.role]
    return os.environ.get("PICO_FI_CONFIG")


def config_target_dir(config_path: str) -> str:
    return Path(config_path).stem


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


if __name__ == "__main__":
    raise SystemExit(main())
