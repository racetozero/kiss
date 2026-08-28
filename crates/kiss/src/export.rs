//! Session -> standalone HTML export.

use anyhow::Result;
use kiss_agent::AgentMessage;
use kiss_coding::{SessionEntry, SessionManager};
use std::path::Path;

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn export_html(manager: &SessionManager, target: &Path) -> Result<()> {
    let mut body = String::new();
    for entry in manager.build_context_entries() {
        match entry {
            SessionEntry::Message { message, .. } => {
                match message {
                    AgentMessage::User(u) => {
                        body.push_str(&format!(
                            "<div class=\"msg user\"><h4>user</h4><pre>{}</pre></div>\n",
                            escape(&u.content.as_text())
                        ));
                    }
                    AgentMessage::Assistant(a) => {
                        let mut inner = String::new();
                        for block in &a.content {
                            match block {
                                kiss_ai::ContentBlock::Text { text, .. } => {
                                    inner.push_str(&format!(
                                        "<pre class=\"text\">{}</pre>",
                                        escape(text)
                                    ));
                                }
                                kiss_ai::ContentBlock::Thinking { thinking, .. } => {
                                    inner.push_str(&format!("<details><summary>thinking</summary><pre>{}</pre></details>", escape(thinking)));
                                }
                                kiss_ai::ContentBlock::ToolCall(tc) => {
                                    inner.push_str(&format!(
                                        "<div class=\"toolcall\">→ {} <code>{}</code></div>",
                                        escape(&tc.name),
                                        escape(&tc.arguments.to_string())
                                    ));
                                }
                                kiss_ai::ContentBlock::Image { .. } => {
                                    inner.push_str("<em>[image]</em>")
                                }
                            }
                        }
                        body.push_str(&format!(
                            "<div class=\"msg assistant\"><h4>assistant ({})</h4>{inner}</div>\n",
                            escape(&a.model)
                        ));
                    }
                    AgentMessage::ToolResult(t) => {
                        let text: String = t
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                kiss_ai::ContentBlock::Text { text, .. } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let class = if t.is_error { "tool error" } else { "tool" };
                        body.push_str(&format!(
                        "<details class=\"{class}\"><summary>{} result</summary><pre>{}</pre></details>\n",
                        escape(&t.tool_name),
                        escape(&text)
                    ));
                    }
                    other => {
                        body.push_str(&format!(
                            "<div class=\"msg meta\"><h4>{}</h4></div>\n",
                            other.role()
                        ));
                    }
                }
            }
            SessionEntry::Compaction { summary, .. } => {
                body.push_str(&format!("<details class=\"meta\"><summary>compaction summary</summary><pre>{}</pre></details>\n", escape(summary)));
            }
            _ => {}
        }
    }

    let title = manager
        .session_name()
        .unwrap_or_else(|| format!("kiss session {}", manager.session_id()));
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>\n\
        body{{font-family:ui-monospace,monospace;max-width:960px;margin:2rem auto;padding:0 1rem;background:#14141b;color:#d6d6e0}}\n\
        .msg{{margin:1rem 0;padding:.6rem 1rem;border-radius:8px;background:#1d1d29}}\n\
        .msg.user{{background:#232338;border-left:3px solid #7aa2f7}}\n\
        h4{{margin:.1rem 0 .4rem;color:#7aa2f7;font-size:.8rem;text-transform:uppercase}}\n\
        pre{{white-space:pre-wrap;word-break:break-word;margin:.2rem 0}}\n\
        details{{margin:.5rem 0;padding:.4rem .8rem;background:#191924;border-radius:6px}}\n\
        details.error{{border-left:3px solid #f7768e}}\n\
        summary{{cursor:pointer;color:#8b8ba0}}\n\
        .toolcall{{color:#9ece6a;margin:.2rem 0}}\n\
        </style></head><body><h1>{title}</h1>\n{body}</body></html>",
        title = escape(&title),
    );
    std::fs::write(target, html)?;
    Ok(())
}

pub fn export_jsonl(manager: &SessionManager, target: &Path) -> Result<()> {
    std::fs::write(target, manager.to_jsonl()?)?;
    Ok(())
}

pub fn export_session(manager: &SessionManager, target: &Path) -> Result<()> {
    if target.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
        export_jsonl(manager, target)
    } else {
        export_html(manager, target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn jsonl_target_uses_portable_session_format() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("portable.jsonl");
        let manager = SessionManager::in_memory(Path::new("/tmp/project"));
        export_session(&manager, &target).unwrap();
        let reopened = SessionManager::open(&target).unwrap();
        assert_eq!(reopened.session_id(), manager.session_id());
    }
}
