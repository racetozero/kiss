import init, { initSync, KissAgent } from "./pkg/kiss_core_wasm.js";

export { initSync, KissAgent };
export default init;

const MAX_ERROR_BYTES = 64 * 1024;
const MAX_STREAM_BYTES = 16 * 1024 * 1024;

/**
 * Create a browser-fetch model capability for OpenAI-compatible Chat
 * Completions endpoints. The returned callback plugs directly into
 * `KissAgent.create` and supports streaming text, reasoning, and tool calls.
 */
export function createOpenAICompatibleProvider(options) {
  if (!options || typeof options !== "object") {
    throw new TypeError("KISS_INVALID_PROVIDER: options must be an object");
  }
  const endpoint = new URL(options.url);
  if (endpoint.protocol !== "https:" && endpoint.protocol !== "http:") {
    throw new TypeError("KISS_INVALID_PROVIDER: url must use http or https");
  }
  if (endpoint.username || endpoint.password || endpoint.hash) {
    throw new TypeError("KISS_INVALID_PROVIDER: url must not contain credentials or a fragment");
  }
  const fetchImpl = options.fetch ?? globalThis.fetch;
  if (typeof fetchImpl !== "function") {
    throw new TypeError("KISS_INVALID_PROVIDER: fetch is unavailable");
  }
  const headers = normalizeHeaders(options.headers);
  if (options.apiKey) headers.set("authorization", `Bearer ${options.apiKey}`);
  headers.set("content-type", "application/json");
  headers.set("accept", "text/event-stream, application/json");

  return async function openAICompatibleProvider(request, emit, signal) {
    const body = buildChatRequest(request, options);
    const response = await fetchImpl(endpoint.href, {
      method: "POST",
      headers,
      body: JSON.stringify(body),
      signal,
    });
    if (!response.ok) {
      const text = await readBoundedText(response, MAX_ERROR_BYTES, signal);
      throw new Error(`model endpoint returned HTTP ${response.status}: ${text}`);
    }
    const contentType = response.headers.get("content-type") ?? "";
    if (!response.body || contentType.includes("application/json")) {
      const text = await readBoundedText(response, MAX_STREAM_BYTES, signal);
      return parseCompleteResponse(JSON.parse(text), request.model);
    }
    return parseChatStream(response.body, request.model, emit, signal);
  };
}

function normalizeHeaders(input) {
  const headers = new Headers();
  if (input === undefined) return headers;
  for (const [name, value] of Object.entries(input)) {
    if (typeof value !== "string") {
      throw new TypeError(`KISS_INVALID_PROVIDER: header ${name} must be a string`);
    }
    headers.set(name, value);
  }
  return headers;
}

function buildChatRequest(request, options) {
  const messages = [];
  if (request.context.systemPrompt) {
    messages.push({ role: "system", content: request.context.systemPrompt });
  }
  for (const message of request.context.messages) {
    if (message.role === "user") {
      messages.push({ role: "user", content: openAIUserContent(message.content) });
    } else if (message.role === "assistant") {
      const toolCalls = (message.content ?? [])
        .filter((block) => block.type === "toolCall")
        .map((block) => ({
          id: block.id,
          type: "function",
          function: { name: block.name, arguments: JSON.stringify(block.arguments ?? {}) },
        }));
      const text = (message.content ?? [])
        .filter((block) => block.type === "text")
        .map((block) => block.text)
        .join("");
      const output = { role: "assistant", content: text || null };
      if (toolCalls.length) output.tool_calls = toolCalls;
      messages.push(output);
    } else if (message.role === "toolResult") {
      messages.push({
        role: "tool",
        tool_call_id: message.toolCallId,
        content: textFromBlocks(message.content),
      });
    }
  }

  const body = {
    model: request.model.id,
    messages,
    stream: true,
    stream_options: { include_usage: true },
  };
  if (request.context.tools.length) {
    body.tools = request.context.tools.map((tool) => ({
      type: "function",
      function: {
        name: tool.name,
        description: tool.description,
        parameters: tool.parameters,
      },
    }));
  }
  if (request.temperature !== undefined) body.temperature = request.temperature;
  if (request.maxTokens !== undefined) {
    body[options.maxTokensField ?? "max_tokens"] = request.maxTokens;
  }
  if (request.reasoning && request.reasoning !== "off" && options.reasoningField !== false) {
    body[options.reasoningField ?? "reasoning_effort"] = request.reasoning;
  }
  return body;
}

