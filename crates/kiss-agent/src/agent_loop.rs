//! The agent loop: streams assistant turns, executes tool batches, injects
//! steering messages, drains follow-ups, and emits events throughout.

use crate::config::{AgentContext, AgentLoopConfig, TurnInfo};
use crate::events::AgentEvent;
use crate::message::AgentMessage;
use crate::tool::{DynTool, ExecutionMode, ToolResult, ToolUpdateSink};
use crate::validate::validate_arguments;
use kiss_ai::{
    AssistantEvent, AssistantMessage, ContentBlock, StopReason, StreamOptions, ToolCall,
    ToolResultMessage,
};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Async event sink; awaited so persistence keeps ordering guarantees.
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Run the loop with new prompt messages appended to the context.
pub async fn run_agent_loop(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    cancel: CancellationToken,
    emit: EventSink,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for prompt in &prompts {
        emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        });
        emit(AgentEvent::MessageEnd {
            message: prompt.clone(),
        });
    }

    run_loop(
        &mut context,
        &mut new_messages,
        &mut config,
        cancel,
        &emit,
        true,
    )
    .await;
    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
    new_messages
}

/// Continue from existing context (retry after error, resume). The last
/// message must convert to a user or tool-result message.
pub async fn run_agent_loop_continue(
    mut context: AgentContext,
    mut config: AgentLoopConfig,
    cancel: CancellationToken,
    emit: EventSink,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = Vec::new();
    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    run_loop(
        &mut context,
        &mut new_messages,
        &mut config,
        cancel,
        &emit,
        true,
    )
    .await;
    emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
    new_messages
}

async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &mut AgentLoopConfig,
    cancel: CancellationToken,
    emit: &EventSink,
    mut first_turn: bool,
) {
    // The active prompt always gets the first turn. Steering is read after
    // that turn and is then inserted before the next model call.
    let mut pending: Vec<AgentMessage> = Vec::new();

    // Outer loop: continues when follow-up messages arrive after the agent
    // would otherwise stop.
    'outer: loop {
        let mut has_more_tool_calls = true;

        // Inner loop: turns while tool calls or steering messages remain.
        while has_more_tool_calls || !pending.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart);
            } else {
                first_turn = false;
            }

            for message in pending.drain(..) {
                emit(AgentEvent::MessageStart {
                    message: message.clone(),
                });
                emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                });
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let assistant = stream_assistant(context, config, cancel.clone(), emit).await;
            new_messages.push(AgentMessage::Assistant(assistant.clone()));

            if matches!(
                assistant.stop_reason,
                StopReason::Error | StopReason::Aborted
            ) {
                emit(AgentEvent::TurnEnd {
                    message: AgentMessage::Assistant(assistant),
                    tool_results: Vec::new(),
                });
                return;
            }

            let tool_calls: Vec<ToolCall> = assistant.tool_calls().cloned().collect();
            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                let batch = if assistant.stop_reason == StopReason::Length {
                    fail_truncated_batch(&tool_calls, emit)
                } else {
                    execute_tool_batch(context, &tool_calls, config, cancel.clone(), emit).await
                };
                has_more_tool_calls = !batch.terminate;
                for result in batch.messages {
                    context
                        .messages
                        .push(AgentMessage::ToolResult(result.clone()));
                    new_messages.push(AgentMessage::ToolResult(result.clone()));
                    tool_results.push(result);
                }
            }

            emit(AgentEvent::TurnEnd {
                message: AgentMessage::Assistant(assistant.clone()),
                tool_results: tool_results.clone(),
            });

            let assistant_message = AgentMessage::Assistant(assistant);
            if let Some(prepare) = &config.prepare_next_turn {
                let info = TurnInfo {
                    message: &assistant_message,
                    tool_results: &tool_results,
                    messages: new_messages,
                };
                if let Some(update) = prepare(&info).await {
                    if let Some(ctx) = update.context {
                        *context = ctx;
                    }
                    if let Some(model) = update.model {
                        config.model = model;
                    }
                    if let Some(level) = update.thinking_level {
                        config.thinking_level = level;
                    }
                }
            }

            if let Some(should_stop) = &config.should_stop_after_turn {
                let info = TurnInfo {
                    message: &assistant_message,
                    tool_results: &tool_results,
                    messages: new_messages,
                };
                if should_stop(&info).await {
                    return;
                }
            }

            pending = match &config.get_steering_messages {
                Some(f) => f().await,
                None => Vec::new(),
            };
        }

        // Agent would stop: check follow-ups.
        if let Some(f) = &config.get_follow_up_messages {
            let follow_ups = f().await;
            if !follow_ups.is_empty() {
                pending = follow_ups;
                continue 'outer;
            }
        }
        break;
    }
}

