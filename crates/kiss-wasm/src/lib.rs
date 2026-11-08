//! Browser WebAssembly client for `kiss --mode rpc --rpc-listen`.
//!
//! The complete native coding agent cannot honestly run inside a browser: its
//! core tools need operating-system files and process spawning, which browser
//! sandboxes intentionally prohibit. This crate therefore compiles the shared
//! protocol and client state machine to WebAssembly and connects to a native
//! KISS agent over WebSocket. The web page remains sandboxed while the agent
//! runs with the permissions of the explicit `kiss` process the user started.

use js_sys::{Function, Promise};
use kiss_sdk::client::Client;
use kiss_sdk::protocol::{Command, Incoming, Response};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};

type PromiseCallbacks = (Function, Function);

struct Inner {
    socket: WebSocket,
    connection: RefCell<ConnectionState>,
    protocol: RefCell<Client>,
    pending: RefCell<HashMap<String, PromiseCallbacks>>,
    on_event: RefCell<Option<Function>>,
    connecting: RefCell<Option<Rc<Inner>>>,
    handlers: RefCell<Option<SocketHandlers>>,
}

enum ConnectionState {
    Connecting { resolve: Function, reject: Function },
    Open,
    Closed,
}

struct SocketHandlers {
    _open: Closure<dyn FnMut(Event)>,
    _error: Closure<dyn FnMut(ErrorEvent)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
    _close: Closure<dyn FnMut(CloseEvent)>,
}

