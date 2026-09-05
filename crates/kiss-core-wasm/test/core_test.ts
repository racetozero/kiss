import init, { KissAgent } from "../pkg/kiss_core_wasm.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

const model = { id: "fixture-model", provider: "fixture", api: "host" };

Deno.test("the agent and host-tool loop run entirely inside WASM", async () => {
  await init();
  const requests: Array<Record<string, unknown>> = [];
  const events: Array<Record<string, unknown>> = [];
  let calls = 0;
  const provider = async (
    request: Record<string, unknown>,
    emit: (event: Record<string, unknown>) => void,
    signal: AbortSignal,
  ) => {
    assert(!signal.aborted, "model signal should start live");
    requests.push(request);
    calls += 1;
    if (calls === 1) {
      return {
        content: [{
          type: "toolCall",
          id: "call_add",
          name: "add",
          arguments: { left: 2, right: 3 },
        }],
        stopReason: "toolUse",
      };
    }
    emit({ type: "text_start", contentIndex: 0 });
    emit({ type: "text_delta", contentIndex: 0, delta: "5" });
    emit({ type: "text_end", contentIndex: 0, content: "5" });
    return { content: [{ type: "text", text: "5" }], stopReason: "stop" };
  };

  const agent = KissAgent.create({ model, systemPrompt: "Use tools." }, provider);
  agent.registerTool({
    name: "add",
    description: "Add two numbers",
    parameters: {
      type: "object",
      properties: { left: { type: "number" }, right: { type: "number" } },
      required: ["left", "right"],
      additionalProperties: false,
    },
  }, (args: { left: number; right: number }) => String(args.left + args.right));

  const result = await agent.prompt("What is 2 + 3?", (event: Record<string, unknown>) => {
    events.push(event);
  });
  assert(result.text === "5", "the second model turn should become the result");
  assert(result.stopReason === "stop", "the final stop reason should be preserved");
  assert(calls === 2, "the WASM loop should request a second model turn after the tool");
  const secondContext = (requests[1].context as { messages: Array<Record<string, unknown>> }).messages;
  assert(secondContext.some((message) => message.role === "toolResult"), "tool output must enter model context");
  const eventTypes = events.map((event) => event.type);
  for (const type of [
    "agent_start",
    "tool_execution_start",
    "tool_execution_end",
    "message_update",
    "agent_end",
    "agent_settled",
  ]) {
    assert(eventTypes.includes(type), `missing ${type} event`);
  }
  assert(agent.state().messageCount === result.messages.length, "history should be retained in WASM");

  const checkpoint = agent.checkpoint();
  const restoredRequests: Array<Record<string, unknown>> = [];
  const restored = KissAgent.create({
    model,
    checkpoint,
  }, (request: Record<string, unknown>) => {
    restoredRequests.push(request);
    return { content: [{ type: "text", text: "restored" }] };
  });
  const restoredResult = await restored.prompt("continue");
  assert(restoredResult.text === "restored", "restored agent should run");
  const restoredContext = restoredRequests[0].context as { messages: Array<Record<string, unknown>> };
  assert(restoredContext.messages.length > 1, "checkpoint history should reach the next model request");

  restored.close();
  restored.close();
  agent.close();
  let closed = false;
  try {
    await agent.prompt("must fail");
  } catch (error) {
    closed = String(error).includes("KISS_CLOSED");
  }
  assert(closed, "closed agents must reject prompts");
});

Deno.test("steering and idle controls are owned by the WASM session", async () => {
  await init();
  let releaseFirst: (() => void) | undefined;
  let entered = false;
  let calls = 0;
  const contexts: Array<{ messages: Array<Record<string, unknown>> }> = [];
  const agent = KissAgent.create({ model }, async (request: { context: { messages: Array<Record<string, unknown>> } }) => {
    calls += 1;
    contexts.push(request.context);
    if (calls === 1) {
      entered = true;
      await new Promise<void>((resolve) => releaseFirst = resolve);
      return { content: [{ type: "text", text: "first" }] };
    }
    return { content: [{ type: "text", text: calls === 2 ? "steered" : "followed" }] };
  });

  const pending = agent.prompt("begin");
  while (!entered) await Promise.resolve();
  agent.steer("new priority");
  agent.followUp("after steering");
  assert(agent.state().steeringCount === 1, "steering should be queued inside WASM");
  assert(agent.state().followUpCount === 1, "follow-up should be queued inside WASM");
  releaseFirst?.();
  const result = await pending;
  assert(result.text === "followed", "follow-up should become the final model turn");
  assert(calls === 3, "steering and follow-up should each produce one additional turn");
  assert(contexts[1].messages.some((message) =>
    message.role === "user" && message.content === "new priority"
  ), "the steered message should enter the second model context");
  assert(contexts[2].messages.some((message) =>
    message.role === "user" && message.content === "after steering"
  ), "the follow-up message should enter the final model context");

  agent.setModel({ id: "replacement", provider: "fixture", api: "host" });
  agent.setThinkingLevel("high");
  assert(agent.state().model.id === "replacement", "the model should be mutable while idle");
  assert(agent.state().thinkingLevel === "off", "unsupported reasoning must be clamped by the model");
  assert(agent.messages().length > 0, "retained messages should be readable");
  agent.clearHistory();
  assert(agent.messages().length === 0, "history should clear while preserving the agent");
  agent.close();
});

Deno.test("schema failures stay in the agent loop and cancellation reaches the host", async () => {
  await init();
  let calls = 0;
  let toolCalled = false;
  const schemaAgent = KissAgent.create({ model, maxTurns: 2 }, () => {
    calls += 1;
    if (calls === 1) {
      return {
        content: [{ type: "toolCall", id: "bad", name: "strict", arguments: {} }],
        stopReason: "toolUse",
      };
    }
    return { content: [{ type: "text", text: "recovered" }] };
  });
  schemaAgent.registerTool({
    name: "strict",
    description: "Requires a value",
    parameters: { type: "object", properties: { value: { type: "string" } }, required: ["value"] },
  }, () => {
    toolCalled = true;
    return "should not run";
  });
  const schemaResult = await schemaAgent.prompt("validate");
  assert(schemaResult.text === "recovered", "the model should recover from schema feedback");
  assert(!toolCalled, "invalid arguments must not invoke host authority");
  schemaAgent.close();

  let sawAbort = false;
  let entered = false;
  let abortCalls = 0;
  const abortAgent = KissAgent.create({ model }, (
    _request: unknown,
    _emit: unknown,
    signal: AbortSignal,
  ) => {
    abortCalls += 1;
    if (abortCalls > 1) return { content: [{ type: "text", text: "recovered" }] };
    return new Promise((_resolve, reject) => {
      entered = true;
      signal.addEventListener("abort", () => {
        sawAbort = true;
        reject(new DOMException("aborted", "AbortError"));
      }, { once: true });
    });
  });
  const pending = abortAgent.prompt("wait forever");
  while (!entered) await Promise.resolve();

  let busy = false;
  try {
    await abortAgent.prompt("overlap");
  } catch (error) {
    busy = String(error).includes("KISS_BUSY");
  }
  assert(busy, "overlapping prompts must be rejected");
  abortAgent.abort();
  const aborted = await pending;
  assert(aborted.stopReason === "aborted", "abort should settle as data");
  assert(sawAbort, "the host model AbortSignal must fire");
  assert(abortAgent.state().isStreaming === false, "agent should return to idle");
  const recovered = await abortAgent.prompt("try again");
  assert(recovered.text === "recovered", "a new prompt should work after cancellation");
  abortAgent.close();
});
