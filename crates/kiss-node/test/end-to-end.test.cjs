const test = require("node:test");
const assert = require("node:assert/strict");
const { mkdtemp, readFile } = require("node:fs/promises");
const { tmpdir } = require("node:os");
const { join } = require("node:path");
const { MockProvider, Session } = require("../dist/index.js");

async function fixture(script) {
  const directory = await mkdtemp(join(tmpdir(), "kiss-node-"));
  const provider = await MockProvider.start(directory, script);
  const session = await Session.create({
    cwd: directory,
    model: "mock/mock-1",
    modelsFile: provider.catalogPath,
    noContextFiles: true,
  });
  return { directory, provider, session };
}

test("a prompt streams and writes a real file", async () => {
  const { directory, provider, session } = await fixture([
    [{ toolCall: { id: "call_1", name: "write", arguments: {
      path: "hello.txt", content: "hello from node\n",
    } } }],
    [{ text: "Done." }],
  ]);
  const events = session.events();
  const seen = [];
  const collector = (async () => {
    for await (const event of events) {
      seen.push(event);
      if (event.type === "agent_settled") return;
    }
  })();
  await session.prompt("create hello.txt");
  await collector;
  assert.equal(await readFile(join(directory, "hello.txt"), "utf8"), "hello from node\n");
  assert.ok(seen.some((event) => event.type === "tool_execution_start" && event.toolName === "write"));
  assert.ok(seen.some((event) => event.type === "tool_execution_end" && event.isError === false));
  const text = seen
    .filter((event) => event.type === "message_update" && event.assistantMessageEvent.type === "text_delta")
    .map((event) => event.assistantMessageEvent.delta ?? "").join("");
  assert.equal(text, "Done.");
  assert.equal(await session.lastAssistantText(), "Done.");
  assert.equal(provider.requests().length, 2);
  session.close();
});

test("state, models, tools, and ping use typed helpers", async () => {
  const { session } = await fixture([[{ text: "hello" }]]);
  const state = await session.state();
  assert.equal(state.model.provider, "mock");
  assert.equal(state.thinkingLevel, "off");
  assert.deepEqual(state.tools, ["read", "write", "edit", "bash"]);
  assert.equal(await session.ping(), true);
  assert.deepEqual(await session.tools(), ["read", "write", "edit", "bash"]);
  assert.deepEqual((await session.availableModels("mock")).map((model) => model.id), ["mock-1"]);
  assert.deepEqual(await session.availableThinkingLevels(), ["off"]);
  session.close();
});

test("direct bash returns a nonzero exit and enters history", async () => {
  const { session } = await fixture([[{ text: "hello" }]]);
  const result = await session.bash("printf node-bash; exit 7");
  assert.equal(result.output, "node-bash");
  assert.equal(result.exitCode, 7);
  const messages = await session.messages();
  assert.equal(messages.at(-1).role, "bashExecution");
  session.close();
});

test("raw execute and clean failures share the RPC response shape", async () => {
  const { session } = await fixture([[{ text: "hello" }]]);
  assert.deepEqual(await session.execute({ type: "ping" }), {
    type: "response", command: "ping", success: true, data: { pong: true },
  });
  await assert.rejects(session.setModel("nope", "nope"), /nope\/nope/);
  await assert.rejects(session.execute({ type: "teleport" }), /unknown variant|invalid/i);
  session.close();
});
