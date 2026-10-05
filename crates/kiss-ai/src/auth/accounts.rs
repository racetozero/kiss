//! Multi-account pools with rate-limit failover.
//!
//! The active account's credential stays in `auth.json` under the provider
//! key, so every existing resolver, refresher, and model list keeps working.
//! `accounts.json` beside it holds the ordered pool: labels, cooldowns, and
//! the credentials of inactive accounts. Each update first copies the
//! `auth.json` entry into the active pool slot ("sync"), so refreshed tokens
//! survive rotation. Both files are written under the shared auth lock.

use super::{AuthEntry, AuthFile, read_auth_file_at, with_auth_lock, write_auth_file_at};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Cooldown for a limited account whose provider reported no reset time.
pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(flatten)]
    pools: BTreeMap<String, Pool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Pool {
    active: usize,
    accounts: Vec<Account>,
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

fn read_accounts_at(path: &Path) -> AccountsFile {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[derive(Default)]
struct Cache {
    path: Option<PathBuf>,
    signature: Option<(u64, SystemTime)>,
    value: AccountsFile,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Read with a metadata check so the footer can call this on every frame.
fn read_accounts_cached_at(path: &Path) -> AccountsFile {
    let signature = std::fs::metadata(path)
        .ok()
        .and_then(|metadata| Some((metadata.len(), metadata.modified().ok()?)));
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if cache.path.as_deref() != Some(path) || cache.signature != signature {
        cache.value = read_accounts_at(path);
        cache.path = Some(path.to_path_buf());
        cache.signature = signature;
    }
    cache.value.clone()
}

/// Copy each provider's live `auth.json` credential into its active slot.
fn sync(auth: &AuthFile, file: &mut AccountsFile) {
    for (provider, pool) in &mut file.pools {
        pool.active = pool.active.min(pool.accounts.len().saturating_sub(1));
        if let (Some(entry), Some(account)) = (
            auth.entries.get(provider),
            pool.accounts.get_mut(pool.active),
        ) {
            account.credential = entry.clone();
        }
    }
    file.pools.retain(|_, pool| !pool.accounts.is_empty());
}

/// Lock, read and sync both files, apply `f`, then write them back.
fn update_at<T>(
    auth_path: &Path,
    f: impl FnOnce(&mut AuthFile, &mut AccountsFile) -> Result<T>,
) -> Result<T> {
    let path = accounts_path(auth_path);
    with_auth_lock(auth_path, || {
        let mut auth = read_auth_file_at(auth_path);
        let mut file = read_accounts_at(&path);
        sync(&auth, &mut file);
        let result = f(&mut auth, &mut file)?;
        write_auth_file_at(auth_path, &auth)?;
        if path.exists() || !file.pools.is_empty() {
            super::write_json_at(&path, &file)?;
        }
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
    read_accounts_cached_at(&accounts_path(&auth_path().ok()?))
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
    read_accounts_cached_at(&accounts_path(&path))
        .pools
        .iter()
        .filter(|(_, pool)| !pool.accounts.is_empty())
        .map(|(provider, pool)| info(provider, pool))
        .collect()
}

fn begin_add_at(auth_path: &Path, provider: &str) -> Result<()> {
    update_at(auth_path, |auth, file| {
        if !file.pools.contains_key(provider)
            && let Some(entry) = auth.entries.get(provider)
        {
            file.pools.insert(
                provider.to_string(),
                Pool {
                    active: 0,
                    accounts: vec![Account {
                        label: "account 1".into(),
                        credential: entry.clone(),
                        limited_until: None,
                        added_at: now_ms(),
                    }],
                },
            );
        }
        Ok(())
    })
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
    let path = accounts_path(auth_path);
    with_auth_lock(auth_path, || {
        // Do not sync first: the login just replaced auth.json, and the
        // active slot still holds the previous account's latest credential.
        let mut auth = read_auth_file_at(auth_path);
        let mut file = read_accounts_at(&path);
        let Some(new_entry) = auth.entries.get(provider).cloned() else {
            return Ok(None);
        };
        let pool = file.pools.entry(provider.to_string()).or_default();
        if pool
            .accounts
            .iter()
            .any(|account| account.credential.access_token() == new_entry.access_token())
        {
            return Ok(None);
        }
        let number = pool.accounts.len() + 1;
        pool.accounts.push(Account {
            label: label
                .map(str::trim)
                .filter(|label| !label.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("account {number}")),
            credential: new_entry,
            limited_until: None,
            added_at: now_ms(),
        });
        let index = pool.accounts.len() - 1;
        if index > 0 {
            // Keep the previously active account in use.
            pool.active = pool.active.min(index - 1);
            auth.entries.insert(
                provider.to_string(),
                pool.accounts[pool.active].credential.clone(),
            );
            write_auth_file_at(auth_path, &auth)?;
        } else {
            pool.active = 0;
        }
        let added = info(provider, pool).accounts[index].clone();
        super::write_json_at(&path, &file)?;
        Ok(Some(added))
    })
}

/// Record the credential the login just saved as a new pool account and
/// restore the previously active account. Returns `None` when the login
/// saved a credential that is already in the pool.
pub fn finish_add(provider: &str, label: Option<&str>) -> Result<Option<AccountInfo>> {
    finish_add_at(&auth_path()?, provider, label)
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

/// Make account `index` (zero-based) active and clear its cooldown.
pub fn activate(provider: &str, index: usize) -> Result<AccountInfo> {
    activate_at(&auth_path()?, provider, index)
}

fn remove_at(auth_path: &Path, provider: &str, index: usize) -> Result<AccountInfo> {
    update_at(auth_path, |auth, file| {
        let pool = checked_index(provider, file.pools.get_mut(provider), index)?;
        let removed = info(provider, pool).accounts[index].clone();
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
    let status = |code: u16| {
        [
            format!("http {code}"),
            format!("status {code}"),
            format!("status: {code}"),
            format!("status code {code}"),
            format!("{code} too many requests"),
        ]
        .iter()
        .any(|marker| text.contains(marker))
            || compact.contains(&format!("\"status\":{code}"))
            || compact.contains(&format!("\"code\":{code}"))
    };
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
        read_auth_file_at(auth_path).entries[provider]
            .access_token()
            .to_string()
    }

    fn pooled(auth_path: &Path, provider: &str) -> PoolInfo {
        let file = read_accounts_at(&accounts_path(auth_path));
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
        assert!(read_auth_file_at(&auth_path).entries.is_empty());
        assert!(
            read_accounts_at(&accounts_path(&auth_path))
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
