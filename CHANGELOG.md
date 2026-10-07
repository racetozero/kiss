# Changelog

## Unreleased

### Added

- Publish a Homebrew formula on each stable release. Install KISS with
  `brew install racetozero/tap/kiss`. On Linux, the formula installs the
  static musl build, so KISS does not require glibc. For a Homebrew
  install, `kiss update` stops and tells you to run
  `brew upgrade racetozero/tap/kiss`.

## 0.0.35 - 2026-10-06

### Changed

- Use mimalloc for memory allocation in the CLI, Python module, and Node
  module to improve performance.

## 0.0.34 - 2026-10-05

### Added

- Add native Herdr and cmux support for interactive sessions, including
  work status, decision notifications, and saved session resume commands.

### Fixed

- Answer Cursor web search and fetch permission requests instead of ending
  the turn. Return a refusal for native questions, mode changes, and plan
  requests that KISS cannot perform so the model can continue with KISS tools.

## 0.0.33 - 2026-10-05

### Fixed

- Select Git Bash, PowerShell 7, Windows PowerShell, or `cmd.exe` automatically
  on Windows when `shellPath` is not set. Tell the model which shell and
  command syntax to use, including Cursor-native Shell calls.

## 0.0.32 - 2026-10-05

### Fixed

- Run Cursor-native Shell requests through KISS's enabled `bash` tool and
  return Cursor's expected shell results, including streamed completion.
  Preserve working directories and convert shell timeouts to seconds.
- Preserve integer tool arguments from Cursor so agent wait timeouts work.
- Load `SKILL.md` from the directory passed to `--skill`.

## 0.0.31 - 2026-10-05

### Added

- Add multiple accounts per provider with `/accounts` or
  `kiss login PROVIDER --add-account`. When the active account is rate
  limited or out of quota, KISS resends the request with the next ready
  account before any output appears. This works for OpenAI Codex (HTTP and
  WebSocket), Anthropic, Cursor, Google, Meta, xAI, and every other provider
  with a login. The footer shows the active account when a provider has two
  or more accounts.
- Manage accounts interactively: `/accounts` opens provider, account, and
  action pickers to add, use, or remove accounts.
- Add `--no-auth` to `kiss provider add` and `/provider add` for proxies,
  such as CodexLB, that handle authentication themselves.
- Keep `retry-after` and Claude subscription reset headers, and the status
  and type of Responses WebSocket error frames, in provider error messages.

## 0.0.30 - 2026-10-03

### Fixed

- Accept keyboard repeat events so held arrow keys move through TUI menus
  and model pickers in terminals that use the Kitty keyboard protocol.

## 0.0.29 - 2026-10-03

### Added

- Test Cursor through TLS interception proxies with automatic and forced H2,
  forced H1 with and without ALPN, and forced H2 rejection without ALPN
  before sending input.

### Fixed

- Refresh Cursor's account model list when the TUI opens the model picker or
  selects a Cursor model, including after login in the same session. Use the
  discovered model entry for selection instead of the startup fallback list,
  and show discovery errors in the TUI. Reuse a successful discovery for the
  active session; clear the cache on Cursor login or logout.
- Replace the Cursor fallback roster after account model discovery so stale
  built-in names cannot override the account's actual model IDs. Preserve
  Cursor's declared `auto` alias.
- Resolve `cursor/auto` through Cursor's account-default model API before
  sending input, and send both model selection fields required by Cursor.
- Answer Cursor's request-context handshake with KISS tools and system
  instructions instead of rejecting it as an unsupported native tool.
- Stop retrying permanent model and protocol errors merely because the
  message contains `request failed`. Respect Cursor's `isRetryable: false`.

## 0.0.28 - 2026-10-02

### Fixed

- Select Cursor's HTTP/2 `Run` or HTTP/1 `RunSSE` and `BidiAppend` transport
  through normal ALPN negotiation before sending input. Add
  `KISS_CURSOR_TRANSPORT=auto|http1|http2` for an explicit transport choice.
  Do not retry submitted prompts or tool results. Remove the custom TLS ALPN
  configuration and direct certificate loader.

