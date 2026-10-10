"""Measure visible KISS startup, typing, installed CLI bytes and process-tree RSS.

Run with uv run --no-project --with pyte==0.8.2 python scripts/benchmark_kiss_startup.py.
Cold means a fresh project and session directory, not a cold OS file cache.
"""

from __future__ import annotations

import argparse
import codecs
import fcntl
import hashlib
import json
import math
import os
import platform
import pty
import select
import signal
import statistics
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path
from typing import TypedDict

import pyte


class Launch(TypedDict):
    first_visible_ms: float
    time_to_type_ms: float


class Memory(TypedDict):
    rss_mb: float
    process_count: int


class Distribution(TypedDict):
    samples: int
    mean: float
    stdev: float
    median: float
    p95: float
    p99: float
    minimum: float
    maximum: float


class Results(TypedDict):
    binary: str
    binary_sha256: str
    version: str
    host: str
    screen: str
    method: str
    variation: str
    first_visible_cold_ms: Distribution
    first_visible_warm_ms: Distribution
    time_to_type_cold_ms: Distribution
    time_to_type_warm_ms: Distribution
    startup_tree_rss_mb: Distribution
    installed_cli_payload_mb: float
    installed_size_scope: str
    cold_samples: list[Launch]
    warm_samples: list[Launch]
    memory_samples: list[Memory]


def distribution(values: list[float]) -> Distribution:
    ordered = sorted(values)
    return {
        'samples': len(values),
        'mean': statistics.fmean(values),
        'stdev': statistics.stdev(values) if len(values) > 1 else 0.0,
        'median': statistics.median(values),
        'p95': ordered[math.ceil(len(values) * 0.95) - 1],
        'p99': ordered[math.ceil(len(values) * 0.99) - 1],
        'minimum': ordered[0],
        'maximum': ordered[-1],
    }


