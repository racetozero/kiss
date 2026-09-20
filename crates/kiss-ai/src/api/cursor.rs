//! Cursor Agent provider over native HTTP/2 Connect and Protobuf.

use super::PartialBuilder;
use super::cursor_protocol as wire;
use crate::event::EventSink;
use crate::model::{Model, ModelCost};
use crate::stream::StreamOptions;
use crate::types::{ContentBlock, Context, Message, StopReason, ToolDef, ToolResultMessage};
use anyhow::{Context as _, Result};
use base64::Engine as _;
use futures::{StreamExt as _, stream};
use prost::Message as _;
use prost_types::{ListValue, Struct, Value, value};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

const PROVIDER: &str = "cursor";
const API: &str = "cursor-agent";
const DEFAULT_URL: &str = "https://agentn.us.api5.cursor.sh";
const DEFAULT_CLIENT_VERSION: &str = "cli-2026.07.23-e383d2b";
const RUN_PATH: &str = "/agent.v1.AgentService/Run";
const MODELS_PATH: &str = "/agent.v1.AgentService/GetUsableModels";
const RESUME_TIMEOUT: Duration = Duration::from_secs(60 * 60);

type RequestSender = mpsc::UnboundedSender<std::result::Result<Vec<u8>, io::Error>>;

struct Resume {
    context: Context,
    options: StreamOptions,
    sink: EventSink,
}

fn pending() -> &'static Mutex<HashMap<String, oneshot::Sender<Resume>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, oneshot::Sender<Resume>>>> = OnceLock::new();
    PENDING.get_or_init(Default::default)
}

/// Stream one Cursor turn. A call after a KISS tool result resumes the saved
/// HTTP/2 request instead of opening a second request.
pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    if let Some(sender) = take_resume(context, options) {
        if sender
            .send(Resume {
                context: context.clone(),
                options: options.clone(),
                sink: sink.clone(),
            })
            .is_err()
        {
            PartialBuilder::new(model, sink).fail(
                "Cursor tool continuation ended before KISS returned its result",
                false,
                model,
            );
        }
        return;
    }

    let Some(token) = options.credential.as_ref().map(|value| value.value()) else {
        PartialBuilder::new(model, sink).fail(
            "Cursor authentication is required. Run `kiss login cursor` or set CURSOR_ACCESS_TOKEN",
            false,
            model,
        );
        return;
    };

    if let Err(error) = run(model, context, options, sink.clone(), token).await {
        PartialBuilder::new(model, sink).fail(
            format!("Cursor request failed: {error:#}"),
            false,
            model,
        );
    }
}

fn take_resume(context: &Context, options: &StreamOptions) -> Option<oneshot::Sender<Resume>> {
    let response_id = context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(message) if message.provider == PROVIDER => {
                message.response_id.clone()
            }
            _ => None,
        });
    let mut pending = pending().lock().unwrap();
    options
        .session_id
        .as_deref()
        .and_then(|key| pending.remove(key))
        .or_else(|| response_id.as_deref().and_then(|key| pending.remove(key)))
}