impl Inner {
    fn close(&self, reason: &str) {
        let previous = self.connection.replace(ConnectionState::Closed);
        let pending = std::mem::take(&mut *self.pending.borrow_mut());
        *self.on_event.borrow_mut() = None;
        self.connecting.borrow_mut().take();
        let error = js_sys::Error::new(reason);
        if let ConnectionState::Connecting { reject, .. } = previous {
            let _ = reject.call1(&JsValue::NULL, &error);
        }
        for (_, (_, reject)) in pending {
            let _ = reject.call1(&JsValue::NULL, &error);
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onerror(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

/// A connection to one native KISS RPC session.
#[wasm_bindgen]
pub struct KissClient {
    inner: Rc<Inner>,
}

impl Drop for KissClient {
    fn drop(&mut self) {
        self.inner
            .close("The client was freed. Connect to KISS again before you send a command.");
    }
}

#[wasm_bindgen]
impl KissClient {
    /// Connect to an RPC WebSocket, resolving only once it is open.
    ///
    /// Start the other side with, for example:
    /// `kiss --mode rpc --rpc-listen 127.0.0.1:9944 --no-session`.
    #[wasm_bindgen(js_name = connect)]
    pub fn connect(url: String) -> Promise {
        Promise::new(&mut move |resolve: Function, reject: Function| {
            let socket = match WebSocket::new(&url) {
                Ok(socket) => socket,
                Err(error) => {
                    let _ = reject.call1(&JsValue::NULL, &error);
                    return;
                }
            };
            socket.set_binary_type(web_sys::BinaryType::Arraybuffer);
            let inner = Rc::new(Inner {
                socket,
                connection: RefCell::new(ConnectionState::Connecting { resolve, reject }),
                protocol: RefCell::new(Client::new()),
                pending: RefCell::new(HashMap::new()),
                on_event: RefCell::new(None),
                connecting: RefCell::new(None),
                handlers: RefCell::new(None),
            });
            // Keep setup alive until it settles. Installed handlers hold weak
            // references, so an open or closed client has no ownership cycle.
            *inner.connecting.borrow_mut() = Some(inner.clone());
            let open_inner = Rc::downgrade(&inner);
            let on_open = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                let Some(inner) = open_inner.upgrade() else {
                    return;
                };
                let resolve = {
                    let mut state = inner.connection.borrow_mut();
                    if !matches!(*state, ConnectionState::Connecting { .. }) {
                        return;
                    }
                    let ConnectionState::Connecting { resolve, .. } =
                        std::mem::replace(&mut *state, ConnectionState::Open)
                    else {
                        unreachable!("only a connecting client can open");
                    };
                    resolve
                };
                inner.connecting.borrow_mut().take();
                let client = KissClient { inner };
                let _ = resolve.call1(&JsValue::NULL, &JsValue::from(client));
            });
            inner
                .socket
                .set_onopen(Some(on_open.as_ref().unchecked_ref()));
            let error_inner = Rc::downgrade(&inner);
            let on_error = Closure::<dyn FnMut(ErrorEvent)>::new(move |event: ErrorEvent| {
                let Some(inner) = error_inner.upgrade() else {
                    return;
                };
                let message = if event.message().is_empty() {
                    "WebSocket connection failed".to_string()
                } else {
                    event.message()
                };
                inner.close(&message);
            });
            inner
                .socket
                .set_onerror(Some(on_error.as_ref().unchecked_ref()));
            *inner.handlers.borrow_mut() = Some(SocketHandlers {
                _open: on_open,
                _error: on_error,
                _message: install_message_handler(&inner.socket, Rc::downgrade(&inner)),
                _close: install_close_handler(&inner.socket, Rc::downgrade(&inner)),
            });
        })
    }

    /// Run one protocol command. `command` is a plain JavaScript object and the
    /// promise resolves to the plain response object with the matching id.
    pub fn execute(&self, command: JsValue) -> Promise {
        let command: Command = match serde_wasm_bindgen::from_value(command) {
            Ok(command) => command,
            Err(error) => return Promise::reject(&js_sys::Error::new(&error.to_string())),
        };
        if !matches!(*self.inner.connection.borrow(), ConnectionState::Open)
            || self.inner.socket.ready_state() != WebSocket::OPEN
        {
            self.inner.close(
                "The connection is closed. Connect to KISS again before you send a command.",
            );
            return Promise::reject(&js_sys::Error::new(
                "The connection is closed. Connect to KISS again before you send a command.",
            ));
        }
        let (id, line) = self.inner.protocol.borrow_mut().encode(command);
        let inner = self.inner.clone();
        Promise::new(&mut move |resolve: Function, reject: Function| {
            inner
                .pending
                .borrow_mut()
                .insert(id.clone(), (resolve, reject.clone()));
            if let Err(error) = inner.socket.send_with_str(&line) {
                inner.pending.borrow_mut().remove(&id);
                let _ = reject.call1(&JsValue::NULL, &error);
            }
        })
    }

    /// Send a prompt. The promise resolves when the native agent accepts it;
    /// listen for `agent_settled` to know the whole run has finished.
    pub fn prompt(&self, message: String) -> Promise {
        let command = serde_wasm_bindgen::to_value(&Command::Prompt {
            message,
            images: Vec::new(),
            streaming_behavior: None,
        })
        .expect("a prompt command serializes");
        self.execute(command)
    }

    /// Cancel the current operation.
    pub fn abort(&self) -> Promise {
        let command =
            serde_wasm_bindgen::to_value(&Command::Abort {}).expect("an abort command serializes");
        self.execute(command)
    }

    /// Return message counts, token usage, cache reads and writes, cost, and context use.
    #[wasm_bindgen(js_name = sessionStats)]
    pub fn session_stats(&self) -> Promise {
        let command = serde_wasm_bindgen::to_value(&Command::GetSessionStats {})
            .expect("a session statistics command serializes");
        self.execute(command)
    }

    /// Receive event objects. The callback is invoked once per event and never
    /// for command responses.
    #[wasm_bindgen(js_name = onEvent)]
    pub fn on_event(&self, callback: Function) {
        if matches!(*self.inner.connection.borrow(), ConnectionState::Open) {
            *self.inner.on_event.borrow_mut() = Some(callback);
        }
    }

    /// Stop delivering events to the current callback.
    #[wasm_bindgen(js_name = clearEventHandler)]
    pub fn clear_event_handler(&self) {
        *self.inner.on_event.borrow_mut() = None;
    }

    /// Close the WebSocket. Every pending command promise rejects.
    pub fn close(&self) {
        self.inner
            .close("The connection was closed. Connect to KISS again before you send a command.");
        let _ = self.inner.socket.close();
    }
}

fn install_message_handler(
    socket: &WebSocket,
    inner: Weak<Inner>,
) -> Closure<dyn FnMut(MessageEvent)> {
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(inner) = inner.upgrade() else { return };
        if !matches!(*inner.connection.borrow(), ConnectionState::Open) {
            return;
        }
        let Some(line) = event.data().as_string() else {
            return;
        };
        let incoming = inner.protocol.borrow().decode(&line);
        match incoming {
            Ok(Incoming::Response(response)) => handle_response(&inner, response),
            Ok(Incoming::Event(event)) => {
                let callback = inner.on_event.borrow().clone();
                if let Some(callback) = callback
                    && let Ok(value) = to_js_object(&event.0)
                {
                    let _ = callback.call1(&JsValue::NULL, &value);
                }
            }
            Err(error) => web_sys::console::error_1(&JsValue::from_str(&format!(
                "invalid KISS RPC message: {error}"
            ))),
        }
    });
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message
}

fn handle_response(inner: &Rc<Inner>, response: Response) {
    let Some(id) = response.id.clone() else {
        return;
    };
    let Some((resolve, reject)) = inner.pending.borrow_mut().remove(&id) else {
        return;
    };
    if response.success {
        match to_js_object(&response) {
            Ok(value) => {
                let _ = resolve.call1(&JsValue::NULL, &value);
            }
            Err(error) => {
                let _ = reject.call1(
                    &JsValue::NULL,
                    &js_sys::Error::new(&format!("could not decode response: {error}")),
                );
            }
        }
    } else {
        let message = response.error.as_deref().unwrap_or("KISS command failed");
        let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new(message));
    }
}

/// Serialize maps as ordinary JavaScript objects rather than ES `Map` values.
/// SDK callers expect `response.data.pong`, not `response.data.get("pong")`.
fn to_js_object<T: Serialize>(value: &T) -> Result<JsValue, serde_wasm_bindgen::Error> {
    value.serialize(&serde_wasm_bindgen::Serializer::json_compatible())
}

fn install_close_handler(socket: &WebSocket, inner: Weak<Inner>) -> Closure<dyn FnMut(CloseEvent)> {
    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        let Some(inner) = inner.upgrade() else { return };
        let reason = if event.reason().is_empty() {
            format!("KISS WebSocket closed with code {}", event.code())
        } else {
            event.reason()
        };
        inner.close(&reason);
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close
}
