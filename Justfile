default:
    @just --list

# Test the Rust SDK, shared protocol, mock provider, and RPC transport.
sdk-test:
    @cargo test -p kiss-sdk --features "mock rpc"

# Build and test the Python 3.11+ PyO3 package in its managed environment.
sdk-test-python:
    @cd crates/kiss-py && uv venv --python 3.12 .venv >/dev/null && uv pip install --python .venv maturin pytest pytest-asyncio >/dev/null && .venv/bin/maturin develop -q && .venv/bin/pytest -q

# Build the N-API package and run its suite in Node and Bun.
sdk-test-node:
    @cd crates/kiss-node && npm install --silent && npm run build && npm test && bun test test/end-to-end.test.cjs && npm run test:deno

# Build the browser WASM package and execute it in Deno against WebSocket RPC.
sdk-test-wasm:
    @cd crates/kiss-wasm && wasm-pack build --target web --out-dir pkg && deno test --allow-net --allow-read test/client_test.ts

# Verify every SDK surface end to end.
sdk-test-all: sdk-test sdk-test-python sdk-test-node sdk-test-wasm

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
