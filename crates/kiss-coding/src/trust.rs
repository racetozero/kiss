//! Project trust: gate loading of project-local resources behind a saved
//! decision in `~/.kiss/agent/trust.json` (parent-folder decisions apply).

use crate::settings::ProjectTrustDefault;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(flatten)]
    decisions: BTreeMap<String, bool>,
}

pub fn trust_file_path() -> Option<PathBuf> {
    kiss_ai::paths::agent_dir().map(|dir| dir.join("trust.json"))
}

fn read_trust_file() -> TrustFile {
    trust_file_path()
        .and_then(|path| kiss_ai::config_file::read_json(&path).ok())
        .unwrap_or_default()
}

/// The project paths that load only when the project is trusted: `.kiss/`
/// (settings, skills, prompts, workflows, MCP config and SYSTEM.md),
/// `.mcp.json`, and `.agents/skills` in the folder and its parents up to the
/// git root. Every loader of a project-local resource must be listed here.
pub fn gated_paths(cwd: &Path) -> Vec<PathBuf> {
    let mut paths = vec![cwd.join(".kiss"), cwd.join(".mcp.json")];
    paths.extend(crate::skills::project_agents_skill_dirs(cwd));
    paths
}

/// Does this project directory carry project-local resources that need a
/// trust decision at all?
pub fn has_project_resources(cwd: &Path) -> bool {
    gated_paths(cwd).iter().any(|path| path.exists())
}

/// Saved decision for `cwd` or any ancestor.
pub fn saved_decision(cwd: &Path) -> Option<bool> {
    let file = read_trust_file();
    for dir in cwd.ancestors() {
        if let Some(&decision) = file.decisions.get(&dir.display().to_string()) {
            return Some(decision);
        }
    }
    None
}

pub fn save_decision(path: &Path, trusted: bool) -> anyhow::Result<()> {
    let file_path = trust_file_path().ok_or_else(|| anyhow::anyhow!(
        "The trust decision cannot be saved because the home directory is unknown. Set HOME, then try again."
    ))?;
    kiss_ai::config_file::with_lock(&file_path, || {
        let mut file: TrustFile = kiss_ai::config_file::read_json(&file_path)?;
        file.decisions.insert(path.display().to_string(), trusted);
        kiss_ai::config_file::write_atomic(
            &file_path,
            serde_json::to_string_pretty(&file)?.as_bytes(),
        )
    })
}

/// Whether to load project-local resources for `cwd`. `run_override` is a
/// per-run choice such as `--approve` (`Some(true)`) or `--no-approve`
/// (`Some(false)`); without one, a saved decision applies, then the user's
/// `defaultProjectTrust` setting (never a project's own setting).
pub fn resolve(cwd: &Path, run_override: Option<bool>) -> bool {
    decide(
        cwd,
        run_override,
        crate::Settings::load(cwd, false).default_project_trust,
    )
}

fn decide(cwd: &Path, cli_override: Option<bool>, default: ProjectTrustDefault) -> bool {
    if let Some(explicit) = cli_override {
        return explicit;
    }
    // A saved decision wins, also "Do not trust" for a project with nothing
    // that needs trust, such as a folder with only AGENTS.md.
    if let Some(saved) = saved_decision(cwd) {
        return saved;
    }
    if !has_project_resources(cwd) {
        return true; // nothing project-local to gate
    }
    matches!(default, ProjectTrustDefault::Always)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_trust_gated_project_file_needs_a_decision() {
        let empty = tempfile::tempdir().unwrap();
        assert!(!has_project_resources(empty.path()));

        let mcp_only = tempfile::tempdir().unwrap();
        std::fs::write(mcp_only.path().join(".mcp.json"), "{}").unwrap();
        assert!(has_project_resources(mcp_only.path()));

        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::create_dir_all(repo.path().join(".agents/skills")).unwrap();
        let nested = repo.path().join("crates/app");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(has_project_resources(&nested));
    }
}
