//! Native Agent Client Protocol server over standard input and output.

use crate::args::Args;
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, DeleteSessionRequest, DeleteSessionResponse, Diff, EmbeddedResourceResource,
    Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, McpCapabilities, McpServer,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SessionDeleteCapabilities, SessionId, SessionInfo, SessionListCapabilities,
    SessionNotification, SessionResumeCapabilities, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Responder, Stdio};
use anyhow::Result;
use kiss_agent::{AgentEvent, AgentMessage, ToolResult};
use kiss_ai::{
    AssistantEvent, ContentBlock as KissContentBlock, ThinkingLevel, UserContent, UserMessage,
};
use kiss_coding::session::manager::{SessionManager, default_session_dir};
use kiss_coding::session_runner::{AgentSession, SessionEvent};
use kiss_sdk::{SessionOptions, SessionSource};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

enum AcpEvent {
    Session(SessionEvent),
    MutationSnapshot {
        id: String,
        path: PathBuf,
        old_text: Option<String>,
    },
}

struct ActiveSession {
    agent: Arc<AgentSession>,
    cwd: PathBuf,
    events: tokio::sync::Mutex<mpsc::UnboundedReceiver<AcpEvent>>,
    prompt: tokio::sync::Mutex<()>,
    cancelled: AtomicBool,
    closed: AtomicBool,
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.agent.abort();
    }
}

#[derive(Clone)]
struct KissAcpAgent {
    options: SessionOptions,
    sessions: Arc<Mutex<HashMap<SessionId, Arc<ActiveSession>>>>,
}

impl KissAcpAgent {
    fn new(options: SessionOptions) -> Self {
        Self {
            options,
            sessions: Default::default(),
        }
    }

