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
use std::rc::Rc;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::*;
use web_sys::{CloseEvent, ErrorEvent, Event, MessageEvent, WebSocket};

type PromiseCallbacks = (Function, Function);

struct Inner {
    protocol: RefCell<Client>,
    pending: RefCell<HashMap<String, PromiseCallbacks>>,
    on_event: RefCell<Option<Function>>,
}

/// A connection to one native KISS RPC session.
#[wasm_bindgen]
pub struct KissClient {
    socket: WebSocket,
    inner: Rc<Inner>,
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
                protocol: RefCell::new(Client::new()),
                pending: RefCell::new(HashMap::new()),
                on_event: RefCell::new(None),
            });
            install_message_handler(&socket, inner.clone());
            install_close_handler(&socket, inner.clone());

            let open_socket = socket.clone();
            let open_inner = inner.clone();
            let on_open = Closure::<dyn FnMut(Event)>::once(move |_event: Event| {
                let client = KissClient {
                    socket: open_socket,
                    inner: open_inner,
                };
                let _ = resolve.call1(&JsValue::NULL, &JsValue::from(client));
            });
            socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            on_open.forget();

            let on_error = Closure::<dyn FnMut(ErrorEvent)>::once(move |event: ErrorEvent| {
                let message = if event.message().is_empty() {
                    "WebSocket connection failed".to_string()
                } else {
                    event.message()
                };
                let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new(&message));
            });
            socket.set_onerror(Some(on_error.as_ref().unchecked_ref()));
            on_error.forget();
        })
    }

    /// Run one protocol command. `command` is a plain JavaScript object and the
    /// promise resolves to the plain response object with the matching id.
    pub fn execute(&self, command: JsValue) -> Promise {
        let command: Command = match serde_wasm_bindgen::from_value(command) {
            Ok(command) => command,
            Err(error) => return Promise::reject(&js_sys::Error::new(&error.to_string())),
        };
        let (id, line) = self.inner.protocol.borrow_mut().encode(command);
        let socket = self.socket.clone();
        let inner = self.inner.clone();
        Promise::new(&mut move |resolve: Function, reject: Function| {
            inner
                .pending
                .borrow_mut()
                .insert(id.clone(), (resolve, reject.clone()));
            if let Err(error) = socket.send_with_str(&line) {
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

    /// Receive event objects. The callback is invoked once per event and never
    /// for command responses.
    #[wasm_bindgen(js_name = onEvent)]
    pub fn on_event(&self, callback: Function) {
        *self.inner.on_event.borrow_mut() = Some(callback);
    }

    /// Stop delivering events to the current callback.
    #[wasm_bindgen(js_name = clearEventHandler)]
    pub fn clear_event_handler(&self) {
        *self.inner.on_event.borrow_mut() = None;
    }

    /// Close the WebSocket. Every pending command promise rejects.
    pub fn close(&self) {
        let _ = self.socket.close();
    }
}

fn install_message_handler(socket: &WebSocket, inner: Rc<Inner>) {
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(line) = event.data().as_string() else {
            return;
        };
        match inner.protocol.borrow().decode(&line) {
            Ok(Incoming::Response(response)) => handle_response(&inner, response),
            Ok(Incoming::Event(event)) => {
                if let Some(callback) = inner.on_event.borrow().as_ref()
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
    on_message.forget();
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

fn install_close_handler(socket: &WebSocket, inner: Rc<Inner>) {
    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        let reason = if event.reason().is_empty() {
            format!("KISS WebSocket closed with code {}", event.code())
        } else {
            event.reason()
        };
        for (_, (_, reject)) in inner.pending.borrow_mut().drain() {
            let _ = reject.call1(&JsValue::NULL, &js_sys::Error::new(&reason));
        }
        *inner.on_event.borrow_mut() = None;
    });
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
    on_close.forget();
}