async fn run(
    model: &Model,
    context: &Context,
    initial_options: &StreamOptions,
    sink: EventSink,
    token: &str,
) -> Result<()> {
    let built = build_request(model, context, initial_options)?;
    let response_id = format!("cursor:{}", Uuid::new_v4());
    let resume_key = initial_options
        .session_id
        .clone()
        .unwrap_or_else(|| response_id.clone());
    let (request_tx, request_rx) = mpsc::unbounded_channel();
    request_tx
        .send(Ok(wire::frame(&built.message)))
        .map_err(|_| anyhow::anyhow!("could not start Cursor request body"))?;
    let body_stream = stream::unfold(request_rx, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    });
    let body = reqwest::Body::wrap_stream(body_stream);
    let url = format!("{}{}", cursor_url(&model.base_url), RUN_PATH);
    let request = cursor_request(crate::stream::http_client().post(&url), token, true).body(body);
    let response = tokio::select! {
        response = request.send() => response.with_context(|| format!("connect to {url}"))?,
        _ = initial_options.cancel.cancelled() => anyhow::bail!("request cancelled"),
    };
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {status}: {}", crate::truncate_err(&body));
    }

    let mut response_stream = response.bytes_stream();
    let mut decoder = wire::FrameDecoder::default();
    let mut blobs = built.blobs;
    let mut options = initial_options.clone();
    let mut builder = Some(new_builder(model, sink, &response_id));
    let mut text_index = None;
    let mut thinking_index = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let drive_result: Result<()> = async {
    loop {
        let chunk = tokio::select! {
            chunk = response_stream.next() => chunk,
            _ = heartbeat.tick() => {
                send(&request_tx, wire::AgentClientMessage {
                    message: Some(wire::agent_client_message::Message::ClientHeartbeat(
                        wire::ClientHeartbeat {},
                    )),
                })?;
                continue;
            }
            _ = options.cancel.cancelled() => anyhow::bail!("request cancelled"),
        };
        let Some(chunk) = chunk else {
            let current = builder.as_mut().expect("active Cursor event stream");
            close_blocks(current, &mut text_index, &mut thinking_index);
            builder.take().unwrap().finish(StopReason::Stop, model);
            return Ok(());
        };
        let chunk = chunk.context("read Cursor response")?;
        for (flags, payload) in decoder.push(&chunk)? {
            if flags & wire::END_STREAM_FLAG != 0 {
                let detail = String::from_utf8_lossy(&payload);
                if serde_json::from_slice::<serde_json::Value>(&payload)
                    .is_ok_and(|value| value.get("error").is_none())
                {
                    let current = builder.as_mut().expect("active Cursor event stream");
                    close_blocks(current, &mut text_index, &mut thinking_index);
                    builder.take().unwrap().finish(StopReason::Stop, model);
                    return Ok(());
                }
                anyhow::bail!("stream ended: {}", crate::truncate_err(&detail));
            }
            let message = wire::AgentServerMessage::decode(payload.as_slice())
                .context("decode Cursor server message")?;
            match message.message {
                Some(wire::agent_server_message::Message::InteractionUpdate(update)) => {
                    match update.message {
                        Some(wire::interaction_update::Message::TextDelta(delta)) => {
                            let current = builder.as_mut().expect("active Cursor event stream");
                            if thinking_index.is_some() {
                                end_thinking(current, &mut thinking_index);
                            }
                            let index = *text_index.get_or_insert_with(|| current.begin_text());
                            current.append_text(index, &delta.text);
                        }
                        Some(wire::interaction_update::Message::ThinkingDelta(delta)) => {
                            let current = builder.as_mut().expect("active Cursor event stream");
                            if text_index.is_some() {
                                end_text(current, &mut text_index);
                            }
                            let index =
                                *thinking_index.get_or_insert_with(|| current.begin_thinking());
                            current.append_thinking(index, &delta.text);
                        }
                        Some(wire::interaction_update::Message::TokenDelta(delta)) => {
                            let current = builder.as_mut().expect("active Cursor event stream");
                            current.message.usage.output = current
                                .message
                                .usage
                                .output
                                .saturating_add(delta.tokens.max(0) as u64);
                        }
                        Some(wire::interaction_update::Message::TurnEnded(_)) => {
                            let current = builder.as_mut().expect("active Cursor event stream");
                            close_blocks(current, &mut text_index, &mut thinking_index);
                            builder.take().unwrap().finish(StopReason::Stop, model);
                            return Ok(());
                        }
                        None => {}
                    }
                }
                Some(wire::agent_server_message::Message::KvServerMessage(message)) => {
                    handle_kv(message, &mut blobs, &request_tx)?;
                }
                Some(wire::agent_server_message::Message::ExecServerMessage(exec)) => {
                    let Some(wire::exec_server_message::Message::McpArgs(args)) = exec.message
                    else {
                        send_exec_throw(
                            &request_tx,
                            exec.id,
                            "KISS does not run Cursor-native tools",
                        )?;
                        continue;
                    };
                    let tool_name = strip_tool_prefix(if args.tool_name.is_empty() {
                        &args.name
                    } else {
                        &args.tool_name
                    });
                    if !context.tools.iter().any(|tool| tool.name == tool_name) {
                        send_exec_throw(
                            &request_tx,
                            exec.id,
                            &format!("unknown KISS tool: {tool_name}"),
                        )?;
                        continue;
                    }
                    let current = builder.as_mut().expect("active Cursor event stream");
                    close_blocks(current, &mut text_index, &mut thinking_index);
                    let arguments = decode_arguments(args.args)?;
                    let call_id = if args.tool_call_id.is_empty() {
                        Uuid::new_v4().to_string()
                    } else {
                        args.tool_call_id
                    };
                    let index = current.begin_tool_call(call_id.clone(), tool_name.to_string());
                    current.end_tool_call(index, Some(arguments));
                    let (resume_tx, resume_rx) = oneshot::channel();
                    pending()
                        .lock()
                        .unwrap()
                        .insert(resume_key.clone(), resume_tx);
                    builder.take().unwrap().finish(StopReason::ToolUse, model);
                    let resumed = tokio::select! {
                        result = resume_rx => result.context("Cursor tool continuation was dropped")?,
                        _ = tokio::time::sleep(RESUME_TIMEOUT) => {
                            pending().lock().unwrap().remove(&resume_key);
                            anyhow::bail!("Cursor tool continuation timed out")
                        }
                        _ = options.cancel.cancelled() => {
                            pending().lock().unwrap().remove(&resume_key);
                            anyhow::bail!("request cancelled")
                        }
                    };
                    options = resumed.options;
                    builder = Some(new_builder(model, resumed.sink, &response_id));
                    let result =
                        find_tool_result(&resumed.context, &call_id).with_context(|| {
                            format!("KISS returned no result for Cursor tool call {call_id}")
                        })?;
                    send_tool_result(&request_tx, exec.id, exec.exec_id, result)?;
                    text_index = None;
                    thinking_index = None;
                }
                Some(wire::agent_server_message::Message::InteractionQuery(query)) => {
                    anyhow::bail!("unsupported Cursor interaction query {}", query.id);
                }
                Some(wire::agent_server_message::Message::ConversationCheckpointUpdate(_))
                | None => {}
            }
        }
    }
    }.await;
    pending().lock().unwrap().remove(&resume_key);
    if let Err(error) = drive_result
        && let Some(builder) = builder
    {
        builder.fail(format!("Cursor request failed: {error:#}"), false, model);
    }
    Ok(())
}

