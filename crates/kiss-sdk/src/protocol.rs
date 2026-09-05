//! The wire protocol shared by every KISS SDK surface.
//!
//! This module is the single definition of what a client may ask for and what
//! it receives back. It is deliberately free of tokio, the filesystem, and the
//! network so it also compiles for `wasm32-unknown-unknown`.
//!
//! Three kinds of JSON object exist on the wire, one per line:
//!
//! * a **command**, sent by the client, tagged by a snake_case `type` such as
//!   `"prompt"` or `"get_state"`, with camelCase payload fields;
//! * a **response**, sent by the agent, always `{"type":"response",...}`, which
//!   echoes the command name and the client's optional correlation `id`;
//! * an **event**, sent by the agent, tagged by any other `type` such as
//!   `"message_update"`.
//!
//! Commands are named after Pi's RPC protocol so that clients written for
//! `pi --mode rpc` need minimal changes.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One image attached to a prompt, matching the `ImageContent` wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageInput {
    /// Always `"image"`. Present so the object round-trips unchanged.
    #[serde(rename = "type", default = "image_type")]
    pub kind: String,
    /// Base64-encoded image bytes, without a data-URL prefix.
    pub data: String,
    /// For example `image/png`.
    pub mime_type: String,
}

fn image_type() -> String {
    "image".to_string()
}

/// How a prompt should be handled when the agent is already streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamingBehavior {
    /// Deliver after the current turn's tool calls, before the next model call.
    Steer,
    /// Deliver only once the agent has completely stopped.
    FollowUp,
}

/// How queued steering or follow-up messages are released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    /// Release every queued message at the next opportunity.
    All,
    /// Release one queued message per opportunity.
    OneAtATime,
}

/// Every operation a client can request.
///
/// The `type` tag is snake_case; payload fields are camelCase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    // --- prompting ---
    #[serde(rename_all = "camelCase")]
    Prompt {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        streaming_behavior: Option<StreamingBehavior>,
    },
    #[serde(rename_all = "camelCase")]
    Steer {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
    },
    #[serde(rename_all = "camelCase")]
    FollowUp {
        message: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageInput>,
    },
    Abort {},
    ClearQueue {},
    NewSession {},

    // --- state ---
    GetState {},
    GetMessages {},
    #[serde(rename_all = "camelCase")]
    GetEntries {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<String>,
    },
    GetTree {},
    GetLastAssistantText {},
    GetSessionStats {},
    #[serde(rename_all = "camelCase")]
    SetSessionName {
        name: String,
    },

    // --- model ---
    #[serde(rename_all = "camelCase")]
    SetModel {
        provider: String,
        model_id: String,
    },
    #[serde(rename_all = "camelCase")]
    GetAvailableModels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        search: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SetThinkingLevel {
        level: String,
    },
    GetAvailableThinkingLevels {},

    // --- queues ---
    #[serde(rename_all = "camelCase")]
    SetSteeringMode {
        mode: QueueMode,
    },
    #[serde(rename_all = "camelCase")]
    SetFollowUpMode {
        mode: QueueMode,
    },

    // --- context management ---
    #[serde(rename_all = "camelCase")]
    Compact {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        custom_instructions: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SetAutoCompaction {
        enabled: bool,
    },
    #[serde(rename_all = "camelCase")]
    SetAutoRetry {
        enabled: bool,
    },

    // --- direct shell ---
    #[serde(rename_all = "camelCase")]
    Bash {
        command: String,
    },
    AbortBash {},

    // --- tools ---
    GetTools {},

    // --- session files ---
    #[serde(rename_all = "camelCase")]
    ExportHtml {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_path: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    SwitchSession {
        session_path: String,
    },
    #[serde(rename_all = "camelCase")]
    Fork {
        entry_id: String,
    },
    GetForkMessages {},

    // --- liveness ---
    Ping {},
}

impl Command {
    /// The snake_case name that appears in the `type` field and is echoed on
    /// the response as `command`.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Prompt { .. } => "prompt",
            Command::Steer { .. } => "steer",
            Command::FollowUp { .. } => "follow_up",
            Command::Abort {} => "abort",
            Command::ClearQueue {} => "clear_queue",
            Command::NewSession {} => "new_session",
            Command::GetState {} => "get_state",
            Command::GetMessages {} => "get_messages",
            Command::GetEntries { .. } => "get_entries",
            Command::GetTree {} => "get_tree",
            Command::GetLastAssistantText {} => "get_last_assistant_text",
            Command::GetSessionStats {} => "get_session_stats",
            Command::SetSessionName { .. } => "set_session_name",
            Command::SetModel { .. } => "set_model",
            Command::GetAvailableModels { .. } => "get_available_models",
            Command::SetThinkingLevel { .. } => "set_thinking_level",
            Command::GetAvailableThinkingLevels {} => "get_available_thinking_levels",
            Command::SetSteeringMode { .. } => "set_steering_mode",
            Command::SetFollowUpMode { .. } => "set_follow_up_mode",
            Command::Compact { .. } => "compact",
            Command::SetAutoCompaction { .. } => "set_auto_compaction",
            Command::SetAutoRetry { .. } => "set_auto_retry",
            Command::Bash { .. } => "bash",
            Command::AbortBash {} => "abort_bash",
            Command::GetTools {} => "get_tools",
            Command::ExportHtml { .. } => "export_html",
            Command::SwitchSession { .. } => "switch_session",
            Command::Fork { .. } => "fork",
            Command::GetForkMessages {} => "get_fork_messages",
            Command::Ping {} => "ping",
        }
    }
}

