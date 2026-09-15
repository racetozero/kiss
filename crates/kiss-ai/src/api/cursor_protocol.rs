//! The small Cursor Agent Protobuf surface that KISS uses.
//!
//! Cursor's full private schema is much larger. Protobuf field numbers make
//! this subset forward compatible: Prost skips every field that is not listed
//! here.

use prost::{Message, Oneof};
use std::collections::HashMap;

pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const END_STREAM_FLAG: u8 = 0x02;

pub fn frame(message: &impl Message) -> Vec<u8> {
    frame_bytes(&message.encode_to_vec())
}

pub fn frame_bytes(message: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(message.len() + 5);
    output.push(0);
    output.extend_from_slice(&(message.len() as u32).to_be_bytes());
    output.extend_from_slice(message);
    output
}

#[derive(Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) -> anyhow::Result<Vec<(u8, Vec<u8>)>> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.pending.len() < 5 {
                break;
            }
            let length = u32::from_be_bytes(self.pending[1..5].try_into().unwrap()) as usize;
            if length > MAX_FRAME_BYTES {
                anyhow::bail!("Cursor frame is too large: {length} bytes");
            }
            if self.pending.len() < length + 5 {
                break;
            }
            let flags = self.pending[0];
            let payload = self.pending[5..length + 5].to_vec();
            self.pending.drain(..length + 5);
            frames.push((flags, payload));
        }
        Ok(frames)
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentClientMessage {
    #[prost(oneof = "agent_client_message::Message", tags = "1, 2, 3, 5, 7")]
    pub message: Option<agent_client_message::Message>,
}

