//! Lazy MCP server lifecycle and metadata cache.

use crate::config::{AuthMode, LoadedConfig, ServerEntry};
use crate::oauth;
use anyhow::{Context as _, Result, bail};
use rmcp::model::{
    CallToolRequestParams, ClientInfo, GetPromptRequestParams, JsonObject,
    ReadResourceRequestParams,
};
use rmcp::service::RunningService;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const CACHE_VERSION: u32 = 1;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

type Client = RunningService<RoleClient, ClientInfo>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedTool {
    pub server: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedResource {
    pub server: String,
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CachedPrompt {
    pub server: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServerState {
    Disabled,
    NotConnected,
    Cached,
    Connected,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    pub state: ServerState,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CacheFile {
    version: u32,
    servers: BTreeMap<String, ServerCache>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct ServerCache {
    fingerprint: String,
    tools: Vec<CachedTool>,
    resources: Vec<CachedResource>,
    prompts: Vec<CachedPrompt>,
}

struct Runtime {
    client: Option<Client>,
    last_used: Option<Instant>,
    error: Option<String>,
}

impl Runtime {
    fn new() -> Self {
        Self {
            client: None,
            last_used: None,
            error: None,
        }
    }
}

struct Inner {
    config: LoadedConfig,
    runtimes: BTreeMap<String, Arc<Mutex<Runtime>>>,
    cache: Mutex<CacheFile>,
    cache_path: PathBuf,
    credential_path: Option<PathBuf>,
}

#[derive(Clone)]
pub struct McpManager {
    inner: Arc<Inner>,
}

impl McpManager {
    pub fn new(config: LoadedConfig) -> Result<Self> {
        let cache_path = dirs::home_dir()
            .context("no home directory")?
            .join(".kiss/agent/mcp-cache.json");
        Ok(Self::with_paths(config, cache_path, None))
    }

    pub fn with_paths(
        config: LoadedConfig,
        cache_path: PathBuf,
        credential_path: Option<PathBuf>,
    ) -> Self {
        let runtimes = config
            .config
            .mcp_servers
            .keys()
            .map(|name| (name.clone(), Arc::new(Mutex::new(Runtime::new()))))
            .collect();
        let mut cache = load_cache(&cache_path).unwrap_or_default();
        cache.servers.retain(|name, entry| {
            config
                .config
                .mcp_servers
                .get(name)
                .is_some_and(|server| entry.fingerprint == fingerprint(server))
        });
        Self {
            inner: Arc::new(Inner {
                config,
                runtimes,
                cache: Mutex::new(cache),
                cache_path,
                credential_path,
            }),
        }
    }

    pub fn config(&self) -> &LoadedConfig {
        &self.inner.config
    }

    pub async fn status(&self) -> Vec<ServerStatus> {
        let cache = self.inner.cache.lock().await.clone();
        let mut statuses = Vec::new();
        for (name, server) in &self.inner.config.config.mcp_servers {
            let runtime = self.inner.runtimes[name].lock().await;
            let cached = cache.servers.get(name);
            let state = if server.disabled {
                ServerState::Disabled
            } else if runtime.client.is_some() {
                ServerState::Connected
            } else if runtime.error.is_some() {
                ServerState::Failed
            } else if cached.is_some() {
                ServerState::Cached
            } else {
                ServerState::NotConnected
            };
            statuses.push(ServerStatus {
                name: name.clone(),
                state,
                tools: cached.map_or(0, |item| item.tools.len()),
                resources: cached.map_or(0, |item| item.resources.len()),
                prompts: cached.map_or(0, |item| item.prompts.len()),
                error: runtime.error.clone(),
            });
        }
        statuses
    }

    pub async fn cached_tools(&self) -> Vec<CachedTool> {
        self.inner
            .cache
            .lock()
            .await
            .servers
            .values()
            .flat_map(|server| server.tools.clone())
            .collect()
    }

    pub async fn list_tools(
        &self,
        server_name: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let names = self.server_names(server_name)?;
        let mut tools = Vec::new();
        for name in names {
            tools.extend(self.refresh_tools(&name, cancel).await?);
        }
        Ok(tools)
    }

    pub async fn search_tools(
        &self,
        query: &str,
        server_name: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let names = self.server_names(server_name)?;
        let mut tools = Vec::new();
        let mut missing = Vec::new();
        {
            let cache = self.inner.cache.lock().await;
            for name in names {
                if let Some(server) = cache.servers.get(&name) {
                    tools.extend(server.tools.clone());
                } else {
                    missing.push(name);
                }
            }
        }
        for name in missing {
            tools.extend(self.refresh_tools(&name, cancel).await?);
        }
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .collect();
        tools.retain(|tool| {
            let haystack = format!(
                "{} {} {} {}",
                tool.server,
                tool.name,
                tool.title.as_deref().unwrap_or(""),
                tool.description.as_deref().unwrap_or("")
            )
            .to_ascii_lowercase();
            terms.iter().all(|term| haystack.contains(term))
        });
        tools.sort_by_key(|tool| {
            let name_match = tool.name.eq_ignore_ascii_case(query);
            (!name_match, tool.server.clone(), tool.name.clone())
        });
        Ok(tools)
    }

    pub async fn describe_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        cancel: &CancellationToken,
    ) -> Result<CachedTool> {
        if let Some(found) = self
            .inner
            .cache
            .lock()
            .await
            .servers
            .get(server_name)
            .and_then(|server| server.tools.iter().find(|tool| tool.name == tool_name))
            .cloned()
        {
            return Ok(found);
        }
        self.refresh_tools(server_name, cancel)
            .await?
            .into_iter()
            .find(|tool| tool.name == tool_name)
            .with_context(|| format!("MCP tool `{server_name}/{tool_name}` was not found"))
    }

    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<rmcp::model::CallToolResult> {
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => JsonObject::new(),
            _ => bail!("MCP tool arguments must be a JSON object"),
        };
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let request = client
            .peer()
            .call_tool(CallToolRequestParams::new(tool_name.to_string()).with_arguments(arguments));
        let result =
            wait_request(request, self.timeout(server_name), cancel, "MCP tool call").await?;
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.schedule_idle_disconnect(server_name);
        Ok(result)
    }

    pub async fn list_resources(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedResource>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let listed = wait_request(
            client.peer().list_all_resources(),
            self.timeout(server_name),
            cancel,
            "MCP resource list",
        )
        .await?;
        let resources = listed
            .into_iter()
            .map(|resource| CachedResource {
                server: server_name.to_string(),
                uri: resource.uri,
                name: resource.name,
                description: resource.description,
                mime_type: resource.mime_type,
            })
            .collect::<Vec<_>>();
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.update_cache(server_name, |cache| cache.resources = resources.clone())
            .await;
        self.schedule_idle_disconnect(server_name);
        Ok(resources)
    }

    pub async fn read_resource(
        &self,
        server_name: &str,
        uri: &str,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let result = wait_request(
            client
                .peer()
                .read_resource(ReadResourceRequestParams::new(uri)),
            self.timeout(server_name),
            cancel,
            "MCP resource read",
        )
        .await?;
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.schedule_idle_disconnect(server_name);
        Ok(serde_json::to_value(result)?)
    }

    pub async fn list_prompts(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedPrompt>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let listed = wait_request(
            client.peer().list_all_prompts(),
            self.timeout(server_name),
            cancel,
            "MCP prompt list",
        )
        .await?;
        let prompts = listed
            .into_iter()
            .map(|prompt| CachedPrompt {
                server: server_name.to_string(),
                name: prompt.name,
                title: prompt.title,
                description: prompt.description,
                arguments: serde_json::to_value(prompt.arguments).unwrap_or(Value::Null),
            })
            .collect::<Vec<_>>();
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.update_cache(server_name, |cache| cache.prompts = prompts.clone())
            .await;
        self.schedule_idle_disconnect(server_name);
        Ok(prompts)
    }

    pub async fn get_prompt(
        &self,
        server_name: &str,
        prompt_name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            Value::Null => JsonObject::new(),
            _ => bail!("MCP prompt arguments must be a JSON object"),
        };
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let result = wait_request(
            client
                .peer()
                .get_prompt(GetPromptRequestParams::new(prompt_name).with_arguments(arguments)),
            self.timeout(server_name),
            cancel,
            "MCP prompt request",
        )
        .await?;
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.schedule_idle_disconnect(server_name);
        Ok(serde_json::to_value(result)?)
    }

    pub async fn disconnect(&self, server_name: &str) -> Result<bool> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        let Some(client) = runtime.client.take() else {
            return Ok(false);
        };
        let _ = client.cancel().await;
        runtime.last_used = None;
        Ok(true)
    }

    pub async fn disconnect_all(&self) {
        for name in self.inner.runtimes.keys() {
            let _ = self.disconnect(name).await;
        }
    }

    async fn refresh_tools(
        &self,
        server_name: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<CachedTool>> {
        let runtime = self.runtime(server_name)?;
        let mut runtime = runtime.lock().await;
        self.ensure_connected(server_name, &mut runtime).await?;
        let client = runtime
            .client
            .as_ref()
            .context("MCP client is not connected")?;
        let listed = wait_request(
            client.peer().list_all_tools(),
            self.timeout(server_name),
            cancel,
            "MCP tool list",
        )
        .await?;
        let server = &self.inner.config.config.mcp_servers[server_name];
        let tools = listed
            .into_iter()
            .filter(|tool| tool_is_enabled(server, tool.name.as_ref()))
            .map(|tool| CachedTool {
                server: server_name.to_string(),
                name: tool.name.into_owned(),
                title: tool.title,
                description: tool.description.map(|value| value.into_owned()),
                input_schema: Value::Object((*tool.input_schema).clone()),
            })
            .collect::<Vec<_>>();
        runtime.last_used = Some(Instant::now());
        drop(runtime);
        self.update_cache(server_name, |cache| cache.tools = tools.clone())
            .await;
        self.schedule_idle_disconnect(server_name);
        Ok(tools)
    }

    async fn update_cache(&self, server_name: &str, update: impl FnOnce(&mut ServerCache)) {
        let mut cache = self.inner.cache.lock().await;
        let server = &self.inner.config.config.mcp_servers[server_name];
        let entry = cache
            .servers
            .entry(server_name.to_string())
            .or_insert_with(|| ServerCache {
                fingerprint: fingerprint(server),
                ..Default::default()
            });
        update(entry);
        cache.version = CACHE_VERSION;
        let _ = save_cache(&self.inner.cache_path, &cache);
    }

    async fn ensure_connected(&self, server_name: &str, runtime: &mut Runtime) -> Result<()> {
        let server = self
            .inner
            .config
            .config
            .mcp_servers
            .get(server_name)
            .with_context(|| format!("MCP server `{server_name}` is not configured"))?;
        if server.disabled {
            bail!("MCP server `{server_name}` is disabled")
        }
        if runtime.client.is_some()
            && runtime.last_used.is_some_and(|last| {
                last.elapsed() >= Duration::from_secs(server.idle_timeout.unwrap_or(10) * 60)
            })
            && let Some(client) = runtime.client.take()
        {
            let _ = client.cancel().await;
        }
        if runtime.client.is_some() {
            return Ok(());
        }
        let result = connect(server_name, server, self.inner.credential_path.clone()).await;
        match result {
            Ok(client) => {
                runtime.client = Some(client);
                runtime.last_used = Some(Instant::now());
                runtime.error = None;
                Ok(())
            }
            Err(error) => {
                runtime.error = Some(error.to_string());
                Err(error)
            }
        }
    }

    fn runtime(&self, server_name: &str) -> Result<Arc<Mutex<Runtime>>> {
        self.inner
            .runtimes
            .get(server_name)
            .cloned()
            .with_context(|| format!("MCP server `{server_name}` is not configured"))
    }

    fn server_names(&self, one: Option<&str>) -> Result<Vec<String>> {
        if let Some(name) = one {
            let server = self
                .inner
                .config
                .config
                .mcp_servers
                .get(name)
                .with_context(|| format!("MCP server `{name}` is not configured"))?;
            if server.disabled {
                bail!("MCP server `{name}` is disabled")
            }
            return Ok(vec![name.to_string()]);
        }
        Ok(self
            .inner
            .config
            .config
            .mcp_servers
            .iter()
            .filter(|(_, server)| !server.disabled)
            .map(|(name, _)| name.clone())
            .collect())
    }

    fn timeout(&self, server_name: &str) -> Duration {
        self.inner.config.config.mcp_servers[server_name]
            .request_timeout_ms
            .map(Duration::from_millis)
            .filter(|duration| !duration.is_zero())
            .unwrap_or(DEFAULT_TIMEOUT)
    }

    fn schedule_idle_disconnect(&self, server_name: &str) {
        let server = &self.inner.config.config.mcp_servers[server_name];
        if server.lifecycle.as_deref() == Some("persistent") {
            return;
        }
        let delay = Duration::from_secs(server.idle_timeout.unwrap_or(10).saturating_mul(60));
        let manager = self.clone();
        let name = server_name.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Ok(runtime) = manager.runtime(&name) else {
                return;
            };
            let mut runtime = runtime.lock().await;
            if runtime
                .last_used
                .is_some_and(|last_used| last_used.elapsed() >= delay)
                && let Some(client) = runtime.client.take()
            {
                let _ = client.cancel().await;
                runtime.last_used = None;
            }
        });
    }
}

