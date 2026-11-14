//! JSON files under the agent directory that several kiss processes may
//! read and replace: credentials, account pools, the custom model catalog,
//! MCP configuration, caches and settings.

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::Path;

/// Read a JSON file. A missing file reads as `T::default()`. A file that
/// exists but cannot be read or parsed is an error, so a caller that
/// writes the value back never replaces data it did not understand.
pub fn read_json<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Could not read {}. Restore read access to the file, then try again.",
                    path.display()
                )
            });
        }
    };
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid. Correct the JSON or move the file away, then try again. kiss does not overwrite a file it cannot parse.",
            path.display()
        )
    })
}

/// Atomically replace `path` with pretty JSON readable only by the owner.
pub fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_vec_pretty(value)?;
    text.push(b'\n');
    replace(path, &text, true)
}

/// Atomically replace `path` with `contents`, keeping default permissions.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    replace(path, contents, false)
}

/// Run `f` while holding an exclusive lock on `.<file name>.lock` beside
/// `path`. Every process that updates `path` must take this lock.
pub fn with_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let parent = parent_dir(path)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let lock_path = parent.join(format!(".{name}.lock"));
    let lock = open_private(
        &lock_path,
        OpenOptions::new().read(true).write(true).create(true),
    )?;
    fs2::FileExt::lock_exclusive(&lock).with_context(|| {
        format!(
            "Could not lock {}. Use a filesystem with file lock support.",
            path.display()
        )
    })?;
    let result = f();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

/// Open `path` with `options`, creating it readable only by the owner.
pub fn open_private(path: &Path, options: &mut OpenOptions) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).with_context(|| {
        format!(
            "Could not open {}. Restore read and write access to it.",
            path.display()
        )
    })
}

/// Deep-merge `overlay` onto `base`: objects merge key by key, and every
/// other value replaces the one below it.
pub fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

fn parent_dir(path: &Path) -> Result<&Path> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory.", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "Could not create {}. Use a writable directory.",
            parent.display()
        )
    })?;
    Ok(parent)
}

fn replace(path: &Path, contents: &[u8], private: bool) -> Result<()> {
    // Replace a symlink's target, not the link: a dotfile setup links these
    // files into another folder.
    let resolved;
    let path = if std::fs::symlink_metadata(path).is_ok_and(|data| data.file_type().is_symlink()) {
        resolved = std::fs::canonicalize(path)
            .with_context(|| format!("Could not follow the link {}.", path.display()))?;
        resolved.as_path()
    } else {
        path
    };
    let parent = parent_dir(path)?;
    let temp_path = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file"),
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = if private {
            open_private(&temp_path, &mut options)?
        } else {
            options
                .open(&temp_path)
                .with_context(|| format!("Could not create {}.", temp_path.display()))?
        };
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temp_path, path)
            .with_context(|| format!("Could not replace {}.", path.display()))?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_save_through_a_symlink_keeps_the_link() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("dotfiles-settings.json");
        let link = directory.path().join("settings.json");
        std::fs::write(&target, "{}").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        write_atomic(&link, b"{\"theme\":\"light\"}").unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "{\"theme\":\"light\"}"
        );
    }
}
