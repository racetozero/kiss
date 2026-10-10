"""Compare CPU time for idle baseline and candidate terminals."""

from __future__ import annotations

import argparse
import ctypes
import json
import os
import select
import struct
import subprocess
import sys
import time
from pathlib import Path
from typing import TypedDict

from benchmark_kiss_harness import start, stop, wait_for


class IdleResult(TypedDict):
    sessions_per_version: int
    duration_seconds: float
    baseline_cpu_seconds: float
    candidate_cpu_seconds: float


def cpu_seconds(pid: int) -> float:
    if sys.platform.startswith('linux'):
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')
    if sys.platform == 'darwin':
        library = ctypes.CDLL('/usr/lib/libproc.dylib')
        data = ctypes.create_string_buffer(96)
        if library.proc_pidinfo(pid, 4, 0, data, len(data)) != len(data):
            raise RuntimeError(
                f'CPU information for process {pid} is unavailable. Run this benchmark as the user who started KISS.'
            )
        fields = struct.unpack('=6Q12i', data.raw)
        timebase = ctypes.create_string_buffer(8)
        library.mach_timebase_info(timebase)
        numerator, denominator = struct.unpack('=2I', timebase.raw)
        return (fields[2] + fields[3]) * numerator / denominator / 1_000_000_000
    value = subprocess.check_output(
        ['ps', '-o', 'time=', '-p', str(pid)], text=True
    ).strip()
    seconds = 0.0
    for part in value.split(':'):
        seconds = seconds * 60 + float(part)
    return seconds


def benchmark(baseline: Path, candidate: Path, duration: float) -> list[IdleResult]:
    results: list[IdleResult] = []
    arguments = (
        '--no-approve',
        '--provider',
        'anthropic',
        '--model',
        'anthropic/claude-sonnet-4-5',
        '--api-key',
        'local-idle-test',
    )
    for count in [1, 10]:
        baseline_sessions = [start(baseline, arguments) for _ in range(count)]
        candidate_sessions = [start(candidate, arguments) for _ in range(count)]
        try:
            for process, master in baseline_sessions + candidate_sessions:
                wait_for(master, b'$ skills', time.perf_counter(), 30)
            masters = [master for _, master in baseline_sessions + candidate_sessions]
            # Drain terminal output so a full PTY cannot stop KISS in a write.
            for index, interval in enumerate((3, duration)):
                if index == 1:
                    baseline_before = sum(
                        cpu_seconds(process.pid) for process, _ in baseline_sessions
                    )
                    candidate_before = sum(
                        cpu_seconds(process.pid) for process, _ in candidate_sessions
                    )
                deadline = time.perf_counter() + interval
                while (remaining := deadline - time.perf_counter()) > 0:
                    readable, _, _ = select.select(masters, [], [], min(remaining, 0.1))
                    for master in readable:
                        os.read(master, 65_536)
            result: IdleResult = {
                'sessions_per_version': count,
                'duration_seconds': duration,
                'baseline_cpu_seconds': sum(
                    cpu_seconds(process.pid) for process, _ in baseline_sessions
                )
                - baseline_before,
                'candidate_cpu_seconds': sum(
                    cpu_seconds(process.pid) for process, _ in candidate_sessions
                )
                - candidate_before,
            }
            results.append(result)
            print(json.dumps(result), flush=True)
        finally:
            for process, master in baseline_sessions + candidate_sessions:
                stop(process, master)
    return results


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--candidate', type=Path, required=True)
    parser.add_argument('--duration', type=float, default=60)
    parser.add_argument('--json', type=Path)
    arguments = parser.parse_args()
    if arguments.duration <= 0:
        parser.error(
            'The duration must be greater than zero. Use --duration 60 for a one-minute idle sample.'
        )
    results = benchmark(
        arguments.baseline.resolve(), arguments.candidate.resolve(), arguments.duration
    )
    if arguments.json:
        arguments.json.write_text(json.dumps(results, indent=2) + '\n')
