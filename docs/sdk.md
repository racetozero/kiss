# KISS SDK

The SDK embeds KISS's agent loop, model providers, tools, history, compaction,
and streaming events in another application. Rust, Python 3.11+, and TypeScript
use one Rust dispatcher and one event encoder, so they have the same behavior.
For another language, use [RPC mode](rpc.md).

## Rust

Add the workspace crate (or its published version) and Tokio:

```toml
[dependencies]
kiss-sdk = "0.0.3"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust
use kiss_sdk::Session;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let session = Session::builder().tools(["read", "bash"]).build().await?;
    let mut events = session.events();
    let printer = tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if event.event_type() == "message_update" {
                if let Some(delta) = event.0["assistantMessageEvent"]["delta"].as_str() {
                    print!("{delta}");
                }
            }
            if event.event_type() == "agent_settled" { break; }
        }
    });
    session.prompt("What files are here?").await?;
    printer.await?;
    Ok(())
}
```

`SessionOptions` configures `cwd`, model/provider/key, `models_file`, thinking
level, tool allow/exclude lists, custom Rust tools, prompts, project trust,
event capacity, and session persistence. SDK sessions are in-memory by default;
choose `SessionSource::Create` to persist one.

## Python 3.11+

Build/install with `maturin` (published wheels use PyO3 `abi3-py311`):

```sh
pip install kiss-sdk
```

```python
import asyncio
from kiss_sdk import Event, Session, ToolName

async def main() -> None:
    async with await Session.create(tools=[ToolName.READ, ToolName.BASH]) as session:
        async def print_events() -> None:
            async for event in session.events():
                if event.type == "message_update":
                    update = event["assistantMessageEvent"]
                    if update["type"] == "text_delta":
                        print(update["delta"], end="", flush=True)
                if event.type == "agent_settled":
                    return
        printer = asyncio.create_task(print_events())
        await session.prompt("What files are here?")
        await printer

asyncio.run(main())
```

The package uses Python 3.11 `StrEnum` values (`ThinkingLevel`, `ToolName`,
`StreamingBehavior`, `QueueMode`) and exports `TypedDict` declarations for
commands, responses, models, messages, events, and statistics. Event JSON is
converted directly into Python dict/list objects in Rust, without a per-token
`json.loads`.

## TypeScript: Node, Bun, and Deno

```sh
npm install @kiss-sdk/node
```

```ts
import { Session } from "@kiss-sdk/node";

const session = await Session.create({ tools: ["read", "bash"] });
const events = session.events();
session.promptDetached("What files are here?");
for await (const event of events) {
  if (event.type === "message_update" &&
      event.assistantMessageEvent.type === "text_delta") {
    process.stdout.write(event.assistantMessageEvent.delta ?? "");
  }
  if (event.type === "agent_settled") break;
}
session.close();
```

The native addon uses N-API rather than V8-specific APIs, so the same package
loads in Node, Bun, and Deno's Node compatibility layer. Deno needs `--allow-ffi`
plus permissions required by enabled tools.

## Browser / WebAssembly

KISS offers two separate browser topologies.

### Local agent kernel

`@kiss-sdk/core-wasm` runs the conversation, model/tool turn loop, schema
validation, events, cancellation, limits, and checkpoints inside WebAssembly.
It needs no native KISS process, WebSocket, or JSPI support. The host explicitly
provides model and tool capabilities:

```sh
wasm-pack build crates/kiss-core-wasm --target web --release
```

```ts
import init, {
  KissAgent,
  createOpenAICompatibleProvider,
} from "@kiss-sdk/core-wasm";
await init();

const provider = createOpenAICompatibleProvider({
  url: "https://gateway.example/v1/chat/completions",
  apiKey: shortLivedBrowserToken,
});
const agent = KissAgent.create({
  model: { id: "my-model", provider: "openai", api: "openai-completions" },
}, provider);

agent.registerTool({
  name: "lookup",
  description: "Look up a value",
  parameters: { type: "object", properties: { key: { type: "string" } }, required: ["key"] },
}, async (args, { signal }) => lookup(args, { signal }));

const result = await agent.prompt("Look up alpha", console.log);
console.log(result.text);
```

