use anyhow::Result;
use kiss_agent::AgentMessage;
use kiss_ai::{AssistantMessage, ContentBlock, StopReason};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Metadata needed for the picker without loading the full session history.
#[derive(Debug, Clone)]
struct SessionMetadata {
    id: String,
    title: String,
    cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) enum SessionSource {
    Kiss(PathBuf),
    Pi(PathBuf),
    Claude(PathBuf),
    Codex(PathBuf),
}

impl SessionSource {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Kiss(_) => "KISS",
            Self::Pi(_) => "Pi",
            Self::Claude(_) => "Claude Code",
            Self::Codex(_) => "Codex",
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
    /// None for foreign sessions when counting would require a full scan.
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

    let project_cwd = (!global).then_some(cwd);
    add_pi_records(&mut records, &home.join(".pi/agent/sessions"))?;
    add_jsonl_records(
        &mut records,
        &home.join(".claude/projects"),
        SessionSource::Claude,
        claude_metadata,
        project_cwd,
    );
    add_jsonl_records(
        &mut records,
        &home.join(".codex/sessions"),
        SessionSource::Codex,
        codex_metadata,
        project_cwd,
    );
    for record in &mut records {
        record.title = compact_title(&record.title);
    }
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
    metadata: fn(&Path, Option<&Path>) -> Option<SessionMetadata>,
    cwd: Option<&Path>,
) {
    for path in jsonl_files(root) {
        let Some(metadata) = metadata(&path, cwd) else {
            continue;
        };
        let modified = std::fs::metadata(&path)
            .and_then(|value| value.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        records.push(SessionRecord {
            source: source(path),
            id: metadata.id,
            title: metadata.title,
            cwd: metadata.cwd,
            modified,
            entry_count: None,
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
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = String::new();
    let mut messages = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if let Ok(value) = serde_json::from_str(&line)
            && let Some(message) = convert(&value)
        {
            messages.push(message);
        }
    }
    Ok(messages)
}

fn claude_metadata(path: &Path, cwd_filter: Option<&Path>) -> Option<SessionMetadata> {
    scan_jsonl_metadata(
        path,
        cwd_filter,
        |value| {
            Some((
                value.get("sessionId")?.as_str()?.to_owned(),
                PathBuf::from(value.get("cwd")?.as_str()?),
            ))
        },
        claude_message,
    )
}

fn codex_metadata(path: &Path, cwd_filter: Option<&Path>) -> Option<SessionMetadata> {
    scan_jsonl_metadata(
        path,
        cwd_filter,
        |value| {
            let meta = value
                .get("payload")
                .filter(|_| value.get("type").and_then(Value::as_str) == Some("session_meta"))?;
            Some((
                meta.get("id")?.as_str()?.to_owned(),
                PathBuf::from(meta.get("cwd")?.as_str()?),
            ))
        },
        codex_message,
    )
}

fn scan_jsonl_metadata(
    path: &Path,
    cwd_filter: Option<&Path>,
    metadata: fn(&Value) -> Option<(String, PathBuf)>,
    convert: fn(&Value) -> Option<AgentMessage>,
) -> Option<SessionMetadata> {
    let file = File::open(path).ok()?;
    let mut session = None;
    let mut title = None;
    for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if session.is_none()
            && let Some((id, cwd)) = metadata(&value)
        {
            if let Some(filter) = cwd_filter
                && cwd != filter
            {
                return None;
            }
            session = Some((id, cwd));
        }
        if title.is_none()
            && let Some(AgentMessage::User(user)) = convert(&value)
        {
            title = Some(user.content.as_text().chars().take(120).collect());
        }
        if session.is_some() && title.is_some() {
            break;
        }
    }
    let (id, cwd) = session?;
    Some(SessionMetadata {
        title: title.unwrap_or_else(|| id.clone()),
        id,
        cwd,
    })
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
    let text = content_text(payload.get("content")?, kinds);
    if role == "user" && is_codex_context(&text) {
        return None;
    }
    message(role, text, "codex")
}

fn is_codex_context(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("# AGENTS.md instructions for ")
        || text.starts_with("<environment_context>")
        || text.starts_with("<permissions instructions>")
        || text.starts_with("<collaboration_mode>")
        || text.starts_with("<model_switch>")
        || text.starts_with("<turn_aborted>")
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

fn compact_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
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
    fn codex_context_is_not_a_session_title() {
        let context = serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "# AGENTS.md instructions for /project\n\n<INSTRUCTIONS>"}
            ]}
        });
        assert!(codex_message(&context).is_none());
        assert_eq!(compact_title("one\n\n two   three"), "one two three");
    }

    #[test]
    fn foreign_metadata_uses_stored_cwd_and_title() {
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
        let metadata = claude_metadata(&claude, None).unwrap();
        assert_eq!(metadata.id, "claude-id");
        assert_eq!(metadata.title, "hello");
        assert_eq!(metadata.cwd, Path::new("/project"));
        assert!(claude_metadata(&claude, Some(Path::new("/project"))).is_some());
        assert!(claude_metadata(&claude, Some(Path::new("/other"))).is_none());

        let codex = dir.path().join("codex.jsonl");
        std::fs::write(
            &codex,
            concat!(
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"codex-id\",\"cwd\":\"/other\"}}\n",
                "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"task\"}]}}\n"
            ),
        )
        .unwrap();
        let metadata = codex_metadata(&codex, None).unwrap();
        assert_eq!(metadata.id, "codex-id");
        assert_eq!(metadata.title, "task");
        assert_eq!(metadata.cwd, Path::new("/other"));
    }
}
