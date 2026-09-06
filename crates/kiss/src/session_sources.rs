use anyhow::{Context as _, Result};
use kiss_agent::AgentMessage;
use kiss_ai::{AssistantMessage, ContentBlock, StopReason};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

type SessionMetadata = (String, String, PathBuf, usize);

#[derive(Debug, Clone)]
pub(crate) enum SessionSource {
    Kiss(PathBuf),
    Pi(PathBuf),
    Claude(PathBuf),
    Codex(PathBuf),
    OpenCode(String),
}

impl SessionSource {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Kiss(_) => "KISS",
            Self::Pi(_) => "Pi",
            Self::Claude(_) => "Claude Code",
            Self::Codex(_) => "Codex",
            Self::OpenCode(_) => "OpenCode",
        }
    }

    pub(crate) fn kiss_path(&self) -> Option<&Path> {
        match self {
            Self::Kiss(path) => Some(path),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionRecord {
    pub(crate) source: SessionSource,
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) cwd: PathBuf,
    pub(crate) modified: SystemTime,
    pub(crate) entry_count: Option<usize>,
}

pub(crate) fn discover(cwd: &Path, kiss_dir: &Path, global: bool) -> Result<Vec<SessionRecord>> {
    let Some(home) = dirs::home_dir() else {
        return Ok(Vec::new());
    };
    let mut records = Vec::new();
    let kiss = if global {
        kiss_coding::SessionManager::list_all(kiss_dir)?
    } else {
        kiss_coding::SessionManager::list(cwd, kiss_dir)?
    };
    records.extend(kiss.into_iter().map(|item| SessionRecord {
        source: SessionSource::Kiss(item.path),
        id: item.id.clone(),
        title: item.name.or(item.first_message).unwrap_or(item.id),
        cwd: PathBuf::from(item.cwd),
        modified: item.modified,
        entry_count: Some(item.entry_count),
    }));

    add_pi_records(&mut records, &home.join(".pi/agent/sessions"))?;
    add_jsonl_records(
        &mut records,
        &home.join(".claude/projects"),
        SessionSource::Claude,
        claude_metadata,
    );
    add_jsonl_records(
        &mut records,
        &home.join(".codex/sessions"),
        SessionSource::Codex,
        codex_metadata,
    );
    add_opencode_records(&mut records);

    if !global {
        records.retain(|record| record.cwd == cwd);
    }
    records.sort_by_key(|record| std::cmp::Reverse(record.modified));
    Ok(records)
}

pub(crate) fn import(source: &SessionSource) -> Result<Vec<AgentMessage>> {
    match source {
        SessionSource::Kiss(path) | SessionSource::Pi(path) => {
            let manager = kiss_coding::SessionManager::open(path)?;
            Ok(manager
                .build_session_context()
                .messages
                .into_iter()
                .filter(|message| {
                    matches!(message, AgentMessage::User(_) | AgentMessage::Assistant(_))
                })
                .collect())
        }
        SessionSource::Claude(path) => parse_jsonl(path, claude_message),
        SessionSource::Codex(path) => parse_jsonl(path, codex_message),
        SessionSource::OpenCode(id) => {
            let output = Command::new("opencode")
                .args(["export", id])
                .output()
                .context("run opencode export")?;
            if !output.status.success() {
                anyhow::bail!("opencode export failed");
            }
            opencode_messages(&serde_json::from_slice(&output.stdout)?)
        }
    }
}

fn add_pi_records(records: &mut Vec<SessionRecord>, root: &Path) -> Result<()> {
    for item in kiss_coding::SessionManager::list_all(root)? {
        records.push(SessionRecord {
            source: SessionSource::Pi(item.path),
            id: item.id.clone(),
            title: item.name.or(item.first_message).unwrap_or(item.id),
            cwd: PathBuf::from(item.cwd),
            modified: item.modified,
            entry_count: Some(item.entry_count),
        });
    }
    Ok(())
}

fn add_jsonl_records(
    records: &mut Vec<SessionRecord>,
    root: &Path,
    source: fn(PathBuf) -> SessionSource,
    metadata: fn(&Path) -> Option<SessionMetadata>,
) {
    for path in jsonl_files(root) {
        let Some((id, title, cwd, entry_count)) = metadata(&path) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|value| value.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        records.push(SessionRecord {
            source: source(path),
            id,
            title,
            cwd,
            modified,
            entry_count: Some(entry_count),
        });
    }
}

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                files.push(path);
            }
        }
    }
    files
}

fn parse_jsonl(
    path: &Path,
    convert: fn(&Value) -> Option<AgentMessage>,
) -> Result<Vec<AgentMessage>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter_map(|value| convert(&value))
        .collect())
}

fn claude_metadata(path: &Path) -> Option<SessionMetadata> {
    let values = jsonl_values(path);
    let metadata = values.iter().find(|value| {
        value.get("sessionId").and_then(Value::as_str).is_some()
            && value.get("cwd").and_then(Value::as_str).is_some()
    })?;
    let id = metadata.get("sessionId")?.as_str()?.to_string();
    let cwd = PathBuf::from(metadata.get("cwd")?.as_str()?);
    let messages = values.iter().filter_map(claude_message).collect::<Vec<_>>();
    let title = first_user_text(&messages).unwrap_or_else(|| id.clone());
    Some((id, title, cwd, messages.len()))
}