def launch(
    binary: Path, root: Path, *, memory: bool = False
) -> tuple[Launch, Memory | None]:
    project = root / 'project'
    project.mkdir(exist_ok=True)
    models = root / 'models.json'
    models.write_text('{}\n')
    environment = os.environ.copy()
    environment.update(
        {
            'TERM': 'xterm-256color',
            'KISS_MODELS_FILE': str(models),
            'KISS_SESSION_DIR': str(root / 'sessions'),
        }
    )
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 160, 0, 0))
    screen = pyte.Screen(160, 40)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder('utf-8')(errors='replace')
    started = time.perf_counter_ns()
    process = subprocess.Popen(
        [
            str(binary),
            '--provider',
            'anthropic',
            '--api-key',
            'local-benchmark-key',
            '--no-context-files',
        ],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        cwd=project,
        env=environment,
        start_new_session=True,
    )
    os.close(slave)
    first_visible: float | None = None
    timing: Launch | None = None
    probe = 'kiss-visible-type-probe'
    try:
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if not select.select([master], [], [], 0.05)[0]:
                continue
            stream.feed(decoder.decode(os.read(master, 65_536)))
            now = time.perf_counter_ns()
            # pyte stores private terminal modes shifted by five bits.
            # A synchronized frame is visible only after mode 2026 is reset.
            if (2026 << 5) in screen.mode:
                continue
            if first_visible is None and any(line.strip() for line in screen.display):
                first_visible = (now - started) / 1_000_000
                # A terminal echo is not proof that the app accepts input.
                if termios.tcgetattr(master)[3] & termios.ECHO:
                    raise RuntimeError(
                        'KISS showed output with terminal echo enabled. Use an interactive release executable and repeat the run.'
                    )
                os.write(master, probe.encode())
            if first_visible is not None and any(
                probe in line for line in screen.display
            ):
                timing = {
                    'first_visible_ms': first_visible,
                    'time_to_type_ms': (now - started) / 1_000_000,
                }
                break
        if timing is None:
            raise RuntimeError(
                'KISS did not show the typing probe within 10 seconds. Use an interactive release executable with the built-in Anthropic provider and repeat the run.'
            )
        if not memory:
            return timing, None
        # Keep reading so an unread terminal cannot block startup work.
        deadline = time.monotonic() + 1
        while (remaining := deadline - time.monotonic()) > 0:
            if select.select([master], [], [], min(remaining, 0.05))[0]:
                stream.feed(decoder.decode(os.read(master, 65_536)))
        if process.poll() is not None:
            raise RuntimeError(
                'KISS exited before the memory sample. Use an interactive release executable and repeat the run.'
            )
        table = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss='], text=True)
        rows = [
            tuple(map(int, line.split())) for line in table.splitlines() if line.strip()
        ]
        tree = {process.pid}
        while children := {pid for pid, parent, _ in rows if parent in tree} - tree:
            tree.update(children)
        rss_kib = sum(rss for pid, _, rss in rows if pid in tree)
        if rss_kib == 0:
            raise RuntimeError(
                'ps returned no resident memory for KISS. Run on macOS or Linux with process inspection available and repeat the run.'
            )
        return timing, {
            'rss_mb': rss_kib * 1024 / 1_000_000,
            'process_count': len(tree),
        }
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=2)
        os.close(master)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/kiss'))
    parser.add_argument('--samples', type=int, default=1000)
    parser.add_argument('--memory-trials', type=int, default=20)
    parser.add_argument('--json', type=Path, required=True)
    args = parser.parse_args()
    if args.samples < 1 or args.memory_trials < 1:
        parser.error(
            'Sample counts must be positive integers. Set --samples 1000 and --memory-trials 20.'
        )
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(
            f'KISS executable was not found at {binary}. Build with cargo build --release -p kiss and set --binary to that executable.'
        )
    cold: list[Launch] = []
    warm: list[Launch] = []
    memory: list[Memory] = []
    with tempfile.TemporaryDirectory(prefix='kiss-startup-warm-') as directory:
        warm_root = Path(directory)
        launch(binary, warm_root)
        for index in range(args.samples):
            # Alternate order to reduce bias from changes in host load.
            with tempfile.TemporaryDirectory(prefix='kiss-startup-cold-') as fresh:
                if index % 2:
                    warm.append(launch(binary, warm_root)[0])
                    cold.append(launch(binary, Path(fresh))[0])
                else:
                    cold.append(launch(binary, Path(fresh))[0])
                    warm.append(launch(binary, warm_root)[0])
            if (index + 1) % 100 == 0:
                print(f'Completed {index + 1} cold and warm samples', flush=True)
        for _ in range(args.memory_trials):
            _, sample = launch(binary, warm_root, memory=True)
            assert sample is not None
            memory.append(sample)
    result: Results = {
        'binary': str(binary),
        'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'version': subprocess.check_output(
            [str(binary), '--version'], text=True
        ).strip(),
        'host': platform.platform(),
        'screen': '160 columns by 40 rows; pyte 0.8.2',
        'method': 'Clock before Popen; visible text and probe in screen cells after synchronized output ends; terminal echo disabled; no model turn. Cold: fresh project/session directories. Warm: same directories after warmup. OS caches are not purged. User global config is read. Explicit built-in Anthropic provider, local placeholder key, empty custom-model fixture and no context files.',
        'variation': 'sample standard deviation; p95/p99 nearest rank',
        'first_visible_cold_ms': distribution(
            [row['first_visible_ms'] for row in cold]
        ),
        'first_visible_warm_ms': distribution(
            [row['first_visible_ms'] for row in warm]
        ),
        'time_to_type_cold_ms': distribution([row['time_to_type_ms'] for row in cold]),
        'time_to_type_warm_ms': distribution([row['time_to_type_ms'] for row in warm]),
        'startup_tree_rss_mb': distribution([row['rss_mb'] for row in memory]),
        'installed_cli_payload_mb': binary.stat().st_size / 1_000_000,
        'installed_size_scope': 'Single CLI executable; no separate runtime or updater. Excludes build files, SDK packages, generated sessions and OS libraries.',
        'cold_samples': cold,
        'warm_samples': warm,
        'memory_samples': memory,
    }
    args.json.parent.mkdir(parents=True, exist_ok=True)
    args.json.write_text(json.dumps(result, indent=2) + '\n')
    print(f'first_visible_cold_ms: {result["first_visible_cold_ms"]}')
    print(f'time_to_type_cold_ms: {result["time_to_type_cold_ms"]}')
    print(f'time_to_type_warm_ms: {result["time_to_type_warm_ms"]}')
    print(f'startup_tree_rss_mb: {result["startup_tree_rss_mb"]}')
    print(f'installed_cli_payload_mb: {result["installed_cli_payload_mb"]:.6f}')


if __name__ == '__main__':
    main()
