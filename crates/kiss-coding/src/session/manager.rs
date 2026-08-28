//! SessionManager: append-only JSONL tree with leaf tracking, branching,
//! context building, and pi-compatible on-disk layout.

use super::entry::{
    EntryBase, SESSION_VERSION, SessionEntry, SessionHeader, iso_now, new_entry_id,
};
use anyhow::{Context as _, Result};
use kiss_agent::{AgentMessage, BranchSummaryMessage, CompactionSummaryMessage, CustomMessage};
use kiss_ai::{Model, ThinkingLevel, Usage};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SessionManager {
    header: SessionHeader,
    entries: Vec<SessionEntry>,
    index: HashMap<String, usize>,
    leaf_id: Option<String>,
    context_revision: u64,
    file: Option<PathBuf>,
    append_file: Option<File>,
    session_dir: PathBuf,
    cwd: PathBuf,
}

/// Slug a working directory the way pi does: `/a/b` -> `--a-b--`.
pub fn cwd_slug(cwd: &Path) -> String {
    let s = cwd.display().to_string().replace(['/', '\\', ':'], "-");
    format!("-{s}-")
}

pub fn default_session_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kiss/agent/sessions")
}

#[derive(Debug, Clone)]
pub struct SessionListing {
    pub path: PathBuf,
    pub id: String,
    pub name: Option<String>,
    pub first_message: Option<String>,
    pub cwd: String,
    pub modified: std::time::SystemTime,
    pub entry_count: usize,
}

impl SessionManager {
    // ----- creation -----------------------------------------------------

    pub fn create(cwd: &Path, session_dir: Option<PathBuf>) -> Result<Self> {
        let session_dir = session_dir.unwrap_or_else(default_session_dir);
        let mut manager = Self::in_memory(cwd);
        manager.session_dir = session_dir;
        manager.assign_new_file()?;
        Ok(manager)
    }

    pub fn in_memory(cwd: &Path) -> Self {
        SessionManager {
            header: SessionHeader {
                entry_type: "session".into(),
                version: SESSION_VERSION,
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: iso_now(),
                cwd: cwd.display().to_string(),
                parent_session: None,
                extra: Map::new(),
            },
            entries: Vec::new(),
            index: HashMap::new(),
            leaf_id: None,
            context_revision: 0,
            file: None,
            append_file: None,
            session_dir: default_session_dir(),
            cwd: cwd.to_path_buf(),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("open session {}", path.display()))?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header_line = lines.next().context("empty session file")?;
        let header: SessionHeader =
            serde_json::from_str(header_line).context("parse session header")?;
        let cwd = PathBuf::from(&header.cwd);
        let mut manager = SessionManager {
            header,
            entries: Vec::new(),
            index: HashMap::new(),
            leaf_id: None,
            context_revision: 0,
            file: Some(path.to_path_buf()),
            append_file: None,
            session_dir: path
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(default_session_dir),
            cwd,
        };
        for line in lines {
            match serde_json::from_str::<SessionEntry>(line) {
                Ok(entry) => manager.insert_entry(entry),
                Err(err) => eprintln!("warning: skipping malformed session line: {err}"),
            }
        }
        manager.leaf_id = manager.entries.last().map(|e| e.id().to_string());
        manager.reopen_append_file()?;
        Ok(manager)
    }

    pub fn continue_recent(cwd: &Path, session_dir: Option<PathBuf>) -> Result<Self> {
        let dir = session_dir.clone().unwrap_or_else(default_session_dir);
        let listings = Self::list(cwd, &dir)?;
        match listings.first() {
            Some(listing) => Self::open(&listing.path),
            None => Self::create(cwd, session_dir),
        }
    }

    pub fn create_sibling(&self) -> Result<Self> {
        if self.is_persisted() {
            Self::create(&self.cwd, Some(self.session_dir.clone()))
        } else {
            let mut manager = Self::in_memory(&self.cwd);
            manager.session_dir = self.session_dir.clone();
            Ok(manager)
        }
    }

