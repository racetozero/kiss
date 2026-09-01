//! Release updates and the non-blocking launch check.

use anyhow::{Context as _, Result};
use chrono::{DateTime, Duration, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

const GITHUB_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/racetozero/kiss/releases/latest";
const GITHUB_RELEASES_URL: &str = "https://github.com/racetozero/kiss/releases";
const RAW_REPOSITORY_URL: &str = "https://raw.githubusercontent.com/racetozero/kiss";
const CACHE_MAX_AGE_HOURS: i64 = 20;
const VERSION_CACHE_FILE: &str = "version.json";

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VersionCache {
    latest_version: Version,
    last_checked_at: DateTime<Utc>,
}

/// Run the explicit `kiss update` command.
pub async fn run() -> Result<i32> {
    if cfg!(debug_assertions) {
        anyhow::bail!(
            "`kiss update` is not available in debug builds; install a KISS release to use this command"
        );
    }
    run_release_update().await?;
    Ok(0)
}

async fn run_release_update() -> Result<()> {
    let client = http_client()?;
    let current = current_version()?;
    let latest = fetch_latest_version(&client).await?;

    if latest <= current {
        println!("kiss v{current} is up to date.");
        return Ok(());
    }

    println!("Updating kiss from v{current} to v{latest}...");
    let installer = download_installer(&client, &latest).await?;
    let staging = tempfile::tempdir().context("create update staging directory")?;
    let staged_binary = run_staged_installer(&installer, &latest, staging.path()).await?;

    self_replace::self_replace(&staged_binary).context("replace the current KISS executable")?;
    println!("Updated kiss to v{latest}. Restart KISS to use the new version.");
    Ok(())
}

/// Read a cached update result and refresh stale data without delaying launch.
pub fn check_on_launch() -> Option<Version> {
    let cache_path = version_cache_path()?;
    let cache = read_cache(&cache_path).ok();
    if needs_refresh(cache.as_ref(), Utc::now()) {
        tokio::spawn(async move {
            let _ = refresh_cache(&cache_path).await;
        });
    }

    let current = current_version().ok()?;
    newer_cached_version(cache.as_ref(), &current)
}

/// Build the single startup instruction shown for a newer cached release.
pub fn notice(current: &Version, latest: &Version) -> String {
    format!("Update available: kiss v{current} -> v{latest}. Run `kiss update`.")
}

fn current_version() -> Result<Version> {
    Version::parse(env!("CARGO_PKG_VERSION")).context("parse the current KISS version")
}

fn parse_release_tag(tag: &str) -> Result<Version> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(version)
        .with_context(|| format!("latest GitHub release has invalid tag `{tag}`"))
}

fn http_client() -> Result<reqwest::Client> {
    ensure_tls_crypto_provider()?;
    reqwest::Client::builder()
        .timeout(StdDuration::from_secs(10))
        .user_agent(concat!("kiss/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build the GitHub update client")
}

fn ensure_tls_crypto_provider() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        anyhow::bail!("install the Rustls crypto provider");
    }
    Ok(())
}

async fn fetch_latest_version(client: &reqwest::Client) -> Result<Version> {
    let release = client
        .get(GITHUB_LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("check the latest KISS release on GitHub")?
        .error_for_status()
        .context("GitHub did not return the latest KISS release")?
        .json::<GitHubRelease>()
        .await
        .context("read the latest KISS release from GitHub")?;
    parse_release_tag(&release.tag_name)
}

async fn download_installer(
    client: &reqwest::Client,
    version: &Version,
) -> Result<tempfile::TempPath> {
    let url = installer_url(version);
    let bytes = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("download the KISS installer from {url}"))?
        .error_for_status()
        .with_context(|| format!("KISS installer is not available at {url}"))?
        .bytes()
        .await
        .context("read the KISS installer")?;

    let mut file = tempfile::Builder::new()
        .prefix("kiss-update-")
        .suffix(installer_suffix())
        .tempfile()
        .context("create a temporary installer file")?;
    file.write_all(&bytes)
        .context("write the temporary KISS installer")?;
    file.flush().context("flush the temporary KISS installer")?;
    Ok(file.into_temp_path())
}

fn installer_url(version: &Version) -> String {
    format!("{RAW_REPOSITORY_URL}/v{version}/{}", installer_file_name())
}

#[cfg(windows)]
fn installer_file_name() -> &'static str {
    "install.ps1"
}

#[cfg(not(windows))]
fn installer_file_name() -> &'static str {
    "install.sh"
}

#[cfg(windows)]
fn installer_suffix() -> &'static str {
    ".ps1"
}

#[cfg(not(windows))]
fn installer_suffix() -> &'static str {
    ".sh"
}

async fn run_staged_installer(
    installer: &Path,
    version: &Version,
    staging_directory: &Path,
) -> Result<PathBuf> {
    let mut command = installer_command(installer)?;
    command
        .current_dir(staging_directory)
        .env("KISS_VERSION", version.to_string())
        .env("KISS_INSTALL_DIR", staging_directory)
        .env("KISS_REPOSITORY", "racetozero/kiss")
        .env("KISS_RELEASES_URL", GITHUB_RELEASES_URL)
        .env_remove("KISS_TARGET");

    let output = command
        .output()
        .await
        .context("start the KISS release installer")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        if detail.is_empty() {
            anyhow::bail!("KISS installer failed with status {}", output.status);
        }
        anyhow::bail!(
            "KISS installer failed with status {}: {detail}",
            output.status
        );
    }

    let staged_binary = staging_directory.join(staged_binary_name());
    if !staged_binary.is_file() {
        anyhow::bail!("KISS installer did not create {}", staged_binary.display());
    }
    Ok(staged_binary)
}

