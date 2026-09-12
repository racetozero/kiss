use futures::{StreamExt as _, stream};
use kiss_ai::{Model, Registry};
use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal as _;
use std::time::{Duration, Instant};
use url::Url;

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_CONCURRENCY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Connection {
    Sse,
    WebSocket,
    EventStream,
    Login,
}

impl Connection {
    fn label(self) -> &'static str {
        match self {
            Self::Sse => "SSE",
            Self::WebSocket => "WebSocket",
            Self::EventStream => "event stream",
            Self::Login => "login",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Info,
    Skip,
    Fail,
}

impl Status {
    fn label(self, color: bool) -> String {
        let (text, code) = match self {
            Self::Ok => ("✓ OK", "32"),
            Self::Info => ("○ INFO", "36"),
            Self::Skip => ("○ SKIP", "33"),
            Self::Fail => ("✗ FAIL", "31"),
        };
        let padded = format!("{text:<6}");
        if color {
            format!("\x1b[{code}m{padded}\x1b[0m")
        } else {
            padded
        }
    }
}

#[derive(Debug, Clone)]
struct Target {
    provider: String,
    connection: Connection,
    host: String,
    url: Option<Url>,
    skip_reason: Option<String>,
}

#[derive(Debug)]
struct CheckResult {
    target: Target,
    status: Status,
    detail: String,
}

#[derive(Debug)]
struct LocalCheck {
    name: &'static str,
    status: Status,
    detail: String,
}

pub async fn run(summary: bool) -> anyhow::Result<i32> {
    let registry = Registry::load(None);
    let local = local_checks(&registry);
    let targets = provider_targets(&registry);
    let mut results = stream::iter(targets.into_iter().map(probe))
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect::<Vec<_>>()
        .await;
    results.sort_by(|left, right| {
        (
            &left.target.provider,
            left.target.connection,
            &left.target.host,
        )
            .cmp(&(
                &right.target.provider,
                right.target.connection,
                &right.target.host,
            ))
    });

    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    println!(
        "KISS Doctor v{} · {}-{}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!("\nEnvironment");
    let name_width = local
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or_default();
    for check in &local {
        println!(
            "  {}  {:name_width$}  {}",
            check.status.label(color),
            check.name,
            check.detail
        );
    }

    println!("\nConnectivity");
    if summary {
        print_summary(&results, color);
    } else {
        print_details(&results, color);
    }

    let ok = results
        .iter()
        .filter(|result| result.status == Status::Ok)
        .count();
    let skipped = results
        .iter()
        .filter(|result| result.status == Status::Skip)
        .count();
    let failed = results
        .iter()
        .filter(|result| result.status == Status::Fail)
        .count();
    println!("\n{ok} reachable · {skipped} skipped · {failed} failed");
    if summary {
        println!("Run `kiss doctor` to show each host and connection type.");
    }

    let local_failed = local.iter().any(|check| check.status == Status::Fail);
    Ok(i32::from(local_failed || failed > 0))
}

fn local_checks(registry: &Registry) -> Vec<LocalCheck> {
    let install = match std::env::current_exe() {
        Ok(path) => LocalCheck {
            name: "install",
            status: Status::Ok,
            detail: path.display().to_string(),
        },
        Err(error) => LocalCheck {
            name: "install",
            status: Status::Fail,
            detail: error.to_string(),
        },
    };

    let providers = registry
        .all()
        .iter()
        .map(|model| model.provider.as_str())
        .chain(std::iter::once("radius"))
        .collect::<BTreeSet<_>>();
    let catalog = match kiss_ai::provider_config::list() {
        Ok(custom) => LocalCheck {
            name: "catalog",
            status: Status::Ok,
            detail: format!(
                "{} models · {} providers · {} custom",
                registry.all().len(),
                providers.len(),
                custom.len()
            ),
        },
        Err(error) => LocalCheck {
            name: "catalog",
            status: Status::Fail,
            detail: error.to_string(),
        },
    };
    let configured = providers
        .iter()
        .filter(|provider| {
            let credential_provider = registry.credential_provider(provider);
            kiss_ai::auth::resolve_credential_local(credential_provider, &registry.declared_keys)
                .is_some()
        })
        .count();
    let auth = LocalCheck {
        name: "auth",
        status: Status::Info,
        detail: format!(
            "{configured} of {} providers have local credentials",
            providers.len()
        ),
    };

    const PROXY_NAMES: [&str; 8] = [
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "all_proxy",
        "no_proxy",
    ];
    let proxy_names = PROXY_NAMES
        .into_iter()
        .filter(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
        .collect::<Vec<_>>();
    let proxy = LocalCheck {
        name: "proxy",
        status: Status::Ok,
        detail: if proxy_names.is_empty() {
            "no proxy environment variables".into()
        } else {
            format!("set: {}", proxy_names.join(", "))
        },
    };

    vec![install, catalog, auth, proxy]
}

fn provider_targets(registry: &Registry) -> Vec<Target> {
    let mut targets = BTreeMap::new();
    for model in registry.all() {
        let endpoint = model_endpoint(registry, model);
        let connection = if model.api == "bedrock-converse-stream" {
            Connection::EventStream
        } else {
            Connection::Sse
        };
        insert_target(
            &mut targets,
            endpoint_target(model, connection, endpoint.clone()),
        );
        if supports_websocket(model) {
            insert_target(
                &mut targets,
                endpoint_target(model, Connection::WebSocket, endpoint),
            );
        }
    }

    let radius = kiss_ai::auth::radius::OAuthConfig::default().gateway;
    insert_url(
        &mut targets,
        "radius",
        Connection::Sse,
        format!("{}/v1/messages", radius.trim_end_matches('/')),
    );

    add_login_targets(&mut targets);
    targets.into_values().collect()
}

fn model_endpoint(registry: &Registry, model: &Model) -> Result<String, String> {
    let mut model = model.clone();
    let credential_provider = registry.credential_provider(&model.provider);
    if let Some(credential) =
        kiss_ai::auth::resolve_credential_local(credential_provider, &registry.declared_keys)
    {
        model.base_url = kiss_ai::api::provider_base_url(&model, credential.value());
    }
    if model.base_url.contains("{location}") {
        return Err("set GOOGLE_CLOUD_LOCATION".into());
    }
    if model.base_url.trim().is_empty() && model.api != "azure-openai-responses" {
        return Err("set the provider base URL".into());
    }

    match model.api.as_str() {
        "anthropic-messages" => Ok(format!(
            "{}/v1/messages",
            model.base_url.trim_end_matches('/')
        )),
        "openai-completions" => Ok(format!(
            "{}/chat/completions",
            model.base_url.trim_end_matches('/')
        )),
        "openai-responses" | "openai-codex-responses" | "azure-openai-responses" => {
            kiss_ai::api::openai_responses::response_url(&model).map_err(|error| error.to_string())
        }
        "google-generative-ai" | "google-vertex" | "bedrock-converse-stream" => Ok(model.base_url),
        "pi-messages" => Ok(format!("{}/messages", model.base_url.trim_end_matches('/'))),
        other => Err(format!("unsupported API type {other}")),
    }
}

fn supports_websocket(model: &Model) -> bool {
    model.api == "openai-codex-responses"
        || model.api == "azure-openai-responses"
        || (model.provider == "openai" && model.api == "openai-responses")
}

fn endpoint_target(
    model: &Model,
    connection: Connection,
    endpoint: Result<String, String>,
) -> Target {
    match endpoint {
        Ok(url) => target_from_url(&model.provider, connection, &url),
        Err(reason) => Target {
            provider: model.provider.clone(),
            connection,
            host: unresolved_host(model),
            url: None,
            skip_reason: Some(reason),
        },
    }
}

fn target_from_url(provider: &str, connection: Connection, value: &str) -> Target {
    let parsed = Url::parse(value).and_then(sanitize_url);
    match parsed {
        Ok(url) => Target {
            provider: provider.into(),
            connection,
            host: authority(&url),
            url: Some(url),
            skip_reason: None,
        },
        Err(error) => Target {
            provider: provider.into(),
            connection,
            host: raw_authority(value),
            url: None,
            skip_reason: Some(format!("invalid endpoint: {error}")),
        },
    }
}

fn sanitize_url(mut url: Url) -> Result<Url, url::ParseError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(url::ParseError::RelativeUrlWithoutBase);
    }
    url.set_username("")
        .map_err(|_| url::ParseError::InvalidDomainCharacter)?;
    url.set_password(None)
        .map_err(|_| url::ParseError::InvalidDomainCharacter)?;
    url.set_query(None);
    url.set_fragment(None);
    if url.host_str().is_none() {
        return Err(url::ParseError::EmptyHost);
    }
    Ok(url)
}

fn authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or("<missing host>");
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("{host}:{}", url.port_or_known_default().unwrap_or(0))
}

