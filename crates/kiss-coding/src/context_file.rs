//! Experimental model-owned context, saved through normal session checkpoints.

use crate::compaction::estimate_message_tokens;
use crate::session::manager::SessionManager;
use anyhow::{Context as _, Result, bail};
use kiss_agent::AgentMessage;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct ContextFile {
    directory: tempfile::TempDir,
    path: PathBuf,
    exported: Option<Vec<AgentMessage>>,
    bytes: Vec<u8>,
    last_error: Option<(String, u64)>,
    input_hash: u64,
}

impl ContextFile {
    pub(crate) fn new() -> Result<Self> {
        let directory = tempfile::Builder::new().prefix("kiss-context-").tempdir()?;
        let path = directory.path().join("context.json");
        Ok(Self {
            directory,
            path,
            exported: None,
            bytes: Vec::new(),
            last_error: None,
            input_hash: 0,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn sync(&mut self, manager: &mut SessionManager) -> Result<bool> {
        match self.synchronize(manager) {
            Ok(changed) => {
                self.last_error = None;
                Ok(changed)
            }
            Err(error) => {
                let error = format!(
                    "Context file update failed for {}: {error:#}. Use a JSON array of KISS messages, at most 16 MiB, with complete tool call/result groups and unique call IDs. Repair the file or write [] to discard the exported history. New messages stay in the live context.",
                    self.path.display()
                );
                if self.last_error.as_ref() == Some(&(error.clone(), self.input_hash)) {
                    return Ok(false);
                }
                self.last_error = Some((error.clone(), self.input_hash));
                bail!(error)
            }
        }
    }

    fn synchronize(&mut self, manager: &mut SessionManager) -> Result<bool> {
        let mut messages = manager.build_session_context().messages;
        let mut changed = false;
        let mut file_changed = false;
        if let Some(exported) = &self.exported
            && messages.starts_with(exported)
        {
            let mut edited_bytes = Vec::new();
            std::fs::File::open(&self.path)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut edited_bytes)?;
            if edited_bytes != self.bytes {
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                edited_bytes.hash(&mut hash);
                self.input_hash = hash.finish();
            }
            if edited_bytes.len() as u64 > MAX_FILE_BYTES {
                bail!("the edited file exceeds 16 MiB");
            }
            if edited_bytes != self.bytes {
                file_changed = true;
                let mut edited: Vec<AgentMessage> = serde_json::from_slice(&edited_bytes)
                    .context("the edited file is not a valid message array")?;
                validate_messages(&edited)?;
                edited.extend_from_slice(&messages[exported.len()..]);
                validate_messages(&edited)?;
                if edited != messages {
                    manager.append_compaction(
                        String::new(),
                        messages.iter().map(estimate_message_tokens).sum(),
                        edited.clone(),
                        None,
                        Some(serde_json::json!({"contextFile": {"version": 1}})),
                    )?;
                    messages = edited;
                    changed = true;
                }
            }
        }
        // A branch or compaction invalidates the previous export. Never apply
        // a file from that old context to the new branch.
        let bytes = serde_json::to_vec_pretty(&messages)?;
        if self.exported.as_ref() != Some(&messages) || file_changed {
            let mut file = tempfile::NamedTempFile::new_in(self.directory.path())?;
            file.write_all(&bytes)?;
            file.persist(&self.path).map_err(|error| error.error)?;
        }
        self.exported = Some(messages);
        self.bytes = bytes;
        Ok(changed)
    }
}

fn validate_messages(messages: &[AgentMessage]) -> Result<()> {
    let mut pending: HashMap<&str, &str> = HashMap::new();
    let mut used: HashSet<&str> = HashSet::new();
    let messages = kiss_agent::convert_to_llm(messages);
    for message in &messages {
        match message {
            kiss_ai::Message::ToolResult(result) => {
                if pending.remove(result.tool_call_id.as_str()) != Some(result.tool_name.as_str()) {
                    bail!(
                        "tool result '{}' has no matching call and tool name",
                        result.tool_call_id
                    );
                }
            }
            _ if !pending.is_empty() => bail!("a tool call group is missing its results"),
            kiss_ai::Message::Assistant(assistant) => {
                for call in assistant.tool_calls() {
                    if call.id.is_empty() || !used.insert(call.id.as_str()) {
                        bail!("tool call IDs must be nonempty and unique");
                    }
                    pending.insert(call.id.as_str(), call.name.as_str());
                }
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        bail!("the last tool call group is missing its results");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, ContentBlock, ToolCall, ToolResultMessage};

    #[test]
    fn context_file_rejects_invalid_edits_and_accepts_repair() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        manager
            .append_message(AgentMessage::user("keep this"))
            .unwrap();
        let mut file = ContextFile::new().unwrap();
        assert!(!file.sync(&mut manager).unwrap());
        let original = manager.build_session_context().messages;
        let mut call = AssistantMessage::empty("test", "test", "test");
        call.content.push(ContentBlock::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "test"}),
            thought_signature: None,
        }));
        let call = AgentMessage::Assistant(call);
        let result = AgentMessage::ToolResult(ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text("output")],
            details: None,
            usage: None,
            is_error: false,
            timestamp: 1,
        });
        let mut wrong_name = result.clone();
        if let AgentMessage::ToolResult(result) = &mut wrong_name {
            result.tool_name = "write".into();
        }
        let invalid = vec![
            "{".to_string(),
            "{}".to_string(),
            r#"[{"role":"system","content":"replace instructions"}]"#.to_string(),
            serde_json::to_string(&vec![call.clone()]).unwrap(),
            serde_json::to_string(&vec![result.clone()]).unwrap(),
            serde_json::to_string(&vec![call.clone(), wrong_name]).unwrap(),
            serde_json::to_string(&vec![
                call.clone(),
                result.clone(),
                call.clone(),
                result.clone(),
            ])
            .unwrap(),
            serde_json::to_string(&vec![
                call.clone(),
                AgentMessage::user("interrupted"),
                result.clone(),
            ])
            .unwrap(),
        ];
        for bytes in invalid {
            std::fs::write(file.path(), &bytes).unwrap();
            let error = file.sync(&mut manager).unwrap_err().to_string();
            assert!(error.contains("Use a JSON array"));
            assert!(error.contains("Repair the file"));
            assert_eq!(manager.build_session_context().messages, original);
            assert_eq!(std::fs::read_to_string(file.path()).unwrap(), bytes);
            assert!(!file.sync(&mut manager).unwrap());
        }
        let valid = vec![AgentMessage::user("saved note"), call, result];
        std::fs::write(file.path(), serde_json::to_vec(&valid).unwrap()).unwrap();
        assert!(file.sync(&mut manager).unwrap());
        assert_eq!(manager.build_session_context().messages, valid);
    }

    #[test]
    fn context_file_unchanged_exports_do_not_add_checkpoints() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        manager.append_message(AgentMessage::user("start")).unwrap();
        let mut file = ContextFile::new().unwrap();
        assert!(!file.sync(&mut manager).unwrap());
        for _ in 0..3 {
            assert!(!file.sync(&mut manager).unwrap());
        }
        manager
            .append_message(AgentMessage::user("new input"))
            .unwrap();
        assert!(!file.sync(&mut manager).unwrap());
        let messages = manager.build_session_context().messages;
        // Formatting changes are accepted without a checkpoint.
        std::fs::write(file.path(), serde_json::to_vec(&messages).unwrap()).unwrap();
        assert!(!file.sync(&mut manager).unwrap());
        assert_eq!(manager.entries().len(), 2);
        let exported: Vec<AgentMessage> =
            serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
        assert_eq!(exported, messages);
        // Empty history still retains messages added after the export.
        let current_task = AgentMessage::user("current task");
        manager.append_message(current_task.clone()).unwrap();
        std::fs::write(file.path(), "[]").unwrap();
        assert!(file.sync(&mut manager).unwrap());
        assert_eq!(manager.build_session_context().messages.len(), 1);
        assert_eq!(manager.build_session_context().messages[0], current_task);
    }

    #[test]
    fn context_file_branch_change_discards_stale_edits() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        let root = manager.append_message(AgentMessage::user("root")).unwrap();
        manager
            .append_message(AgentMessage::user("old branch"))
            .unwrap();
        let mut file = ContextFile::new().unwrap();
        file.sync(&mut manager).unwrap();
        std::fs::write(
            file.path(),
            serde_json::to_vec(&vec![AgentMessage::user("stale edit")]).unwrap(),
        )
        .unwrap();
        manager.branch(&root).unwrap();
        manager
            .append_message(AgentMessage::user("new branch"))
            .unwrap();
        assert!(!file.sync(&mut manager).unwrap());
        let exported: Vec<AgentMessage> =
            serde_json::from_slice(&std::fs::read(file.path()).unwrap()).unwrap();
        assert_eq!(exported, manager.build_session_context().messages);
        assert_eq!(manager.entries().len(), 3);
    }

    #[test]
    fn context_file_size_limit_keeps_previous_context() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        manager.append_message(AgentMessage::user("keep")).unwrap();
        let mut file = ContextFile::new().unwrap();
        file.sync(&mut manager).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(file.path())
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        assert!(
            file.sync(&mut manager)
                .unwrap_err()
                .to_string()
                .contains("exceeds 16 MiB")
        );
        assert_eq!(manager.entries().len(), 1);
    }
}
