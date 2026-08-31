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

## Performance

KISS release binaries use target-specific profile-guided optimization (PGO).
macOS releases also use safe identical code folding (ICF). The Cargo `dist`
profile inherits the release settings: optimization level 3, fat link-time
optimization, one code generation unit, stripped symbols, and abort-on-panic.

These selected `just bench` results used an Apple M4, macOS 26.5.1, and Rust
1.98.0 on 2026-08-31. The command measures internal code with the Cargo
`release` profile. Lower values are better.

| Benchmark | Work | Median | p95 |
| --- | --- | ---: | ---: |
| File search | 100,000 files, three warm queries | 5.122 ms | 5.277 ms |
| File search | 500,000 files, three warm queries | 10.639 ms | 11.836 ms |
| SSE parsing | 10,000 events | 1.144 ms | 1.486 ms |
| Grep | 1,000 files and 200 matches | 14.611 ms | 16.121 ms |
| Incremental Markdown | 200 streaming prefix renders | 15.672 ms | 16.299 ms |
| Unchanged frame | 10,000 logical rows | 133.220 us | 134.583 us |

The matched PGO benchmark used the same machine and three held-out trials.
Both executables used the `dist` profile and macOS ICF.

| PGO result | Ordinary | PGO | Change |
| --- | ---: | ---: | ---: |
| `kiss --help` startup | 3.696 ms | 3.676 ms | 0.52% faster |
| Geometric mean latency | 1.000x | 0.985x | 1.51% faster |
| Executable size | 17.16 MiB | 14.87 MiB | 13.37% smaller |
| gzip size | 8.17 MiB | 7.36 MiB | 9.94% smaller |

## Install

macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/racetozero/kiss/main/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/racetozero/kiss/main/install.ps1 | iex
```

The installer selects the correct release, verifies its SHA-256 checksum, and
installs `kiss` in your user binary directory. The repository and its releases
must be public for these commands to work without GitHub authentication.

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
