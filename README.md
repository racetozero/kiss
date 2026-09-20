# KISS

A fast terminal coding agent that keeps the interface simple and gives you
control of the model, tools, sessions, and automation.

KISS takes its name and product philosophy from [Keep It Simple, Stupid](https://en.wikipedia.org/wiki/KISS_principle).
Why? Because I am stupid :)

KISS has 43 built-in providers, including OpenAI Codex, Cursor, Anthropic,
Google, OpenRouter, Bedrock, Databricks, Snowflake, and GitHub Copilot. You can also add
OpenAI-compatible providers. KISS is built in Rust and based on
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
installs `kiss` in your user binary directory. On Linux, if the glibc
release needs a newer `GLIBC_*` version than the system provides, it
automatically installs the matching musl release.

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
`/loop`, `/autoresearch`, `/jobs`, `/provider`, `/export`, `/cache-usage`,
`/bug`, `/fast`, `/update`, `/settings`, and `/hotkeys`.

Use `/fast` to toggle the low-latency tier for a supported provider. This
setting applies only to the current session, and provider costs can increase.
Use `/update` to update the installed KISS binary.

### Track cache efficiency

Prompt caching can reduce the cost of repeated context. KISS shows the current
cache rate beside the session cost, so you can see when a workload benefits.

Run `/cache-usage` to see the current session trend. Use `/cache-usage all` to
check whether cache efficiency improves across saved sessions, or
`/cache-usage <provider>` to compare one provider. Every chart uses the same
scale, so changes are easy to compare.

Use the same report in scripts and CI:

```bash
kiss cache-usage
kiss cache-usage --provider anthropic
kiss cache-usage --session <session-id-or-jsonl-file>
```

KISS also protects valuable prompt caches during long-running work when a
refresh is expected to save money. This is automatic. Set `cacheWarming` to
`off` to disable refreshes, or to `idle` to protect the cache while you decide
what to do next. Model-aware context management and bounded retry delays keep
long sessions responsive without routine tuning.

## Continue work from another agent

Run `/resume` to continue a KISS, Pi, Claude Code, or OpenAI Codex session.
The picker starts with sessions from the current working directory. Press
`Ctrl+G` to switch between project and global results.

If KISS fails, run `/bug` to open the GitHub issue form. In a headless
environment, KISS prints
`https://github.com/racetozero/kiss/issues/new` instead.

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

| Key            | Action                                   |
| -------------- | ---------------------------------------- |
| Up or Down     | Select a job or scroll its latest result |
| Enter or Right | Open the selected job                    |
| `p`            | Pause or resume between iterations       |
| `x`            | Stop the selected job                    |
| Escape or Left | Return or close the view                 |

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

### Diagnose installation and network access

Run a full health report before you use a provider, or when a corporate
firewall stops a connection:

```bash
kiss doctor
kiss doctor --summary
```

The full report shows each provider connection type, host, port, HTTP result,
and elapsed time. It lists separate SSE, WebSocket, AWS event-stream, and login
destinations. You can send the failed rows to a network team as a firewall
allowlist request. The summary report shows one row for each provider.

Any HTTP status means that the destination is reachable. For example, `401`
is normal when the probe does not send a credential. `SKIP` means that the
provider needs local configuration, such as an Azure resource name or a Google
Cloud location. The command does not send a prompt, use provider credentials,
or create model cost. It checks reachability, not credential validity.

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

Use a Cursor subscription through KISS's native HTTP/2 provider:

```bash
kiss login cursor
kiss --model cursor/auto
```

KISS talks directly to Cursor's Agent service. It does not start Cursor's
`agent` command, Cursor desktop, Node.js, Bun, or a local proxy. You can also
set `CURSOR_ACCESS_TOKEN` instead of saving a login. When Cursor is selected,
KISS refreshes the model list for the signed-in account and keeps a built-in
fallback list if discovery is not available.

Use Databricks Unity Gateway with a workspace token and URL:

```bash
kiss login databricks-unity-gateway \
  --api-key YOUR_TOKEN \
  --base-url https://your-workspace.cloud.databricks.com
kiss --list-models databricks-unity-gateway
kiss --model databricks-unity-gateway/system.ai.claude-sonnet-4-6
```

Azure Databricks uses the same provider. Supply the Azure workspace URL, such
as `https://adb-1234567890123456.7.azuredatabricks.net`. You can set
`DATABRICKS_TOKEN` and `DATABRICKS_HOST` instead of saving a login. KISS gets
the complete `system.ai` model-service list from the selected workspace. The
list can differ by workspace and can change after KISS is released.

Use Snowflake Cortex with a programmatic access token and an account URL:

```bash
kiss login snowflake-cortex \
  --api-key YOUR_PAT \
  --base-url https://account.snowflakecomputing.com
kiss --model snowflake-cortex/claude-sonnet-4-6
```

KISS also accepts a URL that ends in `/api/v2/cortex` or
`/api/v2/aigateways/SNOWFLAKE`. You can set `SNOWFLAKE_PAT` and
`SNOWFLAKE_CORTEX_BASE_URL` instead of saving a login. The built-in catalog
contains all 34 text-generation models in the current Cortex REST API model
availability table. Account and region rules can reduce the models that you
can use.

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