/// Stream one assistant response, emitting message events and mutating the
/// context (partial message inserted at start, replaced on updates).
async fn stream_assistant(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    emit: &EventSink,
) -> AssistantMessage {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages).await;
    }
    let llm_messages = (config.convert_to_llm)(&messages);
    let llm_context = kiss_ai::Context {
        system_prompt: Some(context.system_prompt.clone()),
        openai_responses_input: context.openai_responses_input.clone(),
        messages: llm_messages,
        tools: context.tools.iter().map(|t| t.to_def()).collect(),
    };
    let api_key = match &config.get_api_key {
        Some(resolve) => resolve(config.model.provider.clone()).await,
        None => None,
    };
    let options = StreamOptions {
        api_key,
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        reasoning: config.thinking_level,
        session_id: config.session_id.clone(),
        transport: config.transport,
        cancel,
    };

    let mut stream = (config.stream_fn)(&config.model, &llm_context, &options);
    let mut added_partial = false;

    while let Some(event) = stream.next().await {
        match &event {
            AssistantEvent::Start { partial } => {
                context
                    .messages
                    .push(AgentMessage::Assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart {
                    message: AgentMessage::Assistant(partial.clone()),
                });
            }
            AssistantEvent::Done { message, .. } | AssistantEvent::Error { message, .. } => {
                if added_partial {
                    *context.messages.last_mut().unwrap() =
                        AgentMessage::Assistant(message.clone());
                } else {
                    context
                        .messages
                        .push(AgentMessage::Assistant(message.clone()));
                    emit(AgentEvent::MessageStart {
                        message: AgentMessage::Assistant(message.clone()),
                    });
                }
                emit(AgentEvent::MessageEnd {
                    message: AgentMessage::Assistant(message.clone()),
                });
                return message.clone();
            }
            other => {
                emit(AgentEvent::MessageUpdate {
                    assistant_event: Box::new(other.clone()),
                });
            }
        }
    }
    // Stream dropped without terminal event: synthesize an error message.
    let mut m =
        AssistantMessage::empty(&config.model.api, &config.model.provider, &config.model.id);
    m.stop_reason = StopReason::Error;
    m.error_message = Some("provider stream ended unexpectedly".into());
    if added_partial {
        *context.messages.last_mut().unwrap() = AgentMessage::Assistant(m.clone());
    } else {
        context.messages.push(AgentMessage::Assistant(m.clone()));
        emit(AgentEvent::MessageStart {
            message: AgentMessage::Assistant(m.clone()),
        });
    }
    emit(AgentEvent::MessageEnd {
        message: AgentMessage::Assistant(m.clone()),
    });
    m
}

struct ExecutedBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct FinalizedCall {
    tool_call: ToolCall,
    result: ToolResult,
    is_error: bool,
}

