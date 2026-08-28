//! Write tool: create/overwrite a file, creating parent directories.

use crate::tool::{AgentTool, ToolResult, ToolUpdateSink};
use crate::tools::mutation_queue::lock_path;
use crate::tools::path::resolve;
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

pub struct WriteTool {
    pub cwd: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> String {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to write (relative or absolute)"},
                "content": {"type": "string", "description": "Content to write to the file"},
            },
            "required": ["path", "content"],
        })
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let path = args["path"].as_str().unwrap_or_default().to_string();
        let content = args["content"].as_str().unwrap_or_default().to_string();
        let absolute = resolve(&self.cwd, &path);
        let _guard = lock_path(&absolute).await;
        if cancel.is_cancelled() {
            anyhow::bail!("Operation aborted");
        }
        if let Some(parent) = absolute.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                anyhow::anyhow!("Could not create parent directories for {path}: {e}")
            })?;
        }
        let bytes = content.len();
        tokio::fs::write(&absolute, content)
            .await
            .map_err(|e| anyhow::anyhow!("Could not write file: {path}. {e}"))?;
        Ok(ToolResult::text(format!(
            "Successfully wrote {bytes} bytes to {path}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_creates_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute(
                "1",
                json!({"path": "a/b/c.txt", "content": "hello"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(r.output_text().contains("5 bytes"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hello"
        );
    }
}
