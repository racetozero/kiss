//! Grep tool: gitignore-aware content search using ripgrep's libraries
//! in-process (ignore + grep-searcher), matching pi's grep tool surface.

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use kiss_agent::tool::{AgentTool, ToolResult, ToolUpdateSink};
use kiss_agent::tools::path::resolve;
use kiss_agent::tools::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, GREP_MAX_LINE_LENGTH, truncate_head, truncate_line,
};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

const DEFAULT_LIMIT: usize = 100;
const PARALLEL_GREP_MIN_FILES: usize = 256;
const MAX_GREP_WORKERS: usize = 4;

struct GrepRecord {
    path: String,
    line_number: u64,
    text: String,
    is_match: bool,
    was_truncated: bool,
}

struct GrepChunk {
    records: Vec<GrepRecord>,
}

pub struct GrepTool {
    pub cwd: PathBuf,
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> String {
        "Search file contents for a pattern (regex or literal string). Respects .gitignore. Returns matching lines with file paths and line numbers.".to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Search pattern (regex or literal string)"},
                "path": {"type": "string", "description": "Directory or file to search (default: current directory)"},
                "glob": {"type": "string", "description": "Filter files by glob pattern, e.g. '*.rs' or '**/*.spec.ts'"},
                "ignoreCase": {"type": "boolean", "description": "Case-insensitive search (default: false)"},
                "literal": {"type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)"},
                "context": {"type": "number", "description": "Number of lines to show before and after each match (default: 0)"},
                "limit": {"type": "number", "description": "Maximum number of matches to return (default: 100)"},
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
        let glob = args["glob"].as_str().map(String::from);
        let ignore_case = args["ignoreCase"].as_bool().unwrap_or(false);
        let literal = args["literal"].as_bool().unwrap_or(false);
        let context = args["context"].as_f64().unwrap_or(0.0) as usize;
        let limit = args["limit"]
            .as_f64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT);
        let cwd = self.cwd.clone();

