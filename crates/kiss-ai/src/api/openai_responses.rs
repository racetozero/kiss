//! OpenAI Responses API adapter (reasoning models, encrypted reasoning
//! continuity, item-based streaming).

use super::{PartialBuilder, apply_provider_headers, provider_base_url, reasoning_effort};
use crate::event::EventSink;
use crate::model::Model;
use crate::sse::{SseEvent, SseParser};
use crate::stream::{StreamOptions, Transport, http_client};
use crate::types::{ContentBlock, Context, Message, StopReason, UserContent};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Error as WebSocketError, Message as WebSocketMessage};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const CODEX_WEBSOCKET_BETA: &str = "responses_websockets=2026-02-06";
const CODEX_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CODEX_WEBSOCKET_IDLE_TTL: Duration = Duration::from_secs(5 * 60);
const CODEX_WEBSOCKET_MAX_AGE: Duration = Duration::from_secs(55 * 60);

type CodexSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SocketCacheKey {
    session_id: String,
    account_id: String,
}

struct Continuation {
    last_request: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

struct CachedSocket {
    socket: CodexSocket,
    continuation: Option<Continuation>,
}

struct SocketCacheEntry {
    socket: Arc<Mutex<CachedSocket>>,
    created_at: Instant,
    last_used: Instant,
}

type SocketCache = HashMap<SocketCacheKey, SocketCacheEntry>;

fn socket_cache() -> &'static Mutex<SocketCache> {
    static CACHE: OnceLock<Mutex<SocketCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn websocket_fallback_sessions() -> &'static Mutex<std::collections::HashSet<String>> {
    static SESSIONS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

pub async fn stream(model: &Model, context: &Context, options: &StreamOptions, sink: EventSink) {
    let mut builder = PartialBuilder::new(model, sink);
    let Some(api_key) = options.api_key.clone() else {
        builder.fail(
            format!("no API key for provider {}", model.provider),
            false,
            model,
        );
        return;
    };
    let body = build_request(model, context, options);
    let mut endpoint_model = model.clone();
    endpoint_model.base_url = provider_base_url(model, &api_key);
    let url = match response_url(&endpoint_model) {
        Ok(url) => url,
        Err(error) => {
            builder.fail(error.to_string(), false, model);
            return;
        }
    };
    let codex_account_id = if model.api == "openai-codex-responses" {
        let Some(account_id) = crate::auth::openai_codex::decode_jwt_account_id(&api_key) else {
            builder.fail("OpenAI Codex access token has no account ID", false, model);
            return;
        };
        Some(account_id)
    } else {
        None
    };
    let mut state = DecodeState::default();

    if let Some(account_id) = codex_account_id.as_deref()
        && options.transport != Transport::Sse
    {
        let fallback_active = if let Some(session_id) = options.session_id.as_deref() {
            websocket_fallback_sessions()
                .lock()
                .await
                .contains(session_id)
        } else {
            false
        };
        if !fallback_active {
            match stream_codex_websocket(
                model,
                &url,
                &body,
                &api_key,
                account_id,
                options,
                &mut builder,
                &mut state,
            )
            .await
            {
                Ok(reason) => {
                    builder.finish(reason, model);
                    return;
                }
                Err(error) if error.aborted => {
                    builder.fail(error.message, true, model);
                    return;
                }
                Err(error) if !error.is_transport || builder.is_started() => {
                    builder.fail(
                        format!("WebSocket stream failed: {}", error.message),
                        false,
                        model,
                    );
                    return;
                }
                Err(_) => {
                    state = DecodeState::default();
                    if let Some(session_id) = options.session_id.clone() {
                        websocket_fallback_sessions()
                            .lock()
                            .await
                            .insert(session_id);
                    }
                }
            }
        }
    }

    let mut request = http_client()
        .post(&url)
        .header("content-type", "application/json")
        .json(&body);
    for (key, value) in &model.headers {
        request = request.header(key, value);
    }
    match model.api.as_str() {
        "openai-codex-responses" => {
            request = request
                .bearer_auth(&api_key)
                .header(
                    "chatgpt-account-id",
                    codex_account_id.as_deref().unwrap_or_default(),
                )
                .header("originator", "kiss")
                .header("user-agent", concat!("kiss/", env!("CARGO_PKG_VERSION")))
                .header("OpenAI-Beta", "responses=experimental")
                .header("accept", "text/event-stream");
            if let Some(session_id) = &options.session_id {
                request = request
                    .header("session-id", session_id)
                    .header("x-client-request-id", session_id);
            }
        }
        "azure-openai-responses" => {
            request = request.header("api-key", &api_key);
        }
        _ => {
            request = request.bearer_auth(&api_key);
        }
    }
    request = apply_provider_headers(request, model, context);

    let response = tokio::select! {
        r = request.send() => r,
        _ = options.cancel.cancelled() => { builder.fail("Request aborted", true, model); return; }
    };
    let response = match response {
        Ok(r) => r,
        Err(e) => {
            builder.fail(format!("request failed: {e}"), false, model);
            return;
        }
    };
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        builder.fail(
            format!("HTTP {status}: {}", crate::truncate_err(&text)),
            false,
            model,
        );
        return;
    }