function openAIUserContent(content) {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return String(content ?? "");
  return content.flatMap((block) => {
    if (block.type === "text") return [{ type: "text", text: block.text }];
    if (block.type === "image") {
      return [{
        type: "image_url",
        image_url: { url: `data:${block.mimeType};base64,${block.data}` },
      }];
    }
    return [];
  });
}

function textFromBlocks(content) {
  if (typeof content === "string") return content;
  return (content ?? [])
    .filter((block) => block.type === "text")
    .map((block) => block.text)
    .join("\n");
}

function parseCompleteResponse(payload, model) {
  const choice = payload?.choices?.[0];
  if (!choice?.message) throw new Error("model endpoint returned no assistant message");
  const message = choice.message;
  const content = [];
  if (message.reasoning_content) {
    content.push({ type: "thinking", thinking: message.reasoning_content, redacted: false });
  }
  if (message.content) content.push({ type: "text", text: message.content });
  for (const call of message.tool_calls ?? []) {
    content.push({
      type: "toolCall",
      id: call.id,
      name: call.function?.name ?? "",
      arguments: parseArguments(call.function?.arguments),
    });
  }
  return {
    content,
    usage: normalizeUsage(payload.usage),
    stopReason: mapStopReason(choice.finish_reason, content),
    responseModel: payload.model ?? model.id,
    responseId: payload.id,
    rawStopReason: choice.finish_reason,
  };
}

async function parseChatStream(body, model, emit, signal) {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let totalBytes = 0;
  let text = "";
  let thinking = "";
  let textIndex;
  let thinkingIndex;
  let nextContentIndex = 0;
  let usage;
  let responseId;
  let responseModel;
  let rawStopReason;
  const toolCalls = new Map();

  const consume = (payload) => {
    if (payload === "[DONE]") return;
    const chunk = JSON.parse(payload);
    responseId ??= chunk.id;
    responseModel ??= chunk.model;
    if (chunk.usage) usage = normalizeUsage(chunk.usage);
    const choice = chunk.choices?.[0];
    if (!choice) return;
    if (choice.finish_reason) rawStopReason = choice.finish_reason;
    const delta = choice.delta ?? {};
    const textDelta = typeof delta.content === "string" ? delta.content : "";
    if (textDelta) {
      if (textIndex === undefined) {
        textIndex = nextContentIndex++;
        emit({ type: "text_start", contentIndex: textIndex });
      }
      text += textDelta;
      emit({ type: "text_delta", contentIndex: textIndex, delta: textDelta });
    }
    const thinkingDelta = delta.reasoning_content ?? delta.reasoning;
    if (typeof thinkingDelta === "string" && thinkingDelta) {
      if (thinkingIndex === undefined) {
        thinkingIndex = nextContentIndex++;
        emit({ type: "thinking_start", contentIndex: thinkingIndex });
      }
      thinking += thinkingDelta;
      emit({ type: "thinking_delta", contentIndex: thinkingIndex, delta: thinkingDelta });
    }
    for (const partial of delta.tool_calls ?? []) {
      const index = partial.index ?? 0;
      let call = toolCalls.get(index);
      if (!call) {
        call = {
          id: partial.id ?? `tool-${index}`,
          name: "",
          arguments: "",
          started: false,
          contentIndex: nextContentIndex++,
        };
        toolCalls.set(index, call);
      }
      if (partial.id) call.id = partial.id;
      if (partial.function?.name) call.name += partial.function.name;
      if (!call.started && call.name) {
        call.started = true;
        emit({
          type: "toolcall_start",
          contentIndex: call.contentIndex,
          toolCall: { id: call.id, name: call.name, arguments: {} },
        });
      }
      if (partial.function?.arguments) {
        call.arguments += partial.function.arguments;
        emit({ type: "toolcall_delta", contentIndex: call.contentIndex, delta: partial.function.arguments });
      }
    }
  };

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (signal.aborted) throw new DOMException("aborted", "AbortError");
      totalBytes += value.byteLength;
      if (totalBytes > MAX_STREAM_BYTES) throw new Error("model stream exceeds 16777216 bytes");
      buffer += decoder.decode(value, { stream: true });
      buffer = drainSse(buffer, consume);
    }
    buffer += decoder.decode();
    drainSse(`${buffer}\n\n`, consume);
  } finally {
    reader.releaseLock();
  }

  const indexedContent = [];
  if (thinkingIndex !== undefined) {
    emit({ type: "thinking_end", contentIndex: thinkingIndex, content: thinking });
    indexedContent.push([thinkingIndex, { type: "thinking", thinking, redacted: false }]);
  }
  if (textIndex !== undefined) {
    emit({ type: "text_end", contentIndex: textIndex, content: text });
    indexedContent.push([textIndex, { type: "text", text }]);
  }
  for (const [index, call] of [...toolCalls.entries()].sort(([a], [b]) => a - b)) {
    const toolCall = {
      id: call.id,
      name: call.name,
      arguments: parseArguments(call.arguments),
    };
    emit({ type: "toolcall_end", contentIndex: call.contentIndex, toolCall });
    indexedContent.push([call.contentIndex, { type: "toolCall", ...toolCall }]);
  }
  indexedContent.sort(([left], [right]) => left - right);
  const content = indexedContent.map(([, block]) => block);
  return {
    content,
    usage: usage ?? normalizeUsage(),
    stopReason: mapStopReason(rawStopReason, content),
    responseModel: responseModel ?? model.id,
    responseId,
    rawStopReason,
  };
}

