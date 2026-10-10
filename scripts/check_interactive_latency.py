"""Check first paint, typing, and paste while Cursor discovery cannot finish."""

from __future__ import annotations

import argparse
import fcntl
import os
import pty
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
from pathlib import Path

from benchmark_kiss_harness import stop, wait_for


def check(binary: Path) -> None:
    connected = threading.Event()
    release = threading.Event()
    with socket.socket() as server, tempfile.TemporaryDirectory() as directory:
        server.bind(('127.0.0.1', 0))
        server.listen()
        server.settimeout(5)

        def hang() -> None:
            try:
                connection, _ = server.accept()
                with connection:
                    connection.recv(65_536)
                    connected.set()
                    release.wait(10)
            except OSError:
                return

        thread = threading.Thread(target=hang, daemon=True)
        thread.start()
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 160, 0, 0))
        environment = os.environ.copy()
        environment.update(
            {
                'TERM': 'xterm-256color',
                'CURSOR_ACCESS_TOKEN': 'local-test-token',
                'KISS_CURSOR_URL': f'http://127.0.0.1:{server.getsockname()[1]}',
                'KISS_CURSOR_TRANSPORT': 'http1',
            }
        )
        started = time.perf_counter()
        process = subprocess.Popen(
            [
                str(binary),
                '--no-session',
                '--no-approve',
                '--provider',
                'cursor',
                '--model',
                'cursor/auto',
            ],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=environment,
            cwd=directory,
            start_new_session=True,
        )
        os.close(slave)
        try:
            first_frame = wait_for(master, b'Loading the session.', started, 2)
            if not connected.wait(3):
                raise RuntimeError(
                    'Cursor discovery did not reach the local server. Run with a Cursor provider and a local test token.'
                )
            typing_started = time.perf_counter()
            os.write(master, b'latency-probe')
            typing = wait_for(master, b'latency-probe', typing_started, 1)
            paste_started = time.perf_counter()
            payload = memoryview(
                b'\x1b[200~' + b'p' * 16_384 + b'paste-end' + b'\x1b[201~'
            )
            while payload:
                payload = payload[os.write(master, payload) :]
            paste = wait_for(master, b'Pasted text #1 16393 chars', paste_started, 2)
            os.write(master, b'\x03')
            process.wait(timeout=2)
            print(
                f'first_frame_ms={first_frame:.3f} typing_ms={typing:.3f} paste_ms={paste:.3f} stalled_discovery=true exit_code={process.returncode}'
            )
        finally:
            stop(process, master)
            release.set()
            thread.join(timeout=1)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/kiss'))
    arguments = parser.parse_args()
    check(arguments.binary.resolve())