The `auto`, `websocket`, and `websocket-cached` transport settings use the
Responses WebSocket API for the built-in OpenAI, OpenAI Codex, and Azure OpenAI
providers. `websocket-cached` reuses the connection and sends only new input
after a successful response. Other providers keep their normal streaming
transport.

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

### WebMCP

KISS can discover and call tools that Chrome pages expose through the
experimental [WebMCP API](https://webmachinelearning.github.io/webmcp/). Enable
`chrome://flags/#enable-webmcp-testing` and, when present,
`chrome://flags/#devtools-webmcp-support`. Restart Chrome, enable remote
debugging at `chrome://inspect/#remote-debugging`, and open a WebMCP page. See
the [Chrome guide](https://developer.chrome.com/docs/ai/webmcp) for current
browser requirements and demos.

Use WebMCP in the interactive TUI:

```text
/webmcp
/webmcp connect
/webmcp list
/webmcp disconnect
```

`/webmcp` and `/webmcp connect` add one session-only agent tool. It can list,
describe, and call page tools. The list omits descriptions. KISS follows page
tool changes, navigation, and tab changes. `/webmcp disconnect` closes the
connection and removes the agent tool.

Limit access to known sites in `~/.kiss/agent/settings.json` or a trusted
project's `.kiss/settings.json`:

```json
{
  "webmcp": {
    "allowedOrigins": ["https://example.com"],
    "disallowedOrigins": ["blocked.example"],
    "cdp": 9222
  }
}
```

`allowedOrigins` and `disallowedOrigins` accept a complete origin or a host
name. Without an allow list, KISS permits normal page origins after the user
connects. The deny list always wins. `cdp` accepts a local debugging port or a
complete `ws://` loopback or `wss://` browser WebSocket URL.

KISS does not connect until the user runs `/webmcp`. Calls require the exact
origin and tool name. KISS ignores internal browser pages, treats page metadata
and results as untrusted, and limits page output sent to the model to 100,000
bytes. Calls time out after 60 seconds and send a browser cancellation request
when canceled.

### Agent Client Protocol

KISS is a native [Agent Client Protocol](https://agentclientprotocol.com/)
agent. It implements stable ACP v1 directly over JSON-RPC standard input and
output. ACP clients run:

```bash
kiss acp
```

The command waits for a client and does not open the TUI.

Add KISS to the Zed settings file:

```json
{
  "agent_servers": {
    "KISS": {
      "type": "custom",
      "command": "kiss",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

If Zed cannot find `kiss`, use its full path in `command`.

Global KISS options must come before `acp`. In the example above, change
`args` to `["--model", "sonnet:high", "acp"]` to select a model and thinking
level, or to `["--no-session", "acp"]` to disable session history.

Persistent sessions are the default. Clients can list, load, resume, close,
and delete them, and can change the model and thinking level. KISS accepts
text, images, resource links, and embedded resources. It streams answers,
reasoning, tool status, usage, file locations, and diffs. Tools run in the
working directory that the client supplies. Cancellation stops active and
queued work.

Client-provided stdio and streamable-HTTP MCP servers apply only to the ACP
session and are not saved. Draft ACP v2, audio, legacy MCP SSE, and client
filesystem or terminal delegation are not supported.

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
    await session.prompt('What files are here?')
    stats = await session.session_stats()
    print(
        stats['tokens']['cacheRead'],
        stats['tokens']['cacheWrite'],
        stats['tokens']['cacheWrite1h'],
    )
```

```typescript
const session = await Session.create({ tools: ["read", "bash"] })
await session.prompt("What files are here?")
const stats = await session.sessionStats()
console.log(stats.tokens.cacheRead, stats.tokens.cacheWrite, stats.tokens.cacheWrite1h)
```

`@kiss-sdk/core-wasm` runs the full agent and model/tool loop in a browser. It
does not need a KISS server. `@kiss-sdk/wasm` is the remote client for
applications that need native filesystem and shell tools.

Use cached-token totals in product analytics, cost dashboards, or alerts.
Python, Node, and RPC WASM return cumulative totals through session statistics.
Core WASM returns `cacheRead`, `cacheWrite`, `cacheWrite1h`, and
`cacheReadAvailable` in `PromptResult.usage`.

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

| Measure                     |            Mean |
| --------------------------- | --------------: |
| Warm time to first frame    |        5.037 ms |
| Warm time to first input    |        5.094 ms |
| One idle session            |  15.802 MiB RSS |
| Ten idle sessions           | 159.010 MiB RSS |
| Extra RSS per added session |      15.912 MiB |

Startup results use ten launches after one warm-up. Memory results use three
trials. RSS is the resident memory reported by macOS. Do not compare these
values directly with Linux proportional set size.

### Core operations

| User action              | Test size                         |      Mean |       p95 |
| ------------------------ | --------------------------------- | --------: | --------: |
| File search              | 100,000 files, three warm queries |  4.606 ms |  4.889 ms |
| File search              | 500,000 files, three warm queries |  7.110 ms |  7.690 ms |
| SSE parsing              | 10,000 events                     |  0.931 ms |  0.973 ms |
| Grep                     | 1,000 files and 200 matches       |  6.933 ms |  8.340 ms |
| Incremental Markdown     | 200 streaming prefix renders      | 12.427 ms | 12.576 ms |
| Rust syntax highlighting | One 200-line fence                |  5.830 ms |  6.165 ms |
| Unchanged frame          | 10,000 logical rows               |  0.876 ms |  0.885 ms |

### SDK, RPC, and browser WebAssembly

These tests use an immediate local model or `ping`. They do not call an
external model.

| Surface      | Work                                              |      Mean |
| ------------ | ------------------------------------------------- | --------: |
| Native SDK   | Shared in-process command dispatch                |    113 ns |
| JSONL RPC    | Encode, decode, in-memory transport, and dispatch | 14.439 us |
| Browser WASM | Warm full-agent prompt, 100 samples               |  0.077 ms |
| Browser WASM | 25 isolated agents in parallel, 11 batches        |  0.749 ms |

Fresh WebAssembly module initialization averaged 11.269 ms per Deno process.
The release module is 574,734 bytes raw and 209,375 bytes gzip. It starts with
17 linear-memory pages, or 1,114,112 bytes.

### Agent automation

Subagents are off by default. Session setup had no measured slowdown when they
were enabled. Their six control tools added 71 ns to request preparation.

| Measure                    | State or size         |       Mean |        p95 |
| -------------------------- | --------------------- | ---------: | ---------: |
| Subagent session setup     | Off                   | 358.845 us | 364.466 us |
| Subagent session setup     | On                    | 358.352 us | 361.740 us |
| Request preparation        | Subagents off         |     171 ns |     177 ns |
| Request preparation        | Subagents on          |     242 ns |     249 ns |
| Workflow script parsing    | 200 lines             |  57.884 us |  59.922 us |
| Workflow interpreter       | 1,000 agent calls     |   2.185 ms |   2.423 ms |
| Workflow progress snapshot | 500 agents, 5 phases  |  43.075 us |  64.816 us |
| Workflow phase view        | 500 agents, 5 phases  |  11.298 us |  12.207 us |
| Workflow agent detail      | One prompt and result |   4.886 us |   5.021 us |
| Workflow unchanged view    | 500 agents, cached    |     311 ns |     317 ns |
| Job detail view            | Long goal and result  |  45.193 us |  46.595 us |

The workflow interpreter used 2.185 us per agent call. Arming a workflow added
651 ns to request preparation and kept the total below 1 us. Loop and
autoresearch jobs sleep between model turns and update the TUI through small
events.

### TUI rendering and resize

| Measure                   | Test size           |     Mean |      p95 |
| ------------------------- | ------------------- | -------: | -------: |
| Full renderer             | 1,800 logical rows  | 0.398 ms | 0.436 ms |
| Unchanged renderer        | 10,000 logical rows | 0.876 ms | 0.885 ms |
| Last-row update           | 10,000 logical rows | 0.895 ms | 0.916 ms |
| Cached transcript render  | 2,885 logical rows  | 0.058 ms | 0.063 ms |
| Spinner transcript render | 2,885 logical rows  | 0.055 ms | 0.057 ms |
| Full resize redraw        | 1,800 logical rows  | 0.435 ms | 0.477 ms |

KISS combines rapid resize events and redraws once 75 ms after the final
change. The full resize test wrote 178,231 bytes.

### Profile-guided release builds

| Measure                | Standard build | Optimized build |         Change |
| ---------------------- | -------------: | --------------: | -------------: |
| `kiss --help` startup  |       3.696 ms |        3.676 ms |   0.52% faster |
| Geometric mean latency |         1.000x |          0.985x |   1.51% faster |
| Executable size        |      17.16 MiB |       14.87 MiB | 13.37% smaller |
| gzip size              |       8.17 MiB |        7.36 MiB |  9.94% smaller |

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

| Path                          | Purpose                          |
| ----------------------------- | -------------------------------- |
| `~/.kiss/agent/settings.json` | User settings                    |
| `.kiss/settings.json`         | Project settings                 |
| `~/.kiss/agent/models.json`   | Custom providers and models      |
| `~/.kiss/agent/mcp.json`      | User MCP servers                 |
| `.mcp.json`                   | Project MCP servers              |
| `~/.kiss/agent/skills/`       | User skills                      |
| `.kiss/skills/`               | Project skills                   |
| `~/.kiss/agent/workflows/`    | Personal workflow scripts        |
| `.kiss/workflows/`            | Trusted project workflow scripts |

Open `/settings` for common TUI settings. Run `kiss --help` for all command-line
options. Custom themes live in `~/.kiss/agent/settings.json`.

## Compatibility

KISS tracks [Pi v0.85.1](https://github.com/earendil-works/pi/releases/tag/v0.85.1).
It keeps Pi-compatible session files, model data, core commands, compaction,
and OpenAI Responses WebSocket transport. `Cargo.toml` records the tracked
release.

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