    /// Fork: copy the source session's entries into a new file for `cwd`.
    pub fn fork_from(source: &Path, cwd: &Path, session_dir: Option<PathBuf>) -> Result<Self> {
        let origin = Self::open(source)?;
        let mut manager = Self::create(cwd, session_dir)?;
        manager.header.parent_session = Some(source.display().to_string());
        manager.rewrite_file()?;
        for entry in &origin.entries {
            manager.insert_entry(entry.clone());
            manager.append_line(&serde_json::to_string(entry)?)?;
        }
        manager.leaf_id = origin.leaf_id.clone();
        Ok(manager)
    }

    /// Copy one active branch into a new session file.
    ///
    /// `through` selects the last entry to inspect. When `include_through` is
    /// false, the new session ends at that entry's parent. This lets an
    /// interactive caller put a selected user message back in the editor.
    pub fn fork_active_branch(&self, through: Option<&str>, include_through: bool) -> Result<Self> {
        if let Some(entry_id) = through
            && self.get_entry(entry_id).is_none()
        {
            anyhow::bail!("unknown entry id {entry_id}");
        }

        let mut branch = self.branch_entries(through);
        if !include_through && through.is_some() {
            branch.pop();
        }

        let mut manager = self.create_sibling()?;
        manager.header.parent_session = self.file.as_ref().map(|path| path.display().to_string());
        manager.entries.clear();
        manager.index.clear();
        manager.leaf_id = None;
        for entry in branch {
            let entry = entry.clone();
            manager.leaf_id = Some(entry.id().to_string());
            manager.insert_entry(entry);
        }
        manager.rewrite_file()?;
        Ok(manager)
    }

    // ----- listing ------------------------------------------------------

    pub fn list(cwd: &Path, session_dir: &Path) -> Result<Vec<SessionListing>> {
        let dir = session_dir.join(cwd_slug(cwd));
        let mut out = Self::list_dir(&dir)?;
        out.sort_by_key(|listing| std::cmp::Reverse(listing.modified));
        Ok(out)
    }