    fn session(&self, id: &SessionId) -> Result<Arc<ActiveSession>, agent_client_protocol::Error> {
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| {
                agent_client_protocol::Error::invalid_params()
                    .data(format!("unknown session: {id}"))
            })
    }

    fn build_session(
        &self,
        cwd: PathBuf,
        source: SessionSource,
        mcp_servers: Vec<McpServer>,
    ) -> Result<(SessionId, Arc<ActiveSession>), agent_client_protocol::Error> {
        if !cwd.is_absolute() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("session cwd must be an absolute path"));
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let snapshot_cwd = cwd.clone();
        let sink = Arc::new(move |event: SessionEvent| {
            if let SessionEvent::Agent(agent_event) = &event
                && let AgentEvent::ToolExecutionStart {
                    tool_call_id,
                    tool_name,
                    args,
                } = agent_event.as_ref()
                && matches!(tool_name.as_str(), "edit" | "write")
                && let Some(path) = tool_path(args, &snapshot_cwd)
            {
                let old_text = std::fs::read_to_string(&path).ok();
                let _ = sender.send(AcpEvent::MutationSnapshot {
                    id: tool_call_id.clone(),
                    path,
                    old_text,
                });
            }
            let _ = sender.send(AcpEvent::Session(event));
        });
        let mut options = self.options.clone();
        options.cwd = cwd.clone();
        options.session = source;
        options.mcp_servers = acp_mcp_servers(mcp_servers)?;
        let built = options.build(sink).map_err(internal_error)?;
        let id = SessionId::new(
            built
                .session
                .manager
                .lock()
                .expect("session manager poisoned")
                .session_id()
                .to_string(),
        );
        let session = Arc::new(ActiveSession {
            agent: built.session,
            cwd,
            events: tokio::sync::Mutex::new(receiver),
            prompt: tokio::sync::Mutex::new(()),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .insert(id.clone(), session.clone());
        Ok((id, session))
    }

    fn open_session(
        &self,
        id: &SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Arc<ActiveSession>, agent_client_protocol::Error> {
        if let Ok(session) = self.session(id) {
            return Ok(session);
        }
        let directory = self
            .options
            .session_dir
            .clone()
            .unwrap_or_else(default_session_dir);
        let reference = id.to_string();
        let path = SessionManager::find_by_id(&directory, &reference)
            .map_err(internal_error)?
            .ok_or_else(|| {
                agent_client_protocol::Error::resource_not_found(Some(reference.clone()))
            })?;
        let stored_cwd = SessionManager::open(&path)
            .map_err(internal_error)?
            .cwd()
            .to_path_buf();
        if stored_cwd != cwd {
            return Err(agent_client_protocol::Error::invalid_params().data(format!(
                "session {id} belongs to {}, not {}",
                stored_cwd.display(),
                cwd.display()
            )));
        }
        let (opened_id, session) =
            self.build_session(cwd, SessionSource::Open(path), mcp_servers)?;
        if &opened_id != id {
            return Err(agent_client_protocol::Error::invalid_params()
                .data(format!("session id {id} resolved to {opened_id}")));
        }
        Ok(session)
    }

    fn session_directory(&self) -> PathBuf {
        self.options
            .session_dir
            .clone()
            .unwrap_or_else(default_session_dir)
    }

    fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, agent_client_protocol::Error> {
        const PAGE_SIZE: usize = 100;

        if self.options.session == SessionSource::InMemory {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        if request.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("session list cwd must be an absolute path"));
        }
        let directory = self.session_directory();
        let listings = match request.cwd.as_deref() {
            Some(cwd) => SessionManager::list(cwd, &directory),
            None => SessionManager::list_all(&directory),
        }
        .map_err(internal_error)?;
        let mut sessions = listings
            .into_iter()
            .filter_map(|listing| {
                let cwd = PathBuf::from(listing.cwd);
                cwd.is_absolute().then(|| {
                    SessionInfo::new(listing.id, cwd)
                        .title(listing.name.or(listing.first_message))
                        .updated_at(
                            chrono::DateTime::<chrono::Utc>::from(listing.modified)
                                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        )
                })
            })
            .collect::<Vec<_>>();
        for (id, session) in self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .iter()
        {
            if request.cwd.as_ref().is_some_and(|cwd| cwd != &session.cwd)
                || sessions.iter().any(|listed| &listed.session_id == id)
            {
                continue;
            }
            sessions.insert(0, SessionInfo::new(id.clone(), session.cwd.clone()));
        }
        let offset = request
            .cursor
            .as_deref()
            .map(|cursor| cursor.strip_prefix("kiss:").unwrap_or("").parse::<usize>())
            .transpose()
            .map_err(|_| {
                agent_client_protocol::Error::invalid_params().data("invalid session list cursor")
            })?
            .unwrap_or(0);
        if offset > sessions.len() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("session list cursor is out of range"));
        }
        let end = offset.saturating_add(PAGE_SIZE).min(sessions.len());
        let next = (end < sessions.len()).then(|| format!("kiss:{end}"));
        Ok(
            ListSessionsResponse::new(sessions.into_iter().skip(offset).take(PAGE_SIZE).collect())
                .next_cursor(next),
        )
    }

    fn delete_session(&self, id: &SessionId) -> Result<(), agent_client_protocol::Error> {
        if self.options.session == SessionSource::InMemory {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let active = {
            let mut sessions = self.sessions.lock().expect("ACP session map poisoned");
            if sessions
                .get(id)
                .is_some_and(|session| session.agent.is_running())
            {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("cancel or close the active prompt before deleting its session"));
            }
            let session = sessions.remove(id);
            if let Some(session) = &session {
                session.closed.store(true, Ordering::Release);
            }
            session
        };
        if let Some(session) = &active {
            session.agent.abort();
        }
        drop(active);

        let path = SessionManager::list_all(&self.session_directory())
            .map_err(internal_error)?
            .into_iter()
            .find(|listing| listing.id == id.to_string())
            .map(|listing| listing.path);
        if let Some(path) = path
            && let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(internal_error(error));
        }
        Ok(())
    }

    fn set_config_option(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, agent_client_protocol::Error> {
        let session = self.session(&request.session_id)?;
        if session.agent.is_running() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("session configuration cannot change during a prompt"));
        }
        let value = request.value.as_value_id().ok_or_else(|| {
            agent_client_protocol::Error::invalid_params()
                .data("KISS ACP session options require a selected value")
        })?;
        match request.config_id.to_string().as_str() {
            "model" => {
                let value = value.to_string();
                let current = session.agent.model();
                let model = session
                    .agent
                    .registry
                    .available_models()
                    .into_iter()
                    .map(|(_, model)| model)
                    .find(|model| model_value(model) == value)
                    .cloned()
                    .or_else(|| (model_value(&current) == value).then_some(current))
                    .ok_or_else(|| {
                        agent_client_protocol::Error::invalid_params()
                            .data(format!("unknown or unavailable model: {value}"))
                    })?;
                let thinking = model.clamp_thinking_level(session.agent.thinking_level());
                session.agent.set_model(model);
                if thinking != session.agent.thinking_level() {
                    session.agent.set_thinking_level(thinking);
                }
            }
            "thought_level" => {
                let value = value.to_string();
                let level = ThinkingLevel::parse(&value).ok_or_else(|| {
                    agent_client_protocol::Error::invalid_params()
                        .data(format!("unknown thinking level: {value}"))
                })?;
                if !session
                    .agent
                    .model()
                    .supported_thinking_levels()
                    .contains(&level)
                {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data(format!("thinking level is not supported: {value}")));
                }
                session.agent.set_thinking_level(level);
            }
            other => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data(format!("unknown session configuration option: {other}")));
            }
        }
        Ok(SetSessionConfigOptionResponse::new(session_config_options(
            &session.agent,
        )))
    }

    fn initialize(
        &self,
        request: InitializeRequest,
        responder: Responder<InitializeResponse>,
    ) -> agent_client_protocol::Result<()> {
        let mut session_capabilities = SessionCapabilities::new()
            .resume(SessionResumeCapabilities::new())
            .close(SessionCloseCapabilities::new());
        if self.options.session != SessionSource::InMemory {
            session_capabilities = session_capabilities
                .list(SessionListCapabilities::new())
                .delete(SessionDeleteCapabilities::new());
        }
        let capabilities = AgentCapabilities::new()
            .load_session(true)
            .prompt_capabilities(PromptCapabilities::new().image(true).embedded_context(true))
            .mcp_capabilities(McpCapabilities::new().http(true))
            .session_capabilities(session_capabilities);
        responder.respond(
            InitializeResponse::new(negotiated_protocol_version(request.protocol_version))
                .agent_capabilities(capabilities)
                .agent_info(Implementation::new("kiss", env!("CARGO_PKG_VERSION")).title("KISS")),
        )
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        responder: Responder<PromptResponse>,
        connection: ConnectionTo<Client>,
    ) -> agent_client_protocol::Result<()> {
        let session = match self.session(&request.session_id) {
            Ok(session) => session,
            Err(error) => return responder.respond_with_error(error),
        };
        let message = match prompt_message(request.prompt) {
            Ok(message) => message,
            Err(error) => return responder.respond_with_error(error),
        };
        let session_id = request.session_id;
        session.cancelled.store(false, Ordering::Release);
        connection.clone().spawn(async move {
            let _guard = session.prompt.lock().await;
            if session.closed.load(Ordering::Acquire) {
                return responder.respond_with_error(
                    agent_client_protocol::Error::resource_not_found(Some(session_id.to_string())),
                );
            }
            if session.cancelled.swap(false, Ordering::AcqRel) {
                return responder.respond(PromptResponse::new(StopReason::Cancelled));
            }
            let mut receiver = session.events.lock().await;
            while receiver.try_recv().is_ok() {}
            let mode = session.agent.prompt_mode_for(&message.content.as_text());
            let run = session
                .agent
                .prompt_with_mode(vec![AgentMessage::User(message)], mode);
            tokio::pin!(run);
            let mut stop_reason = StopReason::EndTurn;
            let mut prompt_error = None;
            let mut snapshots = HashMap::new();
            loop {
                tokio::select! {
                    () = &mut run => {
                        while let Ok(event) = receiver.try_recv() {
                            forward_event(
                                event,
                                &session_id,
                                &session.cwd,
                                &connection,
                                &mut snapshots,
                                &mut stop_reason,
                                &mut prompt_error,
                            )?;
                        }
                        break;
                    }
                    event = receiver.recv() => {
                        let Some(event) = event else { break };
                        forward_event(
                            event,
                            &session_id,
                            &session.cwd,
                            &connection,
                            &mut snapshots,
                            &mut stop_reason,
                            &mut prompt_error,
                        )?;
                    }
                }
            }
            let (used, size) = session.agent.context_usage();
            send_update(
                &connection,
                &session_id,
                SessionUpdate::UsageUpdate(UsageUpdate::new(used, size)),
            )?;
            match prompt_error {
                Some(error) => responder.respond_with_error(internal_error(error)),
                None => responder.respond(PromptResponse::new(stop_reason)),
            }
        })
    }
}

