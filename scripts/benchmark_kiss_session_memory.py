"""Measure retained CLI RSS for long histories and added local shell results."""

from __future__ import annotations

import argparse
import ctypes
import fcntl
import hashlib
import json
import os
import platform
import pty
import select
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path
from typing import NotRequired, TypedDict

from benchmark_kiss_harness import stop, wait_for


class Cost(TypedDict):
    input: float
    output: float
    cacheRead: float
    cacheWrite: float
    total: float


class Usage(TypedDict):
    input: int
    output: int
    cacheRead: int
    cacheWrite: int
    totalTokens: int
    cost: Cost


class TextBlock(TypedDict):
    type: str
    text: str


class Message(TypedDict):
    role: str
    timestamp: int
    content: NotRequired[str | list[TextBlock]]
    api: NotRequired[str]
    provider: NotRequired[str]
    model: NotRequired[str]
    usage: NotRequired[Usage]
    stopReason: NotRequired[str]
    command: NotRequired[str]
    output: NotRequired[str]
    exitCode: NotRequired[int]
    cancelled: NotRequired[bool]
    truncated: NotRequired[bool]


class Record(TypedDict):
    type: str
    id: str
    timestamp: str
    parentId: NotRequired[str | None]
    version: NotRequired[int]
    cwd: NotRequired[str]
    message: NotRequired[Message]
    summary: NotRequired[str]
    tokensBefore: NotRequired[int]
    firstKeptEntryId: NotRequired[str]


class MemorySample(TypedDict):
    rss_mib: float
    footprint_mib: float | None


class Trial(TypedDict):
    variant: NotRequired[str]
    entries: int
    compacted: bool
    trial: int
    history_file_bytes: int
    loaded_memory: list[MemorySample]
    ready_memory: list[MemorySample]
    updated_memory: list[MemorySample]
    idle_memory: list[MemorySample]
    saved_updates: int


def fixture(path: Path, cwd: Path, entries: int, text: str, compacted: bool) -> int:
    usage: Usage = {
        'input': 0,
        'output': 0,
        'cacheRead': 0,
        'cacheWrite': 0,
        'totalTokens': 0,
        'cost': {'input': 0, 'output': 0, 'cacheRead': 0, 'cacheWrite': 0, 'total': 0},
    }
    header: Record = {
        'type': 'session',
        'version': 3,
        'id': 'session-memory-benchmark',
        'timestamp': '2026-01-01T00:00:00Z',
        'cwd': str(cwd),
    }
    with path.open('w') as output:
        output.write(json.dumps(header) + '\n')
        for index in range(entries):
            message: Message = {'role': 'user', 'timestamp': index, 'content': text}
            if index % 3 == 1:
                message = {
                    'role': 'assistant',
                    'timestamp': index,
                    'content': [{'type': 'text', 'text': text}],
                    'api': 'anthropic-messages',
                    'provider': 'anthropic',
                    'model': 'claude-sonnet-4-5',
                    'usage': usage,
                    'stopReason': 'stop',
                }
            elif index % 3 == 2:
                message = {
                    'role': 'bashExecution',
                    'timestamp': index,
                    'command': 'fixture',
                    'output': text,
                    'exitCode': 0,
                    'cancelled': False,
                    'truncated': False,
                }
            entry: Record = {
                'type': 'message',
                'id': f'{index:08x}',
                'parentId': f'{index - 1:08x}' if index else None,
                'timestamp': '2026-01-01T00:00:00Z',
                'message': message,
            }
            output.write(json.dumps(entry) + '\n')
        if compacted:
            compaction: Record = {
                'type': 'compaction',
                'id': 'compact0',
                'parentId': f'{entries - 1:08x}',
                'timestamp': '2026-01-01T00:00:00Z',
                'summary': 'Older work is complete.',
                'tokensBefore': entries * len(text) // 4,
                'firstKeptEntryId': f'{max(0, entries - 32):08x}',
            }
            output.write(json.dumps(compaction) + '\n')
        marker: Record = {
            'type': 'message',
            'id': 'marker00',
            'parentId': 'compact0'
            if compacted
            else f'{entries - 1:08x}'
            if entries
            else None,
            'timestamp': '2026-01-01T00:00:00Z',
            'message': {
                'role': 'user',
                'timestamp': entries,
                'content': 'KISS_HISTORY_READY',
            },
        }
        output.write(json.dumps(marker) + '\n')
    return path.stat().st_size


