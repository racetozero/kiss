import path from "node:path";

// The addon is deliberately loaded once. Node, Bun, and Deno's Node-compat
// layer all implement N-API and `require` native addons.
// eslint-disable-next-line @typescript-eslint/no-require-imports
const binding = require(path.join(__dirname, "..", "kiss.node")) as NativeBinding;

export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
export type StreamingBehavior = "steer" | "followUp";
export type QueueMode = "all" | "one-at-a-time";
export type ToolName = "read" | "write" | "edit" | "bash" | "grep" | "find" | "ls" | "mcp";

export interface ImageInput { type: "image"; data: string; mimeType: string }
export interface Model {
  id: string; name: string; api: string; provider: string; baseUrl: string;
  reasoning: boolean; input: string[]; contextWindow: number; maxTokens: number;
  cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
}
export interface SessionOptions {
  cwd?: string; model?: string; provider?: string; apiKey?: string;
  modelsFile?: string; thinkingLevel?: ThinkingLevel; tools?: (ToolName | string)[];
  excludeTools?: string[]; noTools?: boolean; systemPrompt?: string;
  appendSystemPrompt?: string; session?: "in-memory" | "create" | "continue" | `open:${string}` | `fork:${string}`;
  sessionDir?: string; sessionName?: string; trustProjectFiles?: boolean;
  noContextFiles?: boolean; eventCapacity?: number;
}
export interface SessionState {
  model: Model | null; thinkingLevel: ThinkingLevel; isStreaming: boolean;
  sessionFile: string | null; sessionId: string; sessionName: string | null;
  messageCount: number; tools: string[]; steeringMode: QueueMode;
  followUpMode: QueueMode; autoCompactionEnabled: boolean; autoRetryEnabled: boolean;
}
export interface BashResult {
  output: string; exitCode: number | null; cancelled: boolean; truncated: boolean;
  fullOutputPath: string | null;
}
export interface AssistantDelta {
  type: "start" | "text_start" | "text_delta" | "text_end" |
    "thinking_start" | "thinking_delta" | "thinking_end" |
    "toolcall_start" | "toolcall_delta" | "toolcall_end" | "done" | "error";
  contentIndex?: number; delta?: string; content?: string; id?: string;
  toolName?: string; toolCall?: Record<string, unknown>;
}
export interface BaseEvent { type: string }
export interface MessageUpdateEvent extends BaseEvent {
  type: "message_update"; assistantMessageEvent: AssistantDelta;
}
export interface ToolEvent extends BaseEvent {
  type: "tool_execution_start" | "tool_execution_update" | "tool_execution_end";
  toolCallId: string; toolName: string; args?: Record<string, unknown>;
  isError?: boolean; result?: Record<string, unknown>; partialResult?: Record<string, unknown>;
}
export interface AgentSettledEvent extends BaseEvent { type: "agent_settled" }
export interface EventLagEvent extends BaseEvent { type: "event_lag"; skipped: number }
export type KissEvent = MessageUpdateEvent | ToolEvent | AgentSettledEvent | EventLagEvent | BaseEvent;

export interface Command { type: string; [field: string]: unknown }
export interface Response<T = unknown> {
  type: "response"; id?: string; command: string; success: boolean; data?: T; error?: string;
}

interface NativeEventStream { nextJson(): Promise<string | null> }
interface NativeSession {
  executeJson(json: string): Promise<string>;
  prompt(message: string): Promise<void>;
  promptDetached(message: string): void;
  events(): NativeEventStream;
  abort(): void;
  waitIdle(): Promise<void>;
  close(): void;
}
interface NativeSessionConstructor { create(optionsJson: string): Promise<NativeSession> }
interface NativeMockProvider {
  readonly catalogPath: string;
  requestsJson(): string;
  stop(): void;
}
interface NativeMockProviderConstructor {
  start(directory: string, scriptJson: string): Promise<NativeMockProvider>;
}
interface NativeBinding {
  NativeSession: NativeSessionConstructor;
  MockProvider?: NativeMockProviderConstructor;
}