async fn connect(
    server_name: &str,
    server: &ServerEntry,
    credential_path: Option<PathBuf>,
) -> Result<Client> {
    if let Some(command) = &server.command {
        let mut process = Command::new(expand_env(command));
        process.args(server.args.iter().map(|value| expand_env(value)));
        process.envs(
            server
                .env
                .iter()
                .map(|(key, value)| (key, expand_env(value))),
        );
        if let Some(cwd) = &server.cwd {
            process.current_dir(expand_home(&expand_env(cwd)));
        }
        let transport = TokioChildProcess::new(process)
            .with_context(|| format!("start MCP server `{server_name}`"))?;
        return Ok(ClientInfo::default().serve(transport).await?);
    }

    crate::ensure_tls_crypto_provider();
    let url = expand_env(server.url.as_deref().context("MCP URL is missing")?);
    let transport_config = http_transport_config(server, &url)?;
    if server.auth_mode() == Some(AuthMode::OAuth) {
        let oauth_config = server.oauth.clone().unwrap_or_default();
        let mut manager =
            oauth::new_manager(server_name, &url, &oauth_config, credential_path).await?;
        if !manager.initialize_from_store().await? {
            bail!(
                "MCP server `{server_name}` needs OAuth login; run `kiss mcp login {server_name}`"
            )
        }
        let client = AuthClient::new(reqwest_mcp::Client::default(), manager);
        let transport = StreamableHttpClientTransport::with_client(client, transport_config);
        return Ok(ClientInfo::default().serve(transport).await?);
    }

    let transport = StreamableHttpClientTransport::with_client(
        reqwest_mcp::Client::default(),
        transport_config,
    );
    Ok(ClientInfo::default().serve(transport).await?)
}

