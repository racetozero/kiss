# kiss-agent-sdk for Python

Python 3.11+ bindings for the KISS coding agent. Releases include separate
wheels for CPython 3.11 through 3.15, plus free-threaded 3.14t and 3.15t.
PyO3 0.29 does not support the experimental 3.13t build.

```python
import asyncio
from kiss_sdk import Session


async def main() -> None:
    async with await Session.create(tools=['read', 'bash']) as session:
        events = session.events()
        session.prompt_detached('List the files here')
        async for event in events:
            if event.type == 'message_update':
                update = event['assistantMessageEvent']
                if update['type'] == 'text_delta':
                    print(update['delta'], end='', flush=True)
            if event.type == 'agent_settled':
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
