import { MockProvider, Session } from "../dist/index.mjs";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

Deno.test("Deno loads N-API through the ESM entry point and runs a session", async () => {
  const directory = await Deno.makeTempDir({ prefix: "kiss-deno-" });
  const provider = await MockProvider.start(directory, [[{ text: "hello from deno" }]]);
  const session = await Session.create({
    cwd: directory,
    model: "mock/mock-1",
    modelsFile: provider.catalogPath,
    noContextFiles: true,
  });
  assert(await session.ping(), "ping should return true");
  const state = await session.state();
  assert(state.model?.provider === "mock", "the mock model should be selected");
  session.close();
  provider.stop();
  await Deno.remove(directory, { recursive: true });
});
