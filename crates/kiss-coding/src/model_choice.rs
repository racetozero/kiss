//! Which model a new session uses when the caller does not name one.

use crate::Settings;
use crate::session::manager::SessionManager;
use anyhow::{Context as _, Result};
use kiss_ai::{Model, Registry, ThinkingLevel};

/// What the caller asked for: a model pattern, a provider, and whether an
/// API key was given for this run.
pub struct ModelRequest<'a> {
    pub pattern: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub api_key_given: bool,
    /// Import a credential from another installed tool when no stored one
    /// is usable. The CLI does; a library must not write the user's
    /// credential file on its own.
    pub import_external_credentials: bool,
}

/// The model for a session: the requested pattern, else the settings
/// default, else the provider's default, else the first catalog model with a
/// usable credential (importing one from another tool when allowed and
/// that is the only way). `Ok(None)` when no model is usable; the caller says how to fix it.
pub fn choose(
    request: &ModelRequest<'_>,
    settings: &Settings,
    registry: &Registry,
) -> Result<Option<(Model, Option<ThinkingLevel>)>> {
    // CLI flag > settings default > first model with an available key.
    if let Some(pattern) = &request.pattern {
        return registry
            .resolve(pattern, request.provider)
            .map(Some)
            .with_context(|| format!("no model matches pattern '{pattern}'"));
    }
    if let (Some(provider), Some(model)) = (&settings.default_provider, &settings.default_model)
        && let Some(found) = registry.resolve(model, Some(provider))
    {
        return Ok(Some(found));
    }
    if let Some(provider) = request.provider.or(settings.default_provider.as_deref())
        && let Some(model) = registry.provider_default(provider)
    {
        return Ok(Some((model, None)));
    }
    if request.api_key_given {
        let provider = request.provider.unwrap_or("anthropic");
        if let Some(model) = registry.provider_default(provider) {
            return Ok(Some((model, None)));
        }
    }
    for model in registry.all() {
        if kiss_ai::auth::resolve_api_key_local(
            registry.credential_provider(&model.provider),
            &registry.declared_keys,
        )
        .is_some()
        {
            return Ok(Some((model.clone(), None)));
        }
    }
    if !request.import_external_credentials {
        return Ok(None);
    }
    // External discovery can query the macOS Keychain. Run it once only after
    // the fast local scan finds no usable credential.
    let external = kiss_ai::auth::external::discover();
    let mut checked = std::collections::HashSet::new();
    for model in registry.all() {
        let credential_provider = registry.credential_provider(&model.provider);
        if !checked.insert(credential_provider) {
            continue;
        }
        if kiss_ai::auth::external::auto_import_unique_from_sources(credential_provider, &external)
            .ok()
            .flatten()
            .is_some()
            && kiss_ai::auth::resolve_api_key_local(
                registry.credential_provider(&model.provider),
                &registry.declared_keys,
            )
            .is_some()
        {
            return Ok(Some((model.clone(), None)));
        }
    }
    Ok(None)
}

/// The model and thinking level a session starts with, given the `chosen`
/// result of [`choose`]. A resumed session keeps the model it last used
/// unless the request names a model or a provider, and keeps its thinking
/// level unless `thinking` or the model pattern names one.
pub fn for_session(
    request: &ModelRequest<'_>,
    thinking: Option<ThinkingLevel>,
    chosen: (Model, Option<ThinkingLevel>),
    manager: &SessionManager,
    settings: &Settings,
    registry: &Registry,
) -> (Model, ThinkingLevel) {
    let (model, pattern_thinking) = chosen;
    let (saved_model, saved_thinking) = manager.session_settings();
    let model = saved_model
        .filter(|_| request.pattern.is_none() && request.provider.is_none())
        .and_then(|(provider, id)| registry.resolve(&id, Some(&provider)))
        .map_or(model, |(model, _)| model);
    let thinking = thinking
        .or(pattern_thinking)
        .or(saved_thinking)
        .or_else(|| {
            settings
                .default_thinking_level
                .as_deref()
                .and_then(ThinkingLevel::parse)
        })
        .unwrap_or(ThinkingLevel::Off);
    (model, thinking)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resumed_session_keeps_its_model_unless_the_caller_names_one() {
        let registry = Registry::from_builtin();
        let settings = Settings::default();
        let mut manager = SessionManager::in_memory(std::path::Path::new("/"));
        let saved = registry.provider_default("anthropic").unwrap();
        manager
            .append_model_change(&saved.provider, &saved.id)
            .unwrap();
        manager
            .append_thinking_level_change(ThinkingLevel::High)
            .unwrap();
        let chosen = registry.provider_default("openai").unwrap();
        let request = |provider| ModelRequest {
            pattern: None,
            provider,
            api_key_given: true,
            import_external_credentials: false,
        };

        let (model, thinking) = for_session(
            &request(None),
            None,
            (chosen.clone(), None),
            &manager,
            &settings,
            &registry,
        );
        assert_eq!(
            (model.provider.as_str(), thinking),
            ("anthropic", ThinkingLevel::High)
        );

        let (model, thinking) = for_session(
            &request(Some("openai")),
            Some(ThinkingLevel::Low),
            (chosen, None),
            &manager,
            &settings,
            &registry,
        );
        assert_eq!(
            (model.provider.as_str(), thinking),
            ("openai", ThinkingLevel::Low)
        );
    }
}