fn new_builder(model: &Model, sink: EventSink, response_id: &str) -> PartialBuilder {
    let mut builder = PartialBuilder::new(model, sink);
    builder.message.response_id = Some(response_id.to_string());
    builder.start();
    builder
}

fn close_blocks(
    builder: &mut PartialBuilder,
    text_index: &mut Option<usize>,
    thinking_index: &mut Option<usize>,
) {
    end_text(builder, text_index);
    end_thinking(builder, thinking_index);
}

fn end_text(builder: &mut PartialBuilder, index: &mut Option<usize>) {
    if let Some(index) = index.take() {
        builder.end_text(index);
    }
}

fn end_thinking(builder: &mut PartialBuilder, index: &mut Option<usize>) {
    if let Some(index) = index.take() {
        builder.end_thinking(index);
    }
}

fn cursor_request(
    request: reqwest::RequestBuilder,
    token: &str,
    streaming: bool,
) -> reqwest::RequestBuilder {
    request
        .version(http::Version::HTTP_2)
        .header(
            "content-type",
            if streaming {
                "application/connect+proto"
            } else {
                "application/proto"
            },
        )
        .header("connect-protocol-version", "1")
        .header("authorization", format!("Bearer {token}"))
        .header("x-ghost-mode", "true")
        .header("x-cursor-client-type", "cli")
        .header(
            "x-cursor-client-version",
            std::env::var("KISS_CURSOR_CLIENT_VERSION")
                .unwrap_or_else(|_| DEFAULT_CLIENT_VERSION.into()),
        )
        .header("x-request-id", Uuid::new_v4().to_string())
}

