//! Multi-account pools with rate-limit failover.
//!
//! The active account's credential stays in `auth.json` under the provider
//! key, so every existing resolver, refresher, and model list keeps working.
//! `accounts.json` beside it holds the ordered pool: labels, cooldowns, and
//! the credentials of inactive accounts. Each update first copies the
//! `auth.json` entry into the active pool slot ("sync"), so refreshed tokens
//! survive rotation. Both files are written under the shared auth lock.

use super::{AuthEntry, AuthFile, read_auth_file_at, write_auth_file_at};
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Cooldown for a limited account whose provider reported no reset time.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

type AddLocks = BTreeMap<(PathBuf, String), File>;
static ADD_LOCKS: OnceLock<Mutex<AddLocks>> = OnceLock::new();

/// The file lock survives every await in a login, and the OS releases it if
/// the process ends. A later process can then recover the persisted intent.
fn lock_add_at(auth_path: &Path, provider: &str, starting: bool) -> Result<File> {
    let locks = ADD_LOCKS.get_or_init(Default::default).lock().unwrap();
    if let Some(lock) = locks.get(&(auth_path.to_path_buf(), provider.to_string())) {
        if starting {
            bail!(
                "An account login for {provider} is already in progress. Finish or cancel it before you add another account."
            );
        }
        return Ok(lock.try_clone()?);
    }
    let parent = auth_path.parent().context("The credential path has no parent directory. Use a credential file inside an existing directory.")?;
    std::fs::create_dir_all(parent).with_context(|| format!("Could not create credential directory {}. Use a writable directory and retry the account login.", parent.display()))?;
    let path = parent.join(format!(
        ".account-add-{:x}.lock",
        Sha256::digest(provider.as_bytes())
    ));
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let lock = options.open(&path).with_context(|| format!("Could not open the account login lock at {}. Restore read and write access to this file, then retry the login.", path.display()))?;
    if let Err(error) = fs2::FileExt::try_lock_exclusive(&lock) {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Err(error).with_context(|| format!("An account login for {provider} is already in progress. Finish or cancel it before you try again."));
        }
        return Err(error).context("The account login lock could not be acquired. Use a filesystem with file lock support and retry the login.");
    }
    Ok(lock)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(flatten)]
    pools: BTreeMap<String, Pool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pool {
    active: usize,
    accounts: Vec<Account>,
    #[serde(default)]
    add_state: AddState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum AddState {
    #[default]
    Ready,
    Adding {
        owner: u32,
    },
    Added {
        owner: u32,
        index: Option<usize>,
    },
}

#[derive(Serialize, Deserialize)]
struct AccountsTransaction {
    auth: AuthFile,
    accounts: AccountsFile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Account {
    label: String,
    credential: AuthEntry,
    /// Unix milliseconds until which the account is considered rate limited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    limited_until: Option<i64>,
    #[serde(default)]
    added_at: i64,
}

/// One account as shown to the user. `index` is zero-based.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountInfo {
    pub index: usize,
    pub label: String,
    pub active: bool,
    /// Unix milliseconds; `None` or a past value means ready.
    pub limited_until: Option<i64>,
}

impl AccountInfo {
    pub fn is_limited(&self) -> bool {
        self.limited_until.is_some_and(|until| until > now_ms())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInfo {
    pub provider: String,
    pub accounts: Vec<AccountInfo>,
}

impl PoolInfo {
    pub fn active(&self) -> Option<&AccountInfo> {
        self.accounts.iter().find(|account| account.active)
    }
}

/// User-facing result of `finish_add`, shared by the CLI and the TUI.
pub fn added_message(provider: &str, added: &AccountInfo) -> String {
    let pool = pool(provider);
    let total = pool.as_ref().map_or(1, |pool| pool.accounts.len());
    let active = pool
        .as_ref()
        .and_then(|pool| pool.active().map(|account| account.label.clone()))
        .unwrap_or_default();
    format!(
        "{provider}: added account {}/{total} ({}). Active: {active}",
        added.index + 1,
        added.label
    )
}

/// A completed rotation away from a rate-limited account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSwitch {
    pub provider: String,
    pub from: String,
    pub to: String,
    /// Zero-based index of the account that is now active.
    pub index: usize,
    pub total: usize,
    /// Cooldown recorded for the account that failed.
    pub retry_after_secs: u64,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn accounts_path(auth_path: &Path) -> PathBuf {
    auth_path.with_file_name("accounts.json")
}

fn auth_path() -> Result<PathBuf> {
    super::auth_file_path().ok_or_else(|| anyhow::anyhow!("no home directory"))
}

fn read_accounts_at(path: &Path) -> Result<AccountsFile> {
    crate::config_file::read_json(path)
}

/// Called with the shared auth lock held. A prepared write is replayed before
/// any later writer can read a partially updated pair of credential files.
pub(super) fn recover_at(auth_path: &Path) -> Result<()> {
    let path = auth_path.with_file_name(".accounts-transaction.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("Could not read the pending account update. Restore access to the credential directory and retry the account operation."),
    };
    let transaction: AccountsTransaction = serde_json::from_str(&text)
        .context("The pending account update is not valid JSON. Restore it from a valid backup before you change accounts.")?;
    crate::config_file::write_private_json(&accounts_path(auth_path), &transaction.accounts)?;
    write_auth_file_at(auth_path, &transaction.auth)?;
    std::fs::remove_file(path)?;
    Ok(())
}

fn commit_at(auth_path: &Path, auth: AuthFile, accounts: AccountsFile) -> Result<()> {
    let path = auth_path.with_file_name(".accounts-transaction.json");
    crate::config_file::write_private_json(&path, &AccountsTransaction { auth, accounts })?;
    recover_at(auth_path)
}

/// Capture a completed login before sync can mistake it for an active-account
/// refresh. The receipt remains until finish_add supplies the user's label.
fn capture_add(provider: &str, auth: &mut AuthFile, pool: &mut Pool) -> bool {
    let AddState::Adding { owner } = pool.add_state else {
        return false;
    };
    let Some(entry) = auth.entries.get(provider).cloned() else {
        return false;
    };
    pool.active = pool.active.min(pool.accounts.len().saturating_sub(1));
    if pool
        .accounts
        .get(pool.active)
        .is_some_and(|account| account.credential.access_token() == entry.access_token())
    {
        return false;
    }
    let index = if pool
        .accounts
        .iter()
        .any(|account| account.credential.access_token() == entry.access_token())
    {
        None
    } else {
        let index = pool.accounts.len();
        pool.accounts.push(Account {
            label: format!("account {}", index + 1),
            credential: entry,
            limited_until: None,
            added_at: now_ms(),
        });
        Some(index)
    };
    if let Some(active) = pool.accounts.get(pool.active) {
        auth.entries
            .insert(provider.to_string(), active.credential.clone());
    }
    pool.add_state = AddState::Added { owner, index };
    true
}

/// Called under the auth lock before an unrelated credential write.
pub(super) fn recover_add_at(auth_path: &Path) -> Result<()> {
    // An unreadable accounts.json must not block auth.json writes, such as
    // saving a refreshed token; account commands still refuse to change it.
    let Ok(mut file) = read_accounts_at(&accounts_path(auth_path)) else {
        return Ok(());
    };
    let mut auth = read_auth_file_at(auth_path)?;
    let mut changed = false;
    for (provider, pool) in &mut file.pools {
        changed |= capture_add(provider, &mut auth, pool);
    }
    if changed {
        commit_at(auth_path, auth, file)?;
    }
    Ok(())
}

/// Copy each provider's live `auth.json` credential into its active slot.
fn sync(auth: &AuthFile, file: &mut AccountsFile) {
    for (provider, pool) in &mut file.pools {
        if matches!(pool.add_state, AddState::Adding { .. }) {
            continue;
        }
        pool.active = pool.active.min(pool.accounts.len().saturating_sub(1));
        if let (Some(entry), Some(account)) = (
            auth.entries.get(provider),
            pool.accounts.get_mut(pool.active),
        ) {
            account.credential = entry.clone();
        }
    }
    file.pools.retain(|_, pool| {
        !pool.accounts.is_empty() || matches!(pool.add_state, AddState::Adding { .. })
    });
}

/// Lock, read and sync both files, apply `f`, then write them back.
fn update_at<T>(
    auth_path: &Path,
    f: impl FnOnce(&mut AuthFile, &mut AccountsFile) -> Result<T>,
) -> Result<T> {
    let path = accounts_path(auth_path);
    crate::config_file::with_lock(auth_path, || {
        recover_at(auth_path)?;
        recover_add_at(auth_path)?;
        let mut auth = read_auth_file_at(auth_path)?;
        let mut file = read_accounts_at(&path)?;
        sync(&auth, &mut file);
        let result = f(&mut auth, &mut file)?;
        commit_at(auth_path, auth, file)?;
        Ok(result)
    })
}

fn info(provider: &str, pool: &Pool) -> PoolInfo {
    PoolInfo {
        provider: provider.to_string(),
        accounts: pool
            .accounts
            .iter()
            .enumerate()
            .map(|(index, account)| AccountInfo {
                index,
                label: account.label.clone(),
                active: index == pool.active,
                limited_until: account.limited_until,
            })
            .collect(),
    }
}

/// The provider's pool, or `None` when the provider has no pool.
pub fn pool(provider: &str) -> Option<PoolInfo> {
    read_accounts_at(&accounts_path(&auth_path().ok()?))
        .ok()?
        .pools
        .get(provider)
        .filter(|pool| !pool.accounts.is_empty())
        .map(|pool| info(provider, pool))
}

/// Every configured pool, sorted by provider ID.
pub fn pools() -> Vec<PoolInfo> {
    let Ok(path) = auth_path() else {
        return Vec::new();
    };
    read_accounts_at(&accounts_path(&path))
        .unwrap_or_default()
        .pools
        .iter()
        .filter(|(_, pool)| !pool.accounts.is_empty())
        .map(|(provider, pool)| info(provider, pool))
        .collect()
}

fn begin_add_at(auth_path: &Path, provider: &str) -> Result<()> {
    let lock = lock_add_at(auth_path, provider, true)?;
    let result = update_at(auth_path, |auth, file| {
        let pool = file.pools.entry(provider.to_string()).or_default();
        if pool.accounts.is_empty()
            && let Some(entry) = auth.entries.get(provider)
        {
            pool.accounts.push(Account {
                label: "account 1".into(),
                credential: entry.clone(),
                limited_until: None,
                added_at: now_ms(),
            });
        }
        pool.add_state = AddState::Adding {
            owner: std::process::id(),
        };
        Ok(())
    });
    if result.is_ok() {
        ADD_LOCKS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert((auth_path.to_path_buf(), provider.to_string()), lock);
    }
    result
}

/// Prepare to add an account: adopt an existing saved credential as
/// account 1 so the next login does not overwrite it.
pub fn begin_add(provider: &str) -> Result<()> {
    begin_add_at(&auth_path()?, provider)
}

fn finish_add_at(
    auth_path: &Path,
    provider: &str,
    label: Option<&str>,
) -> Result<Option<AccountInfo>> {
    let _add_lock = lock_add_at(auth_path, provider, false)?;
    let path = accounts_path(auth_path);
    let result = crate::config_file::with_lock(auth_path, || {
        recover_at(auth_path)?;
        let mut auth = read_auth_file_at(auth_path)?;
        let mut file = read_accounts_at(&path)?;
        let pool = file.pools.entry(provider.to_string()).or_default();
        if matches!(pool.add_state, AddState::Ready) {
            // Keep finish_add compatible with callers and files from before
            // account-add intent was persisted.
            pool.add_state = AddState::Adding {
                owner: std::process::id(),
            };
        }
        capture_add(provider, &mut auth, pool);
        let added = match std::mem::replace(&mut pool.add_state, AddState::Ready) {
            AddState::Added {
                index: Some(index), ..
            } if index < pool.accounts.len() => {
                if let Some(label) = label.map(str::trim).filter(|label| !label.is_empty()) {
                    pool.accounts[index].label = label.to_string();
                }
                Some(info(provider, pool).accounts[index].clone())
            }
            _ => None,
        };
        commit_at(auth_path, auth, file)?;
        Ok(added)
    });
    ADD_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(&(auth_path.to_path_buf(), provider.to_string()));
    result
}

/// Record the credential the login just saved as a new pool account and
/// restore the previously active account. Returns `None` when the login
/// saved a credential that is already in the pool.
pub fn finish_add(provider: &str, label: Option<&str>) -> Result<Option<AccountInfo>> {
    finish_add_at(&auth_path()?, provider, label)
}

fn cancel_add_at(auth_path: &Path, provider: &str, owner: Option<u32>) -> Result<()> {
    let _add_lock = match lock_add_at(auth_path, provider, false) {
        Ok(lock) => lock,
        Err(error)
            if owner.is_some()
                && error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let result = crate::config_file::with_lock(auth_path, || {
        recover_at(auth_path)?;
        let mut auth = read_auth_file_at(auth_path)?;
        let mut file = read_accounts_at(&accounts_path(auth_path))?;
        if let Some(pool) = file.pools.get_mut(provider)
            && !matches!(pool.add_state, AddState::Ready)
            && owner.is_none_or(|expected| match pool.add_state {
                AddState::Adding { owner } | AddState::Added { owner, .. } => owner == expected,
                AddState::Ready => false,
            })
        {
            capture_add(provider, &mut auth, pool);
            pool.add_state = AddState::Ready;
            commit_at(auth_path, auth, file)?;
        }
        Ok(())
    });
    ADD_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .remove(&(auth_path.to_path_buf(), provider.to_string()));
    result
}

/// End an account add that did not complete. Preserve a credential already
/// written by the login, and restore the account that was active before it.
pub fn cancel_add(provider: &str) -> Result<()> {
    cancel_add_at(&auth_path()?, provider, None)
}

/// Cancel only an add started by this process. A stale UI must not cancel
/// another process's pending login.
pub fn cancel_owned_add(provider: &str) -> Result<()> {
    cancel_add_at(&auth_path()?, provider, Some(std::process::id()))
}

fn checked_index<'a>(
    provider: &str,
    pool: Option<&'a mut Pool>,
    index: usize,
) -> Result<&'a mut Pool> {
    let Some(pool) = pool else {
        bail!("{provider} has no account pool. Add one with /accounts add {provider}");
    };
    if index >= pool.accounts.len() {
        bail!(
            "{provider} has {} accounts. Choose a number from 1 to {}",
            pool.accounts.len(),
            pool.accounts.len()
        );
    }
    Ok(pool)
}

fn activate_at(auth_path: &Path, provider: &str, index: usize) -> Result<AccountInfo> {
    update_at(auth_path, |auth, file| {
        let pool = checked_index(provider, file.pools.get_mut(provider), index)?;
        pool.active = index;
        pool.accounts[index].limited_until = None;
        auth.entries.insert(
            provider.to_string(),
            pool.accounts[index].credential.clone(),
        );
        Ok(info(provider, pool).accounts[index].clone())
    })
}

/// Save the refreshed token of the account whose credential was
/// `previous`. While that account is still the live one, `auth.json` gets
/// the token. When the pool rotated to another account during the refresh,
/// only the refreshed account's slot gets it, so the live account keeps its
/// own credential. A credential that is gone (a logout) stays gone.
pub(super) fn save_refreshed_at(
    auth_path: &Path,
    provider: &str,
    previous: &AuthEntry,
    refreshed: AuthEntry,
) -> Result<()> {
    crate::config_file::with_lock(auth_path, || {
        recover_at(auth_path)?;
        recover_add_at(auth_path)?;
        let mut auth = read_auth_file_at(auth_path)?;
        let same = |entry: &AuthEntry| entry.access_token() == previous.access_token();
        match auth.entries.get_mut(provider) {
            Some(entry) if same(entry) => {
                *entry = refreshed;
                return write_auth_file_at(auth_path, &auth);
            }
            None => return Ok(()),
            Some(_) => {}
        }
        let Ok(mut file) = read_accounts_at(&accounts_path(auth_path)) else {
            return Ok(());
        };
        let Some(account) = file.pools.get_mut(provider).and_then(|pool| {
            pool.accounts
                .iter_mut()
                .find(|account| same(&account.credential))
        }) else {
            return Ok(());
        };
        account.credential = refreshed;
        commit_at(auth_path, auth, file)
    })
}

/// Make account `index` (zero-based) active and clear its cooldown.
pub fn activate(provider: &str, index: usize) -> Result<AccountInfo> {
    activate_at(&auth_path()?, provider, index)
}

fn remove_at(auth_path: &Path, provider: &str, index: usize) -> Result<AccountInfo> {
    update_at(auth_path, |auth, file| {
        let pool = checked_index(provider, file.pools.get_mut(provider), index)?;
        let removed = info(provider, pool).accounts[index].clone();
        pool.add_state = AddState::Ready;
        pool.accounts.remove(index);
        if pool.accounts.is_empty() {
            file.pools.remove(provider);
            auth.entries.remove(provider);
            return Ok(removed);
        }
        if removed.active {
            pool.active = index.min(pool.accounts.len() - 1);
            auth.entries.insert(
                provider.to_string(),
                pool.accounts[pool.active].credential.clone(),
            );
        } else if pool.active > index {
            pool.active -= 1;
        }
        Ok(removed)
    })
}

/// Remove one account. Removing the active account activates the next one;
/// removing the last account also removes the saved credential.
pub fn remove(provider: &str, index: usize) -> Result<AccountInfo> {
    remove_at(&auth_path()?, provider, index)
}

/// Remove the provider's saved credential and its whole pool.
pub(super) fn forget_at(auth_path: &Path, provider: &str) -> Result<bool> {
    update_at(auth_path, |auth, file| {
        let pooled = file.pools.remove(provider).is_some();
        Ok(auth.entries.remove(provider).is_some() || pooled)
    })
}

fn mark_limited_and_rotate_at(
    auth_path: &Path,
    provider: &str,
    failed_secret: &str,
    retry_after: Option<Duration>,
) -> Result<Option<AccountSwitch>> {
    update_at(auth_path, |auth, file| {
        let Some(pool) = file.pools.get_mut(provider) else {
            return Ok(None);
        };
        let Some(failed) = pool
            .accounts
            .iter()
            .position(|account| account.credential.access_token() == failed_secret)
        else {
            return Ok(None);
        };
        let cooldown = retry_after.unwrap_or(DEFAULT_COOLDOWN);
        let now = now_ms();
        pool.accounts[failed].limited_until =
            Some(now.saturating_add(cooldown.as_millis().min(i64::MAX as u128) as i64));
        let total = pool.accounts.len();
        let switch = |pool: &Pool, index: usize| AccountSwitch {
            provider: provider.to_string(),
            from: pool.accounts[failed].label.clone(),
            to: pool.accounts[index].label.clone(),
            index,
            total,
            retry_after_secs: cooldown.as_secs(),
        };
        if failed != pool.active {
            // Another request or KISS process rotated already.
            return Ok(Some(switch(pool, pool.active)));
        }
        let Some(next) = (1..total)
            .map(|offset| (failed + offset) % total)
            .find(|&index| pool.accounts[index].limited_until.is_none_or(|t| t <= now))
        else {
            return Ok(None);
        };
        pool.active = next;
        auth.entries
            .insert(provider.to_string(), pool.accounts[next].credential.clone());
        Ok(Some(switch(pool, next)))
    })
}

/// Mark the pooled account whose secret is `failed_secret` as rate limited
/// and activate the next ready account. Returns `None` when the secret is
/// not a pooled account (environment variables, `--api-key`) or when every
/// other account is still limited.
pub fn mark_limited_and_rotate(
    provider: &str,
    failed_secret: &str,
    retry_after: Option<Duration>,
) -> Result<Option<AccountSwitch>> {
    mark_limited_and_rotate_at(&auth_path()?, provider, failed_secret, retry_after)
}

/// Classify a provider error. Returns `Some(reset hint)` when the error
/// means this account is rate limited or out of quota, so another account
/// may succeed. Shared capacity failures (overload, 5xx) return `None`.
pub fn rate_limit_retry_after(error: &str) -> Option<Option<Duration>> {
    let text = error.to_ascii_lowercase();
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let status = |code: u16| crate::api::mentions_status(error, code);
    if text.contains("overloaded") || [500, 502, 503, 504, 520, 529].into_iter().any(status) {
        return None;
    }
    let limited = status(429)
        || [
            "usage_limit_reached",
            "usage_not_included",
            "rate_limit_exceeded",
            "rate_limit_error",
            "resource_exhausted",
            "error_rate_limited",
            "insufficient_quota",
            "quota_exceeded",
            "exceeded your current quota",
            "used all available credits",
            "monthly spending limit",
            "usage limit",
            "rate limit exceeded",
            "subscription_sharing_usage_limit_exceeded",
        ]
        .iter()
        .any(|needle| text.contains(needle));
    limited.then(|| retry_hint(&text, &compact))
}

fn number_after(text: &str, marker: &str) -> Option<f64> {
    let start = text.find(marker)? + marker.len();
    let rest = text[start..].trim_start_matches(['"', ' ', ':']);
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn retry_hint(text: &str, compact: &str) -> Option<Duration> {
    let seconds = number_after(text, "retry-after:")
        .or_else(|| number_after(compact, "\"resets_in_seconds\""))
        .or_else(|| number_after(compact, "\"retrydelay\""))
        .or_else(|| number_after(text, "retry in "))
        .or_else(|| {
            number_after(compact, "\"resets_at\"")
                .map(|at| at - chrono::Utc::now().timestamp() as f64)
        })?;
    (seconds.is_finite() && seconds > 0.0).then(|| Duration::from_secs_f64(seconds.ceil()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oauth(access: &str) -> AuthEntry {
        AuthEntry::OAuth(super::super::OAuthCredential {
            kind: "oauth".into(),
            access: access.into(),
            refresh: format!("{access}-refresh"),
            expires: i64::MAX,
            account_id: String::new(),
            client_id: None,
            available_model_ids: None,
        })
    }

    fn login(auth_path: &Path, provider: &str, access: &str) {
        super::super::update_auth_file_at(auth_path, |auth| {
            auth.entries.insert(provider.into(), oauth(access));
            Ok(())
        })
        .unwrap();
    }

    fn active_secret(auth_path: &Path, provider: &str) -> String {
        read_auth_file_at(auth_path).unwrap().entries[provider]
            .access_token()
            .to_string()
    }

    fn pooled(auth_path: &Path, provider: &str) -> PoolInfo {
        let file = read_accounts_at(&accounts_path(auth_path)).unwrap();
        info(provider, &file.pools[provider])
    }

    /// Add accounts `one`, `two`, `three` the way `/accounts add` does.
    fn three_accounts() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let auth_path = directory.path().join("auth.json");
        login(&auth_path, "openai-codex", "one");
        for (access, label) in [("two", "personal"), ("three", "spare")] {
            begin_add_at(&auth_path, "openai-codex").unwrap();
            login(&auth_path, "openai-codex", access);
            finish_add_at(&auth_path, "openai-codex", Some(label)).unwrap();
        }
        (directory, auth_path)
    }

    #[test]
    fn a_refresh_that_finishes_after_a_rotation_updates_only_its_own_account() {
        let (_directory, auth_path) = three_accounts();
        let slot = |index: usize| {
            read_accounts_at(&accounts_path(&auth_path)).unwrap().pools["openai-codex"].accounts
                [index]
                .credential
                .access_token()
                .to_string()
        };
        save_refreshed_at(&auth_path, "openai-codex", &oauth("one"), oauth("one-new")).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one-new");

        // A refresh of account 1 starts, then the pool rotates to account 2.
        activate_at(&auth_path, "openai-codex", 1).unwrap();
        save_refreshed_at(
            &auth_path,
            "openai-codex",
            &oauth("one-new"),
            oauth("one-newer"),
        )
        .unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "two");
        assert_eq!(slot(0), "one-newer");
        assert_eq!(slot(1), "two");
        activate_at(&auth_path, "openai-codex", 2).unwrap();
        assert_eq!(slot(1), "two");
    }

    #[test]
    fn adding_accounts_adopts_the_saved_credential_and_keeps_it_active() {
        let (_directory, auth_path) = three_accounts();
        let pool = pooled(&auth_path, "openai-codex");
        let labels: Vec<_> = pool.accounts.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["account 1", "personal", "spare"]);
        assert_eq!(pool.active().unwrap().index, 0);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one");
    }

    #[test]
    fn adding_the_same_login_twice_does_not_duplicate_it() {
        let (_directory, auth_path) = three_accounts();
        begin_add_at(&auth_path, "openai-codex").unwrap();
        login(&auth_path, "openai-codex", "two");
        assert_eq!(
            finish_add_at(&auth_path, "openai-codex", None).unwrap(),
            None
        );
        assert_eq!(pooled(&auth_path, "openai-codex").accounts.len(), 3);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one");
    }

    #[test]
    fn an_interrupted_add_preserves_both_credentials_on_the_next_update() {
        let (_directory, auth_path) = three_accounts();
        begin_add_at(&auth_path, "openai-codex").unwrap();
        login(&auth_path, "openai-codex", "four");
        // The next process reads both files, without the original UI's state.
        activate_at(&auth_path, "openai-codex", 0).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one");
        activate_at(&auth_path, "openai-codex", 3).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "four");
        cancel_add_at(&auth_path, "openai-codex", None).unwrap();
    }

    #[test]
    fn a_prepared_account_write_is_recovered_before_another_auth_write() {
        let (_directory, auth_path) = three_accounts();
        let mut auth = read_auth_file_at(&auth_path).unwrap();
        let mut file = read_accounts_at(&accounts_path(&auth_path)).unwrap();
        file.pools.get_mut("openai-codex").unwrap().active = 1;
        auth.entries.insert("openai-codex".into(), oauth("two"));
        let journal = auth_path.with_file_name(".accounts-transaction.json");
        crate::config_file::write_private_json(
            &journal,
            &AccountsTransaction {
                auth,
                accounts: file,
            },
        )
        .unwrap();
        // Only the first file of the pair was replaced before the process ended.
        let transaction: AccountsTransaction =
            serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        crate::config_file::write_private_json(&accounts_path(&auth_path), &transaction.accounts)
            .unwrap();
        login(&auth_path, "anthropic", "unrelated");
        assert_eq!(active_secret(&auth_path, "openai-codex"), "two");
        activate_at(&auth_path, "openai-codex", 0).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one");
        assert!(!journal.exists());
    }

    #[test]
    fn cancelling_an_add_keeps_normal_login_and_refresh_behavior() {
        let (_directory, auth_path) = three_accounts();
        begin_add_at(&auth_path, "openai-codex").unwrap();
        cancel_add_at(&auth_path, "openai-codex", None).unwrap();
        login(&auth_path, "openai-codex", "one-refreshed");
        activate_at(&auth_path, "openai-codex", 1).unwrap();
        activate_at(&auth_path, "openai-codex", 0).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one-refreshed");
        assert_eq!(pooled(&auth_path, "openai-codex").accounts.len(), 3);
    }

    #[test]
    fn competing_account_adds_are_rejected_until_the_current_login_ends() {
        let directory = tempfile::tempdir().unwrap();
        let auth_path = directory.path().join("auth.json");
        // A separately opened file represents a lock held outside this registry.
        let other = lock_add_at(&auth_path, "openai-codex", false).unwrap();
        assert!(begin_add_at(&auth_path, "openai-codex").is_err());
        assert!(cancel_add_at(&auth_path, "openai-codex", None).is_err());
        drop(other);
        begin_add_at(&auth_path, "openai-codex").unwrap();
        assert!(begin_add_at(&auth_path, "openai-codex").is_err());
        login(&auth_path, "openai-codex", "one");
        assert!(
            finish_add_at(&auth_path, "openai-codex", None)
                .unwrap()
                .is_some()
        );
        begin_add_at(&auth_path, "openai-codex").unwrap();
        cancel_add_at(&auth_path, "openai-codex", None).unwrap();
    }

    #[test]
    fn an_invalid_prepared_write_cannot_replace_saved_credentials() {
        let (_directory, auth_path) = three_accounts();
        let before = std::fs::read(&auth_path).unwrap();
        let journal = auth_path.with_file_name(".accounts-transaction.json");
        std::fs::write(&journal, "{").unwrap();
        let result = super::super::update_auth_file_at(&auth_path, |auth| {
            auth.entries.insert("anthropic".into(), oauth("unrelated"));
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(auth_path).unwrap(), before);
        assert_eq!(std::fs::read_to_string(journal).unwrap(), "{");
    }

    #[test]
    fn rotation_skips_limited_accounts_and_wraps() {
        let (_directory, auth_path) = three_accounts();
        let switch = mark_limited_and_rotate_at(&auth_path, "openai-codex", "one", None)
            .unwrap()
            .unwrap();
        assert_eq!((switch.index, switch.total), (1, 3));
        assert_eq!(
            (switch.from.as_str(), switch.to.as_str()),
            ("account 1", "personal")
        );
        assert_eq!(switch.retry_after_secs, DEFAULT_COOLDOWN.as_secs());
        assert_eq!(active_secret(&auth_path, "openai-codex"), "two");

        // Account 1 is still cooling down, so account 2 rotates to account 3.
        let switch = mark_limited_and_rotate_at(
            &auth_path,
            "openai-codex",
            "two",
            Some(Duration::from_secs(5)),
        )
        .unwrap()
        .unwrap();
        assert_eq!(switch.index, 2);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "three");

        // Every other account is limited: no switch, account 3 stays.
        assert_eq!(
            mark_limited_and_rotate_at(&auth_path, "openai-codex", "three", None).unwrap(),
            None
        );
        assert_eq!(active_secret(&auth_path, "openai-codex"), "three");
    }

    #[test]
    fn rotation_wraps_to_an_account_whose_cooldown_ended() {
        let (_directory, auth_path) = three_accounts();
        activate_at(&auth_path, "openai-codex", 2).unwrap();
        let switch = mark_limited_and_rotate_at(&auth_path, "openai-codex", "three", None)
            .unwrap()
            .unwrap();
        assert_eq!(switch.index, 0);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one");
    }

    #[test]
    fn unknown_secret_never_rotates() {
        let (_directory, auth_path) = three_accounts();
        assert_eq!(
            mark_limited_and_rotate_at(&auth_path, "openai-codex", "from-env", None).unwrap(),
            None
        );
        assert_eq!(
            mark_limited_and_rotate_at(&auth_path, "anthropic", "one", None).unwrap(),
            None
        );
    }

    #[test]
    fn stale_failure_reports_the_account_another_process_activated() {
        let (_directory, auth_path) = three_accounts();
        activate_at(&auth_path, "openai-codex", 1).unwrap();
        let switch = mark_limited_and_rotate_at(&auth_path, "openai-codex", "one", None)
            .unwrap()
            .unwrap();
        assert_eq!(switch.index, 1);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "two");
    }

    #[test]
    fn refreshed_active_token_is_kept_when_rotating_away() {
        let (_directory, auth_path) = three_accounts();
        // Simulate an OAuth refresh that rewrote only auth.json.
        login(&auth_path, "openai-codex", "one-refreshed");
        mark_limited_and_rotate_at(&auth_path, "openai-codex", "one-refreshed", None)
            .unwrap()
            .unwrap();
        activate_at(&auth_path, "openai-codex", 0).unwrap();
        assert_eq!(active_secret(&auth_path, "openai-codex"), "one-refreshed");
        assert!(!pooled(&auth_path, "openai-codex").accounts[0].is_limited());
    }

    #[test]
    fn removing_accounts_and_logout_clean_up() {
        let (_directory, auth_path) = three_accounts();
        let removed = remove_at(&auth_path, "openai-codex", 0).unwrap();
        assert!(removed.active);
        assert_eq!(active_secret(&auth_path, "openai-codex"), "two");
        assert!(remove_at(&auth_path, "openai-codex", 5).is_err());

        assert!(forget_at(&auth_path, "openai-codex").unwrap());
        assert!(read_auth_file_at(&auth_path).unwrap().entries.is_empty());
        assert!(
            read_accounts_at(&accounts_path(&auth_path))
                .unwrap()
                .pools
                .is_empty()
        );
    }

    #[test]
    fn recognizes_account_limits_from_each_provider() {
        let secs =
            |error: &str| rate_limit_retry_after(error).map(|hint| hint.map(|d| d.as_secs()));
        // OpenAI Codex over HTTP and over the Responses WebSocket.
        assert_eq!(
            secs(
                r#"HTTP 429 Too Many Requests: {"error":{"type":"usage_limit_reached","resets_in_seconds":3600}}"#
            ),
            Some(Some(3600))
        );
        assert_eq!(
            secs(
                "WebSocket stream failed: HTTP 429 usage_limit_reached: The usage limit has been reached (retry-after: 120s)"
            ),
            Some(Some(120))
        );
        assert!(secs("response.failed: usage_not_included: plan lacks Codex").is_some());
        // Anthropic: 429 is an account limit; 529 overload is shared capacity.
        assert_eq!(
            secs(
                r#"HTTP 429 Too Many Requests: {"type":"error","error":{"type":"rate_limit_error"}} (retry-after: 42s)"#
            ),
            Some(Some(42))
        );
        assert_eq!(
            secs(
                r#"HTTP 529 <unknown status code>: {"type":"error","error":{"type":"overloaded_error"}}"#
            ),
            None
        );
        assert_eq!(secs("overloaded_error: Overloaded"), None);
        // Gemini RESOURCE_EXHAUSTED with RetryInfo.
        assert_eq!(
            secs(
                r#"HTTP 429 Too Many Requests: {"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"34s"}]}}"#
            ),
            Some(Some(34))
        );
        assert_eq!(
            secs("You exceeded your current quota. Please retry in 33.47s."),
            Some(Some(34))
        );
        // xAI credit exhaustion and Cursor resource exhaustion.
        assert!(secs("HTTP 429: Your team has either used all available credits or reached its monthly spending limit").is_some());
        assert!(secs(r#"Cursor request failed: stream ended: {"error":"ERROR_RESOURCE_EXHAUSTED","details":{"detail":"Rate limit exceeded"}}"#).is_some());
        // Unrelated failures.
        assert_eq!(secs("HTTP 401 Unauthorized: invalid token"), None);
        assert_eq!(
            secs("HTTP 503 Service Unavailable: rate limit exceeded upstream"),
            None
        );
        assert_eq!(secs("model has a 500 token limit"), None);
    }
}
