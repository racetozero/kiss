import init, { KissAgent, createOpenAICompatibleProvider } from "../browser.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("the OpenAI-compatible fetch adapter streams through the WASM tool loop", async () => {
  await init();
  const bodies: Array<Record<string, unknown>> = [];
  let requestCount = 0;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen: () => {} }, async (request) => {
    bodies.push(await request.json());
    requestCount += 1;
    const encoder = new TextEncoder();
    const records = requestCount === 1
      ? [
        { id: "r1", model: "fixture", choices: [{ delta: { tool_calls: [{ index: 0, id: "add-1", function: { name: "add", arguments: '{"left":2,"right":' } }] } }] },
        { choices: [{ delta: { tool_calls: [{ index: 0, function: { arguments: "4}" } }] }, finish_reason: "tool_calls" }] },
      ]
      : [
        { id: "r2", model: "fixture", choices: [{ delta: { content: "answer: " } }] },
        { choices: [{ delta: { content: "6" }, finish_reason: "stop" }], usage: { prompt_tokens: 5, completion_tokens: 2, total_tokens: 7 } },
      ];
    const stream = new ReadableStream({
      start(controller) {
        for (const record of records) controller.enqueue(encoder.encode(`data: ${JSON.stringify(record)}\n\n`));
        controller.enqueue(encoder.encode("data: [DONE]\n\n"));
        controller.close();
      },
    });
    return new Response(stream, { headers: { "content-type": "text/event-stream" } });
  });

  try {
    const address = server.addr as Deno.NetAddr;
    const provider = createOpenAICompatibleProvider({
      url: `http://127.0.0.1:${address.port}/v1/chat/completions`,
      apiKey: "fixture-key",
    });
    const agent = KissAgent.create({
      model: { id: "fixture", provider: "openai", api: "openai-completions" },
    }, provider);
    agent.registerTool({
      name: "add",
      description: "Add two",
      parameters: {
        type: "object",
        properties: { left: { type: "number" }, right: { type: "number" } },
        required: ["left", "right"],
      },
    }, (args: { left: number; right: number }) => String(args.left + args.right));

    const deltas: string[] = [];
    const result = await agent.prompt("Add 2 and 4", (event: Record<string, unknown>) => {
      const update = event.assistantMessageEvent as Record<string, unknown> | undefined;
      if (event.type === "message_update" && update?.type === "text_delta") {
        deltas.push(String(update.delta));
      }
    });
    assert(result.text === "answer: 6", "streamed text should become authoritative final content");
    assert(deltas.join("") === "answer: 6", "stream deltas should reach KISS events");
    assert(requestCount === 2, "tool use should trigger another HTTP model request");
    const secondMessages = bodies[1].messages as Array<Record<string, unknown>>;
    assert(secondMessages.some((message) => message.role === "tool" && message.content === "6"), "tool output should use OpenAI's tool role");
    agent.close();
  } finally {
    await server.shutdown();
  }
});