pub mod agent_client_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, boxed, tag = "1")]
        RunRequest(Box<AgentRunRequest>),
        #[prost(message, tag = "2")]
        ExecClientMessage(ExecClientMessage),
        #[prost(message, tag = "3")]
        KvClientMessage(KvClientMessage),
        #[prost(message, tag = "5")]
        ExecClientControlMessage(ExecClientControlMessage),
        #[prost(message, tag = "7")]
        ClientHeartbeat(ClientHeartbeat),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentRunRequest {
    #[prost(message, optional, tag = "1")]
    pub conversation_state: Option<ConversationStateStructure>,
    #[prost(message, optional, tag = "2")]
    pub action: Option<ConversationAction>,
    #[prost(message, optional, tag = "4")]
    pub mcp_tools: Option<McpTools>,
    #[prost(string, optional, tag = "5")]
    pub conversation_id: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub custom_system_prompt: Option<String>,
    #[prost(message, optional, tag = "9")]
    pub requested_model: Option<RequestedModel>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationStateStructure {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(string, repeated, tag = "9")]
    pub previous_workspace_uris: Vec<String>,
    #[prost(int32, optional, tag = "10")]
    pub mode: Option<i32>,
    #[prost(string, tag = "22")]
    pub client_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ConversationAction {
    #[prost(oneof = "conversation_action::Action", tags = "1")]
    pub action: Option<conversation_action::Action>,
}

pub mod conversation_action {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Action {
        #[prost(message, tag = "1")]
        UserMessageAction(UserMessageAction),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessageAction {
    #[prost(message, optional, tag = "1")]
    pub user_message: Option<UserMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserMessage {
    #[prost(string, tag = "1")]
    pub text: String,
    #[prost(string, tag = "2")]
    pub message_id: String,
    #[prost(message, optional, tag = "3")]
    pub selected_context: Option<SelectedContext>,
    #[prost(int32, tag = "4")]
    pub mode: i32,
    #[prost(bytes = "vec", tag = "10")]
    pub selected_context_blob: Vec<u8>,
    #[prost(string, tag = "17")]
    pub correlation_id: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedContext {
    #[prost(message, repeated, tag = "1")]
    pub selected_images: Vec<SelectedImage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedImage {
    #[prost(string, tag = "2")]
    pub uuid: String,
    #[prost(string, tag = "7")]
    pub mime_type: String,
    #[prost(oneof = "selected_image::DataOrBlobId", tags = "1, 8")]
    pub data_or_blob_id: Option<selected_image::DataOrBlobId>,
}

pub mod selected_image {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum DataOrBlobId {
        #[prost(bytes, tag = "1")]
        BlobId(Vec<u8>),
        #[prost(bytes, tag = "8")]
        Data(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct SelectedContextBlob {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub root_prompt_messages_json: Vec<Vec<u8>>,
    #[prost(string, tag = "22")]
    pub client_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestedModel {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(bool, tag = "2")]
    pub max_mode: bool,
    #[prost(message, repeated, tag = "3")]
    pub parameters: Vec<ModelParameter>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelParameter {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTools {
    #[prost(message, repeated, tag = "1")]
    pub mcp_tools: Vec<McpToolDefinition>,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpToolDefinition {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(bytes = "vec", tag = "3")]
    pub input_schema: Vec<u8>,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct AgentServerMessage {
    #[prost(oneof = "agent_server_message::Message", tags = "1, 2, 3, 4, 7")]
    pub message: Option<agent_server_message::Message>,
}

pub mod agent_server_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        InteractionUpdate(InteractionUpdate),
        #[prost(message, tag = "2")]
        ExecServerMessage(ExecServerMessage),
        #[prost(message, tag = "3")]
        ConversationCheckpointUpdate(ConversationStateStructure),
        #[prost(message, tag = "4")]
        KvServerMessage(KvServerMessage),
        #[prost(message, tag = "7")]
        InteractionQuery(InteractionQuery),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct InteractionUpdate {
    #[prost(oneof = "interaction_update::Message", tags = "1, 4, 8, 14")]
    pub message: Option<interaction_update::Message>,
}

pub mod interaction_update {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "1")]
        TextDelta(TextDeltaUpdate),
        #[prost(message, tag = "4")]
        ThinkingDelta(ThinkingDeltaUpdate),
        #[prost(message, tag = "8")]
        TokenDelta(TokenDeltaUpdate),
        #[prost(message, tag = "14")]
        TurnEnded(TurnEndedUpdate),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct TextDeltaUpdate {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ThinkingDeltaUpdate {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct TokenDeltaUpdate {
    #[prost(int32, tag = "1")]
    pub tokens: i32,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct TurnEndedUpdate {}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct InteractionQuery {
    #[prost(uint32, tag = "1")]
    pub id: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(oneof = "exec_server_message::Message", tags = "11")]
    pub message: Option<exec_server_message::Message>,
}

pub mod exec_server_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "11")]
        McpArgs(McpArgs),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct McpArgs {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(map = "string, bytes", tag = "2")]
    pub args: HashMap<String, Vec<u8>>,
    #[prost(string, tag = "3")]
    pub tool_call_id: String,
    #[prost(string, tag = "4")]
    pub provider_identifier: String,
    #[prost(string, tag = "5")]
    pub tool_name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "15")]
    pub exec_id: String,
    #[prost(oneof = "exec_client_message::Message", tags = "11")]
    pub message: Option<exec_client_message::Message>,
}

pub mod exec_client_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "11")]
        McpResult(McpResult),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct McpResult {
    #[prost(oneof = "mcp_result::Result", tags = "1, 2")]
    pub result: Option<mcp_result::Result>,
}

pub mod mcp_result {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Result {
        #[prost(message, tag = "1")]
        Success(McpSuccess),
        #[prost(message, tag = "2")]
        Error(McpError),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct McpSuccess {
    #[prost(message, repeated, tag = "1")]
    pub content: Vec<McpToolResultContentItem>,
    #[prost(bool, tag = "2")]
    pub is_error: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpError {
    #[prost(string, tag = "1")]
    pub error: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpToolResultContentItem {
    #[prost(oneof = "mcp_tool_result_content_item::Content", tags = "1, 2")]
    pub content: Option<mcp_tool_result_content_item::Content>,
}

pub mod mcp_tool_result_content_item {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Content {
        #[prost(message, tag = "1")]
        Text(McpTextContent),
        #[prost(message, tag = "2")]
        Image(McpImageContent),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct McpTextContent {
    #[prost(string, tag = "1")]
    pub text: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct McpImageContent {
    #[prost(bytes = "vec", tag = "1")]
    pub data: Vec<u8>,
    #[prost(string, tag = "2")]
    pub mime_type: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientControlMessage {
    #[prost(oneof = "exec_client_control_message::Message", tags = "2")]
    pub message: Option<exec_client_control_message::Message>,
}

pub mod exec_client_control_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        Throw(ExecClientThrow),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ExecClientThrow {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(string, tag = "2")]
    pub error: String,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ClientHeartbeat {}

#[derive(Clone, PartialEq, Message)]
pub struct KvServerMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_server_message::Message", tags = "2, 3")]
    pub message: Option<kv_server_message::Message>,
}

pub mod kv_server_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobArgs(GetBlobArgs),
        #[prost(message, tag = "3")]
        SetBlobArgs(SetBlobArgs),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetBlobArgs {
    #[prost(bytes = "vec", tag = "1")]
    pub blob_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub blob_data: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KvClientMessage {
    #[prost(uint32, tag = "1")]
    pub id: u32,
    #[prost(oneof = "kv_client_message::Message", tags = "2, 3")]
    pub message: Option<kv_client_message::Message>,
}

pub mod kv_client_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Message {
        #[prost(message, tag = "2")]
        GetBlobResult(GetBlobResult),
        #[prost(message, tag = "3")]
        SetBlobResult(SetBlobResult),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBlobResult {
    #[prost(bytes = "vec", optional, tag = "1")]
    pub blob_data: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct SetBlobResult {}

#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsRequest {
    #[prost(string, repeated, tag = "1")]
    pub custom_model_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetUsableModelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub models: Vec<ModelDetails>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ModelDetails {
    #[prost(string, tag = "1")]
    pub model_id: String,
    #[prost(string, tag = "3")]
    pub display_model_id: String,
    #[prost(string, tag = "4")]
    pub display_name: String,
    #[prost(string, tag = "5")]
    pub display_name_short: String,
    #[prost(message, optional, tag = "2")]
    pub thinking_details: Option<ThinkingDetails>,
}

#[derive(Clone, Copy, PartialEq, Message)]
pub struct ThinkingDetails {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_decoder_accepts_split_and_joined_frames() {
        let first = frame(&TextDeltaUpdate { text: "one".into() });
        let second = frame(&TextDeltaUpdate { text: "two".into() });
        let joined = [first, second].concat();
        let mut decoder = FrameDecoder::default();
        assert!(decoder.push(&joined[..3]).unwrap().is_empty());
        let frames = decoder.push(&joined[3..]).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(
            TextDeltaUpdate::decode(frames[0].1.as_slice())
                .unwrap()
                .text,
            "one"
        );
        assert_eq!(
            TextDeltaUpdate::decode(frames[1].1.as_slice())
                .unwrap()
                .text,
            "two"
        );
    }

    #[test]
    fn client_run_request_round_trips() {
        let message = AgentClientMessage {
            message: Some(agent_client_message::Message::RunRequest(Box::new(
                AgentRunRequest {
                    requested_model: Some(RequestedModel {
                        model_id: "auto".into(),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ))),
        };
        let decoded = AgentClientMessage::decode(message.encode_to_vec().as_slice()).unwrap();
        let Some(agent_client_message::Message::RunRequest(request)) = decoded.message else {
            panic!("run request");
        };
        assert_eq!(request.requested_model.unwrap().model_id, "auto");
    }
}