    pub fn list_all(session_dir: &Path) -> Result<Vec<SessionListing>> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(session_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    out.extend(Self::list_dir(&entry.path())?);
                }
            }
        }
        out.sort_by_key(|listing| std::cmp::Reverse(listing.modified));
        Ok(out)
    }

    fn list_dir(dir: &Path) -> Result<Vec<SessionListing>> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(out);
        };
        for file in entries.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(listing) = Self::peek(&path) {
                out.push(listing);
            }
        }
        Ok(out)
    }

    /// Cheap listing: header + scan for name/first user message.
    fn peek(path: &Path) -> Option<SessionListing> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: SessionHeader = serde_json::from_str(lines.next()?).ok()?;
        let mut name = None;
        let mut first_message = None;
        let mut entry_count = 0usize;
        for line in lines {
            entry_count += 1;
            if name.is_none()
                && line.contains("\"session_info\"")
                && let Ok(v) = serde_json::from_str::<Value>(line)
                && v["type"] == "session_info"
            {
                name = v["name"].as_str().map(String::from);
            }
            if first_message.is_none()
                && line.contains("\"message\"")
                && let Ok(v) = serde_json::from_str::<Value>(line)
                && v["type"] == "message"
                && v["message"]["role"] == "user"
            {
                let content = &v["message"]["content"];
                let text = content.as_str().map(String::from).or_else(|| {
                    content
                        .as_array()
                        .and_then(|a| a.iter().find(|b| b["type"] == "text"))
                        .and_then(|b| b["text"].as_str().map(String::from))
                });
                first_message = text.map(|t| t.chars().take(120).collect());
            }
        }
        let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
        Some(SessionListing {
            path: path.to_path_buf(),
            id: header.id,
            name,
            first_message,
            cwd: header.cwd,
            modified,
            entry_count,
        })
    }

    /// Find a session file by partial UUID across all projects.
    pub fn find_by_id(session_dir: &Path, partial: &str) -> Result<Option<PathBuf>> {
        for listing in Self::list_all(session_dir)? {
            if listing.id.starts_with(partial) {
                return Ok(Some(listing.path));
            }
        }
        Ok(None)
    }

    // ----- appends ------------------------------------------------------

    fn make_base(&self) -> EntryBase {
        EntryBase {
            id: new_entry_id(),
            parent_id: self.leaf_id.clone(),
            timestamp: iso_now(),
        }
    }

    pub fn append_message(&mut self, message: AgentMessage) -> Result<String> {
        self.append_entry(|base| SessionEntry::Message {
            base,
            message,
            extra: Map::new(),
        })
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> Result<String> {
        let (provider, model_id) = (provider.to_string(), model_id.to_string());
        self.append_entry(|base| SessionEntry::ModelChange {
            base,
            provider,
            model_id,
            extra: Map::new(),
        })
    }

    pub fn append_thinking_level_change(&mut self, level: ThinkingLevel) -> Result<String> {
        self.append_entry(|base| SessionEntry::ThinkingLevelChange {
            base,
            thinking_level: level,
            extra: Map::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_compaction(
        &mut self,
        summary: String,
        tokens_before: u64,
        retained_tail: Vec<AgentMessage>,
        usage: Option<Usage>,
        details: Option<Value>,
    ) -> Result<String> {
        self.append_entry(|base| SessionEntry::Compaction {
            base,
            summary,
            tokens_before,
            first_kept_entry_id: None,
            retained_tail: Some(retained_tail),
            usage,
            details,
            extra: Map::new(),
        })
    }

    pub fn append_session_info(&mut self, name: &str) -> Result<String> {
        let name = name.to_string();
        self.append_entry(|base| SessionEntry::SessionInfo {
            base,
            name: Some(name),
            extra: Map::new(),
        })
    }

    pub fn append_label(&mut self, target_id: &str, label: Option<String>) -> Result<String> {
        let target_id = target_id.to_string();
        self.append_entry(|base| SessionEntry::Label {
            base,
            target_id,
            label,
            extra: Map::new(),
        })
    }

    pub fn append_custom(&mut self, custom_type: &str, data: Option<Value>) -> Result<String> {
        let custom_type = custom_type.to_string();
        self.append_entry(|base| SessionEntry::Custom {
            base,
            custom_type,
            data,
            extra: Map::new(),
        })
    }

    fn append_entry(&mut self, make: impl FnOnce(EntryBase) -> SessionEntry) -> Result<String> {
        let entry = make(self.make_base());
        let id = entry.id().to_string();
        let line = serde_json::to_string(&entry)?;
        self.insert_entry(entry);
        self.leaf_id = Some(id.clone());
        self.append_line(&line)?;
        Ok(id)
    }

    fn insert_entry(&mut self, entry: SessionEntry) {
        self.index
            .insert(entry.id().to_string(), self.entries.len());
        self.entries.push(entry);
        self.context_revision = self.context_revision.wrapping_add(1);
    }

    fn append_line(&mut self, line: &str) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        if self.append_file.is_none() {
            self.reopen_append_file()?;
        }
        let file = self
            .append_file
            .as_mut()
            .expect("a persisted session has an append file");
        writeln!(file, "{line}")?;
        Ok(())
    }

    fn reopen_append_file(&mut self) -> Result<()> {
        self.append_file = match &self.file {
            Some(path) => Some(std::fs::OpenOptions::new().append(true).open(path)?),
            None => None,
        };
        Ok(())
    }

    fn assign_new_file(&mut self) -> Result<()> {
        let dir = self.session_dir.join(cwd_slug(&self.cwd));
        std::fs::create_dir_all(&dir)?;
        let stamp = chrono::Utc::now().format("%Y-%m-%d-%H-%M-%S");
        let path = dir.join(format!("{stamp}_{}.jsonl", self.header.id));
        self.file = Some(path);
        self.rewrite_file()
    }

    /// Rewrite the whole file (header + entries). Used at creation and fork.
    fn rewrite_file(&mut self) -> Result<()> {
        let Some(path) = self.file.clone() else {
            return Ok(());
        };
        self.append_file = None;
        let mut out = serde_json::to_string(&self.header)?;
        out.push('\n');
        for entry in &self.entries {
            out.push_str(&serde_json::to_string(entry)?);
            out.push('\n');
        }
        std::fs::write(path, out)?;
        self.reopen_append_file()?;
        Ok(())
    }

    // ----- tree ---------------------------------------------------------

    pub fn get_entry(&self, id: &str) -> Option<&SessionEntry> {
        self.index.get(id).map(|&i| &self.entries[i])
    }

    pub fn entries(&self) -> &[SessionEntry] {
        &self.entries
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.leaf_id.as_deref()
    }

    /// Monotonic value for cache invalidation after context or tree changes.
    pub fn context_revision(&self) -> u64 {
        self.context_revision
    }

    /// Move the leaf to an earlier entry (in-place branching).
    pub fn branch(&mut self, entry_id: &str) -> Result<()> {
        if self.get_entry(entry_id).is_none() {
            anyhow::bail!("unknown entry id {entry_id}");
        }
        self.leaf_id = Some(entry_id.to_string());
        self.context_revision = self.context_revision.wrapping_add(1);
        Ok(())
    }

    /// Reset the leaf to before any entries.
    pub fn reset_leaf(&mut self) {
        self.leaf_id = None;
        self.context_revision = self.context_revision.wrapping_add(1);
    }

    /// Branch to `entry_id` and record a summary of the abandoned path.
    pub fn branch_with_summary(
        &mut self,
        entry_id: Option<&str>,
        from_id: &str,
        summary: String,
        usage: Option<Usage>,
        details: Option<Value>,
    ) -> Result<String> {
        if let Some(entry_id) = entry_id {
            self.branch(entry_id)?;
        } else {
            self.reset_leaf();
        }
        let from_id = from_id.to_string();
        self.append_entry(|base| SessionEntry::BranchSummary {
            base,
            from_id,
            summary,
            usage,
            details,
            extra: Map::new(),
        })
    }

    pub fn children(&self, parent_id: Option<&str>) -> Vec<&SessionEntry> {
        self.entries
            .iter()
            .filter(|e| e.parent_id() == parent_id)
            .collect()
    }

    /// Walk leaf -> root; returns entries in root-first order.
    pub fn branch_entries(&self, from: Option<&str>) -> Vec<&SessionEntry> {
        let mut out = Vec::new();
        let mut cursor = from.or(self.leaf_id.as_deref()).map(String::from);
        while let Some(id) = cursor {
            let Some(entry) = self.get_entry(&id) else {
                break;
            };
            out.push(entry);
            cursor = entry.parent_id().map(String::from);
        }
        out.reverse();
        out
    }

    /// LLM messages on one branch after an optional common ancestor.
    pub fn branch_messages_after(&self, from: &str, ancestor: Option<&str>) -> Vec<AgentMessage> {
        let path = self.branch_entries(Some(from));
        let start = ancestor
            .and_then(|ancestor| path.iter().position(|entry| entry.id() == ancestor))
            .map_or(0, |position| position + 1);
        let mut messages = Vec::new();
        for entry in &path[start..] {
            match entry {
                SessionEntry::Message { message, .. } => messages.push(message.clone()),
                SessionEntry::Compaction {
                    summary,
                    tokens_before,
                    base,
                    ..
                } => messages.push(AgentMessage::CompactionSummary(CompactionSummaryMessage {
                    summary: summary.clone(),
                    tokens_before: *tokens_before,
                    timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                        .map(|time| time.timestamp_millis())
                        .unwrap_or_else(|_| kiss_ai::now_ms()),
                })),
                SessionEntry::BranchSummary {
                    summary,
                    from_id,
                    base,
                    ..
                } => messages.push(AgentMessage::BranchSummary(BranchSummaryMessage {
                    summary: summary.clone(),
                    from_id: from_id.clone(),
                    timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                        .map(|time| time.timestamp_millis())
                        .unwrap_or_else(|_| kiss_ai::now_ms()),
                })),
                SessionEntry::CustomMessage {
                    custom_type,
                    content,
                    display,
                    details,
                    base,
                    ..
                } => messages.push(AgentMessage::Custom(CustomMessage {
                    custom_type: custom_type.clone(),
                    content: content.clone(),
                    display: *display,
                    details: details.clone(),
                    timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                        .map(|time| time.timestamp_millis())
                        .unwrap_or_else(|_| kiss_ai::now_ms()),
                })),
                _ => {}
            }
        }
        messages
    }

    /// Latest session name on the active path.
    pub fn session_name(&self) -> Option<String> {
        self.branch_entries(None)
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEntry::SessionInfo { name, .. } => name.clone(),
                _ => None,
            })
    }

    pub fn label_of(&self, target: &str) -> Option<String> {
        // Labels apply tree-wide; last write wins.
        self.entries
            .iter()
            .rev()
            .find_map(|e| match e {
                SessionEntry::Label {
                    target_id, label, ..
                } if target_id == target => Some(label.clone()),
                _ => None,
            })
            .flatten()
    }

    // ----- context building --------------------------------------------

    /// Active-branch entries with compaction applied: on a compaction with a
    /// retained tail, the checkpoint replaces everything before it.
    pub fn build_context_entries(&self) -> Vec<&SessionEntry> {
        let path = self.branch_entries(None);
        let compaction_pos = path
            .iter()
            .rposition(|e| matches!(e, SessionEntry::Compaction { .. }));
        match compaction_pos {
            Some(pos) => {
                let SessionEntry::Compaction {
                    retained_tail,
                    first_kept_entry_id,
                    ..
                } = path[pos]
                else {
                    unreachable!()
                };
                let mut out: Vec<&SessionEntry> = Vec::new();
                out.push(path[pos]);
                if retained_tail.is_none() {
                    // Legacy: include entries from firstKeptEntryId up to the
                    // compaction entry.
                    if let Some(first_kept) = first_kept_entry_id
                        && let Some(start) = path.iter().position(|e| e.id() == first_kept)
                    {
                        for e in &path[start..pos] {
                            out.push(e);
                        }
                    }
                }
                for e in &path[pos + 1..] {
                    out.push(e);
                }
                out
            }
            None => path,
        }
    }

    /// Messages + model + thinking level for the next LLM call.
    pub fn build_session_context(&self) -> SessionContext {
        // Model/thinking come from the full active path.
        let path = self.branch_entries(None);
        let mut model: Option<(String, String)> = None;
        let mut thinking: Option<ThinkingLevel> = None;
        for entry in &path {
            match entry {
                SessionEntry::ModelChange {
                    provider, model_id, ..
                } => {
                    model = Some((provider.clone(), model_id.clone()));
                }
                SessionEntry::ThinkingLevelChange { thinking_level, .. } => {
                    thinking = Some(*thinking_level)
                }
                _ => {}
            }
        }

        let mut messages: Vec<AgentMessage> = Vec::new();
        for entry in self.build_context_entries() {
            match entry {
                SessionEntry::Message { message, .. } => messages.push(message.clone()),
                SessionEntry::Compaction {
                    summary,
                    tokens_before,
                    retained_tail,
                    base,
                    ..
                } => {
                    messages.push(AgentMessage::CompactionSummary(CompactionSummaryMessage {
                        summary: summary.clone(),
                        tokens_before: *tokens_before,
                        timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                            .map(|t| t.timestamp_millis())
                            .unwrap_or_else(|_| kiss_ai::now_ms()),
                    }));
                    if let Some(tail) = retained_tail {
                        messages.extend(tail.iter().cloned());
                    }
                }
                SessionEntry::BranchSummary {
                    summary,
                    from_id,
                    base,
                    ..
                } => {
                    messages.push(AgentMessage::BranchSummary(BranchSummaryMessage {
                        summary: summary.clone(),
                        from_id: from_id.clone(),
                        timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                            .map(|t| t.timestamp_millis())
                            .unwrap_or_else(|_| kiss_ai::now_ms()),
                    }));
                }
                SessionEntry::CustomMessage {
                    custom_type,
                    content,
                    display,
                    details,
                    base,
                    ..
                } => {
                    messages.push(AgentMessage::Custom(CustomMessage {
                        custom_type: custom_type.clone(),
                        content: content.clone(),
                        display: *display,
                        details: details.clone(),
                        timestamp: chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                            .map(|t| t.timestamp_millis())
                            .unwrap_or_else(|_| kiss_ai::now_ms()),
                    }));
                }
                _ => {}
            }
        }
        SessionContext {
            messages,
            model,
            thinking_level: thinking,
        }
    }

    /// Restore provider-native OpenAI history from the latest compaction on
    /// the active branch. Completed turns from another model are excluded.
    pub fn build_openai_compaction_context(
        &self,
        model: &Model,
    ) -> Option<OpenAICompactionContext> {
        let path = self.branch_entries(None);
        let compaction_pos = path
            .iter()
            .rposition(|entry| matches!(entry, SessionEntry::Compaction { .. }))?;
        let SessionEntry::Compaction {
            details: Some(details),
            ..
        } = path[compaction_pos]
        else {
            return None;
        };
        let remote = details.get("remoteCompaction")?.as_object()?;
        if remote.get("version")?.as_u64()? != 2
            || remote.get("provider")?.as_str()? != "openai-responses-compaction"
            || remote.get("modelKey")?.as_str()?
                != kiss_ai::api::openai_compaction::model_key(model)
        {
            return None;
        }
        let replacement_history = remote
            .get("replacementHistory")?
            .as_array()?
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str).is_some())
            .cloned()
            .collect::<Vec<_>>();
        if replacement_history.is_empty()
            || !replacement_history
                .iter()
                .any(|item| item["type"] == "compaction")
        {
            return None;
        }

        let mut messages = Vec::new();
        let mut pending = Vec::new();
        for entry in &path[compaction_pos + 1..] {
            let Some(message) = context_message_from_entry(entry) else {
                continue;
            };
            match &message {
                AgentMessage::Assistant(assistant) => {
                    if assistant.provider == model.provider && assistant.model == model.id {
                        messages.append(&mut pending);
                        messages.push(message);
                    } else {
                        pending.clear();
                    }
                }
                _ => pending.push(message),
            }
        }
        // Pending messages are the current unfinished turn. They have not yet
        // received an assistant completion from a possibly different model.
        messages.append(&mut pending);
        Some(OpenAICompactionContext {
            replacement_history,
            messages,
        })
    }

    // ----- info ---------------------------------------------------------

    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    pub fn session_id(&self) -> &str {
        &self.header.id
    }

    pub fn session_file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn is_persisted(&self) -> bool {
        self.file.is_some()
    }

    /// Serialize the full append-only session file format.
    pub fn to_jsonl(&self) -> Result<String> {
        let mut output = serde_json::to_string(&self.header)?;
        output.push('\n');
        for entry in &self.entries {
            output.push_str(&serde_json::to_string(entry)?);
            output.push('\n');
        }
        Ok(output)
    }
}

