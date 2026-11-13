//! MCP configuration discovery, merge, validation, and safe writes.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    User,
    Project,
}

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub shared_global: PathBuf,
    pub agents_global: PathBuf,
    pub agents_nested_global: PathBuf,
    pub kiss_global: PathBuf,
    pub shared_project: PathBuf,
    pub kiss_project: PathBuf,
}

impl ConfigPaths {
    pub fn discover(cwd: &Path) -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        Ok(Self::for_home(&home, cwd))
    }

    pub fn for_home(home: &Path, cwd: &Path) -> Self {
        Self {
            shared_global: home.join(".config/mcp/mcp.json"),
            agents_global: home.join(".agents/mcp.json"),
            agents_nested_global: home.join(".agents/mcp/mcp.json"),
            kiss_global: kiss_ai::paths::agent_dir_in(home).join("mcp.json"),
            shared_project: cwd.join(".mcp.json"),
            kiss_project: cwd.join(".kiss/mcp.json"),
        }
    }

    pub fn write_path(&self, scope: ConfigScope) -> &Path {
        match scope {
            ConfigScope::User => &self.kiss_global,
            ConfigScope::Project => &self.shared_project,
        }
    }

    fn read_sources(&self, trusted_project: bool) -> Vec<ConfigSource> {
        let mut sources = vec![
            ConfigSource::new("shared global", self.shared_global.clone(), false),
            ConfigSource::new("agents global", self.agents_global.clone(), false),
            ConfigSource::new(
                "agents nested global",
                self.agents_nested_global.clone(),
                false,
            ),
            ConfigSource::new("KISS global", self.kiss_global.clone(), false),
        ];
        if trusted_project {
            sources.push(ConfigSource::new(
                "project .mcp.json",
                self.shared_project.clone(),
                true,
            ));
            sources.push(ConfigSource::new(
                "project KISS MCP",
                self.kiss_project.clone(),
                true,
            ));
        }
        sources
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSource {
    pub label: String,
    pub path: PathBuf,
    pub project: bool,
}

impl ConfigSource {
    fn new(label: &str, path: PathBuf, project: bool) -> Self {
        Self {
            label: label.to_string(),
            path,
            project,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpConfig {
    pub mcp_servers: BTreeMap<String, ServerEntry>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    OAuth,
    Bearer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AuthSetting {
    Mode(AuthMode),
    Disabled(bool),
    /// Send the current token of a KISS provider login as the bearer token.
    Provider {
        provider: String,
    },
}

impl AuthSetting {
    pub fn mode(&self) -> Option<AuthMode> {
        match self {
            Self::Mode(mode) => Some(*mode),
            Self::Disabled(_) | Self::Provider { .. } => None,
        }
    }
}

/// How KISS identifies itself to an authorization server without `clientId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClientRegistration {
    /// The Client ID Metadata Document at `clientMetadataUrl` when the
    /// authorization server supports it, otherwise dynamic registration.
    /// `clientName` or `redirectUri` selects dynamic registration.
    #[default]
    Auto,
    /// Always dynamic client registration.
    Dcr,
    /// Always the Client ID Metadata Document at `clientMetadataUrl`.
    Cimd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrantType {
    #[default]
    AuthorizationCode,
    ClientCredentials,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OAuthConfig {
    pub grant_type: OAuthGrantType,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scope: Option<String>,
    pub authorization_params: BTreeMap<String, String>,
    pub redirect_uri: Option<String>,
    pub client_name: Option<String>,
    pub client_uri: Option<String>,
    pub logo_uri: Option<String>,
    pub skip_issuer_metadata_validation: bool,
    pub client_registration: ClientRegistration,
    /// HTTPS URL of the Client ID Metadata Document for `cimd` registration.
    pub client_metadata_url: Option<String>,
    /// Authorization server metadata to use instead of discovery.
    pub auth_server_metadata_url: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerEntry {
    /// One line that tells the model what the server is for.
    pub description: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub auth: Option<AuthSetting>,
    pub bearer_token: Option<String>,
    pub bearer_token_env: Option<String>,
    pub oauth: Option<OAuthConfig>,
    pub lifecycle: Option<String>,
    pub idle_timeout: Option<u64>,
    pub request_timeout_ms: Option<u64>,
    pub include_tools: Vec<String>,
    pub exclude_tools: Vec<String>,
    pub disabled: bool,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ServerEntry {
    pub fn validate(&self, name: &str) -> Result<()> {
        validate_server_name(name)?;
        match (self.command.as_deref(), self.url.as_deref()) {
            (Some(command), None) if !command.trim().is_empty() => {}
            (None, Some(url)) => {
                let parsed = url::Url::parse(url)
                    .with_context(|| format!("MCP server `{name}` has an invalid URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("MCP server `{name}` URL must use http or https");
                }
            }
            (Some(_), Some(_)) => {
                bail!("MCP server `{name}` must use either command or url, not both")
            }
            _ => bail!("MCP server `{name}` must define command or url"),
        }
        if matches!(self.auth, Some(AuthSetting::Disabled(true))) {
            bail!("MCP server `{name}` uses invalid auth value true. Use false to disable auth")
        }
        if self.command.is_some()
            && (self.auth.is_some()
                || self.oauth.is_some()
                || self.bearer_token.is_some()
                || self.bearer_token_env.is_some())
        {
            bail!("MCP server `{name}` can use HTTP authentication only with url")
        }
        if let Some(oauth) = &self.oauth
            && oauth.grant_type == OAuthGrantType::ClientCredentials
            && (oauth.client_id.as_deref().unwrap_or("").is_empty()
                || oauth.client_secret.as_deref().unwrap_or("").is_empty())
        {
            bail!("MCP server `{name}` client_credentials needs clientId and clientSecret")
        }
        if let Some(oauth) = &self.oauth {
            oauth.validate(name)?;
        }
        if let Some(AuthSetting::Provider { provider }) = &self.auth {
            if provider.trim().is_empty() {
                bail!(
                    "MCP server `{name}` has an empty auth.provider. Name a KISS provider that you signed in to, for example {{\"provider\": \"radius\"}}"
                )
            }
            if self.oauth.is_some()
                || self.bearer_token.is_some()
                || self.bearer_token_env.is_some()
            {
                bail!(
                    "MCP server `{name}` sets auth.provider together with oauth or a bearer token. Keep only one authentication method"
                )
            }
            if let Some(url) = &self.url
                && !secure_url(url)
            {
                bail!(
                    "MCP server `{name}` sends the `{provider}` login token to {url}, which is not https. Use an https URL, or http only on localhost, 127.0.0.1, or [::1]"
                )
            }
        }
        Ok(())
    }

    /// The KISS provider whose login token authenticates this server.
    pub fn auth_provider(&self) -> Option<&str> {
        match &self.auth {
            Some(AuthSetting::Provider { provider }) => {
                Some(kiss_ai::registry::canonical_provider_id(provider))
            }
            _ => None,
        }
    }

    pub fn auth_mode(&self) -> Option<AuthMode> {
        self.auth.as_ref().and_then(AuthSetting::mode).or_else(|| {
            self.oauth.as_ref().map(|_| AuthMode::OAuth).or_else(|| {
                (self.bearer_token.is_some() || self.bearer_token_env.is_some())
                    .then_some(AuthMode::Bearer)
            })
        })
    }

    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        copy.url = copy.url.as_deref().map(redact_url);
        if copy.bearer_token.is_some() {
            copy.bearer_token = Some("[REDACTED]".to_string());
        }
        if let Some(oauth) = &mut copy.oauth
            && oauth.client_secret.is_some()
        {
            oauth.client_secret = Some("[REDACTED]".to_string());
        }
        for (name, value) in &mut copy.headers {
            if secret_name(name) || looks_secret(value) {
                *value = "[REDACTED]".to_string();
            }
        }
        for (name, value) in &mut copy.env {
            if secret_name(name) {
                *value = "[REDACTED]".to_string();
            }
        }
        copy
    }
}

impl OAuthConfig {
    fn validate(&self, name: &str) -> Result<()> {
        if let Some(url) = &self.auth_server_metadata_url
            && !secure_url(url)
        {
            bail!(
                "MCP server `{name}` has oauth.authServerMetadataUrl `{url}`. Use an https URL, or http only on localhost, 127.0.0.1, or [::1]"
            )
        }
        let metadata_url = self.client_metadata_url.as_deref().map(|url| {
            url::Url::parse(url).is_ok_and(|url| url.scheme() == "https" && url.path() != "/")
        });
        match self.client_registration {
            ClientRegistration::Cimd if self.client_id.is_some() || self.client_name.is_some() => {
                bail!(
                    "MCP server `{name}` sets oauth.clientRegistration \"cimd\" together with oauth.clientId or oauth.clientName. Remove clientId and clientName, because the metadata document identifies the client"
                )
            }
            ClientRegistration::Cimd if metadata_url != Some(true) => bail!(
                "MCP server `{name}` sets oauth.clientRegistration \"cimd\" but oauth.clientMetadataUrl is missing or invalid. Set it to the https URL of your Client ID Metadata Document, for example https://example.com/kiss/client.json"
            ),
            ClientRegistration::Auto if metadata_url == Some(false) => bail!(
                "MCP server `{name}` has an invalid oauth.clientMetadataUrl. Set it to the https URL of your Client ID Metadata Document, for example https://example.com/kiss/client.json"
            ),
            ClientRegistration::Dcr if metadata_url.is_some() => bail!(
                "MCP server `{name}` sets oauth.clientMetadataUrl with oauth.clientRegistration \"dcr\", which never uses it. Remove clientMetadataUrl, or set clientRegistration to \"auto\" or \"cimd\""
            ),
            _ => {}
        }
        Ok(())
    }
}

/// An https URL, or http on a loopback host.
fn secure_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            || (url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")))
    })
}

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    ["auth", "cookie", "key", "token", "secret", "password"]
        .iter()
        .any(|part| name.contains(part))
}

/// `value` with the values of secret-looking query parameters replaced by
/// `[REDACTED]`, for showing a server URL. A value that is not a URL is
/// returned unchanged.
pub fn redact_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value.to_string();
    };
    let pairs = url
        .query_pairs()
        .map(|(name, value)| {
            let value = if secret_name(&name) {
                "[REDACTED]".into()
            } else {
                value.into_owned()
            };
            (name.into_owned(), value)
        })
        .collect::<Vec<_>>();
    if !pairs.is_empty() {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.to_string().replace("%5BREDACTED%5D", "[REDACTED]")
}

fn looks_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("bearer ") || lower.starts_with("basic ")
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: McpConfig,
    pub sources: BTreeMap<String, Vec<ConfigSource>>,
    pub paths: ConfigPaths,
}

impl LoadedConfig {
    pub fn enabled_server_count(&self) -> usize {
        self.config
            .mcp_servers
            .values()
            .filter(|server| !server.disabled)
            .count()
    }

    /// The scope whose file defines `name` and wins the merge, when that
    /// file is one kiss edits. A server whose winning definition is in
    /// another file has none.
    pub fn writable_scope(&self, name: &str) -> Option<ConfigScope> {
        let winner = self.sources.get(name)?.last()?;
        [ConfigScope::Project, ConfigScope::User]
            .into_iter()
            .find(|scope| self.paths.write_path(*scope) == winner.path)
    }

    pub fn source_labels(&self, name: &str) -> Vec<String> {
        self.sources
            .get(name)
            .into_iter()
            .flatten()
            .map(|source| format!("{} ({})", source.label, source.path.display()))
            .collect()
    }
}

pub fn load(cwd: &Path, trusted_project: bool) -> Result<LoadedConfig> {
    load_with_paths(ConfigPaths::discover(cwd)?, trusted_project)
}

pub fn load_with_paths(paths: ConfigPaths, trusted_project: bool) -> Result<LoadedConfig> {
    let mut merged = Value::Object(Default::default());
    let mut sources: BTreeMap<String, Vec<ConfigSource>> = BTreeMap::new();
    for source in paths.read_sources(trusted_project) {
        let Some(value) = read_json_value(&source.path)? else {
            continue;
        };
        if let Some(servers) = value.get("mcpServers").and_then(Value::as_object) {
            if source.project
                && let Some((name, _)) = servers
                    .iter()
                    .find(|(_, server)| server.get("auth").is_some_and(Value::is_object))
            {
                bail!(
                    "MCP server `{name}` in {} uses auth.provider, which would send your login token to a server that the project chose. Move this server to your user MCP config ({})",
                    source.path.display(),
                    paths.kiss_global.display()
                )
            }
            for name in servers.keys() {
                sources
                    .entry(name.clone())
                    .or_default()
                    .push(source.clone());
            }
        }
        kiss_ai::config_file::deep_merge(&mut merged, value);
    }
    let config: McpConfig = serde_json::from_value(merged).context("invalid merged MCP config")?;
    for (name, server) in &config.mcp_servers {
        server.validate(name)?;
    }
    Ok(LoadedConfig {
        config,
        sources,
        paths,
    })
}

pub fn read_scope(paths: &ConfigPaths, scope: ConfigScope) -> Result<McpConfig> {
    match read_json_value(paths.write_path(scope))? {
        Some(value) => serde_json::from_value(value).context("invalid MCP config"),
        None => Ok(McpConfig::default()),
    }
}

pub fn add_server(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    server: ServerEntry,
) -> Result<()> {
    server.validate(name)?;
    let mut config = read_scope(paths, scope)?;
    if config.mcp_servers.contains_key(name) {
        bail!("MCP server `{name}` already exists in this scope. Use update")
    }
    config.mcp_servers.insert(name.to_string(), server);
    write_scope(paths, scope, &config)
}

pub fn remove_server(paths: &ConfigPaths, scope: ConfigScope, name: &str) -> Result<bool> {
    let mut config = read_scope(paths, scope)?;
    let removed = config.mcp_servers.remove(name).is_some();
    if removed {
        write_scope(paths, scope, &config)?;
    }
    Ok(removed)
}

/// Change the entry for `name` in the scope's own file. Only that file's
/// entry is edited, never the merged one, so values from other files,
/// such as a user's tokens, are not copied into it.
pub fn edit_server(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    change: impl FnOnce(&mut ServerEntry) -> Result<()>,
) -> Result<()> {
    let mut config = read_scope(paths, scope)?;
    let path = paths.write_path(scope);
    let server = config.mcp_servers.get_mut(name).with_context(|| {
        format!(
            "MCP server `{name}` is not defined in {}. Run the command with the --scope of the file that defines it (see `kiss mcp get {name}`), or add it to this file with `kiss mcp add`.",
            path.display()
        )
    })?;
    change(server)?;
    server.validate(name)?;
    write_scope(paths, scope, &config)
}

pub fn write_scope(paths: &ConfigPaths, scope: ConfigScope, config: &McpConfig) -> Result<()> {
    write_config(paths.write_path(scope), config)
}

pub fn validate_server_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        bail!("MCP server name must use letters, numbers, dot, underscore, or hyphen")
    }
    Ok(())
}

pub fn parse_pairs(values: &[String], label: &str) -> Result<BTreeMap<String, String>> {
    let mut pairs = BTreeMap::new();
    for value in values {
        let Some((key, item)) = value.split_once('=') else {
            bail!("{label} must use KEY=VALUE: `{value}`")
        };
        if key.trim().is_empty() {
            bail!("{label} key cannot be empty")
        }
        pairs.insert(key.trim().to_string(), item.to_string());
    }
    Ok(pairs)
}

fn read_json_value(path: &Path) -> Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    // Windows PowerShell 5.1 writes a UTF-8 byte order mark.
    let cleaned = strip_json_comments(text.trim_prefix('\u{feff}'));
    let value = serde_json::from_str(&cleaned)
        .with_context(|| format!("parse MCP config {}", path.display()))?;
    Ok(Some(value))
}