class EventIterator implements AsyncIterableIterator<KissEvent> {
  readonly #native: NativeEventStream;
  constructor(native: NativeEventStream) { this.#native = native; }
  [Symbol.asyncIterator](): AsyncIterableIterator<KissEvent> { return this; }
  async next(): Promise<IteratorResult<KissEvent>> {
    const json = await this.#native.nextJson();
    if (json === null) return { done: true, value: undefined };
    return { done: false, value: JSON.parse(json) as KissEvent };
  }
}

/** One embeddable conversation with the KISS coding agent. */
export class Session {
  readonly #native: NativeSession;
  private constructor(native: NativeSession) { this.#native = native; }

  static async create(options: SessionOptions = {}): Promise<Session> {
    return new Session(await binding.NativeSession.create(JSON.stringify(options)));
  }

  /** Escape hatch: every typed method below calls this shared dispatcher. */
  async execute<T = unknown>(command: Command): Promise<Response<T>> {
    return JSON.parse(await this.#native.executeJson(JSON.stringify(command))) as Response<T>;
  }
  async #require<T>(command: Command): Promise<T> {
    const response = await this.execute<T>(command);
    if (!response.success) throw new Error(response.error ?? `${command.type} failed`);
    return response.data as T;
  }

  events(): AsyncIterableIterator<KissEvent> { return new EventIterator(this.#native.events()); }
  async prompt(message: string): Promise<void> { await this.#native.prompt(message); }
  promptDetached(message: string): void { this.#native.promptDetached(message); }
  abort(): void { this.#native.abort(); }
  async waitIdle(): Promise<void> { await this.#native.waitIdle(); }
  close(): void { this.#native.close(); }
  async dispose(): Promise<void> { this.close(); }
  async [Symbol.asyncDispose](): Promise<void> { this.close(); }

  async steer(message: string): Promise<void> { await this.#require({ type: "steer", message }); }
  async followUp(message: string): Promise<void> { await this.#require({ type: "follow_up", message }); }
  async state(): Promise<SessionState> { return this.#require({ type: "get_state" }); }
  async messages(): Promise<Record<string, unknown>[]> {
    return (await this.#require<{ messages: Record<string, unknown>[] }>({ type: "get_messages" })).messages;
  }
  async entries(since?: string): Promise<{ entries: Record<string, unknown>[]; leafId: string | null }> {
    return this.#require({ type: "get_entries", ...(since === undefined ? {} : { since }) });
  }
  async lastAssistantText(): Promise<string | null> {
    return (await this.#require<{ text: string | null }>({ type: "get_last_assistant_text" })).text;
  }
  async sessionStats(): Promise<Record<string, unknown>> { return this.#require({ type: "get_session_stats" }); }
  async tools(): Promise<string[]> {
    const data = await this.#require<{ tools: { name: string }[] }>({ type: "get_tools" });
    return data.tools.map((tool) => tool.name);
  }
  async setModel(provider: string, modelId: string): Promise<Model> {
    return this.#require({ type: "set_model", provider, modelId });
  }
  async availableModels(search?: string): Promise<Model[]> {
    const command: Command = { type: "get_available_models" };
    if (search !== undefined) command.search = search;
    return (await this.#require<{ models: Model[] }>(command)).models;
  }
  async setThinkingLevel(level: ThinkingLevel): Promise<void> {
    await this.#require({ type: "set_thinking_level", level });
  }
  async availableThinkingLevels(): Promise<ThinkingLevel[]> {
    return (await this.#require<{ levels: ThinkingLevel[] }>({ type: "get_available_thinking_levels" })).levels;
  }
  async setSteeringMode(mode: QueueMode): Promise<void> {
    await this.#require({ type: "set_steering_mode", mode });
  }
  async setFollowUpMode(mode: QueueMode): Promise<void> {
    await this.#require({ type: "set_follow_up_mode", mode });
  }
  async compact(customInstructions?: string): Promise<Record<string, number>> {
    const command: Command = { type: "compact" };
    if (customInstructions !== undefined) command.customInstructions = customInstructions;
    return this.#require(command);
  }
  async setAutoCompaction(enabled: boolean): Promise<void> {
    await this.#require({ type: "set_auto_compaction", enabled });
  }
  async setAutoRetry(enabled: boolean): Promise<void> {
    await this.#require({ type: "set_auto_retry", enabled });
  }
  async bash(command: string): Promise<BashResult> { return this.#require({ type: "bash", command }); }
  async abortBash(): Promise<void> { await this.#require({ type: "abort_bash" }); }
  async ping(): Promise<boolean> { return (await this.#require<{ pong: boolean }>({ type: "ping" })).pong; }
}

/** Test-only scripted provider; present in development builds and omitted from slim wheels. */
export class MockProvider {
  readonly #native: NativeMockProvider;
  private constructor(native: NativeMockProvider) { this.#native = native; }
  static async start(directory: string, script: unknown[][]): Promise<MockProvider> {
    if (binding.MockProvider === undefined) throw new Error("this package was built without the mock feature");
    return new MockProvider(await binding.MockProvider.start(directory, JSON.stringify(script)));
  }
  get catalogPath(): string { return this.#native.catalogPath; }
  requests(): Record<string, unknown>[] { return JSON.parse(this.#native.requestsJson()) as Record<string, unknown>[]; }
  stop(): void { this.#native.stop(); }
}