#[derive(Debug, Clone)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub model: Option<(String, String)>,
    pub thinking_level: Option<ThinkingLevel>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OpenAICompactionContext {
    pub replacement_history: Vec<Value>,
    pub messages: Vec<AgentMessage>,
}

fn context_message_from_entry(entry: &SessionEntry) -> Option<AgentMessage> {
    match entry {
        SessionEntry::Message { message, .. } => Some(message.clone()),
        SessionEntry::BranchSummary {
            summary,
            from_id,
            base,
            ..
        } => Some(AgentMessage::BranchSummary(BranchSummaryMessage {
            summary: summary.clone(),
            from_id: from_id.clone(),
            timestamp: entry_timestamp(base),
        })),
        SessionEntry::CustomMessage {
            custom_type,
            content,
            display,
            details,
            base,
            ..
        } => Some(AgentMessage::Custom(CustomMessage {
            custom_type: custom_type.clone(),
            content: content.clone(),
            display: *display,
            details: details.clone(),
            timestamp: entry_timestamp(base),
        })),
        _ => None,
    }
}

fn entry_timestamp(base: &EntryBase) -> i64 {
    chrono::DateTime::parse_from_rfc3339(&base.timestamp)
        .map(|time| time.timestamp_millis())
        .unwrap_or_else(|_| kiss_ai::now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::{AssistantMessage, ContentBlock, ModelCost, StopReason};
    use std::collections::BTreeMap;

    fn manager() -> SessionManager {
        SessionManager::in_memory(Path::new("/tmp/project"))
    }

    fn openai_model() -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT test".into(),
            api: "openai-responses".into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 1_000,
            compat: None,
            headers: BTreeMap::new(),
        }
    }

    fn assistant(provider: &str, model: &str, text: &str) -> AgentMessage {
        let mut message = AssistantMessage::empty("openai-responses", provider, model);
        message.content.push(ContentBlock::text(text));
        message.stop_reason = StopReason::Stop;
        AgentMessage::Assistant(message)
    }

    fn remote_details(model: &Model) -> Value {
        kiss_ai::api::openai_compaction::build_remote_compaction_details(
            model,
            &kiss_ai::api::openai_compaction::RemoteCompactionResult {
                replacement_history: vec![
                    serde_json::json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "old request"}]
                    }),
                    serde_json::json!({
                        "type": "compaction",
                        "encrypted_content": "opaque"
                    }),
                ],
                usage: None,
            },
        )
    }

    #[test]
    fn linear_appends_build_context() {
        let mut m = manager();
        m.append_message(AgentMessage::user("one")).unwrap();
        m.append_message(AgentMessage::user("two")).unwrap();
        let ctx = m.build_session_context();
        assert_eq!(ctx.messages.len(), 2);
    }

    #[test]
    fn branching_moves_leaf() {
        let mut m = manager();
        let a = m.append_message(AgentMessage::user("a")).unwrap();
        let _b = m.append_message(AgentMessage::user("b")).unwrap();
        m.branch(&a).unwrap();
        let c = m.append_message(AgentMessage::user("c")).unwrap();
        let path: Vec<String> = m
            .branch_entries(None)
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert_eq!(path, vec![a, c]);
        assert_eq!(m.children(path.first().map(String::as_str)).len(), 2);
    }

    #[test]
    fn branch_summary_can_move_to_root() {
        let mut m = manager();
        let first = m.append_message(AgentMessage::user("first")).unwrap();
        let second = m.append_message(AgentMessage::user("second")).unwrap();
        let messages = m.branch_messages_after(&second, Some(&first));
        assert!(matches!(
            messages.as_slice(),
            [AgentMessage::User(user)] if user.content.as_text() == "second"
        ));

        let summary = m
            .branch_with_summary(None, &second, "old work".into(), None, None)
            .unwrap();
        assert_eq!(m.leaf_id(), Some(summary.as_str()));
        assert_eq!(m.get_entry(&summary).unwrap().parent_id(), None);
    }

    #[test]
    fn compaction_with_retained_tail_replaces_history() {
        let mut m = manager();
        m.append_message(AgentMessage::user("old1")).unwrap();
        m.append_message(AgentMessage::user("old2")).unwrap();
        m.append_compaction(
            "summary of old".into(),
            5000,
            vec![AgentMessage::user("kept")],
            None,
            None,
        )
        .unwrap();
        m.append_message(AgentMessage::user("new")).unwrap();
        let ctx = m.build_session_context();
        let texts: Vec<String> = ctx
            .messages
            .iter()
            .map(|msg| match msg {
                AgentMessage::User(u) => u.content.as_text(),
                AgentMessage::CompactionSummary(c) => format!("summary:{}", c.summary),
                other => other.role().to_string(),
            })
            .collect();
        assert_eq!(texts, vec!["summary:summary of old", "kept", "new"]);
    }

    #[test]
    fn matching_openai_model_restores_remote_history_and_trailing_turns() {
        let model = openai_model();
        let mut manager = manager();
        manager.append_message(AgentMessage::user("old")).unwrap();
        manager
            .append_compaction(
                "portable summary".into(),
                2_000,
                vec![AgentMessage::user("local tail")],
                None,
                Some(remote_details(&model)),
            )
            .unwrap();
        manager
            .append_message(AgentMessage::user("after compaction"))
            .unwrap();
        manager
            .append_message(assistant("openai", "gpt-test", "continued"))
            .unwrap();

        let restored = manager.build_openai_compaction_context(&model).unwrap();
        assert_eq!(restored.replacement_history.len(), 2);
        assert_eq!(restored.replacement_history[1]["type"], "compaction");
        assert_eq!(restored.messages.len(), 2);
        assert!(matches!(
            &restored.messages[0],
            AgentMessage::User(user) if user.content.as_text() == "after compaction"
        ));
    }

    #[test]
    fn remote_history_drops_other_model_turns_but_keeps_current_prompt() {
        let model = openai_model();
        let mut manager = manager();
        manager
            .append_compaction(
                "portable summary".into(),
                2_000,
                vec![],
                None,
                Some(remote_details(&model)),
            )
            .unwrap();
        manager
            .append_message(AgentMessage::user("question for Claude"))
            .unwrap();
        manager
            .append_message(assistant("anthropic", "claude-test", "Claude answer"))
            .unwrap();
        manager
            .append_message(AgentMessage::user("back to OpenAI"))
            .unwrap();

        let restored = manager.build_openai_compaction_context(&model).unwrap();
        assert_eq!(restored.messages.len(), 1);
        assert!(matches!(
            &restored.messages[0],
            AgentMessage::User(user) if user.content.as_text() == "back to OpenAI"
        ));

        let mut other_model = model.clone();
        other_model.id = "gpt-other".into();
        assert!(
            manager
                .build_openai_compaction_context(&other_model)
                .is_none()
        );
    }

    #[test]
    fn persist_and_reopen_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut m = SessionManager::create(&cwd, Some(dir.path().join("sessions"))).unwrap();
        m.append_message(AgentMessage::user("persisted")).unwrap();
        m.append_session_info("my task").unwrap();
        let path = m.session_file().unwrap().to_path_buf();

        let reopened = SessionManager::open(&path).unwrap();
        assert_eq!(reopened.entries().len(), 2);
        assert_eq!(reopened.session_name().as_deref(), Some("my task"));
        let listings = SessionManager::list(&cwd, &dir.path().join("sessions")).unwrap();
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].name.as_deref(), Some("my task"));
    }

    #[test]
    fn model_and_thinking_tracked() {
        let mut m = manager();
        m.append_model_change("anthropic", "claude-sonnet-4-5")
            .unwrap();
        m.append_thinking_level_change(ThinkingLevel::High).unwrap();
        let ctx = m.build_session_context();
        assert_eq!(
            ctx.model,
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(ctx.thinking_level, Some(ThinkingLevel::High));
        assert!(ctx.messages.is_empty());
    }

    #[test]
    fn active_branch_fork_excludes_selected_user_message_for_editing() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut source = SessionManager::create(&cwd, Some(dir.path().join("sessions"))).unwrap();
        let first = source.append_message(AgentMessage::user("first")).unwrap();
        let selected = source
            .append_message(AgentMessage::user("selected"))
            .unwrap();
        source
            .append_message(AgentMessage::user("abandoned"))
            .unwrap();

        let fork = source.fork_active_branch(Some(&selected), false).unwrap();
        assert_eq!(fork.leaf_id(), Some(first.as_str()));
        assert_eq!(fork.entries().len(), 1);
        assert_ne!(fork.session_id(), source.session_id());
        assert_eq!(
            fork.header().parent_session.as_deref(),
            source
                .session_file()
                .map(|path| path.to_string_lossy())
                .as_deref()
        );
    }

    #[test]
    fn jsonl_serialization_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("session.jsonl");
        let mut source = manager();
        source
            .append_message(AgentMessage::user("portable"))
            .unwrap();
        std::fs::write(&target, source.to_jsonl().unwrap()).unwrap();
        let reopened = SessionManager::open(&target).unwrap();
        assert_eq!(reopened.entries().len(), 1);
    }

    #[test]
    fn sibling_of_in_memory_session_stays_in_memory() {
        let source = manager();
        let sibling = source.create_sibling().unwrap();
        assert!(!sibling.is_persisted());
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_session_append() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let session_dir = dir.path().join("sessions");
        kiss_bench::measure(
            "session_append_500",
            11,
            1,
            "500_persisted_user_messages",
            || {
                let mut manager = SessionManager::create(&cwd, Some(session_dir.clone())).unwrap();
                for index in 0..500 {
                    manager
                        .append_message(AgentMessage::user(format!(
                            "message {index}: deterministic session append benchmark"
                        )))
                        .unwrap();
                }
                manager.entries().len()
            },
        );
    }
}
