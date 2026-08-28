//! MCP configuration discovery, merge, validation, and safe writes.

use anyhow::{Context as _, Result, bail};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
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
            kiss_global: home.join(".kiss/agent/mcp.json"),
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
}

impl AuthSetting {
    pub fn mode(&self) -> Option<AuthMode> {
        match self {
            Self::Mode(mode) => Some(*mode),
            Self::Disabled(_) => None,
        }
    }
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
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerEntry {
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
            bail!("MCP server `{name}` uses invalid auth value true; use false to disable auth")
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
        Ok(())
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

fn secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "key",
        "token",
        "secret",
        "password",
    ]
    .iter()
    .any(|part| name.contains(part))
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
            for name in servers.keys() {
                sources
                    .entry(name.clone())
                    .or_default()
                    .push(source.clone());
            }
        }
        deep_merge(&mut merged, value);
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
        bail!("MCP server `{name}` already exists in this scope; use update")
    }
    config.mcp_servers.insert(name.to_string(), server);
    write_scope(paths, scope, &config)
}

pub fn put_server(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    server: ServerEntry,
) -> Result<()> {
    server.validate(name)?;
    let mut config = read_scope(paths, scope)?;
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

pub fn set_disabled(
    paths: &ConfigPaths,
    scope: ConfigScope,
    name: &str,
    disabled: bool,
    inherited: Option<&ServerEntry>,
) -> Result<()> {
    let mut config = read_scope(paths, scope)?;
    let server = config
        .mcp_servers
        .entry(name.to_string())
        .or_insert_with(|| inherited.cloned().unwrap_or_default());
    server.disabled = disabled;
    server.validate(name)?;
    write_scope(paths, scope, &config)
}

pub fn write_scope(paths: &ConfigPaths, scope: ConfigScope, config: &McpConfig) -> Result<()> {
    write_json_atomic(paths.write_path(scope), config)
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
    let cleaned = strip_json_comments(&text);
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

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, value) => *slot = value,
    }
}

fn write_json_atomic(path: &Path, config: &McpConfig) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("MCP config path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let lock_path = path.with_extension("json.lock");
    let lock = secure_open(&lock_path)?;
    lock.lock_exclusive()
        .with_context(|| format!("lock {}", lock_path.display()))?;

    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = secure_create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, config)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    let _ = fs2::FileExt::unlock(&lock);
    result
}

fn secure_open(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn secure_create(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create {}", path.display()))
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
    fn project_config_overrides_global_fields() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(&temp);
        let mut global = McpConfig::default();
        let mut entry = stdio("global-command");
        entry.args = vec!["one".to_string()];
        global.mcp_servers.insert("demo".to_string(), entry);
        write_json_atomic(&paths.kiss_global, &global).unwrap();
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
