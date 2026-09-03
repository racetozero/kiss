# kiss-sdk for Python

Python 3.11+ bindings for the KISS coding agent. The native module uses PyO3's
stable `abi3-py311` ABI, so one wheel works on Python 3.11 and newer.

```python
import asyncio
from kiss_sdk import Session

async def main() -> None:
    async with await Session.create(tools=["read", "bash"]) as session:
        events = session.events()
        session.prompt_detached("List the files here")
        async for event in events:
            if event.type == "message_update":
                update = event["assistantMessageEvent"]
                if update["type"] == "text_delta":
                    print(update["delta"], end="", flush=True)
            if event.type == "agent_settled":
                break

asyncio.run(main())
```

Build locally with:

```sh
maturin develop
pytest -q
```

See the repository's `docs/sdk.md` and `docs/rpc.md` for the complete API and
event protocol.
