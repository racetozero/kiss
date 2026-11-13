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

/// Show `path` with the home directory written as `~`, the inverse of
/// [`expand_home`].
pub fn abbreviate_home(path: &str) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = Path::new(path).strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.to_string()
}