pub async fn probe_oauth_challenge(server: &ServerEntry) -> Result<Option<String>> {
    crate::ensure_tls_crypto_provider();
    let url = expand_env(server.url.as_deref().context("MCP URL is missing")?);
    let transport = StreamableHttpClientTransport::with_client(
        reqwest_mcp::Client::default(),
        http_transport_config(server, &url)?,
    );
    match ClientInfo::default().serve(transport).await {
        Ok(client) => {
            let _ = client.cancel().await;
            Ok(None)
        }
        Err(error) => match error.auth_challenge() {
            Some(challenge) => Ok(Some(challenge.to_string())),
            None => Err(error.into()),
        },
    }
}

fn http_transport_config(
    server: &ServerEntry,
    url: &str,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut headers = HashMap::new();
    for (name, value) in &server.headers {
        headers.insert(
            http::HeaderName::try_from(name)?,
            http::HeaderValue::try_from(expand_env(value))?,
        );
    }
    let mut config = StreamableHttpClientTransportConfig::with_uri(url)
        .custom_headers(headers)
        .reinit_on_expired_session(true);
    if server.auth_mode() == Some(AuthMode::Bearer) {
        let token = server
            .bearer_token
            .as_ref()
            .map(|value| expand_env(value))
            .or_else(|| {
                server
                    .bearer_token_env
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok())
            })
            .context("MCP bearer token is not configured")?;
        config = config.auth_header(token);
    }
    Ok(config)
}

