# @kiss-sdk/node

A native N-API TypeScript SDK for the KISS coding agent. It runs on Node.js,
Bun, and Deno's Node-compatibility layer.

```ts
import { Session } from "@kiss-sdk/node";

const session = await Session.create({ tools: ["read", "bash"] });
const events = session.events();
session.promptDetached("List the files here");
for await (const event of events) {
  if (event.type === "message_update" &&
      event.assistantMessageEvent.type === "text_delta") {
    process.stdout.write(event.assistantMessageEvent.delta ?? "");
  }
  if (event.type === "agent_settled") break;
}
session.close();
```

Bun loads the same package directly. Deno requires permissions for the native
addon and for whichever tools you enable:

```sh
deno run --allow-ffi --allow-read --allow-write --allow-run --allow-net app.ts
```

See `docs/sdk.md` and `docs/rpc.md` in the repository for the complete API.
