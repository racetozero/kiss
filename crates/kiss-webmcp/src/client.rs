use anyhow::{Context as _, Result, bail};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
type PendingRequests = Arc<Mutex<RequestState>>;

enum RequestState {
    Open(HashMap<u64, oneshot::Sender<Result<Value, String>>>),
    Closed,
}

impl RequestState {
    fn remove(&mut self, id: u64) -> Option<oneshot::Sender<Result<Value, String>>> {
        match self {
            Self::Open(pending) => pending.remove(&id),
            Self::Closed => None,
        }
    }

    fn close(&mut self) {
        if let Self::Open(pending) = std::mem::replace(self, Self::Closed) {
            for (_, reply) in pending {
                let _ = reply.send(Err("Chrome connection closed".into()));
            }
        }
    }
}

struct PendingRequest {
    pending: PendingRequests,
    id: u64,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(self.id);
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct CdpClient {
    inner: Arc<Inner>,
}

struct Inner {
    outgoing: mpsc::UnboundedSender<Message>,
    pending: PendingRequests,
    events: broadcast::Sender<CdpEvent>,
    next_id: AtomicU64,
    task: Mutex<Option<JoinHandle<()>>>,
    closed: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.closed.cancel();
        self.pending.lock().unwrap().close();
        if let Some(task) = self.task.get_mut().ok().and_then(Option::take) {
            task.abort();
        }
    }
}

impl CdpClient {
    pub async fn connect(url: &str) -> Result<Self> {
        let (socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connect to Chrome at {url}"))?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel();
        let pending = Arc::new(Mutex::new(RequestState::Open(HashMap::new())));
        let task_pending = pending.clone();
        let (events, _) = broadcast::channel(256);
        let task_events = events.clone();
        let closed = CancellationToken::new();
        let task_closed = closed.clone();

        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    message = outgoing_rx.recv() => {
                        let Some(message) = message else { break };
                        if sink.send(message).await.is_err() {
                            break;
                        }
                    }
                    message = stream.next() => {
                        let Some(Ok(message)) = message else { break };
                        let text = match message {
                            Message::Text(text) => text.to_string(),
                            Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                                Ok(text) => text,
                                Err(_) => continue,
                            },
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let Ok(value) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if let Some(id) = value.get("id").and_then(Value::as_u64) {
                            if let Some(reply) = task_pending.lock().unwrap().remove(id) {
                                let result = match value.get("error") {
                                    Some(error) => Err(error.to_string()),
                                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                                };
                                let _ = reply.send(result);
                            }
                        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
                            let _ = task_events.send(CdpEvent {
                                method: method.to_string(),
                                params: value.get("params").cloned().unwrap_or_else(|| json!({})),
                                session_id: value
                                    .get("sessionId")
                                    .and_then(Value::as_str)
                                    .map(str::to_string),
                            });
                        }
                    }
                }
            }

            task_pending.lock().unwrap().close();
            task_closed.cancel();
        });

        Ok(Self {
            inner: Arc::new(Inner {
                outgoing,
                pending,
                events,
                next_id: AtomicU64::new(1),
                task: Mutex::new(Some(task)),
                closed,
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
    }

    pub fn closed(&self) -> CancellationToken {
        self.inner.closed.clone()
    }

    pub async fn send(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            message["sessionId"] = Value::String(session_id.to_string());
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut state = self.inner.pending.lock().unwrap();
            let RequestState::Open(pending) = &mut *state else {
                bail!(
                    "Chrome connection is closed. Connect to Chrome again before you send {method}."
                );
            };
            pending.insert(id, reply_tx);
        }
        let _request = PendingRequest {
            pending: self.inner.pending.clone(),
            id,
        };
        if self
            .inner
            .outgoing
            .send(Message::Text(message.to_string().into()))
            .is_err()
        {
            bail!("Chrome connection is closed");
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("Chrome rejected {method}: {error}"),
            Ok(Err(_)) => bail!("Chrome connection closed during {method}"),
            Err(_) => {
                bail!("Chrome did not answer {method} within 10 seconds")
            }
        }
    }

    pub fn close(&self) {
        self.inner.pending.lock().unwrap().close();
        self.inner.closed.cancel();
        let _ = self.inner.outgoing.send(Message::Close(None));
        if let Some(task) = self.inner.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_closed_connection_rejects_requests_without_waiting_for_a_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while socket.next().await.is_some() {}
        });
        let client = CdpClient::connect(&format!("ws://{address}"))
            .await
            .unwrap();
        client.close();
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            client.send("Target.getTargets", json!({}), None),
        )
        .await;
        fixture.abort();
        assert!(
            result
                .expect("a closed connection must reject immediately")
                .is_err()
        );
    }
}
