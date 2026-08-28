//! Browser authentication shared by CLI and interactive mode.

use anyhow::Result;
use tokio_util::sync::CancellationToken;

pub async fn login_browser(
    provider: &str,
    cancel: &CancellationToken,
    show_url: impl FnOnce(&str),
) -> Result<()> {
    let credential = match provider {
        "openai-codex" => {
            kiss_ai::auth::openai_codex::login_browser(&Default::default(), cancel, show_url)
                .await?
        }
        "anthropic" => {
            kiss_ai::auth::anthropic::login_browser(&Default::default(), cancel, show_url).await?
        }
        "openrouter" => {
            kiss_ai::auth::openrouter::login_browser(&Default::default(), cancel, show_url).await?
        }
        "radius" => {
            kiss_ai::auth::radius::login_browser(&Default::default(), cancel, show_url).await?
        }
        _ => anyhow::bail!("{provider} does not provide browser authentication"),
    };
    kiss_ai::auth::store_oauth(provider, credential)
}

pub fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    return false;

    command.arg(url).spawn().is_ok()
}
