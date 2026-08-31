//! Amazon Bedrock ConverseStream adapter.
//!
//! The AWS SDK supplies the standard credential chain, SigV4 signing, bearer
//! token authentication, endpoint selection, and event-stream decoding.

use super::{PartialBuilder, thinking_budget};
use crate::event::EventSink;
use crate::model::Model;
use crate::stream::StreamOptions;
use crate::types::{ContentBlock, Context, Message as KissMessage, StopReason, UserContent};
use anyhow::{Context as _, Result};
use aws_sdk_bedrockruntime::config::{BehaviorVersion, Region, Token};
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::{
    ContentBlock as BedrockContent, ContentBlockDelta, ContentBlockStart, ConversationRole,
    ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
    ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock, SystemContentBlock,
    Tool, ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock,
    ToolResultStatus, ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use base64::Engine as _;
use serde_json::Value;
use std::collections::HashMap;

pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    let mut builder = PartialBuilder::new(model, sink);
    let request = match build_request(model, context, options).await {
        Ok(request) => request,
        Err(error) => {
            builder.fail(
                format!("Bedrock request setup failed: {error:#}"),
                false,
                model,
            );
            return;
        }
    };

    let response = tokio::select! {
        response = request.send() => response,
        _ = options.cancel.cancelled() => {
            builder.fail("Request aborted", true, model);
            return;
        }
    };
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            builder.fail(format!("Bedrock request failed: {error}"), false, model);
            return;
        }
    };

    let mut blocks: HashMap<i32, (usize, BlockKind)> = HashMap::new();
    let mut stop_reason = None;
    loop {
        let event = tokio::select! {
            event = response.stream.recv() => event,
            _ = options.cancel.cancelled() => {
                builder.fail("Request aborted", true, model);
                return;
            }
        };
        let event = match event {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(error) => {
                builder.fail(format!("Bedrock stream failed: {error}"), false, model);
                return;
            }
        };
        match event {
            ConverseStreamOutput::MessageStart(_) => builder.start(),
            ConverseStreamOutput::ContentBlockStart(event) => {
                if let Some(ContentBlockStart::ToolUse(tool)) = event.start() {
                    let index = builder
                        .begin_tool_call(tool.tool_use_id().to_string(), tool.name().to_string());
                    blocks.insert(event.content_block_index(), (index, BlockKind::Tool));
                }
            }
            ConverseStreamOutput::ContentBlockDelta(event) => {
                let provider_index = event.content_block_index();
                let Some(delta) = event.delta() else {
                    continue;
                };
                match delta {
                    ContentBlockDelta::Text(text) => {
                        let index =
                            block_index(&mut blocks, provider_index, BlockKind::Text, || {
                                builder.begin_text()
                            });
                        builder.append_text(index, text);
                    }
                    ContentBlockDelta::ToolUse(tool) => {
                        if let Some(&(index, BlockKind::Tool)) = blocks.get(&provider_index) {
                            builder.append_tool_args(index, tool.input());
                        }
                    }
                    ContentBlockDelta::ReasoningContent(reasoning) => {
                        let index =
                            block_index(&mut blocks, provider_index, BlockKind::Thinking, || {
                                builder.begin_thinking()
                            });
                        match reasoning {
                            ReasoningContentBlockDelta::Text(text) => {
                                builder.append_thinking(index, text);
                            }
                            ReasoningContentBlockDelta::Signature(signature) => {
                                builder.set_thinking_signature(index, signature.clone());
                            }
                            ReasoningContentBlockDelta::RedactedContent(content) => {
                                builder.set_thinking_signature(
                                    index,
                                    format!(
                                        "redacted:{}",
                                        base64::engine::general_purpose::STANDARD
                                            .encode(content.as_ref())
                                    ),
                                );
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            ConverseStreamOutput::ContentBlockStop(event) => {
                if let Some((index, kind)) = blocks.remove(&event.content_block_index()) {
                    match kind {
                        BlockKind::Text => builder.end_text(index),
                        BlockKind::Thinking => builder.end_thinking(index),
                        BlockKind::Tool => builder.end_tool_call(index, None),
                    }
                }
            }
            ConverseStreamOutput::MessageStop(event) => {
                let raw = event.stop_reason().as_str();
                builder.message.raw_stop_reason = Some(raw.to_string());
                stop_reason = Some(map_stop_reason(raw));
            }
            ConverseStreamOutput::Metadata(event) => {
                if let Some(usage) = event.usage() {
                    builder.message.usage.input = positive(usage.input_tokens());
                    builder.message.usage.output = positive(usage.output_tokens());
                    builder.message.usage.cache_read =
                        positive(usage.cache_read_input_tokens().unwrap_or_default());
                    builder.message.usage.cache_write =
                        positive(usage.cache_write_input_tokens().unwrap_or_default());
                }
            }
            _ => {}
        }
    }

    match stop_reason {
        Some(StopReason::Error) => {
            let error = builder.message.raw_stop_reason.clone().map_or_else(
                || "Bedrock stopped with an error".into(),
                |reason| format!("Bedrock stopped with: {reason}"),
            );
            builder.fail(error, false, model);
        }
        Some(reason) => builder.finish(reason, model),
        None => builder.fail("Bedrock stream ended without a stop reason", false, model),
    }
}

fn block_index(
    blocks: &mut HashMap<i32, (usize, BlockKind)>,
    provider_index: i32,
    kind: BlockKind,
    create: impl FnOnce() -> usize,
) -> usize {
    blocks
        .entry(provider_index)
        .or_insert_with(|| (create(), kind))
        .0
}

#[derive(Clone, Copy)]
enum BlockKind {
    Text,
    Thinking,
    Tool,
}

fn positive(value: i32) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

async fn build_request(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> Result<aws_sdk_bedrockruntime::operation::converse_stream::builders::ConverseStreamFluentBuilder>
{
    let region = bedrock_region(model);
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).region(Region::new(region.clone()));
    if let Some(profile) = crate::auth::provider_env("amazon-bedrock", "AWS_PROFILE") {
        loader = loader.profile_name(profile);
    }
    let shared = loader.load().await;
    let mut config = aws_sdk_bedrockruntime::config::Builder::from(&shared);
    if should_use_catalog_endpoint(model) {
        config = config.endpoint_url(&model.base_url);
    }
    if let Some(token) = options.api_key.as_ref().filter(|token| !token.is_empty()) {
        config = config.bearer_token(Token::new(token.clone(), None));
    }
    let client = aws_sdk_bedrockruntime::Client::from_conf(config.build());
    let messages = convert_messages(context)?;
    let system = context
        .system_prompt
        .as_ref()
        .filter(|prompt| !prompt.trim().is_empty())
        .map(|prompt| vec![SystemContentBlock::Text(prompt.clone())]);
    let inference = InferenceConfiguration::builder()
        .max_tokens(i32::try_from(
            options.max_tokens.unwrap_or(model.max_tokens),
        )?)
        .set_temperature(options.temperature.map(|temperature| temperature as f32))
        .build();
    let tools = convert_tools(context)?;
    let additional = thinking_budget(options.reasoning, model.max_tokens).map(|budget| {
        value_to_document(&serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": budget}
        }))
    });

    Ok(client
        .converse_stream()
        .model_id(&model.id)
        .set_messages(Some(messages))
        .set_system(system)
        .inference_config(inference)
        .set_tool_config(tools)
        .set_additional_model_request_fields(additional))
}

fn bedrock_region(model: &Model) -> String {
    arn_region(&model.id)
        .or_else(|| crate::auth::provider_env("amazon-bedrock", "AWS_REGION"))
        .or_else(|| crate::auth::provider_env("amazon-bedrock", "AWS_DEFAULT_REGION"))
        .or_else(|| endpoint_region(&model.base_url))
        .unwrap_or_else(|| "us-east-1".into())
}

fn arn_region(model_id: &str) -> Option<String> {
    let mut parts = model_id.split(':');
    (parts.next() == Some("arn"))
        .then(|| {
            let _partition = parts.next()?;
            (parts.next()? == "bedrock").then_some(parts.next()?.to_string())
        })
        .flatten()
}

fn endpoint_region(base_url: &str) -> Option<String> {
    let host = url::Url::parse(base_url)
        .ok()?
        .host_str()?
        .to_ascii_lowercase();
    let suffix = host
        .strip_prefix("bedrock-runtime.")
        .or_else(|| host.strip_prefix("bedrock-runtime-fips."))?;
    suffix
        .strip_suffix(".amazonaws.com")
        .or_else(|| suffix.strip_suffix(".amazonaws.com.cn"))
        .map(str::to_string)
}

fn should_use_catalog_endpoint(model: &Model) -> bool {
    endpoint_region(&model.base_url).is_none()
        || (std::env::var_os("AWS_REGION").is_none()
            && std::env::var_os("AWS_DEFAULT_REGION").is_none()
            && std::env::var_os("AWS_PROFILE").is_none())
}

fn convert_messages(context: &Context) -> Result<Vec<Message>> {
    let mut output = Vec::new();
    for message in &context.messages {
        let (role, content) = match message {
            KissMessage::User(user) => {
                let content = match &user.content {
                    UserContent::Text(text) => vec![required_text(text)],
                    UserContent::Blocks(blocks) => convert_user_blocks(blocks)?,
                };
                (ConversationRole::User, content)
            }
            KissMessage::Assistant(assistant) => {
                let mut content = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            content.push(BedrockContent::Text(text.clone()));
                        }
                        ContentBlock::Thinking {
                            thinking,
                            thinking_signature,
                            redacted,
                        } if !thinking.trim().is_empty() || thinking_signature.is_some() => {
                            if *redacted {
                                if let Some(encoded) = thinking_signature
                                    .as_deref()
                                    .and_then(|value| value.strip_prefix("redacted:"))
                                    && let Ok(bytes) =
                                        base64::engine::general_purpose::STANDARD.decode(encoded)
                                {
                                    content.push(BedrockContent::ReasoningContent(
                                        ReasoningContentBlock::RedactedContent(Blob::new(bytes)),
                                    ));
                                }
                            } else if !thinking.trim().is_empty() {
                                let block = ReasoningTextBlock::builder()
                                    .text(thinking)
                                    .set_signature(thinking_signature.clone())
                                    .build()?;
                                content.push(BedrockContent::ReasoningContent(
                                    ReasoningContentBlock::ReasoningText(block),
                                ));
                            }
                        }
                        ContentBlock::ToolCall(call) => {
                            content.push(BedrockContent::ToolUse(
                                ToolUseBlock::builder()
                                    .tool_use_id(&call.id)
                                    .name(&call.name)
                                    .input(value_to_document(&call.arguments))
                                    .build()?,
                            ));
                        }
                        _ => {}
                    }
                }
                if content.is_empty() {
                    continue;
                }
                (ConversationRole::Assistant, content)
            }
            KissMessage::ToolResult(result) => {
                let text = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        ContentBlock::Image { .. } => Some("[image attached]"),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let tool_result = ToolResultBlock::builder()
                    .tool_use_id(&result.tool_call_id)
                    .content(ToolResultContentBlock::Text(if text.is_empty() {
                        "<empty>".into()
                    } else {
                        text
                    }))
                    .status(if result.is_error {
                        ToolResultStatus::Error
                    } else {
                        ToolResultStatus::Success
                    })
                    .build()?;
                (
                    ConversationRole::User,
                    vec![BedrockContent::ToolResult(tool_result)],
                )
            }
        };
        push_message(&mut output, role, content)?;
    }
    Ok(output)
}

