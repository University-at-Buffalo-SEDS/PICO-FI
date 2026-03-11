#!/usr/bin/env python3

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parent
TARGET = "thumbv6m-none-eabi"
BIN_NAME = "pico-fi"


def main() -> int:
    raw_args = sys.argv[1:]
    cargo_args, flash, probe = parse_args(raw_args)
    profile = "release" if "--release" in cargo_args else "debug"

    cargo = find_tool("cargo")
    if cargo is None:
        fail("`cargo` not found. Install Rust/Cargo first.")

    elf2uf2 = find_tool("elf2uf2-rs")
    if elf2uf2 is None:
        fail("`elf2uf2-rs` not found.\nInstall it with: cargo install elf2uf2-rs")

    run([cargo, "build", *cargo_args], cwd=ROOT)

    elf = ROOT / "target" / TARGET / profile / BIN_NAME
    uf2 = elf.with_suffix(".uf2")

    if not elf.is_file():
        fail(f"expected ELF was not created: {elf}")

    run([elf2uf2, str(elf), str(uf2)], cwd=ROOT)

    print(f"ELF: {elf}")
    print(f"UF2: {uf2}")

    if flash:
        flash_with_probe(elf, probe)

    return 0


def run(cmd: list[str], cwd: Path) -> None:
    try:
        subprocess.run(cmd, cwd=cwd, check=True)
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


def parse_args(args: list[str]) -> tuple[list[str], bool, str | None]:
    cargo_args: list[str] = []
    flash = False
    probe: str | None = None

    idx = 0
    while idx < len(args):
        arg = args[idx]
        if arg == "--flash":
            flash = True
            idx += 1
            continue
        if arg == "--probe":
            if idx + 1 >= len(args):
                fail("`--probe` requires a value, for example: --probe 2e8a:000c")
            probe = args[idx + 1]
            idx += 2
            continue
        cargo_args.append(arg)
        idx += 1

    return cargo_args, flash, probe


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


if __name__ == "__main__":
    raise SystemExit(main())
