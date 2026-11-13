//! Path resolution helpers for tools.

use std::path::{Path, PathBuf};

/// Resolve a tool-supplied path against `cwd`, expanding a leading `~`.
pub fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let expanded = kiss_ai::paths::expand_home(path);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
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
