//! The `/resume` session browser.
//!
//! It draws a framed panel with a search field, a scope switch, a list of
//! two-row session cards with relative times, and a key hint row. The panel
//! keeps one height while the user types, so the rows above and below it do
//! not move with each key press.

use crate::session_sources::SessionRecord;
use chrono::Datelike;
use kiss_tui::{CURSOR_MARKER, Key, KeyEvent, Theme, text};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Two-row session cards in the panel: 19 rows in all with the frame.
const CARDS: usize = 6;

#[derive(Debug)]
pub(crate) enum SessionPickerAction {
    None,
    Close,
    Resume(SessionRecord),
    ToggleScope,
    Rename { path: PathBuf, name: String },
}

pub(crate) struct SessionPicker {
    records: Vec<SessionRecord>,
    pub(crate) global: bool,
    current: Option<PathBuf>,
    pub(crate) query: String,
    /// Record indices that match `query`, newest first.
    matches: Vec<usize>,
    /// Position in `matches`.
    selected: usize,
    /// First visible position in `matches`.
    scroll: usize,
    /// The new name while an inline rename is open.
    rename: Option<String>,
    /// A one-line message on the status row, such as a rename result.
    pub(crate) notice: Option<String>,
}

impl SessionPicker {
    pub(crate) fn new(
        records: Vec<SessionRecord>,
        global: bool,
        current: Option<PathBuf>,
        query: String,
    ) -> Self {
        let mut picker = Self {
            records,
            global,
            current,
            query,
            matches: Vec::new(),
            selected: 0,
            scroll: 0,
            rename: None,
            notice: None,
        };
        picker.refilter();
        picker
    }

    /// Show a new name in the list after the app saved it.
    pub(crate) fn renamed(&mut self, path: &Path, name: &str) {
        for record in &mut self.records {
            if record.source.kiss_path() == Some(path) {
                record.title = name.to_string();
            }
        }
        self.notice = Some(format!("Renamed to “{name}”"));
    }