fn cursor_url(model_url: &str) -> String {
    std::env::var("KISS_CURSOR_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if model_url.is_empty() {
                DEFAULT_URL.into()
            } else {
                model_url.into()
            }
        })
        .trim_end_matches('/')
        .to_string()
}

fn send(sender: &RequestSender, message: wire::AgentClientMessage) -> Result<()> {
    sender
        .send(Ok(wire::frame(&message)))
        .map_err(|_| anyhow::anyhow!("Cursor request body closed"))
}

fn send_exec_throw(sender: &RequestSender, id: u32, error: &str) -> Result<()> {
    send(
        sender,
        wire::AgentClientMessage {
            message: Some(
                wire::agent_client_message::Message::ExecClientControlMessage(
                    wire::ExecClientControlMessage {
                        message: Some(wire::exec_client_control_message::Message::Throw(
                            wire::ExecClientThrow {
                                id,
                                error: error.into(),
                            },
                        )),
                    },
                ),
            ),
        },
    )
}

fn handle_kv(
    message: wire::KvServerMessage,
    blobs: &mut HashMap<Vec<u8>, Vec<u8>>,
    sender: &RequestSender,
) -> Result<()> {
    use wire::kv_client_message::Message as Reply;
    use wire::kv_server_message::Message as Request;
    let reply = match message.message {
        Some(Request::GetBlobArgs(args)) => Reply::GetBlobResult(wire::GetBlobResult {
            blob_data: blobs.get(&args.blob_id).cloned(),
        }),
        Some(Request::SetBlobArgs(args)) => {
            blobs.insert(args.blob_id, args.blob_data);
            Reply::SetBlobResult(wire::SetBlobResult {})
        }
        None => return Ok(()),
    };
    send(
        sender,
        wire::AgentClientMessage {
            message: Some(wire::agent_client_message::Message::KvClientMessage(
                wire::KvClientMessage {
                    id: message.id,
                    message: Some(reply),
                },
            )),
        },
    )
}

fn send_tool_result(
    sender: &RequestSender,
    id: u32,
    exec_id: String,
    result: &ToolResultMessage,
) -> Result<()> {
    let mut content = Vec::new();
    for block in &result.content {
        let item = match block {
            ContentBlock::Text { text, .. } => {
                wire::mcp_tool_result_content_item::Content::Text(wire::McpTextContent {
                    text: text.clone(),
                })
            }
            ContentBlock::Image { data, mime_type } => {
                wire::mcp_tool_result_content_item::Content::Image(wire::McpImageContent {
                    data: base64::engine::general_purpose::STANDARD
                        .decode(data)
                        .unwrap_or_default(),
                    mime_type: mime_type.clone(),
                })
            }
            ContentBlock::Thinking { .. } | ContentBlock::ToolCall(_) => continue,
        };
        content.push(wire::McpToolResultContentItem {
            content: Some(item),
        });
    }
    if content.is_empty() {
        content.push(wire::McpToolResultContentItem {
            content: Some(wire::mcp_tool_result_content_item::Content::Text(
                wire::McpTextContent {
                    text: String::new(),
                },
            )),
        });
    }
    let reply = wire::mcp_result::Result::Success(wire::McpSuccess {
        content,
        is_error: result.is_error,
    });
    send(
        sender,
        wire::AgentClientMessage {
            message: Some(wire::agent_client_message::Message::ExecClientMessage(
                wire::ExecClientMessage {
                    id,
                    exec_id,
                    message: Some(wire::exec_client_message::Message::McpResult(
                        wire::McpResult {
                            result: Some(reply),
                        },
                    )),
                },
            )),
        },
    )
}

fn find_tool_result<'a>(context: &'a Context, call_id: &str) -> Option<&'a ToolResultMessage> {
    context
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::ToolResult(result) if result.tool_call_id == call_id => Some(result),
            _ => None,
        })
}

fn tool_result_text(result: &ToolResultMessage) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_tool_prefix(name: &str) -> &str {
    name.strip_prefix("mcp_kiss_").unwrap_or(name)
}

