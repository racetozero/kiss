# @kiss-sdk/core-wasm

The complete KISS agent loop for browser WebAssembly. Conversation state,
model/tool turn sequencing, tool argument validation, normalized events,
cancellation, limits, and checkpoints run in WASM. It does **not** connect to a
native KISS process and has no WebSocket dependency.

```sh
wasm-pack build crates/kiss-core-wasm --target web --release
```

```ts
import init, {
  KissAgent,
  createOpenAICompatibleProvider,
  type ModelProvider,
} from "@kiss-sdk/core-wasm";

await init();

const provider: ModelProvider = async (request, emit, signal) => {
  // Use fetch(), an AI SDK, or a worker here. Translate its response into KISS
  // content blocks. emit() can publish normalized deltas while it streams.
  const answer = await myModel(request, { signal, onDelta(delta) {
    emit({ type: "text_delta", contentIndex: 0, delta });
  }});
  return {
    content: [{ type: "text", text: answer.text }],
    usage: answer.usage,
    stopReason: "stop",
  };
};

// A bundled adapter is also available for OpenAI-compatible Chat Completions:
// const provider = createOpenAICompatibleProvider({
//   url: "https://gateway.example/v1/chat/completions",
//   apiKey: shortLivedBrowserToken,
// });

const agent = KissAgent.create({
  model: { id: "my-model", provider: "host", api: "host" },
  systemPrompt: "Use available tools when useful.",
}, provider);

agent.registerTool({
  name: "lookup",
  description: "Look up a value",
  parameters: {
    type: "object",
    properties: { key: { type: "string" } },
    required: ["key"],
  },
}, async (args, { signal, onUpdate }) => {
  onUpdate("Looking up the value…");
  return database.get(args.key, { signal });
});

const result = await agent.prompt("Look up alpha", (event) => {
  if (event.type === "message_update") console.log(event);
});
console.log(result.text);

const checkpoint = agent.checkpoint();
agent.close();
```

The model provider receives the complete provider-neutral KISS context and tool
definitions. It may return text, thinking, image, and tool-call content blocks.
The dependency-free `createOpenAICompatibleProvider` helper translates this
contract to streaming OpenAI-compatible Chat Completions using browser `fetch`.
If it returns a tool call, the agent validates and executes that host capability,
adds the tool result to history, and performs the next model turn itself.

## Browser authority

WebAssembly does not grant native filesystem or process access. Register only
capabilities the application can safely provide:

- an in-memory or File System Access API workspace;
- a WebContainer or remote sandbox shell;
- host-owned MCP, database, or application tools;
- browser `fetch` with short-lived credentials.

Callbacks receive an `AbortSignal`; they must pass it to their underlying work.
Model stream callbacks must not retain and invoke `emit` after their provider
Promise settles. Tool callbacks follow the same rule for `onUpdate`.

While a prompt is active, `steer()` inserts a user message before the next model
turn and `followUp()` inserts one after the run would otherwise stop. Model and
thinking settings, retained messages, and history clearing are available while
idle. The portable schema validator covers `type`, `required`, `properties`,
`items`, `enum`, and `additionalProperties`, which is the vocabulary used by
KISS tool inputs; hosts needing additional JSON Schema keywords should validate
those in their tool callback as well.

Use `@kiss-sdk/wasm` instead when the browser must control the full native KISS
filesystem and shell through an explicitly started RPC process. The two packages
are separate because their security and deployment models differ.

## Limits

The kernel bounds system instructions to 64 KiB, prompts to 1 MiB, checkpoints
to 8 MiB, tool schemas to 256 KiB, tool results to 1 MiB, registered tools to
128, and turns per prompt to 64. Only one prompt runs per agent. Checkpoints are
versioned conversation bytes and intentionally exclude callback functions,
credentials, tools, and instructions.

Current release budgets and measured baseline:

- WASM: 567,046 bytes raw / 207,274 bytes gzip -9
- generated wasm-bindgen loader: 35,043 bytes
- typed browser facade plus OpenAI-compatible adapter: 12,641 bytes
- initial linear memory: 17 pages / 1,114,112 bytes

Across three trials, the deterministic Deno performance fixture averaged 0.195
ms per warm prompt. Batches of 25 isolated agents averaged 1.649 ms. These
figures exclude model/network latency and exist to detect SDK regressions, not
to predict inference speed.

Run `deno test --allow-read --allow-net test/*.ts` after `wasm-pack build` to
exercise the actual generated module. The only listener is a hermetic fake
OpenAI-compatible endpoint; the core agent tests use no server or WebSocket.