    /// Every whitespace-separated term must appear in the title, the
    /// directory, or the source name. Order stays newest first, which is
    /// more predictable for sessions than a fuzzy score.
    fn refilter(&mut self) {
        let terms: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        self.matches = self
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                let haystack = format!(
                    "{}\n{}\n{}",
                    record.title,
                    record.cwd.display(),
                    record.source.name()
                )
                .to_lowercase();
                terms.iter().all(|term| haystack.contains(term.as_str()))
            })
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
        self.scroll = 0;
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1) as isize;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> SessionPickerAction {
        self.notice = None;
        if let Some(name) = &mut self.rename {
            match key.key {
                Key::Escape => self.rename = None,
                Key::Enter => {
                    let name = name.trim().to_string();
                    let path = self
                        .matches
                        .get(self.selected)
                        .and_then(|index| self.records[*index].source.kiss_path())
                        .map(Path::to_path_buf);
                    if name.is_empty() {
                        self.notice = Some(
                            "Type a name with at least one character, or press esc to cancel"
                                .into(),
                        );
                    } else if let Some(path) = path {
                        self.rename = None;
                        return SessionPickerAction::Rename { path, name };
                    }
                }
                _ => {
                    kiss_tui::edit_single_line(name, key);
                }
            }
            return SessionPickerAction::None;
        }

        let page = self.card_count() as isize;
        match key.key {
            Key::Escape if !self.query.is_empty() => {
                self.query.clear();
                self.refilter();
            }
            Key::Escape => return SessionPickerAction::Close,
            Key::Char('c') if key.ctrl => return SessionPickerAction::Close,
            Key::Enter => {
                if let Some(index) = self.matches.get(self.selected) {
                    return SessionPickerAction::Resume(self.records[*index].clone());
                }
            }
            Key::Tab | Key::BackTab => return SessionPickerAction::ToggleScope,
            Key::Char('g') if key.ctrl => return SessionPickerAction::ToggleScope,
            Key::Char('r') if key.ctrl => match self.matches.get(self.selected) {
                Some(index) if self.records[*index].source.kiss_path().is_some() => {
                    self.rename = Some(self.records[*index].title.clone());
                }
                Some(index) => {
                    self.notice = Some(format!(
                        "{} sessions are read-only here. Resume it first, then rename the KISS copy",
                        self.records[*index].source.name()
                    ));
                }
                None => {}
            },
            Key::Up => self.move_by(-1),
            Key::Down => self.move_by(1),
            Key::Char('p') if key.ctrl => self.move_by(-1),
            Key::Char('n') if key.ctrl => self.move_by(1),
            Key::PageUp => self.move_by(-page),
            Key::PageDown => self.move_by(page),
            Key::Home => self.selected = 0,
            Key::End => self.selected = self.matches.len().saturating_sub(1),
            _ => {
                if kiss_tui::edit_single_line(&mut self.query, key) {
                    self.refilter();
                }
            }
        }
        SessionPickerAction::None
    }

    /// Based on all records, not the matches, so the panel does not shrink
    /// while the user types.
    fn card_count(&self) -> usize {
        CARDS.min(self.records.len().max(1))
    }

    pub(crate) fn render(&mut self, width: usize, theme: &Theme) -> Vec<String> {
        let width = width.max(24);
        let inner = width - 4;
        let cards = self.card_count();
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + cards {
            self.scroll = self.selected + 1 - cards;
        }

        let border = |text: &str| theme.fg("border", text);
        let row = |content: &str| {
            format!(
                "{} {} {}",
                border("│"),
                text::fit_to_width(content, inner),
                border("│")
            )
        };
        let mut lines = Vec::with_capacity(7 + cards * 2);

        let title = " Resume a session ";
        lines.push(format!(
            "{}{}{}",
            border("╭─"),
            theme.fg("accent", &theme.bold(title)),
            border(&format!(
                "{}╮",
                "─".repeat(width.saturating_sub(3 + text::display_width(title)))
            ))
        ));

        // Search field on the left, the scope switch on the right.
        let scopes = [
            ("This project", !self.global),
            ("All projects", self.global),
        ];
        let scope_text = scopes
            .iter()
            .map(|(label, active)| {
                if *active {
                    theme.fg("accent", &theme.bold(&format!("● {label}")))
                } else {
                    theme.fg("dim", &format!("○ {label}"))
                }
            })
            .collect::<Vec<_>>()
            .join("  ");
        let scope_width = text::display_width(&scope_text);
        let field = match &self.rename {
            Some(name) => format!(
                "{} {}{CURSOR_MARKER}",
                theme.fg("warning", "✎ Rename:"),
                text::truncate_to_width(name, inner.saturating_sub(12))
            ),
            None if self.query.is_empty() => format!(
                "{} {CURSOR_MARKER}{}",
                theme.fg("accent", "⌕"),
                theme.fg("dim", "Search titles and folders")
            ),
            None => format!(
                "{} {}{CURSOR_MARKER}",
                theme.fg("accent", "⌕"),
                text::truncate_to_width(&self.query, inner.saturating_sub(scope_width + 4))
            ),
        };
        let field_width = text::display_width(&field);
        if self.rename.is_none() && field_width + scope_width + 2 <= inner {
            lines.push(row(&format!(
                "{field}{}{scope_text}",
                " ".repeat(inner - field_width - scope_width)
            )));
        } else {
            lines.push(row(&field));
        }
        lines.push(row(""));

        let empty = if !self.records.is_empty() {
            format!("No sessions match “{}”", self.query)
        } else if self.global {
            "No saved sessions yet".to_string()
        } else {
            "No sessions in this project. Press tab to see all projects".to_string()
        };
        let visible = &self.matches[self.scroll.min(self.matches.len())..];
        for slot in 0..cards {
            let Some(&index) = visible.get(slot) else {
                let message = if slot == 0 { empty.as_str() } else { "" };
                lines.push(row(
                    &theme.fg("muted", &text::truncate_to_width(message, inner))
                ));
                lines.push(row(""));
                continue;
            };
            let selected = self.scroll + slot == self.selected;
            lines.extend(
                self.card(&self.records[index], selected, inner, theme)
                    .into_iter()
                    .map(|content| row(&content)),
            );
        }
        lines.push(row(""));

        let status = match &self.notice {
            Some(notice) => theme.fg("warning", &text::truncate_to_width(notice, inner)),
            None => {
                let count = if self.query.is_empty() {
                    format!("{} sessions", self.records.len())
                } else {
                    format!("{} of {} sessions", self.matches.len(), self.records.len())
                };
                let position = if self.matches.len() > cards {
                    format!(
                        "{}{} {}/{}",
                        if self.scroll > 0 { "▲" } else { " " },
                        if self.scroll + cards < self.matches.len() {
                            "▼"
                        } else {
                            " "
                        },
                        self.selected + 1,
                        self.matches.len()
                    )
                } else {
                    String::new()
                };
                let gap = inner
                    .saturating_sub(text::display_width(&count) + text::display_width(&position));
                theme.fg("dim", &format!("{count}{}{position}", " ".repeat(gap)))
            }
        };
        lines.push(row(&status));
        lines.push(border(&format!("╰{}╯", "─".repeat(width - 2))));

        let other_scope = if self.global {
            "this project"
        } else {
            "all projects"
        };
        let keys: &[(&str, &str)] = if self.rename.is_some() {
            &[("enter", "save"), ("esc", "cancel")]
        } else {
            &[
                ("↑↓", "select"),
                ("enter", "resume"),
                ("tab", other_scope),
                ("ctrl+r", "rename"),
                ("esc", "close"),
            ]
        };
        let hints = keys
            .iter()
            .map(|(key, action)| {
                format!(
                    "{} {}",
                    theme.fg("accent", &theme.bold(key)),
                    theme.fg("dim", action)
                )
            })
            .collect::<Vec<_>>()
            .join("   ");
        lines.push(text::truncate_to_width(&format!(" {hints}"), width));
        lines
    }

    /// Two rows: the title with its age on the right, then the source and
    /// details.
    fn card(
        &self,
        record: &SessionRecord,
        selected: bool,
        inner: usize,
        theme: &Theme,
    ) -> [String; 2] {
        let age = relative_time(record.modified, SystemTime::now());
        let right = format!("{age:>9}");
        let right_width = text::display_width(&right);
        let title_width = inner.saturating_sub(right_width + 4).max(8);
        let title = text::truncate_to_width(&record.title, title_width);
        let title = highlight_terms(&title, &self.query, theme, selected);
        let gap = inner.saturating_sub(2 + text::display_width(&title) + right_width);
        let bar = if selected {
            theme.fg("accent", "▌ ")
        } else {
            "  ".to_string()
        };
        let first = if gap >= 2 {
            format!(
                "{bar}{title}{}{}",
                " ".repeat(gap),
                theme.fg("muted", &right)
            )
        } else {
            format!("{bar}{title}")
        };

        let current = record
            .source
            .kiss_path()
            .is_some_and(|path| self.current.as_deref() == Some(path));
        let mut details = vec![record.source.name().to_string()];
        if self.global {
            details.push(crate::modes::interactive::shorten_path(
                &record.cwd.display().to_string(),
            ));
        }
        if let Some(count) = record.entry_count {
            details.push(format!(
                "{count} {}",
                if count == 1 { "entry" } else { "entries" }
            ));
        }
        if selected && record.source.kiss_path().is_none() {
            details.push("resumes as a new KISS copy".into());
        }
        let mut second = format!(
            "{bar}{}",
            theme.fg(
                "muted",
                &text::truncate_to_width(&details.join(" · "), inner.saturating_sub(12))
            )
        );
        if current {
            second.push_str(&theme.fg("success", "  ● current"));
        }
        let background = theme.color("selectedBg").bg_code();
        [first, second].map(|line| {
            if selected {
                format!("{background}{}\x1b[49m", text::fit_to_width(&line, inner))
            } else {
                line
            }
        })
    }
}

