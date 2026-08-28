# kiss

[Keep it simple, stupid](https://en.wikipedia.org/wiki/KISS_principle).

A fast, minimal terminal coding agent written in Rust 2024, modeled on the
architecture of [pi](https://github.com/earendil-works/pi), without the
TypeScript extension runtime.

- **Multi-provider**: a generated Pi 0.84.3 catalog with more than 1,000
  models. It includes Anthropic, OpenAI, OpenAI Codex, Google, Vertex AI,
  Azure OpenAI, Bedrock, OpenRouter, GitHub Copilot, Groq, Mistral, xAI,
  regional token plans, and gateways. Custom OpenAI-compatible servers use
  `~/.kiss/agent/models.json`.
- **Agent loop** with steering (send while it works) and follow-up queues,
  parallel tool execution, and automatic retry on transient provider errors.
- **Tools**: `read`, `write`, `edit`, and `bash` are enabled by default.
  `grep`, `find`, and `ls` are optional built-ins that you can enable with
  `--tools`. Gitignore-aware search runs in-process on ripgrep's own libraries.
- **MCP**: stdio and Streamable HTTP servers load from shared MCP files,
  `.mcp.json`, and KISS configuration. Configured servers use one lazy `mcp`
  proxy tool, cached metadata, OAuth, and request cancellation. They do not
  start before the first TUI frame.
- **Sessions** as append-only JSONL trees (wire-compatible with pi's session
  format v3): branch in place, fork, clone, name, resume, import, and export to
  HTML or JSONL.
- **Compaction**: automatic context summarization when the window fills,
  manual `/compact [focus]`. Official OpenAI Responses and OpenAI Codex models
  use OpenAI server-side compaction and keep a readable Pi summary. Other
  providers keep Pi-style local summary compaction.
- **Data-driven extensibility**: skills (`SKILL.md`), prompt templates
  (Markdown + `$1`/`$@` expansion), themes (JSON), custom model catalogs
  (JSON). No plugin runtime, no dynamic code loading.
- **Fast**: single static binary, differential terminal rendering, embedded
  model catalog, no startup I/O beyond your settings.

## Install

Tagged releases publish archives for macOS on Apple Silicon and Intel, Linux
on aarch64 and x86_64 with GNU or musl, and Windows on x86_64. Download an
archive from the [GitHub Releases](https://github.com/racetozero/kiss/releases)
page, or use the checked-in installer. Because this repository is private,
the command uses your active GitHub CLI login:

```bash
(
  set -e
  installer="$(mktemp)"
  trap 'rm -f "$installer"' EXIT
  gh api -H "Accept: application/vnd.github.raw+json" \
    repos/racetozero/kiss/contents/install.sh > "$installer"
  sh "$installer"
)
```

On Windows PowerShell:

```powershell
$installer = gh api -H "Accept: application/vnd.github.raw+json" repos/racetozero/kiss/contents/install.ps1
if ($LASTEXITCODE -ne 0) { throw "Could not download the KISS installer" }
$installer | Invoke-Expression
```

The installer verifies the release archive against its `.sha256` file. Set
`KISS_VERSION`, `KISS_TARGET`, or `KISS_INSTALL_DIR` to override the release,
target, or default `$HOME/.local/bin` destination. You can also install the
current source checkout with Cargo:

```bash
cargo install --path crates/kiss
```

## Use

```bash
export ANTHROPIC_API_KEY=...   # or OPENAI_API_KEY, GEMINI_API_KEY, ...

kiss login openai-codex                 # browser and local callback
kiss login openai-codex --device-auth   # headless device-code login
kiss login anthropic                    # Claude Pro or Max browser login
kiss login anthropic --device-auth      # headless pasted callback flow
kiss login anthropic --api-key sk-ant-  # save an Anthropic API key
kiss auth                               # show credential sources, not secrets
kiss auth import                        # import from installed coding agents
kiss auth import anthropic --yes        # non-interactive selected import
kiss logout openai-codex

kiss mcp add local -- npx -y @modelcontextprotocol/server-everything
kiss mcp add remote --url https://example.com/mcp --auth oauth
kiss mcp login remote                 # browser and local callback
kiss mcp login remote --no-browser    # paste the full redirect URL
kiss mcp list
kiss mcp test local

kiss                           # interactive
kiss "explain this repo"       # interactive with initial prompt
kiss -p "summarize the build"  # print mode
cat err.log | kiss -p "what broke?"
kiss --mode json "run tests"   # JSON event stream
kiss -c                        # continue latest session
kiss --model sonnet:high       # model pattern + thinking level
kiss --list-models             # catalog
```

Inside the TUI: `enter` sends (or queues steering while the agent works),
`shift+enter` adds a newline, `alt+enter` queues a follow-up, `alt+up`
restores queued messages, and `esc` or `ctrl+c` cancels active work.
`ctrl+d` exits when the editor is empty. `ctrl+l` selects a model,
`ctrl+p` and `shift+ctrl+p` cycle models, and `shift+tab` cycles thinking
effort. `ctrl+o` expands tool output, `ctrl+t` shows or hides thinking, and
`ctrl+x` copies the last response. `!cmd` runs shell and `!!cmd` keeps its
output out of context. `/hotkeys` lists the active keyboard shortcuts.
Type `/` to open the command pane. Type to filter, use Up and Down to select,
use Tab to complete, use Enter to run, and use Escape to close the pane.
The footer always shows the current thinking effort for reasoning models.

The fixed commands match Pi: `/settings`, `/model`, `/scoped-models`,
`/export`, `/import`, `/share`, `/copy`, `/name`, `/session`, `/changelog`,
`/hotkeys`, `/fork`, `/clone`, `/tree`, `/trust`, `/login`, `/logout`, `/new`,
`/compact`, `/resume`, `/reload`, and `/quit`. Pi's shipped `/llama` command
is also available for a configured llama.cpp router. `/login` opens provider
selection in the TUI. OpenAI Codex and Anthropic use browser OAuth. Other
providers use a masked API-key prompt. `/logout` removes only credentials that
Kiss saved. It does not change environment variables.

Use `/login llama.cpp` to save a router URL such as
`http://127.0.0.1:8080`. Enter `URL|API_KEY` when the router requires a key.
Then use `/llama` to load or unload router models. Use `/share` to create a
secret GitHub gist through an installed and authenticated `gh` command.

`kiss auth import` discovers compatible credentials from OpenAI Codex,
Claude Code, Pi, OpenCode, OpenClaw, and Hermes. On macOS it also discovers
Claude Code OAuth in Keychain. Kiss shows the application and location first,
then asks before it copies any credential to its mode-600 auth file. Shared
stores can supply OAuth credentials and API keys for providers in the built-in
catalog.

`kiss mcp` has `list`, `get`, `add`, `update`, `remove`, `enable`, `disable`,
`login`, `logout`, and `test` commands. Add `--scope project` to write
`.mcp.json`. The default user scope writes `~/.kiss/agent/mcp.json`. KISS also
reads common global MCP files. Project MCP files load only after project trust.
OAuth login uses PKCE and dynamic client registration when the server supports
it. `--no-browser` prints the authorization URL and accepts the full callback
URL. Use this mode for SSH and other headless sessions.

## Configuration

| File | Purpose |
|------|---------|
| `~/.kiss/agent/settings.json` | Global settings (deep-merged with project) |
| `.kiss/settings.json` | Project settings (loaded after trust) |
| `~/.kiss/agent/models.json` | Custom providers and models |
| `~/.kiss/agent/auth.json` | Stored API keys and refreshable OAuth credentials (mode 600) |
| `~/.kiss/agent/mcp.json`, `.mcp.json` | User and project MCP servers |
| `~/.kiss/agent/mcp-oauth.json` | URL-bound MCP OAuth credentials (mode 600) |
| `~/.kiss/agent/keybindings.json` | Key overrides |
| `~/.kiss/agent/skills/`, `.kiss/skills/`, `.agents/skills/` | Skills |
| `~/.kiss/agent/prompts/`, `.kiss/prompts/` | Prompt templates |
| `AGENTS.md` / `CLAUDE.md` | Project context files |

## Pi upstream baseline

Kiss tracks [Pi v0.84.3](https://github.com/earendil-works/pi/releases/tag/v0.84.3).
The canonical release and commit data are in `[workspace.metadata.pi]` in
`Cargo.toml`. Run `scripts/check-pi-upstream.sh` to check for a newer release.
A weekly GitHub Actions job runs the same check.
The latest full interaction audit covers Pi release commit
`4e58f324fae8ebfa98a3d45181fb248072a2afac`.

OpenAI Codex uses Pi's cached WebSocket transport by default. It reuses one
connection for a session and sends only new input after the first response.
If the connection fails before output starts, it uses the REST event stream
for that session. Set `"transport"` in `settings.json` to `"auto"`, `"sse"`,
`"websocket"`, or `"websocket-cached"`.

For an official OpenAI Responses or OpenAI Codex model, `/compact` sends the
current provider context to OpenAI with a `compaction_trigger`. KISS stores the
returned opaque compaction item in the local session JSONL file and reuses it
only with the same compatible model. It also stores the normal readable text
summary for export, tree operations, and provider changes. If the remote call
fails, KISS uses the local summary. This sends conversation context to OpenAI
and saves a provider-native item that is not human-readable.

Anthropic subscription requests use the Claude Code OAuth request protocol.
This includes bearer authentication, Claude Code identity headers, canonical
tool names, optional local Claude identity metadata, and the final serialized
request fingerprint used by Claude Code. API-key requests keep the normal
Anthropic API format.

## Test

Install `cargo-nextest`, then use the same gates as the project plans:

```bash
cargo nextest run --workspace --all-targets
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Release

KISS pins its release compiler in `rust-toolchain.toml` and uses cargo-dist
0.32.0. The installed cargo-dist command is named `dist`:

```bash
cargo install cargo-dist --version 0.32.0 --locked
dist --version
```

To prepare a release, update `[workspace.package].version` in `Cargo.toml` and
add the release notes to `CHANGELOG.md`. Use just to run all test gates,
inspect the cargo-dist plan, build one native archive, verify its checksum,
and run the native binary:

```bash
just release-check 0.2.0
```

The command detects the build-host target. Pass a second argument to override
it. The macOS path applies the same safe ICF flags as the release workflow.

Inspect the archive in `target/distrib`, then commit the version and changelog
changes on `main`. Run the release command:

```bash
just release 0.2.0
```

The release command requires a clean `main` branch. It repeats the checks and
asks you to type `v0.2.0`. It then pushes `main`, creates an annotated tag, and
pushes the tag. For a controlled non-interactive release, set
`KISS_RELEASE_CONFIRM=v0.2.0`.

The tag starts the release workflow. It builds all configured archives,
checksums, shell and PowerShell installers, and GitHub attestations, then
creates the GitHub Release. Do not move a published tag. If a release needs a
fix, increase the version and create a new tag.

The Justfile calls `scripts/release.sh`. Use `dist plan --tag=v0.2.0` and
`dist build --allow-dirty --tag=v0.2.0 --target=<target>` directly when you
need to diagnose cargo-dist.

`dist generate` regenerates `.github/workflows/release.yml`. KISS has two
reviewed changes in that generated file: macOS builds use safe ICF, and the
default token permission is read-only. After a cargo-dist update, reapply
these changes and inspect the generated diff before you commit it.

## License

MIT. Architecture inspired by [pi](https://github.com/earendil-works/pi)
(MIT, © Mario Zechner and contributors).