    let mut parser = SseParser::new();
    let mut events: Vec<SseEvent> = Vec::new();
    let mut body_stream = response.bytes_stream();
    loop {
        let chunk = tokio::select! {
            c = body_stream.next() => c,
            _ = options.cancel.cancelled() => { builder.fail("Request aborted", true, model); return; }
        };
        match chunk {
            Some(Ok(bytes)) => {
                parser.feed(&bytes, &mut events);
                for event in events.drain(..) {
                    match handle_event(&event, &mut builder, &mut state) {
                        Flow::Continue => {}
                        Flow::Done(reason) => {
                            builder.finish(reason, model);
                            return;
                        }
                        Flow::Error(msg) => {
                            builder.fail(msg, false, model);
                            return;
                        }
                    }
                }
            }
            Some(Err(e)) => {
                builder.fail(format!("stream error: {e}"), false, model);
                return;
            }
            None => break,
        }
    }
    builder.fail("response stream ended before completion", false, model);
}

pub(crate) fn response_url(model: &Model) -> anyhow::Result<String> {
    let base = model.base_url.trim_end_matches('/');
    match model.api.as_str() {
        "openai-codex-responses" if base.ends_with("/codex/responses") => Ok(base.into()),
        "openai-codex-responses" if base.ends_with("/codex") => Ok(format!("{base}/responses")),
        "openai-codex-responses" => Ok(format!("{base}/codex/responses")),
        "azure-openai-responses" => {
            let configured = std::env::var("AZURE_OPENAI_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    std::env::var("AZURE_OPENAI_RESOURCE_NAME")
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .map(|resource| format!("https://{resource}.openai.azure.com/openai/v1"))
                })
                .or_else(|| (!base.is_empty()).then(|| base.to_string()))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Azure OpenAI needs AZURE_OPENAI_BASE_URL or AZURE_OPENAI_RESOURCE_NAME"
                    )
                })?;
            let configured = configured.trim_end_matches('/');
            let configured = if configured.ends_with("/responses") {
                configured.to_string()
            } else {
                format!("{configured}/responses")
            };
            let version = std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "v1".into());
            Ok(format!("{configured}?api-version={version}"))
        }
        _ => Ok(format!("{base}/responses")),
    }
}

struct WebSocketFailure {
    message: String,
    is_transport: bool,
    aborted: bool,
}

impl WebSocketFailure {
    fn transport(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_transport: true,
            aborted: false,
        }
    }

    fn protocol(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_transport: false,
            aborted: false,
        }
    }

    fn aborted() -> Self {
        Self {
            message: "Request aborted".into(),
            is_transport: false,
            aborted: true,
        }
    }
}

struct SocketLease {
    guard: OwnedMutexGuard<CachedSocket>,
    shared: Arc<Mutex<CachedSocket>>,
    cache_key: Option<SocketCacheKey>,
}

fn codex_websocket_url(response_url: &str) -> anyhow::Result<String> {
    let mut url = url::Url::parse(response_url)?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => anyhow::bail!("unsupported Codex URL scheme: {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("cannot make Codex WebSocket URL"))?;
    Ok(url.to_string())
}

fn insert_websocket_header(
    headers: &mut tokio_tungstenite::tungstenite::http::HeaderMap,
    name: &str,
    value: &str,
) -> Result<(), WebSocketFailure> {
    let name = HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| WebSocketFailure::protocol(format!("invalid header name: {name}")))?;
    let value = HeaderValue::from_str(value)
        .map_err(|_| WebSocketFailure::protocol("invalid WebSocket header value"))?;
    headers.insert(name, value);
    Ok(())
}

async fn connect_codex_websocket(
    model: &Model,
    response_url: &str,
    api_key: &str,
    account_id: &str,
    request_id: &str,
    options: &StreamOptions,
) -> Result<CodexSocket, WebSocketFailure> {
    let websocket_url = codex_websocket_url(response_url)
        .map_err(|error| WebSocketFailure::protocol(error.to_string()))?;
    let mut request = websocket_url
        .as_str()
        .into_client_request()
        .map_err(|error| WebSocketFailure::protocol(error.to_string()))?;
    for (name, value) in &model.headers {
        insert_websocket_header(request.headers_mut(), name, value)?;
    }
    insert_websocket_header(
        request.headers_mut(),
        "authorization",
        &format!("Bearer {api_key}"),
    )?;
    insert_websocket_header(request.headers_mut(), "chatgpt-account-id", account_id)?;
    insert_websocket_header(request.headers_mut(), "originator", "kiss")?;
    insert_websocket_header(
        request.headers_mut(),
        "user-agent",
        concat!("kiss/", env!("CARGO_PKG_VERSION")),
    )?;
    insert_websocket_header(request.headers_mut(), "openai-beta", CODEX_WEBSOCKET_BETA)?;
    insert_websocket_header(request.headers_mut(), "x-client-request-id", request_id)?;
    insert_websocket_header(request.headers_mut(), "session-id", request_id)?;

    let connect = async {
        tokio::select! {
            result = connect_async(request) => result
                .map(|(socket, _)| socket)
                .map_err(|error| WebSocketFailure::transport(format_websocket_error(&error))),
            _ = options.cancel.cancelled() => Err(WebSocketFailure::aborted()),
        }
    };
    tokio::time::timeout(CODEX_WEBSOCKET_CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| WebSocketFailure::transport("WebSocket connection timed out"))?
}

