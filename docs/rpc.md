# RPC mode

RPC mode runs KISS headlessly for any language or runtime. Commands are JSON
objects sent to standard input, one per line. Responses and asynchronous agent
events are JSON objects read from standard output, one per line.

```sh
kiss --mode rpc --no-session [--provider anthropic] [--model claude-sonnet-4-5]
```

For browsers, use a WebSocket listener:

```sh
kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session
```

**Security:** RPC has no authentication. The default tools can read, modify, and
execute files. Bind only to loopback (`127.0.0.1` or `[::1]`) unless an
authenticated proxy and operating-system sandbox protect the process.

## Framing and correlation

A line feed (`\n`) is the only record separator. Strip one optional `\r` before
it. Do not use a generic reader that also splits on Unicode `U+2028` or `U+2029`:
those characters are valid inside JSON strings. Node's `readline` is not
protocol-safe; buffer bytes and split on `\n`.

Every command may include `id`; its response echoes it. Events normally have no
id. A `bash_execution_update` event echoes the id of its direct `bash` command.
A successful `prompt` response means accepted, not finished; wait for
`agent_settled`.

```json
{"id":"1","type":"prompt","message":"List files"}
{"type":"response","id":"1","command":"prompt","success":true}
{"type":"agent_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Here"}}
{"type":"agent_settled"}
```

Failures are responses and do not close the connection:

```json
{"type":"response","command":"set_model","success":false,"error":"model not found: bad/model"}
```

Malformed input uses command `parse`.

## Commands

All payload fields use camelCase.

### Prompt and lifecycle

- `{"type":"prompt","message":"...","images":[],"streamingBehavior":"steer"}` — accept a prompt. `streamingBehavior` is optional only while idle; use `steer` or `followUp` while busy.
- `{"type":"steer","message":"...","images":[]}` — deliver after current-turn tools.
- `{"type":"follow_up","message":"...","images":[]}` — deliver once the agent stops.
- `{"type":"abort"}` — cancel and respond once idle.
- `{"type":"clear_queue"}` — returns `data.messages`.
- `{"type":"ping"}` — returns `data.pong: true`.

An image is `{"type":"image","data":"<base64>","mimeType":"image/png"}`.

### State and sessions

- `get_state` — model, thinking level, streaming state, session id/file/name, message count, tool names, queue modes, retry and compaction state.
- `get_messages` — active conversation after branching/compaction.
- `get_entries`, optionally `since` — append-only entries after a durable entry-id cursor and current `leafId`.
- `get_tree` — recursive `{entry, children, label}` nodes and `leafId`.
- `get_last_assistant_text` — nullable `data.text`.
- `get_session_stats` — message/tool counts, tokens, cost, and context use.
- `set_session_name` with `name`.
- `new_session` — replace active history with an empty in-memory session.
- `switch_session` with `sessionPath`.
- `fork` with `entryId` — move to the selected point and return editable user text.
- `get_fork_messages` — user entries available for branching.
- `export_html`, optional `outputPath` — writes an HTML transcript and returns its path.

### Models and thinking

- `set_model` with `provider` and `modelId`.
- `get_available_models`, optional `search` substring.
- `set_thinking_level` with one of `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; unsupported levels fail rather than silently downgrade.
- `get_available_thinking_levels`.

### Queue, retry, and compaction

- `set_steering_mode` / `set_follow_up_mode` with `mode` `all` or `one-at-a-time`.
- `compact`, optional `customInstructions`.
- `set_auto_compaction` / `set_auto_retry` with boolean `enabled`.

### Tools and shell

- `get_tools` — enabled tool names.
- `bash` with `command` — runs immediately, streams `bash_execution_update`, returns output/exitCode/cancelled/truncated/fullOutputPath, and records a `bashExecution` message for the next model request.
- `abort_bash`.

## Events

Lifecycle: `agent_start`, `agent_end`, `agent_settled`, `turn_start`, `turn_end`,
`message_start`, `message_update`, `message_end`.

Tools: `tool_execution_start`, `tool_execution_update`, `tool_execution_end`,
and direct-shell `bash_execution_update`.

Session: `queue_update`, `compaction_start`, `compaction_end`, `retry`,
`model_changed`, `workflow_progress`, `workflow_outcome`, and `event_lag`.

`message_update.assistantMessageEvent` types are `start`, `text_start`,
`text_delta`, `text_end`, `thinking_start`, `thinking_delta`, `thinking_end`,
`toolcall_start`, `toolcall_delta`, `toolcall_end`, `done`, and `error`. Assemble
live output by `contentIndex`; `message_end.message` is authoritative.

## Minimal Python subprocess client

```python
import json, subprocess

process = subprocess.Popen(
    ["kiss", "--mode", "rpc", "--no-session"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
process.stdin.write(json.dumps({"id": "1", "type": "prompt", "message": "Hello"}) + "\n")
process.stdin.flush()
for raw_line in process.stdout:
    value = json.loads(raw_line.removesuffix("\n").removesuffix("\r"))
    if value.get("type") == "message_update":
        update = value["assistantMessageEvent"]
        if update["type"] == "text_delta": print(update["delta"], end="", flush=True)
    if value.get("type") == "agent_settled": break
```

## Minimal Go subprocess client

```go
cmd := exec.Command("kiss", "--mode", "rpc", "--no-session")
in, _ := cmd.StdinPipe(); out, _ := cmd.StdoutPipe(); _ = cmd.Start()
_ = json.NewEncoder(in).Encode(map[string]any{"id":"1", "type":"prompt", "message":"Hello"})
scanner := bufio.NewScanner(out) // Scanner splits on LF, as required.
for scanner.Scan() {
    var value map[string]any
    _ = json.Unmarshal(scanner.Bytes(), &value)
    if value["type"] == "agent_settled" { break }
}
```
