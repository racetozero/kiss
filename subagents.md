# Subagent design analysis for KISS

Date: 2026-08-31

Status: Analysis only. This document does not include an implementation plan or code changes.

## 1. Purpose

This document compares the subagent designs in these projects:

- OpenAI Codex
- Claude Code
- OpenCode
- DeepSeek Harness
- Pi Subagents

A subagent is a child agent that gets a small task from a parent agent. The child has its own model context and its own agent loop. The parent collects the result and continues the main task.

The goal is to select a design for KISS. The design must be small, safe, and useful. Subagents must be optional. A user must enable them in `/settings`.

## 2. Research method

I used the upstream documentation and the implementation source. I fixed each source review to one commit. This makes the findings repeatable.

- OpenAI Codex: commit [`9f97cb79eb15b38d24c552c56fe24e211ff9cf3a`](https://github.com/openai/codex/tree/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a)
- Claude Code source copy: commit [`6f6f12b37f529488b10e53928dd5508bb93535c7`](https://github.com/tanbiralam/claude-code/tree/6f6f12b37f529488b10e53928dd5508bb93535c7)
- OpenCode: commit [`26ff3ed3d3e28830190ef53f2ff4b261852139a4`](https://github.com/anomalyco/opencode/tree/26ff3ed3d3e28830190ef53f2ff4b261852139a4)
- DeepSeek Harness: commit [`dd6322d604e00eec1ba5e0c8541159906a21094a`](https://github.com/deepseek-ai/deepseek-harness/tree/dd6322d604e00eec1ba5e0c8541159906a21094a)
- Pi Subagents: commit [`4b30e4b55761313fd7e467c9a8effa95db3f93b6`](https://github.com/nicobailon/pi-subagents/tree/4b30e4b55761313fd7e467c9a8effa95db3f93b6)

The Claude Code source copy needs special care. Its README says that it is source from an exposed source map. It also says that some modules are stubs. It is not an official Anthropic repository. I used it only to inspect implementation details. I used the [official Claude Code subagent documentation](https://code.claude.com/docs/en/sub-agents) as the authority for user-visible behavior.

I compared these design areas:

1. Context isolation and context transfer
2. Child lifecycle and cancellation
3. Tool and permission limits
4. Parallel work and depth limits
5. Result delivery and parent-to-child messages
6. Session storage and resume
7. Agent definitions and model selection
8. User interface and logs
9. Failure handling
10. Fit with the current KISS code

## 3. Executive finding

OpenAI Codex has the best complete native coordination design for KISS to study. It uses the same Rust process, a separate thread for each child, a task tree, clear control tools, bounded parallel work, durable child state, and selectable context forks. It is also the closest technical match to KISS.

OpenCode has the best small core design. Its `task` tool creates a normal child session, uses the existing agent loop, waits for a foreground result, and can resume the child by session ID. This design is easy to understand and is close to the current KISS session model.

DeepSeek Harness has the best service boundary. It keeps subagents out of the core agent loop. It defines a provider contract and rejects unsupported features before a child starts. Its runtime rules are very strong, but its complete design is too large for KISS.

Claude Code has the best agent definition and context selection design. It supports focused Markdown agent files, fresh context, full conversation forks, tool filters, model selection, foreground work, background work, resume, and worktree isolation.

Pi Subagents has the best operations layer. It has strong progress views, artifacts, output limits, retries, budgets, workflows, and external agent adapters. It is a large orchestration product. A direct copy would not fit KISS.

The best KISS design is a hybrid:

- Use the OpenCode child-session model as the small base.
- Use the Codex task tree, control tools, mailbox wake-up, and context-fork rules.
- Use the DeepSeek capability and authority rules.
- Use the Claude Code agent-file format and `fresh` or `fork` context choice.
- Add selected Pi operations features only after the core is stable.

This is the main recommendation. Do not copy one project in full.

## 4. OpenAI Codex

### 4.1 Design

Codex treats each subagent as a separate Codex thread. A thread is a durable agent session with its own context, model calls, tool calls, status, and stored rollout.

The root agent and child agents form a named task tree. A child has a stable thread ID and a canonical path such as `/root/review_api`. The path gives a clear parent and child relation.

Codex exposes small control tools:

- `spawn_agent`
- `send_message`
- `followup_task`
- `wait_agent`
- `list_agents`
- `interrupt_agent`

This is better than one tool with many unrelated actions. Each tool has one clear job. The tool names also tell the model what operation it must use.

The current V2 spawn tool accepts `fork_turns`. It supports these values:

- `none`: start with no parent history
- `all`: copy the full allowed parent history
- A positive integer: copy the last number of turns

The full-history path does not copy all raw events. It filters tool calls, tool outputs, reasoning blocks, inter-agent messages, and old multi-agent instructions. This prevents invalid tool pairs and duplicate role instructions. See the [V2 spawn handler](https://github.com/openai/codex/blob/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a/codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs) and the [agent spawn code](https://github.com/openai/codex/blob/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a/codex-rs/core/src/agent/control/spawn.rs).

The child gets a configuration that comes from the active parent turn. Codex then applies the selected agent role and any allowed model or reasoning override. Runtime data such as the working directory, environment, execution policy, and permission profile also comes from the parent.

Codex intersects permission profiles. An intersection keeps only permissions that both sides allow. A child cannot get more authority than its parent.

Codex separates two limits:

- The execution limit controls how many child turns can run at the same time.
- The residency limit controls how many child threads can stay loaded in memory.

When the residency limit is full, Codex can unload an idle completed child. It keeps the child rollout on disk and can load it later. This is a strong design for long sessions, but KISS does not need this feature in the first release. See the [execution limiter](https://github.com/openai/codex/blob/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a/codex-rs/core/src/agent/control/execution.rs) and [residency manager](https://github.com/openai/codex/blob/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a/codex-rs/core/src/agent/control/residency.rs).

`wait_agent` does not use fast repeated status checks. It waits for mailbox activity, new user input, or a timeout. The runtime limits the minimum and maximum wait times. See the [V2 wait handler](https://github.com/openai/codex/blob/9f97cb79eb15b38d24c552c56fe24e211ff9cf3a/codex-rs/core/src/tools/handlers/multi_agents_v2/wait.rs).

Codex records child activity as normal turn items. The terminal interface can show starts, messages, completion, and status. The child result is a message to the parent. It is not a raw copy of the full child transcript.

The [current Codex documentation](https://developers.openai.com/codex/subagents) says that releases enable subagent workflows by default, but only spawn when the user asks. KISS must use a stricter rule. The KISS setting must default to off.

### 4.2 Strong points

- The child is a real session, not a special recursive function.
- The task tree gives stable identity and ownership.
- The control tools have clear and separate meanings.
- Context transfer is explicit and safe.
- Permission authority can only stay equal or decrease.
- Parallel work and nesting have limits.
- The mailbox removes the need for status polling.
- Child sessions can resume after they leave memory.
- The design has strong logs and test points.

### 4.3 Weak points

- The current code has V1 and V2 paths. This adds migration code and complexity.
- The persistent graph, resident-thread cache, role catalog, encrypted messages, and service-tier rules are too large for the first KISS release.
- Full thread navigation needs much terminal interface code.
- The implementation has many states. A small harness can easily copy the wrong parts.

### 4.4 Lesson for KISS

Use the task tree, separate tools, bounded wait, strict authority, and safe context fork. Do not copy the V1 compatibility path, resident-thread cache, encrypted internal messages, or service-tier logic.

## 5. Claude Code

### 5.1 Design

Claude Code uses an `Agent` tool. The tool selects an agent definition and starts another agent loop. A normal subagent starts with a fresh context window. A conversation fork gets the parent conversation.

Agent definitions can set these main values:

- Name and description
- System prompt
- Model
- Maximum turns
- Allowed tools
- Disallowed tools
- Permission mode
- Skills
- Memory
- Background mode
- Worktree isolation

The description is important. Claude uses it to select an agent without a direct user command.

The implementation builds a child tool pool from the parent permission context and the selected definition. It checks denied agent types and required MCP servers before it starts the child. See [`AgentTool.tsx`](https://github.com/tanbiralam/claude-code/blob/6f6f12b37f529488b10e53928dd5508bb93535c7/src/tools/AgentTool/AgentTool.tsx) and [`runAgent.ts`](https://github.com/tanbiralam/claude-code/blob/6f6f12b37f529488b10e53928dd5508bb93535c7/src/tools/AgentTool/runAgent.ts).

A normal child gets its own prompt and a focused system prompt. It does not get the full parent system prompt or transcript. A fork gets the parent transcript and the exact rendered parent system prompt. The fork code uses fixed placeholder tool results so that parallel child requests share the same cached prompt prefix. See [`forkSubagent.ts`](https://github.com/tanbiralam/claude-code/blob/6f6f12b37f529488b10e53928dd5508bb93535c7/src/tools/AgentTool/forkSubagent.ts).

Foreground mode waits for the child result. Background mode returns an agent ID and an output path. A task registry tracks progress, cancellation, output, and user notifications. A completed background child notifies the parent.

Claude Code stores the child transcript and metadata. Resume loads the transcript, removes incomplete tool-use records, restores the child agent definition, restores the worktree when it still exists, and starts another child turn. See [`resumeAgent.ts`](https://github.com/tanbiralam/claude-code/blob/6f6f12b37f529488b10e53928dd5508bb93535c7/src/tools/AgentTool/resumeAgent.ts).

Current Claude Code supports nested subagents with depth and concurrency limits. At the depth limit, the runtime removes the `Agent` tool from normal children. A fork keeps the tool shape for prompt-cache stability, but the call returns an error. The [official subagent documentation](https://code.claude.com/docs/en/sub-agents) defines this behavior.

Claude Code can run a child in an isolated Git worktree. A worktree is a second working copy of the same repository. It lets two write agents work without direct file conflicts. This is valuable, but it needs merge and cleanup rules.

### 5.2 Strong points

- Markdown agent files are easy to write and share.
- Fresh context is the normal and low-cost choice.
- A full context fork is available when it is necessary.
- Tool filters and permission modes support focused agents.
- Foreground, background, resume, and worktree modes are complete.
- The child transcript has a stable agent ID.
- Prompt-cache details reduce the cost of parallel forks.

### 5.3 Weak points

- Many behaviors depend on feature flags.
- The `Agent` tool also has teammate and remote-agent paths. This makes the call code large.
- Background and foreground children can have different tool sets.
- Permission prompts in background work need careful routing.
- The reviewed source copy is not an official source repository.

### 5.4 Lesson for KISS

Use Markdown agent definitions later. Use `fresh` as the default context and `fork` as an explicit option. Do not copy teammate mode, remote-agent mode, or prompt-cache optimization in the first release.

## 6. OpenCode

### 6.1 Design

OpenCode has primary agents and subagents. A primary agent can call a `task` tool. A user can also call a subagent with an `@` mention.

The `task` tool creates a normal session with `parentID` set to the current session. The child session stores the selected agent name and a derived permission set. The child then uses the normal prompt and agent-loop code. See [`task.ts`](https://github.com/anomalyco/opencode/blob/26ff3ed3d3e28830190ef53f2ff4b261852139a4/packages/opencode/src/tool/task.ts).

The task input has these core fields:

- Short description
- Prompt
- Subagent type
- Optional task ID for resume
- Optional background flag

The selected subagent can have a different prompt, model, step limit, and permission rules. If it has no model override, it uses the model from the parent request.

The child receives the parent deny rules and external-directory rules. The child uses its own other permission rules. The runtime denies task and todo tools unless the child definition explicitly allows them. This makes nesting an explicit choice. See [`subagent-permissions.ts`](https://github.com/anomalyco/opencode/blob/26ff3ed3d3e28830190ef53f2ff4b261852139a4/packages/opencode/src/agent/subagent-permissions.ts).

OpenCode counts the parent chain before it starts a child. The default depth limit is one. A user can increase it with `subagent_depth`.

Foreground mode waits for the child session. Background mode uses the same child session and a background-job service. On completion, the service inserts one synthetic result message into the parent. The parent does not need to poll.

The task ID is also the child session ID. A later tool call can use that ID to continue the same session.

The terminal interface can move from a parent session to its child sessions and back. This is a simple and useful inspection model.

Agent definitions are JSON entries or Markdown files. They set the description, mode, model, prompt, steps, and permissions. The [OpenCode agent documentation](https://opencode.ai/docs/agents) describes this format.

### 6.2 Strong points

- The design reuses the normal session and prompt code.
- The parent-child relation is simple.
- Resume uses the existing session ID.
- Foreground and background modes use the same child state.
- Agent definitions are small and clear.
- The default depth is one.
- The result enters the parent once, without polling.
- The code is much smaller than the Codex, DeepSeek, or Pi designs.

### 6.3 Weak points

- The basic design has less direct parent-to-child control than Codex.
- The default child context is a fresh task prompt. There is no equally strong context-fork contract in this path.
- Parallel write agents share one working directory unless another feature isolates them.
- The permission derivation rules need careful reading. A child can use its own permissions, but it must still keep all parent deny rules.
- The background feature is experimental at the reviewed commit.

### 6.4 Lesson for KISS

Use a normal child `SessionManager` and the normal `run_agent_loop`. Use the child session ID as the agent ID. Keep the first depth limit at one. Add stronger Codex-style controls and context rules around this small core.

## 7. DeepSeek Harness

### 7.1 Design

DeepSeek Harness makes subagents an optional capability. The core agent loop does not know how a child runs. A named provider implements the child runtime.

The provider registry can hold several implementations at the same time. The reviewed source has providers for these paths:

- In-process fresh child
- In-process context fork
- Agent Client Protocol, or ACP
- Codex process
- Claude Code process
- DeepSeek Harness software development kit, or SDK

Each provider declares capabilities. Examples are model options, structured output, depth limit, tool filter, and persona. The runtime checks these flags before it starts a child. If a provider does not support a requested feature, the runtime returns a typed error. It does not ignore the option. This is an excellent rule.

DeepSeek separates one-shot children from continuable children:

- A one-shot child runs one delegated task and returns one result.
- A continuable child has a durable session and can accept more turns.

A continuable child has at most one live activation. An activation is the period in which the child agent is loaded and can run. The child inbox is the only message queue. This gives one order for initial work and follow-up work.

The continuation manager owns these tasks:

- Start admission
- Parent authorization
- Child identity
- Cold resume
- Child ownership
- Child-first cleanup
- Final settlement delivery

The agent loop still owns turn execution. This avoids a second agent state machine.

Only the direct parent can send a follow-up. The runtime checks the exact live parent agent and the durable parent session ID. A sender label does not grant authority.

A child can explicitly report to its parent. The runtime also sends a separate settlement notice when the child stops. These two messages have different source types. This prevents the log from showing runtime text as child-written text.

The child catalog can list stored children without loading them. The event system records start, end, provider add, and provider remove events.

See the [subagent subsystem document](https://github.com/deepseek-ai/deepseek-harness/blob/dd6322d604e00eec1ba5e0c8541159906a21094a/docs/subsystems/subagent.md), the [provider types](https://github.com/deepseek-ai/deepseek-harness/blob/dd6322d604e00eec1ba5e0c8541159906a21094a/packages/subagent/subagent/src/types.ts), and the [continuation manager](https://github.com/deepseek-ai/deepseek-harness/blob/dd6322d604e00eec1ba5e0c8541159906a21094a/packages/subagent/subagent/src/continuation.ts).

### 7.2 Strong points

- Subagents stay outside the core loop.
- Provider capabilities are explicit.
- Unsupported options fail before start.
- One-shot and continuable lifecycles are separate.
- The inbox gives one message order.
- Parent authority is strict.
- Cleanup follows the child tree from the leaves to the root.
- Stored child discovery does not start a child.
- The event and error contracts are complete.

### 7.3 Weak points

- The design uses many packages and service scopes.
- The continuation manager has many state and ownership rules.
- Multiple backends make test work much larger.
- The project is a developer preview and warns about breaking changes.
- The full design is not consistent with the size goal of KISS.

### 7.4 Lesson for KISS

Keep subagents behind a small runtime interface. Check capabilities before start. Use one child inbox and direct-parent authority. Do not add a provider plugin system, remote backends, or durable activation cache in the first release.

## 8. Pi Subagents

### 8.1 Design

Pi Subagents is an extension for Pi. It starts focused child Pi sessions. It supports fresh context and forked context.

The extension uses one large `subagent` tool. The tool can run agents and can also manage agents, workflows, missions, schedules, worktrees, fleet views, budgets, watchdogs, and external agent processes.

A foreground child streams progress in the parent conversation. A background child runs in a separate process and writes status and result files. The parent can inspect, steer, interrupt, stop, or resume work.

Pi Subagents has strong output control:

- Output truncation
- Saved output files
- Child transcripts
- Structured output
- Run metadata
- Tool-call budgets
- Token and cost budgets
- Per-tool timeouts
- Run timeouts
- Startup retries
- Model fallback

It also has a fleet view. The fleet view shows active work and lets a user open a child transcript. Machine-readable lifecycle files support other tools.

Pi supports workflow scripts for serial work, parallel work, and several ordered lanes. It can use managed Git worktrees for isolated write agents. It can also call Codex, Claude Code, and Cursor as external child processes.

The extension limits active parallel work, tree depth, and total child spawns. It has a recursion guard and a separate session-wide spawn budget.

See the [README](https://github.com/nicobailon/pi-subagents/blob/4b30e4b55761313fd7e467c9a8effa95db3f93b6/README.md), [tool reference](https://github.com/nicobailon/pi-subagents/blob/4b30e4b55761313fd7e467c9a8effa95db3f93b6/docs/tool-reference.md), [observability document](https://github.com/nicobailon/pi-subagents/blob/4b30e4b55761313fd7e467c9a8effa95db3f93b6/docs/observability.md), and [background runner](https://github.com/nicobailon/pi-subagents/blob/4b30e4b55761313fd7e467c9a8effa95db3f93b6/src/runs/background/subagent-runner.ts).

### 8.2 Strong points

- It has the most complete operations controls.
- It has strong output, budget, timeout, and retry rules.
- It makes background work visible.
- Worktree support helps parallel write work.
- Workflow scripts support repeatable large tasks.
- It can use several external coding agents.
- It has extensive tests for failure paths.

### 8.3 Weak points

- One tool has too many actions and parameters.
- The extension has a very large state and file surface.
- Background process control has many operating-system failure cases.
- Workflows, missions, schedules, watchdogs, and adapters are separate products.
- A direct port would make KISS much larger and harder to maintain.

### 8.4 Lesson for KISS

Add output limits, timeouts, useful status events, and worktree isolation in later releases. Do not start with the large single tool, workflow language, mission store, schedule service, watchdog, or external agent adapters.

## 9. Comparative result

### 9.1 Best design by area

- Best native coordination: OpenAI Codex
- Best small child-session core: OpenCode
- Best capability boundary: DeepSeek Harness
- Best agent definitions and context choice: Claude Code
- Best operations and workflow controls: Pi Subagents
- Best direct source match for KISS: OpenAI Codex
- Best first implementation shape for KISS: OpenCode with selected Codex and DeepSeek rules

### 9.2 KISS selection score

I scored each design from zero to five in five areas. The areas are safety, lifecycle, context control, simplicity, and fit with KISS. A score of five is best. This score is only for the first KISS release. It is not a general product-quality score.

- OpenAI Codex: 22 of 25. Safety 5, lifecycle 5, context 5, simplicity 2, KISS fit 5.
- OpenCode: 21 of 25. Safety 4, lifecycle 4, context 3, simplicity 5, KISS fit 5.
- Claude Code: 19 of 25. Safety 4, lifecycle 4, context 5, simplicity 3, KISS fit 3.
- DeepSeek Harness: 17 of 25. Safety 5, lifecycle 5, context 4, simplicity 1, KISS fit 2.
- Pi Subagents: 17 of 25. Safety 4, lifecycle 5, context 5, simplicity 1, KISS fit 2.

OpenCode is close to Codex because it has a small implementation shape. Codex wins because it has stronger control, context, and lifecycle rules. DeepSeek and Pi lose points only because their complete systems are much larger than KISS needs.

### 9.3 Overall order for the KISS use case

1. OpenAI Codex
2. OpenCode
3. Claude Code
4. DeepSeek Harness
5. Pi Subagents

This order measures fit with KISS. It does not measure the total number of features.

Codex is first because it has a strong Rust-native design and complete coordination rules. OpenCode is second because its core is much smaller and maps well to the current KISS session code. Claude Code is third because its agent definitions and context modes are excellent, but its total runtime has more feature branches. DeepSeek is fourth because its contracts are excellent but its service system is too large. Pi is fifth because it solves many problems that KISS does not need in its first subagent release.

## 10. Current KISS fit

KISS already has most of the required base parts.

### 10.1 Existing parts to reuse

`crates/kiss-agent/src/agent_loop.rs` already supports a full independent agent run. A child can use the same `run_agent_loop` function.

`crates/kiss-agent/src/config.rs` separates `AgentContext` from `AgentLoopConfig`. This is useful for child context and model settings.

`crates/kiss-agent/src/tool.rs` has an asynchronous `AgentTool` contract. A subagent control tool can start child work without a new core-loop feature.

`crates/kiss-coding/src/session/manager.rs` already has normal sessions, sibling sessions, session forks, persistent JSON Lines files, and parent-session metadata.

`crates/kiss-coding/src/session_runner.rs` already owns model selection, tools, settings, cancellation, message queues, compaction, and session storage. It also has `run_ephemeral`, which proves that KISS can run an isolated model task with a separate context.

`crates/kiss-coding/src/session/entry.rs` has `Custom` and `CustomMessage` entries. KISS can use these entries for child metadata and synthetic completion messages without changing provider messages.

`crates/kiss/src/modes/interactive.rs` already saves global settings from the `/settings` picker and updates the active `AgentSession`.

### 10.2 Missing parts

KISS does not have a child runtime manager. It needs one owner for child handles, status, result, cancellation token, parent ID, and concurrency permits.

KISS does not have a child mailbox. It needs a bounded wait notification and a follow-up queue for each child.

KISS does not have a safe context-fork function for an active model turn. It must copy only complete stored messages. It must not copy a partial assistant response or unresolved tool call.

KISS does not have per-agent tool permissions. The first release must at least enforce a strict child subset of the parent tools.

KISS does not have child lifecycle events in `SessionEvent`. The terminal interface needs small start, status, and completion notices.

KISS does not rebuild the active tool list when a simple setting changes. The `/settings` subagent switch must update tool exposure for the next model call.

## 11. Recommended KISS architecture

### 11.1 Product rule

Add this global setting:

```json
{
  "subagents": {
    "enabled": false
  }
}
```

Show `Subagents` in `/settings`. Its values are `off` and `on`. The default is `off`.

When the value is off, do not show subagent tools to the model. Do not add subagent instructions to the system prompt.

When the user turns the value on, expose the tools on the next model call. Do not require a process restart.

When the user turns the value off while children are active, reject new child starts. Keep control tools available until the active children stop. Then remove all subagent tools.

Project settings can enable the feature only for a trusted project. This follows the current trusted project-settings rule.

### 11.2 Runtime boundary

Add a `SubagentRuntime` in `kiss-coding`. Keep it outside `kiss-agent`.

`kiss-agent` must continue to know only about agent loops and tools. It must not know about parent-child sessions.

The runtime must own:

- Child records
- Parent and child IDs
- Child status
- Child cancellation tokens
- Child message queues
- A concurrency semaphore
- Child task handles
- Completion notifications

Use one record per child. Use these first-release states:

- `queued`
- `running`
- `completed`
- `failed`
- `interrupted`

Do not add a second detailed execution state machine. The agent loop remains the authority for turn execution.

### 11.3 Tool surface

Use separate tools. Do not use one large action tool.

Start with these tools:

1. `spawn_agent`

   Inputs: `task_name`, `message`, optional `agent_type`, and optional `context`.

   `context` supports `fresh` and `fork`. The default is `fresh`.

   The call returns the child ID and task name after the runtime accepts the child. It does not wait for completion.

2. `wait_agent`

   Inputs: optional child IDs and optional timeout.

   The call waits for a child status change or completion notice. Use minimum and maximum timeouts. Do not permit fast repeated polling.

3. `send_agent`

   Inputs: child ID and message.

   If the child is idle, start a new child turn. If it is running, queue the message for the next safe turn boundary.

4. `list_agents`

   Return ID, task name, parent ID, depth, state, and a short final result when one exists.

5. `interrupt_agent`

   Cancel the active child turn. Keep stored child history so that a later message can resume it.

This surface has the strong parts of Codex, but it is smaller. A separate `followup_task` and `send_message` split can come later if one `send_agent` operation becomes unclear.

### 11.4 Child identity and session storage

Use the child session ID as the child agent ID. Do not create a second public ID.

Create a separate `SessionManager` for each child. Set its parent session reference. Add one `Custom` entry with this data:

- Parent session ID
- Task name
- Agent type
- Depth
- Context mode
- Creation time
- Final state

Do not put lifecycle metadata into normal model messages.

Keep the child transcript in its child session file. Return only a short child result to the parent context.

### 11.5 Context rules

Use `fresh` by default.

A fresh child gets:

- The current KISS base system prompt
- Project context files
- Its agent-specific prompt
- The delegated task
- Its allowed tools
- The current working directory

It does not get the parent transcript.

An explicit `fork` child gets a snapshot of complete parent context at spawn time. Use `SessionManager::build_session_context()` as the source. Do not read the live partial assistant message from the terminal stream.

Before the child starts, remove or replace these items from the fork:

- Old subagent role instructions
- Old child completion notices
- Any unresolved tool call
- Any partial assistant message

KISS stores complete assistant and tool-result messages after a turn. This makes a safe snapshot simpler than the Codex rollout filter.

Do not implement `last N turns` in the first release. Add it only after fresh and full fork behavior have tests.

### 11.6 Tool and authority rules

A child tool set must be a subset of the parent tool set.

The child must never get a tool that the parent does not have. A child agent definition can remove tools. It cannot add tools.

At the depth limit, remove all subagent start tools from the child. Keep list, wait, send, and interrupt tools only when the child owns descendants.

The direct parent owns its child. Another agent cannot send, wait for, or interrupt that child unless it is an ancestor and the runtime explicitly allows the operation.

For the first release, require the direct parent for send and interrupt. This is easier to test and safer.

### 11.7 Limits

Use safe fixed defaults in the first release:

- Maximum active child turns per root session: 4
- Maximum nesting depth: 1
- Maximum created children per root session: 16
- Default wait: 30 seconds
- Minimum wait: 250 milliseconds
- Maximum wait: 10 minutes
- Default child run timeout: 30 minutes

The parent agent does not count as a child turn. A semaphore controls active child turns.

Make the limits internal constants first. Add more `/settings` values only after real use shows that users need them.

### 11.8 Result delivery

The runtime must deliver each terminal child result exactly once.

Store the result in the child record and child session. Then add one synthetic `CustomMessage` to the parent. The message must include:

- Child ID
- Task name
- Final state
- Short final answer or error

Wake an idle parent after the message is durable. If the parent is running, add the notice at the next turn boundary through the existing follow-up queue.

Do not copy the child tool transcript into the parent. This protects the parent context from large and noisy output.

### 11.9 Parallel file changes

All current KISS tools use the same working directory. Two write agents can edit the same file at the same time.

The first release must make this risk clear in the system instructions. The parent must give non-overlapping tasks to parallel write agents.

Add Git worktree isolation in a later release. Until then, the safest built-in agents are read-heavy agents such as `explore` and `reviewer`.

Do not hide this risk with last-write-wins behavior.

### 11.10 Agent definitions

The first release can ship these built-in agent types:

- `general`: inherits the allowed parent tools
- `explore`: read-only code search and analysis
- `reviewer`: read-only review and test analysis

After the runtime is stable, add Markdown agent files in these locations:

- Global: `~/.kiss/agent/agents/*.md`
- Project: `.kiss/agents/*.md`

Use a small front matter:

- `description`
- `model`
- `thinking`
- `tools`
- `context`
- `maxTurns`

The Markdown body is the agent system prompt. Project agent files load only for trusted projects.

Do not add permissions, hooks, memory, skills, MCP requirements, worktrees, and background defaults to the first file format. Add a field only when the runtime can enforce it.

### 11.11 Events and terminal interface

Add these `SessionEvent` variants:

- `SubagentStarted`
- `SubagentStatusChanged`
- `SubagentMessageQueued`
- `SubagentCompleted`

Show one short terminal line for each state change. Do not stream all child tool events into the parent transcript.

A later `/agents` view can show the task tree and child transcript. This is useful, but it is not required for the first release.

### 11.12 Failure and cleanup rules

Start must have all-or-nothing behavior. If child setup fails, remove the incomplete child record and release the parallel-work permit.

Cancellation must use one `CancellationToken` per child turn.

When KISS exits, cancel active child turns and wait for a short bounded cleanup period. Keep completed child session files.

A child panic or provider error must change the child to `failed` and notify the parent. It must not stop the parent agent loop.

If the parent session closes, cancel its active children from the leaves to the root.

An unknown child ID must return a clear tool error. A repeated interrupt on a completed child can return the current state without an error.

## 12. Recommended delivery order

The implementation must use an ExecPlan as required by `.agent/PLANS.md`.

Use these milestones when implementation starts:

1. Add the disabled-by-default setting and dynamic tool exposure.
2. Add `SubagentRuntime`, child records, concurrency limits, and lifecycle tests.
3. Add a fresh-context child session and `spawn_agent` plus `wait_agent`.
4. Add exactly-once completion delivery to the parent.
5. Add `list_agents`, `send_agent`, and `interrupt_agent`.
6. Add the full context fork and context-safety tests.
7. Add built-in `explore` and `reviewer` agent types.
8. Add terminal status events and end-to-end tests.
9. Add Markdown agent files in a later change.
10. Add worktree isolation only after the shared-workspace version is stable.

## 13. Features to defer

Do not include these features in the first implementation:

- Agent teams
- Peer-to-peer child messages
- External Codex, Claude Code, or Cursor process adapters
- Workflow scripts
- Missions and schedules
- Child resident-memory cache
- Remote child backends
- Structured child output
- Token and cost budgets
- Automatic model fallback
- Watchdog reviewers
- Git worktree creation and merge
- More than one context-fork mode
- Proactive automatic delegation

These features are useful in larger products. They are not necessary to prove the KISS subagent core.

## 14. Acceptance rules for the future implementation

The first implementation is complete only when all these statements are true:

1. A new KISS install does not expose subagent tools.
2. A user can enable subagents in `/settings` without a restart.
3. The next model call sees the subagent tools and the subagent usage instructions.
4. A parent can start two independent read-only children in parallel.
5. Each child uses a separate KISS session and agent loop.
6. A fresh child does not receive the parent transcript.
7. A fork child receives only a complete and valid parent context snapshot.
8. A child cannot get more tools than its parent.
9. The runtime rejects a child above the depth or count limit.
10. The parent receives each child completion one time.
11. The parent can wait without fast repeated polling.
12. The parent can list, message, and interrupt its child.
13. A child failure does not fail the parent loop.
14. Active children stop during parent shutdown.
15. Turning the setting off prevents new children and keeps control of active children until they stop.

## 15. Final recommendation

Build a small native subagent runtime in `kiss-coding`.

Use normal KISS child sessions and the existing KISS agent loop. Use fresh context by default. Make a full context fork explicit. Use a stable task tree, direct-parent authority, a strict child tool subset, bounded parallel work, exactly-once result delivery, and separate control tools.

Keep the feature off by default behind `/settings`.

This design gets the strongest ideas from the five reviewed harnesses. It also keeps the KISS core small enough to understand, test, and maintain.
