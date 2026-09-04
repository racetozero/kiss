# FX WebAssembly review and KISS recommendations

Status: research note for future implementation work  
Reviewed: 2026-09-04  
FX revision: [`964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4`](https://github.com/vercel-labs/fx/tree/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4)  
KISS revision: `b9907eee881d67933d6771a05ca802addf7265ff`

## Executive summary

FX and KISS currently mean different things by “browser WASM”:

- **FX runs a restricted agent kernel inside WebAssembly.** JavaScript owns network access, storage, credentials, custom tools, and optional workspace capabilities.
- **KISS runs only its RPC client inside WebAssembly.** The real agent and all filesystem/process authority remain in a native `kiss` process reached over WebSocket.

This is not an apples-to-apples performance comparison. KISS has the smaller and more compatible browser component, keeps credentials and OS authority outside the page, and can expose the complete native coding agent. FX avoids a WebSocket hop and can operate as an in-page agent, but requires JSPI, a substantially more involved host ABI, and browser-visible credentials.

The main recommendations are:

1. **Keep the current remote-agent architecture**, but explicitly call it the browser RPC client rather than implying that the agent runs in WASM.
2. **Secure the WebSocket handshake before expanding browser use.** Loopback binding alone does not protect a local agent from a malicious web origin.
3. **Bound every queue, frame, command, and concurrent operation.** The current RPC writer channels and command spawning are unbounded.
4. **Fix WASM closure/socket lifecycle ownership.** The current `.forget()` handlers are never reclaimed deterministically.
5. **Adopt FX’s benchmark contracts:** cold/warm latency, stream shapes, bridge latency, multi-client capacity, cancellation recovery, and post-close reclamation.
6. **Consider a second, optional `kiss-core-wasm` surface** only if in-browser agent execution is a product requirement. It should be a restricted capability-based kernel, not a replacement for remote native KISS.
7. **Benchmark a direct TypeScript WebSocket client.** For KISS’s current thin-client workload, native `JSON.parse` plus WebSocket may be faster and much faster to load than passing every frame through Rust WASM.

## Architectures compared

### KISS today

```text
Browser
  TypeScript/JavaScript
       |
  wasm-bindgen KissClient
  - command validation
  - request IDs / pending promises
  - JSON encode/decode
       |
  browser WebSocket
       |
Native kiss RPC server
  - full Session
  - model providers
  - filesystem and process tools
```

The design in [`crates/kiss-wasm/src/lib.rs`](../crates/kiss-wasm/src/lib.rs) is honest about browser sandbox limits, but its statement that an agent cannot run in a browser is now too broad. Native filesystem and process tools cannot run directly, but FX demonstrates that a **restricted agent kernel with host-provided capabilities** can.

### FX today

```text
Browser or JS runtime
  dependency-free JS host SDK
  - fetch and AbortController
  - credentials/config/storage
  - custom tools and optional workspace
  - module cache and lifecycle
       |
  typed pointer/capacity/status host ABI + JSPI
       |
  fx-core.wasm
  - actual headless agent kernel
  - ACP JSON-RPC server
  - conversation/history/checkpoints
  - no implicit native filesystem/process authority
```

FX also builds a separate `fx-term.wasm`. Node selects a native N-API backend when available and uses the same high-level JavaScript wrapper, with WASM as fallback. Relevant references:

- [`sdk/README.md`](https://github.com/vercel-labs/fx/blob/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4/sdk/README.md)
- [`sdk/fx-sdk.js`](https://github.com/vercel-labs/fx/blob/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4/sdk/fx-sdk.js)
- [`src/wasm_core_main.zig`](https://github.com/vercel-labs/fx/blob/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4/src/wasm_core_main.zig)
- [`sdk/NAPI.md`](https://github.com/vercel-labs/fx/blob/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4/sdk/NAPI.md)

## What FX does particularly well

### 1. It treats the host boundary as a capability and security boundary

The browser kernel has no accidental ambient OS authority. Fetch, tools, persistence, OAuth/config stores, prompt history, and optional workspace execution are explicit host imports. Inputs and outputs have concrete limits. Workspace commands have a maximum duration and output size, cancellation, path policy, and explicit permission checks.

This is the right model for any future local KISS WASM kernel:

- deny capabilities by default;
- pass capabilities explicitly at agent creation;
- validate every pointer, length, enum, URL, and schema at the boundary;
- make the host authoritative for side effects;
- propagate cancellation into every host operation;
- keep browser credentials short-lived.

### 2. Its high-level API hides backend differences

FX’s JavaScript caller gets one `Agent` shape whether the backend is N-API or WASM. A prompt returns an async iterable of normalized events and a separate final result. JavaScript tools receive an `AbortSignal`. Only one prompt can run at a time.

KISS has strong protocol consistency across Rust, Python, TypeScript, and RPC, but browser WASM currently exposes a lower-level callback API and a generic `execute(JsValue)`. A shared high-level API would reduce application-specific state machines.

Recommended common shape:

```text
session.prompt(input, { signal, streamingBehavior })
  -> PromptTurn { events: AsyncIterable<Event>, result: Promise<PromptResult> }

session.execute(command, { signal }) -> Promise<Response>
session.state() -> Promise<SessionState>
session.close() -> Promise<void>
```

Rust and Python should use their idiomatic stream types, while preserving the same lifecycle and event semantics.

### 3. Compilation, instance isolation, and cleanup are tested as contracts

FX caches a compiled `WebAssembly.Module` by stable source, removes failed cache entries so retries work, and creates a separate instance per in-WASM agent. Its tests verify:

- concurrent and sequential creation compile the same source once;
- different sources compile separately;
- failed reads/compiles can be retried;
- agent state is isolated;
- all fetch signals are aborted on close;
- closed instances become garbage-collectable.

See [`sdk/tests/test-wasm-module-cache.mjs`](https://github.com/vercel-labs/fx/blob/964c040491dcb40a4c6cc63ffdb0b89e9e85c9f4/sdk/tests/test-wasm-module-cache.mjs).

KISS’s generated wasm-bindgen loader already uses one module/instance singleton. That is appropriate for a small transport client whose per-client state is in Rust objects; KISS should **not** create one WASM instance per WebSocket client. It should nevertheless add compile-once, retry, multi-client isolation, and post-close collection tests.

### 4. Resource limits are explicit and pervasive

FX uses fixed caps for instructions, URLs, model names/catalogs, command text, output, API keys, and workspace metadata. Its N-API transport also has bounded input/output/fetch queues and runtime-count limits.

KISS has a bounded session broadcast channel, but the RPC layer converts it into an unbounded output channel and allows unbounded command tasks. Incoming stdio lines and WebSocket messages are not capped by KISS itself. Resource limits need to be end-to-end; one unbounded hop defeats an earlier bounded hop.

### 5. Performance is continuously measured, including cleanup

FX benchmarks more than throughput:

- first prompt versus warm prompts;
- prompt-to-fetch and first-body-to-first-text;
- one tiny chunk, 1,000 tiny chunks, and large chunks;
- tool event to JS callback and callback to follow-up inference;
- 25 simultaneous agents;
- cancellation and recovery;
- descriptors, handles, threads, RSS, JS heap, and WASM external memory before and after close;
- native versus WASM and native versus Pi.

The deterministic checks run in CI rather than leaving benchmark output as an informal report.

## Performance evidence and interpretation

### KISS artifact measurements

Measured from the existing release artifact in `crates/kiss-wasm/pkg` at the KISS revision above:

| Item | Measurement |
|---|---:|
| `kiss_wasm_bg.wasm` | 260,818 bytes |
| gzip -9 WASM | 79,504 bytes |
| generated JS loader | 29,372 bytes |
| initial linear memory | 17 pages / 1,114,112 bytes |

These are measurements of a **thin RPC client**, not an agent kernel. They should become CI budgets so dependency or binding changes cannot silently regress startup cost.

The standalone WASM profile already uses `opt-level = "s"`, LTO, and one codegen unit. It should also explicitly evaluate `opt-level = "z"`, `strip`, `panic = "abort"`, and `wasm-opt -Oz`; retain only changes that win on measured compressed size and runtime.

### FX benchmark evidence

Raw output was downloaded from the successful FX benchmark run for the reviewed commit: [GitHub Actions run 33860379171](https://github.com/vercel-labs/fx/actions/runs/33860379171). The runner used Node 24.20.0 and synthetic local inference, so these numbers isolate orchestration better than a live-model benchmark.

| Node metric | FX native | FX WASM |
|---|---:|---:|
| First prompt to first text | 22.243 ms | 80.960 ms |
| Warm prompt to first text p50 | 2.300 ms | 2.383 ms |
| Warm first body to first text p50 | 0.078 ms | 0.214 ms |
| Tool round trip p50 | 2.362 ms | 2.334 ms |

Interpretation:

- WASM has a meaningful cold-start/first-turn cost.
- After warmup, the orchestration difference is small in this fixture.
- Crossing JS/WASM for each streamed body segment is visible in first-body-to-text latency.
- Bridge costs are noisy enough that a single p50 should not drive architectural decisions.
- Real model/network latency will usually dominate, but token-heavy local fixtures and UI rendering can expose per-event overhead.

For 25 Node WASM agents, FX external memory rose from about 6.2 MB to 50.5 MB after creation, then returned to about 6.2 MB after close. This is strong evidence for lifecycle cleanup even though process RSS remained elevated. FX also has a source-level CI assertion that `fx-core.wasm` defines one memory with at most 32 initial pages.

No equivalent KISS latency/capacity suite exists yet, so a numeric “KISS is faster/slower than FX” claim would be misleading. KISS additionally pays WebSocket scheduling/network costs that FX’s in-process WASM backend does not, while doing dramatically less work in WASM.

### KISS’s likely hot-path costs

An inbound KISS frame currently takes this path:

```text
WebSocket string
 -> serde_json parse in WASM
 -> serde_json::Value / Rust Response
 -> serde-wasm-bindgen conversion
 -> JavaScript object
 -> callback
```

An outbound generic command takes the reverse typed conversion and then `serde_json` serialization. This preserves shared Rust protocol validation, but it adds allocations and boundary conversions around data the browser already represents naturally.

For a thin JSON WebSocket client, benchmark these implementations:

1. current Rust WASM codec/client;
2. direct TypeScript with `JSON.stringify`/`JSON.parse`;
3. JavaScript transport and correlation with WASM used only for optional validation;
4. current client with event-delta coalescing.

A direct TypeScript client is expected to win cold start and may win frame throughput. WASM should not be assumed to be faster merely because it is WASM. Its current value is shared protocol behavior and Rust reuse.

## KISS findings, prioritized

### P0: authenticate and authorize browser RPC at the handshake

[`serve_websocket`](../crates/kiss-sdk/src/rpc.rs) accepts every connection and does not inspect `Origin`. [`docs/rpc.md`](rpc.md) accurately says there is no authentication, but “bind to loopback” is insufficient for browser use: a malicious page can attempt a WebSocket connection to a local service, and the browser supplies an `Origin` that the current server ignores. A successful connection can prompt, abort, mutate files, or run enabled tools with the native KISS process’s authority.

Before presenting browser RPC as production-ready:

- generate a high-entropy, short-lived capability token when the listener starts;
- authenticate during the WebSocket upgrade, preferably through an allowed `Sec-WebSocket-Protocol` value or another handshake-time mechanism supported by the browser API;
- validate `Origin` against an explicit allowlist, with a separate intentional mode for non-browser clients;
- reject unauthenticated upgrades before creating event subscriptions;
- never print credentials in URLs or normal logs;
- bind loopback by default and require an explicit unsafe/remote option for non-loopback addresses;
- scope tokens to session and permissions when practical.

Authentication and Origin checks are complementary, not substitutes.

### P0: bound input, output, and concurrency

Current risks in [`crates/kiss-sdk/src/rpc.rs`](../crates/kiss-sdk/src/rpc.rs):

- stdio `read_line` can grow without a KISS limit;
- WebSocket text/binary frames have no explicit KISS limit;
- writer channels are `mpsc::unbounded_channel`;
- each accepted command creates a new task without a limit;
- pending responses and events can accumulate behind a slow client;
- the browser client’s pending promise map is unbounded.

Recommended controls:

- protocol-wide maximum frame size plus lower command-specific limits;
- bounded writer queues measured in both records and bytes;
- explicit slow-consumer behavior: coalesce text deltas, emit `event_lag`, or disconnect;
- bounded normal command concurrency with a reserved control path for `abort`, `state`, and `close`;
- maximum outstanding requests per client;
- checked request-ID rollover;
- per-command deadlines where sensible;
- tests that flood commands/events while the reader or writer is stalled.

Do not serialize `abort` behind a long prompt merely to obtain boundedness.

### P1: deterministic browser lifecycle

The WebSocket handlers in [`crates/kiss-wasm/src/lib.rs`](../crates/kiss-wasm/src/lib.rs) are installed and then `.forget()` is called. The message and close closures capture shared state, and `close()` only asks the socket to close. KISS does not unset all socket callbacks or deterministically release the closures.

Change the client to own its callback closures and implement idempotent teardown that:

1. marks the connection closed;
2. unsets `onopen`, `onerror`, `onmessage`, and `onclose`;
3. aborts/closes the socket once;
4. rejects and clears pending requests once;
5. clears user callbacks/queued events;
6. drops owned closures;
7. behaves the same from explicit close, socket close, connection failure, and finalization.

Add a repeated create/connect/close test using `WeakRef`, `FinalizationRegistry`, and forced GC where the runtime supports it.

### P1: add protocol negotiation and connection semantics

The WebSocket currently starts accepting commands immediately. Add a versioned hello/initialize exchange containing:

- protocol version and server version;
- supported commands and optional features;
- negotiated frame/queue limits;
- session identity;
- ownership mode and permissions;
- event sequencing/replay capability;
- optional binary encoding capability, if ever added.

All WebSocket clients currently share one session. That can be useful for a local UI, but it also means one client can abort or reset another client’s work. Choose and document one of:

- one isolated session per connection;
- an exclusive controller lease plus read-only observers;
- explicit attach to a named session with scoped capabilities.

Do not leave cross-client control as an accidental consequence of a shared `Arc<Session>`.

### P1: make cancellation and streaming idiomatic

FX’s `AbortSignal` integration and async iterable are better browser ergonomics than KISS’s global `abort()` plus push callback. Add:

- per-turn cancellation connected to the server request/turn identity;
- immediate local rejection/removal when a signal is already aborted;
- cancellation propagation through queued work and host/network effects;
- an async iterable with a bounded queue;
- a distinct final result promise;
- recovery tests proving the next prompt works after cancellation.

Keep a callback adapter for simple UIs, but build it on the same bounded stream.

### P1: avoid repeated event serialization where possible

KISS creates event JSON values, clones them through broadcast subscribers, serializes each subscriber’s event, reparses it in WASM, and converts it back to a JS object. Potential improvements, in order:

1. benchmark before changing the wire format;
2. serialize a wire event once and broadcast/share it as `Arc<str>` or bytes in the RPC path;
3. batch/coalesce adjacent text deltas within a very small latency budget;
4. use native `JSON.parse` in the browser hot path and inspect only `type`/`id` for routing;
5. retain JSON as the compatibility baseline.

A binary encoding is lower priority. JSON’s interoperability is central to RPC mode, and model latency usually dominates. If binary frames are added, negotiate them and retain JSON conformance tests.

### P2: durable state, reconnect, and observability

FX’s bounded, versioned, opaque checkpoints are a useful model. KISS persistence is richer on the native side, but browser reconnect behavior is underspecified. Consider:

- session attach/resume tokens separate from authentication tokens;
- monotonically increasing event sequence numbers;
- bounded replay from a sequence or an explicit “snapshot required” response;
- an authoritative state snapshot after gaps;
- protocol timestamps or optional diagnostics for prompt accepted, model request, first delta, tool start/end, settled, and cancellation;
- credentials and raw sensitive headers excluded from diagnostics.

Sequence IDs are more important than timestamps for correctness. Avoid expensive per-token wall-clock instrumentation by making detailed diagnostics optional or sampling them.

### P2: artifact and memory budgets

Add CI gates for:

- raw and gzip/brotli WASM size;
- generated JS size;
- initial and maximum declared memory;
- cold init and first connection;
- compile-once behavior;
- N simultaneous clients;
- post-close pending map, socket, callback, and WASM object reclamation.

The current 17-page initial memory and approximately 80 KB gzip artifact are sensible baselines, not permanent target values.

## Recommended target architecture

### Surface A: remote browser client — recommended now

Preserve the existing native-agent topology and harden it:

```text
@kiss-sdk/browser
  high-level typed TS API
  bounded AsyncIterable events
  AbortSignal cancellation
  authenticated WebSocket transport
  hello/version/capability negotiation
       |
optional codec backend
  direct JS fast path (default if benchmarks win)
  Rust WASM codec/client (shared-protocol/conformance path)
       |
kiss RPC gateway
  handshake authentication + Origin policy
  bounded framing/queues/concurrency
  explicit controller/session ownership
       |
full native Session
```

This mode remains the only browser mode with full KISS filesystem/process tools. It also keeps provider secrets on the native side.

### Surface B: local restricted agent kernel — optional research track

If “run in browser” must mean the agent itself runs locally, create a separate package/artifact such as `kiss-core-wasm`; do not overload the remote client artifact.

Required properties:

- one restricted agent conversation per instance;
- no ambient filesystem, process, environment, or credential access;
- host-owned fetch with cancellation and URL policy;
- host-registered tools with JSON Schema and strict input/output limits;
- optional sandbox workspace behind a typed permission boundary;
- host-owned persistence using bounded, versioned checkpoints;
- compile module once, instantiate isolated memory per agent;
- short-lived browser credentials only;
- capability detection rather than user-agent detection;
- independent tests for browser and worker contexts.

Unlike FX, KISS controls its Rust SDK and wasm-bindgen surface, so it should evaluate a direct typed async ABI before adopting an ACP-over-stdio/JSONL bridge inside the module. JSONL is excellent for external RPC compatibility, but an internal JSON loop can add avoidable parsing and copies.

JSPI is a credible route for suspending synchronous WASI code around async host functions, but it narrows browser support and is still experimental in some runtimes. A KISS design should compare:

- JSPI plus WASI;
- native Rust async compiled with `wasm-bindgen-futures`;
- a worker-hosted kernel with explicit message passing.

Choose based on supported browsers, bundle size, cancellation behavior, and measured bridge latency—not implementation novelty.

## Practices to adopt without copying FX wholesale

Adopt:

- capability-based host authority;
- one high-level API across native and WASM backends;
- async iterable turn events plus a final result;
- `AbortSignal` propagation;
- explicit limits at every boundary;
- compile caching with failure eviction;
- independent per-agent memory only when the agent runs in WASM;
- lifecycle and reclamation tests;
- cold/warm/stream/bridge/capacity CI benchmarks;
- separate headless-core and terminal artifacts.

Do not copy blindly:

- **JSPI as a requirement for the existing RPC client.** KISS currently works with ordinary WebSocket APIs across more browsers.
- **A full WASI kernel when only remote control is needed.** It adds download, compile, memory, and maintenance cost.
- **Internal JSONL just because the external protocol uses JSONL.** Prefer a typed direct ABI for an in-process module if it benchmarks better.
- **Browser-held long-lived provider keys.** Remote KISS’s server-side secret ownership is safer.
- **Optimization flags that remove safety without evidence.** FX disables several Zig runtime protections for its small WASM build. Rust release tuning should preserve boundary validation and be justified by measurements.
- **Polling-based async bridges without latency tests.** FX uses short timeout races around some host stream operations; KISS should prefer event-driven wakeups where its runtime design permits them.

## Proposed benchmark matrix for KISS

Use a fake local inference provider so network/model variance does not hide SDK overhead.

| Area | Cases |
|---|---|
| Load | fetch/compile/instantiate, cached init, first client |
| Prompt | first prompt, 100 warm prompts |
| Stream | 1×1 B, 1,000×1 B, 16×64 KiB, realistic token deltas |
| Bridge | server event→callback, event→iterator consumer, abort→server observation |
| RPC | direct Rust, stdio, loopback WebSocket, browser WASM, direct TypeScript |
| Capacity | 1, 25, and 100 idle clients; 25 active clients |
| Backpressure | stalled browser, event flood, command flood, oversized frame |
| Lifecycle | close during connect/prompt/tool/event callback; repeated close; GC |
| Recovery | malformed frame, disconnect, cancellation, failed compile, failed reconnect |

Report p50/p95/p99/max, allocations where practical, bytes copied/serialized, queue high-water marks, RSS/heap/WASM memory, descriptors, and active handles. Establish budgets only after collecting stable baselines on pinned CI runners.

## Suggested implementation order

1. Handshake token authentication, Origin policy, and safe listener defaults.
2. Frame limits, bounded byte-aware queues, bounded command admission, and slow-client behavior.
3. Deterministic WASM/WebSocket teardown with lifecycle stress tests.
4. Versioned hello and explicit session/controller semantics.
5. AbortSignal plus bounded async iterable turn API.
6. KISS benchmark harness and artifact budgets.
7. Benchmark direct TypeScript routing versus the Rust WASM hot path; select the default from data.
8. Serialize/broadcast RPC events once and evaluate small delta coalescing.
9. Design `kiss-core-wasm` only after documenting browser-local use cases and acceptable browser support.

The first three items are security/reliability work and should precede feature expansion. The later performance changes should be accepted only with benchmark evidence.
