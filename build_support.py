"""Shared build tools and verified RP2040 flashing for both CLI entry points."""
from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent
CHIP = "RP2040"
# Qualified on the gateway client board; the faster probe default produced
# secondary debug-port cleanup failures after reset.
DEFAULT_SWD_SPEED = "100"


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


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


def run_probe_command(cmd: list[str], *, cleanup_report: list[str] | None = None) -> bool:
    """Stream probe diagnostics and preserve failure status; return diagnostics seen."""
    diagnostic = False
    try:
        with subprocess.Popen(
            cmd, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, bufsize=1,
        ) as process:
            assert process.stdout is not None
            for line in process.stdout:
                print(line, end="", flush=True)
                plain = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", line)
                diagnostic |= bool(re.search(r"\b(?:warn(?:ing)?|error|failed|failure)\b", plain, re.I))
                if cleanup_report is not None and re.search(
                    r"could not clear all hardware breakpoints|failed to deconfigure device during shutdown",
                    plain, re.I,
                ):
                    cleanup_report.append(plain.rstrip())
            code = process.wait()
    except OSError as exc:
        fail(f"could not run probe-rs: {exc}")
    if code:
        raise SystemExit(code)
    return diagnostic


def flash_with_probe(elf: Path, probe: str | None, speed: int | None = None) -> None:
    probe_rs = find_tool("probe-rs")
    if probe_rs is None:
        fail("`probe-rs` not found. Install it first.")

    print(f"Using probe-rs executable: {probe_rs}", flush=True)
    run([probe_rs, "--version"], cwd=ROOT)
    options = ["--chip", CHIP, "--protocol", "swd", "--non-interactive"]
    if probe:
        options.extend(["--probe", probe])
    if speed is not None:
        options.extend(["--speed", str(speed)])
    cleanup: list[str] = []
    # Keep verification and reset on the same connection to the target.
    diagnostic = run_probe_command(
        [probe_rs, "download", str(elf), *options, "--verify", "--reset"],
        cleanup_report=cleanup,
    )
    if cleanup:
        print("Verified download and reset command returned success, but probe-rs reported "
              "a debug-session cleanup failure. Target execution remains unverified.")
    elif diagnostic:
        print("Verified download and reset command returned success, but probe-rs emitted "
              "diagnostics above. Do not treat this as a clean connection or runtime check.")
    else:
        print("ELF download verified; reset command returned success.")
    print("Firmware startup and bridge connectivity have not been checked.")


def positive_speed(value: str) -> int:
    try:
        speed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("SWD speed must be a positive integer in kHz") from exc
    if speed <= 0:
        raise argparse.ArgumentTypeError("SWD speed must be a positive integer in kHz")
    return speed


