export type ThinkingLevel = "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";
export type StopReason = "pending" | "stop" | "length" | "toolUse" | "error" | "aborted";

export interface Model {
  id: string;
  name?: string;
  api?: string;
  provider?: string;
  baseUrl?: string;
  reasoning?: boolean;
  input?: string[];
  contextWindow?: number;
  maxTokens?: number;
  headers?: Record<string, string>;
}

export interface TextBlock { type: "text"; text: string; textSignature?: string }
export interface ThinkingBlock {
  type: "thinking";
  thinking: string;
  thinkingSignature?: string;
  redacted?: boolean;
}
export interface ImageBlock { type: "image"; data: string; mimeType: string }
export interface ToolCall {
  id: string;
  name: string;
  arguments: unknown;
  thoughtSignature?: string;
}
export interface ToolCallBlock extends ToolCall { type: "toolCall" }
export type ContentBlock = TextBlock | ThinkingBlock | ImageBlock | ToolCallBlock;
export type PromptInput = string | { content: string | ContentBlock[] };

export interface Usage {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  reasoning?: number;
  totalTokens: number;
  cost: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
    total: number;
  };
}

export interface ToolDefinition {
  name: string;
  label?: string;
  description: string;
  parameters: Record<string, unknown>;
  executionMode?: "sequential" | "parallel";
}

export type ToolResultInput = string | {
  content?: string | ContentBlock[];
  details?: unknown;
  terminate?: boolean;
};

export interface ToolExecutionContext {
  toolCallId: string;
  signal: AbortSignal;
  onUpdate(partial: ToolResultInput): void;
}

export type ToolExecutor = (
  args: unknown,
  context: ToolExecutionContext,
) => ToolResultInput | Promise<ToolResultInput>;

export interface ModelContext {
  systemPrompt?: string;
  messages: Array<Record<string, unknown>>;
  tools: ToolDefinition[];
}

export interface ModelRequest {
  model: Model;
  context: ModelContext;
  reasoning: ThinkingLevel;
  temperature?: number;
  maxTokens?: number;
}

export type ModelStreamEvent =
  | { type: "text_start"; contentIndex: number }
  | { type: "text_delta"; contentIndex: number; delta: string }
  | { type: "text_end"; contentIndex: number; content: string }
  | { type: "thinking_start"; contentIndex: number }
  | { type: "thinking_delta"; contentIndex: number; delta: string }
  | { type: "thinking_end"; contentIndex: number; content: string }
  | { type: "toolcall_start"; contentIndex: number; toolCall: ToolCall }
  | { type: "toolcall_delta"; contentIndex: number; delta: string }
  | { type: "toolcall_end"; contentIndex: number; toolCall: ToolCall };

export interface ModelResponse {
  content?: ContentBlock[];
  usage?: Usage;
  stopReason?: StopReason;
  responseModel?: string;
  responseId?: string;
  rawStopReason?: string;
  errorMessage?: string;
}

export type ModelProvider = (
  request: ModelRequest,
  emit: (event: ModelStreamEvent) => void,
  signal: AbortSignal,
) => ModelResponse | Promise<ModelResponse>;

export interface OpenAICompatibleProviderOptions {
  url: string | URL;
  apiKey?: string;
  headers?: Record<string, string>;
  fetch?: typeof globalThis.fetch;
  maxTokensField?: "max_tokens" | "max_completion_tokens";
  reasoningField?: string | false;
}

export function createOpenAICompatibleProvider(
  options: OpenAICompatibleProviderOptions,
): ModelProvider;

export interface AgentOptions {
  model: Model;
  systemPrompt?: string;
  thinkingLevel?: ThinkingLevel;
  temperature?: number;
  maxTokens?: number;
  maxTurns?: number;
  maxHistoryMessages?: number;
  checkpoint?: Uint8Array | number[];
}

export interface AgentState {
  model: Model;
  thinkingLevel: ThinkingLevel;
  isStreaming: boolean;
  closed: boolean;
  messageCount: number;
  steeringCount: number;
  followUpCount: number;
  tools: string[];
}

export interface PromptResult {
  text: string;
  stopReason: StopReason;
  messages: Array<Record<string, unknown>>;
  usage: Usage;
  state: AgentState;
}

export type AssistantMessageEvent =
  | { type: "start" }
  | ModelStreamEvent
  | { type: "done"; reason: StopReason; message: Record<string, unknown> }
  | { type: "error"; reason: StopReason; error: Record<string, unknown> };

export type AgentEvent =
  | { type: "agent_start" }
  | { type: "agent_end"; messages: Array<Record<string, unknown>> }
  | { type: "agent_settled" }
  | { type: "turn_start" }
  | { type: "turn_end"; message: Record<string, unknown>; toolResults: Array<Record<string, unknown>> }
  | { type: "message_start"; message: Record<string, unknown> }
  | { type: "message_update"; assistantMessageEvent: AssistantMessageEvent }
  | { type: "message_end"; message: Record<string, unknown> }
  | { type: "tool_execution_start"; toolCallId: string; toolName: string; args: unknown }
  | { type: "tool_execution_update"; toolCallId: string; toolName: string; args: unknown; partialResult: ToolResultInput }
  | { type: "tool_execution_end"; toolCallId: string; toolName: string; result: ToolResultInput; isError: boolean };

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;
export interface InitOutput { readonly memory: WebAssembly.Memory }

export class KissAgent {
  private constructor();
  static create(options: AgentOptions, modelProvider: ModelProvider): KissAgent;
  registerTool(definition: ToolDefinition, execute: ToolExecutor): void;
  prompt(input: PromptInput, onEvent?: (event: AgentEvent) => void): Promise<PromptResult>;
  steer(input: PromptInput): void;
  followUp(input: PromptInput): void;
  abort(): void;
  setModel(model: Model): void;
  setThinkingLevel(level: ThinkingLevel): void;
  messages(): Array<Record<string, unknown>>;
  clearHistory(): void;
  state(): AgentState;
  checkpoint(): Uint8Array;
  close(): void;
  free(): void;
}

export function initSync(module: { module: BufferSource | WebAssembly.Module } | BufferSource | WebAssembly.Module): InitOutput;
export default function init(moduleOrPath?: InitInput | Promise<InitInput>): Promise<InitOutput>;
