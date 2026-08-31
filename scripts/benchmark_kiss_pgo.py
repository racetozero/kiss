"""Compare ordinary and PGO KISS executables on held-out workloads."""

from __future__ import annotations

import argparse
import json
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path

from build_kiss_pgo import binary_path, rustc_host
from pgo_common import (
    MockProvider,
    REPOSITORY_ROOT,
    executable_size,
    geometric_mean,
    held_out_workloads,
    host_description,
    prepare_fixture,
    run_workload,
)


@dataclass(frozen=True, slots=True)
class Timing:
    median_ns: int
    p95_ns: int


def summarize(samples: list[int]) -> Timing:
    if not samples:
        raise ValueError("at least one timing sample is required")
    ordered = sorted(samples)
    p95_index = max(0, (len(ordered) * 95 + 99) // 100 - 1)
    return Timing(int(statistics.median(ordered)), ordered[p95_index])


def percent_change(baseline: int, pgo: int) -> float:
    return (pgo / baseline - 1.0) * 100.0


def command_output(command: list[str]) -> str:
    return subprocess.run(
        command,
        cwd=REPOSITORY_ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def benchmark(args: argparse.Namespace) -> dict[str, object]:
    baseline, pgo = resolve_binaries(args)
    for binary in (baseline, pgo):
        if not binary.is_file():
            raise RuntimeError(f"KISS executable not found: {binary}")

    workloads = held_out_workloads()
    raw: dict[str, dict[str, list[int]]] = {
        workload.name: {"baseline": [], "pgo": []} for workload in workloads
    }
    trial_medians: dict[str, dict[str, list[int]]] = {
        workload.name: {"baseline": [], "pgo": []} for workload in workloads
    }
    with (
        tempfile.TemporaryDirectory(prefix="kiss-pgo-benchmark-") as temporary,
        MockProvider() as server,
    ):
        cwd, environment = prepare_fixture(Path(temporary), server.base_url)
        for workload in workloads:
            sample_count = args.agent_samples if workload.name == "agent_turn" else args.samples
            for trial in range(args.trials):
                for binary in (baseline, pgo):
                    for _ in range(args.warmups):
                        run_workload(binary, workload, cwd=cwd, environment=environment)

                trial_raw: dict[str, list[int]] = {"baseline": [], "pgo": []}
                for index in range(sample_count):
                    order = (("baseline", baseline), ("pgo", pgo))
                    if (trial + index) % 2:
                        order = tuple(reversed(order))
                    for label, binary in order:
                        started = time.perf_counter_ns()
                        run_workload(binary, workload, cwd=cwd, environment=environment)
                        elapsed = time.perf_counter_ns() - started
                        trial_raw[label].append(elapsed)
                        raw[workload.name][label].append(elapsed)
                for label in ("baseline", "pgo"):
                    trial_medians[workload.name][label].append(
                        summarize(trial_raw[label]).median_ns
                    )

    rows: list[dict[str, object]] = []
    speed_ratios: list[float] = []
    for workload in workloads:
        baseline_all = summarize(raw[workload.name]["baseline"])
        pgo_all = summarize(raw[workload.name]["pgo"])
        baseline_timing = Timing(
            int(statistics.median(trial_medians[workload.name]["baseline"])),
            baseline_all.p95_ns,
        )
        pgo_timing = Timing(
            int(statistics.median(trial_medians[workload.name]["pgo"])),
            pgo_all.p95_ns,
        )
        change = percent_change(baseline_timing.median_ns, pgo_timing.median_ns)
        speed_ratios.append(pgo_timing.median_ns / baseline_timing.median_ns)
        rows.append(
            {
                "name": workload.name,
                "baseline": asdict(baseline_timing),
                "pgo": asdict(pgo_timing),
                "median_change_percent": change,
                "samples": len(raw[workload.name]["baseline"]),
                "trials": args.trials,
            }
        )

    baseline_bytes, baseline_gzip = executable_size(baseline)
    pgo_bytes, pgo_gzip = executable_size(pgo)
    aggregate_change = (geometric_mean(speed_ratios) - 1.0) * 100.0
    result: dict[str, object] = {
        "host": host_description(),
        "platform": platform.platform(),
        "python": platform.python_version(),
        "rustc": command_output(["rustc", "--version"]),
        "git_commit": command_output(["git", "rev-parse", "HEAD"]),
        "trials": args.trials,
        "short_samples_per_trial": args.samples,
        "agent_samples_per_trial": args.agent_samples,
        "warmups_per_trial": args.warmups,
        "baseline": str(baseline),
        "pgo": str(pgo),
        "workloads": rows,
        "aggregate_median_change_percent": aggregate_change,
        "size": {
            "baseline_bytes": baseline_bytes,
            "pgo_bytes": pgo_bytes,
            "change_percent": percent_change(baseline_bytes, pgo_bytes),
            "baseline_gzip_bytes": baseline_gzip,
            "pgo_gzip_bytes": pgo_gzip,
            "gzip_change_percent": percent_change(baseline_gzip, pgo_gzip),
        },
    }
    return result


def resolve_binaries(args: argparse.Namespace) -> tuple[Path, Path]:
    if args.target_dir is not None:
        target = args.target or rustc_host()
        root = args.target_dir.resolve()
        return (
            binary_path(root / "baseline", target),
            binary_path(root / "optimized", target),
        )
    if args.baseline is None or args.pgo is None:
        raise RuntimeError("provide --target-dir or both --baseline and --pgo")
    return args.baseline.resolve(), args.pgo.resolve()


def print_report(result: dict[str, object]) -> None:
    print(f"Host: {result['host']}")
    print("workload\tbaseline_median_ms\tpgo_median_ms\tchange")
    for row in result["workloads"]:
        baseline_ms = row["baseline"]["median_ns"] / 1_000_000
        pgo_ms = row["pgo"]["median_ns"] / 1_000_000
        print(
            f"{row['name']}\t{baseline_ms:.3f}\t{pgo_ms:.3f}\t"
            f"{row['median_change_percent']:+.2f}%"
        )
    print(f"aggregate\t\t\t{result['aggregate_median_change_percent']:+.2f}%")
    size = result["size"]
    print(
        f"binary bytes\t{size['baseline_bytes']}\t{size['pgo_bytes']}\t"
        f"{size['change_percent']:+.2f}%"
    )
    print(
        f"gzip bytes\t{size['baseline_gzip_bytes']}\t{size['pgo_gzip_bytes']}\t"
        f"{size['gzip_change_percent']:+.2f}%"
    )


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--pgo", type=Path)
    parser.add_argument("--target-dir", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--samples", type=int, default=50)
    parser.add_argument("--agent-samples", type=int, default=20)
    parser.add_argument("--warmups", type=int, default=5)
    parser.add_argument("--trials", type=int, default=3)
    parser.add_argument("--json", type=Path)
    args = parser.parse_args()
    if args.target_dir is not None and (args.baseline is not None or args.pgo is not None):
        parser.error("--target-dir cannot be combined with --baseline or --pgo")
    if args.target_dir is None and (args.baseline is None or args.pgo is None):
        parser.error("provide --target-dir or both --baseline and --pgo")
    if min(args.samples, args.agent_samples, args.warmups, args.trials) < 1:
        parser.error("sample, warm-up, and trial counts must be positive")
    return args


if __name__ == "__main__":
    try:
        parsed = parse_arguments()
        measured = benchmark(parsed)
        print_report(measured)
        if parsed.json:
            parsed.json.parent.mkdir(parents=True, exist_ok=True)
            parsed.json.write_text(
                json.dumps(measured, indent=2) + "\n", encoding="utf-8", newline="\n"
            )
    except (OSError, RuntimeError, subprocess.CalledProcessError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