fn raw_authority(value: &str) -> String {
    let scheme_end = value.find("://").map_or(0, |index| index + 3);
    let rest = &value[scheme_end..];
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    if host.contains(':') {
        host.to_string()
    } else if value.starts_with("http://") {
        format!("{host}:80")
    } else {
        format!("{host}:443")
    }
}

fn unresolved_host(model: &Model) -> String {
    if model.provider == "azure-openai-responses" {
        "<AZURE_OPENAI_BASE_URL>:443".into()
    } else {
        raw_authority(&model.base_url)
    }
}

fn add_login_targets(targets: &mut BTreeMap<(String, Connection, String), Target>) {
    let openai = kiss_ai::auth::openai_codex::OAuthConfig::default();
    insert_url(
        targets,
        "openai-codex",
        Connection::Login,
        openai.auth_base_url,
    );

    let anthropic = kiss_ai::auth::anthropic::OAuthConfig::default();
    insert_url(
        targets,
        "anthropic",
        Connection::Login,
        anthropic.authorize_url,
    );
    insert_url(targets, "anthropic", Connection::Login, anthropic.token_url);

    let openrouter = kiss_ai::auth::openrouter::OAuthConfig::default();
    insert_url(
        targets,
        "openrouter",
        Connection::Login,
        openrouter.authorize_url,
    );
    insert_url(
        targets,
        "openrouter",
        Connection::Login,
        openrouter.token_url,
    );

    insert_url(
        targets,
        "github-copilot",
        Connection::Login,
        "https://github.com/login/device/code".into(),
    );
    insert_url(
        targets,
        "github-copilot",
        Connection::Login,
        "https://api.github.com/copilot_internal/v2/token".into(),
    );

    let kimi = kiss_ai::auth::kimi_coding::OAuthConfig::default();
    insert_url(targets, "kimi-coding", Connection::Login, kimi.oauth_host);

    let xai = kiss_ai::auth::xai::OAuthConfig::default();
    insert_url(targets, "xai", Connection::Login, xai.device_url);
    insert_url(targets, "xai", Connection::Login, xai.token_url);

    let radius = kiss_ai::auth::radius::OAuthConfig::default();
    insert_url(targets, "radius", Connection::Login, radius.gateway);
}

