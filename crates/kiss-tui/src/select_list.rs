//! Filterable, scrollable selection list (model picker, session picker,
//! settings, tree view).

use crate::component::Component;
use crate::text::{display_width, fit_to_width, truncate_to_width};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub label: String,
    pub detail: Option<String>,
    /// Opaque payload index for the caller.
    pub value: usize,
}

pub struct SelectList {
    pub items: Vec<SelectItem>,
    pub filter: String,
    pub selected: usize,
    pub max_visible: usize,
    pub title: String,
    theme: Theme,
    filtered_cache: Option<Vec<usize>>,
}

impl SelectList {
    pub fn new(title: impl Into<String>, items: Vec<SelectItem>, theme: Theme) -> Self {
        SelectList {
            items,
            filter: String::new(),
            selected: 0,
            max_visible: 10,
            title: title.into(),
            theme,
            filtered_cache: None,
        }
    }

    pub fn filtered_indices(&mut self) -> Vec<usize> {
        if let Some(cache) = &self.filtered_cache {
            return cache.clone();
        }
        let indices: Vec<usize> = if self.filter.is_empty() {
            (0..self.items.len()).collect()
        } else {
            let labels: Vec<&str> = self.items.iter().map(|i| i.label.as_str()).collect();
            crate::fuzzy::fuzzy_rank(&self.filter, labels.iter().copied())
                .into_iter()
                .filter_map(|(label, _)| self.items.iter().position(|i| i.label == label))
                .collect()
        };
        self.filtered_cache = Some(indices.clone());
        indices
    }

    pub fn move_selection(&mut self, delta: isize) {
        let count = self.filtered_indices().len();
        if count == 0 {
            return;
        }
        let new = (self.selected as isize + delta).rem_euclid(count as isize);
        self.selected = new as usize;
    }

    pub fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.selected = 0;
        self.filtered_cache = None;
    }

    pub fn current(&mut self) -> Option<&SelectItem> {
        let indices = self.filtered_indices();
        let idx = *indices.get(self.selected)?;
        self.items.get(idx)
    }

    /// Render only selectable rows. This is used for editor-attached
    /// completion menus, where the editor already shows the active filter.
    pub fn render_compact(&mut self, width: usize, label_prefix: &str) -> Vec<String> {
        let theme = self.theme.clone();
        let indices = self.filtered_indices();
        if indices.is_empty() {
            return vec![theme.fg("dim", "  No matching commands")];
        }

        let visible = self.max_visible.min(indices.len());
        let start = self
            .selected
            .saturating_sub(visible / 2)
            .min(indices.len().saturating_sub(visible));
        let widest = indices
            .iter()
            .map(|index| display_width(&format!("{label_prefix}{}", self.items[*index].label)))
            .max()
            .unwrap_or(0);
        // Use the available row width for long primary labels. File
        // completion puts the base name here and the full path in detail.
        let primary_width = (widest + 2).max(12).min(width.max(1));
        let mut lines = Vec::with_capacity(visible + 1);

        for (row, &item_idx) in indices[start..start + visible].iter().enumerate() {
            let absolute = start + row;
            let item = &self.items[item_idx];
            let marker = if absolute == self.selected {
                "→ "
            } else {
                "  "
            };
            let primary = format!("{label_prefix}{}", item.label);
            let primary = truncate_to_width(&primary, primary_width.saturating_sub(2));
            let mut line = format!("{marker}{primary}");
            if width > 40
                && let Some(detail) = &item.detail
            {
                let used = display_width(&line);
                let padding = primary_width.saturating_add(2).saturating_sub(used).max(1);
                let remaining = width.saturating_sub(used + padding);
                if remaining > 10 {
                    line.push_str(&" ".repeat(padding));
                    line.push_str(&theme.fg("muted", &truncate_to_width(detail, remaining)));
                }
            }
            let line = if display_width(&line) > width {
                truncate_to_width(&crate::text::strip_ansi(&line), width)
            } else {
                line
            };
            if absolute == self.selected {
                lines.push(theme.fg("accent", &line));
            } else {
                lines.push(line);
            }
        }
        if indices.len() > visible {
            lines.push(theme.fg(
                "dim",
                &format!("  ({}/{})", self.selected + 1, indices.len()),
            ));
        }
        lines
    }
}

impl Component for SelectList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let theme = self.theme.clone();
        lines.push(theme.fg(
            "accent",
            &theme.bold(&truncate_to_width(&self.title, width)),
        ));
        if !self.filter.is_empty() {
            lines.push(theme.fg(
                "muted",
                &truncate_to_width(&format!("filter: {}", self.filter), width),
            ));
        }
        let indices = self.filtered_indices();
        if indices.is_empty() {
            lines.push(theme.fg("dim", "(no matches)"));
            return lines;
        }
        let visible = self.max_visible.min(indices.len());
        // Scroll window centered on selection.
        let start = self
            .selected
            .saturating_sub(visible / 2)
            .min(indices.len().saturating_sub(visible));
        for (row, &item_idx) in indices[start..start + visible].iter().enumerate() {
            let absolute = start + row;
            let item = &self.items[item_idx];
            let marker = if absolute == self.selected {
                "→ "
            } else {
                "  "
            };
            let mut label = format!("{marker}{}", item.label);
            if let Some(detail) = &item.detail {
                label.push_str(&format!("  {detail}"));
            }
            let line = truncate_to_width(&label, width);
            if absolute == self.selected {
                lines.push(format!(
                    "{}{}\x1b[49m",
                    theme.color("selectedBg").bg_code(),
                    fit_to_width(&line, width)
                ));
            } else {
                lines.push(line);
            }
        }
        if indices.len() > visible {
            lines.push(theme.fg("dim", &format!("({}/{})", self.selected + 1, indices.len())));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> SelectList {
        let items = (0..3)
            .map(|i| SelectItem {
                label: format!("item-{i}"),
                detail: None,
                value: i,
            })
            .collect();
        SelectList::new("Pick", items, Theme::dark())
    }

    #[test]
    fn selection_wraps() {
        let mut l = list();
        l.move_selection(-1);
        assert_eq!(l.selected, 2);
        l.move_selection(1);
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn filter_narrows() {
        let mut l = list();
        l.set_filter("item-2".into());
        assert_eq!(l.filtered_indices(), vec![2]);
        assert_eq!(l.current().unwrap().value, 2);
    }

    #[test]
    fn renders_within_width() {
        let mut l = list();
        for line in l.render(20) {
            assert!(crate::text::display_width(&line) <= 20);
        }
    }

    #[test]
    fn compact_render_has_no_title_or_filter_row() {
        let mut list = list();
        list.set_filter("item".into());
        let rendered = list.render_compact(60, "");
        let plain: Vec<String> = rendered
            .iter()
            .map(|line| crate::text::strip_ansi(line))
            .collect();
        assert!(plain[0].contains("item-0"));
        assert!(plain.iter().all(|line| !line.contains("Pick")));
        assert!(plain.iter().all(|line| !line.contains("filter:")));
    }

    #[test]
    fn compact_render_keeps_a_file_name_when_the_row_has_room() {
        let file_name = "a_complete_file_name_that_is_longer_than_32.rs";
        let mut list = SelectList::new(
            "Files",
            vec![SelectItem {
                label: file_name.into(),
                detail: Some(format!("src/generated/{file_name}")),
                value: 0,
            }],
            Theme::dark(),
        );
        let rendered = list.render_compact(80, "");
        let plain = crate::text::strip_ansi(&rendered[0]);
        assert!(plain.contains(file_name));
    }
}
