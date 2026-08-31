"""Shared deterministic workloads for KISS PGO training and measurement."""

from __future__ import annotations

import gzip
import json
import os
import platform
import subprocess
import threading
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Iterable

REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
SENSITIVE_ENV_MARKERS = ("API_KEY", "CREDENTIAL", "PASSWORD", "SECRET", "TOKEN")


@dataclass(frozen=True, slots=True)
class Workload:
    """One process invocation and its expected output."""

    name: str
    arguments: tuple[str, ...]
    expected_output: str
    repetitions: int = 1
    stdin: str | None = None


class _MockProviderHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        content_length = int(self.headers.get("content-length", "0"))
        request = self.rfile.read(content_length)
        try:
            payload = json.loads(request)
            model = payload.get("model", "pgo-model")
        except (UnicodeDecodeError, json.JSONDecodeError):
            model = "pgo-model"

        chunks = (
            {
                "id": "chatcmpl-kiss-pgo",
                "model": model,
                "choices": [
                    {"index": 0, "delta": {"role": "assistant"}, "finish_reason": None}
                ],
            },
            {
                "id": "chatcmpl-kiss-pgo",
                "model": model,
                "choices": [
                    {
                        "index": 0,
                        "delta": {"content": "KISS mock response"},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-kiss-pgo",
                "model": model,
                "choices": [
                    {"index": 0, "delta": {"content": "."}, "finish_reason": "stop"}
                ],
                "usage": {
                    "prompt_tokens": 32,
                    "completion_tokens": 4,
                    "total_tokens": 36,
                },
            },
        )
        body = "".join(f"data: {json.dumps(chunk, separators=(',', ':'))}\n\n" for chunk in chunks)
        body += "data: [DONE]\n\n"
        encoded = body.encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(encoded)))
        self.send_header("connection", "close")
        self.end_headers()
        self.wfile.write(encoded)

    def log_message(self, format: str, *args: object) -> None:
        del format, args


class MockProvider:
    """A loopback-only OpenAI-compatible streaming server."""

    def __init__(self) -> None:
        self._server = ThreadingHTTPServer(("127.0.0.1", 0), _MockProviderHandler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def base_url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}/v1"

    def __enter__(self) -> MockProvider:
        self._thread.start()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)


def prepare_fixture(root: Path, base_url: str) -> tuple[Path, dict[str, str]]:
    """Create isolated KISS model, MCP, and attachment data."""

    home = root / "home"
    project = root / "project"
    agent_dir = home / ".kiss" / "agent"
    agent_dir.mkdir(parents=True, exist_ok=True)
    project.mkdir(parents=True, exist_ok=True)

    models = {
        "providers": {
            "pgo": {
                "baseUrl": base_url,
                "api": "openai-completions",
                "apiKey": "pgo-key",
                "compat": {
                    "supportsFinishReason": True,
                    "supportsUsageInStreaming": True,
                },
                "models": [
                    {
                        "id": "pgo-model",
                        "name": "KISS PGO fixture",
                        "contextWindow": 128_000,
                        "maxTokens": 4_096,
                    }
                ],
            }
        }
    }
    (agent_dir / "models.json").write_text(
        json.dumps(models, separators=(",", ":")), encoding="utf-8"
    )
    mcp = {
        "mcpServers": {
            "offline": {
                "command": "kiss-pgo-offline-server",
                "args": ["--fixture"],
                "disabled": True,
            }
        }
    }
    (project / ".mcp.json").write_text(
        json.dumps(mcp, separators=(",", ":")), encoding="utf-8"
    )
    (project / "training.txt").write_text(
        "Training data covers provider startup and streaming.\n", encoding="utf-8"
    )
    (project / "held-out.txt").write_text(
        "Held-out data uses a different prompt and attachment.\n", encoding="utf-8"
    )

    environment = {
        key: value
        for key, value in os.environ.items()
        if not any(marker in key.upper() for marker in SENSITIVE_ENV_MARKERS)
    }
    environment.update(
        {
            "HOME": str(home),
            "USERPROFILE": str(home),
            "XDG_CONFIG_HOME": str(home / ".config"),
            "APPDATA": str(home / "AppData" / "Roaming"),
            "LOCALAPPDATA": str(home / "AppData" / "Local"),
            "KISS_MODELS_FILE": str(agent_dir / "models.json"),
            "KISS_SESSION_DIR": str(root / "sessions"),
            "NO_COLOR": "1",
            "NO_PROXY": "127.0.0.1,localhost",
            "no_proxy": "127.0.0.1,localhost",
        }
    )
    return project, environment