async fn wait_request<F, T, E>(
    future: F,
    timeout: Duration,
    cancel: &CancellationToken,
    label: &str,
) -> Result<T>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    tokio::select! {
        _ = cancel.cancelled() => bail!("{label} cancelled"),
        result = tokio::time::timeout(timeout, future) => {
            result
                .with_context(|| format!("{label} timed out after {} ms", timeout.as_millis()))?
                .map_err(anyhow::Error::new)
        }
    }
}

fn tool_is_enabled(server: &ServerEntry, name: &str) -> bool {
    (server.include_tools.is_empty() || server.include_tools.iter().any(|item| item == name))
        && !server.exclude_tools.iter().any(|item| item == name)
}

fn expand_env(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let Some(end) = tail.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };
        let name = &tail[..end];
        output.push_str(&std::env::var(name).unwrap_or_default());
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    output
}

fn expand_home(value: &str) -> PathBuf {
    value
        .strip_prefix("~/")
        .and_then(|rest| dirs::home_dir().map(|home| home.join(rest)))
        .unwrap_or_else(|| PathBuf::from(value))
}

fn fingerprint(server: &ServerEntry) -> String {
    let encoded = serde_json::to_vec(server).unwrap_or_default();
    format!("{:x}", Sha256::digest(encoded))
}

