# Changelog

## 0.0.6 - 2026-09-07

### Added

- Added CLI and TUI commands to add, list, and remove OpenAI-compatible Chat
  Completions, Responses, and Codex API providers, including CodexLB.
- Added bounded `/loop` and `/autoresearch` jobs. Each job branches into a new
  session, and `/jobs` shows live progress and controls for multiple jobs.
- Added bold Markdown text, cyan underlined terminal hyperlinks, automatic
  links for bare HTTP and HTTPS URLs, and fenced-code syntax highlighting.

### Changed

- Added repeatable interactive startup and idle-memory measurements to the
  benchmark suite.
- Made Catppuccin Mocha the default dark terminal theme.
- Made `Ctrl+R` expand and collapse long tool results, as the TUI hint states.

## 0.0.5 - 2026-09-05

### Added

- Updated Pi compatibility to v0.85.1, including GPT-6 Astra, the refreshed
  model catalog, tiered model prices, persistent Claude effort, and restorable
  in-memory sessions.
- Added custom-provider request controls for vLLM priority and Responses output
  token limits.
- Added project and global resume support for KISS, Pi, Claude Code, and OpenAI
  Codex sessions.

### Changed

- Shared the model registry across agents, indexed model updates, reduced
  transcript copies, made session writes atomic, cleaned up idle mutation locks,
  and made transient-error detection stricter.
- Hid header-only sessions and delayed session-file creation until the first
  entry.

### Fixed

- Wrap long TUI rows instead of hiding their remaining text behind an ellipsis.
- Flush terminal OpenAI Codex SSE events even when EOF has no blank separator.
- Keep skills available when Bash is the only enabled file-reading tool.
- Make the model/thinking picker default-save shortcut configurable.
- Open the skill search menu for inline `$` mentions and invoke the selected
  skill without replacing preceding prompt text.
- Made the resume picker compact and kept terminal rendering stable after a
  resumed OpenAI Codex session.

## 0.0.4 - 2026-09-04

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
- Added a full browser WebAssembly agent with portable provider support.

### Changed

- Added SDK documentation and release checks for each language binding.
- Added SDK, RPC, and browser WebAssembly performance benchmarks with mean
  results.
- Reduced repeated terminal redraws during rapid window resizing.

### Fixed

- Added a Deno-compatible ESM entry point for the TypeScript SDK.
- Restored keyboard shortcuts, including `Ctrl+W`, Kitty and keypad Enter,
  and `Shift+Tab`.
- Fixed terminal rendering after rapid window size changes.

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