        // File walking + searching is sync CPU/IO work; run it off the async
        // executor so long searches never stall the event loop.
        let result = tokio::task::spawn_blocking(move || {
            run_grep(
                &cwd,
                &search_path,
                &pattern,
                glob.as_deref(),
                ignore_case,
                literal,
                context,
                limit,
                cancel,
            )
        })
        .await??;
        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_grep(
    cwd: &std::path::Path,
    search_path: &std::path::Path,
    pattern: &str,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context: usize,
    limit: usize,
    cancel: CancellationToken,
) -> anyhow::Result<ToolResult> {
    if !search_path.exists() {
        anyhow::bail!("Path not found: {}", search_path.display());
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(ignore_case)
        .fixed_strings(literal)
        .build(pattern)
        .map_err(|e| anyhow::anyhow!("Invalid pattern: {e}"))?;

    let glob_matcher = match glob {
        Some(g) => {
            let mut builder = globset::GlobSetBuilder::new();
            let normalized = if g.contains('/') {
                g.to_string()
            } else {
                format!("**/{g}")
            };
            builder.add(
                globset::Glob::new(&normalized)
                    .map_err(|e| anyhow::anyhow!("Invalid glob: {e}"))?,
            );
            Some(builder.build()?)
        }
        None => None,
    };

    let paths = Mutex::new(Vec::new());
    let mut walker = ignore::WalkBuilder::new(search_path);
    walker
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .require_git(false);
    walker.build_parallel().run(|| {
        let cancel = cancel.clone();
        let paths = &paths;
        let glob_matcher = &glob_matcher;
        Box::new(move |entry| {
            if cancel.is_cancelled() {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let path = entry.path();
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return ignore::WalkState::Continue;
            }
            if let Some(glob_matcher) = glob_matcher {
                let relative = path.strip_prefix(search_path).unwrap_or(path);
                if !glob_matcher.is_match(relative) && !glob_matcher.is_match(path) {
                    return ignore::WalkState::Continue;
                }
            }
            paths.lock().unwrap().push(path.to_path_buf());
            ignore::WalkState::Continue
        })
    });

    let mut paths = paths.into_inner().unwrap();
    paths.sort_unstable();
    let worker_count = if paths.len() >= PARALLEL_GREP_MIN_FILES {
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(MAX_GREP_WORKERS)
            .min(paths.len())
    } else {
        1
    };
    let chunk_size = paths.len().div_ceil(worker_count.max(1));
    let chunks = if worker_count <= 1 {
        vec![search_grep_chunk(
            &matcher, &paths, cwd, context, limit, &cancel,
        )]
    } else {
        std::thread::scope(|scope| {
            paths
                .chunks(chunk_size)
                .map(|paths| {
                    scope.spawn(|| search_grep_chunk(&matcher, paths, cwd, context, limit, &cancel))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| handle.join().expect("grep worker panicked"))
                .collect::<Vec<_>>()
        })
    };

    let mut output_lines = Vec::new();
    let mut selected_matches = 0usize;
    let mut lines_truncated = false;
    'chunks: for chunk in chunks {
        for record in chunk.records {
            if record.is_match && selected_matches >= limit {
                break 'chunks;
            }
            let separator = if record.is_match { ':' } else { '-' };
            output_lines.push(format!(
                "{}{separator}{}{separator}{}",
                record.path, record.line_number, record.text
            ));
            lines_truncated |= record.was_truncated;
            if record.is_match {
                selected_matches += 1;
                if selected_matches >= limit {
                    break 'chunks;
                }
            }
        }
    }
    if output_lines.is_empty() {
        return Ok(ToolResult::text("No matches found"));
    }
    let joined = output_lines.join("\n");
    let truncation = truncate_head(&joined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut output = truncation.content.clone();
    if selected_matches >= limit {
        output.push_str(&format!(
            "\n\n[Match limit of {limit} reached. Narrow the pattern or raise limit.]"
        ));
    }
    if truncation.truncated {
        output.push_str("\n\n[Output truncated. Narrow the search or use a more specific path.]");
    }
    let details = json!({
        "matchLimitReached": if selected_matches >= limit { Some(limit) } else { None },
        "linesTruncated": lines_truncated,
        "truncation": if truncation.truncated { Some(&truncation) } else { None },
    });
    Ok(ToolResult {
        content: vec![kiss_ai::ContentBlock::text(output)],
        details,
        ..Default::default()
    })
}

fn search_grep_chunk(
    matcher: &grep_regex::RegexMatcher,
    paths: &[PathBuf],
    cwd: &std::path::Path,
    context: usize,
    limit: usize,
    cancel: &CancellationToken,
) -> GrepChunk {
    let mut records = Vec::new();
    let mut match_count = 0usize;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .build();
    for path in paths {
        if cancel.is_cancelled() || match_count >= limit {
            break;
        }
        let display_path = path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        let _ = searcher.search_path(
            matcher,
            path,
            UTF8(|line_number, line| {
                if cancel.is_cancelled() || match_count >= limit {
                    return Ok(false);
                }
                let is_match = matcher.is_match(line.as_bytes()).unwrap_or(false);
                let (text, was_truncated) =
                    truncate_line(line.trim_end_matches('\n'), GREP_MAX_LINE_LENGTH);
                records.push(GrepRecord {
                    path: display_path.clone(),
                    line_number,
                    text,
                    is_match,
                    was_truncated,
                });
                if is_match {
                    match_count += 1;
                }
                Ok(match_count < limit)
            }),
        );
    }
    GrepChunk { records }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle here too\n").unwrap();
        std::fs::create_dir_all(dir.path().join("skip")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "skip/\n").unwrap();
        std::fs::write(dir.path().join("skip/c.rs"), "needle ignored\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_matches_respecting_gitignore() {
        let dir = setup();
        let tool = GrepTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute(
                "1",
                json!({"pattern": "needle"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = r.output_text();
        assert!(text.contains("a.rs:2:"));
        assert!(text.contains("b.txt:1:"));
        assert!(!text.contains("ignored"));
    }

    #[tokio::test]
    async fn glob_filter_and_literal() {
        let dir = setup();
        let tool = GrepTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute(
                "1",
                json!({"pattern": "needle", "glob": "*.rs", "literal": true}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let text = r.output_text();
        assert!(text.contains("a.rs"));
        assert!(!text.contains("b.txt"));
    }

    #[tokio::test]
    async fn no_matches_message() {
        let dir = setup();
        let tool = GrepTool {
            cwd: dir.path().to_path_buf(),
        };
        let r = tool
            .execute(
                "1",
                json!({"pattern": "zzz_absent"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(r.output_text(), "No matches found");
    }

    #[test]
    fn parallel_limit_selects_the_same_sorted_files() {
        let dir = tempfile::tempdir().unwrap();
        for index in (0..300).rev() {
            std::fs::write(dir.path().join(format!("file_{index:03}.rs")), "needle\n").unwrap();
        }
        let mut outputs = Vec::new();
        for _ in 0..4 {
            outputs.push(
                run_grep(
                    dir.path(),
                    dir.path(),
                    "needle",
                    Some("*.rs"),
                    false,
                    true,
                    0,
                    10,
                    CancellationToken::new(),
                )
                .unwrap()
                .output_text(),
            );
        }

        assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
        for index in 0..10 {
            assert!(outputs[0].contains(&format!("file_{index:03}.rs:1:needle")));
        }
        assert!(!outputs[0].contains("file_010.rs:1:needle"));
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_grep_tree() {
        let dir = tempfile::tempdir().unwrap();
        for directory in 0..20 {
            let path = dir.path().join(format!("src/module_{directory:02}"));
            std::fs::create_dir_all(&path).unwrap();
            for file in 0..50 {
                let marker = if file % 5 == 0 { "needle" } else { "ordinary" };
                std::fs::write(
                    path.join(format!("file_{file:03}.rs")),
                    format!("fn item_{file}() {{}}\nlet value = \"{marker}\";\n"),
                )
                .unwrap();
            }
        }
        kiss_bench::measure("grep_tree_1000", 11, 1, "1000_files_200_matches", || {
            run_grep(
                dir.path(),
                dir.path(),
                "needle",
                Some("*.rs"),
                false,
                true,
                0,
                10_000,
                CancellationToken::new(),
            )
            .unwrap()
            .output_text()
            .len()
        });
    }
}
