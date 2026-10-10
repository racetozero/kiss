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
    dirs::home_dir().map(|h| h.join(".kiss/agent/trust.json"))
}

fn read_trust_file() -> TrustFile {
    trust_file_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Does this project directory carry project-local resources that need a
/// trust decision at all?
pub fn has_project_resources(cwd: &Path) -> bool {
    cwd.join(".kiss").is_dir() || cwd.join(".agents/skills").is_dir()
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
    let file_path = trust_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = read_trust_file();
    file.decisions.insert(path.display().to_string(), trusted);
    std::fs::write(&file_path, serde_json::to_string_pretty(&file)?)?;
    Ok(())
}

/// Resolve trust non-interactively (print/json modes and CLI overrides).
pub fn resolve_non_interactive(
    cwd: &Path,
    cli_override: Option<bool>,
    default: ProjectTrustDefault,
) -> bool {
    if let Some(explicit) = cli_override {
        return explicit;
    }
    if !has_project_resources(cwd) {
        return true; // nothing project-local to gate
    }
    if let Some(saved) = saved_decision(cwd) {
        return saved;
    }
    matches!(default, ProjectTrustDefault::Always)
}
