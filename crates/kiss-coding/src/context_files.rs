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

    if let Some(agent_dir) = kiss_ai::paths::agent_dir() {
        let global = agent_dir.join("AGENTS.md");
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

    // A symlinked AGENTS.md, such as one in a git worktree nested inside its
    // main repository, can name a file that an ancestor already loaded.
    let mut seen = std::collections::HashSet::new();
    paths.retain(|path| seen.insert(std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())));
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

/// `SYSTEM.md` and `APPEND_SYSTEM.md`, from the project's `.kiss/` when the
/// project is trusted, otherwise from the agent directory.
pub fn system_prompt_files(cwd: &Path, project_trusted: bool) -> SystemPromptFiles {
    let dirs: Vec<PathBuf> = project_trusted
        .then(|| cwd.join(".kiss"))
        .into_iter()
        .chain(kiss_ai::paths::agent_dir())
        .collect();
    let read = |name: &str| {
        dirs.iter()
            .find_map(|dir| std::fs::read_to_string(dir.join(name)).ok())
    };
    SystemPromptFiles {
        replace: read("SYSTEM.md"),
        append: read("APPEND_SYSTEM.md"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untrusted_project_cannot_replace_the_system_prompt() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".kiss")).unwrap();
        std::fs::write(project.path().join(".kiss/SYSTEM.md"), "project prompt").unwrap();
        assert_eq!(
            system_prompt_files(project.path(), true).replace.as_deref(),
            Some("project prompt")
        );
        assert_ne!(
            system_prompt_files(project.path(), false)
                .replace
                .as_deref(),
            Some("project prompt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_context_file_loads_once() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("AGENTS.md"), "rules").unwrap();
        let worktree = repo.path().join("worktrees/feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::os::unix::fs::symlink(repo.path().join("AGENTS.md"), worktree.join("AGENTS.md"))
            .unwrap();
        let files: Vec<ContextFile> = discover(&worktree)
            .into_iter()
            .filter(|file| file.path.starts_with(repo.path()))
            .collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "rules");
    }

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