fn insert_url(
    targets: &mut BTreeMap<(String, Connection, String), Target>,
    provider: &str,
    connection: Connection,
    url: String,
) {
    insert_target(targets, target_from_url(provider, connection, &url));
}

fn insert_target(targets: &mut BTreeMap<(String, Connection, String), Target>, target: Target) {
    let key = (
        target.provider.clone(),
        target.connection,
        target.host.clone(),
    );
    match targets.get(&key) {
        Some(existing) if existing.url.is_some() || target.url.is_none() => {}
        _ => {
            targets.insert(key, target);
        }
    }
}

async fn probe(target: Target) -> CheckResult {
    let Some(url) = target.url.clone() else {
        return CheckResult {
            detail: target
                .skip_reason
                .clone()
                .unwrap_or_else(|| "missing configuration".into()),
            target,
            status: Status::Skip,
        };
    };
    let started = Instant::now();
    let client = kiss_ai::stream::http_client();
    let request = if target.connection == Connection::WebSocket {
        client
            .get(url)
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "a2lzcy1kb2N0b3ItcHJvYmU=")
    } else {
        client.head(url)
    }
    .header(
        "user-agent",
        concat!("kiss/", env!("CARGO_PKG_VERSION"), " doctor"),
    );

    match tokio::time::timeout(PROBE_TIMEOUT, request.send()).await {
        Ok(Ok(response)) => CheckResult {
            detail: format!(
                "HTTP {} · {} ms",
                response.status().as_u16(),
                started.elapsed().as_millis()
            ),
            target,
            status: Status::Ok,
        },
        Ok(Err(error)) => CheckResult {
            detail: error.to_string(),
            target,
            status: Status::Fail,
        },
        Err(_) => CheckResult {
            detail: format!("timed out after {} s", PROBE_TIMEOUT.as_secs()),
            target,
            status: Status::Fail,
        },
    }
}

