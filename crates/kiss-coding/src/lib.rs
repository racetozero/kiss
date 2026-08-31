//! kiss-coding: the coding harness — sessions, compaction, settings,
//! project context, skills, prompt templates, search tools, and the
//! AgentSession facade.

pub mod compaction;
pub mod context_files;
pub mod prompts;
pub mod session;
pub mod session_runner;
pub mod settings;
pub mod skills;
pub mod subagents;
pub mod system_prompt;
pub mod tools;
pub mod trust;

pub use session::entry::{SessionEntry, SessionHeader};
pub use session::manager::{SessionListing, SessionManager, default_session_dir};
pub use session_runner::{
    AgentSession, EphemeralResponse, SessionEvent, SessionEventSink, TreeNavigationOutcome,
};
pub use settings::Settings;