fn push_message(
    messages: &mut Vec<Message>,
    role: ConversationRole,
    content: Vec<BedrockContent>,
) -> Result<()> {
    if let Some(last) = messages.last_mut()
        && last.role == role
    {
        last.content.extend(content);
        return Ok(());
    }
    messages.push(
        Message::builder()
            .role(role)
            .set_content(Some(content))
            .build()?,
    );
    Ok(())
}

fn convert_user_blocks(blocks: &[ContentBlock]) -> Result<Vec<BedrockContent>> {
    let mut output = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                output.push(BedrockContent::Text(text.clone()));
            }
            ContentBlock::Image { data, mime_type } => {
                let format = match mime_type.as_str() {
                    "image/jpeg" => ImageFormat::Jpeg,
                    "image/gif" => ImageFormat::Gif,
                    "image/webp" => ImageFormat::Webp,
                    _ => ImageFormat::Png,
                };
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .context("decode Bedrock image")?;
                output.push(BedrockContent::Image(
                    ImageBlock::builder()
                        .format(format)
                        .source(ImageSource::Bytes(Blob::new(bytes)))
                        .build()?,
                ));
            }
            _ => {}
        }
    }
    if output.is_empty() {
        output.push(required_text(""));
    }
    Ok(output)
}

fn required_text(text: &str) -> BedrockContent {
    BedrockContent::Text(if text.trim().is_empty() {
        "<empty>".into()
    } else {
        text.to_string()
    })
}

