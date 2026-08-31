default:
    @just --list

# Run the release-mode performance benchmark suite through cargo-nextest.
bench:
    @cargo nextest run --workspace --release --run-ignored only --no-capture -E 'test(~benchmark_performance_)'

# Test the cross-platform PGO build and benchmark helpers.
pgo-test:
    @python3 scripts/test_pgo.py

# Build one native PGO release binary.
pgo-build target="":
    @python3 scripts/build_kiss_pgo.py --target-dir target/kiss-pgo {{ if target == "" { "" } else { "--target=" + target } }}

# Build matched ordinary and PGO binaries, then compare held-out workloads.
pgo-bench target="":
    @python3 scripts/build_kiss_pgo.py --baseline-and-pgo --target-dir target/kiss-pgo {{ if target == "" { "" } else { "--target=" + target } }}
    @python3 scripts/benchmark_kiss_pgo.py --target-dir target/kiss-pgo {{ if target == "" { "" } else { "--target=" + target } }} --json target/kiss-pgo/results.json

# Run every release gate and build one native archive without publishing.
release-check version target="":
    @scripts/release.sh check "{{ version }}" "{{ target }}"

# Check, confirm, tag, and publish a release from a clean main branch.
release version target="":
    @scripts/release.sh release "{{ version }}" "{{ target }}"
