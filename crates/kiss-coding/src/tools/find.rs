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
                "pattern": {"type": "string", "description": "Glob pattern relative to the search directory, e.g. 'src/**/*.rs'. Without '/', e.g. '*.rs', it matches file names at any depth"},
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
        // A limit below 1 means 1.
        let limit = args["limit"]
            .as_f64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .max(1);

        let result = tokio::task::spawn_blocking(move || {
            if !search_path.exists() {
                anyhow::bail!("Path not found: {}", search_path.display());
            }
            // On Windows `\` separates paths, so `src\**\*.ts` means `src/**/*.ts`.
            // Elsewhere `\` escapes a glob character.
            let pattern = if cfg!(windows) { pattern.replace('\\', "/") } else { pattern };
            // Relative paths never start with `./`, so `./src/*.rs` means `src/*.rs`.
            let mut pattern = pattern.as_str();
            while let Some(rest) = pattern.strip_prefix("./") {
                pattern = rest;
            }
            let normalized = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
            let glob = globset::GlobBuilder::new(&normalized)
                .literal_separator(true)
                .build()
                .map_err(|e| anyhow::anyhow!("Invalid glob: {e}"))?
                .compile_matcher();

            let mut results: Vec<String> = Vec::new();
            let walker = super::search_walker(&search_path).build();
            for entry in walker {
                // One result past the limit tells "exactly N" from "more than N".
                if cancel.is_cancelled() || results.len() > limit {
                    break;
                }
                let Ok(entry) = entry else { continue };
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                let rel = entry.path().trim_prefix(&search_path);
                if glob.is_match(rel) {
                    results.push(rel.display().to_string().replace('\\', "/"));
                }
            }
            results.sort();

            if results.is_empty() {
                return Ok(ToolResult::text("No files found"));
            }
            let limit_reached = results.len() > limit;
            results.truncate(limit);
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
    use futures::{StreamExt as _, stream};
    use std::time::Instant;

    #[tokio::test]
    async fn limit_notice_needs_a_result_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        let tool = FindTool {
            cwd: dir.path().to_path_buf(),
        };
        let run = |limit: u64| {
            let tool = &tool;
            async move {
                tool.execute(
                    "1",
                    json!({"pattern": "*.rs", "limit": limit}),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap()
            }
        };
        let exact = run(2).await;
        assert!(
            !exact.output_text().contains("limit"),
            "{}",
            exact.output_text()
        );
        let cut = run(1).await;
        assert!(cut.output_text().contains("[Result limit of 1 reached.]"));
        assert_eq!(cut.output_text().matches(".rs").count(), 1);
        let zero = run(0).await;
        assert_eq!(zero.output_text().matches(".rs").count(), 1);
    }

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

    #[tokio::test]
    async fn path_patterns_start_at_the_search_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("pkg/src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("pkg/src/lib.rs"), "").unwrap();
        let tool = FindTool {
            cwd: dir.path().to_path_buf(),
        };
        for (pattern, expected) in [
            ("src/*.rs", "src/main.rs"),
            ("./src/*.rs", "src/main.rs"),
            ("**/src/*.rs", "pkg/src/lib.rs\nsrc/main.rs"),
        ] {
            let result = tool
                .execute(
                    "1",
                    json!({"pattern": pattern}),
                    CancellationToken::new(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(result.output_text(), expected, "{pattern}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore = "release-mode performance benchmark"]
    async fn benchmark_performance_200_find_calls() {
        const CALLS: usize = 200;
        let dir = tempfile::tempdir().unwrap();
        for directory in 0..10 {
            let path = dir.path().join(format!("src/module_{directory:02}"));
            std::fs::create_dir_all(&path).unwrap();
            for file in 0..20 {
                std::fs::write(path.join(format!("file_{file:03}.rs")), "fn item() {}\n").unwrap();
            }
        }
        let tool = FindTool {
            cwd: dir.path().to_path_buf(),
        };

        for (name, concurrency) in [("unlimited", usize::MAX), ("bounded", 32)] {
            let started = Instant::now();
            let results = stream::iter(0..CALLS)
                .map(|index| {
                    tool.execute(
                        "bench",
                        json!({"pattern": format!("file_{:03}.rs", index % 20)}),
                        CancellationToken::new(),
                        None,
                    )
                })
                .buffer_unordered(concurrency)
                .collect::<Vec<_>>()
                .await;
            assert!(results.into_iter().all(|result| result.is_ok()));

            let mut sample = [started.elapsed().as_nanos() / CALLS as u128];
            kiss_bench::report(
                &format!("find_batch_200_{name}"),
                &mut sample,
                CALLS,
                &format!("200_file_tree_max_active={concurrency}"),
            );
        }
    }
}