struct BuiltRequest {
    message: wire::AgentClientMessage,
    blobs: HashMap<Vec<u8>, Vec<u8>>,
}

fn build_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<BuiltRequest> {
    let mut blobs = HashMap::new();
    let (history, user_text, images) = split_context(context);
    let mut root_ids = Vec::new();
    if let Some(system) = context
        .system_prompt
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        root_ids.push(store_blob(
            serde_json::to_vec(&serde_json::json!({
                "role": "user",
                "content": [{"type": "text", "text": format!("<rules>\n{system}\n</rules>")}],
            }))?,
            &mut blobs,
        ));
    }
    for message in history {
        if let Some(json) = root_message(message) {
            root_ids.push(store_blob(serde_json::to_vec(&json)?, &mut blobs));
        }
    }
    let selected_context_blob = store_blob(
        wire::SelectedContextBlob {
            root_prompt_messages_json: root_ids.clone(),
            client_name: "kiss".into(),
        }
        .encode_to_vec(),
        &mut blobs,
    );
    let message_id = Uuid::new_v4().to_string();
    let selected_images = images
        .into_iter()
        .map(|(data, mime_type)| wire::SelectedImage {
            uuid: Uuid::new_v4().to_string(),
            mime_type,
            data_or_blob_id: Some(wire::selected_image::DataOrBlobId::Data(data)),
        })
        .collect();
    let user_message = wire::UserMessage {
        text: user_text,
        message_id: message_id.clone(),
        selected_context: Some(wire::SelectedContext { selected_images }),
        mode: 1,
        selected_context_blob,
        correlation_id: message_id,
    };
    let tools = context
        .tools
        .iter()
        .map(mcp_tool)
        .collect::<Result<Vec<_>>>()?;
    let run_request = wire::AgentRunRequest {
        conversation_state: Some(wire::ConversationStateStructure {
            root_prompt_messages_json: root_ids,
            previous_workspace_uris: std::env::current_dir()
                .ok()
                .and_then(|path| url::Url::from_directory_path(path).ok())
                .map(Into::into)
                .into_iter()
                .collect(),
            mode: Some(1),
            client_name: "kiss".into(),
        }),
        action: Some(wire::ConversationAction {
            action: Some(wire::conversation_action::Action::UserMessageAction(
                wire::UserMessageAction {
                    user_message: Some(user_message),
                },
            )),
        }),
        mcp_tools: Some(wire::McpTools { mcp_tools: tools }),
        conversation_id: Some(Uuid::new_v4().to_string()),
        requested_model: Some(requested_model(model, options)),
        custom_system_prompt: None,
    };
    Ok(BuiltRequest {
        message: wire::AgentClientMessage {
            message: Some(wire::agent_client_message::Message::RunRequest(Box::new(
                run_request,
            ))),
        },
        blobs,
    })
}

type SplitContext<'a> = (&'a [Message], String, Vec<(Vec<u8>, String)>);

fn split_context(context: &Context) -> SplitContext<'_> {
    if let Some((index, user)) =
        context
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| match message {
                Message::User(user) => Some((index, user)),
                _ => None,
            })
        && index + 1 == context.messages.len()
    {
        let images = match &user.content {
            crate::types::UserContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Image { data, mime_type } => {
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .ok()
                            .map(|data| (data, mime_type.clone()))
                    }
                    _ => None,
                })
                .collect(),
            crate::types::UserContent::Text(_) => Vec::new(),
        };
        return (&context.messages[..index], user.content.as_text(), images);
    }
    (
        &context.messages,
        "Continue from the tool results above.".into(),
        Vec::new(),
    )
}