#[cfg(windows)]
fn installer_command(installer: &Path) -> Result<tokio::process::Command> {
    let system_root = std::env::var_os("SystemRoot").context("SystemRoot is not set")?;
    let powershell = PathBuf::from(system_root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    if !powershell.is_file() {
        anyhow::bail!("PowerShell was not found at {}", powershell.display());
    }
    let mut command = tokio::process::Command::new(powershell);
    command
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(installer);
    Ok(command)
}

#[cfg(not(windows))]
fn installer_command(installer: &Path) -> Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new("/bin/sh");
    command.arg(installer);
    Ok(command)
}

#[cfg(windows)]
fn staged_binary_name() -> &'static str {
    "kiss.exe"
}

#[cfg(not(windows))]
fn staged_binary_name() -> &'static str {
    "kiss"
}

fn version_cache_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".kiss/agent").join(VERSION_CACHE_FILE))
}

fn read_cache(path: &Path) -> Result<VersionCache> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read version cache at {}", path.display()))?;
    serde_json::from_str(&contents)
        .with_context(|| format!("parse version cache at {}", path.display()))
}

fn needs_refresh(cache: Option<&VersionCache>, now: DateTime<Utc>) -> bool {
    cache.is_none_or(|cache| cache.last_checked_at < now - Duration::hours(CACHE_MAX_AGE_HOURS))
}

fn newer_cached_version(cache: Option<&VersionCache>, current: &Version) -> Option<Version> {
    cache
        .filter(|cache| cache.latest_version > *current)
        .map(|cache| cache.latest_version.clone())
}

async fn refresh_cache(path: &Path) -> Result<()> {
    let client = http_client()?;
    let latest_version = fetch_latest_version(&client).await?;
    let cache = VersionCache {
        latest_version,
        last_checked_at: Utc::now(),
    };
    let parent = path
        .parent()
        .context("version cache path has no parent directory")?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("create version cache directory at {}", parent.display()))?;
    let json = serde_json::to_vec_pretty(&cache).context("serialize version cache")?;
    tokio::fs::write(path, json)
        .await
        .with_context(|| format!("write version cache at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn parses_release_tags_as_semantic_versions() {
        assert_eq!(parse_release_tag("v1.2.3").unwrap(), Version::new(1, 2, 3));
        assert_eq!(parse_release_tag("1.2.3").unwrap(), Version::new(1, 2, 3));
        assert!(parse_release_tag("release-1.2.3").is_err());
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    async fn debug_update_stops_before_network_or_replacement() {
        let error = run().await.unwrap_err();
        assert!(error.to_string().contains("not available in debug builds"));
    }

    #[test]
    fn builds_the_update_http_client() {
        http_client().unwrap();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn refreshes_only_missing_or_old_cache_data() {
        let now = Utc::now();
        let current = VersionCache {
            latest_version: Version::new(1, 2, 3),
            last_checked_at: now - TimeDelta::hours(19),
        };
        let old = VersionCache {
            last_checked_at: now - TimeDelta::hours(21),
            ..current.clone()
        };

        assert!(!needs_refresh(Some(&current), now));
        assert!(needs_refresh(Some(&old), now));
        assert!(needs_refresh(None, now));
    }

    #[test]
    fn returns_only_a_newer_cached_version() {
        let current = Version::new(1, 2, 3);
        let mut cache = VersionCache {
            latest_version: Version::new(1, 2, 4),
            last_checked_at: Utc::now(),
        };
        assert_eq!(
            newer_cached_version(Some(&cache), &current),
            Some(Version::new(1, 2, 4))
        );

        cache.latest_version = current.clone();
        assert_eq!(newer_cached_version(Some(&cache), &current), None);
        cache.latest_version = Version::new(1, 2, 2);
        assert_eq!(newer_cached_version(Some(&cache), &current), None);
    }

    #[test]
    fn gives_a_direct_update_instruction() {
        assert_eq!(
            notice(&Version::new(1, 2, 3), &Version::new(1, 3, 0)),
            "Update available: kiss v1.2.3 -> v1.3.0. Run `kiss update`."
        );
    }

    #[test]
    fn pins_the_installer_to_the_release_tag() {
        let url = installer_url(&Version::new(1, 2, 3));
        assert!(url.starts_with("https://raw.githubusercontent.com/racetozero/kiss/v1.2.3/"));
        assert!(url.ends_with(installer_file_name()));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn runs_the_installer_in_a_staging_directory() {
        let mut installer = tempfile::Builder::new().suffix(".sh").tempfile().unwrap();
        installer
            .write_all(
                b"#!/bin/sh\nset -eu\nprintf '%s' \"$KISS_VERSION\" > \"$KISS_INSTALL_DIR/kiss\"\nchmod 0755 \"$KISS_INSTALL_DIR/kiss\"\n",
            )
            .unwrap();
        installer.flush().unwrap();
        let staging = tempfile::tempdir().unwrap();

        let binary = run_staged_installer(installer.path(), &Version::new(1, 2, 3), staging.path())
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(binary).unwrap(), "1.2.3");
    }
}
