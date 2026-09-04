// ESM facade for Deno and ESM Node/Bun applications. The native implementation
// remains CommonJS because `require()` is the most consistently implemented
// N-API loading path across all three runtimes.
import { createRequire } from "node:module";
import type * as Api from "./index.js";

const require = createRequire(import.meta.url);
const sdk = require("./index.js") as typeof Api;

export const Session = sdk.Session;
export const MockProvider = sdk.MockProvider;

export type {
  AgentSettledEvent,
  AssistantDelta,
  BaseEvent,
  BashResult,
  Command,
  EventLagEvent,
  ImageInput,
  KissEvent,
  MessageUpdateEvent,
  Model,
  QueueMode,
  Response,
  SessionOptions,
  SessionState,
  StreamingBehavior,
  ThinkingLevel,
  ToolEvent,
  ToolName,
} from "./index.js";
