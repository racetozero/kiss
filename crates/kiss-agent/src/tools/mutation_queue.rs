//! Per-path async mutex so concurrent edit/write calls against the same
//! file serialize instead of interleaving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Mutex as AsyncMutex;

type Registry = Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// Acquire the mutation lock for `path`; the guard releases on drop.
pub async fn lock_path(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut map = registry().lock().expect("mutation registry poisoned");
        map.entry(path.to_path_buf()).or_default().clone()
    };
    lock.lock_owned().await
}
