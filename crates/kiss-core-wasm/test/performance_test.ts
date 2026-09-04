import init, { KissAgent } from "../pkg/kiss_core_wasm.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function percentile(values: number[], fraction: number): number {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.ceil(sorted.length * fraction) - 1];
}

Deno.test("warm turns and 25 isolated agents stay within browser budgets", async () => {
  const initStarted = performance.now();
  await init();
  const initMs = performance.now() - initStarted;
  assert(initMs < 1_000, `cached/module initialization took ${initMs.toFixed(3)}ms`);

  const provider = () => ({ content: [{ type: "text", text: "ok" }] });
  const agent = KissAgent.create({
    model: { id: "performance", provider: "fixture", api: "host" },
    maxHistoryMessages: 10,
  }, provider);
  for (let index = 0; index < 10; index += 1) await agent.prompt("warmup");

  const samples: number[] = [];
  for (let index = 0; index < 100; index += 1) {
    const started = performance.now();
    await agent.prompt("measure");
    samples.push(performance.now() - started);
  }
  const p50 = percentile(samples, 0.5);
  const p95 = percentile(samples, 0.95);
  assert(p50 < 5, `warm prompt p50 exceeded 5ms: ${p50.toFixed(3)}ms`);
  assert(p95 < 20, `warm prompt p95 exceeded 20ms: ${p95.toFixed(3)}ms`);
  agent.close();

  const agents = Array.from({ length: 25 }, () => KissAgent.create({
    model: { id: "capacity", provider: "fixture", api: "host" },
  }, provider));
  const capacityStarted = performance.now();
  const results = await Promise.all(agents.map((entry, index) => entry.prompt(`agent-${index}`)));
  const capacityMs = performance.now() - capacityStarted;
  assert(results.every((result) => result.text === "ok"), "all isolated agents must complete");
  assert(agents.every((entry) => entry.state().messageCount === 2), "agent histories must remain isolated");
  assert(capacityMs < 1_000, `25-agent fixture exceeded 1s: ${capacityMs.toFixed(3)}ms`);
  for (const entry of agents) entry.close();

  console.log(`kiss-core-wasm performance: init=${initMs.toFixed(3)}ms warm-p50=${p50.toFixed(3)}ms warm-p95=${p95.toFixed(3)}ms agents25=${capacityMs.toFixed(3)}ms`);
});