/// A `length` stop means arguments may be silently truncated; fail them all.
fn fail_truncated_batch(tool_calls: &[ToolCall], emit: &EventSink) -> ExecutedBatch {
    let mut messages = Vec::new();
    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.arguments.clone(),
        });
        let result = ToolResult::text(format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            tc.name
        ));
        emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            result: result.clone(),
            is_error: true,
        });
        let message = tool_result_message(tc, &result, true);
        emit(AgentEvent::MessageStart {
            message: AgentMessage::ToolResult(message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: AgentMessage::ToolResult(message.clone()),
        });
        messages.push(message);
    }
    ExecutedBatch {
        messages,
        terminate: false,
    }
}

async fn execute_tool_batch(
    context: &AgentContext,
    tool_calls: &[ToolCall],
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    emit: &EventSink,
) -> ExecutedBatch {
    let force_sequential = tool_calls.iter().any(|tc| {
        context
            .find_tool(&tc.name)
            .is_some_and(|t| t.execution_mode() == ExecutionMode::Sequential)
    });
    let sequential = config.tool_execution == ExecutionMode::Sequential || force_sequential;

    enum Prepared {
        Immediate(FinalizedCall),
        Run {
            tool: DynTool,
            tool_call: ToolCall,
            args: Value,
        },
    }

    // Preflight sequentially in source order.
    let mut prepared: Vec<Prepared> = Vec::new();
    for tc in tool_calls {
        emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.arguments.clone(),
        });
        let outcome = prepare_tool_call(context, tc, config, &cancel).await;
        prepared.push(match outcome {
            Ok((tool, args)) => Prepared::Run {
                tool,
                tool_call: tc.clone(),
                args,
            },
            Err(finalized) => Prepared::Immediate(*finalized),
        });
        if cancel.is_cancelled() {
            break;
        }
    }

    let mut finalized: Vec<FinalizedCall> = Vec::new();
    if sequential {
        for p in prepared {
            match p {
                Prepared::Immediate(f) => {
                    emit_execution_end(&f, emit);
                    finalized.push(f);
                }
                Prepared::Run {
                    tool,
                    tool_call,
                    args,
                } => {
                    let f =
                        run_and_finalize(tool, tool_call, args, config, cancel.clone(), emit).await;
                    emit_execution_end(&f, emit);
                    finalized.push(f);
                }
            }
            if cancel.is_cancelled() {
                break;
            }
        }
    } else {
        // Execute concurrently; ToolExecutionEnd fires per-completion, but
        // result messages are appended in source order afterwards.
        let mut handles: Vec<ParallelEntry> = Vec::new();
        enum ParallelEntry {
            Ready(Box<FinalizedCall>),
            Pending(tokio::task::JoinHandle<FinalizedCall>),
        }
        for p in prepared {
            match p {
                Prepared::Immediate(f) => {
                    emit_execution_end(&f, emit);
                    handles.push(ParallelEntry::Ready(Box::new(f)));
                }
                Prepared::Run {
                    tool,
                    tool_call,
                    args,
                } => {
                    let config = config.clone();
                    let cancel = cancel.clone();
                    let emit = emit.clone();
                    handles.push(ParallelEntry::Pending(tokio::spawn(async move {
                        let f =
                            run_and_finalize(tool, tool_call, args, &config, cancel, &emit).await;
                        emit_execution_end(&f, &emit);
                        f
                    })));
                }
            }
        }
        for entry in handles {
            match entry {
                ParallelEntry::Ready(f) => finalized.push(*f),
                ParallelEntry::Pending(handle) => match handle.await {
                    Ok(f) => finalized.push(f),
                    Err(join_err) => {
                        // A panicking tool must not kill the loop.
                        finalized.push(FinalizedCall {
                            tool_call: ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: Value::Null,
                                thought_signature: None,
                            },
                            result: ToolResult::text(format!("tool task failed: {join_err}")),
                            is_error: true,
                        });
                    }
                },
            }
        }
    }

    let terminate = !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate);
    let mut messages = Vec::new();
    for f in &finalized {
        if f.tool_call.id.is_empty() {
            continue;
        }
        let message = tool_result_message(&f.tool_call, &f.result, f.is_error);
        emit(AgentEvent::MessageStart {
            message: AgentMessage::ToolResult(message.clone()),
        });
        emit(AgentEvent::MessageEnd {
            message: AgentMessage::ToolResult(message.clone()),
        });
        messages.push(message);
    }
    ExecutedBatch {
        messages,
        terminate,
    }
}

