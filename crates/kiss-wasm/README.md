# @kiss-sdk/wasm

A browser WebAssembly client for the language-neutral KISS RPC protocol.

Browsers cannot run KISS's filesystem and shell tools inside their sandbox. Run
the native agent explicitly and connect to it:

```sh
kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session
```

```ts
import init, { KissClient } from "@kiss-sdk/wasm";
await init();
const client = await KissClient.connect("ws://127.0.0.1:9944");
client.onEvent((event) => {
  if (event.type === "message_update" &&
      event.assistantMessageEvent.type === "text_delta") {
    console.log(event.assistantMessageEvent.delta);
  }
});
await client.prompt("List the files here");
```

Build with `wasm-pack build --target web --out-dir pkg`. See `demo/index.html`
for a complete page and the repository's `docs/rpc.md` for the protocol.
