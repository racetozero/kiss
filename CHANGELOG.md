# Changelog

## Unreleased

### Added

- Added the `kiss-sdk` Rust crate with a high-level session builder, typed
  dispatcher, bounded streaming events, model/session controls, direct shell
  execution, and a hermetic mock provider.
- Added Python 3.11+ PyO3 bindings with async APIs, `StrEnum` options, and typed
  protocol dictionaries.
- Added N-API TypeScript bindings for Node.js, Bun, and Deno.
- Added a WebAssembly browser client for RPC WebSocket sessions.
- Added language-neutral JSONL RPC mode over stdin/stdout and WebSocket with
  Pi-compatible command and event names.

## 0.0.3 - 2026-09-02

### Added

- Added dynamic workflows with deterministic orchestration scripts, parallel
  child agents, approval and progress views, saved slash commands, and run
  controls.

### Changed

- Added performance coverage for workflow parsing, execution, progress
  snapshots, terminal rendering, and request preparation.

### Fixed

- Made workflow activation consistent for startup, interactive, print, JSON,
  queued, and slash-command prompts.
- Added verified workflow outcomes that cannot be replaced by model text.
- Fixed stale pause state in the workflow progress view.
- Unified child-turn cancellation, result handling, and usage accounting for
  manual subagents and workflow agents.

## 0.0.2 - 2026-09-01

### Added

- Added opt-in Codex-style subagents with controls for parallel work.
- Added `kiss update` and a launch notice when a newer release is available.
- Added profile-guided release builds and repeatable performance benchmarks.

### Changed

- Updated Pi compatibility to v0.84.4, including model thinking levels and
  interactive commands.
- Reduced release binary and compressed download sizes.

### Fixed

- Fixed multiline paste and Shift+Enter newline input.
- Restored the blinking block cursor in the terminal interface.
- Made profile-guided release tests portable across workspace locations.
- Fixed the Windows profile-guided release build, which collected empty
  profiles because the program did not return from main.

## 0.0.1 - 2026-08-28

- Matched Pi's interactive command menu and keyboard behavior.
- Added browser and headless provider authentication.
- Added automatic import of credentials from other coding agents.
- Added cached WebSocket transport for OpenAI-compatible response APIs.
- Added cargo-dist release archives, checksums, installers, and GitHub
  attestations for macOS, Linux, and Windows.
- Reduced release binary size with measured fat LTO, abort-on-panic, and
  macOS safe ICF settings.
- Added just commands for release checks and confirmed tag publication.
- Added the Rust coding-agent runtime, provider adapters, sessions, tools, and terminal UI.