fn print_details(results: &[CheckResult], color: bool) {
    let rows = results
        .iter()
        .map(|result| {
            vec![
                result.status.label(color),
                result.target.provider.clone(),
                result.target.connection.label().into(),
                result.target.host.clone(),
                result.detail.clone(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &["STATUS", "PROVIDER", "CONNECTION", "HOST:PORT", "RESULT"],
        &rows,
    );
}

fn print_summary(results: &[CheckResult], color: bool) {
    let mut providers: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for result in results {
        let counts = providers.entry(&result.target.provider).or_default();
        match result.status {
            Status::Ok => counts.0 += 1,
            Status::Skip => counts.1 += 1,
            Status::Fail => counts.2 += 1,
            Status::Info => {}
        }
    }
    let rows = providers
        .into_iter()
        .map(|(provider, (ok, skipped, failed))| {
            let status = if failed > 0 {
                Status::Fail
            } else if ok == 0 {
                Status::Skip
            } else {
                Status::Ok
            };
            vec![
                status.label(color),
                provider.into(),
                ok.to_string(),
                skipped.to_string(),
                failed.to_string(),
            ]
        })
        .collect::<Vec<_>>();
    print_table(
        &["STATUS", "PROVIDER", "REACHABLE", "SKIPPED", "FAILED"],
        &rows,
    );
}

fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths = headers
        .iter()
        .map(|header| header.len())
        .collect::<Vec<_>>();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            let visible = value
                .chars()
                .filter(|character| *character != '\x1b')
                .count();
            widths[index] = widths[index].max(if value.contains("\x1b[") { 6 } else { visible });
        }
    }
    print!("  ");
    for (index, header) in headers.iter().enumerate() {
        print!("{header:<width$}", width = widths[index]);
        if index + 1 != headers.len() {
            print!("  ");
        }
    }
    println!();
    print!("  ");
    for (index, width) in widths.iter().enumerate() {
        print!("{}", "-".repeat(*width));
        if index + 1 != widths.len() {
            print!("  ");
        }
    }
    println!();
    for row in rows {
        print!("  ");
        for (index, value) in row.iter().enumerate() {
            if value.contains("\x1b[") {
                print!("{value}");
            } else {
                print!("{value:<width$}", width = widths[index]);
            }
            if index + 1 != row.len() {
                print!("  ");
            }
        }
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    #[test]
    fn targets_cover_all_builtin_providers_and_transports() {
        let registry = Registry::from_builtin();
        let targets = provider_targets(&registry);
        let providers = targets
            .iter()
            .map(|target| target.provider.as_str())
            .collect::<BTreeSet<_>>();
        for provider in kiss_ai::registry::BUILTIN_PROVIDER_IDS {
            assert!(providers.contains(provider), "missing provider {provider}");
        }
        assert!(targets.iter().any(|target| {
            target.provider == "openai"
                && target.connection == Connection::Sse
                && target.host == "api.openai.com:443"
        }));
        assert!(targets.iter().any(|target| {
            target.provider == "openai"
                && target.connection == Connection::WebSocket
                && target.host == "api.openai.com:443"
        }));
        assert!(targets.iter().any(|target| {
            target.provider == "anthropic"
                && target.connection == Connection::Login
                && target.host == "claude.ai:443"
        }));
        assert!(
            targets.iter().any(|target| {
                target.provider == "radius" && target.connection == Connection::Sse
            })
        );
    }

    #[test]
    fn target_removes_private_url_parts() {
        let target = target_from_url(
            "private",
            Connection::Sse,
            "https://user:password@example.com/v1?token=secret#fragment",
        );
        assert_eq!(target.host, "example.com:443");
        let url = target.url.unwrap().to_string();
        assert!(!url.contains("user"));
        assert!(!url.contains("password"));
        assert!(!url.contains("secret"));
        assert!(!url.contains("fragment"));
        assert_eq!(url, "https://example.com/v1");
    }

    #[tokio::test]
    async fn http_error_response_is_reachable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let target = target_from_url(
            "local",
            Connection::Sse,
            &format!("http://{address}/v1/messages"),
        );
        let result = probe(target).await;
        server.await.unwrap();
        assert_eq!(result.status, Status::Ok);
        assert!(result.detail.starts_with("HTTP 401"));
    }
}