fn format_websocket_error(error: &WebSocketError) -> String {
    match error {
        WebSocketError::Http(response) => {
            format!("WebSocket handshake returned HTTP {}", response.status())
        }
        _ => format!("WebSocket transport error: {error}"),
    }
}

async fn acquire_codex_socket(
    model: &Model,
    response_url: &str,
    api_key: &str,
    account_id: &str,
    options: &StreamOptions,
) -> Result<SocketLease, WebSocketFailure> {
    let cache_key = options
        .session_id
        .as_ref()
        .map(|session_id| SocketCacheKey {
            session_id: session_id.clone(),
            account_id: account_id.to_string(),
        });
    let mut cache_new_connection = cache_key.is_some();

    if let Some(key) = cache_key.as_ref() {
        let now = Instant::now();
        let mut cache = socket_cache().lock().await;
        let expired = cache.get(key).is_some_and(|entry| {
            now.duration_since(entry.last_used) >= CODEX_WEBSOCKET_IDLE_TTL
                || now.duration_since(entry.created_at) >= CODEX_WEBSOCKET_MAX_AGE
        });
        if expired {
            cache.remove(key);
        }
        if let Some(entry) = cache.get(key) {
            let shared = Arc::clone(&entry.socket);
            if let Ok(guard) = Arc::clone(&shared).try_lock_owned() {
                return Ok(SocketLease {
                    guard,
                    shared,
                    cache_key: Some(key.clone()),
                });
            }
            cache_new_connection = false;
        }
    }

    let request_id = options
        .session_id
        .clone()
        .unwrap_or_else(|| format!("{:032x}", rand::random::<u128>()));
    let socket = connect_codex_websocket(
        model,
        response_url,
        api_key,
        account_id,
        &request_id,
        options,
    )
    .await?;
    let shared = Arc::new(Mutex::new(CachedSocket {
        socket,
        continuation: None,
    }));
    let guard = Arc::clone(&shared).lock_owned().await;
    let cache_key = if cache_new_connection {
        if let Some(key) = cache_key {
            let now = Instant::now();
            socket_cache().lock().await.insert(
                key.clone(),
                SocketCacheEntry {
                    socket: Arc::clone(&shared),
                    created_at: now,
                    last_used: now,
                },
            );
            Some(key)
        } else {
            None
        }
    } else {
        None
    };
    Ok(SocketLease {
        guard,
        shared,
        cache_key,
    })
}

async fn release_codex_socket(mut lease: SocketLease, keep: bool) {
    let Some(key) = lease.cache_key.clone() else {
        let _ = lease.guard.socket.close(None).await;
        return;
    };

    if !keep {
        let _ = lease.guard.socket.close(None).await;
        let mut cache = socket_cache().lock().await;
        if cache
            .get(&key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.socket, &lease.shared))
        {
            cache.remove(&key);
        }
        return;
    }

    let released_at = Instant::now();
    {
        let mut cache = socket_cache().lock().await;
        if let Some(entry) = cache.get_mut(&key)
            && Arc::ptr_eq(&entry.socket, &lease.shared)
        {
            entry.last_used = released_at;
        }
    }
    let shared = Arc::clone(&lease.shared);
    drop(lease);
    tokio::spawn(async move {
        tokio::time::sleep(CODEX_WEBSOCKET_IDLE_TTL).await;
        let removed = {
            let mut cache = socket_cache().lock().await;
            let can_remove = cache.get(&key).is_some_and(|entry| {
                Arc::ptr_eq(&entry.socket, &shared) && entry.last_used <= released_at
            });
            can_remove.then(|| cache.remove(&key)).flatten()
        };
        if removed.is_some()
            && let Ok(mut socket) = shared.try_lock_owned()
        {
            let _ = socket.socket.close(None).await;
        }
    });
}

fn request_without_input(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("input");
        object.remove("previous_response_id");
    }
    body
}