impl ConnectTo<Client> for KissAcpAgent {
    async fn connect_to(self, client: impl ConnectTo<Agent>) -> agent_client_protocol::Result<()> {
        Agent
            .builder()
            .name("kiss")
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: InitializeRequest, responder, _connection| {
                        agent.initialize(request, responder)
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: NewSessionRequest, responder, _connection| match agent
                        .build_session(
                            request.cwd,
                            if agent.options.session == SessionSource::InMemory {
                                SessionSource::InMemory
                            } else {
                                SessionSource::Create
                            },
                            request.mcp_servers,
                        ) {
                        Ok((id, session)) => responder.respond(
                            NewSessionResponse::new(id)
                                .config_options(session_config_options(&session.agent)),
                        ),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: LoadSessionRequest, responder, connection| {
                        let session = match agent.open_session(
                            &request.session_id,
                            request.cwd,
                            request.mcp_servers,
                        ) {
                            Ok(session) => session,
                            Err(error) => return responder.respond_with_error(error),
                        };
                        replay_history(&request.session_id, &session.agent, &connection)?;
                        responder.respond(
                            LoadSessionResponse::new()
                                .config_options(session_config_options(&session.agent)),
                        )
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: ResumeSessionRequest, responder, _connection| match agent
                        .open_session(&request.session_id, request.cwd, request.mcp_servers)
                    {
                        Ok(session) => responder.respond(
                            ResumeSessionResponse::new()
                                .config_options(session_config_options(&session.agent)),
                        ),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: ListSessionsRequest, responder, _connection| match agent
                        .list_sessions(request)
                    {
                        Ok(response) => responder.respond(response),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: DeleteSessionRequest, responder, _connection| match agent
                        .delete_session(&request.session_id)
                    {
                        Ok(()) => responder.respond(DeleteSessionResponse::new()),
                        Err(error) => responder.respond_with_error(error),
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: SetSessionConfigOptionRequest, responder, _connection| {
                        match agent.set_config_option(request) {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: CloseSessionRequest, responder, _connection| {
                        if let Some(session) = agent
                            .sessions
                            .lock()
                            .expect("ACP session map poisoned")
                            .remove(&request.session_id)
                        {
                            session.closed.store(true, Ordering::Release);
                            session.agent.abort();
                        }
                        responder.respond(CloseSessionResponse::new())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let agent = self.clone();
                    async move |request: PromptRequest, responder, connection| {
                        agent.prompt(request, responder, connection).await
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                {
                    let agent = self;
                    async move |notification: CancelNotification, _connection| {
                        if let Ok(session) = agent.session(&notification.session_id) {
                            session.cancelled.store(true, Ordering::Release);
                            session.agent.abort();
                        }
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(client)
            .await
    }
}

pub async fn run(args: &Args) -> Result<i32> {
    let mut options = super::rpc::options_from_args(args)?;
    options.session = if args.no_session {
        SessionSource::InMemory
    } else {
        SessionSource::Create
    };
    KissAcpAgent::new(options).connect_to(Stdio::new()).await?;
    Ok(0)
}

fn prompt_message(prompt: Vec<ContentBlock>) -> Result<UserMessage, agent_client_protocol::Error> {
    let mut content = Vec::with_capacity(prompt.len());
    for block in prompt {
        match block {
            ContentBlock::Text(text) => content.push(KissContentBlock::text(text.text)),
            ContentBlock::Image(image) => content.push(KissContentBlock::Image {
                data: image.data,
                mime_type: image.mime_type,
            }),
            ContentBlock::ResourceLink(resource) => content.push(KissContentBlock::text(format!(
                "<resource name=\"{}\" uri=\"{}\" />",
                resource.name, resource.uri
            ))),
            ContentBlock::Resource(resource) => match resource.resource {
                EmbeddedResourceResource::TextResourceContents(resource) => {
                    content.push(KissContentBlock::text(format!(
                        "<resource uri=\"{}\">\n{}\n</resource>",
                        resource.uri, resource.text
                    )));
                }
                EmbeddedResourceResource::BlobResourceContents(resource) => {
                    content.push(KissContentBlock::text(format!(
                        "<resource uri=\"{}\" mime_type=\"{}\" encoding=\"base64\">\n{}\n</resource>",
                        resource.uri,
                        resource.mime_type.as_deref().unwrap_or("application/octet-stream"),
                        resource.blob
                    )));
                }
                _ => {
                    return Err(agent_client_protocol::Error::invalid_params()
                        .data("unsupported embedded resource type"));
                }
            },
            ContentBlock::Audio(_) => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("KISS does not advertise ACP audio prompt support"));
            }
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("unsupported prompt content type"));
            }
        }
    }
    if content.is_empty() {
        return Err(agent_client_protocol::Error::invalid_params().data("prompt is empty"));
    }
    Ok(UserMessage {
        content: UserContent::Blocks(content),
        timestamp: kiss_ai::now_ms(),
    })
}

fn negotiated_protocol_version(client_version: ProtocolVersion) -> ProtocolVersion {
    if client_version == ProtocolVersion::V1 {
        client_version
    } else {
        ProtocolVersion::LATEST
    }
}

fn model_value(model: &kiss_ai::Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn session_config_options(session: &AgentSession) -> Vec<SessionConfigOption> {
    let current_model = session.model();
    let current_value = model_value(&current_model);
    let mut model_options = session
        .registry
        .available_models()
        .into_iter()
        .map(|(_, model)| SessionConfigSelectOption::new(model_value(model), model.display_name()))
        .collect::<Vec<_>>();
    if !model_options
        .iter()
        .any(|option| option.value.to_string() == current_value)
    {
        model_options.insert(
            0,
            SessionConfigSelectOption::new(current_value.clone(), current_model.display_name()),
        );
    }
    let mut options = vec![
        SessionConfigOption::select("model", "Model", current_value, model_options)
            .category(SessionConfigOptionCategory::Model),
    ];
    if current_model.reasoning {
        options.push(
            SessionConfigOption::select(
                "thought_level",
                "Thinking level",
                session.thinking_level().as_str(),
                current_model
                    .supported_thinking_levels()
                    .into_iter()
                    .map(|level| SessionConfigSelectOption::new(level.as_str(), level.as_str()))
                    .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        );
    }
    options
}

fn acp_mcp_servers(
    servers: Vec<McpServer>,
) -> Result<BTreeMap<String, kiss_mcp::ServerEntry>, agent_client_protocol::Error> {
    let mut result = BTreeMap::new();
    for server in servers {
        let (name, entry) = match server {
            McpServer::Stdio(server) => {
                if !server.command.is_absolute() {
                    return Err(agent_client_protocol::Error::invalid_params().data(format!(
                        "MCP command must be absolute: {}",
                        server.command.display()
                    )));
                }
                (
                    server.name,
                    kiss_mcp::ServerEntry {
                        command: Some(server.command.display().to_string()),
                        args: server.args,
                        env: server
                            .env
                            .into_iter()
                            .map(|variable| (variable.name, variable.value))
                            .collect(),
                        ..Default::default()
                    },
                )
            }
            McpServer::Http(server) => (
                server.name,
                kiss_mcp::ServerEntry {
                    url: Some(server.url),
                    headers: server
                        .headers
                        .into_iter()
                        .map(|header| (header.name, header.value))
                        .collect(),
                    ..Default::default()
                },
            ),
            McpServer::Sse(server) => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data(format!("SSE MCP server '{}' is not supported", server.name)));
            }
            _ => {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("unsupported MCP server transport"));
            }
        };
        entry.validate(&name).map_err(internal_error)?;
        if result.insert(name.clone(), entry).is_some() {
            return Err(agent_client_protocol::Error::invalid_params()
                .data(format!("duplicate MCP server name: {name}")));
        }
    }
    Ok(result)
}

fn replay_history(
    session_id: &SessionId,
    session: &AgentSession,
    connection: &ConnectionTo<Client>,
) -> agent_client_protocol::Result<()> {
    let messages = session
        .manager
        .lock()
        .expect("session manager poisoned")
        .build_session_context()
        .messages;
    for message in messages {
        match message {
            AgentMessage::User(user) => {
                let blocks = match &user.content {
                    UserContent::Text(text) => vec![KissContentBlock::text(text)],
                    UserContent::Blocks(blocks) => blocks.clone(),
                };
                for block in &blocks {
                    if let Some(content) = output_content(block) {
                        send_update(
                            connection,
                            session_id,
                            SessionUpdate::UserMessageChunk(ContentChunk::new(content)),
                        )?;
                    }
                }
            }
            AgentMessage::Assistant(assistant) => {
                for block in &assistant.content {
                    if let Some(content) = output_content(block) {
                        send_update(
                            connection,
                            session_id,
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(content)),
                        )?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn forward_event(
    event: AcpEvent,
    session_id: &SessionId,
    cwd: &Path,
    connection: &ConnectionTo<Client>,
    snapshots: &mut HashMap<String, (PathBuf, Option<String>)>,
    stop_reason: &mut StopReason,
    prompt_error: &mut Option<String>,
) -> agent_client_protocol::Result<()> {
    let AcpEvent::Session(event) = event else {
        if let AcpEvent::MutationSnapshot { id, path, old_text } = event {
            snapshots.insert(id, (path, old_text));
        }
        return Ok(());
    };
    let SessionEvent::Agent(event) = event else {
        return Ok(());
    };
    match event.as_ref() {
        AgentEvent::MessageUpdate {
            assistant_event, ..
        } => match assistant_event.as_ref() {
            AssistantEvent::TextDelta { delta, .. } => send_update(
                connection,
                session_id,
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(delta),
                ))),
            ),
            AssistantEvent::ThinkingDelta { delta, .. } => send_update(
                connection,
                session_id,
                SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new(delta),
                ))),
            ),
            AssistantEvent::ToolCallStart { tool_call, .. } => send_update(
                connection,
                session_id,
                SessionUpdate::ToolCall(
                    ToolCall::new(tool_call.id.clone(), tool_title(&tool_call.name))
                        .kind(tool_kind(&tool_call.name)),
                ),
            ),
            _ => Ok(()),
        },
        AgentEvent::MessageEnd {
            message: AgentMessage::Assistant(message),
        } => {
            match prompt_stop_reason(message.stop_reason, message.error_message.as_deref()) {
                Ok(reason) => *stop_reason = reason,
                Err(error) => *prompt_error = Some(error),
            }
            Ok(())
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => send_update(
            connection,
            session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .kind(tool_kind(tool_name))
                    .locations(tool_locations(args, cwd))
                    .raw_input(args.clone()),
            )),
        ),
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            partial,
            ..
        } => send_update(
            connection,
            session_id,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new()
                    .content(tool_content(partial, None))
                    .raw_output(partial.details.clone()),
            )),
        ),
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            result,
            is_error,
            ..
        } => {
            let diff = snapshots.remove(tool_call_id).and_then(|(path, old_text)| {
                std::fs::read_to_string(&path)
                    .ok()
                    .map(|new_text| Diff::new(path, new_text).old_text(old_text))
            });
            send_update(
                connection,
                session_id,
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id.clone(),
                    ToolCallUpdateFields::new()
                        .status(if *is_error {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        })
                        .content(tool_content(result, diff))
                        .raw_output(result.details.clone()),
                )),
            )
        }
        _ => Ok(()),
    }
}

