//! Microsoft Entra ID credentials for Azure OpenAI.

use crate::ResolvedCredential;
use anyhow::{Context as _, Result};
use std::sync::{Arc, OnceLock};

const SCOPE: &str = "https://ai.azure.com/.default";
pub(super) const ENTRA_MARKER: &str = "azure-entra-id";
pub(super) const PROVIDER: &str = "azure-openai-responses";

pub(super) fn env_var_names() -> &'static [&'static str] {
    &["AZURE_OPENAI_API_KEY", "AZURE_OPENAI_AUTH_TOKEN"]
}

pub(super) fn is_entra_marker(entry: &super::AuthEntry) -> bool {
    matches!(entry, super::AuthEntry::Detailed { key, .. } if key == ENTRA_MARKER)
}

pub(super) fn resolve_local(
    declared: &std::collections::BTreeMap<String, String>,
) -> Option<ResolvedCredential> {
    if let Some(entry) = super::read_auth_file().entries.get(PROVIDER) {
        return resolve_entry_local(entry);
    }
    for variable in env_var_names() {
        if let Ok(value) = std::env::var(variable)
            && !value.is_empty()
        {
            return Some(if *variable == "AZURE_OPENAI_AUTH_TOKEN" {
                ResolvedCredential::bearer(value)
            } else {
                ResolvedCredential::api_key(value)
            });
        }
    }
    let value = declared.get(PROVIDER)?;
    if let Some(variable) = value.strip_prefix('$') {
        let value = std::env::var(variable)
            .ok()
            .filter(|value| !value.is_empty())?;
        Some(if variable == "AZURE_OPENAI_AUTH_TOKEN" {
            ResolvedCredential::bearer(value)
        } else {
            ResolvedCredential::api_key(value)
        })
    } else {
        Some(ResolvedCredential::api_key(value))
    }
}

fn resolve_entry_local(entry: &super::AuthEntry) -> Option<ResolvedCredential> {
    if is_entra_marker(entry) {
        return None;
    }
    Some(match entry {
        super::AuthEntry::Key(key) | super::AuthEntry::Detailed { key, .. } => {
            ResolvedCredential::api_key(key)
        }
        super::AuthEntry::OAuth(credential) => ResolvedCredential::bearer(&credential.access),
    })
}

pub(super) async fn resolve_entry(entry: &super::AuthEntry) -> Result<ResolvedCredential> {
    if is_entra_marker(entry) {
        return Ok(ResolvedCredential::bearer(access_token().await?));
    }
    Ok(resolve_entry_local(entry).expect("non-Entra Azure entry has a local credential"))
}

pub(super) async fn resolve_async(
    declared: &std::collections::BTreeMap<String, String>,
) -> Result<Option<ResolvedCredential>> {
    if let Some(entry) = super::read_auth_file().entries.get(PROVIDER) {
        return Ok(Some(resolve_entry(entry).await?));
    }
    Ok(resolve_local(declared))
}

async fn access_token() -> Result<String> {
    static CREDENTIAL: OnceLock<Arc<dyn azure_core::auth::TokenCredential>> = OnceLock::new();
    let credential = if let Some(credential) = CREDENTIAL.get() {
        credential.clone()
    } else {
        let credential = azure_identity::create_default_credential()
            .context("create Microsoft Entra default credential")?;
        let _ = CREDENTIAL.set(credential);
        CREDENTIAL
            .get()
            .expect("Azure credential was just initialized")
            .clone()
    };
    let token = credential
        .get_token(&[SCOPE])
        .await
        .context("get Microsoft Entra token for Azure OpenAI")?;
    if token.token.secret().is_empty() {
        anyhow::bail!("Microsoft Entra returned an empty Azure OpenAI token");
    }
    Ok(token.token.secret().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_azure_entries_keep_their_credential_kind() {
        assert_eq!(
            resolve_entry_local(&super::super::AuthEntry::Key("key".into())),
            Some(ResolvedCredential::api_key("key"))
        );
        assert_eq!(
            resolve_entry_local(&super::super::AuthEntry::Detailed {
                key: ENTRA_MARKER.into(),
                env: Default::default(),
            }),
            None
        );
    }
}
