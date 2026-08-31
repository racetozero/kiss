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

Release builds use profile-guided optimization (PGO). KISS trains each target
on common CLI, model, MCP, and local mock-provider tasks before it builds the
release binary.

This held-out benchmark used an Apple M4, macOS 26.5.1, Rust 1.98.0, and the
`aarch64-apple-darwin` target on 2026-08-31. Both binaries used the `dist`
profile and safe identical code folding. Each result is the median of three
trials. Short tasks used 80 samples per trial. The local mock-provider task
used 30 samples per trial.

| Task | Ordinary release | PGO release | Change |
| --- | ---: | ---: | ---: |
| `kiss --help` startup | 3.696 ms | 3.676 ms | 0.52% faster |
| Model catalog search | 5.870 ms | 5.652 ms | 3.73% faster |
| MCP server lookup | 3.989 ms | 3.902 ms | 2.19% faster |
| Local mock-provider turn | 102.360 ms | 102.808 ms | 0.44% slower |
| Executable size | 17.16 MiB | 14.87 MiB | 13.37% smaller |
| gzip size | 8.17 MiB | 7.36 MiB | 9.94% smaller |

The geometric mean latency gain is 1.51%. These values apply to this machine
and target. Other release targets can have different results.

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
