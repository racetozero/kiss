"""The Python-facing session API.

The native extension (``kiss_sdk._kiss``) exposes a deliberately small surface:
``create``, ``execute``, ``events``, and a handful of operations that need to be
synchronous. This module builds the friendly, fully typed API on top of it, and
keeps every typed method a one-line wrapper over ``execute`` so a change in the
Rust dispatcher is immediately visible in Python without touching this file.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, AsyncIterator, Iterable, Literal, Self

from ._kiss import KissError
from ._kiss import Session as _NativeSession

ThinkingLevel = Literal["off", "minimal", "low", "medium", "high", "xhigh", "max"]
StreamingBehavior = Literal["steer", "followUp"]
ToolName = Literal["read", "write", "edit", "bash", "grep", "find", "ls", "mcp"]
SessionSource = str
QueueModeName = Literal["all", "one-at-a-time"]


class Event:
    """One notification from the agent.

    Behaves like the underlying dictionary — ``event["toolName"]`` works — with
    a convenience ``type`` property so ``match event.type:`` reads naturally.
    The payload shape is identical to the RPC protocol documented in
    ``docs/rpc.md``.
    """

    __slots__ = ("_data",)

    def __init__(self, data: dict[str, Any]) -> None:
        self._data = data

    @property
    def type(self) -> str:
        return self._data.get("type", "")

    @property
    def data(self) -> dict[str, Any]:
        return self._data

    def get(self, key: str, default: Any = None) -> Any:
        return self._data.get(key, default)

    def __getitem__(self, key: str) -> Any:
        return self._data[key]

    def __contains__(self, key: object) -> bool:
        return key in self._data

    def __repr__(self) -> str:
        return f"Event({self._data!r})"

    def __eq__(self, other: object) -> bool:
        if isinstance(other, Event):
            return self._data == other._data
        return NotImplemented


class EventStream:
    """Async iterator over :class:`Event` objects.

    If you consume events more slowly than the agent produces them, the oldest
    are dropped and you receive one ``event_lag`` event naming how many were
    missed. Re-read state rather than assume you saw everything.
    """

    __slots__ = ("_native",)

    def __init__(self, native: Any) -> None:
        self._native = native

    def __aiter__(self) -> AsyncIterator[Event]:
        return self

    async def __anext__(self) -> Event:
        return Event(await self._native.__anext__())


@dataclass(frozen=True, slots=True)
class SessionState:
    """A snapshot of the session, as returned by :meth:`Session.state`."""

    model: dict[str, Any] | None
    thinking_level: ThinkingLevel
    is_streaming: bool
    session_file: str | None
    session_id: str
    session_name: str | None
    message_count: int
    tools: list[str]
    steering_mode: QueueModeName
    follow_up_mode: QueueModeName
    auto_compaction_enabled: bool
    auto_retry_enabled: bool

    @classmethod
    def _from_json(cls, data: dict[str, Any]) -> SessionState:
        return cls(
            model=data.get("model"),
            thinking_level=data.get("thinkingLevel", "off"),
            is_streaming=data.get("isStreaming", False),
            session_file=data.get("sessionFile"),
            session_id=data.get("sessionId", ""),
            session_name=data.get("sessionName"),
            message_count=data.get("messageCount", 0),
            tools=list(data.get("tools", [])),
            steering_mode=data.get("steeringMode", "one-at-a-time"),
            follow_up_mode=data.get("followUpMode", "one-at-a-time"),
            auto_compaction_enabled=data.get("autoCompactionEnabled", True),
            auto_retry_enabled=data.get("autoRetryEnabled", True),
        )


@dataclass(frozen=True, slots=True)
class BashResult:
    """The outcome of :meth:`Session.bash`."""

    output: str
    exit_code: int | None
    cancelled: bool
    truncated: bool
    full_output_path: str | None

    @classmethod
    def _from_json(cls, data: dict[str, Any]) -> BashResult:
        return cls(
            output=data.get("output", ""),
            exit_code=data.get("exitCode"),
            cancelled=data.get("cancelled", False),
            truncated=data.get("truncated", False),
            full_output_path=data.get("fullOutputPath"),
        )


class Session:
    """One embeddable conversation with the agent."""

    __slots__ = ("_native",)

    def __init__(self, native: _NativeSession) -> None:
        self._native = native

    # -- construction -------------------------------------------------

    @classmethod
    async def create(
        cls,
        *,
        cwd: str | None = None,
        model: str | None = None,
        provider: str | None = None,
        api_key: str | None = None,
        models_file: str | None = None,
        thinking_level: ThinkingLevel | None = None,
        tools: Iterable[ToolName | str] | None = None,
        exclude_tools: Iterable[str] | None = None,
        no_tools: bool = False,
        system_prompt: str | None = None,
        append_system_prompt: str | None = None,
        session: SessionSource = "in-memory",
        session_dir: str | None = None,
        session_name: str | None = None,
        trust_project_files: bool = False,
        no_context_files: bool = False,
        event_capacity: int = 1024,
    ) -> Self:
        """Start a session.

        ``session`` selects where history comes from: ``"in-memory"`` (the
        default, nothing is written to disk), ``"create"``, ``"continue"``,
        ``"open:<path>"``, or ``"fork:<path>"``.

        ``models_file`` points at an alternative ``models.json`` catalog, which
        is how the tests and the offline demo reach a local fake provider.
        """
        options: dict[str, Any] = {
            "cwd": cwd,
            "model": model,
            "provider": provider,
            "api_key": api_key,
            "models_file": models_file,
            "thinking_level": thinking_level,
            "tools": list(tools) if tools is not None else None,
            "exclude_tools": list(exclude_tools) if exclude_tools is not None else None,
            "no_tools": no_tools,
            "system_prompt": system_prompt,
            "append_system_prompt": append_system_prompt,
            "session": session,
            "session_dir": session_dir,
            "session_name": session_name,
            "trust_project_files": trust_project_files,
            "no_context_files": no_context_files,
            "event_capacity": event_capacity,
        }
        return cls(await _NativeSession.create(options))

    # -- the escape hatch ---------------------------------------------

    async def execute(self, command: dict[str, Any]) -> dict[str, Any]:
        """Run one protocol command and return the raw response dictionary.

        Every other method is a wrapper over this one.
        """
        return await self._native.execute(command)

    async def _require(self, command: dict[str, Any]) -> dict[str, Any]:
        """Run a command and raise :class:`KissError` unless it succeeded."""
        response = await self.execute(command)
        if not response.get("success", False):
            raise KissError(response.get("error") or f"{command['type']} failed")
        return response.get("data") or {}

    # -- prompting ----------------------------------------------------

    async def prompt(
        self,
        message: str,
        *,
        images: list[dict[str, str]] | None = None,
        streaming_behavior: StreamingBehavior | None = None,
    ) -> None:
        """Send a prompt and wait for the whole run to finish.

        While the agent is already streaming you must say how to queue the
        message, otherwise this raises :class:`KissError`.
        """
        await self._native.prompt(message, images, streaming_behavior)

    def prompt_detached(
        self,
        message: str,
        *,
        streaming_behavior: StreamingBehavior | None = None,
    ) -> None:
        """Send a prompt and return once it is accepted; do not wait for it."""
        self._native.prompt_detached(message, streaming_behavior)

    def steer(self, message: str) -> None:
        """Queue a message for after the current turn's tool calls."""
        self._native.steer(message)

    def follow_up(self, message: str) -> None:
        """Queue a message for when the agent stops."""
        self._native.follow_up(message)

    def abort(self) -> None:
        """Cancel the current run and any direct shell command."""
        self._native.abort()

    async def wait_idle(self) -> None:
        """Wait until no prompt run is in flight."""
        await self._native.wait_idle()

    # -- events -------------------------------------------------------

    def events(self) -> EventStream:
        """Subscribe to events. Each call returns an independent stream."""
        return EventStream(self._native.events())

    # -- state --------------------------------------------------------

    async def state(self) -> SessionState:
        return SessionState._from_json(await self._require({"type": "get_state"}))

    async def messages(self) -> list[dict[str, Any]]:
        return list((await self._require({"type": "get_messages"}))["messages"])

    async def entries(self, since: str | None = None) -> dict[str, Any]:
        return await self._require({"type": "get_entries", "since": since})

    async def tree(self) -> dict[str, Any]:
        return await self._require({"type": "get_tree"})

    async def last_assistant_text(self) -> str | None:
        return (await self._require({"type": "get_last_assistant_text"}))["text"]

    async def session_stats(self) -> dict[str, Any]:
        return await self._require({"type": "get_session_stats"})

    async def set_session_name(self, name: str) -> None:
        await self._require({"type": "set_session_name", "name": name})

    async def tools(self) -> list[str]:
        data = await self._require({"type": "get_tools"})
        return [tool["name"] for tool in data["tools"]]

    # -- model --------------------------------------------------------

    async def set_model(self, provider: str, model_id: str) -> dict[str, Any]:
        return await self._require(
            {"type": "set_model", "provider": provider, "modelId": model_id}
        )

    async def available_models(self, search: str | None = None) -> list[dict[str, Any]]:
        data = await self._require({"type": "get_available_models", "search": search})
        return list(data["models"])

    async def set_thinking_level(self, level: ThinkingLevel) -> None:
        await self._require({"type": "set_thinking_level", "level": level})

    async def available_thinking_levels(self) -> list[str]:
        return list((await self._require({"type": "get_available_thinking_levels"}))["levels"])

    # -- queues -------------------------------------------------------

    async def set_steering_mode(self, mode: QueueModeName) -> None:
        await self._require({"type": "set_steering_mode", "mode": mode})

    async def set_follow_up_mode(self, mode: QueueModeName) -> None:
        await self._require({"type": "set_follow_up_mode", "mode": mode})

    async def clear_queue(self) -> list[str]:
        return list((await self._require({"type": "clear_queue"}))["messages"])

    # -- context management -------------------------------------------

    async def compact(self, custom_instructions: str | None = None) -> dict[str, Any]:
        return await self._require(
            {"type": "compact", "customInstructions": custom_instructions}
        )

    async def set_auto_compaction(self, enabled: bool) -> None:
        await self._require({"type": "set_auto_compaction", "enabled": enabled})

    async def set_auto_retry(self, enabled: bool) -> None:
        await self._require({"type": "set_auto_retry", "enabled": enabled})

    # -- shell --------------------------------------------------------

    async def bash(self, command: str) -> BashResult:
        """Run a shell command and add it to the conversation history.

        The output reaches the model with the *next* prompt, not immediately.
        """
        return BashResult._from_json(await self._require({"type": "bash", "command": command}))

    async def abort_bash(self) -> None:
        await self._require({"type": "abort_bash"})

    # -- sessions -----------------------------------------------------

    async def new_session(self) -> None:
        await self._require({"type": "new_session"})

    async def switch_session(self, session_path: str) -> None:
        await self._require({"type": "switch_session", "sessionPath": session_path})

    async def fork(self, entry_id: str) -> dict[str, Any]:
        return await self._require({"type": "fork", "entryId": entry_id})

    async def fork_messages(self) -> list[dict[str, Any]]:
        return list((await self._require({"type": "get_fork_messages"}))["messages"])

    async def ping(self) -> bool:
        return bool((await self._require({"type": "ping"}))["pong"])

    # -- lifecycle ----------------------------------------------------

    def close(self) -> None:
        self._native.close()

    async def aclose(self) -> None:
        self._native.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exception: object) -> None:
        await self.aclose()
