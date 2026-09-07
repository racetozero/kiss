# KISS

KISS is based on the
[Keep it simple, stupid](https://en.wikipedia.org/wiki/KISS_principle)
principle. Why? Because I am stupid :)

A fast terminal coding agent built in Rust and based on
[Pi](https://github.com/earendil-works/pi).

KISS gives you one focused interface for coding with OpenAI Codex, Anthropic,
Google, OpenRouter, Bedrock, GitHub Copilot, and other model providers. It
supports local tools, persistent sessions, OAuth login, and MCP without a
plugin runtime.

## Why KISS

- **Fast:** a native binary that reaches its first warm frame in about 5 ms.
- **Flexible:** more than 1,000 models from the Pi model catalog.
- **Focused:** `read`, `write`, `edit`, and `bash` are the default tools.
- **Persistent:** resume, branch, compact, import, and export sessions.
- **Connected:** use local or remote MCP servers, including OAuth servers.
- **Readable:** Catppuccin Mocha is the default dark theme.

## Install

macOS and Linux:

```bash
curl -LsSf https://raw.githubusercontent.com/racetozero/kiss/main/install.sh | sh
```

Windows:

```powershell
powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/racetozero/kiss/main/install.ps1 | iex"
```

The installer selects the correct release, verifies its SHA-256 checksum, and
installs `kiss` in your user binary directory.

Update an existing installation with:

```bash
kiss update
```

## Quick start

Sign in with your ChatGPT subscription and start KISS:

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

Use KISS interactively or for one task:

```bash
kiss "explain this repository"
kiss -p "summarize the current changes"
cat error.log | kiss -p "find the cause"
```

## Terminal workflow

- Type `/` to find commands.
- Type `@` to find and attach files.
- Type `!command` to run a shell command.
- Press `Shift+Tab` to change reasoning effort.
- Press `Esc` or `Ctrl+C` to stop active work.
- Press `Ctrl+R` to expand or collapse long tool results.
- Press `Ctrl+D` on an empty input to exit.
- Use the Up arrow to restore earlier prompts.

Assistant Markdown uses bold text, cyan underlined terminal hyperlinks,
automatic links for bare web URLs, and syntax colors for fenced code.

Useful commands include `/login`, `/model`, `/mcp`, `/compact`, `/resume`,
`/loop`, `/autoresearch`, `/jobs`, `/provider`, `/export`, `/settings`, and
`/hotkeys`.

KISS can accept a new instruction while the agent works. Press `Enter` to
steer the current task, or `Alt+Enter` to queue a follow-up task.

### Resume sessions from other coding agents

Run `/resume` to continue a KISS, Pi, Claude Code, or OpenAI Codex session in
KISS. The picker shows sessions for the current working directory by default.
Press `Ctrl+G` to switch between the project and global scopes.

## Use KISS as an SDK

KISS can be embedded in Rust, Python 3.11+, TypeScript on Node/Bun/Deno, or a
browser application through WebAssembly. All SDKs share one Rust dispatcher and
the same streaming event protocol.

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

For browsers, `@kiss-sdk/core-wasm` runs the agent conversation and model/tool
loop directly inside WebAssembly using explicit JavaScript model and tool
capabilities—no KISS server or WebSocket is required. `@kiss-sdk/wasm` remains
the remote client when an application specifically needs native filesystem and
shell tools.

### RPC

For any other language, `kiss --mode rpc --no-session` accepts JSON commands on
stdin and streams JSON responses and events on stdout.

### RPC over WebSocket

For browser and remote clients, start a WebSocket server:

```bash
kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session
```

Connect to `ws://127.0.0.1:9944`. WebSocket clients use the same JSON messages
as standard input and output. All clients share one KISS session.

See [SDK documentation](docs/sdk.md), [RPC protocol documentation](docs/rpc.md),
and the [browser WebAssembly documentation](crates/kiss-core-wasm/README.md).

## Performance

KISS is built to stay responsive during everyday work, from file discovery in
large repositories to streaming model output. These benchmarks measure local
KISS operations, not model or network latency.

### Harness startup and memory

These measurements use the current KISS release build. They cover idle memory,
time to the first frame, and time until typed input appears.

| Measure | KISS result |
| --- | ---: |
| Warm time to first frame | 5.037 ms mean |
| Warm time to first input | 5.094 ms mean |
| One idle session | 15.802 MiB mean RSS |
| Ten idle sessions | 159.010 MiB mean RSS |
| Extra RSS per added session | 15.912 MiB mean |

The startup results use ten launches after one warm-up. The memory results use
three trials. RSS is the resident memory reported by macOS; it is not Linux
proportional set size (PSS), so do not compare the memory values directly
across operating systems.

### Benchmarks

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

These hermetic benchmarks isolate local SDK and transport overhead. The native
SDK and RPC paths use `ping`; the browser benchmark runs a complete agent turn
against an immediate host model callback. No benchmark calls an external model.
The table shows means from the latest full-suite run:

| Surface | Work | Mean |
| --- | --- | ---: |
| Native SDK | Shared in-process command dispatch | 113 ns |
| JSONL RPC | Client encode/decode, in-memory duplex, server dispatch | 14.439 us |
| Browser WASM | Warm full-agent prompt, 100 samples | 0.077 ms |
| Browser WASM | 25 isolated agents in parallel, 11 batches | 0.749 ms |

Fresh WASM module initialization averaged 11.269 ms per Deno process. The
release module is 574,734 bytes raw and 209,375 bytes gzip, with 17 initial
linear-memory pages (1,114,112 bytes). Model and tool callback time will
normally dominate these local costs.

### Subagent overhead

Subagents are off by default, so standard sessions do not load their six
control tools. The latest full-suite run measured this local overhead:

| Measure | State | Mean | p95 |
| --- | --- | ---: | ---: |
| Session setup | Off | 358.845 us | 364.466 us |
| Session setup | On | 358.352 us | 361.740 us |
| Request preparation | Off | 171 ns | 177 ns |
| Request preparation | On | 242 ns | 249 ns |

Session setup had no measured slowdown. Request preparation added 71 ns when
the six control tools were present and stayed below 0.25 us in total.

### Dynamic workflow overhead

Workflow benchmarks use an instant local agent runner. They measure
orchestration only and do not include model or network time. The table shows
the latest full-suite means:

| Measure | Test size | Mean | p95 |
| --- | --- | ---: | ---: |
| Script parsing | 200-line script | 57.884 us | 59.922 us |
| Interpreter | 1,000 agent calls | 2.185 ms | 2.423 ms |
| Progress snapshot | 500 agents, 5 phases | 43.075 us | 64.816 us |
| Phase view | 500 agents, 5 phases | 11.298 us | 12.207 us |
| Agent detail view | One prompt and result | 4.886 us | 5.021 us |
| Unchanged view | 500 agents, cached | 311 ns | 317 ns |
| Request preparation | Workflow disarmed | 262 ns | 274 ns |
| Request preparation | Workflow armed | 913 ns | 934 ns |

The interpreter used 2.185 us per agent call. Arming a workflow added 651 ns
to request preparation. The workflow tool and its instructions are
absent until a workflow turn is armed.

### Iterative job overhead

Loop and autoresearch jobs sleep between model turns and update the terminal
through small version events. Rendering a job detail view with a long goal and
result averaged 45.193 us (46.595 us p95). Model, tool, and verification time
will normally be much larger.

### TUI rendering and resize

The terminal user interface combines rapid resize events and redraws once 75
ms after the final change. The following release-mode results measure local
rendering. They do not include terminal parsing or remote connection time:

| Measure | Test size | Mean | p95 |
| --- | --- | ---: | ---: |
| Full renderer | 1,800 logical rows | 0.398 ms | 0.436 ms |
| Unchanged renderer | 10,000 logical rows | 0.876 ms | 0.885 ms |
| Last-row update | 10,000 logical rows | 0.895 ms | 0.916 ms |
| Cached transcript render | 2,885 logical rows | 0.058 ms | 0.063 ms |
| Spinner transcript render | 2,885 logical rows | 0.055 ms | 0.057 ms |
| Full resize redraw | 1,800 logical rows | 0.435 ms | 0.477 ms |

The full resize redraw wrote 178,231 bytes. This output volume is why KISS
waits for the final stable size instead of replaying the transcript for every
intermediate size.

### Release builds

KISS release binaries use profile-guided optimization. On macOS, this made the
binary smaller and produced a modest latency improvement:

| Measure | Standard build | Optimized build | Change |
| --- | ---: | ---: | ---: |
| `kiss --help` startup | 3.696 ms | 3.676 ms | 0.52% faster |
| Geometric mean latency | 1.000x | 0.985x | 1.51% faster |
| Executable size | 17.16 MiB | 14.87 MiB | 13.37% smaller |
| gzip size | 8.17 MiB | 7.36 MiB | 9.94% smaller |

### Method

Results are based on release-mode runs using local, deterministic fixtures—no
external models or network calls. The harness startup test used a 160 by 40 PTY
and `kiss --no-session` on macOS 26.5.1 with an Apple M4. Startup times are the
mean of ten warm launches. Memory values are the mean of three idle samples.
Core, SDK, RPC, and WASM benchmarks also ran on the Apple M4. PGO results use
separate held-out runs. Lower is better.

Run the full native and browser benchmark suite, including harness startup and
memory, WASM size, and memory budgets, with `just bench` (requires
cargo-nextest, wasm-pack, Deno, and Node). Harness results are also written to
`target/harness-benchmark.json`.

## Subagents

Subagents are off by default. Open `/settings` and change `Subagents` to `on`
to give the main agent these control tools: `spawn_agent`, `send_message`,
`followup_task`, `wait_agent`, `list_agents`, and `interrupt_agent`.

Each child is a separate KISS session, but it uses the same working directory.
KISS allows four active child turns and one child level. A child starts with
fresh context unless the main agent explicitly copies parent turns. Project
settings cannot enable this feature. `--no-tools` also keeps it off.

See [subagents.md](subagents.md) for the design analysis and tradeoffs.

## Loop and autoresearch jobs

Use a loop when a task needs more than one independent attempt:

```text
/loop make the parser tests pass --iterations 8
```

Use autoresearch when each attempt needs the same measurement:

```text
/autoresearch reduce Markdown render time; verify with the existing benchmark --iterations 20
```

Each job branches from the current conversation into a new KISS session. It
keeps its context between iterations and stops when the goal is complete or
the iteration limit is reached. A loop uses 10 iterations by default.
Autoresearch uses 25. The maximum explicit limit is 100.

Jobs run independently. Start another command while one runs, then use
`/jobs`, `/loop` without a goal, or `/autoresearch` without a goal to open the
job view.

| Key | Action |
| --- | --- |
| Up or Down | Select a job or scroll its latest result |
| Enter or Right | Open the selected job |
| `p` | Pause or resume between iterations |
| `x` | Stop the selected job |
| Escape or Left | Return to the job list or close it |

Autoresearch asks the child session to set a repeatable baseline, test one
small change per iteration, keep improvements, and revert regressions. It uses
the normal KISS tools and does not add a permission step.

## Dynamic workflows

A dynamic workflow is a short script that starts many child agents, collects
their answers, and returns one result. The model writes the script; KISS runs
it. The plan lives in the script instead of the model's context window, so one
run can coordinate far more agents than a conversation can, and only the final
answer comes back.

Workflows are built on subagents, so turn `Subagents` on first. The
`Dynamic workflows` row in `/settings` then shows `on`.

Start one in any of three ways:

```text
/workflow audit every tool file for missing path checks
use a workflow to compare the provider adapters
/audit-routes crates/kiss-coding/src/tools
```

- `/workflow <prompt>` runs one task as a workflow.
- A trigger in your own prompt does the same: `use a workflow`, `run a
  workflow`, `as a workflow`, `dynamic workflow`, or `ultracode`. Only whole
  words count, so `src/workflows/run.rs` does not trigger it.
- A workflow you saved runs as `/<name>`. Text after the name reaches the
  script as `args`. JSON becomes structured data; other text stays a string.

Before a run starts, KISS shows its phases and how many agents it will start,
and asks you to approve. When the script's agent count depends on data it has
not fetched yet, the prompt says so rather than guessing. Set
`workflows.confirm` to `false` in your settings file to start runs
immediately, and `workflows.size` to advise how many agents a script should
aim for.

While a run is going, a progress line appears under the transcript. Run
`/workflows` to open the full view:

| Key | Action |
| --- | --- |
| `↑` `↓` | Select a phase, then an agent |
| `Enter` or `→` | Open the selection |
| `Esc` or `←` | Back out one level, or close |
| `j` `k` | Scroll an agent's output |
| `f` | Filter the agent list by status |
| `p` | Pause or resume the run |
| `x` | Stop the selected agent, or the whole run |
| `r` | Restart a queued or running agent |
| `s` | Save the run's script as a command |

Saving writes to `.kiss/workflows/` in the project, or
`~/.kiss/agent/workflows/` for every project. Run `/reload` and the workflow
appears as `/<name>`.

One run starts at most 1,000 agents, with up to 16 at once and up to 4,096
items in a single fan-out. A script that exceeds a limit fails rather than
quietly doing less. A script cannot read a file, use the network, load a
module, or start a process; only its child agents use KISS tools. Scripts also
cannot read the clock or a random number, which is what lets a stopped run
resume: replaying it asks for the same agents, so finished work is reused
instead of repeated.

See [Dynamic workflow overhead](#dynamic-workflow-overhead) for the current
local performance measurements.

## Models and login

KISS supports browser and headless OAuth for OpenAI Codex and Anthropic. It
also supports API keys, environment variables, cloud credentials, and
compatible credentials from OpenAI Codex, Claude Code, Pi, OpenCode,
OpenClaw, and Hermes.

```bash
kiss login openai-codex
kiss login anthropic --device-auth
kiss login anthropic --api-key YOUR_KEY
kiss auth
kiss logout openai-codex
kiss --list-models
kiss --model sonnet:high
```

### Custom OpenAI-compatible providers

Add a provider for Chat Completions, the Responses API, or the Codex Responses
API. The new model then works with the normal model selector.

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
read a proxy key from an environment variable, or run
`kiss login <provider> --api-key KEY` to save a key.

CodexLB can reuse the OpenAI Codex login already stored by KISS:

```bash
kiss login openai-codex
kiss provider add codex-lb \
  --base-url http://127.0.0.1:2455/backend-api/codex \
  --api codex \
  --model gpt-5.6-sol \
  --reasoning
kiss --model codex-lb/gpt-5.6-sol
```

If CodexLB requires its own key, add `--api-key-env CODEX_LB_API_KEY`.
Use `--header KEY=VALUE` more than once when a gateway needs custom headers.

The TUI has the same basic operations:

```text
/provider add <id> <chat-completions|responses|codex> <base-url> <model> [KEY_ENV|auth:<provider>]
/provider list
/provider remove <id>
```

Restart the TUI after an add or remove operation so the current session loads
the changed model catalog.

### Themes

KISS uses Catppuccin Mocha as its default dark theme. Open `/settings` to
switch between the built-in dark and light themes. A custom theme can still be
selected in `~/.kiss/agent/settings.json`.

## MCP

Add local or remote MCP servers from the terminal:

```bash
kiss mcp add local -- npx -y @modelcontextprotocol/server-everything
kiss mcp add remote --url https://example.com/mcp --auth oauth
kiss mcp login remote
kiss mcp list
kiss mcp test local
```

Use `--scope project` to save a server in `.mcp.json`. Use
`kiss mcp login remote --no-browser` for a headless OAuth flow. The `/mcp`
command manages servers inside the TUI.

## Configuration

KISS stores user configuration in `~/.kiss/agent`. Project configuration is
loaded only after you trust the project.

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

Run `kiss --help` for command-line options and `/settings` for common TUI
settings.

## Pi compatibility

KISS currently tracks [Pi v0.85.1](https://github.com/earendil-works/pi/releases/tag/v0.85.1).
It keeps Pi-compatible session files, model data, core commands, compaction,
and OpenAI Codex WebSocket transport. The tracked Pi release is recorded in
`Cargo.toml`.

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
2. Add the release notes to `CHANGELOG.md`.
3. Run `cargo check --workspace` to update `Cargo.lock`.
4. Run `just release-check VERSION`.
5. Commit the version, lock file, and changelog.
6. Run `just release VERSION` from a clean `main` branch.

The release command runs all checks, creates the tag, and starts the GitHub
release workflow.

## License

MIT. KISS is inspired by [Pi](https://github.com/earendil-works/pi), which is
also licensed under MIT.
