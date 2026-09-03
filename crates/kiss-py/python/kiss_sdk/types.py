"""Typed Python representations of the shared KISS wire protocol."""

from __future__ import annotations

from enum import StrEnum
from typing import Any, NotRequired, Required, TypedDict


class ThinkingLevel(StrEnum):
    OFF = "off"
    MINIMAL = "minimal"
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"
    XHIGH = "xhigh"
    MAX = "max"


class StreamingBehavior(StrEnum):
    STEER = "steer"
    FOLLOW_UP = "followUp"


class ToolName(StrEnum):
    READ = "read"
    WRITE = "write"
    EDIT = "edit"
    BASH = "bash"
    GREP = "grep"
    FIND = "find"
    LS = "ls"
    MCP = "mcp"


class QueueMode(StrEnum):
    ALL = "all"
    ONE_AT_A_TIME = "one-at-a-time"


class ImageInput(TypedDict):
    type: Required[str]
    data: Required[str]
    mimeType: Required[str]


class ModelData(TypedDict, total=False):
    id: Required[str]
    name: Required[str]
    api: Required[str]
    provider: Required[str]
    baseUrl: Required[str]
    reasoning: Required[bool]
    input: Required[list[str]]
    contextWindow: Required[int]
    maxTokens: Required[int]
    cost: Required[dict[str, float]]


class MessageData(TypedDict, total=False):
    role: Required[str]
    content: Any
    timestamp: int
    command: str
    output: str
    exitCode: int | None
    cancelled: bool
    truncated: bool


class AssistantMessageEvent(TypedDict, total=False):
    type: Required[str]
    contentIndex: int
    delta: str
    content: str
    id: str
    toolName: str
    toolCall: dict[str, Any]


class EventData(TypedDict, total=False):
    type: Required[str]
    assistantMessageEvent: AssistantMessageEvent
    message: MessageData
    messages: list[MessageData]
    toolCallId: str
    toolName: str
    args: dict[str, Any]
    result: dict[str, Any]
    partialResult: dict[str, Any]
    isError: bool
    steering: list[str]
    followUp: list[str]
    skipped: int
    delta: str
    id: str


class CommandData(TypedDict, total=False):
    type: Required[str]
    message: str
    images: list[ImageInput]
    streamingBehavior: StreamingBehavior | str
    provider: str
    modelId: str
    level: ThinkingLevel | str
    mode: QueueMode | str
    enabled: bool
    command: str
    search: str | None
    since: str | None
    name: str
    customInstructions: str | None
    sessionPath: str
    entryId: str
    outputPath: str | None


class ResponseData(TypedDict, total=False):
    type: Required[str]
    id: str
    command: Required[str]
    success: Required[bool]
    data: Any
    error: str


class SessionStateData(TypedDict, total=False):
    model: ModelData | None
    thinkingLevel: ThinkingLevel
    isStreaming: bool
    sessionFile: str | None
    sessionId: str
    sessionName: str | None
    messageCount: int
    tools: list[str]
    steeringMode: QueueMode
    followUpMode: QueueMode
    autoCompactionEnabled: bool
    autoRetryEnabled: bool


class BashResultData(TypedDict, total=False):
    output: str
    exitCode: int | None
    cancelled: bool
    truncated: bool
    fullOutputPath: str | None


class EntriesData(TypedDict):
    entries: list[dict[str, Any]]
    leafId: str | None


class TreeData(TypedDict):
    tree: list[dict[str, Any]]
    leafId: str | None


class SessionStatsData(TypedDict, total=False):
    sessionFile: str | None
    sessionId: str
    userMessages: int
    assistantMessages: int
    toolCalls: int
    toolResults: int
    totalMessages: int
    tokens: dict[str, int]
    cost: float
    contextUsage: dict[str, int]
