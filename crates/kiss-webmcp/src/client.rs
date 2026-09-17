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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
type PendingRequests = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

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
}

impl Drop for Inner {
    fn drop(&mut self) {
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
        let pending = Arc::new(Mutex::new(HashMap::<
            u64,
            oneshot::Sender<Result<Value, String>>,
        >::new()));
        let task_pending = pending.clone();
        let (events, _) = broadcast::channel(256);
        let task_events = events.clone();

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
                            if let Some(reply) = task_pending.lock().unwrap().remove(&id) {
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

            for (_, reply) in task_pending.lock().unwrap().drain() {
                let _ = reply.send(Err("Chrome connection closed".into()));
            }
        });

        Ok(Self {
            inner: Arc::new(Inner {
                outgoing,
                pending,
                events,
                next_id: AtomicU64::new(1),
                task: Mutex::new(Some(task)),
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CdpEvent> {
        self.inner.events.subscribe()
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
        self.inner.pending.lock().unwrap().insert(id, reply_tx);
        if self
            .inner
            .outgoing
            .send(Message::Text(message.to_string().into()))
            .is_err()
        {
            self.inner.pending.lock().unwrap().remove(&id);
            bail!("Chrome connection is closed");
        }

        match tokio::time::timeout(REQUEST_TIMEOUT, reply_rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => bail!("Chrome rejected {method}: {error}"),
            Ok(Err(_)) => bail!("Chrome connection closed during {method}"),
            Err(_) => {
                self.inner.pending.lock().unwrap().remove(&id);
                bail!("Chrome did not answer {method} within 10 seconds")
            }
        }
    }

    pub async fn close(&self) {
        let _ = self.inner.outgoing.send(Message::Close(None));
        if let Some(task) = self.inner.task.lock().unwrap().take() {
            task.abort();
        }
        for (_, reply) in self.inner.pending.lock().unwrap().drain() {
            let _ = reply.send(Err("Chrome connection closed".into()));
        }
    }
}