fn load_cache(path: &Path) -> Result<CacheFile> {
    let text = std::fs::read_to_string(path)?;
    let cache: CacheFile = serde_json::from_str(&text)?;
    if cache.version != CACHE_VERSION {
        return Ok(CacheFile::default());
    }
    Ok(cache)
}

fn save_cache(path: &Path, cache: &CacheFile) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut output = options.open(&temporary)?;
    serde_json::to_writer_pretty(&mut output, cache)?;
    use std::io::Write as _;
    output.write_all(b"\n")?;
    output.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigPaths, McpConfig};
    use kiss_agent::AgentTool as _;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    fn loaded(temp: &tempfile::TempDir, server: ServerEntry) -> LoadedConfig {
        let project = temp.path().join("project");
        let paths = ConfigPaths::for_home(&temp.path().join("home"), &project);
        let mut config = McpConfig::default();
        config.mcp_servers.insert("demo".to_string(), server);
        LoadedConfig {
            config,
            sources: BTreeMap::new(),
            paths,
        }
    }

    #[tokio::test]
    async fn construction_is_lazy() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("started");
        let server = ServerEntry {
            command: Some("sh".to_string()),
            args: vec![
                "-c".to_string(),
                format!("touch {}; exit 1", marker.display()),
            ],
            ..Default::default()
        };
        let manager = McpManager::with_paths(
            loaded(&temp, server),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        let status = manager.status().await;
        assert_eq!(status.len(), 1);
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_fixture_lists_and_calls_a_tool() {
        let temp = tempfile::tempdir().unwrap();
        let script = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"stdio-test","version":"1.0.0"}}}\n' "$id"
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"Echo text","inputSchema":{"type":"object"}}]}}\n' "$id"
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello from stdio"}],"isError":false}}\n' "$id"
      ;;
  esac