fn prompt_stop_reason(
    reason: kiss_ai::StopReason,
    error_message: Option<&str>,
) -> Result<StopReason, String> {
    match reason {
        kiss_ai::StopReason::Length => Ok(StopReason::MaxTokens),
        kiss_ai::StopReason::Aborted => Ok(StopReason::Cancelled),
        kiss_ai::StopReason::Error => Err(error_message.unwrap_or("model request failed").into()),
        _ => Ok(StopReason::EndTurn),
    }
}

fn send_update(
    connection: &ConnectionTo<Client>,
    session_id: &SessionId,
    update: SessionUpdate,
) -> agent_client_protocol::Result<()> {
    connection.send_notification(SessionNotification::new(session_id.clone(), update))
}

fn output_content(block: &KissContentBlock) -> Option<ContentBlock> {
    match block {
        KissContentBlock::Text { text, .. } | KissContentBlock::Thinking { thinking: text, .. } => {
            Some(ContentBlock::Text(TextContent::new(text)))
        }
        KissContentBlock::Image { data, mime_type } => Some(ContentBlock::Image(
            agent_client_protocol::schema::v1::ImageContent::new(data, mime_type),
        )),
        KissContentBlock::ToolCall(_) => None,
    }
}

fn tool_content(result: &ToolResult, diff: Option<Diff>) -> Vec<ToolCallContent> {
    let mut content: Vec<ToolCallContent> = result
        .content
        .iter()
        .filter_map(output_content)
        .map(ToolCallContent::from)
        .collect();
    if let Some(diff) = diff {
        content.push(diff.into());
    } else if let Some(patch) = result.details.get("diff").and_then(Value::as_str) {
        content.push(ContentBlock::Text(TextContent::new(patch)).into());
    }
    content
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "ls" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "grep" | "find" => ToolKind::Search,
        "bash" => ToolKind::Execute,
        "mcp" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_title(name: &str) -> String {
    match name {
        "read" => "Read file",
        "write" => "Write file",
        "edit" => "Edit file",
        "bash" => "Run command",
        "grep" => "Search text",
        "find" => "Find files",
        "ls" => "List directory",
        "mcp" => "Use MCP server",
        other => other,
    }
    .to_string()
}

fn tool_path(args: &Value, cwd: &Path) -> Option<PathBuf> {
    let path = args
        .get("path")
        .or_else(|| args.get("filePath"))
        .and_then(Value::as_str)?;
    let path = PathBuf::from(path);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn tool_locations(args: &Value, cwd: &Path) -> Vec<ToolCallLocation> {
    tool_path(args, cwd)
        .map(ToolCallLocation::new)
        .into_iter()
        .collect()
}

fn internal_error(error: impl std::fmt::Display) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        BlobResourceContents, DeleteSessionRequest, EmbeddedResource, ImageContent,
        InitializeRequest, ListSessionsRequest, ResourceLink, SessionConfigKind,
        SetSessionConfigOptionRequest, TextResourceContents,
    };
    use agent_client_protocol::{Client, SessionMessage};
    use kiss_sdk::mock::{MockProvider, MockScript, MockTurn};
    use serde_json::json;

    #[test]
    fn prompt_keeps_images_and_embeds_resources() {
        let message = prompt_message(vec![
            ContentBlock::Text(TextContent::new("inspect")),
            ContentBlock::Image(ImageContent::new("aGVsbG8=", "image/png")),
            ContentBlock::ResourceLink(ResourceLink::new("main", "file:///work/main.rs")),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                    "fn main() {}",
                    "file:///work/main.rs",
                )),
            )),
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(BlobResourceContents::new(
                    "AAE=",
                    "file:///work/data.bin",
                )),
            )),
        ])
        .unwrap();
        let UserContent::Blocks(blocks) = &message.content else {
            panic!("typed prompt blocks");
        };
        assert!(matches!(blocks[1], KissContentBlock::Image { .. }));
        let text = message.content.as_text();
        assert!(text.contains("file:///work/main.rs"));
        assert!(text.contains("fn main() {}"));
        assert!(text.contains("AAE="));
    }

    #[test]
    fn tool_paths_are_absolute_and_kinds_are_specific() {
        let cwd = Path::new("/work/project");
        let locations = tool_locations(&json!({"path": "src/main.rs"}), cwd);
        assert_eq!(locations[0].path, Path::new("/work/project/src/main.rs"));
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
    }

    #[test]
    fn edit_results_prefer_structured_diffs() {
        let result = ToolResult {
            content: vec![KissContentBlock::text("edited")],
            details: json!({"diff": "fallback patch"}),
            ..Default::default()
        };
        let content = tool_content(
            &result,
            Some(Diff::new("/work/main.rs", "new").old_text("old")),
        );
        assert!(matches!(content[1], ToolCallContent::Diff(_)));
    }

    #[test]
    fn model_errors_are_not_reported_as_refusals() {
        assert_eq!(
            prompt_stop_reason(kiss_ai::StopReason::Length, None).unwrap(),
            StopReason::MaxTokens
        );
        assert_eq!(
            prompt_stop_reason(kiss_ai::StopReason::Error, Some("provider failed")).unwrap_err(),
            "provider failed"
        );
    }

    #[test]
    fn unsupported_protocol_versions_negotiate_to_stable_v1() {
        assert_eq!(
            negotiated_protocol_version(ProtocolVersion::V0),
            ProtocolVersion::V1
        );
        assert_eq!(
            negotiated_protocol_version(ProtocolVersion::V1),
            ProtocolVersion::V1
        );
    }

    #[test]
    fn mcp_declarations_map_without_losing_environment_or_headers() {
        let servers = acp_mcp_servers(vec![McpServer::Stdio(
            agent_client_protocol::schema::v1::McpServerStdio::new("local", "/bin/tool")
                .args(vec!["serve".into()])
                .env(vec![agent_client_protocol::schema::v1::EnvVariable::new(
                    "TOKEN", "value",
                )]),
        )])
        .unwrap();
        let server = &servers["local"];
        assert_eq!(server.command.as_deref(), Some("/bin/tool"));
        assert_eq!(server.args, ["serve"]);
        assert_eq!(server.env["TOKEN"], "value");
    }

    #[test]
    fn mcp_declarations_reject_relative_commands_and_duplicate_names() {
        let relative = McpServer::Stdio(agent_client_protocol::schema::v1::McpServerStdio::new(
            "local", "tool",
        ));
        assert!(acp_mcp_servers(vec![relative]).is_err());

        let server = || {
            McpServer::Stdio(agent_client_protocol::schema::v1::McpServerStdio::new(
                "local",
                "/bin/tool",
            ))
        };
        assert!(acp_mcp_servers(vec![server(), server()]).is_err());
    }

    #[tokio::test]
    async fn official_client_streams_a_native_kiss_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("hello from ACP"))
            .await
            .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let cwd = directory.path().to_path_buf();

        let run = Client
            .builder()
            .connect_with(agent, async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(initialized.agent_capabilities.load_session);
                assert!(initialized.agent_capabilities.prompt_capabilities.image);
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .list
                        .is_none()
                );
                assert!(
                    connection
                        .send_request(ListSessionsRequest::new())
                        .block_task()
                        .await
                        .is_err()
                );

                let mut session = connection
                    .build_session(cwd)
                    .block_task()
                    .start_session()
                    .await?;
                session.send_prompt("say hello")?;
                assert_eq!(session.read_to_string().await?, "hello from ACP");
                Ok(())
            });
        run.await.unwrap();
    }

    #[tokio::test]
    async fn close_prevents_a_queued_prompt_from_starting() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("must not run"))
            .await
            .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let probe = agent.clone();
        let cwd = directory.path().to_path_buf();

        let result = Client
            .builder()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let mut session = connection
                    .build_session(cwd)
                    .block_task()
                    .start_session()
                    .await?;
                let active = probe.session(session.session_id())?;
                let guard = active.prompt.lock().await;
                session.send_prompt("do not start")?;
                connection
                    .send_request(CloseSessionRequest::new(session.session_id().clone()))
                    .block_task()
                    .await?;
                drop(guard);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(())
            })
            .await;
        assert!(result.is_err());
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn cancel_stops_a_prompt_before_it_starts() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("must not run"))
            .await
            .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let probe = agent.clone();
        let cwd = directory.path().to_path_buf();

        let run = Client
            .builder()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let mut session = connection
                    .build_session(cwd)
                    .block_task()
                    .start_session()
                    .await?;
                let active = probe.session(session.session_id())?;
                let guard = active.prompt.lock().await;
                session.send_prompt("do not start")?;
                session
                    .connection()
                    .send_notification(CancelNotification::new(session.session_id().clone()))?;
                drop(guard);
                loop {
                    if let SessionMessage::StopReason(reason) = session.read_update().await? {
                        assert_eq!(reason, StopReason::Cancelled);
                        break;
                    }
                }
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("queued ACP cancellation timed out")
            .unwrap();
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn cancellation_returns_the_acp_cancelled_stop_reason() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(
            directory.path(),
            MockScript {
                turns: vec![
                    vec![MockTurn::ToolCall {
                        id: "slow".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "sleep 5"}),
                    }],
                    vec![MockTurn::Text("should not finish".into())],
                ],
            },
        )
        .await
        .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let cwd = directory.path().to_path_buf();

        let run = Client
            .builder()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let mut session = connection
                    .build_session(cwd)
                    .block_task()
                    .start_session()
                    .await?;
                session.send_prompt("run slowly")?;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                session
                    .connection()
                    .send_notification(CancelNotification::new(session.session_id().clone()))?;
                loop {
                    if let SessionMessage::StopReason(reason) = session.read_update().await? {
                        assert_eq!(reason, StopReason::Cancelled);
                        break;
                    }
                }
                Ok(())
            });
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("ACP cancellation timed out")
            .unwrap();
    }

    #[tokio::test]
    async fn load_reopens_persistent_history_and_replays_it() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("persisted answer"))
            .await
            .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::Create,
            session_dir: Some(directory.path().join("sessions")),
            no_context_files: true,
            ..Default::default()
        });
        let cwd = directory.path().to_path_buf();
        let replayed: Arc<Mutex<Vec<SessionUpdate>>> = Default::default();
        let sink = replayed.clone();

        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    sink.lock().unwrap().push(notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(&cwd))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(&cwd))
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(listed.sessions[0].session_id, created.session_id);
                connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("persist this"))],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new(created.session_id.clone()))
                    .block_task()
                    .await?;
                replayed.lock().unwrap().clear();

                connection
                    .send_request(LoadSessionRequest::new(created.session_id, cwd))
                    .block_task()
                    .await?;
                let updates = replayed.lock().unwrap();
                assert!(
                    updates
                        .iter()
                        .any(|update| matches!(update, SessionUpdate::UserMessageChunk(_)))
                );
                assert!(
                    updates
                        .iter()
                        .any(|update| matches!(update, SessionUpdate::AgentMessageChunk(_)))
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stable_session_list_and_delete_manage_persistent_history() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("listed answer"))
            .await
            .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::Create,
            session_dir: Some(directory.path().join("sessions")),
            no_context_files: true,
            ..Default::default()
        });
        let cwd = directory.path().to_path_buf();

        Client
            .builder()
            .connect_with(agent, async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .list
                        .is_some()
                );
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .delete
                        .is_some()
                );

                let created = connection
                    .send_request(NewSessionRequest::new(&cwd))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        created.session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new("list this session"))],
                    ))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new(created.session_id.clone()))
                    .block_task()
                    .await?;

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(cwd))
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(listed.sessions[0].session_id, created.session_id);
                assert_eq!(
                    listed.sessions[0].title.as_deref(),
                    Some("list this session")
                );
                assert!(listed.sessions[0].updated_at.is_some());

                connection
                    .send_request(DeleteSessionRequest::new(created.session_id.clone()))
                    .block_task()
                    .await?;
                connection
                    .send_request(DeleteSessionRequest::new(created.session_id))
                    .block_task()
                    .await?;
                let listed = connection
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert!(listed.sessions.is_empty());
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stable_config_options_change_model_and_thinking_level() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(directory.path(), MockScript::text("configured"))
            .await
            .unwrap();
        let mut catalog: Value =
            serde_json::from_str(&std::fs::read_to_string(provider.catalog_path()).unwrap())
                .unwrap();
        catalog["providers"]["mock"]["models"] = json!([
            {
                "id": "mock-1",
                "name": "Mock 1",
                "reasoning": true,
                "contextWindow": 128000,
                "maxTokens": 4096
            },
            {
                "id": "mock-2",
                "name": "Mock 2",
                "reasoning": true,
                "contextWindow": 128000,
                "maxTokens": 4096
            }
        ]);
        std::fs::write(provider.catalog_path(), catalog.to_string()).unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let probe = agent.clone();
        let cwd = directory.path().to_path_buf();

        Client
            .builder()
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await?;
                assert_eq!(created.config_options.as_ref().map(Vec::len), Some(2));

                let configured = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        created.session_id.clone(),
                        "model",
                        "mock/mock-2",
                    ))
                    .block_task()
                    .await?;
                let model = configured
                    .config_options
                    .iter()
                    .find(|option| option.id.to_string() == "model")
                    .unwrap();
                let SessionConfigKind::Select(model) = &model.kind else {
                    panic!("model is a select option");
                };
                assert_eq!(model.current_value.to_string(), "mock/mock-2");

                let configured = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        created.session_id,
                        "thought_level",
                        "high",
                    ))
                    .block_task()
                    .await?;
                let thinking = configured
                    .config_options
                    .iter()
                    .find(|option| option.id.to_string() == "thought_level")
                    .unwrap();
                let SessionConfigKind::Select(thinking) = &thinking.kind else {
                    panic!("thinking level is a select option");
                };
                assert_eq!(thinking.current_value.to_string(), "high");
                Ok(())
            })
            .await
            .unwrap();

        let session = probe
            .sessions
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(session.agent.model().id, "mock-2");
        assert_eq!(session.agent.thinking_level(), ThinkingLevel::High);
    }

    #[tokio::test]
    async fn tool_execution_streams_status_location_and_structured_diff() {
        let directory = tempfile::tempdir().unwrap();
        let provider = MockProvider::start(
            directory.path(),
            MockScript {
                turns: vec![
                    vec![MockTurn::ToolCall {
                        id: "write-1".into(),
                        name: "write".into(),
                        arguments: json!({"path": "created.txt", "content": "from ACP\n"}),
                    }],
                    vec![MockTurn::Text("done".into())],
                ],
            },
        )
        .await
        .unwrap();
        let agent = KissAcpAgent::new(SessionOptions {
            model: Some("mock/mock-1".into()),
            models_file: Some(provider.catalog_path()),
            settings: Some(kiss_coding::Settings::default()),
            session: SessionSource::InMemory,
            no_context_files: true,
            ..Default::default()
        });
        let cwd = directory.path().to_path_buf();
        let updates: Arc<Mutex<Vec<SessionUpdate>>> = Default::default();
        let sink = updates.clone();

        Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    sink.lock().unwrap().push(notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(&cwd))
                    .block_task()
                    .await?;
                connection
                    .send_request(PromptRequest::new(
                        created.session_id,
                        vec![ContentBlock::Text(TextContent::new("write a file"))],
                    ))
                    .block_task()
                    .await?;

                assert_eq!(
                    std::fs::read_to_string(cwd.join("created.txt")).unwrap(),
                    "from ACP\n"
                );
                let updates = updates.lock().unwrap();
                assert!(updates.iter().any(|update| {
                    matches!(update, SessionUpdate::ToolCall(call) if call.tool_call_id.0.as_ref() == "write-1")
                }));
                assert!(updates.iter().any(|update| {
                    matches!(
                        update,
                        SessionUpdate::ToolCallUpdate(update)
                            if update.fields.status == Some(ToolCallStatus::InProgress)
                                && update.fields.locations.as_ref().is_some_and(|locations| locations.iter().any(|location| location.path == cwd.join("created.txt")))
                    )
                }));
                assert!(updates.iter().any(|update| {
                    matches!(
                        update,
                        SessionUpdate::ToolCallUpdate(update)
                            if update.fields.status == Some(ToolCallStatus::Completed)
                                && update.fields.content.as_ref().is_some_and(|content| content.iter().any(|item| matches!(item, ToolCallContent::Diff(_))))
                    )
                }));
                assert!(updates.iter().any(|update| {
                    matches!(
                        update,
                        SessionUpdate::UsageUpdate(usage)
                            if usage.used > 0 && usage.size == 128_000
                    )
                }));
                Ok(())
            })
            .await
            .unwrap();
    }

    #[test]
    #[ignore = "performance benchmark"]
    fn acp_translation() {
        kiss_bench::measure(
            "acp_text_delta_translation",
            20,
            100_000,
            "one delta; no prior-delta copy",
            || {
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("delta"),
                )))
            },
        );
    }
}
