# KISS

A fast terminal coding agent that keeps the interface simple and gives you
control of the model, tools, sessions, and automation.

KISS works with OpenAI Codex, Anthropic, Google, OpenRouter, Bedrock, GitHub
Copilot, and OpenAI-compatible providers. It is built in Rust and based on
[Pi](https://github.com/earendil-works/pi).

## Why KISS

- **Start quickly.** The native terminal interface reaches its first warm frame
  in about 5 ms.
- **Keep your work.** Resume, branch, compact, import, and export persistent
  sessions.
- **Use your preferred model.** Choose from more than 1,000 catalog models or
  add your own compatible provider.
- **Automate long tasks.** Run scheduled loops, measured autoresearch, parallel
  subagents, and dynamic workflows.
- **Connect your tools.** Use local and remote MCP servers, including OAuth
  servers.
- **Build on it.** Embed KISS through Rust, Python, TypeScript, WebAssembly,
  JSONL RPC, or WebSocket RPC.

KISS uses four focused tools by default: `read`, `write`, `edit`, and `bash`.
Catppuccin Mocha is the default dark theme.

## Install

macOS and Linux:

```bash
curl -LsSf https://raw.githubusercontent.com/racetozero/kiss/main/install.sh | sh
```

Windows, including ARM64:

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/racetozero/kiss/main/install.ps1 | iex"
```

The installer selects the correct release, verifies its SHA-256 checksum, and
installs `kiss` in your user binary directory.

Update later with:

```bash
kiss update
```

## Start in two commands

Sign in with a ChatGPT subscription and open KISS:

```bash
kiss login openai-codex
kiss
```

For a server or SSH session:

```bash
kiss login openai-codex --device-auth
```

Anthropic login and credential import are also available:

```bash
kiss login anthropic
kiss auth import
```

Use the interactive terminal or run one task:

```bash
kiss "explain this repository"
kiss -p "summarize the current changes"
cat error.log | kiss -p "find the cause"
```

## Work in the terminal

- Type `/` to find commands.
- Type `@` to find and attach files.
- Type `!command` to run a shell command.
- Press `Shift+Tab` to change reasoning effort.
- Press `Esc` or `Ctrl+C` to stop active work.
- Press `Ctrl+R` to expand or collapse long tool results.
- Press `Ctrl+D` on an empty input to exit.
- Use the Up arrow to restore earlier prompts.

KISS renders bold Markdown, cyan underlined terminal links, bare web links, and
syntax colors for fenced code.

Send a new instruction while the agent works. Press `Enter` to steer the
current task, or `Alt+Enter` to queue the instruction for later.

Useful commands include `/login`, `/model`, `/mcp`, `/compact`, `/resume`,
`/loop`, `/autoresearch`, `/jobs`, `/provider`, `/export`, `/settings`, and
`/hotkeys`.

## Continue work from another agent

Run `/resume` to continue a KISS, Pi, Claude Code, or OpenAI Codex session.
The picker starts with sessions from the current working directory. Press
`Ctrl+G` to switch between project and global results.

## Automate long tasks

### Loop and autoresearch

Use a loop for repeated work. With no limit, it runs until the goal is complete
or you stop it:

```text
/loop make the parser tests pass
```

Put an interval before the goal to wait between turns. The first turn starts
immediately. Compound intervals support days, hours, minutes, seconds,
milliseconds, microseconds, and nanoseconds:

```text
/loop 15m check the deployment and fix new errors
/loop 2d4h review dependency updates
```

Use `--iterations` for a fixed number of turns:

```text
/loop make the parser tests pass --iterations 8
```

Autoresearch establishes a baseline, tests one small change at a time, keeps
improvements, and reverts regressions. It is also unlimited by default:

```text
/autoresearch reduce Markdown render time; verify with the existing benchmark
/autoresearch reduce Markdown render time; verify with the existing benchmark --iterations 20
```

A loop interval and `--iterations` are mutually exclusive. Autoresearch does
not accept an interval. The maximum explicit iteration limit is 100.

Each job branches from the current conversation into a persistent KISS
session. Run `/jobs`, `/loop` without a goal, or `/autoresearch` without a goal
to manage jobs.

| Key | Action |
| --- | --- |
| Up or Down | Select a job or scroll its latest result |
| Enter or Right | Open the selected job |
| `p` | Pause or resume between iterations |
| `x` | Stop the selected job |
| Escape or Left | Return or close the view |

### Subagents

Subagents let one task branch into focused child sessions. Open `/settings`
and set `Subagents` to `on`. KISS then gives the main agent tools to start,
guide, wait for, and stop child agents.

Each child uses the same working directory. KISS allows four active child turns
and one child level. Project settings cannot enable subagents, and `--no-tools`
keeps them off.

### Dynamic workflows

A dynamic workflow coordinates many child agents with a short generated
script. The script holds the plan, while only final results return to the main
conversation. Enable subagents first, then use:

```text
/workflow audit every tool file for missing path checks
use a workflow to compare the provider adapters
```

Run `/workflows` to inspect, pause, resume, stop, restart, or save a workflow.
A saved workflow becomes a reusable slash command after `/reload`.

One workflow can start up to 1,000 agents, with 16 active at once. Workflow
scripts cannot read files, use the network, load modules, or start processes.
Only their child agents use KISS tools.

## Models and integrations

### Login and model selection

KISS supports browser and headless OAuth, API keys, environment variables, and
cloud credentials. It can import compatible credentials from OpenAI Codex,
Claude Code, Pi, OpenCode, OpenClaw, and Hermes.

```bash
kiss login openai-codex
kiss login anthropic --device-auth
kiss login anthropic --api-key YOUR_KEY
kiss auth
kiss logout openai-codex
kiss --list-models
kiss --model sonnet:high
```

### Your own OpenAI-compatible provider

Add a Chat Completions, Responses, or Codex Responses provider. The model then
appears in the normal model selector.

```bash
kiss provider add local \
  --base-url http://127.0.0.1:8000/v1 \
  --api chat-completions \
  --model local-model
kiss provider list
kiss --model local/local-model
kiss provider remove local
```

Use `--api responses` for a Responses API server. Use `--api-key-env NAME` to
read a key from an environment variable, or use `kiss login <provider>
--api-key KEY` to save one.

CodexLB can reuse the OpenAI Codex login stored by KISS:

```bash
kiss login openai-codex
kiss provider add codex-lb \
  --base-url http://127.0.0.1:2455/backend-api/codex \
  --api codex \
  --model gpt-5.6-sol \
  --reasoning
kiss --model codex-lb/gpt-5.6-sol
```

Use `--api-key-env CODEX_LB_API_KEY` when CodexLB needs its own key. Repeat
`--header KEY=VALUE` when a gateway needs custom headers.

The TUI supports the same basic operations:

```text
/provider add <id> <chat-completions|responses|codex> <base-url> <model> [KEY_ENV|auth:<provider>]
/provider list
/provider remove <id>
```

Restart the TUI after an add or remove operation so the current session loads
the changed model catalog.

### MCP

Add local or remote MCP servers:

```bash
kiss mcp add local -- npx -y @modelcontextprotocol/server-everything
kiss mcp add remote --url https://example.com/mcp --auth oauth
kiss mcp login remote
kiss mcp list
kiss mcp test local
```

Use `--scope project` to save a server in `.mcp.json`. Use `kiss mcp login
remote --no-browser` for headless OAuth. Use `/mcp` to manage servers in the
TUI.

## Build with KISS

KISS provides SDKs for Rust, Python 3.11+, TypeScript on Node, Bun, and Deno,
and browser applications through WebAssembly. All SDKs use the same streaming
event protocol.

```rust
let session = kiss_sdk::Session::builder().tools(["read", "bash"]).build().await?;
session.prompt("What files are here?").await?;
```

```python
async with await kiss_sdk.Session.create(tools=[kiss_sdk.ToolName.READ]) as session:
    await session.prompt("What files are here?")
```

```typescript
const session = await Session.create({ tools: ["read", "bash"] });
await session.prompt("What files are here?");
```

`@kiss-sdk/core-wasm` runs the full agent and model/tool loop in a browser. It
does not need a KISS server. `@kiss-sdk/wasm` is the remote client for
applications that need native filesystem and shell tools.

### JSONL RPC

For other languages, start JSONL RPC over standard input and output:

```bash
kiss --mode rpc --no-session
```

### RPC over WebSocket

For browser and remote clients, start the WebSocket transport:

```bash
kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session
```

Connect to `ws://127.0.0.1:9944`. WebSocket clients use the same JSON commands
and events as JSONL RPC. All connected clients share one KISS session.

See the [SDK guide](docs/sdk.md), [RPC protocol](docs/rpc.md), and
[browser WebAssembly guide](crates/kiss-core-wasm/README.md).

## Performance

KISS benchmarks local work, not model or network latency. Results below are
from the latest full release benchmark run.

### Startup and memory

| Measure | Mean |
| --- | ---: |
| Warm time to first frame | 5.037 ms |
| Warm time to first input | 5.094 ms |
| One idle session | 15.802 MiB RSS |
| Ten idle sessions | 159.010 MiB RSS |
| Extra RSS per added session | 15.912 MiB |

Startup results use ten launches after one warm-up. Memory results use three
trials. RSS is the resident memory reported by macOS. Do not compare these
values directly with Linux proportional set size.

### Core operations

| User action | Test size | Mean | p95 |
| --- | --- | ---: | ---: |
| File search | 100,000 files, three warm queries | 4.606 ms | 4.889 ms |
| File search | 500,000 files, three warm queries | 7.110 ms | 7.690 ms |
| SSE parsing | 10,000 events | 0.931 ms | 0.973 ms |
| Grep | 1,000 files and 200 matches | 6.933 ms | 8.340 ms |
| Incremental Markdown | 200 streaming prefix renders | 12.427 ms | 12.576 ms |
| Rust syntax highlighting | One 200-line fence | 5.830 ms | 6.165 ms |
| Unchanged frame | 10,000 logical rows | 0.876 ms | 0.885 ms |

### SDK, RPC, and browser WebAssembly

These tests use an immediate local model or `ping`. They do not call an
external model.

| Surface | Work | Mean |
| --- | --- | ---: |
| Native SDK | Shared in-process command dispatch | 113 ns |
| JSONL RPC | Encode, decode, in-memory transport, and dispatch | 14.439 us |
| Browser WASM | Warm full-agent prompt, 100 samples | 0.077 ms |
| Browser WASM | 25 isolated agents in parallel, 11 batches | 0.749 ms |

Fresh WebAssembly module initialization averaged 11.269 ms per Deno process.
The release module is 574,734 bytes raw and 209,375 bytes gzip. It starts with
17 linear-memory pages, or 1,114,112 bytes.

### Agent automation

Subagents are off by default. Session setup had no measured slowdown when they
were enabled. Their six control tools added 71 ns to request preparation.

| Measure | State or size | Mean | p95 |
| --- | --- | ---: | ---: |
| Subagent session setup | Off | 358.845 us | 364.466 us |
| Subagent session setup | On | 358.352 us | 361.740 us |
| Request preparation | Subagents off | 171 ns | 177 ns |
| Request preparation | Subagents on | 242 ns | 249 ns |
| Workflow script parsing | 200 lines | 57.884 us | 59.922 us |
| Workflow interpreter | 1,000 agent calls | 2.185 ms | 2.423 ms |
| Workflow progress snapshot | 500 agents, 5 phases | 43.075 us | 64.816 us |
| Workflow phase view | 500 agents, 5 phases | 11.298 us | 12.207 us |
| Workflow agent detail | One prompt and result | 4.886 us | 5.021 us |
| Workflow unchanged view | 500 agents, cached | 311 ns | 317 ns |
| Job detail view | Long goal and result | 45.193 us | 46.595 us |

The workflow interpreter used 2.185 us per agent call. Arming a workflow added
651 ns to request preparation and kept the total below 1 us. Loop and
autoresearch jobs sleep between model turns and update the TUI through small
events.

### TUI rendering and resize

| Measure | Test size | Mean | p95 |
| --- | --- | ---: | ---: |
| Full renderer | 1,800 logical rows | 0.398 ms | 0.436 ms |
| Unchanged renderer | 10,000 logical rows | 0.876 ms | 0.885 ms |
| Last-row update | 10,000 logical rows | 0.895 ms | 0.916 ms |
| Cached transcript render | 2,885 logical rows | 0.058 ms | 0.063 ms |
| Spinner transcript render | 2,885 logical rows | 0.055 ms | 0.057 ms |
| Full resize redraw | 1,800 logical rows | 0.435 ms | 0.477 ms |

KISS combines rapid resize events and redraws once 75 ms after the final
change. The full resize test wrote 178,231 bytes.

### Profile-guided release builds

| Measure | Standard build | Optimized build | Change |
| --- | ---: | ---: | ---: |
| `kiss --help` startup | 3.696 ms | 3.676 ms | 0.52% faster |
| Geometric mean latency | 1.000x | 0.985x | 1.51% faster |
| Executable size | 17.16 MiB | 14.87 MiB | 13.37% smaller |
| gzip size | 8.17 MiB | 7.36 MiB | 9.94% smaller |

### Method

The tests use release builds and local deterministic fixtures. The startup
test used a 160 by 40 terminal and `kiss --no-session` on macOS 26.5.1 with an
Apple M4. Startup values are the mean of ten warm launches. Memory values are
the mean of three idle samples. Core, SDK, RPC, and WebAssembly tests also ran
on the Apple M4. Profile-guided results use separate held-out runs. Lower is
better.

Run the complete native and browser suite with `just bench`. It requires
cargo-nextest, wasm-pack, Deno, and Node. Harness results are written to
`target/harness-benchmark.json`.

## Configuration

KISS stores user configuration in `~/.kiss/agent`. It loads project
configuration only after you trust the project.

| Path | Purpose |
| --- | --- |
| `~/.kiss/agent/settings.json` | User settings |
| `.kiss/settings.json` | Project settings |
| `~/.kiss/agent/models.json` | Custom providers and models |
| `~/.kiss/agent/mcp.json` | User MCP servers |
| `.mcp.json` | Project MCP servers |
| `~/.kiss/agent/skills/` | User skills |
| `.kiss/skills/` | Project skills |
| `~/.kiss/agent/workflows/` | Personal workflow scripts |
| `.kiss/workflows/` | Trusted project workflow scripts |

Open `/settings` for common TUI settings. Run `kiss --help` for all command-line
options. Custom themes live in `~/.kiss/agent/settings.json`.

## Compatibility

KISS tracks [Pi v0.85.1](https://github.com/earendil-works/pi/releases/tag/v0.85.1).
It keeps Pi-compatible session files, model data, core commands, compaction,
and OpenAI Codex WebSocket transport. `Cargo.toml` records the tracked release.

## Development

Use Rust stable and cargo-nextest:

```bash
cargo nextest run --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
just pgo-test
just pgo-bench
```

## Release

1. Set `[workspace.package].version` in `Cargo.toml`.
2. Add release notes to `CHANGELOG.md`.
3. Run `cargo check --workspace` to update `Cargo.lock`.
4. Run `just release-check VERSION`.
5. Commit the version, lock file, and changelog.
6. Run `just release VERSION` from a clean `main` branch.

The release command runs all checks, creates the tag, and starts the GitHub
release workflow.

## License

MIT. The name follows the
[Keep it simple, stupid](https://en.wikipedia.org/wiki/KISS_principle)
principle. KISS is inspired by Pi, which is also licensed under MIT.