fn cached_request_body(body: &Value, continuation: &Continuation) -> Option<Value> {
    if request_without_input(body) != request_without_input(&continuation.last_request) {
        return None;
    }
    let current = body["input"].as_array()?;
    let mut baseline = continuation.last_request["input"].as_array()?.clone();
    baseline.extend(continuation.last_response_items.iter().cloned());
    if current.len() < baseline.len() || current[..baseline.len()] != baseline {
        return None;
    }
    let mut request = body.clone();
    request["previous_response_id"] = json!(continuation.last_response_id);
    request["input"] = Value::Array(current[baseline.len()..].to_vec());
    Some(request)
}

fn websocket_frame(body: &Value) -> Result<WebSocketMessage, WebSocketFailure> {
    let mut frame = body
        .as_object()
        .cloned()
        .ok_or_else(|| WebSocketFailure::protocol("Codex request body is not an object"))?;
    frame.insert("type".into(), json!("response.create"));
    Ok(WebSocketMessage::Text(
        Value::Object(frame).to_string().into(),
    ))
}

fn websocket_json(message: WebSocketMessage) -> Result<Option<Value>, WebSocketFailure> {
    let bytes = match message {
        WebSocketMessage::Text(text) => text.as_bytes().to_vec(),
        WebSocketMessage::Binary(bytes) => bytes.to_vec(),
        WebSocketMessage::Ping(_) | WebSocketMessage::Pong(_) | WebSocketMessage::Frame(_) => {
            return Ok(None);
        }
        WebSocketMessage::Close(frame) => {
            let detail = frame.map_or_else(
                || "WebSocket closed before response completion".to_string(),
                |frame| {
                    format!(
                        "WebSocket closed before response completion: {} {}",
                        u16::from(frame.code),
                        frame.reason
                    )
                },
            );
            return Err(WebSocketFailure::transport(detail));
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| WebSocketFailure::protocol("invalid JSON in Codex WebSocket response"))
}

fn codex_event_error(data: &Value) -> Option<(String, String)> {
    let event_type = data["type"].as_str()?;
    match event_type {
        "error" => {
            let code = data["code"]
                .as_str()
                .or_else(|| data["error"]["code"].as_str())
                .unwrap_or("provider_error");
            let message = data["message"]
                .as_str()
                .or_else(|| data["error"]["message"].as_str())
                .unwrap_or(code);
            Some((code.to_string(), message.to_string()))
        }
        "response.failed" => {
            let code = data["response"]["error"]["code"]
                .as_str()
                .unwrap_or("response_failed");
            let message = data["response"]["error"]["message"]
                .as_str()
                .unwrap_or(code);
            Some((code.to_string(), message.to_string()))
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_codex_websocket(
    model: &Model,
    response_url: &str,
    full_body: &Value,
    api_key: &str,
    account_id: &str,
    options: &StreamOptions,
    builder: &mut PartialBuilder,
    state: &mut DecodeState,
) -> Result<StopReason, WebSocketFailure> {
    let mut lease = acquire_codex_socket(model, response_url, api_key, account_id, options).await?;
    let use_continuation = matches!(
        options.transport,
        Transport::Auto | Transport::WebSocketCached
    );
    let mut retried_missing_continuation = false;

    loop {
        let request_body = if use_continuation {
            lease
                .guard
                .continuation
                .as_ref()
                .and_then(|continuation| cached_request_body(full_body, continuation))
                .unwrap_or_else(|| {
                    lease.guard.continuation = None;
                    full_body.clone()
                })
        } else {
            full_body.clone()
        };
        let frame = websocket_frame(&request_body)?;
        let sent = tokio::select! {
            result = lease.guard.socket.send(frame) => result,
            _ = options.cancel.cancelled() => {
                release_codex_socket(lease, false).await;
                return Err(WebSocketFailure::aborted());
            }
        };
        if let Err(error) = sent {
            release_codex_socket(lease, false).await;
            return Err(WebSocketFailure::transport(format_websocket_error(&error)));
        }

        loop {
            let message = tokio::select! {
                message = lease.guard.socket.next() => message,
                _ = options.cancel.cancelled() => {
                    release_codex_socket(lease, false).await;
                    return Err(WebSocketFailure::aborted());
                }
            };
            let message = match message {
                Some(Ok(message)) => message,
                Some(Err(error)) => {
                    release_codex_socket(lease, false).await;
                    return Err(WebSocketFailure::transport(format_websocket_error(&error)));
                }
                None => {
                    release_codex_socket(lease, false).await;
                    return Err(WebSocketFailure::transport(
                        "WebSocket closed before response completion",
                    ));
                }
            };
            let data = match websocket_json(message) {
                Ok(Some(data)) => data,
                Ok(None) => continue,
                Err(error) => {
                    release_codex_socket(lease, false).await;
                    return Err(error);
                }
            };
            if let Some((code, message)) = codex_event_error(&data) {
                if code == "previous_response_not_found"
                    && request_body.get("previous_response_id").is_some()
                    && !retried_missing_continuation
                {
                    retried_missing_continuation = true;
                    lease.guard.continuation = None;
                    *state = DecodeState::default();
                    break;
                }
                let is_transport = code == "websocket_connection_limit_reached";
                release_codex_socket(lease, false).await;
                return Err(WebSocketFailure {
                    message,
                    is_transport,
                    aborted: false,
                });
            }
            let event_type = data["type"].as_str().unwrap_or_default();
            let event = SseEvent {
                event: Some(event_type.to_string()),
                data: data.to_string(),
            };
            match handle_event(&event, builder, state) {
                Flow::Continue => {}
                Flow::Done(reason) => {
                    if use_continuation
                        && let Some(response_id) = builder.message.response_id.clone()
                    {
                        lease.guard.continuation = Some(Continuation {
                            last_request: full_body.clone(),
                            last_response_id: response_id,
                            last_response_items: state.response_items.clone(),
                        });
                    }
                    release_codex_socket(lease, true).await;
                    return Ok(reason);
                }
                Flow::Error(message) => {
                    release_codex_socket(lease, false).await;
                    return Err(WebSocketFailure::protocol(message));
                }
            }
        }
    }
}

#[derive(Default)]
struct DecodeState {
    /// Provider output_index -> (builder index, item kind).
    items: std::collections::HashMap<u64, (usize, ItemKind)>,
    /// Completed output items used by cached WebSocket continuation.
    response_items: Vec<Value>,
}

#[derive(Clone, Copy, PartialEq)]
enum ItemKind {
    Text,
    Reasoning,
    FunctionCall,
}

enum Flow {
    Continue,
    Done(StopReason),
    Error(String),
}

fn handle_event(event: &SseEvent, builder: &mut PartialBuilder, state: &mut DecodeState) -> Flow {
    let data: Value = match serde_json::from_str(&event.data) {
        Ok(v) => v,
        Err(_) => return Flow::Continue,
    };
    let event_type = event
        .event
        .as_deref()
        .or_else(|| data["type"].as_str())
        .unwrap_or("");
    match event_type {
        "response.created" => {
            builder.start();
            if let Some(id) = data["response"]["id"].as_str() {
                builder.message.response_id = Some(id.to_string());
            }
            Flow::Continue
        }
        "response.output_item.added" => {
            let output_index = data["output_index"].as_u64().unwrap_or(0);
            let item = &data["item"];
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    let idx = builder.begin_text();
                    if let Some(id) = item["id"].as_str() {
                        let signature = if let Some(phase) = item["phase"].as_str() {
                            json!({"v":1,"id":id,"phase":phase}).to_string()
                        } else {
                            id.to_string()
                        };
                        builder.set_text_signature(idx, signature);
                    }
                    state.items.insert(output_index, (idx, ItemKind::Text));
                }
                "reasoning" => {
                    let idx = builder.begin_thinking();
                    if let Some(id) = item["id"].as_str() {
                        builder.set_thinking_signature(idx, id.to_string());
                    }
                    state.items.insert(output_index, (idx, ItemKind::Reasoning));
                }
                "function_call" => {
                    let call_id = item["call_id"].as_str().unwrap_or_default().to_string();
                    let item_id = item["id"].as_str().unwrap_or_default();
                    let call_id = if item_id.is_empty() {
                        call_id
                    } else {
                        format!("{call_id}|{item_id}")
                    };
                    let name = item["name"].as_str().unwrap_or_default().to_string();
                    let idx = builder.begin_tool_call(call_id, name);
                    state
                        .items
                        .insert(output_index, (idx, ItemKind::FunctionCall));
                }
                _ => {}
            }
            Flow::Continue
        }
        "response.output_text.delta" => {
            let output_index = data["output_index"].as_u64().unwrap_or(0);
            if let Some(&(idx, ItemKind::Text)) = state.items.get(&output_index)
                && let Some(t) = data["delta"].as_str()
            {
                builder.append_text(idx, t);
            }
            Flow::Continue
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let output_index = data["output_index"].as_u64().unwrap_or(0);
            if let Some(&(idx, ItemKind::Reasoning)) = state.items.get(&output_index)
                && let Some(t) = data["delta"].as_str()
            {
                builder.append_thinking(idx, t);
            }
            Flow::Continue
        }
        "response.function_call_arguments.delta" => {
            let output_index = data["output_index"].as_u64().unwrap_or(0);
            if let Some(&(idx, ItemKind::FunctionCall)) = state.items.get(&output_index)
                && let Some(t) = data["delta"].as_str()
            {
                builder.append_tool_args(idx, t);
            }
            Flow::Continue
        }
        "response.output_item.done" => {
            let output_index = data["output_index"].as_u64().unwrap_or(0);
            let item = &data["item"];
            if let Some(&(idx, kind)) = state.items.get(&output_index) {
                match kind {
                    ItemKind::Text => builder.end_text(idx),
                    ItemKind::Reasoning => {
                        if let Ok(signature) = serde_json::to_string(item) {
                            builder.set_thinking_signature(idx, signature);
                        }
                        builder.end_thinking(idx);
                    }
                    ItemKind::FunctionCall => {
                        let structured = item["arguments"]
                            .as_str()
                            .and_then(|s| serde_json::from_str::<Value>(s).ok());
                        builder.end_tool_call(idx, structured);
                    }
                }
            }
            if matches!(
                item["type"].as_str(),
                Some("message" | "reasoning" | "function_call" | "custom_tool_call")
            ) {
                state.response_items.push(item.clone());
            }
            Flow::Continue
        }
        "response.completed" | "response.done" | "response.incomplete" => {
            let response = &data["response"];
            if let Some(id) = response["id"].as_str() {
                builder.message.response_id = Some(id.to_string());
            }
            let usage = &response["usage"];
            let cached = usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            builder.message.usage.input = usage["input_tokens"]
                .as_u64()
                .unwrap_or(0)
                .saturating_sub(cached);
            builder.message.usage.cache_read = cached;
            builder.message.usage.output = usage["output_tokens"].as_u64().unwrap_or(0);
            if let Some(r) = usage["output_tokens_details"]["reasoning_tokens"].as_u64() {
                builder.message.usage.reasoning = Some(r);
            }
            let status = response["status"].as_str().unwrap_or("completed");
            builder.message.raw_stop_reason = Some(status.to_string());
            let incomplete_len =
                response["incomplete_details"]["reason"].as_str() == Some("max_output_tokens");
            let reason = if event_type == "response.incomplete" && incomplete_len {
                StopReason::Length
            } else {
                StopReason::Stop
            };
            Flow::Done(reason)
        }
        "response.failed" => {
            let msg = data["response"]["error"]["message"]
                .as_str()
                .unwrap_or("response failed")
                .to_string();
            Flow::Error(msg)
        }
        "error" => Flow::Error(
            data["message"]
                .as_str()
                .unwrap_or("provider error")
                .to_string(),
        ),
        _ => Flow::Continue,
    }
}

pub(crate) fn response_input(context: &Context) -> Vec<Value> {
    let mut input = context.openai_responses_input.clone().unwrap_or_default();
    for message in &context.messages {
        match message {
            Message::User(user) => {
                let content = match &user.content {
                    UserContent::Text(t) => json!([{ "type": "input_text", "text": t }]),
                    UserContent::Blocks(blocks) => Value::Array(
                        blocks
                            .iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text, .. } => {
                                    Some(json!({"type": "input_text", "text": text}))
                                }
                                ContentBlock::Image { data, mime_type } => Some(json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{mime_type};base64,{data}"),
                                })),
                                _ => None,
                            })
                            .collect(),
                    ),
                };
                input.push(json!({"type": "message", "role": "user", "content": content}));
            }
            Message::Assistant(assistant) => {
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking {
                            thinking_signature: Some(sig),
                            ..
                        } => {
                            if let Ok(item) = serde_json::from_str::<Value>(sig)
                                && item["type"] == "reasoning"
                            {
                                input.push(item);
                            } else if let Some(enc) = sig.strip_prefix("enc:") {
                                input.push(json!({
                                    "type": "reasoning",
                                    "summary": [],
                                    "encrypted_content": enc,
                                }));
                            }
                        }
                        ContentBlock::Text {
                            text,
                            text_signature,
                        } => {
                            if !text.is_empty() {
                                let mut item = json!({
                                    "type": "message",
                                    "role": "assistant",
                                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                                    "status": "completed",
                                });
                                if let Some(signature) = text_signature {
                                    if let Ok(parsed) = serde_json::from_str::<Value>(signature)
                                        && parsed["v"] == 1
                                        && parsed["id"].is_string()
                                    {
                                        item["id"] = parsed["id"].clone();
                                        if parsed["phase"].is_string() {
                                            item["phase"] = parsed["phase"].clone();
                                        }
                                    } else {
                                        item["id"] = json!(signature);
                                    }
                                }
                                input.push(item);
                            }
                        }
                        ContentBlock::ToolCall(tc) => {
                            let (call_id, item_id) = tc
                                .id
                                .split_once('|')
                                .map_or((tc.id.as_str(), None), |(call_id, item_id)| {
                                    (call_id, Some(item_id))
                                });
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".into()),
                            });
                            if let Some(item_id) = item_id {
                                item["id"] = json!(item_id);
                            }
                            input.push(item);
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(result) => {
                let call_id = result
                    .tool_call_id
                    .split_once('|')
                    .map_or(result.tool_call_id.as_str(), |(call_id, _)| call_id);
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        ContentBlock::Image { .. } => Some("[image attached]"),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text,
                }));
            }
        }
    }
    input
}