done
"#;
        let manager = McpManager::with_paths(
            loaded(
                &temp,
                ServerEntry {
                    command: Some("sh".to_string()),
                    args: vec!["-c".to_string(), script.to_string()],
                    ..Default::default()
                },
            ),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        let cancel = CancellationToken::new();
        let tools = manager.list_tools(Some("demo"), &cancel).await.unwrap();
        assert_eq!(tools[0].name, "echo");
        let result = manager
            .call_tool("demo", "echo", Value::Null, &cancel)
            .await
            .unwrap();
        assert_eq!(
            result.content[0].as_text().unwrap().text,
            "hello from stdio"
        );
        manager.disconnect_all().await;
    }

    async fn start_http_fixture() -> (String, Arc<AtomicUsize>, CancellationToken) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = requests.clone();
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = server_cancel.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else { break };
                request_count.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 4096];
                    let header_end = loop {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(position) =
                            request.windows(4).position(|item| item == b"\r\n\r\n")
                        {
                            break position + 4;
                        }
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .or_else(|| {
                            headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while request.len() < header_end + length {
                        let count = socket.read(&mut buffer).await.unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..count]);
                    }
                    let value: Value =
                        serde_json::from_slice(&request[header_end..header_end + length])
                            .unwrap_or(Value::Null);
                    let Some(id) = value.get("id").cloned() else {
                        let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    };
                    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
                    let result = match method {
                        "initialize" => json!({
                            "protocolVersion": value.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-11-25")),
                            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                            "serverInfo": {"name": "kiss-test", "version": "1.0.0"}
                        }),
                        "tools/list" => json!({"tools": [{
                            "name": "echo",
                            "description": "Echo a value",
                            "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}}
                        }]}),
                        "tools/call" => {
                            if value.pointer("/params/name").and_then(Value::as_str) == Some("slow")
                            {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                            }
                            json!({"content": [{"type": "text", "text": "hello from MCP"}], "isError": false})
                        }
                        "resources/list" => {
                            json!({"resources": [{"uri": "test://note", "name": "note", "mimeType": "text/plain"}]})
                        }
                        "resources/read" => {
                            json!({"contents": [{"uri": "test://note", "mimeType": "text/plain", "text": "fixture resource"}]})
                        }
                        "prompts/list" => {
                            json!({"prompts": [{"name": "review", "description": "Review text"}]})
                        }
                        "prompts/get" => {
                            json!({"description": "Review text", "messages": [{"role": "user", "content": {"type": "text", "text": "review this"}}]})
                        }
                        _ => json!({}),
                    };
                    let body =
                        serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
                            .unwrap();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                });
            }
        });
        (format!("http://{address}/mcp"), requests, cancel)
    }

    #[tokio::test]
    async fn http_fixture_supports_proxy_tools_resources_and_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let (url, requests, fixture_cancel) = start_http_fixture().await;
        let manager = McpManager::with_paths(
            loaded(
                &temp,
                ServerEntry {
                    url: Some(url),
                    ..Default::default()
                },
            ),
            temp.path().join("cache.json"),
            Some(temp.path().join("oauth.json")),
        );
        assert_eq!(requests.load(Ordering::Relaxed), 0);
        assert_eq!(manager.status().await.len(), 1);
        assert_eq!(requests.load(Ordering::Relaxed), 0);

        let cancel = CancellationToken::new();
        let tools = manager.list_tools(Some("demo"), &cancel).await.unwrap();
        assert_eq!(tools[0].name, "echo");
        let called = manager
            .call_tool("demo", "echo", json!({"value": "hello"}), &cancel)
            .await
            .unwrap();
        assert_eq!(called.content[0].as_text().unwrap().text, "hello from MCP");
        assert_eq!(
            manager.list_resources("demo", &cancel).await.unwrap()[0].name,
            "note"
        );
        let resource = manager
            .read_resource("demo", "test://note", &cancel)
            .await
            .unwrap();
        assert!(resource.to_string().contains("fixture resource"));
        assert_eq!(
            manager.list_prompts("demo", &cancel).await.unwrap()[0].name,
            "review"
        );
        let prompt = manager
            .get_prompt("demo", "review", Value::Null, &cancel)
            .await
            .unwrap();
        assert!(prompt.to_string().contains("review this"));

        let proxy = crate::tool::McpTool::new(manager.clone());
        let proxy_result = proxy
            .execute(
                "call-1",
                json!({"action": "call", "server": "demo", "name": "echo", "arguments": {"value": "hello"}}),
                cancel,
                None,
            )
            .await
            .unwrap();
        assert_eq!(proxy_result.output_text(), "hello from MCP");
        let slow_cancel = CancellationToken::new();
        let trigger = slow_cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let error = manager
            .call_tool("demo", "slow", Value::Null, &slow_cancel)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(requests.load(Ordering::Relaxed) > 0);
        manager.disconnect_all().await;
        fixture_cancel.cancel();
    }

    #[test]
    fn environment_and_home_expansion_are_explicit() {
        unsafe { std::env::set_var("KISS_MCP_TEST_VALUE", "value") };
        assert_eq!(expand_env("a-${KISS_MCP_TEST_VALUE}-b"), "a-value-b");
        assert!(expand_home("~/work").ends_with("work"));
    }
}
