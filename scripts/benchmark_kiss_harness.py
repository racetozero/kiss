"""Measure KISS interactive startup and idle memory."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import platform
import pty
import select
import signal
import statistics
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path


def start(binary: Path) -> tuple[subprocess.Popen[bytes], int]:
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    environment = os.environ.copy()
    environment.setdefault("TERM", "xterm-256color")
    process = subprocess.Popen(
        [str(binary), "--no-session"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=environment,
        start_new_session=True,
        close_fds=True,
    )
    os.close(slave)
    return process, master


def stop(process: subprocess.Popen[bytes], master: int) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=2)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    os.close(master)


def wait_for(master: int, needle: bytes, started: float, timeout: float = 5) -> float:
    data = bytearray()
    deadline = started + timeout
    while time.perf_counter() < deadline:
        readable, _, _ = select.select([master], [], [], 0.05)
        if not readable:
            continue
        try:
            data.extend(os.read(master, 65_536))
        except OSError:
            break
        if needle in data:
            return (time.perf_counter() - started) * 1_000
    raise RuntimeError(f"timed out waiting for {needle!r}")


def launch_sample(binary: Path, heading: bytes) -> tuple[float, float]:
    started = time.perf_counter()
    process, master = start(binary)
    try:
        first_frame = wait_for(master, heading, started)
        probe = b"kiss-startup-probe"
        os.write(master, probe)
        first_input = wait_for(master, probe, started)
        return first_frame, first_input
    finally:
        stop(process, master)


def linux_pss_kib(pid: int) -> int:
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
        if line.startswith("Pss:"):
            return int(line.split()[1])
    raise RuntimeError(f"Pss is missing for process {pid}")


def memory_kib(pid: int) -> int:
    if sys.platform.startswith("linux"):
        return linux_pss_kib(pid)
    if sys.platform == "darwin":
        return int(
            subprocess.check_output(
                ["ps", "-o", "rss=", "-p", str(pid)], text=True
            ).strip()
        )
    raise RuntimeError("memory measurement supports Linux and macOS")


def memory_sample(binary: Path, heading: bytes, count: int) -> float:
    sessions = [start(binary) for _ in range(count)]
    try:
        for _, master in sessions:
            wait_for(master, heading, time.perf_counter())
        time.sleep(1)
        return sum(memory_kib(process.pid) for process, _ in sessions) / 1_024
    finally:
        for process, master in sessions:
            stop(process, master)


def benchmark(binary: Path, launches: int, memory_trials: int) -> dict[str, object]:
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    heading = version.replace("kiss ", "kiss v", 1).encode()
    launch_sample(binary, heading)
    timings = [launch_sample(binary, heading) for _ in range(launches)]
    one_session = [
        memory_sample(binary, heading, 1) for _ in range(memory_trials)
    ]
    ten_sessions = [
        memory_sample(binary, heading, 10) for _ in range(memory_trials)
    ]
    one_mean = statistics.fmean(one_session)
    ten_mean = statistics.fmean(ten_sessions)
    memory_kind = "PSS" if sys.platform.startswith("linux") else "RSS"
    return {
        "binary": str(binary),
        "version": version,
        "host": platform.platform(),
        "launches": launches,
        "memory_trials": memory_trials,
        "first_frame_ms_mean": statistics.fmean(value[0] for value in timings),
        "first_input_ms_mean": statistics.fmean(value[1] for value in timings),
        "memory_kind": memory_kind,
        "one_session_mib_mean": one_mean,
        "ten_sessions_mib_mean": ten_mean,
        "extra_mib_per_session": (ten_mean - one_mean) / 9,
    }


def print_report(result: dict[str, object]) -> None:
    print(f"Host: {result['host']}")
    print(f"Binary: {result['version']}")
    print("measure\tmean")
    print(f"first frame\t{result['first_frame_ms_mean']:.3f} ms")
    print(f"first input\t{result['first_input_ms_mean']:.3f} ms")
    kind = result["memory_kind"]
    print(f"one session {kind}\t{result['one_session_mib_mean']:.3f} MiB")
    print(f"ten sessions {kind}\t{result['ten_sessions_mib_mean']:.3f} MiB")
    print(f"extra {kind} per session\t{result['extra_mib_per_session']:.3f} MiB")


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/kiss"))
    parser.add_argument("--launches", type=int, default=10)
    parser.add_argument("--memory-trials", type=int, default=3)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    if min(args.launches, args.memory_trials) < 1:
        parser.error("launch and memory trial counts must be positive")
    args.binary = args.binary.resolve()
    if not args.binary.is_file():
        parser.error(f"KISS executable not found: {args.binary}")
    return args


if __name__ == "__main__":
    try:
        arguments = parse_arguments()
        result = benchmark(
            arguments.binary, arguments.launches, arguments.memory_trials
        )
        print_report(result)
        if arguments.json:
            arguments.json.parent.mkdir(parents=True, exist_ok=True)
            arguments.json.write_text(
                json.dumps(result, indent=2) + "\n", encoding="utf-8", newline="\n"
            )
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