## 0.0.27 - 2026-10-02

### Added

- Test all built-in providers through a TLS proxy that accepts only
  HTTP/1.1 in ALPN. Check that Cursor still sends HTTP/2.

### Fixed

- Accept TLS proxies that select HTTP/1.1 through ALPN while keeping Cursor
  model runs on HTTP/2. Proxies could close the connection when KISS offered
  only HTTP/2 during TLS setup.

## 0.0.26 - 2026-10-02

### Added

- Test all 44 built-in providers with direct TLS, a TLS interception
  proxy without ALPN, proxy bypass, and an untrusted proxy certificate.
  Check actual Responses WebSocket upgrades as well as HTTP requests.

### Fixed

- Use HTTP/2 for Cursor runs when corporate TLS proxies omit ALPN. Keep model
  discovery on `api2.cursor.sh` with HTTP/1.1 support.
- Use proxy settings and trusted root certificates for OpenAI, Codex, and
  Azure Responses WebSocket connections.
- Use the configured base URL for Google Vertex requests.

## 0.0.25 - 2026-10-02

### Fixed

- Send a current Cursor CLI version for model requests and model lists. The
  old version could cause Cursor to reject model requests after a successful
  sign-in.
- Use native absolute paths in ACP test inputs so the tests can run on Windows.

## 0.0.24 - 2026-10-01

### Added

- Sign in to Cursor from the CLI. Use `kiss login cursor --no-browser` on a
  server or SSH session, then open the printed URL on another computer.
- Let the model edit its conversation context with
  `--experimental-context-file`. Valid edits are saved in session checkpoints.
  The default is off.

### Fixed

- Keep complete Windows sign-in URLs. Do not treat URL query fields such as
  `uuid` as shell commands.
- Find Git Bash on Windows and use the correct arguments for Bash, PowerShell,
  and `cmd.exe` in agent tools, the SDK, and the TUI.
- Start Windows MCP servers that use `.cmd` launchers. Resolve their commands
  against the configured path and working directory.
- Expand native home paths, enable Windows terminal input, and restore the
  original console modes after normal exit or a panic.
- Use the Windows Google credential directory and `gcloud.cmd` for Google
  Application Default Credentials.

### Changed

- Use the shared provider login methods in the CLI. Replace manual UTF-8
  boundary checks and the Python mean calculation with standard functions.
- Run nextest on Windows, Linux, and macOS. Run documentation tests separately
  and add a Windows panic-cleanup regression check.

## 0.0.23 - 2026-09-30

### Added

- Sign in with a ChatGPT subscription through `kiss login openai`. KISS saves
  the login and refreshes it when needed. Legacy Codex login remains available.
- Choose session tools with `defaultTools`. Replace the defaults or adjust
  them with `+name` and `-name` entries in user and trusted project settings.
- RPC prompt responses now report whether input started a turn or entered a
  queue. Steering and follow-up responses report queued input.

### Changed

- Updated the Pi baseline and model catalog to v0.99.1. Added GPT-6.1 Sol and
  Claude Sonnet 5.5, selected GPT-6.1 Sol as the Codex default, and updated
  Fireworks, Together, and OpenCode Go to Kimi K3.
- Made Jev compaction and dynamic reasoning generally available. Both remain
  opt-in through `/settings` and require TypeSafe credentials.
- Updated the README with clearer login, tool selection, and setup guidance.

### Fixed

- Reject incomplete Responses tool streams before tools run. Keep MCP error
  content and details, and preserve reasoning separately from answer text.
- Apply model sampling defaults and request overrides. Calculate OpenAI costs
  from the reported service tier.
- Stop browser login after an authorization denial. Show the ChatGPT usage
  page for subscription limits and retry temporary subscription errors.
- Accept null optional read limits, opening wrappers in file completion, and
  short `#rgb` theme colors.

