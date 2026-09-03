//! kiss-sdk: embed the KISS coding agent in your own program.
//!
//! # What this crate gives you
//!
//! A [`Session`] owns one conversation with the agent: its message history, its
//! model, its tools, and its streaming events. You build one, subscribe to its
//! events, and send it prompts.
//!
//! (This snippet is checked as a compiled example on [`Session`]; it is shown
//! here without compilation because the crate can also be built without the
//! `native` feature, where `Session` does not exist.)
//!
//! ```ignore
//! let session = kiss_sdk::Session::builder()
//!     .tools(["read", "bash"])
//!     .build()
//!     .await?;
//!
//! let mut events = session.events();
//! tokio::spawn(async move {
//!     while let Ok(event) = events.recv().await {
//!         println!("{}", event.to_line());
//!     }
//! });
//!
//! session.prompt("List the files here").await?;
//! ```
//!
//! # One protocol, four surfaces
//!
//! Every operation is also a [`protocol::Command`], and every command is handled
//! by exactly one function, [`Session::execute`]. The Python binding, the
//! TypeScript binding, and the RPC server in [`rpc`] all call that same
//! function, so the three SDKs and the language-neutral protocol cannot drift
//! apart. If you need something the typed helpers do not expose, build the
//! command yourself and call `execute`.
//!
//! # Feature flags
//!
//! * `native` (default) — the in-process agent. Needs a filesystem.
//! * `rpc` — the JSON-line server over stdin/stdout, TCP, or WebSocket.
//! * `mock` — a scripted HTTP mock model provider for hermetic tests.
//!
//! With `--no-default-features` only [`protocol`] and [`client`] are compiled,
//! which is what the WebAssembly binding uses.

pub mod client;
pub mod protocol;

pub use client::{Client, LineBuffer};
pub use protocol::{
    Command, Event, ImageInput, Incoming, ProtocolError, QueueMode, Request, Response,
    StreamingBehavior,
};

#[cfg(feature = "mock")]
pub mod mock;

#[cfg(feature = "native")]
pub mod events;
#[cfg(feature = "native")]
pub mod options;
#[cfg(feature = "native")]
pub mod session;
#[cfg(feature = "native")]
pub mod tools;

#[cfg(feature = "rpc")]
pub mod rpc;

#[cfg(feature = "native")]
pub use options::{SessionOptions, SessionSource};
#[cfg(feature = "native")]
pub use session::{BashResult, EventStream, PromptArgs, SdkError, Session, SessionBuilder};