def sample_memory(process: subprocess.Popen[bytes], master: int) -> list[MemorySample]:
    observations: list[MemorySample] = []
    library = (
        ctypes.CDLL('/usr/lib/libproc.dylib') if sys.platform == 'darwin' else None
    )
    for _ in range(3):
        deadline = time.monotonic() + 0.2
        while (remaining := deadline - time.monotonic()) > 0:
            if select.select([master], [], [], min(remaining, 0.05))[0]:
                data = os.read(master, 65_536)
                if b'warning: skipping malformed session line' in data:
                    raise RuntimeError(
                        'The fixture has invalid entries. Fix its session schema and repeat the benchmark.'
                    )
        if process.poll() is not None:
            raise RuntimeError(
                'KISS exited before the RSS sample. Use an interactive release binary and repeat the benchmark.'
            )
        rss = subprocess.check_output(
            ['ps', '-o', 'rss=', '-p', str(process.pid)], text=True
        )
        footprint: float | None = None
        if library is not None:
            data = ctypes.create_string_buffer(96)
            if library.proc_pid_rusage(process.pid, 0, ctypes.byref(data)) != 0:
                raise RuntimeError(
                    'The macOS memory counter is unavailable. Run this benchmark as the user who started KISS.'
                )
            # rusage_info_v0 has a 16-byte UUID and seven u64 fields before ri_phys_footprint.
            footprint = struct.unpack_from('=Q', data.raw, 72)[0] / (1024 * 1024)
        observations.append(
            {'rss_mib': int(rss.strip()) / 1024, 'footprint_mib': footprint}
        )
    return observations