/// Bold the query terms inside an already truncated, plain title.
fn highlight_terms(title: &str, query: &str, theme: &Theme, selected: bool) -> String {
    let base = |text: &str| {
        if selected {
            theme.bold(text)
        } else {
            theme.fg("text", text)
        }
    };
    let lower = title.to_lowercase();
    // Byte positions from the lower-case copy only map back when lowering
    // keeps every character's length.
    if query.trim().is_empty() || lower.len() != title.len() {
        return base(title);
    }
    let mut marked = vec![false; title.len()];
    for term in query.split_whitespace().map(str::to_lowercase) {
        for (start, _) in lower.match_indices(term.as_str()) {
            marked[start..start + term.len()].fill(true);
        }
    }
    let mut out = String::new();
    let mut start = 0;
    while start < title.len() {
        let hit = marked[start];
        let mut end = start;
        while end < title.len() && marked[end] == hit {
            end += 1;
        }
        while !title.is_char_boundary(end) {
            end += 1;
        }
        let part = &title[start..end];
        out.push_str(&if hit {
            theme.fg("accent", &theme.bold(part))
        } else {
            base(part)
        });
        start = end;
    }
    out
}

fn relative_time(then: SystemTime, now: SystemTime) -> String {
    let age = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    match age {
        0..60 => "just now".into(),
        60..3_600 => format!("{}m ago", age / 60),
        3_600..86_400 => format!("{}h ago", age / 3_600),
        86_400..172_800 => "yesterday".into(),
        172_800..604_800 => format!("{}d ago", age / 86_400),
        604_800..2_419_200 => format!("{}w ago", age / 604_800),
        _ => {
            let then = chrono::DateTime::<chrono::Local>::from(then);
            let now = chrono::DateTime::<chrono::Local>::from(now);
            then.format(if then.year() == now.year() {
                "%b %-d"
            } else {
                "%b %Y"
            })
            .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_sources::SessionSource;

    fn record(title: &str, cwd: &str, minutes_ago: u64) -> SessionRecord {
        SessionRecord {
            source: SessionSource::Kiss(PathBuf::from(format!("/sessions/{title}.jsonl"))),
            id: title.into(),
            title: title.into(),
            cwd: PathBuf::from(cwd),
            modified: SystemTime::now() - Duration::from_secs(minutes_ago * 60),
            entry_count: Some(3),
        }
    }

    fn picker() -> SessionPicker {
        SessionPicker::new(
            vec![
                record("Fix renderer flicker", "/work/kiss", 5),
                record("Add OAuth login", "/work/kiss", 90),
                record("Write release notes", "/work/docs", 3_000),
            ],
            true,
            None,
            String::new(),
        )
    }

    fn plain(picker: &mut SessionPicker, width: usize) -> Vec<String> {
        picker
            .render(width, &Theme::dark())
            .iter()
            .map(|line| text::strip_ansi(line).replace(CURSOR_MARKER, ""))
            .collect()
    }

    fn press(key: Key) -> KeyEvent {
        KeyEvent {
            key,
            ..Default::default()
        }
    }

    fn type_text(picker: &mut SessionPicker, text: &str) {
        for character in text.chars() {
            picker.handle_key(&KeyEvent::char(character));
        }
    }

    #[test]
    fn search_matches_every_term_and_keeps_the_panel_height() {
        let mut picker = picker();
        let height = plain(&mut picker, 80).len();

        type_text(&mut picker, "kiss oauth");
        assert_eq!(picker.matches, vec![1]);
        assert_eq!(plain(&mut picker, 80).len(), height);

        type_text(&mut picker, "zzz");
        let rendered = plain(&mut picker, 80);
        assert_eq!(rendered.len(), height);
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("No sessions match"))
        );
        assert!(matches!(
            picker.handle_key(&press(Key::Enter)),
            SessionPickerAction::None
        ));
    }

    #[test]
    fn escape_clears_the_search_before_it_closes() {
        let mut picker = picker();
        let escape = KeyEvent {
            key: Key::Escape,
            ..Default::default()
        };
        type_text(&mut picker, "docs");
        assert!(matches!(
            picker.handle_key(&escape),
            SessionPickerAction::None
        ));
        assert!(picker.query.is_empty());
        assert_eq!(picker.matches.len(), 3);
        assert!(matches!(
            picker.handle_key(&escape),
            SessionPickerAction::Close
        ));
    }

    #[test]
    fn rows_fit_the_width_and_show_age_and_folder() {
        let mut picker = picker();
        picker.handle_key(&press(Key::Down));
        for width in [30, 80, 140] {
            for line in picker.render(width, &Theme::dark()) {
                assert!(text::display_width(&line) <= width.max(24), "{line:?}");
            }
        }
        let rendered = plain(&mut picker, 100).join("\n");
        assert!(rendered.contains("5m ago"));
        assert!(rendered.contains("/work/docs"));
        assert!(rendered.contains("▌ Add OAuth login"));
        assert!(matches!(
            picker.handle_key(&press(Key::Enter)),
            SessionPickerAction::Resume(record) if record.title == "Add OAuth login"
        ));
    }

    #[test]
    fn inline_rename_returns_the_new_name() {
        let mut picker = picker();
        picker.handle_key(&KeyEvent::ctrl('r'));
        picker.handle_key(&KeyEvent::ctrl('u'));
        type_text(&mut picker, "Renderer work");
        assert!(
            plain(&mut picker, 80)
                .iter()
                .any(|line| line.contains("✎ Rename: Renderer work"))
        );
        let action = picker.handle_key(&press(Key::Enter));
        assert!(matches!(
            action,
            SessionPickerAction::Rename { path, name }
                if path == Path::new("/sessions/Fix renderer flicker.jsonl") && name == "Renderer work"
        ));
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = SystemTime::now();
        let ago = |seconds| relative_time(now - Duration::from_secs(seconds), now);
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(90_000), "yesterday");
        assert_eq!(ago(3 * 86_400), "3d ago");
    }
}