fn convert_tools(context: &Context) -> Result<Option<ToolConfiguration>> {
    if context.tools.is_empty() {
        return Ok(None);
    }
    let tools = context
        .tools
        .iter()
        .map(|tool| {
            Ok(Tool::ToolSpec(
                ToolSpecification::builder()
                    .name(&tool.name)
                    .description(&tool.description)
                    .input_schema(ToolInputSchema::Json(value_to_document(&tool.parameters)))
                    .build()?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(
        ToolConfiguration::builder()
            .set_tools(Some(tools))
            .build()?,
    ))
}

fn value_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(value) => {
            let number = value
                .as_u64()
                .map(Number::PosInt)
                .or_else(|| value.as_i64().map(Number::NegInt))
                .unwrap_or_else(|| Number::Float(value.as_f64().unwrap_or_default()));
            Document::Number(number)
        }
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(value_to_document).collect()),
        Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), value_to_document(value)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolDef, UserMessage};
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "anthropic.claude-test-v1:0".into(),
            name: "test".into(),
            api: "bedrock-converse-stream".into(),
            provider: "amazon-bedrock".into(),
            base_url: "https://bedrock-runtime.us-west-2.amazonaws.com".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 1000,
            max_tokens: 100,
            compat: None,
            thinking_level_map: Default::default(),
            headers: Default::default(),
        }
    }

    #[test]
    fn region_comes_from_standard_endpoint() {
        assert_eq!(
            endpoint_region(&model().base_url).as_deref(),
            Some("us-west-2")
        );
        assert_eq!(
            arn_region("arn:aws:bedrock:ca-central-1:123:inference-profile/x").as_deref(),
            Some("ca-central-1")
        );
    }

    #[test]
    fn converts_messages_and_tools_to_sdk_types() {
        let context = Context {
            system_prompt: Some("system".into()),
            openai_responses_input: None,
            messages: vec![KissMessage::User(UserMessage {
                content: UserContent::Text("hello".into()),
                timestamp: 1,
            })],
            tools: vec![ToolDef {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
        };
        let messages = convert_messages(&context).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role(), &ConversationRole::User);
        assert_eq!(messages[0].content()[0].as_text().unwrap(), "hello");
        let tools = convert_tools(&context).unwrap().unwrap();
        let specification = tools.tools()[0].as_tool_spec().unwrap();
        assert_eq!(specification.name(), "read");
        assert!(specification.input_schema().unwrap().is_json());
    }

    #[test]
    fn maps_all_common_stop_reasons() {
        assert_eq!(map_stop_reason("end_turn"), StopReason::Stop);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("guardrail_intervened"), StopReason::Error);
    }
}
