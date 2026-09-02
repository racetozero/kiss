//! Deterministic workflow orchestration scripts.
//!
//! A workflow script says which agents to start, in what order, and what to do
//! with their answers. This crate parses such a script and runs it. It knows
//! nothing about sessions, models, or terminals: it reaches the outside world
//! only through the [`AgentRunner`] trait, so the whole language is testable
//! without a model.
//!
//! The language is a small, fixed subset of JavaScript syntax. It has no module
//! loading, no file access, no network access, and no host bindings beyond the
//! workflow builtins, because none of those were implemented. It is a workflow
//! orchestrator, not a general scripting runtime.
//!
//! A script is also deterministic: the clock and the random generator are
//! unavailable. That is what lets a stopped run resume, because replaying the
//! script issues the same sequence of `agent()` calls, so results kept from the
//! earlier run still line up.
//!
//! ```no_run
//! # use kiss_workflow::{AgentOutcome, AgentRequest, AgentRunner, Limits, Script, Workflow};
//! # use std::sync::Arc;
//! # use tokio_util::sync::CancellationToken;
//! # struct Host;
//! # #[async_trait::async_trait]
//! # impl AgentRunner for Host {
//! #     async fn run_agent(&self, _r: AgentRequest, _c: CancellationToken) -> AgentOutcome {
//! #         AgentOutcome::Done(serde_json::Value::String("ok".into()))
//! #     }
//! # }
//! # async fn example() -> anyhow::Result<()> {
//! let script = Script::parse(
//!     "export const meta = { name: 'sweep', description: 'Check two files' }\n\
//!      const found = await parallel([() => agent('check a.rs'), () => agent('check b.rs')])\n\
//!      return found.filter(Boolean)",
//! )?;
//! let workflow = Workflow::new(
//!     script,
//!     serde_json::Value::Null,
//!     ".".to_string(),
//!     Arc::new(Host),
//!     Limits::default(),
//! );
//! let report = workflow.run().await?;
//! # let _ = report;
//! # Ok(())
//! # }
//! ```

mod ast;
mod diagnostic;
mod interp;
mod lexer;
mod parser;
mod progress;
mod runner;
mod script;
mod value;
mod workflow;

pub use crate::diagnostic::Diagnostic;
pub use crate::interp::RunError;
pub use crate::progress::{AgentSnapshot, AgentStatus, PhaseSnapshot, RunSnapshot, RunStatus};
pub use crate::runner::{AgentId, AgentOutcome, AgentRequest, AgentRunner, Journal, Limits};
pub use crate::script::{Meta, Script};
pub use crate::workflow::Workflow;
