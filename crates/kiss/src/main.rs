mod args;
mod auth_flow;
mod export;
mod file_search;
mod modes {
    pub mod interactive;
    pub mod json;
    pub mod print;
}
mod mcp_cli;
mod setup;
mod slash_commands;
mod update;

use args::{Args, Command};
use clap::Parser;
use std::io::{IsTerminal as _, Write as _};

fn main() {
    let args = Args::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = runtime.block_on(run(args)).unwrap_or_else(|err| {
        eprintln!("error: {err:#}");
        1
    });
    std::process::exit(code);
}

async fn run(args: Args) -> anyhow::Result<i32> {
    if let Some(command) = &args.command {
        return run_command(command).await;
    }

    // --list-models
    if let Some(search) = &args.list_models {
        let registry = kiss_ai::Registry::load(None);
        let needle = search.to_lowercase();
        for model in registry.all() {
            let label = format!("{}/{}", model.provider, model.id);
            if needle.is_empty() || label.to_lowercase().contains(&needle) {
                println!(
                    "{label}\t{}\tctx {}k",
                    model.display_name(),
                    model.context_window / 1000
                );
            }
        }
        return Ok(0);
    }

    // --export <in> [out]
    if let Some(export_args) = &args.export {
        let input = std::path::PathBuf::from(&export_args[0]);
        let manager = kiss_coding::SessionManager::open(&input)?;
        let output = export_args
            .get(1)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| input.with_extension("html"));
        export::export_session(&manager, &output)?;
        println!("exported to {}", output.display());
        return Ok(0);
    }

    match args.mode.as_deref() {
        Some("json") => modes::json::run(&args).await,
        Some(other) => anyhow::bail!("unknown mode: {other} (supported: json)"),
        None if args.print => modes::print::run(&args).await,
        None => modes::interactive::run(&args).await,
    }
}