def common_agent_arguments() -> tuple[str, ...]:
    return (
        "--provider",
        "pgo",
        "--model",
        "pgo-model",
        "--api-key",
        "pgo-key",
        "--no-session",
        "--no-context-files",
        "--no-skills",
        "--no-prompt-templates",
    )


def training_workloads() -> tuple[Workload, ...]:
    common = common_agent_arguments()
    return (
        Workload("version", ("--version",), "kiss", repetitions=6),
        Workload("help", ("--help",), "Usage:", repetitions=4),
        Workload(
            "models_claude", ("--list-models", "claude-sonnet"), "claude", repetitions=4
        ),
        Workload("models_gpt", ("--list-models", "gpt-5"), "gpt-5", repetitions=4),
        Workload("mcp_list", ("mcp", "list", "--json"), "offline", repetitions=3),
        Workload(
            "agent_print",
            (*common, "--print", "@training.txt", "Summarize this training fixture."),
            "KISS mock response.",
            repetitions=4,
        ),
        Workload(
            "agent_json",
            (*common, "--mode", "json", "--no-tools", "Return a short training reply."),
            '"type":"agent_end"',
            repetitions=3,
        ),
    )


def held_out_workloads() -> tuple[Workload, ...]:
    common = common_agent_arguments()
    return (
        Workload("startup_help", ("--help",), "Usage:"),
        Workload("catalog_search", ("--list-models", "gpt-5.6"), "gpt-5.6"),
        Workload("mcp_get", ("mcp", "get", "offline", "--json"), "offline"),
        Workload(
            "agent_turn",
            (*common, "--print", "@held-out.txt", "Give one held-out response."),
            "KISS mock response.",
        ),
    )


def run_workload(
    binary: Path,
    workload: Workload,
    *,
    cwd: Path,
    environment: dict[str, str],
    profile_directory: Path | None = None,
) -> subprocess.CompletedProcess[str]:
    child_environment = environment.copy()
    if profile_directory is not None:
        child_environment["LLVM_PROFILE_FILE"] = str(
            profile_directory / f"kiss-{workload.name}-%m-%p.profraw"
        )
    completed = subprocess.run(
        [str(binary), *workload.arguments],
        cwd=cwd,
        env=child_environment,
        input=workload.stdin,
        text=True,
        capture_output=True,
        check=False,
        timeout=30,
    )
    output = completed.stdout + completed.stderr
    if completed.returncode != 0:
        raise RuntimeError(
            f"{workload.name} returned {completed.returncode}: {output.strip()}"
        )
    if workload.expected_output not in output:
        raise RuntimeError(
            f"{workload.name} did not contain {workload.expected_output!r}: "
            f"{output.strip()}"
        )
    return completed


def executable_size(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    return len(data), len(gzip.compress(data, compresslevel=9, mtime=0))


def host_description() -> str:
    return f"{platform.system()} {platform.machine()}"


def geometric_mean(values: Iterable[float]) -> float:
    values = tuple(values)
    if not values or any(value <= 0 for value in values):
        raise ValueError("geometric mean needs positive values")
    product = 1.0
    for value in values:
        product *= value
    return product ** (1.0 / len(values))
