// This imports and executes the actual wasm-pack artifact. The small local
// WebSocket endpoint emulates the KISS RPC transport. Rust's RPC server has its
// own end-to-end tests in crates/kiss-sdk/tests/rpc_end_to_end.rs.
import init, { KissClient } from "../pkg/kiss_wasm.js"

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message)
}

Deno.test("the generated WASM client correlates responses and publishes events", async () => {
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen: () => {} }, (request) => {
    const { socket, response } = Deno.upgradeWebSocket(request)
    socket.onmessage = (message) => {
      const command = JSON.parse(String(message.data))
      const data =
        command.type === "ping"
          ? { pong: true }
          : command.type === "get_session_stats"
            ? {
                tokens: {
                  input: 6,
                  output: 5,
                  cacheRead: 4,
                  cacheWrite: 0,
                  cacheReadAvailable: true,
                  total: 15,
                },
                cost: 0.1,
              }
            : {}
      socket.send(
        JSON.stringify({
          type: "response",
          id: command.id,
          command: command.type,
          success: true,
          data,
        }),
      )
      setTimeout(() => socket.send(JSON.stringify({ type: "agent_settled" })), 20)
    }
    return response
  })
  try {
    await init()
    const address = server.addr as Deno.NetAddr
    const client = await KissClient.connect(`ws://127.0.0.1:${address.port}`)
    const events: Array<Record<string, unknown>> = []
    let callbackCompleted = false
    client.onEvent((event: Record<string, unknown>) => {
      events.push(event)
      void client.execute({ type: "ping" })
      client.clearEventHandler()
      callbackCompleted = true
    })
    const response = await client.execute({ type: "ping" })
    assert(response.success === true, "ping should succeed")
    assert(response.command === "ping", "the command should correlate")
    assert(response.data.pong === true, "pong should be true")
    const stats = await client.sessionStats()
    assert(stats.data.tokens.cacheRead === 4, "cache reads should be available")
    assert(stats.data.tokens.cacheReadAvailable === true, "cache availability should be available")
    await new Promise((resolve) => setTimeout(resolve, 100))
    assert(
      events.some((event) => event.type === "agent_settled"),
      "event callback should run",
    )
    assert(callbackCompleted, "event callbacks must be able to call client methods")
    assert(events.length === 1, "a callback must be able to remove itself")
    client.close()
  } finally {
    await server.shutdown()
  }
})

Deno.test("a closed client rejects new commands", async () => {
  let closed!: () => void
  const closeSeen = new Promise<void>((resolve) => { closed = resolve })
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen: () => {} }, (request) => {
    const { socket, response } = Deno.upgradeWebSocket(request)
    socket.onclose = closed
    return response
  })
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    await init()
    const address = server.addr as Deno.NetAddr
    const client = await KissClient.connect(`ws://127.0.0.1:${address.port}`)
    client.close()
    await closeSeen
    const result = await Promise.race([
      client.execute({ type: "ping" }).then(() => "resolved", () => "rejected"),
      new Promise<string>((resolve) => { timer = setTimeout(() => resolve("pending"), 200) }),
    ])
    assert(result === "rejected", `a command after close must reject, got ${result}`)
    client.close()
    client.free()
  } finally {
    clearTimeout(timer)
    await server.shutdown()
  }
})

Deno.test("freeing a client closes its owned connection", async () => {
  let closed!: () => void
  let peer: WebSocket | undefined
  const closeSeen = new Promise<string>((resolve) => { closed = () => resolve("closed") })
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen: () => {} }, (request) => {
    if (new URL(request.url).pathname === "/reject") return new Response("rejected", { status: 400 })
    const { socket, response } = Deno.upgradeWebSocket(request)
    peer = socket
    socket.onclose = closed
    return response
  })
  let timer: ReturnType<typeof setTimeout> | undefined
  try {
    await init()
    const address = server.addr as Deno.NetAddr
    const failed = await KissClient.connect(`ws://127.0.0.1:${address.port}/reject`)
      .then(() => false, () => true)
    assert(failed, "failed connection setup must reject")
    const client = await KissClient.connect(`ws://127.0.0.1:${address.port}`)
    const command = client.execute({ type: "ping" }).then(() => "resolved", () => "rejected")
    client.free()
    const result = await Promise.race([
      Promise.all([closeSeen, command]),
      new Promise<string>((resolve) => { timer = setTimeout(() => resolve("open"), 200) }),
    ])
    assert(
      Array.isArray(result) && result[0] === "closed" && result[1] === "rejected",
      `free must close the connection and reject pending commands, got ${result}`,
    )
  } finally {
    clearTimeout(timer)
    peer?.close()
    await server.shutdown()
  }
})
