default:
    @just --list

# Run the release-mode performance benchmark suite through cargo-nextest.
bench:
    @cargo nextest run --workspace --release --run-ignored only --no-capture -E 'test(~benchmark_performance_)'

# Run every release gate and build one native archive without publishing.
release-check version target="":
    @scripts/release.sh check "{{ version }}" "{{ target }}"

# Check, confirm, tag, and publish a release from a clean main branch.
release version target="":
    @scripts/release.sh release "{{ version }}" "{{ target }}"