fn root_message(message: &Message) -> Option<serde_json::Value> {
    match message {
        Message::User(user) => Some(serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": format!("<user_query>\n{}\n</user_query>", user.content.as_text())}],
        })),
        Message::Assistant(assistant) => {
            let content = assistant
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } if !text.is_empty() => {
                        Some(serde_json::json!({"type": "text", "text": text}))
                    }
                    ContentBlock::ToolCall(call) => Some(serde_json::json!({
                        "type": "tool-call",
                        "toolCallId": call.id,
                        "toolName": format!("mcp_kiss_{}", call.name),
                        "args": call.arguments,
                    })),
                    _ => None,
                })
                .collect::<Vec<_>>();
            (!content.is_empty())
                .then(|| serde_json::json!({"role": "assistant", "content": content}))
        }
        Message::ToolResult(result) => Some(serde_json::json!({
            "role": "tool",
            "content": [{
                "type": "tool-result",
                "toolCallId": result.tool_call_id,
                "toolName": format!("mcp_kiss_{}", result.tool_name),
                "result": tool_result_text(result),
                "isError": result.is_error,
            }],
        })),
    }
}

fn store_blob(data: Vec<u8>, blobs: &mut HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    let id = Sha256::digest(&data).to_vec();
    blobs.insert(id.clone(), data);
    id
}

fn mcp_tool(tool: &ToolDef) -> Result<wire::McpToolDefinition> {
    Ok(wire::McpToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        input_schema: json_to_proto(&tool.parameters).encode_to_vec(),
        provider_identifier: "kiss".into(),
        tool_name: tool.name.clone(),
    })
}

fn requested_model(model: &Model, options: &StreamOptions) -> wire::RequestedModel {
    let mut parameters = Vec::new();
    if model.id == "gpt-5.5" {
        parameters.push(wire::ModelParameter {
            id: "context".into(),
            value: "272k".into(),
        });
        parameters.push(wire::ModelParameter {
            id: "reasoning".into(),
            value: match options.reasoning {
                crate::ThinkingLevel::Off => "none",
                crate::ThinkingLevel::Minimal | crate::ThinkingLevel::Low => "low",
                crate::ThinkingLevel::Medium => "medium",
                crate::ThinkingLevel::High => "high",
                crate::ThinkingLevel::Xhigh | crate::ThinkingLevel::Max => "extra-high",
            }
            .into(),
        });
        parameters.push(wire::ModelParameter {
            id: "fast".into(),
            value: options.fast_mode.to_string(),
        });
    }
    wire::RequestedModel {
        model_id: model.id.clone(),
        max_mode: matches!(options.reasoning, crate::ThinkingLevel::Max),
        parameters,
    }
}

