//! Ls tool: directory listing, directories first with a trailing slash.

use kiss_agent::tool::{AgentTool, ToolResult, ToolUpdateSink};
use kiss_agent::tools::path::resolve;
use kiss_agent::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 500;

pub struct LsTool {
    pub cwd: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> String {
        "List directory contents. Directories are listed first with a trailing slash.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory to list (default: current directory)"},
                "limit": {"type": "number", "description": "Maximum number of entries to return (default: 500)"},
            },
        })
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let path = resolve(&self.cwd, args["path"].as_str().unwrap_or("."));
        let limit = args["limit"]
            .as_f64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT);
        if !path.exists() {
            anyhow::bail!("Path not found: {}", path.display());
        }
        if !path.is_dir() {
            anyhow::bail!("Not a directory: {}", path.display());
        }
        let mut dirs: Vec<String> = Vec::new();
        let mut files: Vec<String> = Vec::new();
        let mut read = tokio::fs::read_dir(&path).await?;
        while let Some(entry) = read.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        let mut entries: Vec<String> = dirs;
        entries.extend(files);
        let total = entries.len();
        let limit_reached = total > limit;
        entries.truncate(limit);

        if entries.is_empty() {
            return Ok(ToolResult::text("(empty directory)"));
        }
        let joined = entries.join("\n");
        let truncation = truncate_head(&joined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        let mut output = truncation.content.clone();
        if limit_reached {
            output.push_str(&format!(
                "\n\n[Entry limit of {limit} reached ({total} total).]"
            ));
        }
        Ok(ToolResult {
            content: vec![kiss_ai::ContentBlock::text(output)],
            details: json!({"entryLimitReached": if limit_reached { Some(limit) } else { None }}),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dirs_first_with_slash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("afile.txt"), "").unwrap();
        let tool = LsTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute("1", json!({}), CancellationToken::new(), None)
            .await
            .unwrap();
        assert_eq!(r.output_text(), "zdir/\nafile.txt");
    }
}
