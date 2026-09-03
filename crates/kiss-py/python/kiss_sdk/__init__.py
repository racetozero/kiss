"""Embed the kiss coding agent in a Python program.

Quick start::

    import asyncio
    import kiss_sdk

    async def main() -> None:
        async with await kiss_sdk.Session.create(tools=["read", "bash"]) as session:
            async def show() -> None:
                async for event in session.events():
                    if event.type == "message_update":
                        delta = event["assistantMessageEvent"]
                        if delta["type"] == "text_delta":
                            print(delta["delta"], end="", flush=True)

            printer = asyncio.create_task(show())
            await session.prompt("List the files here")
            printer.cancel()

    asyncio.run(main())

Everything here funnels into the same Rust dispatcher that the Rust SDK, the
TypeScript SDK, and ``kiss --mode rpc`` use, so the four surfaces cannot behave
differently. If a method you need is missing, build the command yourself and
call :meth:`Session.execute`; the command names and payloads are documented in
``docs/rpc.md``.
"""

from __future__ import annotations

from ._kiss import KissError, __version__
from ._session import (
    BashResult,
    Event,
    EventStream,
    Session,
    SessionState,
    StreamingBehavior,
    ThinkingLevel,
    ToolName,
)

__all__ = [
    "BashResult",
    "Event",
    "EventStream",
    "KissError",
    "Session",
    "SessionState",
    "StreamingBehavior",
    "ThinkingLevel",
    "ToolName",
    "__version__",
]
