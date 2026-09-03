from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

import kiss_sdk
from kiss_sdk._kiss import MockProvider


async def make_session(tmp_path: Path, script: list[list[dict[str, Any]]]) -> tuple[Any, kiss_sdk.Session]:
    provider = await MockProvider.start(str(tmp_path), script)
    session = await kiss_sdk.Session.create(
        cwd=str(tmp_path),
        model="mock/mock-1",
        models_file=provider.catalog_path,
        no_context_files=True,
    )
    return provider, session


@pytest.mark.asyncio
async def test_prompt_streams_and_writes_a_real_file(tmp_path: Path) -> None:
    provider, session = await make_session(
        tmp_path,
        [
            [
                {
                    "toolCall": {
                        "id": "call_1",
                        "name": "write",
                        "arguments": {"path": "hello.txt", "content": "hello from python\n"},
                    }
                }
            ],
            [{"text": "Done."}],
        ],
    )
    events = session.events()
    seen: list[kiss_sdk.Event] = []

    async def collect() -> None:
        async for event in events:
            seen.append(event)
            if event.type == "agent_settled":
                return

    collector = asyncio.create_task(collect())
    await session.prompt("create hello.txt")
    await asyncio.wait_for(collector, timeout=5)

    assert (tmp_path / "hello.txt").read_text() == "hello from python\n"
    assert any(e.type == "tool_execution_start" and e["toolName"] == "write" for e in seen)
    assert any(e.type == "tool_execution_end" and e["isError"] is False for e in seen)
    streamed = "".join(
        e["assistantMessageEvent"].get("delta", "")
        for e in seen
        if e.type == "message_update"
        and e["assistantMessageEvent"]["type"] == "text_delta"
    )
    assert streamed == "Done."
    assert await session.last_assistant_text() == "Done."
    assert len(provider.requests()) == 2
    await session.aclose()


@pytest.mark.asyncio
async def test_state_models_tools_and_ping_are_typed(tmp_path: Path) -> None:
    _provider, session = await make_session(tmp_path, [[{"text": "hello"}]])
    state = await session.state()
    assert state.model is not None
    assert state.model["provider"] == "mock"
    assert state.thinking_level == "off"
    assert state.tools == ["read", "write", "edit", "bash"]
    assert await session.ping() is True
    assert await session.tools() == ["read", "write", "edit", "bash"]
    models = await session.available_models("mock")
    assert [model["id"] for model in models] == ["mock-1"]
    assert await session.available_thinking_levels() == ["off"]
    await session.aclose()


@pytest.mark.asyncio
async def test_direct_bash_reports_nonzero_exit_and_enters_history(tmp_path: Path) -> None:
    _provider, session = await make_session(tmp_path, [[{"text": "hello"}]])
    result = await session.bash("printf python-bash; exit 7")
    assert result.output == "python-bash"
    assert result.exit_code == 7
    assert result.cancelled is False
    messages = await session.messages()
    assert messages[-1]["role"] == "bashExecution"
    assert messages[-1]["command"] == "printf python-bash; exit 7"
    await session.aclose()


@pytest.mark.asyncio
async def test_raw_execute_uses_the_same_protocol_and_failures_raise(tmp_path: Path) -> None:
    _provider, session = await make_session(tmp_path, [[{"text": "hello"}]])
    response = await session.execute({"type": "ping"})
    assert response == {
        "type": "response",
        "command": "ping",
        "success": True,
        "data": {"pong": True},
    }
    with pytest.raises(kiss_sdk.KissError, match="nope/nope"):
        await session.set_model("nope", "nope")
    with pytest.raises(ValueError, match="invalid command"):
        await session.execute({"type": "teleport"})
    await session.aclose()
