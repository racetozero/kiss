//! Saved workflows on disk.
//!
//! A saved workflow is a `.js` file whose `meta.name` becomes a slash command,
//! following the same discovery rules as prompt templates in
//! `crate::prompts`: the user's own directory always, and the project's only
//! when the project is trusted.

use kiss_workflow::Script;
use std::path::{Path, PathBuf};

/// A workflow found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedWorkflow {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// True for a workflow in the project rather than the user's home.
    pub from_project: bool,
}

/// Where a saved workflow can be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveLocation {
    /// `.kiss/workflows/` in the project, shared with everyone who clones it.
    Project,
    /// `~/.kiss/agent/workflows/`, available in every project.
    Personal,
}

pub fn user_workflow_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiss/agent/workflows"))
}

pub fn project_workflow_dir(cwd: &Path) -> PathBuf {
    cwd.join(".kiss/workflows")
}

impl SaveLocation {
    pub fn directory(self, cwd: &Path) -> Option<PathBuf> {
        match self {
            SaveLocation::Project => Some(project_workflow_dir(cwd)),
            SaveLocation::Personal => user_workflow_dir(),
        }
    }
}

fn scan(dir: &Path, from_project: bool, out: &mut Vec<SavedWorkflow>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("js") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // A file that does not parse is skipped rather than reported: a broken
        // file in the directory must not stop the others from loading, and the
        // error surfaces when the user runs it.
        let Ok(script) = Script::parse(&text) else {
            continue;
        };
        out.push(SavedWorkflow {
            name: script.meta().name.clone(),
            description: script.meta().description.clone(),
            path,
            from_project,
        });
    }
}

/// Find every saved workflow, with the project's copy winning on a name clash.
pub fn discover(cwd: &Path, project_trusted: bool) -> Vec<SavedWorkflow> {
    let mut out = Vec::new();
    if project_trusted {
        scan(&project_workflow_dir(cwd), true, &mut out);
    }
    if let Some(dir) = user_workflow_dir() {
        scan(&dir, false, &mut out);
    }
    out.sort_by(|left, right| left.name.cmp(&right.name));
    let mut seen = std::collections::HashSet::new();
    out.retain(|workflow| seen.insert(workflow.name.clone()));
    out
}

/// Write a workflow script, refusing to follow a symbolic link.
///
/// Writing through a link would put the file somewhere the user did not choose,
/// so each part of the path is checked first. The write goes through a
/// temporary file in the same directory and is then renamed, so an interrupted
/// save cannot leave a truncated script behind.
pub fn save(
    location: SaveLocation,
    cwd: &Path,
    name: &str,
    source: &str,
    overwrite: bool,
) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    if name.is_empty() || !name.bytes().all(is_name_byte) {
        anyhow::bail!(
            "a workflow name uses lower-case letters, digits, and dashes, for example \
             `audit-routes`"
        );
    }
    let directory = location
        .directory(cwd)
        .ok_or_else(|| anyhow::anyhow!("no home directory"))?;

    // The project location has two directories of its own to check; the
    // personal one is often managed by a dotfiles tool, so only the file itself
    // is checked there.
    if location == SaveLocation::Project {
        for ancestor in [directory.parent(), Some(directory.as_path())]
            .into_iter()
            .flatten()
        {
            refuse_symlink(ancestor)?;
        }
    }

    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{name}.js"));
    refuse_symlink(&path)?;
    if path.exists() && !overwrite {
        anyhow::bail!("{} already exists", path.display());
    }

    let temporary = directory.join(format!(".{name}.js.tmp"));
    std::fs::write(&temporary, source)?;
    std::fs::rename(&temporary, &path)?;
    Ok(path)
}

fn refuse_symlink(path: &Path) -> anyhow::Result<()> {
    if std::fs::symlink_metadata(path).is_ok_and(|data| data.file_type().is_symlink()) {
        anyhow::bail!(
            "{} is a symbolic link; saving there would write outside the location you chose",
            path.display()
        );
    }
    Ok(())
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str =
        "export const meta = { name: 'audit-routes', description: 'Audit routes' }\nreturn 1\n";

    #[test]
    fn a_saved_workflow_can_be_found_again_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = save(
            SaveLocation::Project,
            dir.path(),
            "audit-routes",
            SCRIPT,
            false,
        )
        .unwrap();
        assert!(path.ends_with("audit-routes.js"));

        let found = discover(dir.path(), true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "audit-routes");
        assert_eq!(found[0].description, "Audit routes");
        assert!(found[0].from_project);
    }

    #[test]
    fn an_untrusted_project_contributes_no_workflows() {
        let dir = tempfile::tempdir().unwrap();
        save(
            SaveLocation::Project,
            dir.path(),
            "audit-routes",
            SCRIPT,
            false,
        )
        .unwrap();
        // The user's own directory may hold workflows on this machine, so the
        // check is that the project's file is absent rather than that nothing
        // was found.
        assert!(
            !discover(dir.path(), false)
                .iter()
                .any(|workflow| workflow.from_project)
        );
    }

    #[test]
    fn saving_twice_needs_permission_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        save(SaveLocation::Project, dir.path(), "audit", SCRIPT, false).unwrap();
        let error = save(SaveLocation::Project, dir.path(), "audit", SCRIPT, false).unwrap_err();
        assert!(error.to_string().contains("already exists"));
        assert!(save(SaveLocation::Project, dir.path(), "audit", SCRIPT, true).is_ok());
    }

    #[test]
    fn a_bad_name_is_refused_with_an_example() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["", "Audit Routes", "audit/routes", "../escape"] {
            let error = save(SaveLocation::Project, dir.path(), name, SCRIPT, false)
                .unwrap_err()
                .to_string();
            assert!(error.contains("audit-routes"), "{name}: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn saving_refuses_to_write_through_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".kiss")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".kiss/workflows")).unwrap();

        let error = save(SaveLocation::Project, dir.path(), "audit", SCRIPT, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(!elsewhere.path().join("audit.js").exists());
    }

    #[test]
    fn a_file_that_does_not_parse_is_skipped_rather_than_breaking_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = project_workflow_dir(dir.path());
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("broken.js"), "this is not a workflow").unwrap();
        std::fs::write(workflows.join("good.js"), SCRIPT).unwrap();

        let found: Vec<String> = discover(dir.path(), true)
            .into_iter()
            .filter(|workflow| workflow.from_project)
            .map(|workflow| workflow.name)
            .collect();
        assert_eq!(found, ["audit-routes"]);
    }

    #[test]
    fn the_name_comes_from_meta_not_from_the_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = project_workflow_dir(dir.path());
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(workflows.join("whatever.js"), SCRIPT).unwrap();
        let found = discover(dir.path(), true);
        assert_eq!(found[0].name, "audit-routes");
    }
}
