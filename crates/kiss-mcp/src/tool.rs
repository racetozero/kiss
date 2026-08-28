//! One compact agent tool that proxies all configured MCP servers.

use crate::manager::McpManager;
use anyhow::{Context as _, Result, bail};
use kiss_agent::tool::{AgentTool, ExecutionMode, ToolResult, ToolUpdateSink};
use kiss_ai::ContentBlock as KissContent;
use rmcp::model::ContentBlock as McpContent;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const MAX_TEXT_BYTES: usize = 100_000;

#[derive(Clone)]
pub struct McpTool {
    manager: McpManager,
}

impl McpTool {
    pub fn new(manager: McpManager) -> Self {
        Self { manager }
    }

    pub fn manager(&self) -> &McpManager {
        &self.manager
    }
}

#[async_trait::async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        "mcp"
    }

    fn label(&self) -> &str {
        "MCP"
    }

    fn description(&self) -> String {
        "Find and use tools, resources, and prompts from configured MCP servers. Start with search or list. Use describe before a tool call when you do not know its input schema.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "list", "search", "describe", "call", "resources", "read_resource", "prompts", "get_prompt"],
                    "description": "The MCP operation."
                },
                "server": {"type": "string", "description": "The configured MCP server name."},
                "name": {"type": "string", "description": "The tool or prompt name."},
                "query": {"type": "string", "description": "Text used to search tool names and descriptions."},
                "uri": {"type": "string", "description": "The resource URI."},
                "arguments": {"type": "object", "description": "Arguments for a tool or prompt."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> Result<ToolResult> {
        let action = required_string(&args, "action")?;
        match action {
            "status" => json_result(self.manager.status().await),
            "list" => json_result(
                self.manager
                    .list_tools(optional_string(&args, "server"), &cancel)
                    .await?,
            ),
            "search" => json_result(
                self.manager
                    .search_tools(
                        required_string(&args, "query")?,
                        optional_string(&args, "server"),
                        &cancel,
                    )
                    .await?,
            ),
            "describe" => json_result(
                self.manager
                    .describe_tool(
                        required_string(&args, "server")?,
                        required_string(&args, "name")?,
                        &cancel,
                    )
                    .await?,
            ),
            "call" => {
                let server = required_string(&args, "server")?;
                let name = required_string(&args, "name")?;
                let result = self
                    .manager
                    .call_tool(
                        server,
                        name,
                        args.get("arguments").cloned().unwrap_or(Value::Null),
                        &cancel,
                    )
                    .await?;
                let is_error = result.is_error.unwrap_or(false);
                let details = json!({
                    "action": action,
                    "server": server,
                    "name": name,
                    "isError": is_error,
                    "structuredContent": result.structured_content,
                    "meta": result.meta,
                });
                let content = convert_content(result.content)?;
                if is_error {
                    let text = content
                        .iter()
                        .filter_map(|block| match block {
                            KissContent::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    bail!("MCP tool `{server}/{name}` failed: {text}")
                }
                Ok(ToolResult {
                    content,
                    details,
                    ..Default::default()
                })
            }
            "resources" => json_result(
                self.manager
                    .list_resources(required_string(&args, "server")?, &cancel)
                    .await?,
            ),
            "read_resource" => json_result(
                self.manager
                    .read_resource(
                        required_string(&args, "server")?,
                        required_string(&args, "uri")?,
                        &cancel,
                    )
                    .await?,
            ),
            "prompts" => json_result(
                self.manager
                    .list_prompts(required_string(&args, "server")?, &cancel)
                    .await?,
            ),
            "get_prompt" => json_result(
                self.manager
                    .get_prompt(
                        required_string(&args, "server")?,
                        required_string(&args, "name")?,
                        args.get("arguments").cloned().unwrap_or(Value::Null),
                        &cancel,
                    )
                    .await?,
            ),
            _ => bail!("unknown MCP action `{action}`"),
        }
    }
}

fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("MCP action needs `{key}`"))
}

fn optional_string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn json_result(value: impl serde::Serialize) -> Result<ToolResult> {
    let details = serde_json::to_value(&value)?;
    let text = serde_json::to_string_pretty(&value)?;
    Ok(ToolResult {
        content: vec![KissContent::text(limit_text(text))],
        details,
        ..Default::default()
    })
}

fn convert_content(content: Vec<McpContent>) -> Result<Vec<KissContent>> {
    let mut converted = Vec::with_capacity(content.len());
    for block in content {
        match block {
            McpContent::Text(text) => converted.push(KissContent::text(limit_text(text.text))),
            McpContent::Image(image) => converted.push(KissContent::Image {
                data: image.data,
                mime_type: image.mime_type,
            }),
            McpContent::Audio(audio) => converted.push(KissContent::text(format!(
                "[MCP audio content: {}; {} base64 bytes]",
                audio.mime_type,
                audio.data.len()
            ))),
            McpContent::Resource(resource) => converted.push(KissContent::text(limit_text(
                serde_json::to_string_pretty(&resource)?,
            ))),
            McpContent::ResourceLink(resource) => converted.push(KissContent::text(format!(
                "MCP resource: {} ({})",
                resource.name, resource.uri
            ))),
            other => converted.push(KissContent::text(limit_text(serde_json::to_string_pretty(
                &other,
            )?))),
        }
    }
    if converted.is_empty() {
        converted.push(KissContent::text("MCP tool completed with no content."));
    }
    Ok(converted)
}

fn limit_text(mut text: String) -> String {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n\n[MCP output was truncated by KISS.]\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_limit_keeps_utf8_valid() {
        let text = "é".repeat(MAX_TEXT_BYTES);
        let limited = limit_text(text);
        assert!(limited.is_char_boundary(limited.len()));
        assert!(limited.contains("truncated"));
    }

    #[test]
    fn required_values_are_checked() {
        let input = json!({"action": "search", "query": "files"});
        assert_eq!(required_string(&input, "action").unwrap(), "search");
        assert_eq!(required_string(&input, "query").unwrap(), "files");
    }
}
