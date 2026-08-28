//! Session JSONL entry types, wire-compatible with pi's session format v3.

use kiss_agent::AgentMessage;
use kiss_ai::{ThinkingLevel, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const SESSION_VERSION: u32 = 3;

/// First line of every session file. Not part of the entry tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: String, // always "session"
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// Unknown fields from foreign sessions survive a rewrite.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Common id/parent/timestamp fields on every tree entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryBase {
    /// 8-char hex id.
    pub id: String,
    pub parent_id: Option<String>,
    /// ISO-8601 timestamp.
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    #[serde(rename_all = "camelCase")]
    Message {
        #[serde(flatten)]
        base: EntryBase,
        message: AgentMessage,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    ModelChange {
        #[serde(flatten)]
        base: EntryBase,
        provider: String,
        model_id: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    ThinkingLevelChange {
        #[serde(flatten)]
        base: EntryBase,
        thinking_level: ThinkingLevel,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    Compaction {
        #[serde(flatten)]
        base: EntryBase,
        summary: String,
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        first_kept_entry_id: Option<String>,
        /// Post-compaction context checkpoint (newer format).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained_tail: Option<Vec<AgentMessage>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    BranchSummary {
        #[serde(flatten)]
        base: EntryBase,
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    /// Harness/extension state; never part of LLM context.
    #[serde(rename_all = "camelCase")]
    Custom {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    /// Injected message that does participate in LLM context.
    #[serde(rename_all = "camelCase")]
    CustomMessage {
        #[serde(flatten)]
        base: EntryBase,
        custom_type: String,
        content: kiss_ai::UserContent,
        display: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    Label {
        #[serde(flatten)]
        base: EntryBase,
        target_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(rename_all = "camelCase")]
    SessionInfo {
        #[serde(flatten)]
        base: EntryBase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

impl SessionEntry {
    pub fn base(&self) -> &EntryBase {
        match self {
            SessionEntry::Message { base, .. }
            | SessionEntry::ModelChange { base, .. }
            | SessionEntry::ThinkingLevelChange { base, .. }
            | SessionEntry::Compaction { base, .. }
            | SessionEntry::BranchSummary { base, .. }
            | SessionEntry::Custom { base, .. }
            | SessionEntry::CustomMessage { base, .. }
            | SessionEntry::Label { base, .. }
            | SessionEntry::SessionInfo { base, .. } => base,
        }
    }

    pub fn id(&self) -> &str {
        &self.base().id
    }

    pub fn parent_id(&self) -> Option<&str> {
        self.base().parent_id.as_deref()
    }
}

pub fn new_entry_id() -> String {
    let n: u32 = rand::random();
    format!("{n:08x}")
}

pub fn iso_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_wire_shape() {
        let entry = SessionEntry::Message {
            base: EntryBase {
                id: "a1b2c3d4".into(),
                parent_id: None,
                timestamp: iso_now(),
            },
            message: AgentMessage::user("hello"),
            extra: Map::new(),
        };
        let v = serde_json::to_value(&entry).unwrap();
        assert_eq!(v["type"], "message");
        assert_eq!(v["parentId"], Value::Null);
        assert_eq!(v["message"]["role"], "user");

        let parsed: SessionEntry = serde_json::from_value(v).unwrap();
        assert_eq!(parsed.id(), "a1b2c3d4");
    }

    #[test]
    fn unknown_fields_preserved() {
        let line = r#"{"type":"label","id":"x","parentId":"y","timestamp":"t","targetId":"z","label":"L","futureField":42}"#;
        let entry: SessionEntry = serde_json::from_str(line).unwrap();
        let out = serde_json::to_value(&entry).unwrap();
        assert_eq!(out["futureField"], 42);
    }
}