If a model returns tool-call content, the WASM loop validates and executes the
registered callback, adds its result to conversation history, and performs the
next model turn. Model and tool callbacks receive `AbortSignal`. Browser agents
also expose `steer`, `followUp`, `abort`, `setModel`, `setThinkingLevel`,
`messages`, `clearHistory`, `state`, and bounded versioned `checkpoint` data.
See `crates/kiss-core-wasm/README.md` and its no-server browser demo.

Browsers still cannot acquire ambient native filesystem/process authority.
Applications can register safe implementations backed by browser storage, the
File System Access API, WebContainers, or a remote sandbox. Use short-lived
model credentials in public browser applications.

### Remote native agent

`@kiss-sdk/wasm` remains the small WebSocket RPC client for applications that
need KISS's native filesystem and shell implementations:

```sh
kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session
wasm-pack build crates/kiss-wasm --target web
```

```ts
import init, { KissClient } from "@kiss-sdk/wasm";
await init();
const client = await KissClient.connect("ws://127.0.0.1:9944");
client.onEvent((event) => console.log(event));
await client.prompt("What files are here?");
```

The current RPC WebSocket has no authentication or Origin enforcement. Treat it
as development-only even on loopback until handshake authentication lands.

## Consistent operations

| Operation | Rust | Python | TypeScript | RPC `type` |
|---|---|---|---|---|
| create | `Session::create` / builder | `Session.create` | `Session.create` | process startup |
| prompt and wait | `prompt` | `prompt` | `prompt` | `prompt` + wait for event |
| accept immediately | `prompt_detached` | `prompt_detached` | `promptDetached` | `prompt` |
| events | `events().recv()` | `async for ... in events()` | `for await ... of events()` | stdout/WebSocket lines |
| steer | `steer` | `steer` | `steer` | `steer` |
| follow up | `follow_up` | `follow_up` | `followUp` | `follow_up` |
| abort / wait | `abort` / `wait_idle` | `abort` / `wait_idle` | `abort` / `waitIdle` | `abort` |
| state/messages | `state` / `messages` | `state` / `messages` | `state` / `messages` | `get_state` / `get_messages` |
| model | `set_model` | `set_model` | `setModel` | `set_model` |
| thinking | `set_thinking_level` | `set_thinking_level` | `setThinkingLevel` | `set_thinking_level` |
| compact | `compact` | `compact` | `compact` | `compact` |
| shell | `bash` | `bash` | `bash` | `bash` |
| all commands | `execute` | `execute` | `execute` | command object |

Python uses snake_case and TypeScript uses camelCase by each language's normal
convention; wire fields are always camelCase.

## Prompting and events

`prompt()` waits for completion in in-process SDKs. The RPC command and
`promptDetached()` return at acceptance so clients can send `abort` while work
continues. If a prompt arrives while streaming, pass `streamingBehavior:
"steer"` or `"followUp"`; omitting it is an error.

Subscribe before sending a prompt. Important event types are `agent_start`,
`turn_start`, `message_start`, `message_update`, `tool_execution_start`,
`tool_execution_update`, `tool_execution_end`, `message_end`, `turn_end`,
`agent_end`, and SDK-level `agent_settled`. `message_update` contains deltas,
not a repeatedly growing snapshot. A bounded event channel prevents an idle
consumer from growing memory without limit; `event_lag` says how many events a
slow subscriber missed.

## Tools and safety

Defaults are `read`, `write`, `edit`, and `bash`; optional built-ins are `grep`,
`find`, `ls`, and `mcp`. Set a read-only allowlist for untrusted prompts. Project
resource loading is off by default in SDK sessions; explicitly enable
`trust_project_files` only for a trusted directory.

Rust callers can implement `kiss_agent::AgentTool` and pass `custom_tools`.
Cross-language custom callback tools are intentionally not supported yet: a
callback during an agent tool batch needs cancellation, streaming updates, and
safe runtime re-entry semantics, and pretending a plain synchronous callback
is equivalent would be unsafe.
