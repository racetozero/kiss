//! Project context file discovery: AGENTS.md / CLAUDE.md, with
//! AGENTS.override.md taking precedence per directory. Load order: global
//! file, then ancestor directories root->cwd, then cwd.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

fn pick_context_file(dir: &Path) -> Option<PathBuf> {
    for name in ["AGENTS.override.md", "AGENTS.md", "CLAUDE.md"] {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

pub fn discover(cwd: &Path) -> Vec<ContextFile> {
    let mut paths: Vec<PathBuf> = Vec::new();

    if let Some(home) = dirs::home_dir() {
        let global = home.join(".kiss/agent/AGENTS.md");
        if global.is_file() {
            paths.push(global);
        }
    }

    // Ancestors from root down to cwd (cwd last so it wins by proximity).
    let mut chain: Vec<&Path> = cwd.ancestors().collect();
    chain.reverse();
    for dir in chain {
        if let Some(file) = pick_context_file(dir) {
            paths.push(file);
        }
    }

    paths.dedup();
    paths
        .into_iter()
        .filter_map(|path| {
            let content = std::fs::read_to_string(&path).ok()?;
            Some(ContextFile { path, content })
        })
        .collect()
}

/// System-prompt replacement/append files (project wins over global).
pub struct SystemPromptFiles {
    pub replace: Option<String>,
    pub append: Option<String>,
}

pub fn system_prompt_files(cwd: &Path) -> SystemPromptFiles {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    let global = dirs::home_dir().map(|h| h.join(".kiss/agent"));
    let project = cwd.join(".kiss");

    let replace = read(project.join("SYSTEM.md"))
        .or_else(|| global.as_ref().and_then(|g| read(g.join("SYSTEM.md"))));
    let append = read(project.join("APPEND_SYSTEM.md")).or_else(|| {
        global
            .as_ref()
            .and_then(|g| read(g.join("APPEND_SYSTEM.md")))
    });
    SystemPromptFiles { replace, append }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_beats_agents_beats_claude() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "claude").unwrap();
        assert!(
            pick_context_file(dir.path())
                .unwrap()
                .ends_with("CLAUDE.md")
        );
        std::fs::write(dir.path().join("AGENTS.md"), "agents").unwrap();
        assert!(
            pick_context_file(dir.path())
                .unwrap()
                .ends_with("AGENTS.md")
        );
        std::fs::write(dir.path().join("AGENTS.override.md"), "override").unwrap();
        assert!(
            pick_context_file(dir.path())
                .unwrap()
                .ends_with("AGENTS.override.md")
        );
    }

    #[test]
    fn ancestors_ordered_root_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(nested.join("AGENTS.md"), "leaf rules").unwrap();
        let files = discover(&nested);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        let root_pos = contents.iter().position(|c| *c == "root rules").unwrap();
        let leaf_pos = contents.iter().position(|c| *c == "leaf rules").unwrap();
        assert!(root_pos < leaf_pos);
    }
}
