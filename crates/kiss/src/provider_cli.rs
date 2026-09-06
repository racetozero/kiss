use crate::args::ProviderCommand;
use anyhow::{Context as _, Result, bail};
use kiss_ai::provider_config::{self, AddProvider};
use std::collections::BTreeMap;

pub fn run(command: &ProviderCommand) -> Result<i32> {
    match command {
        ProviderCommand::List { json } => {
            let providers = provider_config::list()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&providers)?);
            } else if providers.is_empty() {
                println!("No custom providers configured.");
            } else {
                for provider in providers {
                    let models = provider.models.join(", ");
                    println!(
                        "{}\t{}\t{}\t{}",
                        provider.id, provider.api, provider.base_url, models
                    );
                }
            }
            Ok(0)
        }
        ProviderCommand::Add {
            id,
            base_url,
            api,
            model,
            name,
            model_name,
            api_key_env,
            auth_provider,
            headers,
            reasoning,
            context_window,
            max_tokens,
        } => {
            let path = provider_config::add(&AddProvider {
                id: id.clone(),
                name: name.clone(),
                base_url: base_url.clone(),
                api: *api,
                model_id: model.clone(),
                model_name: model_name.clone(),
                api_key_env: api_key_env.clone(),
                auth_provider: auth_provider.clone(),
                headers: parse_headers(headers)?,
                reasoning: *reasoning,
                context_window: *context_window,
                max_tokens: *max_tokens,
            })?;
            println!("Saved provider {id} in {}.", path.display());
            Ok(0)
        }
        ProviderCommand::Remove { id } => {
            if provider_config::remove(id)? {
                println!("Removed provider {id}.");
            } else {
                println!("No custom provider named {id}.");
            }
            Ok(0)
        }
    }
}

pub(crate) fn parse_headers(values: &[String]) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let (name, header_value) = value
                .split_once('=')
                .with_context(|| format!("header '{value}' must use KEY=VALUE"))?;
            if name.trim().is_empty() || header_value.contains(['\r', '\n']) {
                bail!("invalid HTTP header '{value}'");
            }
            Ok((name.trim().to_string(), header_value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_headers_at_first_equals_sign() {
        let headers = parse_headers(&["Authorization=Bearer=a".into()]).unwrap();
        assert_eq!(headers["Authorization"], "Bearer=a");
    }

    #[test]
    fn rejects_header_without_equals_sign() {
        assert!(parse_headers(&["broken".into()]).is_err());
    }
}
