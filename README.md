# KISS

KISS is based on the
[Keep it simple, stupid](https://en.wikipedia.org/wiki/KISS_principle)
principle.

A fast terminal coding agent built in Rust and based on
[Pi](https://github.com/earendil-works/pi).

KISS gives you one focused interface for coding with OpenAI Codex, Anthropic,
Google, OpenRouter, Bedrock, GitHub Copilot, and other model providers. It
supports local tools, persistent sessions, OAuth login, and MCP without a
plugin runtime.

## Why KISS

- **Fast:** one native binary with a responsive terminal interface.
- **Flexible:** more than 1,000 models from the Pi model catalog.
- **Focused:** `read`, `write`, `edit`, and `bash` are the default tools.
- **Persistent:** resume, branch, compact, import, and export sessions.
- **Connected:** use local or remote MCP servers, including OAuth servers.

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

For any other language, `kiss --mode rpc --no-session` accepts JSON commands on
stdin and streams JSON responses/events on stdout.

See [SDK documentation](docs/sdk.md), [RPC protocol documentation](docs/rpc.md),
and the [FX WebAssembly architecture review](docs/fx-wasm-review.md).

## Performance

KISS is built to stay responsive during everyday work, from file discovery in
large repositories to streaming model output. These benchmarks measure local
KISS operations, not model or network latency.

### Benchmarks

| User action | Test size | Typical time | p95 |
| --- | --- | ---: | ---: |
| File search | 100,000 files, three warm queries | 5.724 ms | 6.238 ms |
| File search | 500,000 files, three warm queries | 12.515 ms | 14.315 ms |
| SSE parsing | 10,000 events | 1.540 ms | 1.707 ms |
| Grep | 1,000 files and 200 matches | 15.251 ms | 18.772 ms |
| Incremental Markdown | 200 streaming prefix renders | 17.557 ms | 18.032 ms |
| Unchanged frame | 10,000 logical rows | 156.370 us | 157.783 us |

### Subagent overhead

Subagents are off by default, so standard sessions do not load their six
control tools. Three trials measured this local overhead:

| Measure | State | Median range | p95 range |
| --- | --- | ---: | ---: |
| Session setup | Off | 305.665-328.389 us | 338.670-348.442 us |
| Session setup | On | 306.161-327.709 us | 335.922-341.570 us |
| Request preparation | Off | 225-239 ns | 231-248 ns |
| Request preparation | On | 325-347 ns | 341-393 ns |

Session setup had no repeatable slowdown. Request preparation added 100-108
ns when the six control tools were present and stayed below 0.4 us in total.

### Dynamic workflow overhead

Workflow benchmarks use an instant local agent runner. They measure
orchestration only and do not include model or network time. The table shows
the range across three trials:

| Measure | Test size | Median range | p95 range |
| --- | --- | ---: | ---: |
| Script parsing | 200-line script | 73.028-79.245 us | 77.136-80.637 us |
| Interpreter | 1,000 agent calls | 2.703-2.930 ms | 3.268-3.672 ms |
| Progress snapshot | 500 agents, 5 phases | 53.576-58.055 us | 84.903-89.773 us |
| Phase view | 500 agents, 5 phases | 11.380-18.821 us | 12.196-20.901 us |
| Agent detail view | One prompt and result | 5.063-8.038 us | 5.156-8.227 us |
| Unchanged view | 500 agents, cached | 344-509 ns | 392-638 ns |
| Request preparation | Workflow disarmed | 337-359 ns | 341-400 ns |
| Request preparation | Workflow armed | 1.051-1.087 us | 1.064-1.185 us |

The interpreter used 2.70-2.93 us per agent call. Arming a workflow added
707-728 ns to request preparation. The workflow tool and its instructions are
absent until a workflow turn is armed.

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

The core, subagent, and workflow results used an Apple M4, macOS 26.5.1, and
Rust 1.98.0 on 2026-09-01. Core benchmarks used the Cargo `release` profile.
The table reports the median result from three trials. Each core benchmark
used 9-15 samples per trial. The subagent and workflow comparisons also used
three trials, with 21 samples for matched feature comparisons and 9-21 samples
for workflow operations. These tests did not call a model or start a real
child session. The complete run passed all 17 performance tests. The optimized
release comparison used three held-out trials on 2026-08-31. Lower latency
values are better.

Run the full benchmark suite with `just bench`.

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
- Press `Ctrl+D` on an empty input to exit.
- Use the Up arrow to restore earlier prompts.

Useful commands include `/login`, `/model`, `/mcp`, `/compact`, `/resume`,
`/export`, `/settings`, and `/hotkeys`.

KISS can accept a new instruction while the agent works. Press `Enter` to
steer the current task, or `Alt+Enter` to queue a follow-up task.

## Subagents

Subagents are off by default. Open `/settings` and change `Subagents` to `on`
to give the main agent these control tools: `spawn_agent`, `send_message`,
`followup_task`, `wait_agent`, `list_agents`, and `interrupt_agent`.

Each child is a separate KISS session, but it uses the same working directory.
KISS allows four active child turns and one child level. A child starts with
fresh context unless the main agent explicitly copies parent turns. Project
settings cannot enable this feature. `--no-tools` also keeps it off.

See [subagents.md](subagents.md) for the design analysis and tradeoffs.

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

While a run is going, a progress line appears under the transcript. Press
`Ctrl+W`, or run `/workflows`, to open the full view:

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

KISS currently tracks [Pi v0.84.4](https://github.com/earendil-works/pi/releases/tag/v0.84.4).
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