fn json_to_proto(input: &serde_json::Value) -> Value {
    let kind = match input {
        serde_json::Value::Null => value::Kind::NullValue(0),
        serde_json::Value::Bool(value) => value::Kind::BoolValue(*value),
        serde_json::Value::Number(value) => value::Kind::NumberValue(value.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(value) => value::Kind::StringValue(value.clone()),
        serde_json::Value::Array(values) => value::Kind::ListValue(ListValue {
            values: values.iter().map(json_to_proto).collect(),
        }),
        serde_json::Value::Object(fields) => value::Kind::StructValue(Struct {
            fields: fields
                .iter()
                .map(|(key, value)| (key.clone(), json_to_proto(value)))
                .collect(),
        }),
    };
    Value { kind: Some(kind) }
}

fn proto_to_json(input: Value) -> serde_json::Value {
    match input.kind {
        None | Some(value::Kind::NullValue(_)) => serde_json::Value::Null,
        Some(value::Kind::BoolValue(value)) => value.into(),
        Some(value::Kind::NumberValue(value)) => serde_json::json!(value),
        Some(value::Kind::StringValue(value)) => value.into(),
        Some(value::Kind::ListValue(value)) => {
            serde_json::Value::Array(value.values.into_iter().map(proto_to_json).collect())
        }
        Some(value::Kind::StructValue(value)) => serde_json::Value::Object(
            value
                .fields
                .into_iter()
                .map(|(key, value)| (key, proto_to_json(value)))
                .collect(),
        ),
    }
}

fn decode_arguments(args: HashMap<String, Vec<u8>>) -> Result<serde_json::Value> {
    let mut output = serde_json::Map::new();
    for (name, bytes) in args {
        let value = Value::decode(bytes.as_slice())
            .map(proto_to_json)
            .unwrap_or_else(|_| String::from_utf8_lossy(&bytes).into_owned().into());
        output.insert(name, value);
    }
    Ok(serde_json::Value::Object(output))
}

/// Fetch the account model list with Cursor's unary native RPC.
pub async fn discover_models(access_token: &str) -> Result<Vec<Model>> {
    let url = format!("{}{}", cursor_url(DEFAULT_URL), MODELS_PATH);
    let response = cursor_request(crate::stream::http_client().post(&url), access_token, false)
        .body(wire::GetUsableModelsRequest::default().encode_to_vec())
        .send()
        .await
        .with_context(|| format!("request Cursor models from {url}"))?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if !status.is_success() {
        anyhow::bail!(
            "Cursor models returned HTTP {status}: {}",
            crate::truncate_err(&String::from_utf8_lossy(&bytes))
        );
    }
    let response = wire::GetUsableModelsResponse::decode(bytes.as_ref())?;
    let mut models = Vec::new();
    for item in response.models {
        if item.model_id.trim().is_empty() {
            continue;
        }
        let name = [
            item.display_name,
            item.display_name_short,
            item.display_model_id,
        ]
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| item.model_id.clone());
        models.push(Model {
            id: item.model_id,
            name,
            api: API.into(),
            provider: PROVIDER.into(),
            base_url: cursor_url(DEFAULT_URL),
            reasoning: item.thinking_details.is_some(),
            input: vec!["text".into(), "image".into()],
            cost: ModelCost::default(),
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 128_000,
            compat: None,
            thinking_level_map: BTreeMap::new(),
            headers: BTreeMap::new(),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, UserContent, UserMessage, now_ms};

    fn model() -> Model {
        Model {
            id: "auto".into(),
            name: "Auto".into(),
            api: API.into(),
            provider: PROVIDER.into(),
            base_url: DEFAULT_URL.into(),
            reasoning: true,
            input: vec!["text".into(), "image".into()],
            cost: Default::default(),
            prompt_cache: None,
            context_window: 200_000,
            max_tokens: 128_000,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
        }
    }

    #[test]
    fn request_contains_history_current_prompt_and_tools() {
        let context = Context {
            system_prompt: Some("Be exact.".into()),
            messages: vec![
                Message::User(UserMessage {
                    content: UserContent::Text("old".into()),
                    timestamp: now_ms(),
                }),
                Message::Assistant(crate::AssistantMessage {
                    content: vec![ContentBlock::ToolCall(ToolCall {
                        id: "call-1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path": "x"}),
                        thought_signature: None,
                    })],
                    ..crate::AssistantMessage::empty(API, PROVIDER, "auto")
                }),
                Message::User(UserMessage {
                    content: UserContent::Text("new".into()),
                    timestamp: now_ms(),
                }),
            ],
            tools: vec![ToolDef {
                name: "read".into(),
                description: "Read a file".into(),
                parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            ..Default::default()
        };
        let built = build_request(&model(), &context, &StreamOptions::default()).unwrap();
        let Some(wire::agent_client_message::Message::RunRequest(request)) = built.message.message
        else {
            panic!("run request");
        };
        assert_eq!(request.requested_model.unwrap().model_id, "auto");
        assert_eq!(request.mcp_tools.unwrap().mcp_tools[0].tool_name, "read");
        let action = request.action.unwrap().action.unwrap();
        let wire::conversation_action::Action::UserMessageAction(action) = action;
        assert_eq!(action.user_message.unwrap().text, "new");
        let state = request.conversation_state.unwrap();
        assert_eq!(state.root_prompt_messages_json.len(), 3);
        assert!(
            state
                .root_prompt_messages_json
                .iter()
                .all(|id| built.blobs.contains_key(id))
        );
    }

    #[test]
    fn protobuf_value_keeps_json_argument_types() {
        let input = serde_json::json!({"s":"x","n":2.0,"b":true,"a":[null, 1.0]});
        assert_eq!(proto_to_json(json_to_proto(&input)), input);
    }

    #[test]
    fn tool_prefix_is_removed_once() {
        assert_eq!(strip_tool_prefix("mcp_kiss_read"), "read");
        assert_eq!(strip_tool_prefix("read"), "read");
    }

    #[test]
    fn blob_request_returns_exact_stored_bytes() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let id = vec![1, 2, 3];
        let data = b"prompt json".to_vec();
        let mut blobs = HashMap::from([(id.clone(), data.clone())]);
        handle_kv(
            wire::KvServerMessage {
                id: 7,
                message: Some(wire::kv_server_message::Message::GetBlobArgs(
                    wire::GetBlobArgs { blob_id: id },
                )),
            },
            &mut blobs,
            &sender,
        )
        .unwrap();
        let bytes = receiver.blocking_recv().unwrap().unwrap();
        let mut decoder = wire::FrameDecoder::default();
        let payload = decoder.push(&bytes).unwrap().pop().unwrap().1;
        let message = wire::AgentClientMessage::decode(payload.as_slice()).unwrap();
        let Some(wire::agent_client_message::Message::KvClientMessage(reply)) = message.message
        else {
            panic!("KV reply");
        };
        let Some(wire::kv_client_message::Message::GetBlobResult(reply)) = reply.message else {
            panic!("blob reply");
        };
        assert_eq!(reply.blob_data, Some(data));
    }

    #[test]
    fn tool_result_is_sent_as_mcp_result() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let result = ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("file data")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: now_ms(),
        };
        send_tool_result(&sender, 9, "exec-9".into(), &result).unwrap();
        let bytes = receiver.blocking_recv().unwrap().unwrap();
        let mut decoder = wire::FrameDecoder::default();
        let payload = decoder.push(&bytes).unwrap().pop().unwrap().1;
        let message = wire::AgentClientMessage::decode(payload.as_slice()).unwrap();
        let Some(wire::agent_client_message::Message::ExecClientMessage(reply)) = message.message
        else {
            panic!("exec reply");
        };
        assert_eq!(reply.id, 9);
        assert_eq!(reply.exec_id, "exec-9");
        let Some(wire::exec_client_message::Message::McpResult(result)) = reply.message else {
            panic!("MCP reply");
        };
        let Some(wire::mcp_result::Result::Success(success)) = result.result else {
            panic!("MCP success");
        };
        let Some(wire::mcp_tool_result_content_item::Content::Text(text)) =
            success.content[0].content.as_ref()
        else {
            panic!("text result");
        };
        assert_eq!(text.text, "file data");
    }

    #[tokio::test]
    async fn response_id_resumes_a_parked_tool_stream_without_a_new_request() {
        let response_id = format!("cursor:test-{}", Uuid::new_v4());
        let (sender, receiver) = oneshot::channel();
        pending()
            .lock()
            .unwrap()
            .insert(response_id.clone(), sender);
        let result = ToolResultMessage {
            tool_call_id: "call-1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("file data")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: now_ms(),
        };
        let context = Context {
            messages: vec![
                Message::Assistant(crate::AssistantMessage {
                    response_id: Some(response_id.clone()),
                    ..crate::AssistantMessage::empty(API, PROVIDER, "auto")
                }),
                Message::ToolResult(result),
            ],
            ..Default::default()
        };
        let (sink, output) = crate::EventStream::channel();
        super::stream(&model(), &context, &StreamOptions::default(), sink).await;
        let resumed = receiver.await.unwrap();
        assert!(find_tool_result(&resumed.context, "call-1").is_some());
        let mut builder = new_builder(&model(), resumed.sink, &response_id);
        let index = builder.begin_text();
        builder.append_text(index, "continued");
        builder.end_text(index);
        builder.finish(StopReason::Stop, &model());
        assert_eq!(output.result().await.text(), "continued");
    }

    #[test]
    fn transport_requires_http2_and_native_connect_headers() {
        let request = cursor_request(
            reqwest::Client::new().post("https://cursor.example/run"),
            "secret",
            true,
        )
        .build()
        .unwrap();
        assert_eq!(request.version(), http::Version::HTTP_2);
        assert_eq!(
            request.headers()["content-type"],
            "application/connect+proto"
        );
        assert_eq!(request.headers()["connect-protocol-version"], "1");
        assert_eq!(request.headers()["authorization"], "Bearer secret");
    }
}
