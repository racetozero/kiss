//! kiss-tui: differential-rendering terminal UI library.
//!
//! Components render to `Vec<String>`; the `DiffRenderer` compares frames
//! and repaints only changed rows on the terminal main screen. Nothing else
//! writes escape sequences.

pub mod component;
pub mod editor;
pub mod fuzzy;
pub mod keybindings;
pub mod keys;
pub mod markdown;
pub mod renderer;
pub mod select_list;
pub mod terminal;
pub mod text;
pub mod theme;

pub use component::{Component, Spacer, TextBlock};
pub use editor::{Editor, EditorSubmission};
pub use keybindings::{Action, Keybindings};
pub use keys::{InputDecoder, InputEvent, Key, KeyEvent};
pub use markdown::{MarkdownRenderer, MermaidMode, StreamingMarkdownCache};
pub use renderer::{CURSOR_MARKER, DiffRenderer};
pub use select_list::{SelectItem, SelectList};
pub use terminal::Terminal;
pub use theme::{Color, Theme};