function drainSse(buffer, consume) {
  for (;;) {
    const match = /\r?\n\r?\n/.exec(buffer);
    if (!match) return buffer;
    const record = buffer.slice(0, match.index);
    buffer = buffer.slice(match.index + match[0].length);
    const data = record
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart())
      .join("\n");
    if (data) consume(data);
  }
}

function parseArguments(value) {
  if (!value) return {};
  if (typeof value !== "string") return value;
  try {
    return JSON.parse(value);
  } catch {
    throw new Error("model returned invalid JSON tool arguments");
  }
}

async function readBoundedText(response, limit, signal) {
  if (!response.body) return "";
  const declared = Number(response.headers.get("content-length"));
  if (Number.isFinite(declared) && declared > limit) {
    throw new Error(`model response exceeds ${limit} bytes`);
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let bytes = 0;
  let text = "";
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      if (signal.aborted) throw new DOMException("aborted", "AbortError");
      bytes += value.byteLength;
      if (bytes > limit) {
        await reader.cancel();
        throw new Error(`model response exceeds ${limit} bytes`);
      }
      text += decoder.decode(value, { stream: true });
    }
    return text + decoder.decode();
  } finally {
    reader.releaseLock();
  }
}

function normalizeUsage(usage = {}) {
  const input = usage.prompt_tokens ?? usage.input_tokens ?? 0;
  const output = usage.completion_tokens ?? usage.output_tokens ?? 0;
  const cacheRead = usage.prompt_tokens_details?.cached_tokens ?? 0;
  const reasoning = usage.completion_tokens_details?.reasoning_tokens;
  return {
    input,
    output,
    cacheRead,
    cacheWrite: 0,
    ...(reasoning === undefined ? {} : { reasoning }),
    totalTokens: usage.total_tokens ?? input + output,
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
  };
}

function mapStopReason(reason, content) {
  if (reason === "length") return "length";
  if (reason === "tool_calls" || content.some((block) => block.type === "toolCall")) return "toolUse";
  return "stop";
}
