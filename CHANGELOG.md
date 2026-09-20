# Changelog

## Unreleased

### Added

- Updated Pi compatibility to v0.86.0 with the current provider catalog,
  offline Radius models, prompt-cache lifetime data and cost-aware warming,
  per-model compaction budgets, and local redacted `/bug` reports.

### Fixed

- Added provider session-affinity headers, unsigned-thinking replay controls,
  Gemini discrete thinking levels, Mistral reasoning effort, one-hour cache
  pricing for Anthropic and Bedrock, Cloudflare 520 retries, Azure peak-load
  retries, capped and cancellable retry waits, and safe compaction after large
  tool results.

## 0.0.13 - 2026-09-19

### Added

- Added historical cache-usage reports to `kiss cache-usage` and
  `/cache-usage`, with session and provider filters, fixed-scale charts,
  current TUI cache rates, and cached-token data in the Python, Node, and
  WebAssembly SDKs.

### Fixed

- Made the doctor WebSocket probe use a valid 16-byte handshake key so strict
  servers do not reject a valid endpoint.

## 0.0.12 - 2026-09-18

### Added

- Added experimental TypeSafe Jev compaction, with TypeSafe API-key login,
  credential-gated opt-in settings, verbatim tool-history filtering, and safe
  fallback to summary compaction.

## 0.0.11 - 2026-09-16

### Added

- Added native stable ACP v1 support over standard input and output, including
  persistent sessions, model and thinking controls, cancellation, streamed
  tool updates and diffs, and session-local stdio or HTTP MCP servers.
- Added WebMCP support for discovering and calling tools exposed by open
  Chromium pages, with origin controls, bounded results, and cancellation.

## 0.0.10 - 2026-09-13

### Added

- Added `/update` to update the installed KISS binary from the interactive
  terminal.
- Added session-local `/fast` toggle support for provider low-latency tiers.
  Supported paths include Anthropic, OpenAI, OpenAI Codex, OpenRouter, xAI,
  Amazon Bedrock, and Google Vertex AI.
- Added a native Cursor provider with direct HTTP/2 transport, browser login,
  live model discovery, image input, and KISS tool execution.
- Documented the 41 built-in providers in the README.

### Changed

- Updated Anthropic OAuth compatibility to Claude Code protocol 2.1.258 and
  the current Pi Black request format.

## 0.0.9 - 2026-09-12

### Fixed

- Limited `/model`, scoped model selection, model cycling, and `--list-models`
  to providers with saved credentials, detected API keys, or manual entries in
  `models.json`.
- Added guidance in the `/model` selector explaining that `/login` connects
  additional providers.

## 0.0.8 - 2026-09-12

### Added

- Added `kiss doctor` and `kiss doctor --summary` with local health checks and
  reachability tables for every provider SSE, WebSocket, AWS event-stream, and
  login destination.
- Added Azure OpenAI authentication with API keys, bearer tokens, and the
  Microsoft Entra default credential chain.
- Added Responses WebSocket and cached WebSocket transport for the standard
  OpenAI and Azure OpenAI providers.

### Changed

- Made cross-agent session discovery and import faster and reduced repeated
  session-file parsing.

### Fixed

- Made the Linux installer use the musl release when the system glibc is not
  available or is too old.

## 0.0.7 - 2026-09-07

### Added

- Added native Windows ARM64 release artifacts.

### Changed

- Made loop and autoresearch jobs run until completion or an explicit stop when
  no iteration limit is given.
- Added optional compound intervals such as `15m` and `2d4h` to `/loop`.
- Reorganized the README around product value, first use, core workflows,
  integrations, and measured performance.

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
