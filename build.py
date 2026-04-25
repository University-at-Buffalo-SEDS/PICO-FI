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
FUZZ_SCRIPT = ROOT / "host" / "python" / "fuzz_bridge_stability.py"
ROLE_CONFIG = {
    "server": "pico-fi-server.json",
    "client": "pico-fi-client.json",
}


def main() -> int:
    args = parse_args(sys.argv[1:])
    cargo_args = args.cargo_args
    run_tests = args.test
    run_fuzz_tests = args.test_fuzz
    flash = args.flash
    probe = args.probe

    cargo = find_tool("cargo")
    if cargo is None:
        fail("`cargo` not found. Install Rust/Cargo first.")

    if run_tests and run_fuzz_tests:
        fail("use either --test or --test-fuzz, not both")

    if run_tests:
        validate_test_args(args, cargo_args, mode="--test")
        print("[1/1] Running host-side framing and soak tests...", flush=True)
        run([cargo, "host-test"], cwd=ROOT)
        print("[done] Host-side tests passed.", flush=True)
        return 0

    if run_fuzz_tests:
        validate_test_args(args, cargo_args, mode="--test-fuzz")
        print("[1/2] Running host-side framing and soak tests...", flush=True)
        run([cargo, "host-test"], cwd=ROOT)
        print("[2/2] Running long fuzz/soak validation...", flush=True)
        print(
            f"      duration={args.fuzz_duration_s:.1f}s status_interval={args.fuzz_status_s:.1f}s seed={args.fuzz_seed}",
            flush=True,
        )
        python = find_repo_python()
        env = os.environ.copy()
        env["PYTHONUNBUFFERED"] = "1"
        run(
            [
                python,
                str(FUZZ_SCRIPT),
                "--duration-s",
                str(args.fuzz_duration_s),
                "--status-s",
                str(args.fuzz_status_s),
                "--seed",
                str(args.fuzz_seed),
            ],
            cwd=ROOT,
            env=env,
        )
        print("[done] Fuzz/soak validation passed.", flush=True)
        return 0

    profile = "release" if "--release" in cargo_args else "debug"

    elf2uf2 = find_tool("elf2uf2-rs")
    if elf2uf2 is None:
        fail("`elf2uf2-rs` not found.\nInstall it with: cargo install elf2uf2-rs")

    env = os.environ.copy()
    config_path = resolve_config_path(args)
    if config_path is not None:
        env["PICO_FI_CONFIG"] = config_path
        env["CARGO_TARGET_DIR"] = str(ROOT / "target" / config_target_dir(config_path))
        print(f"Config: {config_path}", flush=True)

    print("[build] Running cargo build...", flush=True)
    run([cargo, "build", *cargo_args], cwd=ROOT, env=env)

    target_root = Path(env.get("CARGO_TARGET_DIR", str(ROOT / "target")))
    elf = target_root / TARGET / profile / BIN_NAME
    uf2 = elf.with_suffix(".uf2")

    if not elf.is_file():
        fail(f"expected ELF was not created: {elf}")

    print("[build] Converting ELF to UF2...", flush=True)
    run([elf2uf2, str(elf), str(uf2)], cwd=ROOT)

    print(f"ELF: {elf}", flush=True)
    print(f"UF2: {uf2}", flush=True)

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


def find_repo_python() -> str:
    candidates = [
        ROOT / "venv" / "bin" / "python",
        ROOT / ".venv" / "bin" / "python",
    ]
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)
    if sys.executable:
        return sys.executable
    python = find_tool("python3")
    if python:
        return python
    fail("python3 not found. Install Python first.")
    raise AssertionError("unreachable")


def validate_test_args(args: argparse.Namespace, cargo_args: list[str], mode: str) -> None:
    if args.flash or args.probe or args.role or args.config or cargo_args:
        fail(f"{mode} cannot be combined with build/flash/config arguments")


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
        description="Build the firmware, flash it, or run host-side software tests."
    )
    parser.add_argument("--test", action="store_true", help="Run host-side software framing and soak tests.")
    parser.add_argument(
        "--test-fuzz",
        action="store_true",
        help="Run host-side framing tests, then a long fuzz/soak validation with good and bad data.",
    )
    parser.add_argument(
        "--fuzz-duration-s",
        type=float,
        default=600.0,
        help="Duration for --test-fuzz in seconds. Default: 600.",
    )
    parser.add_argument(
        "--fuzz-status-s",
        type=float,
        default=30.0,
        help="Status print interval for --test-fuzz in seconds. Default: 30.",
    )
    parser.add_argument(
        "--fuzz-seed",
        type=int,
        default=0xC0FFEE,
        help="Deterministic RNG seed for --test-fuzz.",
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