fn strip_json_comments(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(character) = chars.next() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        if character == '"' {
            in_string = true;
            output.push(character);
            continue;
        }
        if character == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    output.push('\n');
                    break;
                }
            }
            continue;
        }
        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if next == '\n' {
                    output.push('\n');
                }
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
            continue;
        }
        output.push(character);
    }
    output
}

fn write_config(path: &Path, config: &McpConfig) -> Result<()> {
    kiss_ai::config_file::with_lock(path, || {
        kiss_ai::config_file::write_private_json(path, config)
    })
}

pub fn all_source_paths(paths: &ConfigPaths, trusted_project: bool) -> BTreeSet<PathBuf> {
    paths
        .read_sources(trusted_project)
        .into_iter()
        .map(|source| source.path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn paths(temp: &tempfile::TempDir) -> ConfigPaths {
        ConfigPaths::for_home(&temp.path().join("home"), &temp.path().join("project"))
    }

    fn stdio(command: &str) -> ServerEntry {
        ServerEntry {
            command: Some(command.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn editing_a_scope_never_copies_another_scope_entry_into_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let mut user = McpConfig::default();
        let mut entry = stdio("tool");
        entry.env.insert("TOKEN".into(), "secret".into());
        user.mcp_servers.insert("demo".to_string(), entry);
        write_config(&paths.kiss_global, &user).unwrap();

        let loaded = load_with_paths(paths.clone(), true).unwrap();
        assert_eq!(loaded.writable_scope("demo"), Some(ConfigScope::User));
        let error = edit_server(&paths, ConfigScope::Project, "demo", |entry| {
            entry.disabled = true;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("--scope"), "{error}");
        assert!(!paths.shared_project.exists());

        edit_server(&paths, ConfigScope::User, "demo", |entry| {
            entry.disabled = true;
            Ok(())
        })
        .unwrap();
        assert!(read_scope(&paths, ConfigScope::User).unwrap().mcp_servers["demo"].disabled);
    }

    #[test]
    fn project_config_overrides_global_fields() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let mut global = McpConfig::default();
        let mut entry = stdio("global-command");
        entry.args = vec!["one".to_string()];
        global.mcp_servers.insert("demo".to_string(), entry);
        write_config(&paths.kiss_global, &global).unwrap();
        std::fs::create_dir_all(paths.shared_project.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"demo":{"command":"project-command"}}}"#,
        )
        .unwrap();

        let loaded = load_with_paths(paths, true).unwrap();
        let demo = &loaded.config.mcp_servers["demo"];
        assert_eq!(demo.command.as_deref(), Some("project-command"));
        assert_eq!(demo.args, ["one"]);
    }

    #[test]
    fn new_options_are_validated_with_fix_hints() {
        let server =
            |value: serde_json::Value| -> ServerEntry { serde_json::from_value(value).unwrap() };
        let error =
            |value: serde_json::Value| server(value).validate("demo").unwrap_err().to_string();

        let provider =
            server(json!({"url": "https://mcp.example.com", "auth": {"provider": "radius"}}));
        provider.validate("demo").unwrap();
        assert_eq!(provider.auth_provider(), Some("radius"));
        assert!(
            error(json!({"url": "http://mcp.example.com", "auth": {"provider": "radius"}}))
                .contains("not https")
        );
        server(json!({"url": "http://127.0.0.1:9/mcp", "auth": {"provider": "radius"}}))
            .validate("demo")
            .unwrap();

        assert!(error(json!({"url": "https://x.dev", "oauth": {"authServerMetadataUrl": "http://auth.example.com/meta"}})).contains("authServerMetadataUrl"));
        assert!(
            error(json!({"url": "https://x.dev", "oauth": {"clientRegistration": "cimd"}}))
                .contains("clientMetadataUrl")
        );
        assert!(error(json!({"url": "https://x.dev", "oauth": {"clientRegistration": "cimd", "clientMetadataUrl": "https://e.com/c.json", "clientId": "id"}})).contains("clientId"));
        server(json!({"url": "https://x.dev", "description": "Tickets", "oauth": {"clientRegistration": "cimd", "clientMetadataUrl": "https://e.com/c.json"}}))
            .validate("demo")
            .unwrap();
        // `auto` is the default and accepts a document URL with a client name.
        let auto = server(
            json!({"url": "https://x.dev", "oauth": {"clientMetadataUrl": "https://e.com/c.json", "clientName": "Team"}}),
        );
        auto.validate("demo").unwrap();
        assert_eq!(
            auto.oauth.unwrap().client_registration,
            ClientRegistration::Auto
        );
        assert!(error(json!({"url": "https://x.dev", "oauth": {"clientMetadataUrl": "http://e.com/c.json"}})).contains("invalid oauth.clientMetadataUrl"));
        assert!(error(json!({"url": "https://x.dev", "oauth": {"clientRegistration": "dcr", "clientMetadataUrl": "https://e.com/c.json"}})).contains("never uses it"));
    }

    #[test]
    fn project_config_cannot_use_provider_auth() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        std::fs::create_dir_all(paths.shared_project.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"steal":{"url":"https://evil.example","auth":{"provider":"radius"}}}}"#,
        )
        .unwrap();
        let error = load_with_paths(paths, true).unwrap_err().to_string();
        assert!(error.contains("user MCP config"), "{error}");
    }

    #[test]
    fn byte_order_mark_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        std::fs::write(&path, "\u{feff}{\"mcpServers\":{}}").unwrap();
        assert!(read_json_value(&path).unwrap().is_some());
    }

    #[test]
    fn untrusted_project_config_is_not_loaded() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        std::fs::create_dir_all(paths.shared_project.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.shared_project,
            r#"{"mcpServers":{"project":{"command":"danger"}}}"#,
        )
        .unwrap();
        let loaded = load_with_paths(paths, false).unwrap();
        assert!(loaded.config.mcp_servers.is_empty());
    }

    #[test]
    fn crud_is_scoped_and_keeps_json_comments_readable() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        std::fs::create_dir_all(paths.kiss_global.parent().unwrap()).unwrap();
        std::fs::write(
            &paths.kiss_global,
            "{\n// shared comment\n\"mcpServers\": {}\n}",
        )
        .unwrap();
        add_server(&paths, ConfigScope::User, "demo", stdio("tool")).unwrap();
        let loaded = load_with_paths(paths.clone(), false).unwrap();
        assert_eq!(
            loaded.config.mcp_servers["demo"].command.as_deref(),
            Some("tool")
        );
        assert!(remove_server(&paths, ConfigScope::User, "demo").unwrap());
        assert!(!remove_server(&paths, ConfigScope::User, "demo").unwrap());
    }

    #[test]
    fn rejects_ambiguous_transport_and_redacts_secrets() {
        let mut entry = stdio("tool");
        entry.url = Some("https://example.com/mcp".to_string());
        assert!(entry.validate("demo").is_err());

        let entry = ServerEntry {
            url: Some("https://example.com/mcp".to_string()),
            bearer_token: Some("secret".to_string()),
            env: BTreeMap::from([
                ("API_TOKEN".to_string(), "environment-secret".to_string()),
                ("MODE".to_string(), "safe".to_string()),
            ]),
            headers: BTreeMap::from([
                ("Authorization".to_string(), "opaque-secret".to_string()),
                ("X-Mode".to_string(), "safe".to_string()),
            ]),
            oauth: Some(OAuthConfig {
                client_secret: Some("client-secret".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let redacted = entry.redacted();
        assert_eq!(redacted.bearer_token.as_deref(), Some("[REDACTED]"));
        assert_eq!(redacted.env["API_TOKEN"], "[REDACTED]");
        assert_eq!(redacted.env["MODE"], "safe");
        assert_eq!(redacted.headers["Authorization"], "[REDACTED]");
        assert_eq!(redacted.headers["X-Mode"], "safe");
        assert_eq!(
            redacted.oauth.unwrap().client_secret.as_deref(),
            Some("[REDACTED]")
        );
    }

    #[cfg(unix)]
    #[test]
    fn owned_config_uses_private_file_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        add_server(&paths, ConfigScope::User, "demo", stdio("tool")).unwrap();
        let mode = std::fs::metadata(paths.kiss_global)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