## 0.0.22 - 2026-09-28

### Fixed

- Wrap long Markdown table cells across lines instead of truncating them with an ellipsis.

## 0.0.20 - 2026-09-26

### Added

- Prebuilt Python wheels for CPython 3.11–3.15, including free-threaded
  3.14t and 3.15t, on the existing Linux, macOS, and Windows targets.

### Fixed

- Publish the Rust and Node SDKs as `kiss-agent-sdk` to match PyPI. Rename the
  internal AI crate to `kiss-agent-ai` to avoid a crates.io name owned by
  another publisher, and the browser package to `kiss-agent-sdk-wasm`.
- Build npm packages with Bun and publish them with npm trusted publishing;
  announce the GitHub Release only after all registries accept the packages.
- Avoid a macOS linker incompatibility with newer SDKs.

Retagging does not replace the previously uploaded PyPI 0.0.20 wheels or
remove the already published PyPI and crates.io 0.0.21 packages.

## 0.0.19 - 2026-09-26

### Added

- Published the Rust SDK to crates.io, the native Node SDK to npm using Bun,
  and Python SDK wheels to PyPI using trusted publishing on release tags.
- Added local voice dictation and streaming Deepgram dictation with ElevenLabs
  playback in the interactive editor.
- Improved fresh-install and first-launch experience.

### Fixed

- Identified MCP servers and tools in TUI call titles.

## 0.0.18 - 2026-09-23

### Added

- Added Meta Model API keys and Muse subscription device login.
- Updated the Pi model catalog with Claude Opus 5.5, GPT-6 Sol and Luna, Grok
  4.7, and Meta Muse models.

### Changed

- Moved Jev reasoning selection to a checkpoint before each model generation,
  including the first response. Kept leased choices for later generations.
- Updated Pi compatibility to v0.87.1, including Grok 4.7 and Muse Spark 1.3
  provider defaults, Claude Code 2.1.280 request compatibility, and
  continuation-oriented split-turn compaction summaries.

### Fixed

- Kept the ACP `Cancelled` stop reason when a task is cancelled during Jev
  preparation.
- Rejected stale Jev choices after a model or effort change, and showed the
  saved-effort fallback when Jev could not select an effort.
- Omitted empty text parts from image-only OpenAI-compatible messages, rejected
  invalid prompt frontmatter with a warning, and skipped late cache refreshes
  that would miss the existing prompt cache.

## 0.0.17 - 2026-09-22

### Changed

- Expanded TypeSafe Jev reasoning-effort selection to every
  reasoning model and provider, using each model's supported effort levels and
  existing provider-native mappings.

## 0.0.16 - 2026-09-22

### Added

- Added TypeSafe Jev reasoning-effort selection for GPT-6 Astra,
  with bounded runtime context, reusable generation leases, early reassessment
  after tool failures or new user input, and safe fixed-effort fallback.
- Added colored conversation-pane notices when Jev changes reasoning effort,
  using the same effort colors as the TUI input border.

### Fixed

- Hid unsupported native terminal progress signals in Ghostty.

## 0.0.15 - 2026-09-20

### Added

- Added short, automatically generated session descriptions to terminal tabs
  while preserving explicit session names.

### Changed

- Polished project wording across user messages, documentation, and release
  configuration.

## 0.0.14 - 2026-09-20

### Added

- Added Databricks Unity Gateway, Azure Databricks, and Snowflake Cortex
  providers with bearer-token login, account URLs, and complete model catalogs.
- Added KISS-branded idle and working terminal-tab titles, plus native terminal
  progress signals for supported terminal emulators.
- Updated Pi compatibility to v0.86.0 with the current provider catalog,
  offline Radius models, prompt-cache lifetime data and cost-aware warming,
  per-model compaction budgets, and a `/bug` shortcut to the GitHub issue form.

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

- Added TypeSafe Jev compaction, with TypeSafe API-key login,
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
