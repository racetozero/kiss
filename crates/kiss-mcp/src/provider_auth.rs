//! Streamable HTTP client for `"auth": { "provider": ... }`: send the current
//! token of a KISS provider login. The token is resolved on every request, so
//! provider refreshes apply.

use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::AuthError;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;
use std::collections::HashMap;
use std::sync::Arc;

type Error = <reqwest_mcp::Client as StreamableHttpClient>::Error;

#[derive(Clone)]
pub(crate) struct ProviderTokenClient {
    inner: reqwest_mcp::Client,
    provider: Arc<str>,
}

impl ProviderTokenClient {
    pub(crate) fn new(provider: &str) -> Self {
        Self {
            inner: reqwest_mcp::Client::default(),
            provider: provider.into(),
        }
    }

    async fn token(&self) -> Result<String, StreamableHttpError<Error>> {
        let provider = &*self.provider;
        let failure =
            |message: String| StreamableHttpError::Auth(AuthError::InternalError(message));
        match kiss_ai::auth::resolve_api_key_async(provider, &Default::default()).await {
            Ok(Some(token)) => Ok(token),
            Ok(None) => Err(failure(format!(
                "the MCP server uses the `{provider}` login, but you are not signed in to `{provider}`. Run /login {provider}, or kiss login {provider}"
            ))),
            Err(error) => Err(failure(format!(
                "could not get the `{provider}` login token for the MCP server: {error:#}"
            ))),
        }
    }
}

impl StreamableHttpClient for ProviderTokenClient {
    type Error = Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Error>> {
        let token = self.token().await?;
        self.inner
            .post_message(uri, message, session_id, Some(token), custom_headers)
            .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Error>> {
        let token = self.token().await?;
        self.inner
            .post_message_with_max_sse_event_size(
                uri,
                message,
                session_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Error>> {
        let token = self.token().await?;
        self.inner
            .delete_session(uri, session_id, Some(token), custom_headers)
            .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Error>> {
        let token = self.token().await?;
        self.inner
            .get_stream(uri, session_id, last_event_id, Some(token), custom_headers)
            .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Error>> {
        let token = self.token().await?;
        self.inner
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                Some(token),
                custom_headers,
                max_sse_event_size,
            )
            .await
    }
}