pub(crate) fn build_request(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let input = response_input(context);
    let mut body = json!({
        "model": model.id,
        "input": input,
        "stream": true,
        "store": false,
        "parallel_tool_calls": true,
    });
    if model.api != "openai-codex-responses" {
        body["max_output_tokens"] = json!(options.max_tokens.unwrap_or(model.max_tokens));
    }
    if let Some(system) = &context.system_prompt {
        body["instructions"] = json!(system);
    }
    if model.api == "openai-codex-responses" {
        if context.system_prompt.is_none() {
            body["instructions"] = json!("You are a helpful assistant.");
        }
        body["text"] = json!({"verbosity": "low"});
        body["tool_choice"] = json!("auto");
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if model.reasoning {
        if let Some(effort) = reasoning_effort(options.reasoning) {
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    } else if let Some(t) = options.temperature {
        body["temperature"] = json!(t);
    }
    if !context.tools.is_empty() {
        body["tools"] = Value::Array(
            context
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    })
                })
                .collect(),
        );
    }
    if let Some(session) = &options.session_id {
        body["prompt_cache_key"] = json!(session);
    }
    body
}

#[cfg(test)]
mod request_tests {
    use super::*;
    use base64::Engine as _;
    use std::collections::BTreeMap;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_hdr_async;

