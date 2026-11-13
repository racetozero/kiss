//! Where kiss keeps its user-level files.

use std::path::{Path, PathBuf};

/// `~/.kiss/agent`, which holds settings, credentials, sessions and other
/// user-level state. `None` when the home directory is unknown.
pub fn agent_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| agent_dir_in(&home))
}

/// The agent directory under a given home directory.
pub fn agent_dir_in(home: &Path) -> PathBuf {
    home.join(".kiss").join("agent")
}

/// Expand a leading `~` path component to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    Path::new(path)
        .strip_prefix("~")
        .ok()
        .and_then(|rest| dirs::home_dir().map(|home| home.join(rest)))
        .unwrap_or_else(|| PathBuf::from(path))
}
