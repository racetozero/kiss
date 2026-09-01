"""Build KISS with profile-guided optimization."""

from __future__ import annotations

import argparse
import os
import re
import shlex
import subprocess
import sys
import tempfile
from pathlib import Path

from pgo_common import (
    MockProvider,
    REPOSITORY_ROOT,
    prepare_fixture,
    run_workload,
    training_workloads,
)


def rustc_host() -> str:
    output = subprocess.run(
        ["rustc", "--version", "--verbose"],
        cwd=REPOSITORY_ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise RuntimeError("Could not determine the active Rust compiler host")


def target_runs_on_host(target: str, host: str) -> bool:
    if target == host:
        return True
    host_parts = host.split("-")
    target_parts = target.split("-")
    return (
        len(host_parts) >= 4
        and len(target_parts) >= 4
        and host_parts[0] == target_parts[0]
        and host_parts[2:] == ["linux", "gnu"]
        and target_parts[2:] == ["linux", "musl"]
    )


def append_flags(existing: str | None, additional: str) -> str:
    return " ".join(part for part in (existing, additional) if part)


def find_llvm_profdata(host: str, override: Path | None = None) -> Path:
    if override is not None:
        profiler = override.resolve()
    else:
        sysroot = subprocess.run(
            ["rustc", "--print", "sysroot"],
            cwd=REPOSITORY_ROOT,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        binary = "llvm-profdata.exe" if "windows" in host else "llvm-profdata"
        profiler = Path(sysroot) / "lib" / "rustlib" / host / "bin" / binary
    if not profiler.is_file():
        raise RuntimeError(
            f"Rust toolchain llvm-profdata not found: {profiler}; "
            "run `rustup component add llvm-tools-preview`"
        )
    return profiler


def profile_hot_count(profiler: Path, profile: Path, environment: dict[str, str]) -> int:
    output = subprocess.run(
        [
            str(profiler),
            "show",
            "--detailed-summary",
            "--detailed-summary-cutoffs=950000",
            str(profile),
        ],
        cwd=REPOSITORY_ROOT,
        env=environment,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return parse_hot_count(output)


def parse_hot_count(summary: str) -> int:
    match = re.search(
        r"with count >= (\d+) account for 95% of the total counts\.", summary
    )
    if match is None:
        raise RuntimeError("Could not determine the 95th-percentile PGO hot count")
    count = int(match.group(1))
    if count <= 0:
        raise RuntimeError(f"PGO hot count must be positive, got {count}")
    return count


def cargo_command(target: str) -> list[str]:
    return [
        "cargo",
        "rustc",
        "--locked",
        "--package",
        "kiss",
        "--bin",
        "kiss",
        "--profile",
        "dist",
        "--target",
        target,
    ]


def binary_path(target_directory: Path, target: str) -> Path:
    name = "kiss.exe" if "windows" in target else "kiss"
    return target_directory / target / "dist" / name


def run(command: list[str], environment: dict[str, str]) -> None:
    displayed = shlex.join(command[:16])
    if len(command) > 16:
        displayed += f" ... ({len(command) - 16} arguments omitted)"
    print(f"> {displayed}", flush=True)
    subprocess.run(command, cwd=REPOSITORY_ROOT, env=environment, check=True)


def merge_profiles(
    profiler: Path,
    profiles: list[Path],
    destination: Path,
    environment: dict[str, str],
) -> None:
    empty = [profile for profile in profiles if profile.stat().st_size == 0]
    if not profiles or empty:
        detail = (
            f"{len(empty)} empty profiles, such as {empty[0].name}"
            if empty
            else "no profiles at all"
        )
        raise RuntimeError(
            f"PGO training did not produce complete raw profiles ({detail}); "
            "the trained binary must return from main, because an exit that "
            "skips the atexit handlers does not write the counters"
        )
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        dir=destination.parent, prefix="kiss-", suffix=".profdata", delete=False
    ) as temporary:
        temporary_profile = Path(temporary.name)
    try:
        run(
            [
                str(profiler),
                "merge",
                "--output",
                str(temporary_profile),
                *map(str, profiles),
            ],
            environment,
        )
        temporary_profile.replace(destination)
    finally:
        temporary_profile.unlink(missing_ok=True)
    size = sum(profile.stat().st_size for profile in profiles)
    print(
        f"Merged {len(profiles)} raw profiles ({size:,} bytes): {destination}",
        flush=True,
    )


def optimized_rustflags(existing: str | None, profile: Path, hot_count: int) -> str:
    return append_flags(
        existing,
        f"-Cprofile-use={profile} "
        f"-Cllvm-args=--profile-summary-hot-count={hot_count}",
    )


def write_github_environment(path: Path, rustflags: str) -> None:
    with path.open("a", encoding="utf-8", newline="\n") as stream:
        stream.write(f"RUSTFLAGS={rustflags}\n")


def build(args: argparse.Namespace) -> None:
    host = rustc_host()
    target = args.target or host
    if not target_runs_on_host(target, host):
        raise RuntimeError(
            f"PGO training target {target} cannot run on Rust host {host}; "
            "use a native runner"
        )
    profiler = find_llvm_profdata(host, args.llvm_profdata)
    root = args.target_dir.resolve()
    profile_directory = root / "profiles"
    merged_profile = root / "kiss.profdata"
    profile_directory.mkdir(parents=True, exist_ok=True)
    for old_profile in profile_directory.glob("kiss-*.profraw"):
        old_profile.unlink()

    base_environment = os.environ.copy()
    base_environment["CARGO_INCREMENTAL"] = "0"
    if target.endswith("-apple-darwin"):
        for variable in ("CFLAGS", "CXXFLAGS"):
            base_environment[variable] = append_flags(
                base_environment.get(variable),
                "-fno-profile-generate -fno-profile-use",
            )

    if args.baseline_and_pgo:
        baseline_directory = root / "baseline"
        baseline_environment = base_environment | {
            "CARGO_TARGET_DIR": str(baseline_directory)
        }
        print("Building ordinary dist KISS", flush=True)
        run(cargo_command(target), baseline_environment)
        print(f"Ordinary KISS: {binary_path(baseline_directory, target)}", flush=True)

    instrumented_directory = root / "instrumented"
    instrumented_environment = base_environment | {
        "CARGO_TARGET_DIR": str(instrumented_directory),
        "RUSTFLAGS": append_flags(
            base_environment.get("RUSTFLAGS"),
            f"-Cprofile-generate={profile_directory}",
        ),
    }
    print("Building instrumented dist KISS", flush=True)
    run(cargo_command(target), instrumented_environment)
    instrumented_binary = binary_path(instrumented_directory, target)
    if not instrumented_binary.is_file():
        raise RuntimeError(f"Instrumented KISS binary not found: {instrumented_binary}")

    print("Training KISS on deterministic offline workloads", flush=True)
    with tempfile.TemporaryDirectory(prefix="kiss-pgo-") as temporary, MockProvider() as server:
        cwd, fixture_environment = prepare_fixture(Path(temporary), server.base_url)
        for workload in training_workloads():
            for _ in range(workload.repetitions):
                run_workload(
                    instrumented_binary,
                    workload,
                    cwd=cwd,
                    environment=fixture_environment,
                    profile_directory=profile_directory,
                )

    raw_profiles = sorted(profile_directory.glob("kiss-*.profraw"))
    merge_profiles(profiler, raw_profiles, merged_profile, base_environment)
    hot_count = profile_hot_count(profiler, merged_profile, base_environment)
    (root / "kiss.profile-hot-count").write_text(
        f"{hot_count}\n", encoding="utf-8", newline="\n"
    )
    rustflags = optimized_rustflags(
        base_environment.get("RUSTFLAGS"), merged_profile, hot_count
    )
    print(f"Using 95th-percentile PGO hot count: {hot_count}", flush=True)

    if args.github_env:
        github_environment = os.environ.get("GITHUB_ENV")
        if not github_environment:
            raise RuntimeError("--github-env requires GITHUB_ENV")
        write_github_environment(Path(github_environment), rustflags)
        print(f"Wrote PGO RUSTFLAGS to {github_environment}", flush=True)

    if args.train_only:
        print(f"Merged KISS profile: {merged_profile}", flush=True)
        return

    optimized_directory = root / "optimized"
    optimized_environment = base_environment | {
        "CARGO_TARGET_DIR": str(optimized_directory),
        "RUSTFLAGS": rustflags,
    }
    print("Building PGO dist KISS", flush=True)
    run(cargo_command(target), optimized_environment)
    print(f"PGO KISS: {binary_path(optimized_directory, target)}", flush=True)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Runnable Rust target triple")
    parser.add_argument(
        "--target-dir",
        type=Path,
        default=REPOSITORY_ROOT / "target" / "kiss-pgo",
        help="PGO work directory",
    )
    parser.add_argument("--llvm-profdata", type=Path)
    parser.add_argument("--train-only", action="store_true")
    parser.add_argument("--baseline-and-pgo", action="store_true")
    parser.add_argument(
        "--github-env",
        action="store_true",
        help="Append optimized RUSTFLAGS to the GITHUB_ENV file",
    )
    args = parser.parse_args()
    if args.train_only and args.baseline_and_pgo:
        parser.error("--train-only and --baseline-and-pgo cannot be combined")
    if args.github_env and not args.train_only:
        parser.error("--github-env requires --train-only")
    return args


if __name__ == "__main__":
    try:
        build(parse_arguments())
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1) from error