    fn model(api: &str, base_url: &str) -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT test".into(),
            api: api.into(),
            provider: "test".into(),
            base_url: base_url.into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Default::default(),
            context_window: 1000,
            max_tokens: 100,
            compat: None,
            headers: BTreeMap::new(),
        }
    }

    #[test]
    fn codex_uses_subscription_endpoint() {
        assert_eq!(
            response_url(&model(
                "openai-codex-responses",
                "https://chatgpt.com/backend-api"
            ))
            .unwrap(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }

    #[test]
    fn request_enables_parallel_tools() {
        let body = build_request(
            &model("openai-responses", "https://api.openai.com/v1"),
            &Context::default(),
            &StreamOptions::default(),
        );
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 100);
    }

    #[test]
    fn codex_request_omits_unsupported_output_limit() {
        let body = build_request(
            &model("openai-codex-responses", "https://chatgpt.com/backend-api"),
            &Context::default(),
            &StreamOptions::default(),
        );
        assert!(body.get("max_output_tokens").is_none());
    }

    #[tokio::test]
    async fn codex_stream_sends_subscription_headers_and_decodes_sse() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let events = [
                (
                    "response.created",
                    json!({"type":"response.created","response":{"id":"resp-one"}}),
                ),
                (
                    "response.output_item.added",
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg-one"}}),
                ),
                (
                    "response.output_text.delta",
                    json!({"type":"response.output_text.delta","output_index":0,"delta":"hello"}),
                ),
                (
                    "response.output_item.done",
                    json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}),
                ),
                (
                    "response.completed",
                    json!({"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":12,"input_tokens_details":{"cached_tokens":2},"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1}}}}),
                ),
            ];
            let body = events
                .into_iter()
                .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
                .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });

        let payload = json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"acct-fixture"}
        });
        let access = format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        );
        let mut codex = model("openai-codex-responses", &format!("http://{address}"));
        codex.provider = "openai-codex".into();
        let context = Context {
            system_prompt: Some("be concise".into()),
            openai_responses_input: None,
            messages: vec![Message::User(crate::UserMessage {
                content: UserContent::Text("hi".into()),
                timestamp: 1,
            })],
            tools: vec![crate::types::ToolDef {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type":"object"}),
            }],
        };
        let output = crate::stream_simple(
            &codex,
            &context,
            &StreamOptions {
                api_key: Some(access),
                session_id: Some("session-one".into()),
                transport: Transport::Sse,
                ..Default::default()
            },
        )
        .result()
        .await;
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.text(), "hello");
        assert_eq!(output.response_id.as_deref(), Some("resp-one"));
        assert_eq!(output.usage.input, 10);
        assert_eq!(output.usage.cache_read, 2);
        assert_eq!(output.usage.output, 3);

        let request = server.await.unwrap();
        let lower = request.to_lowercase();
        assert!(lower.starts_with("post /codex/responses http/1.1"));
        assert!(lower.contains("chatgpt-account-id: acct-fixture"));
        assert!(lower.contains("originator: kiss"));
        assert!(lower.contains("openai-beta: responses=experimental"));
        assert!(lower.contains("session-id: session-one"));
        let body = request.split_once("\r\n\r\n").unwrap().1;
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "be concise");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["store"], false);
    }

    fn codex_access_token() -> String {
        let payload = json!({
            "https://api.openai.com/auth": {"chatgpt_account_id":"acct-fixture"}
        });
        format!(
            "e30.{}.sig",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    async fn send_websocket_event<S>(socket: &mut WebSocketStream<S>, event: Value)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        socket
            .send(WebSocketMessage::Text(event.to_string().into()))
            .await
            .unwrap();
    }

    async fn send_websocket_response<S>(socket: &mut WebSocketStream<S>, turn: usize)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let response_id = format!("resp-{turn}");
        let message_id = format!("msg-{turn}");
        let text = if turn == 1 { "hello" } else { "again" };
        let item = json!({
            "type": "message",
            "id": message_id,
            "role": "assistant",
            "status": "completed",
            "phase": "final_answer",
            "content": [{"type":"output_text", "text":text, "annotations":[]}]
        });
        send_websocket_event(
            socket,
            json!({"type":"response.created", "response":{"id":response_id}}),
        )
        .await;
        send_websocket_event(
            socket,
            json!({"type":"response.output_item.added", "output_index":0, "item":item}),
        )
        .await;
        send_websocket_event(
            socket,
            json!({"type":"response.output_text.delta", "output_index":0, "delta":text}),
        )
        .await;
        send_websocket_event(
            socket,
            json!({"type":"response.output_item.done", "output_index":0, "item":item}),
        )
        .await;
        send_websocket_event(
            socket,
            json!({
                "type":"response.completed",
                "response":{
                    "id":response_id,
                    "status":"completed",
                    "usage":{
                        "input_tokens":12,
                        "input_tokens_details":{"cached_tokens":2},
                        "output_tokens":3,
                        "output_tokens_details":{"reasoning_tokens":1}
                    }
                }
            }),
        )
        .await;
    }

    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn codex_websocket_reuses_connection_and_sends_only_delta_input() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let (headers_tx, headers_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut handshake = None;
            let mut socket = accept_hdr_async(
                socket,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                let headers = request
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_string(),
                            value.to_str().unwrap_or_default().to_string(),
                        )
                    })
                    .collect::<BTreeMap<_, _>>();
                handshake = Some((request.uri().path().to_string(), headers));
                    Ok(response)
                },
            )
            .await
            .unwrap();
            headers_tx.send(handshake.unwrap()).unwrap();

            let mut requests = Vec::new();
            for turn in 1..=2 {
                let message = socket.next().await.unwrap().unwrap();
                let WebSocketMessage::Text(text) = message else {
                    panic!("expected text request frame");
                };
                requests.push(serde_json::from_str::<Value>(&text).unwrap());
                send_websocket_response(&mut socket, turn).await;
            }
            requests
        });

        let mut codex = model("openai-codex-responses", &format!("http://{address}"));
        codex.provider = "openai-codex".into();
        let first_user = Message::User(crate::UserMessage {
            content: UserContent::Text("hi".into()),
            timestamp: 1,
        });
        let context = Context {
            system_prompt: Some("be concise".into()),
            openai_responses_input: None,
            messages: vec![first_user.clone()],
            tools: vec![],
        };
        let options = StreamOptions {
            api_key: Some(codex_access_token()),
            session_id: Some("websocket-cache-fixture".into()),
            transport: Transport::WebSocketCached,
            ..Default::default()
        };
        let first = crate::stream_simple(&codex, &context, &options)
            .result()
            .await;
        assert_eq!(first.text(), "hello");
        assert_eq!(first.response_id.as_deref(), Some("resp-1"));

        let second_context = Context {
            system_prompt: Some("be concise".into()),
            openai_responses_input: None,
            messages: vec![
                first_user,
                Message::Assistant(first),
                Message::User(crate::UserMessage {
                    content: UserContent::Text("again".into()),
                    timestamp: 2,
                }),
            ],
            tools: vec![],
        };
        let second = crate::stream_simple(&codex, &second_context, &options)
            .result()
            .await;
        assert_eq!(second.text(), "again");
        assert_eq!(second.response_id.as_deref(), Some("resp-2"));

        let (path, headers) = headers_rx.await.unwrap();
        assert_eq!(path, "/codex/responses");
        assert_eq!(headers["chatgpt-account-id"], "acct-fixture");
        assert_eq!(headers["originator"], "kiss");
        assert_eq!(headers["openai-beta"], CODEX_WEBSOCKET_BETA);
        assert_eq!(headers["session-id"], "websocket-cache-fixture");

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["type"], "response.create");
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 1);
        assert_eq!(requests[1]["previous_response_id"], "resp-1");
        assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
        assert_eq!(requests[1]["input"][0]["content"][0]["text"], "again");
    }

    #[tokio::test]
    async fn codex_websocket_handshake_failure_falls_back_to_sse() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut websocket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = websocket.read(&mut request).await.unwrap();
            websocket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();

            let (mut sse, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let count = sse.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            let body = [
                json!({"type":"response.created","response":{"id":"resp-sse"}}),
                json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg-sse"}}),
                json!({"type":"response.output_text.delta","output_index":0,"delta":"fallback"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}),
                json!({"type":"response.completed","response":{"status":"completed","usage":{}}}),
            ]
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sse.write_all(response.as_bytes()).await.unwrap();
        });

        let mut codex = model("openai-codex-responses", &format!("http://{address}"));
        codex.provider = "openai-codex".into();
        let output = crate::stream_simple(
            &codex,
            &Context::default(),
            &StreamOptions {
                api_key: Some(codex_access_token()),
                session_id: Some("websocket-fallback-fixture".into()),
                ..Default::default()
            },
        )
        .result()
        .await;
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert_eq!(output.text(), "fallback");
        server.await.unwrap();
    }
}