/// One decoded input line: a command plus the client's optional correlation id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(command: Command) -> Self {
        Request { id: None, command }
    }

    pub fn with_id(id: impl Into<String>, command: Command) -> Self {
        Request {
            id: Some(id.into()),
            command,
        }
    }
}

/// The agent's answer to exactly one command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// Always the string `"response"`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    /// A successful response carrying no payload.
    pub fn ok(command: impl Into<String>) -> Self {
        Response {
            kind: "response".into(),
            id: None,
            command: command.into(),
            success: true,
            data: None,
            error: None,
        }
    }

    /// A successful response carrying a JSON payload.
    pub fn ok_data(command: impl Into<String>, data: Value) -> Self {
        Response {
            data: Some(data),
            ..Response::ok(command)
        }
    }

    /// A failed response. `message` is shown to the caller verbatim.
    pub fn err(command: impl Into<String>, message: impl std::fmt::Display) -> Self {
        Response {
            kind: "response".into(),
            id: None,
            command: command.into(),
            success: false,
            data: None,
            error: Some(message.to_string()),
        }
    }

    /// Attach a correlation id, returning the response for chaining.
    pub fn with_id(mut self, id: Option<String>) -> Self {
        self.id = id;
        self
    }
}

/// One notification emitted while the agent works.
///
/// Events are carried as raw JSON because their payloads embed session message
/// and model objects whose serde shape is defined elsewhere in this repository;
/// re-declaring them here would let the two definitions drift.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Event(pub Value);

impl Event {
    /// The value of the event's `type` field, or `""` when malformed.
    pub fn event_type(&self) -> &str {
        self.0.get("type").and_then(Value::as_str).unwrap_or("")
    }

    /// Serialize to one JSON line (without the trailing newline).
    pub fn to_line(&self) -> String {
        self.0.to_string()
    }
}

impl From<Value> for Event {
    fn from(value: Value) -> Self {
        Event(value)
    }
}

/// Anything a client can read from the agent.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

/// Why a line could not be understood.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("could not parse JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("message has no string `type` field")]
    MissingType,
    #[error("unknown command type `{0}`")]
    UnknownCommand(String),
}

/// Decode one line the agent sent to a client.
pub fn decode_line(line: &str) -> Result<Incoming, ProtocolError> {
    let value: Value = serde_json::from_str(line)?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingType)?;
    if kind == "response" {
        Ok(Incoming::Response(serde_json::from_value(value)?))
    } else {
        Ok(Incoming::Event(Event(value)))
    }
}

/// Decode one command line a client sent to the agent.
///
/// The error deliberately names the offending `type` so a client author can see
/// immediately that they used a command this build does not implement.
pub fn decode_request(line: &str) -> Result<Request, ProtocolError> {
    let value: Value = serde_json::from_str(line)?;
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .ok_or(ProtocolError::MissingType)?
        .to_string();
    serde_json::from_value(value).map_err(|error| {
        // serde's "unknown variant" message is noisy; give the caller the name.
        if error.to_string().contains("unknown variant") {
            ProtocolError::UnknownCommand(kind)
        } else {
            ProtocolError::Json(error)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_tags_are_snake_case() {
        let json = serde_json::to_string(&Command::FollowUp {
            message: "x".into(),
            images: Vec::new(),
        })
        .unwrap();
        assert!(json.contains("\"type\":\"follow_up\""), "{json}");
        assert!(!json.contains("images"), "empty images are omitted: {json}");
    }

    #[test]
    fn request_fields_are_camel_case() {
        let request = decode_request(
            r#"{"id":"7","type":"prompt","message":"hi","streamingBehavior":"steer"}"#,
        )
        .unwrap();
        assert_eq!(request.id.as_deref(), Some("7"));
        assert_eq!(
            request.command,
            Command::Prompt {
                message: "hi".into(),
                images: Vec::new(),
                streaming_behavior: Some(StreamingBehavior::Steer),
            }
        );
    }

    #[test]
    fn error_response_shape_is_exact() {
        let json = serde_json::to_string(&Response::err("set_model", "nope")).unwrap();
        assert_eq!(
            json,
            r#"{"type":"response","command":"set_model","success":false,"error":"nope"}"#
        );
    }
}
