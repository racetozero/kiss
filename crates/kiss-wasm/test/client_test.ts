// This imports and executes the actual wasm-pack artifact. The small local
// WebSocket endpoint emulates the KISS RPC transport; Rust's RPC server has its
// own end-to-end tests in crates/kiss-sdk/tests/rpc_end_to_end.rs.
import init, { KissClient } from "../pkg/kiss_wasm.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("the generated WASM client correlates responses and publishes events", async () => {
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen: () => {} }, (request) => {
    const { socket, response } = Deno.upgradeWebSocket(request);
    socket.onmessage = (message) => {
      const command = JSON.parse(String(message.data));
      socket.send(JSON.stringify({
        type: "response", id: command.id, command: command.type,
        success: true, data: command.type === "ping" ? { pong: true } : {},
      }));
      setTimeout(() => socket.send(JSON.stringify({ type: "agent_settled" })), 20);
    };
    return response;
  });
  try {
    await init();
    const address = server.addr as Deno.NetAddr;
    const client = await KissClient.connect(`ws://127.0.0.1:${address.port}`);
    const events: Array<Record<string, unknown>> = [];
    client.onEvent((event: Record<string, unknown>) => events.push(event));
    const response = await client.execute({ type: "ping" });
    assert(response.success === true, "ping should succeed");
    assert(response.command === "ping", "the command should correlate");
    assert(response.data.pong === true, "pong should be true");
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert(events.some((event) => event.type === "agent_settled"), "event callback should run");
    client.close();
  } finally {
    await server.shutdown();
  }
});
