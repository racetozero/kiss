//! Path resolution helpers for tools.

use std::path::{Path, PathBuf};

/// Resolve a tool-supplied path against `cwd`, expanding a leading `~`.
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let expanded: PathBuf = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(path))
    } else if path == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(path))
    } else {
        PathBuf::from(path)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Render a path relative to `cwd` when it is inside it (display only).
pub fn display(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_and_absolute() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve(cwd, "src/main.rs"),
            PathBuf::from("/work/src/main.rs")
        );
        assert_eq!(resolve(cwd, "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }
}