async fn run_command(command: &Command) -> anyhow::Result<i32> {
    match command {
        Command::Update => update::run().await,
        Command::Mcp { command } => mcp_cli::run(command).await,
        Command::Login {
            provider,
            device_auth,
            browser: _,
            api_key,
        } => {
            if api_key.is_none()
                && matches!(
                    provider.as_str(),
                    "openai-codex"
                        | "anthropic"
                        | "github-copilot"
                        | "kimi-coding"
                        | "openrouter"
                        | "radius"
                        | "xai"
                )
            {
                run_oauth_login(provider, *device_auth).await?;
                println!("Saved OAuth credentials for {provider}.");
            } else {
                let key = match api_key {
                    Some(key) => key.clone(),
                    None => rpassword::prompt_password(format!("API key for {provider}: "))?,
                };
                if key.trim().is_empty() {
                    anyhow::bail!("API key cannot be empty");
                }
                kiss_ai::auth::store_api_key(provider, key.trim())?;
                println!("Saved API key for {provider}.");
            }
            Ok(0)
        }
        Command::Logout { provider } => {
            if kiss_ai::auth::remove_api_key(provider)? {
                println!("Removed saved credentials for {provider}.");
            } else {
                println!("No saved credentials for {provider}.");
            }
            Ok(0)
        }
        Command::Auth {
            action_or_provider,
            provider,
            yes,
        } if action_or_provider.as_deref() == Some("import") => {
            import_external_credentials(provider.as_deref(), *yes)
        }
        Command::Auth {
            action_or_provider,
            provider: _,
            yes: _,
        } => {
            let providers: Vec<&str> = match action_or_provider.as_deref() {
                Some(provider) => vec![provider],
                None => kiss_ai::registry::BUILTIN_PROVIDER_IDS.to_vec(),
            };
            let external = kiss_ai::auth::external::discover();
            for provider in providers {
                let source = match kiss_ai::auth::stored_auth_kind(provider) {
                    Some(kiss_ai::auth::StoredAuthKind::OAuth) => "saved OAuth",
                    Some(kiss_ai::auth::StoredAuthKind::ApiKey) => "saved API key",
                    None => {
                        let environment = kiss_ai::auth::env_var_names(provider)
                            .iter()
                            .find(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
                            .copied();
                        if let Some(environment) = environment {
                            environment
                        } else if let Some(found) =
                            external.iter().find(|source| source.provider == provider)
                        {
                            println!(
                                "{provider}\texternal {} ({})",
                                found.application, found.location
                            );
                            continue;
                        } else {
                            "not configured"
                        }
                    }
                };
                println!("{provider}\t{source}");
            }
            Ok(0)
        }
    }
}

async fn run_oauth_login(provider: &str, headless: bool) -> anyhow::Result<()> {
    let cancel = tokio_util::sync::CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    if !headless
        && matches!(
            provider,
            "openai-codex" | "anthropic" | "openrouter" | "radius"
        )
    {
        let result = auth_flow::login_browser(provider, &cancel, |url| {
            println!("Open this URL to sign in:\n{url}");
            if !auth_flow::open_browser(url) {
                eprintln!("The browser did not open. Open the URL manually.");
            }
        })
        .await;
        signal_task.abort();
        return result;
    }
    let result: anyhow::Result<kiss_ai::auth::OAuthCredential> = async {
        match provider {
            "openai-codex" if headless => {
                let config = kiss_ai::auth::openai_codex::OAuthConfig::default();
                let device =
                    kiss_ai::auth::openai_codex::start_device_authorization(&config, &cancel)
                        .await?;
                show_device_code(&device.verification_uri, &device.user_code);
                kiss_ai::auth::openai_codex::finish_device_authorization(&config, &device, &cancel)
                    .await
            }
            "anthropic" if headless => {
                let config = kiss_ai::auth::anthropic::OAuthConfig::default();
                let pending = kiss_ai::auth::anthropic::start_authorization(&config)?;
                println!("Open this URL to sign in:\n{}", pending.authorization_url);
                let input = rpassword::prompt_password(
                    "Paste the final callback URL or authorization code: ",
                )?;
                kiss_ai::auth::anthropic::finish_authorization(&config, &pending, &input, &cancel)
                    .await
            }
            "openrouter" if headless => {
                let config = kiss_ai::auth::openrouter::OAuthConfig::default();
                let pending = kiss_ai::auth::openrouter::start_authorization(
                    &config,
                    "http://localhost:8484/oauth/callback",
                )?;
                println!("Open this URL to sign in:\n{}", pending.authorization_url);
                let input = rpassword::prompt_password(
                    "Paste the final callback URL or authorization code: ",
                )?;
                kiss_ai::auth::openrouter::finish_authorization(&config, &pending, &input, &cancel)
                    .await
            }
            "github-copilot" => {
                let domain = prompt_line("GitHub Enterprise domain (blank for github.com): ")?;
                let config = kiss_ai::auth::github_copilot::OAuthConfig {
                    domain: kiss_ai::auth::github_copilot::normalize_domain(&domain)?,
                };
                let device = kiss_ai::auth::github_copilot::start(&config, &cancel).await?;
                show_device_code(&device.verification_uri, &device.user_code);
                kiss_ai::auth::github_copilot::finish(&config, &device, &cancel).await
            }
            "kimi-coding" => {
                let config = kiss_ai::auth::kimi_coding::OAuthConfig::default();
                let device = kiss_ai::auth::kimi_coding::start(&config, &cancel).await?;
                show_device_code(&device.verification_uri, &device.user_code);
                kiss_ai::auth::kimi_coding::finish(&config, &device, &cancel).await
            }
            "xai" => {
                let config = kiss_ai::auth::xai::OAuthConfig::default();
                let device = kiss_ai::auth::xai::start(&config, &cancel).await?;
                show_device_code(&device.verification_uri, &device.user_code);
                kiss_ai::auth::xai::finish(&config, &device, &cancel).await
            }
            "radius" => {
                let config = kiss_ai::auth::radius::OAuthConfig::default();
                let device = kiss_ai::auth::radius::start_device(&config, &cancel).await?;
                show_device_code(&device.verification_uri, &device.user_code);
                kiss_ai::auth::radius::finish_device(&config, &device, &cancel).await
            }
            _ => anyhow::bail!("{provider} does not support OAuth login"),
        }
    }
    .await;
    signal_task.abort();
    match result {
        Ok(credential) => kiss_ai::auth::store_oauth(provider, credential),
        Err(error) => Err(error),
    }
}

fn show_device_code(url: &str, code: &str) {
    println!("Open {url}");
    println!("Enter code: {code}");
}

fn prompt_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut value = String::new();
    std::io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

fn import_external_credentials(provider: Option<&str>, yes: bool) -> anyhow::Result<i32> {
    let mut selected = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for source in kiss_ai::auth::external::discover() {
        if provider.is_some_and(|provider| provider != source.provider) {
            continue;
        }
        if seen.insert(source.provider.clone()) {
            selected.push(source);
        }
    }
    if selected.is_empty() {
        match provider {
            Some(provider) => println!("No supported external credentials found for {provider}."),
            None => println!("No supported external credentials found."),
        }
        return Ok(0);
    }

    println!("Found external credentials:");
    for source in &selected {
        println!(
            "  {}\t{}\t{}",
            source.provider, source.application, source.location
        );
    }
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("credential import needs a terminal or --yes");
        }
        print!("Import these credentials into ~/.kiss/agent/auth.json? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Import cancelled.");
            return Ok(0);
        }
    }
    for source in &selected {
        kiss_ai::auth::external::import(source)?;
        println!(
            "Imported {} credentials from {}.",
            source.provider, source.application
        );
    }
    Ok(0)
}