async fn prepare_tool_call(
    context: &AgentContext,
    tc: &ToolCall,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> Result<(DynTool, Value), Box<FinalizedCall>> {
    let error = |text: String, terminate: bool| {
        Box::new(FinalizedCall {
            tool_call: tc.clone(),
            result: ToolResult {
                terminate,
                ..ToolResult::text(text)
            },
            is_error: true,
        })
    };
    let Some(tool) = context.find_tool(&tc.name) else {
        return Err(error(format!("Tool {} not found", tc.name), false));
    };
    let args = tool.prepare_arguments(tc.arguments.clone());
    if let Err(msg) = validate_arguments(&tool.parameters(), &args) {
        return Err(error(msg, false));
    }
    if let Some(before) = &config.before_tool_call
        && let Some(outcome) = before(&tc.name, &args).await
    {
        if cancel.is_cancelled() {
            return Err(error("Operation aborted".into(), false));
        }
        if outcome.block {
            return Err(error(
                outcome
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".into()),
                outcome.terminate,
            ));
        }
    }
    if cancel.is_cancelled() {
        return Err(error("Operation aborted".into(), false));
    }
    Ok((tool.clone(), args))
}

async fn run_and_finalize(
    tool: DynTool,
    tool_call: ToolCall,
    args: Value,
    config: &AgentLoopConfig,
    cancel: CancellationToken,
    emit: &EventSink,
) -> FinalizedCall {
    let update_sink: ToolUpdateSink = {
        let emit = emit.clone();
        let id = tool_call.id.clone();
        let name = tool_call.name.clone();
        let args = args.clone();
        Arc::new(move |partial: ToolResult| {
            emit(AgentEvent::ToolExecutionUpdate {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: args.clone(),
                partial,
            });
        })
    };

    let executed = tool
        .execute(
            &tool_call.id,
            args.clone(),
            cancel.clone(),
            Some(update_sink),
        )
        .await;
    let (mut result, mut is_error) = match executed {
        Ok(r) => (r, false),
        Err(e) => (ToolResult::text(format!("{e:#}")), true),
    };

    if let Some(after) = &config.after_tool_call
        && let Some(over) = after(&tool_call.name, &args, &result, is_error).await
    {
        if let Some(content) = over.content {
            result.content = content;
        }
        if let Some(details) = over.details {
            result.details = details;
        }
        if let Some(usage) = over.usage {
            result.usage = Some(usage);
        }
        if let Some(terminate) = over.terminate {
            result.terminate = terminate;
        }
        if let Some(err) = over.is_error {
            is_error = err;
        }
    }

    FinalizedCall {
        tool_call,
        result,
        is_error,
    }
}

fn emit_execution_end(f: &FinalizedCall, emit: &EventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: f.tool_call.id.clone(),
        tool_name: f.tool_call.name.clone(),
        result: f.result.clone(),
        is_error: f.is_error,
    });
}

fn tool_result_message(tc: &ToolCall, result: &ToolResult, is_error: bool) -> ToolResultMessage {
    let content = if result.content.is_empty() {
        vec![ContentBlock::text("")]
    } else {
        result.content.clone()
    };
    ToolResultMessage {
        tool_call_id: tc.id.clone(),
        tool_name: tc.name.clone(),
        content,
        details: if result.details.is_null() {
            None
        } else {
            Some(result.details.clone())
        },
        usage: result.usage,
        is_error,
        timestamp: kiss_ai::now_ms(),
    }
}
