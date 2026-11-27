# KISS

[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](https://github.com/racetozero/kiss/blob/main/rust-toolchain.toml)
[![Downloads](https://img.shields.io/github/downloads/racetozero/kiss/total)](https://github.com/racetozero/kiss/releases)
[![npm SDK downloads](https://img.shields.io/npm/dm/kiss-agent-sdk?label=npm%20SDK%20downloads)](https://www.npmjs.com/package/kiss-agent-sdk)
[![npm WASM downloads](https://img.shields.io/npm/dm/kiss-agent-sdk-wasm?label=npm%20WASM%20downloads)](https://www.npmjs.com/package/kiss-agent-sdk-wasm)
[![crates.io SDK downloads](https://img.shields.io/crates/d/kiss-agent-sdk?label=crates.io%20SDK%20downloads)](https://crates.io/crates/kiss-agent-sdk)

<img width="737" height="294" alt="image" src="https://github.com/user-attachments/assets/725ad3b8-9711-4700-bbf0-23e969b9dc77" />

A [ridiculously fast](#performance) terminal coding agent that keeps the interface simple and gives you
control of the model, tools, sessions, and automation.

KISS has 44 built-in providers. These include OpenAI Codex (ChatGPT
subscription), OpenAI API, Anthropic OAuth (Claude subscription), Anthropic
API, Meta Muse, Cursor, Google, OpenRouter, Bedrock, Databricks, Snowflake,
and GitHub Copilot.

You can also add OpenAI-compatible providers.

KISS is built in Rust and based on incredible work from
[Pi](https://github.com/earendil-works/pi).

KISS takes its name and product philosophy from [Keep It Simple, Stupid](https://en.wikipedia.org/wiki/KISS_principle).
Why? Because I am stupid :)

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
- **Private by design.** The KISS harness runs locally and collects no
  telemetry.

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

On Linux, if the glibc release needs a newer `GLIBC_*` version than your
system provides, the installer uses the matching musl release.

On macOS and Linux, the installer adds `~/.local/bin` to your shell's startup
file (bash, zsh, fish, or sh). Open a new terminal after installation, or run
`~/.local/bin/kiss` immediately.

Update later with:

```bash
kiss update
```

Homebrew on macOS and Linux:

```bash
brew install racetozero/tap/kiss
```

Homebrew owns this binary, so update it with
`brew upgrade racetozero/tap/kiss` instead of `kiss update`.

## Start in two commands

Sign in with a ChatGPT subscription and open KISS:

```bash
kiss login openai
kiss
```

For a server or SSH session, use the legacy Codex device login:

```bash
kiss login openai-codex --device-auth
```

For Cursor on Windows or Linux without a browser, use:

```bash
kiss login cursor --no-browser
```

Keep KISS running. Open the complete printed URL on a computer with a browser
and sign in. KISS checks for completion and saves the credentials. No local
callback port is required. The request times out after ten minutes.

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
syntax colors for fenced code. Tables appear as boxed grids that follow the
column alignment row. A table too wide for the terminal appears as
`Header: value` records. Fenced `mermaid` blocks appear as Unicode diagrams.
Set "Mermaid rendering" in `/settings` to `streaming`, `final`, or `off`.

Send a new instruction while the agent works. Press `Enter` to steer the
current task, or `Alt+Enter` to queue the instruction for later.

Common commands:

- **Setup:** `/login`, `/model`, `/provider`, `/mcp`, `/settings`, `/hotkeys`.
- **Sessions:** `/compact`, `/resume`, `/export`, `/cache-usage`.
- **Automation:** `/loop`, `/autoresearch`, `/jobs`.
- **Support:** `/bug`, `/fast`, `/update`.

Use `/fast` to toggle the low-latency tier for a supported provider. This
setting applies only to the current session, and provider costs can increase.
Use `/update` to update the installed KISS binary.

### Track cache efficiency

Prompt caching can reduce the cost of repeated context. KISS shows the current
cache rate beside the session cost, so you can see when a workload benefits.

Choose a cache report:

- `/cache-usage`: Show the current session trend.
- `/cache-usage all`: Compare cache efficiency across saved sessions.
- `/cache-usage <provider>`: Show results for one provider.

Every chart uses the same scale, so you can compare the results.

Use the same report in scripts and CI:

```bash
kiss cache-usage
kiss cache-usage --provider anthropic
kiss cache-usage --session <session-id-or-jsonl-file>
```

KISS also protects valuable prompt caches during long-running work, and for
up to 30 minutes while you decide what to do next, when a refresh is expected
to save money. This is automatic.

To change this behavior, set `cacheWarming` to `off` to disable refreshes, or
to `streaming` to refresh only while the agent works.

### Choose a decision model

A decision model is a small, fast classifier that helps your main model.
Turn it on in `/settings` for one or both features:

- **Compaction method:** Keep, shorten, or remove older tool interactions
  instead of summarizing the whole conversation.
- **Dynamic reasoning:** Raise or lower reasoning effort as the task changes.

| Decision model         | Provider   | Sign in                        |
| ---------------------- | ---------- | ------------------------------ |
| d1-ultra (recommended) | Liquid     | `/login liquid`                |
| d1.1                   | Liquid     | `/login liquid`                |
| Jev                    | TypeSafe   | `/login typesafe`              |
| Clef Flash, Clef       | Cloudflare | `/login cloudflare-workers-ai` |

Start with d1-ultra for the best balance of quality and cost, or keep Jev if
it already works well for you. Clef Flash costs less, and Clef scores higher
at a higher cost. Cloudflare needs an API token and an account ID. You can
also set `compaction.mode` and `reasoningEffort.mode` in your settings.

Both features are off by default. They send conversation context to the
selected provider, which can add cost. If the decision model is unavailable,
KISS uses summary compaction or your saved reasoning effort.

<img width="432" height="62" alt="image" src="https://github.com/user-attachments/assets/3a3ce46e-3b70-49ce-ba7c-d02996a64630" />

### Choose an advisor model

An advisor is a second model that your main model asks for guidance when it
needs stronger judgment: a hard decision, an ambiguous failure, or a problem
it does not solve. Run `/advisor` and pick any model from any provider you are
logged in to, or pick `No advisor`. For example, use a fast model as the main
model and a stronger model as the advisor.

```text
/advisor                              open the picker
/advisor anthropic/claude-opus-4-5    set the advisor directly
/advisor openai/gpt-5.2:xhigh         set the advisor and its thinking level
/advisor off                          remove the advisor
```

With an advisor set, the main model gets an `advisor` tool without
arguments. A call sends the system prompt and a text transcript of the
session to the advisor and returns its answer as the tool result. Long
transcripts keep the first request and the most recent turns. The advisor
thinks at `high` unless you add a `:level` suffix, and its cost counts in the
session totals.

The advice starts with `PLAN`, `CORRECTION`, or `STOP` and is at most about
700 words. After `STOP`, the agent stops and asks you. The agent can call the
advisor three times for each prompt you send. Set `advisorMaxUses` in
`~/.kiss/agent/settings.json` to change that limit.

KISS saves the choice as `advisorModel` in `~/.kiss/agent/settings.json`.
Project settings cannot set it, and `--no-tools` and child agents have no
advisor. If the advisor's provider is not logged in at startup, KISS shows a
warning and runs without the advisor. Log in with `/login`, then run
`/advisor` again or restart KISS.

## Continue work from another agent

Run `/resume` to continue a KISS, Pi, Claude Code, or OpenAI Codex session.
The picker starts with sessions from the current working directory. Type to
search titles and folders. Press `Tab` (or `Ctrl+G`) to switch between project
and global results, and `Ctrl+R` to rename a KISS session in place. `Esc`
clears the search first, then closes the picker.

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
/autoresearch reduce Markdown render time and verify it with the existing benchmark
/autoresearch reduce Markdown render time and verify it with the existing benchmark --iterations 20
```

Limits:

- A loop accepts an interval or `--iterations`, but not both.
- Autoresearch does not accept an interval.
- The maximum explicit iteration limit is 100.

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

Read the results as follows:

- **An HTTP status:** The destination is reachable. For example, `401` is
  normal because the probe does not send a credential.
- **`SKIP`:** The provider needs local configuration, such as an Azure resource
  name or a Google Cloud location.

The command checks network access. It does not check credential validity,
send a prompt, use provider credentials, or create model cost.

### Login and model selection

KISS supports browser and headless OAuth, API keys, environment variables, and
cloud credentials. It can import compatible credentials from OpenAI Codex,
Claude Code, Pi, OpenCode, OpenClaw, and Hermes.

Use your ChatGPT subscription with `kiss login openai`. Complete the login in
your browser, then select an OpenAI model. KISS saves the login and refreshes
it automatically when needed.

You can also use an OpenAI API key. For a server or SSH session, use
`kiss login openai-codex --device-auth` to sign in through the OpenAI Codex
provider. `kiss auth login <provider>` and `kiss auth logout <provider>` work
like `kiss login` and `kiss logout`, as in Pi.

```bash
kiss login openai
kiss login anthropic --device-auth
kiss login anthropic --api-key YOUR_KEY
kiss auth
kiss logout openai
kiss --list-models
kiss --model sonnet:high
```

#### Anthropic workload identity federation

Run KISS in a cloud workload without storing an Anthropic API key. Set
`ANTHROPIC_FEDERATION_RULE_ID` and `ANTHROPIC_ORGANIZATION_ID`, and set
`ANTHROPIC_IDENTITY_TOKEN_FILE` to the file where your platform writes the
workload's identity token. KISS trades that token for a short-lived Anthropic
token and renews it automatically.

Set `ANTHROPIC_WORKSPACE_ID` when the federation rule covers more than one
workspace, and `ANTHROPIC_SERVICE_ACCOUNT_ID` to choose a service account. A
saved Anthropic login or API key takes precedence.

#### Corporate proxies

Set `HTTPS_PROXY` to your HTTP proxy URL. Use `NO_PROXY` for hosts that must
connect directly. Install the corporate root certificate in the operating
system trust store, or set `SSL_CERT_FILE` to a PEM certificate file.
KISS checks server certificates for HTTP and WebSocket connections. Bedrock
uses the AWS SDK proxy settings from the environment.

The automatic test suite checks each built-in provider with a local TLS
interception proxy.

#### Cursor

Use your Cursor subscription in KISS:

```bash
kiss login cursor
kiss --model cursor/auto
```

Start with Auto, or use `/model cursor` to choose from the models available
to your account. You can also sign in with `CURSOR_ACCESS_TOKEN`.

KISS selects the connection type automatically. If your corporate proxy requires HTTP/1,
set `KISS_CURSOR_TRANSPORT=http1` (`$env:KISS_CURSOR_TRANSPORT = "http1"`
in PowerShell). KISS keeps the Cursor connection and pending tools alive
while a tool runs or waits for approval; resuming uses the original run, not
a replayed prompt.

#### Databricks

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
`DATABRICKS_TOKEN` and `DATABRICKS_HOST` instead of saving a login.

KISS gets the complete `system.ai` model-service list from the selected
workspace. The list can differ by workspace and can change after KISS is
released.

#### Snowflake

Use Snowflake Cortex with a programmatic access token and an account URL:

```bash
kiss login snowflake-cortex \
  --api-key YOUR_PAT \
  --base-url https://account.snowflakecomputing.com
kiss --model snowflake-cortex/claude-sonnet-4-6
```

KISS also accepts a URL that ends in `/api/v2/cortex` or
`/api/v2/aigateways/SNOWFLAKE`. You can set `SNOWFLAKE_PAT` and
`SNOWFLAKE_CORTEX_BASE_URL` instead of saving a login.

Account and region rules determine which Snowflake models you can use.

### Multiple accounts with rate-limit failover

Add more than one account to a provider to continue work when an account
reaches its usage limit. KISS sends the request again with the next available
account and shows a notice when it switches. The footer shows the active
account, for example `⇄ 2/4 personal`.

Use `/accounts` to add, select, or remove accounts:

```text
/accounts
/accounts add
/accounts add openai-codex work
/accounts use openai-codex 2
/accounts remove openai-codex 1
```

To add an account from the command line, use `--add-account`. Use a label
such as `personal` or `work` to identify the account:

```bash
kiss login openai-codex --add-account --account-label personal
kiss login google --api-key KEY_TWO --add-account
```

You can add accounts for any provider that supports KISS login, including
OpenAI Codex, Anthropic, Cursor, Google, Meta, and xAI.

KISS switches accounts for rate limits and exhausted quotas. It does not
switch for server errors. An account becomes available again at the reset
time reported by the provider, or after 15 minutes if no reset time is given.
Keys supplied through environment variables or the command-line `--api-key`
option are not rotated. `/logout PROVIDER` removes all saved accounts for
that provider.

### Your own OpenAI-compatible provider

Connect KISS to a local model server or an API proxy. Add the server URL,
API type, and model ID. The model then appears in the model selector:

```bash
kiss provider add local \
  --base-url http://127.0.0.1:8000/v1 \
  --api chat-completions \
  --model local-model
kiss provider list
kiss --model local/local-model
kiss provider remove local
```

Set `--api` to `chat-completions`, `responses`, or `codex` to match the
server. If it requires a key, use `--api-key-env NAME` to read the key from
an environment variable, or save it with
`kiss login <provider> --api-key KEY`.

Use `--no-auth` when the proxy manages authentication. For CodexLB:

```bash
kiss provider add codex-lb \
  --base-url http://127.0.0.1:2455/backend-api/codex \
  --api codex \
  --model gpt-6.1-sol \
  --no-auth \
  --reasoning
kiss --model codex-lb/gpt-6.1-sol
```

This setup requires no KISS login or environment variable. If CodexLB
requires a key, replace `--no-auth` with `--api-key-env CODEX_LB_API_KEY`.
To use your saved OpenAI Codex login, run `kiss login openai-codex` and omit
`--no-auth`.

Use `--auth-provider PROVIDER` to share another provider's saved login.
Choose one authentication option: `--no-auth`, `--api-key-env`, or
`--auth-provider`. Add `--header KEY=VALUE` for each custom header the
server requires.

Codex providers also support WebSocket transport, including custom proxies.
The built-in OpenAI and Azure OpenAI providers support it too. Use `auto`
to try WebSocket with an SSE fallback, or `sse` for HTTP streaming.
`websocket-cached` reuses the connection and sends only new input after a
successful response.

To manage providers in the terminal interface, use `/provider`:

```text
/provider add <id> <chat-completions|responses|codex> <base-url> <model> [KEY_ENV|auth:<provider>|--no-auth]
/provider list
/provider remove <id>
```

Restart KISS after you add or remove a provider to update the model list.

### MCP

Add local or remote MCP servers:

```bash
kiss mcp add local -- bunx @modelcontextprotocol/server-everything
kiss mcp add remote --url https://example.com/mcp --auth oauth
kiss mcp login remote
kiss mcp list
kiss mcp test local
```

- Use `--scope project` to save a server in `.mcp.json`.
- Use `kiss mcp login remote --no-browser` for headless OAuth.
- Use `/mcp` to manage servers in the terminal interface.
- Use `--description "Team tickets"` to tell the model when to use a server.
  The model sees each description and can find the server's tools by it.

Some servers need special sign-in settings in `mcp.json`:

- **Reuse a KISS login:** `"auth": { "provider": "radius" }` signs in to the
  server with your KISS Radius login and stays signed in as the login
  refreshes. On the command line, use `--auth-provider radius`. To protect
  your login, only your user `mcp.json` can use this, and the server must use
  HTTPS.
- **Fix sign-in discovery:** Set `oauth.authServerMetadataUrl` when the
  server points to the wrong sign-in server, or to none.
- **Use your own client identity:** Set `oauth.clientMetadataUrl` to a
  client metadata document that you host. KISS uses it when the sign-in
  server supports it, and registers a client otherwise. Set
  `oauth.clientRegistration` to `"cimd"` to require the document, or to
  `"dcr"` to always register.
- **Name the client:** Set `oauth.clientName` for servers that accept only
  known client names.

### WebMCP

KISS can discover and call tools that Chrome pages expose through the
experimental [WebMCP API](https://webmachinelearning.github.io/webmcp/).

Set up Chrome:

1. Enable `chrome://flags/#enable-webmcp-testing`.
2. Enable `chrome://flags/#devtools-webmcp-support`, if available.
3. Restart Chrome.
4. Enable remote debugging at `chrome://inspect/#remote-debugging`.
5. Open a WebMCP page.

See the [Chrome guide](https://developer.chrome.com/docs/ai/webmcp) for current
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

Settings:

- `allowedOrigins` and `disallowedOrigins` accept a complete origin or a host
  name. Without an allow list, KISS permits normal page origins after you
  connect. The deny list always takes priority.
- `cdp` accepts a local debugging port, a complete `ws://` loopback URL, or a
  complete `wss://` browser WebSocket URL.

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
and delete them. Clients can also change the model and thinking level.

KISS accepts text, images, resource links, and embedded resources. It streams
answers, reasoning, tool status, usage, file locations, and diffs.

Tools run in the working directory that the client supplies. Cancellation
stops active and queued work.

Client-provided stdio and streamable-HTTP MCP servers apply only to the ACP
session and are not saved. Draft ACP v2, audio, legacy MCP SSE, and client
filesystem or terminal delegation are not supported.

## Build with KISS

KISS provides SDKs for Rust, Python 3.11+, TypeScript on Node, Bun, and Deno,
and browser applications through WebAssembly. All SDKs use the same streaming
event protocol.

### Rust

Add the [Rust SDK crate from crates.io](https://crates.io/crates/kiss-agent-sdk)
to your project.

```bash
cargo add kiss-agent-sdk
```

```rust
let session = kiss_sdk::Session::builder().tools(["read", "bash"]).build().await?;
session.prompt("What files are here?").await?;
```

### Python

Install the [Python SDK from PyPI](https://pypi.org/project/kiss-agent-sdk/).
It requires Python 3.11 or later.

```bash
uv add kiss-agent-sdk
```

```python
import kiss_sdk

async with await kiss_sdk.Session.create(tools=[kiss_sdk.ToolName.READ]) as session:
    await session.prompt('What files are here?')
    stats = await session.session_stats()
    print(
        stats['tokens']['cacheRead'],
        stats['tokens']['cacheWrite'],
        stats['tokens']['cacheWrite1h'],
    )
```

### Node, Bun, and Deno

Install the [native JavaScript SDK from npm](https://www.npmjs.com/package/kiss-agent-sdk)
to use KISS in a Node, Bun, or Deno application.

```bash
bun add kiss-agent-sdk
```

```typescript
import { Session } from "kiss-agent-sdk"

const session = await Session.create({ tools: ["read", "bash"] })
await session.prompt("What files are here?")
const stats = await session.sessionStats()
console.log(stats.tokens.cacheRead, stats.tokens.cacheWrite, stats.tokens.cacheWrite1h)
```

### Browser WebAssembly

Install the [browser SDK from npm](https://www.npmjs.com/package/kiss-agent-sdk-wasm)
to run the agent and model/tool loop in your browser without a KISS server.

```bash
bun add kiss-agent-sdk-wasm
```

For applications that need native filesystem and shell tools, use the
`@kiss-sdk/wasm` remote client. See the [SDK guide](docs/sdk.md#remote-native-agent)
for its build and connection instructions.

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

Local work only; model and network latency are excluded. Apple M4, release builds. Lower is better.

### Startup and memory

| Measure                     |            Mean |
| --------------------------- | --------------: |
| Warm time to first frame    |        7.767 ms |
| Warm time to first input    |        7.869 ms |
| One idle session            |  23.490 MiB RSS |
| Ten idle sessions           | 197.411 MiB RSS |
| Extra RSS per added session |      19.325 MiB |

KISS 0.0.42, 2026-11-22. Mean of 20 warm launches (startup) and 3 trials (memory). RSS is macOS resident memory.

### Long-session memory

KISS 0.0.42, macOS 27.0.1, Rust 1.100.0, mimalloc.

| History entries | Compacted | Footprint, MiB [range] |
| --------------- | --------- | ---------------------: |
| 0               | No        |    9.001 [8.938–9.047] |
| 1,000           | No        | 33.126 [33.094–34.594] |
| 1,000           | Yes       | 33.532 [33.516–33.594] |
| 5,000           | No        | 99.126 [98.704–99.266] |
| 5,000           | Yes       | 99.094 [96.016–99.344] |

macOS physical footprint after 20 `!!` shell updates; median of three processes, process range in brackets.

Run `just bench-session-memory`, or:

```sh
uv run --no-project python scripts/benchmark_kiss_session_memory.py --binary /path/to/kiss --entries 0 1000 5000 --trials 3 --updates 20 --json target/session-memory-benchmark.json
```

### Large sessions (2026-10-10)

Rust 1.99.0, macOS 27.0, mimalloc. Median of process medians, 15 samples each, range in brackets.

| Operation                         | Source text |                ms [range] |
| --------------------------------- | ----------: | ------------------------: |
| First full frame                  |       2 MiB |       6.562 [6.548–6.647] |
| First full frame                  |      10 MiB |    34.768 [34.574–34.961] |
| First full frame                  |      40 MiB | 141.834 [141.437–149.600] |
| Spinner frame                     |       2 MiB |       1.138 [1.134–1.145] |
| Spinner frame                     |      10 MiB |       8.451 [8.400–9.256] |
| Spinner frame                     |      40 MiB |    39.172 [39.037–42.093] |
| Cached app frame                  |       2 MiB |       0.528 [0.521–0.572] |
| Cached app frame                  |      10 MiB |       3.537 [3.093–6.183] |
| Cached app frame                  |      40 MiB |    18.349 [15.945–24.019] |
| Context message count             |       2 MiB |       0.003 [0.003–0.004] |
| Context message count             |      10 MiB |       0.018 [0.017–0.019] |
| Context message count             |      40 MiB |       0.083 [0.080–0.087] |
| Saved model and thinking settings |       2 MiB |       0.003 [0.003–0.004] |
| Saved model and thinking settings |      10 MiB |       0.018 [0.016–0.019] |
| Saved model and thinking settings |      40 MiB |       0.079 [0.079–0.083] |

| Component test       | Source text |     Peak RSS, MiB [range] |
| -------------------- | ----------: | ------------------------: |
| App and renderer     |       2 MiB |    39.078 [39.078–39.078] |
| App and renderer     |      10 MiB | 117.766 [117.766–117.766] |
| App and renderer     |      40 MiB | 415.750 [415.750–415.766] |
| Scalar session reads |       2 MiB |    11.016 [11.016–11.016] |
| Scalar session reads |      10 MiB |    19.453 [19.438–19.453] |
| Scalar session reads |      40 MiB |    51.531 [51.516–51.531] |

Peak RSS is for the whole component test process, not a CLI session.

Repeat with `KISS_PERF_MIB` set to `0`, `2`, `10`, or `40`:

```sh
KISS_PERF_MIB=40 cargo test --release -p kiss --bin kiss benchmark_shared_transcript_pipeline -- --ignored --nocapture --test-threads=1
KISS_PERF_MIB=40 cargo test --release -p kiss --bin kiss benchmark_scalar_session_reads -- --ignored --nocapture --test-threads=1
```

### Core operations

Measured on 2026-10-10 (`3ab0483`): 1,000 samples in each of three processes. p99 range across processes in brackets.

| User action              | Test size                         |       p95 |               p99 [range] |
| ------------------------ | --------------------------------- | --------: | ------------------------: |
| File search              | 100,000 files, three warm queries |  4.692 ms |    4.915 [4.812–9.163] ms |
| File search              | 500,000 files, three warm queries |  8.698 ms |   9.284 [8.987–15.225] ms |
| SSE parsing              | 10,000 events                     |  0.810 ms |    0.865 [0.801–1.184] ms |
| Grep                     | 1,000 files and 200 matches       |  8.623 ms |  10.397 [9.188–12.269] ms |
| Incremental Markdown     | 200 streaming prefix renders      | 13.203 ms | 14.683 [13.870–31.086] ms |
| Rust syntax highlighting | One 200-line fence                |  5.177 ms |    5.514 [5.276–5.537] ms |
| Unchanged frame          | 10,000 logical rows               |  0.151 ms |    0.157 [0.154–0.159] ms |

### SDK, RPC, and browser WebAssembly

Uses a local model or `ping`, not an external model.

| Surface      | Work                                              |      Mean |
| ------------ | ------------------------------------------------- | --------: |
| Native SDK   | Shared in-process command dispatch                |    113 ns |
| JSONL RPC    | Encode, decode, in-memory transport, and dispatch | 14.439 us |
| Browser WASM | Warm full-agent prompt, 100 samples               |  0.077 ms |
| Browser WASM | 25 isolated agents in parallel, 11 batches        |  0.749 ms |

Module start: 11.269 ms. Size: 574,734 bytes, 209,375 gzip.

### Agent automation

Subagents are off by default and add six tools when on.

| Measure                    | State or size                     |        p95 |                  p99 [range] |
| -------------------------- | --------------------------------- | ---------: | ---------------------------: |
| Subagent session setup     | Off                               | 398.542 us | 425.875 [409.417–434.667] us |
| Subagent session setup     | On                                | 398.084 us | 422.625 [414.000–434.542] us |
| Request preparation        | Subagents off                     |     250 ns |             291 [250–333] ns |
| Request preparation        | Subagents on                      |     334 ns |             375 [334–375] ns |
| Workflow script parsing    | 200 lines                         |  63.833 us |    69.500 [67.458–91.875] us |
| Workflow interpreter       | 1,000 agent calls                 |   2.155 ms |       2.350 [2.153–2.482] ms |
| Workflow progress snapshot | 500 agents, 5 phases, growing log |  24.583 us |    24.916 [20.584–25.417] us |
| Workflow phase view        | 500 agents, 5 phases              |   8.291 us |       8.417 [8.334–8.917] us |
| Workflow agent detail      | One prompt and result             |   3.625 us |       4.375 [3.584–4.417] us |
| Workflow unchanged view    | 500 agents, cached                |     167 ns |             250 [208–250] ns |
| Job detail view            | Long goal and result              |  36.958 us |    40.542 [40.166–41.583] us |

### TUI rendering and resize

| Measure                   | Test size           |      p95 |            p99 [range] |
| ------------------------- | ------------------- | -------: | ---------------------: |
| Full renderer             | 1,800 logical rows  | 0.395 ms | 0.423 [0.410–0.426] ms |
| Unchanged renderer        | 10,000 logical rows | 0.151 ms | 0.157 [0.154–0.159] ms |
| Last-row update           | 10,000 logical rows | 0.148 ms | 0.154 [0.153–0.160] ms |
| Cached transcript render  | 2,885 logical rows  | 0.040 ms | 0.043 [0.043–0.045] ms |
| Spinner transcript render | 2,885 logical rows  | 0.040 ms | 0.044 [0.044–0.046] ms |
| Full resize redraw        | 1,800 logical rows  | 0.416 ms | 0.845 [0.507–1.981] ms |

Resizes are combined and redrawn once, 75 ms after the last change.

### Comparison with other harnesses

| Measure                        |   **KISS 0.0.42** | Prime Agent Rust | Prime Agent TypeScript | Claude Code 2.1.289 | Codex CLI 0.160.0 |       Pi 1.0.3 | Hermes Agent 0.21.5 |
| ------------------------------ | ----------------: | ---------------: | ---------------------: | ------------------: | ----------------: | -------------: | ------------------: |
| First visible output           |  **6.2 ± 2.5 ms** |    23.6 ± 0.6 ms |        722.8 ± 15.1 ms |      264.6 ± 5.8 ms |    296.8 ± 2.7 ms | 306.3 ± 6.7 ms |   1,715.3 ± 10.3 ms |
| Cold time to type              |  **8.5 ± 3.3 ms** |    55.8 ± 4.9 ms |        737.8 ± 13.9 ms |      348.4 ± 8.2 ms |    324.6 ± 4.9 ms | 317.7 ± 8.0 ms |   2,094.5 ± 23.5 ms |
| Warm time to type              |  **8.6 ± 7.0 ms** |    42.4 ± 4.3 ms |        549.6 ± 10.0 ms |      345.1 ± 6.2 ms |    321.3 ± 9.2 ms | 240.4 ± 5.9 ms |   2,097.1 ± 40.8 ms |
| Complete installed size        |       **26.4 MB** |          59.6 MB |               172.1 MB |            492.4 MB |          446.8 MB |       456.0 MB |            960.1 MB |
| Process tree RSS after startup | **26.6 ± 0.7 MB** |   106.0 ± 1.3 MB |         607.4 ± 0.9 MB |      226.9 ± 0.4 MB |    344.4 ± 3.1 MB | 138.1 ± 0.7 MB |      194.6 ± 0.3 MB |

Other harnesses: [Prime Intellect's published results](https://www.primeintellect.ai/blog/prime-agent-rust#results) (Linux sandbox, 4 cores, 8 GB). **KISS**: our run below on different hardware, so it is not a direct comparison.

**KISS** 0.0.42 (`7d11f3b`), macOS 27.0.1, 160×40 pseudo-terminal with pyte 0.8.2. Times count until visible screen cells appear. 1,000 cold and 1,000 warm launches; RSS from 20 launches. MB = 1,000,000 bytes.

| Measure                        | **KISS** mean ± sample standard deviation |          p95 |           p99 |
| ------------------------------ | ----------------------------------------: | -----------: | ------------: |
| First visible output, cold     |                      **6.207 ± 2.536 ms** | **7.101 ms** |  **9.187 ms** |
| Cold time to type              |                      **8.459 ± 3.347 ms** | **9.306 ms** | **12.394 ms** |
| Warm time to type              |                      **8.603 ± 7.008 ms** | **9.426 ms** | **13.939 ms** |
| Installed CLI payload          |                             **26.404 MB** |            — |             — |
| Process tree RSS after startup |                     **26.648 ± 0.705 MB** |            — |             — |

To repeat:

```sh
uv run --no-project --with pyte==0.8.2 python scripts/benchmark_kiss_startup.py --binary target/release/kiss --samples 1000 --memory-trials 20 --json target/visible-startup-benchmark.json
```

### Profile-guided release builds

| Measure                | Standard build | Optimized build |         Change |
| ---------------------- | -------------: | --------------: | -------------: |
| `kiss --help` startup  |       3.696 ms |        3.676 ms |   0.52% faster |
| Geometric mean latency |         1.000x |          0.985x |   1.51% faster |
| Executable size        |      17.16 MiB |       14.87 MiB | 13.37% smaller |
| gzip size              |       8.17 MiB |        7.36 MiB |  9.94% smaller |

### Repeat

Run a p95/p99 benchmark alone. `KISS_BENCH_RAW=1` prints samples; `KISS_BENCH_FILTER` picks rows.

```sh
KISS_BENCH_SAMPLES=1000 KISS_BENCH_ITERATIONS=1 KISS_BENCH_RAW=1 cargo test --release -p kiss-tui benchmark_performance_renderer_frames -- --ignored --nocapture --test-threads=1
```

Run everything with `just bench` (needs cargo-nextest, wasm-pack, Deno, Node).

## Voice dictation

Use `/voice` in the terminal interface to enable dictation:

- **`/voice`:** Hold Space to record. Release Space to transcribe.
- **`/voice tap`:** Press Space to start recording. Press it again to stop.
  Use this mode if your terminal does not report key releases.
- **`/voice off`:** Restore normal Space input.
- **Esc:** Cancel a recording.

KISS inserts the transcript at the editor cursor. Press Enter to send it.
If Space does not stop a hold recording, press Esc and use `/voice tap`.

### Set up local dictation

The default backend, `/voice local`, processes audio on your machine. It
transcribes when you stop recording.

1. Install `ffmpeg` and make sure it is in `PATH`.
2. Install whisper.cpp's `whisper-cli`.
3. Download a whisper.cpp GGML model. KISS does not download it for you.
4. Set `KISS_VOICE_MODEL` to the model's absolute path before you start KISS.

For example, on macOS:

```bash
brew install ffmpeg whisper-cpp
export KISS_VOICE_MODEL=/absolute/path/to/ggml-base.en.bin
kiss
```

### Set up cloud dictation

Cloud dictation also requires `ffmpeg` in `PATH`. Set your API key, then select
the service:

- **Deepgram:** Set `DEEPGRAM_API_KEY` and run `/voice deepgram`.
- **ElevenLabs:** Set `ELEVENLABS_API_KEY` and run `/voice elevenlabs`.

Selecting a cloud service permits KISS to send microphone audio to that
service. Cloud transcripts appear as you speak. Run `/voice local` to return
to local processing.

KISS saves the selected backend in user settings. It does not save credentials
there. If the selected provider is unavailable, KISS reports an error. It does
not send audio to another backend without your selection.

### Select a microphone and language

The default microphone input depends on your system:

| System  | Audio source | Default input   |
| ------- | ------------ | --------------- |
| macOS   | avfoundation | `:0`            |
| Linux   | PulseAudio   | `default`       |
| Windows | DirectShow   | `audio=default` |

If the default input is not your microphone, set `KISS_VOICE_INPUT` to the
ffmpeg device name. Use these commands to find devices:

| System  | Command                                           |
| ------- | ------------------------------------------------- |
| macOS   | `ffmpeg -f avfoundation -list_devices true -i ""` |
| Linux   | `pactl list sources short`                        |
| Windows | `ffmpeg -list_devices true -f dshow -i dummy`     |

Give your terminal microphone permission if your system requires it.

Use `/config voice-language es` to select Spanish. The default is `en`.
`auto` is also available. KISS saves the language in user settings.

## Configuration

### Desktop notifications

Get a notification when a prompt finishes or when KISS needs your input, such
as a workflow approval, a trust decision, or a login. Turn on **Desktop
notifications** in `/settings`. They are off by default, and only your user
settings can turn them on.

A notification says only "Your prompt has finished." or "Your input is
required." It never includes prompt text, code, paths, or model output. KISS
skips notifications while you look at the terminal. If the terminal cannot
report focus, KISS waits six seconds and cancels the notice when you type.
Notifications apply only to the interactive terminal.

KISS picks one delivery route:

| Where KISS runs                  | Delivery                                                                    |
| -------------------------------- | --------------------------------------------------------------------------- |
| cmux                             | `cmux notify` for the current pane                                          |
| Kitty                            | Kitty notification (OSC 99)                                                 |
| iTerm2, Warp                     | OSC 9                                                                       |
| Ghostty, WezTerm, rxvt           | OSC 777                                                                     |
| Other terminals on your computer | Notification Center on macOS, a toast on Windows, or `notify-send` on Linux |

Over SSH, KISS notifies only through a supported terminal, and stays silent
otherwise. In tmux, add `set -g allow-passthrough on` to your tmux
configuration. Your system settings, such as Do Not Disturb, still apply.
Rex and Herdr can also notify you on their own, so turn one source off if
you get duplicates.

### Terminal integrations

KISS works in any terminal. These terminals also show what KISS is doing:

| Terminal | Status                              | Resume saved sessions | Notifications                   |
| -------- | ----------------------------------- | --------------------- | ------------------------------- |
| Rex      | Idle, working, done, blocked, error | No                    | Rex, or the desktop route above |
| Herdr    | Idle, working, blocked              | Yes                   | The desktop route above         |
| cmux     | Idle, working, blocked              | Yes                   | `cmux notify`                   |
| Others   | Shown when the terminal supports it | No                    | The desktop route above         |

### Terminal program status

Interactive KISS reports its state with the Program Status Protocol
(OSC 7501). Terminals that support it, such as Rex, can show this state in
a tab or notification. No setup is required, and other terminals ignore
the reports.

| State     | When KISS reports it                                           |
| --------- | -------------------------------------------------------------- |
| `idle`    | KISS waits for input, or you cancelled the last turn.          |
| `working` | A turn, command, compaction, workflow, or job is active.       |
| `done`    | A turn finished and the terminal can mark it as unread.        |
| `blocked` | KISS needs a workflow approval, trust decision, or login.      |
| `error`   | The last turn failed, or KISS stopped after an internal error. |

KISS clears its status when you exit.

### Rex

Run `kiss` in Rex to see in the tab when work is active, finished, or needs
your input. No setup is required.

To let KISS open tabs, split panes, or read terminal output, install the Rex
skill at `~/.agents/skills/rex/SKILL.md` and keep the Bash tool on. KISS finds
the skill at the next session start. Without the skill, ask KISS to run
`rex skill show`. The `rex` command must be on `PATH`.
`rex skill setup --agent` does not list KISS yet, so follow the
[Rex agent guide](https://www.superlogical.com/rex/docs/automate/agent-skill#supported-agents)
to install the skill.

### Herdr and cmux

<img width="856" height="634" alt="image" src="https://github.com/user-attachments/assets/d695eda7-6fad-437f-bfc6-aebe9f8ab53f" />

Run `kiss` in a Herdr or cmux pane to enable native support. No hook script
is required. KISS reports when it is working, idle, or waiting for a workflow
approval, project trust decision, or login input. Reports include active
workflows and jobs. Each cmux surface has its own KISS status entry, and it
receives KISS notifications when **Desktop notifications** is on.

For saved sessions, KISS registers `kiss --session <file>` with the current
model and reasoning level. The command changes when you switch sessions or
models. `--no-session` does not register a resume command. KISS removes its
status and resume data when you exit. A host failure does not stop KISS.

On Linux (including WSL) and macOS, KISS reports directly to Herdr's socket,
without starting a reporter process for each update. The CLI is a fallback
when the socket cannot be opened; `HERDR_BIN_PATH` selects it, otherwise
KISS uses `herdr` on `PATH`. Other platforms use the CLI.

If KISS does not appear in Herdr, start it in a fresh Herdr pane and check
that `HERDR_ENV=1`, `HERDR_PANE_ID`, and `HERDR_SOCKET_PATH` are inherited
inside that pane. In WSL these must refer to the Herdr instance reachable
from that Linux environment. KISS does not translate Windows pipe or path
names into Linux sockets.

Herdr session resume requires version 0.9.2 or later. Herdr cannot accept
resume arguments with apostrophes or control characters, or commands with
more than 64 arguments or 8 KiB of argument data. For these commands, KISS
reports state without session resume.

cmux stores the resume command for the current surface. To permit automatic
resume, approve the KISS command prefix in **Settings > Terminal > Resume
Commands**. KISS does not change these approvals. The cmux CLI must be on
`PATH` and the socket must be enabled.

Resume commands do not contain the original user prompt or an `--api-key`
override. Use saved credentials or environment variables for sessions that
must resume. This integration applies to interactive terminal mode.

### Files and settings

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

### Custom status line

Replace the interactive footer with the output of your own command, using the
same contract as Claude Code. Existing `~/.claude/statusline.sh` scripts work
with little or no modification:

```json
{
  "statusLine": {
    "type": "command",
    "command": "~/.claude/statusline.sh"
  }
}
```

The command receives a JSON payload on stdin (`cwd`, `session_id`,
`transcript_path`, `model.id`, `model.display_name`, `workspace`,
`cost.total_cost_usd`, `context_window` usage, plus kiss extras under `kiss`)
and its stdout, ANSI colors included, becomes the footer (up to 5 lines).
The command re-runs only when that payload changes, debounced by `debounceMs`
(default 300) and killed after `timeoutMs` (default 5000). Optional `padding`
adds leading spaces to each line. A failing or silent command keeps the
previous output; before the first success the default footer shows.

### Tool selection

Choose which tools are available when a session starts with `defaultTools` in
your settings. For example, `["read", "bash"]` starts sessions with only those
two tools.

To adjust the default selection, use `+` to add a tool and `-` to remove one.
For example, `["+grep", "-write"]` adds search and removes the write tool. Add
`"+mcp"` to keep access to your configured MCP servers.

Save this selection in your user settings to use it across projects. A trusted
project can provide its own selection or adjust yours with `+` and `-` entries.
For a single run, `--tools` selects the tools, `--exclude-tools` removes tools,
and `--no-tools` disables them all. These options override your saved selection.

Both options accept `*` patterns. For example, `--exclude-tools 'gr*'` removes
grep. To adjust your saved selection for one run, use only `+` and `-`
entries, such as `kiss --tools +grep,-write`. Your MCP servers stay available
unless the `--tools` list names `mcp`. Use `--no-mcp` to run without them.

## Experimental context file

For long tasks, let the model decide which conversation details to keep.
This experimental mode gives it an editable context file, so it can replace
old logs and failed attempts with task notes as it works.

Enable it for one run:

```bash
kiss --experimental-context-file
```

Ask the model to keep important facts, current progress, and remaining checks
in its context file. KISS applies valid edits before the next model request
and keeps new user messages and tool results. If an edit is invalid, KISS
keeps the previous context and asks the model to repair it. Automatic
compaction remains available.

Saved sessions retain the full conversation record and restore accepted
context edits when you resume. Use `--no-session` to work without saving.

The default is off. Set `"experimentalContextFile": true` in
`~/.kiss/agent/settings.json` to enable it for future sessions. Project
settings cannot change this choice.

The method comes from [Context Language Models](https://arxiv.org/abs/2609.37725).
Benefits over the KISS baseline have not been measured. Context edits can
reduce provider cache reuse and increase cost.

## Compatibility

KISS tracks [Pi v1.5.2](https://github.com/earendil-works/pi/releases/tag/v1.5.2).
You can continue compatible Pi sessions and use the updated model catalog,
core commands, and conversation compaction. KISS also supports OpenAI
Responses over WebSocket.

KISS does not include Pi's TypeScript extension runtime, JavaScript codemode,
or the experimental `pi-durable` runtime.
The tracked Pi release is recorded in `Cargo.toml`.

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
