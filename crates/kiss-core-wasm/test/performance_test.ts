import init, { KissAgent } from "../pkg/kiss_core_wasm.js";

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(message);
}

function percentile(values: number[], fraction: number): number {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.ceil(sorted.length * fraction) - 1];
}

function report(
  name: string,
  samplesMs: number[],
  iterations: number,
  work: string,
): void {
  const meanMs = samplesMs.reduce((total, sample) => total + sample, 0) /
    samplesMs.length;
  const meanNs = Math.round(meanMs * 1_000_000);
  const medianNs = Math.round(percentile(samplesMs, 0.5) * 1_000_000);
  const p95Ns = Math.round(percentile(samplesMs, 0.95) * 1_000_000);
  console.log(
    `KISS_BENCH\t${name}\tmean_ns=${meanNs}\tmedian_ns=${medianNs}\tp95_ns=${p95Ns}` +
      `\tsamples=${samplesMs.length}\titerations=${iterations}\twork=${work}`,
  );
}

Deno.test("warm turns and 25 isolated agents stay within browser budgets", async () => {
  const initStarted = performance.now();
  await init();
  const initMs = performance.now() - initStarted;
  assert(
    initMs < 1_000,
    `cached/module initialization took ${initMs.toFixed(3)}ms`,
  );

  const provider = () => ({ content: [{ type: "text", text: "ok" }] });
  const agent = KissAgent.create({
    model: { id: "performance", provider: "fixture", api: "host" },
    maxHistoryMessages: 10,
  }, provider);
  for (let index = 0; index < 10; index += 1) await agent.prompt("warmup");

  const promptSamples: number[] = [];
  for (let index = 0; index < 100; index += 1) {
    const started = performance.now();
    await agent.prompt("measure");
    promptSamples.push(performance.now() - started);
  }
  const promptP50 = percentile(promptSamples, 0.5);
  const promptP95 = percentile(promptSamples, 0.95);
  assert(
    promptP50 < 5,
    `warm prompt p50 exceeded 5ms: ${promptP50.toFixed(3)}ms`,
  );
  assert(
    promptP95 < 20,
    `warm prompt p95 exceeded 20ms: ${promptP95.toFixed(3)}ms`,
  );
  agent.close();

  const capacitySamples: number[] = [];
  for (let sample = 0; sample < 11; sample += 1) {
    const agents = Array.from({ length: 25 }, () =>
      KissAgent.create({
        model: { id: "capacity", provider: "fixture", api: "host" },
      }, provider));
    const started = performance.now();
    const results = await Promise.all(
      agents.map((entry, index) => entry.prompt(`agent-${index}`)),
    );
    capacitySamples.push(performance.now() - started);
    assert(
      results.every((result) => result.text === "ok"),
      "all isolated agents must complete",
    );
    assert(
      agents.every((entry) => entry.state().messageCount === 2),
      "agent histories must remain isolated",
    );
    for (const entry of agents) entry.close();
  }
  const capacityP95 = percentile(capacitySamples, 0.95);
  assert(
    capacityP95 < 1_000,
    `25-agent fixture exceeded 1s p95: ${capacityP95.toFixed(3)}ms`,
  );

  report("wasm_module_init", [initMs], 1, "cached_deno_module");
  report("wasm_warm_prompt", promptSamples, 1, "host_model_full_agent_turn");
  report(
    "wasm_parallel_agents_25",
    capacitySamples,
    1,
    "25_isolated_full_agent_turns",
  );
});