def run(
    binary: Path, entries: int, compacted: bool, text: str, updates: int, trial: int
) -> Trial:
    with tempfile.TemporaryDirectory(prefix='kiss-memory-') as directory:
        root = Path(directory)
        history = root / 'sessions' / 'project' / 'history.jsonl'
        history.parent.mkdir(parents=True)
        file_bytes = fixture(history, root, entries, text, compacted)
        original_lines = entries + 2 + int(compacted)
        models = root / 'models.json'
        models.write_text('{"providers": {}}\n')
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
        process = subprocess.Popen(
            [
                str(binary),
                '--session',
                str(history),
                '--provider',
                'anthropic',
                '--model',
                'anthropic/claude-sonnet-4-5',
                '--api-key',
                'local-memory-test',
                '--no-context-files',
                '--no-skills',
                '--no-prompt-templates',
                '--no-approve',
            ],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            cwd=root,
            env=environment,
            start_new_session=True,
        )
        os.close(slave)
        try:
            wait_for(master, b'$ skills', time.perf_counter(), 30)
            if termios.tcgetattr(master)[3] & termios.ECHO:
                raise RuntimeError(
                    'Terminal echo is enabled. Use an interactive release binary and repeat the benchmark.'
                )
            loaded = sample_memory(process, master)
            # CLI --session loads entries but starts with an empty transcript.
            # /clone rebuilds every active-branch cell through the public UI.
            os.write(master, b'/clone\r')
            wait_for(master, b'KISS_HISTORY_READY', time.perf_counter(), 30)
            clones = [
                path for path in (root / 'sessions').rglob('*.jsonl') if path != history
            ]
            if len(clones) != 1:
                raise RuntimeError(
                    'The clone did not create one session. Check /clone persistence and repeat the benchmark.'
                )
            history = clones[0]
            ready = sample_memory(process, master)
            for index in range(updates):
                token = f'KISS_UPDATE_DONE_{index:04d}'
                (root / 'update.txt').write_text(
                    text.replace('\n', ' ') + '\n' + token + '\n'
                )
                os.write(master, b'!!cat update.txt\r')
                wait_for(master, token.encode(), time.perf_counter(), 30)
                # Progress can paint before the completed shell entry is saved.
                deadline = time.monotonic() + 30
                while True:
                    with history.open() as source:
                        saved = sum(1 for _ in source) - original_lines
                    if saved >= index + 1:
                        break
                    if time.monotonic() >= deadline:
                        raise RuntimeError(
                            'KISS did not save the local shell result. Check shell execution and session persistence before repeating.'
                        )
                    if select.select([master], [], [], 0.05)[0]:
                        os.read(master, 65_536)
            updated = sample_memory(process, master)
            idle = sample_memory(process, master)
            saved_updates = 0
            with history.open() as source:
                for line_index, line in enumerate(source):
                    if line_index < original_lines:
                        continue
                    record: Record = json.loads(line)
                    token = f'KISS_UPDATE_DONE_{saved_updates:04d}'
                    if (
                        record['type'] != 'message'
                        or record['message']['role'] != 'bashExecution'
                        or token not in record['message']['output']
                    ):
                        raise RuntimeError(
                            'A saved update does not contain the expected shell output. Check session persistence and repeat with automatic jobs disabled.'
                        )
                    saved_updates += 1
            if saved_updates != updates:
                raise RuntimeError(
                    f'The session saved {saved_updates} extra records; expected {updates} local shell records. Check startup settings and repeat with automatic jobs disabled.'
                )
            return {
                'entries': entries,
                'compacted': compacted,
                'trial': trial,
                'history_file_bytes': file_bytes,
                'loaded_memory': loaded,
                'ready_memory': ready,
                'updated_memory': updated,
                'idle_memory': idle,
                'saved_updates': saved_updates,
            }
        finally:
            stop(process, master)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/kiss'))
    parser.add_argument('--baseline', type=Path)
    parser.add_argument('--entries', type=int, nargs='+', default=[0, 1000, 5000])
    parser.add_argument('--text-bytes', type=int, default=2048)
    parser.add_argument('--trials', type=int, default=3)
    parser.add_argument('--updates', type=int, default=20)
    parser.add_argument('--json', type=Path)
    args = parser.parse_args()
    if min(args.entries) < 0 or min(args.text_bytes, args.trials, args.updates) < 1:
        parser.error(
            'Entry counts must be nonnegative; text bytes, trials, and updates must be positive. Use --entries 0 1000 5000 --text-bytes 2048 --trials 3 --updates 20.'
        )
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(
            f'The binary {binary} does not exist. Build it with cargo build --release -p kiss or pass --binary PATH.'
        )
    paragraph = 'A measured long session contains **styled text**, source/path.rs, and saved work.\n\n'
    text = (paragraph * (args.text_bytes // len(paragraph) + 1))[: args.text_bytes]
    cases = [
        (count, compacted)
        for count in args.entries
        for compacted in ([False, True] if count else [False])
    ]
    variants = [('current', binary)]
    if args.baseline:
        baseline = args.baseline.resolve()
        if not baseline.is_file():
            parser.error(
                f'The baseline binary {baseline} does not exist. Pass --baseline PATH to a saved release binary.'
            )
        variants = [('baseline', baseline), ('candidate', binary)]
    results: list[Trial] = []
    for trial in range(args.trials):
        for case_index, (count, compacted) in enumerate(
            cases if trial % 2 == 0 else reversed(cases)
        ):
            for variant, executable in (
                variants if (trial + case_index) % 2 == 0 else reversed(variants)
            ):
                result = run(
                    executable, count, compacted, text, args.updates, trial + 1
                )
                result['variant'] = variant
                results.append(result)
                print(json.dumps(result), flush=True)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(
            json.dumps(
                {
                    'binary': str(binary),
                    'baseline_binary': str(args.baseline.resolve())
                    if args.baseline
                    else None,
                    'baseline_sha256': hashlib.sha256(
                        args.baseline.read_bytes()
                    ).hexdigest()
                    if args.baseline
                    else None,
                    'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                    'host': platform.platform(),
                    'text_bytes_per_entry': args.text_bytes,
                    'updates_per_trial': args.updates,
                    'method': 'Release CLI RSS via ps (KiB converted to MiB), plus macOS physical footprint via proc_pid_rusage v0; 160x40 drained PTY; fresh files/processes; three observations per phase 0.2 seconds apart; user/assistant/bash entries in equal rotation; compaction retains last 32 messages; --session loads entries, /clone reconstructs the transcript; !!cat adds local shell results; no model turns. Reads global user settings. RSS includes allocator retention, excludes child processes and terminal emulator. No peak or memory p99 claim.',
                    'trials': results,
                },
                indent=2,
            )
            + '\n'
        )
    for variant, _ in variants:
        for count, compacted in cases:
            group = [
                result
                for result in results
                if result['variant'] == variant
                and result['entries'] == count
                and result['compacted'] == compacted
            ]
            print(
                f'{variant} entries={count} compacted={compacted} retained MiB [process range]'
            )
            for phase in [
                'loaded_memory',
                'ready_memory',
                'updated_memory',
                'idle_memory',
            ]:
                for metric in ['rss_mib', 'footprint_mib']:
                    medians = [
                        statistics.median(
                            sample[metric]
                            for sample in result[phase]
                            if sample[metric] is not None
                        )
                        for result in group
                        if result[phase][0][metric] is not None
                    ]
                    if medians:
                        print(
                            f'  {phase} {metric}: {statistics.median(medians):.3f} [{min(medians):.3f}–{max(medians):.3f}]'
                        )
