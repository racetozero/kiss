"""Tests for the KISS PGO build and benchmark programs."""

from __future__ import annotations

import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch

import build_kiss_pgo
import benchmark_kiss_pgo
from pgo_common import geometric_mean, prepare_fixture


class BuildPgoTests(unittest.TestCase):
    def test_native_and_linux_musl_targets_can_run(self) -> None:
        self.assertTrue(
            build_kiss_pgo.target_runs_on_host(
                "aarch64-apple-darwin", "aarch64-apple-darwin"
            )
        )
        self.assertTrue(
            build_kiss_pgo.target_runs_on_host(
                "x86_64-unknown-linux-musl", "x86_64-unknown-linux-gnu"
            )
        )
        self.assertFalse(
            build_kiss_pgo.target_runs_on_host(
                "aarch64-unknown-linux-gnu", "x86_64-unknown-linux-gnu"
            )
        )

    def test_hot_count_parser_requires_positive_95th_percentile(self) -> None:
        summary = "123 functions with count >= 41 account for 95% of the total counts."
        self.assertEqual(build_kiss_pgo.parse_hot_count(summary), 41)
        with self.assertRaises(RuntimeError):
            build_kiss_pgo.parse_hot_count("no detailed summary")
        with self.assertRaises(RuntimeError):
            build_kiss_pgo.parse_hot_count(
                "1 functions with count >= 0 account for 95% of the total counts."
            )

    def test_optimized_flags_keep_existing_linker_flags(self) -> None:
        flags = build_kiss_pgo.optimized_rustflags(
            "-C linker=rust-lld", Path("/tmp/kiss.profdata"), 27
        )
        self.assertIn("-C linker=rust-lld", flags)
        self.assertIn("-Cprofile-use=/tmp/kiss.profdata", flags)
        self.assertIn("--profile-summary-hot-count=27", flags)

    def test_github_environment_is_appended(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "github-env"
            path.write_text("EXISTING=1\n", encoding="utf-8")
            build_kiss_pgo.write_github_environment(path, "-Cprofile-use=profile")
            self.assertEqual(
                path.read_text(encoding="utf-8"),
                "EXISTING=1\nRUSTFLAGS=-Cprofile-use=profile\n",
            )


class BenchmarkTests(unittest.TestCase):
    def test_fixture_does_not_forward_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with patch.dict(
                "os.environ",
                {
                    "OPENAI_API_KEY": "private",
                    "GH_TOKEN": "private",
                    "PATH": "/usr/bin",
                },
                clear=True,
            ):
                _, environment = prepare_fixture(
                    Path(temporary), "http://127.0.0.1:1234/v1"
                )
        self.assertNotIn("OPENAI_API_KEY", environment)
        self.assertNotIn("GH_TOKEN", environment)
        self.assertEqual(environment["PATH"], "/usr/bin")

    def test_summary_and_change(self) -> None:
        summary = benchmark_kiss_pgo.summarize([1, 2, 3, 4, 5])
        self.assertEqual(summary.median_ns, 3)
        self.assertEqual(summary.p95_ns, 5)
        self.assertAlmostEqual(benchmark_kiss_pgo.percent_change(100, 90), -10.0)

    def test_target_directory_resolves_matched_binaries(self) -> None:
        arguments = Namespace(
            target_dir=Path("target/pgo"),
            target="x86_64-pc-windows-msvc",
            baseline=None,
            pgo=None,
        )
        baseline, pgo = benchmark_kiss_pgo.resolve_binaries(arguments)
        self.assertTrue(str(baseline).endswith("baseline/x86_64-pc-windows-msvc/dist/kiss.exe"))
        self.assertTrue(str(pgo).endswith("optimized/x86_64-pc-windows-msvc/dist/kiss.exe"))

    def test_geometric_mean(self) -> None:
        self.assertAlmostEqual(geometric_mean([1.0, 4.0]), 2.0)
        with self.assertRaises(ValueError):
            geometric_mean([])


if __name__ == "__main__":
    unittest.main()
