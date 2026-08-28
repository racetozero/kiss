//! Find tool: gitignore-aware glob file search.

use kiss_agent::tool::{AgentTool, ToolResult, ToolUpdateSink};
use kiss_agent::tools::path::resolve;
use kiss_agent::tools::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};
use serde_json::{Value, json};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 1000;

pub struct FindTool {
    pub cwd: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> String {
        "Find files by glob pattern. Respects .gitignore. Returns paths relative to the search directory.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern to match files, e.g. '*.rs', '**/*.json', or 'src/**/*.spec.ts'"},
                "path": {"type": "string", "description": "Directory to search in (default: current directory)"},
                "limit": {"type": "number", "description": "Maximum number of results (default: 1000)"},
            },
            "required": ["pattern"],
        })
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateSink>,
    ) -> anyhow::Result<ToolResult> {
        let pattern = args["pattern"].as_str().unwrap_or_default().to_string();
        let search_path = resolve(&self.cwd, args["path"].as_str().unwrap_or("."));
        let limit = args["limit"]
            .as_f64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let result = tokio::task::spawn_blocking(move || {
            if !search_path.exists() {
                anyhow::bail!("Path not found: {}", search_path.display());
            }
            let normalized = if pattern.contains('/') { pattern.clone() } else { format!("**/{pattern}") };
            let glob = globset::GlobBuilder::new(&normalized)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid glob: {e}"))?
                .compile_matcher();

            let mut results: Vec<String> = Vec::new();
            let mut walker = ignore::WalkBuilder::new(&search_path);
            walker
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .require_git(false);
            let walker = walker.build();
            for entry in walker {
                if cancel.is_cancelled() || results.len() >= limit {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().strip_prefix(&search_path).unwrap_or(entry.path());
                if glob.is_match(rel) {
                    results.push(rel.display().to_string().replace('\\', "/"));
                }
            }
            results.sort();

            if results.is_empty() {
                return Ok(ToolResult::text("No files found"));
            }
            let limit_reached = results.len() >= limit;
            let joined = results.join("\n");
            let truncation = truncate_head(&joined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
            let mut output = truncation.content.clone();
            if limit_reached {
                output.push_str(&format!("\n\n[Result limit of {limit} reached.]"));
            }
            Ok(ToolResult {
                content: vec![kiss_ai::ContentBlock::text(output)],
                details: json!({"resultLimitReached": if limit_reached { Some(limit) } else { None }}),
                ..Default::default()
            })
        })
        .await??;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn glob_find_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("target/out.rs"), "").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        let tool = FindTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute(
                "1",
                json!({"pattern": "*.rs"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = r.output_text();
        assert!(text.contains("src/main.rs"));
        assert!(!text.contains("target/out.rs"));
    }
}