fn codex_metadata(path: &Path) -> Option<SessionMetadata> {
    let values = jsonl_values(path);
    let meta = values
        .iter()
        .find(|value| value.get("type").and_then(Value::as_str) == Some("session_meta"))?
        .get("payload")?;
    let id = meta.get("id")?.as_str()?.to_string();
    let cwd = PathBuf::from(meta.get("cwd")?.as_str()?);
    let messages = values.iter().filter_map(codex_message).collect::<Vec<_>>();
    let title = first_user_text(&messages).unwrap_or_else(|| id.clone());
    Some((id, title, cwd, messages.len()))
}

fn jsonl_values(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn claude_message(value: &Value) -> Option<AgentMessage> {
    let role = value.get("type")?.as_str()?;
    if !matches!(role, "user" | "assistant") {
        return None;
    }
    let content = value.get("message")?.get("content")?;
    let text = content_text(content, &["text", "thinking"]);
    message(role, text, "claude-code")
}

fn codex_message(value: &Value) -> Option<AgentMessage> {
    if value.get("type")?.as_str()? != "response_item" {
        return None;
    }
    let payload = value.get("payload")?;
    if payload.get("type")?.as_str()? != "message" {
        return None;
    }
    let role = payload.get("role")?.as_str()?;
    let kinds = if role == "user" {
        ["input_text"].as_slice()
    } else {
        ["output_text"].as_slice()
    };
    message(role, content_text(payload.get("content")?, kinds), "codex")
}

fn content_text(content: &Value, kinds: &[&str]) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| {
            block
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kinds.contains(&kind))
        })
        .filter_map(|block| {
            block
                .get("text")
                .or_else(|| block.get("thinking"))
                .and_then(Value::as_str)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message(role: &str, text: String, source: &str) -> Option<AgentMessage> {
    if text.trim().is_empty() {
        return None;
    }
    match role {
        "user" => Some(AgentMessage::user(text)),
        "assistant" => {
            let mut assistant = AssistantMessage::empty("import", source, source);
            assistant.content.push(ContentBlock::text(text));
            assistant.stop_reason = StopReason::Stop;
            Some(AgentMessage::Assistant(assistant))
        }
        _ => None,
    }
}

fn first_user_text(messages: &[AgentMessage]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        AgentMessage::User(user) => Some(user.content.as_text().chars().take(120).collect()),
        _ => None,
    })
}

fn add_opencode_records(records: &mut Vec<SessionRecord>) {
    let Ok(output) = Command::new("opencode")
        .args(["session", "list", "--format", "json"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(values) = serde_json::from_slice::<Vec<Value>>(&output.stdout) else {
        return;
    };
    for value in values {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(cwd) = value.get("directory").and_then(Value::as_str) else {
            continue;
        };
        let modified = value
            .get("updated")
            .or_else(|| value.get("created"))
            .and_then(Value::as_u64)
            .map(|value| SystemTime::UNIX_EPOCH + Duration::from_millis(value))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        records.push(SessionRecord {
            source: SessionSource::OpenCode(id.to_string()),
            id: id.to_string(),
            title: value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            cwd: PathBuf::from(cwd),
            modified,
            entry_count: None,
        });
    }
}

fn opencode_messages(value: &Value) -> Result<Vec<AgentMessage>> {
    let rows = value
        .get("messages")
        .and_then(Value::as_array)
        .context("OpenCode export has no messages")?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let role = row.get("info")?.get("role")?.as_str()?;
            let text = content_text(row.get("parts")?, &["text"]);
            message(role, text, "opencode")
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_rows_import_text_only() {
        let claude = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "thinking", "thinking": "hidden shape"},
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "name": "Read"}
            ]}
        });
        assert!(
            matches!(claude_message(&claude), Some(AgentMessage::Assistant(message)) if message.text() == "hidden shape\nanswer")
        );

        let codex = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "question"}
            ]}
        });
        assert!(
            matches!(codex_message(&codex), Some(AgentMessage::User(message)) if message.content.as_text() == "question")
        );
    }

    #[test]
    fn opencode_export_imports_text_parts() {
        let export = serde_json::json!({"messages": [
            {"info": {"role": "user"}, "parts": [{"type": "text", "text": "hello"}]},
            {"info": {"role": "assistant"}, "parts": [
                {"type": "text", "text": "world"}, {"type": "tool", "name": "bash"}
            ]}
        ]});
        let messages = opencode_messages(&export).unwrap();
        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[1], AgentMessage::Assistant(message) if message.text() == "world")
        );
    }

    #[test]
    fn foreign_metadata_uses_stored_cwd_and_counts_text_messages() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude.jsonl");
        std::fs::write(
            &claude,
            concat!(
                "{\"type\":\"queue-operation\"}\n",
                "{\"type\":\"user\",\"sessionId\":\"claude-id\",\"cwd\":\"/project\",\"message\":{\"content\":\"hello\"}}\n",
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"answer\"}]}}\n"
            ),
        )
        .unwrap();
        let metadata = claude_metadata(&claude).unwrap();
        assert_eq!(metadata.0, "claude-id");
        assert_eq!(metadata.1, "hello");
        assert_eq!(metadata.2, Path::new("/project"));
        assert_eq!(metadata.3, 2);

        let codex = dir.path().join("codex.jsonl");
        std::fs::write(
            &codex,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/other\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"task\"}]}}\n"
            ),
        )
        .unwrap();
        let metadata = codex_metadata(&codex).unwrap();
        assert_eq!(metadata.0, "codex-id");
        assert_eq!(metadata.1, "task");
        assert_eq!(metadata.2, Path::new("/other"));
        assert_eq!(metadata.3, 1);
    }
}
