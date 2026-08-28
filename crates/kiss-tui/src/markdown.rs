//! Terminal Markdown renderer (streaming-friendly: render is pure text-in,
//! lines-out, so callers can re-render partial documents cheaply).

use crate::text::wrap_text;
use crate::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MermaidMode {
    Off,
    Final,
    #[default]
    Streaming,
}

pub struct MarkdownRenderer {
    theme: Theme,
    pub code_indent: String,
}

#[derive(Default)]
pub struct StreamingMarkdownCache {
    stable_source: String,
    stable_lines: Vec<String>,
    width: usize,
    mermaid_mode: MermaidMode,
}

impl StreamingMarkdownCache {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Render a growing Markdown document and reuse complete top-level blocks.
    pub fn render(
        &mut self,
        renderer: &MarkdownRenderer,
        markdown: &str,
        width: usize,
        mermaid_mode: MermaidMode,
    ) -> Vec<String> {
        let stable_end = stable_prefix_len(markdown);
        let appended = markdown
            .strip_prefix(&self.stable_source)
            .unwrap_or(markdown);
        if contains_link_reference_definition(appended) {
            self.clear();
            return renderer.render_with_state(markdown, width, true, mermaid_mode);
        }
        let can_extend = self.width == width
            && self.mermaid_mode == mermaid_mode
            && stable_end >= self.stable_source.len()
            && markdown.starts_with(&self.stable_source);

        if !can_extend {
            self.stable_source.clear();
            self.stable_lines.clear();
        }
        if stable_end > self.stable_source.len() {
            let segment = &markdown[self.stable_source.len()..stable_end];
            let segment_lines = renderer.render_with_state(segment, width, true, mermaid_mode);
            append_markdown_block(&mut self.stable_lines, segment_lines);
            self.stable_source.push_str(segment);
        }
        self.width = width;
        self.mermaid_mode = mermaid_mode;

        let tail = renderer.render_with_state(&markdown[stable_end..], width, true, mermaid_mode);
        let mut lines = self.stable_lines.clone();
        append_markdown_block(&mut lines, tail);
        lines
    }
}

fn contains_link_reference_definition(markdown: &str) -> bool {
    markdown.lines().any(|original| {
        let line = original.trim_start_matches(' ');
        let indent = original.len().saturating_sub(line.len());
        indent <= 3
            && line
                .strip_prefix('[')
                .and_then(|rest| rest.find("]:").map(|end| &rest[..end]))
                .is_some_and(|label| !label.is_empty())
    })
}

fn append_markdown_block(lines: &mut Vec<String>, mut block: Vec<String>) {
    if !lines.is_empty() && !block.is_empty() {
        lines.push(String::new());
    }
    lines.append(&mut block);
}

fn stable_prefix_len(markdown: &str) -> usize {
    let parser = Parser::new_ext(
        markdown,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES,
    );
    let mut depth = 0usize;
    let mut stable_end = 0usize;
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let mut start = range.end;
                    while start > 0 && markdown.as_bytes()[start - 1] == b'\n' {
                        start -= 1;
                    }
                    let mut end = range.end;
                    while end < markdown.len()
                        && matches!(markdown.as_bytes()[end], b' ' | b'\t' | b'\r' | b'\n')
                    {
                        end += 1;
                    }
                    let newlines = markdown[start..end]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count();
                    if newlines >= 2 {
                        stable_end = end;
                    }
                }
            }
            _ => {}
        }
    }
    stable_end
}

impl MarkdownRenderer {
    pub fn new(theme: Theme) -> Self {
        MarkdownRenderer {
            theme,
            code_indent: "  ".into(),
        }
    }

    pub fn render(&self, markdown: &str, width: usize) -> Vec<String> {
        self.render_with_state(markdown, width, false, MermaidMode::Streaming)
    }

    pub fn render_with_state(
        &self,
        markdown: &str,
        width: usize,
        is_streaming: bool,
        mermaid_mode: MermaidMode,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_MATH;
        let parser = Parser::new_ext(markdown, options);

        let mut current = String::new();
        let mut styles: Vec<&'static str> = Vec::new();
        let mut list_stack: Vec<Option<u64>> = Vec::new();
        let mut in_code_block = false;
        let mut code_buffer = String::new();
        let mut code_language: Option<String> = None;
        let mut quote_depth = 0usize;
        let mut in_table = false;
        let mut table_row: Vec<String> = Vec::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut heading: Option<HeadingLevel> = None;

        let flush = |current: &mut String,
                     out: &mut Vec<String>,
                     quote_depth: usize,
                     width: usize,
                     theme: &Theme| {
            if current.is_empty() {
                return;
            }
            let prefix = if quote_depth > 0 {
                theme.fg("mdQuote", &"│ ".repeat(quote_depth))
            } else {
                String::new()
            };
            let prefix_width = 2 * quote_depth;
            for line in wrap_text(current, width.saturating_sub(prefix_width).max(10)) {
                out.push(format!("{prefix}{line}"));
            }
            current.clear();
        };

        for event in parser {
            match event {
                Event::Start(tag) => match tag {
                    Tag::Heading { level, .. } => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        if !out.is_empty() {
                            out.push(String::new());
                        }
                        heading = Some(level);
                    }
                    Tag::Paragraph => {
                        if !out.is_empty() && !in_table {
                            out.push(String::new());
                        }
                    }
                    Tag::Strong => styles.push("\x1b[1m"),
                    Tag::Emphasis => styles.push("\x1b[3m"),
                    Tag::Strikethrough => styles.push("\x1b[9m"),
                    Tag::CodeBlock(kind) => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        if !out.is_empty() {
                            out.push(String::new());
                        }
                        in_code_block = true;
                        code_buffer.clear();
                        code_language = match kind {
                            CodeBlockKind::Fenced(info) => info
                                .split_whitespace()
                                .next()
                                .filter(|language| !language.is_empty())
                                .map(|language| language.to_ascii_lowercase()),
                            CodeBlockKind::Indented => None,
                        };
                    }
                    Tag::List(start) => {
                        if list_stack.is_empty() && !out.is_empty() {
                            out.push(String::new());
                        }
                        list_stack.push(start);
                    }
                    Tag::Item => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        let depth = list_stack.len().saturating_sub(1);
                        let marker = match list_stack.last_mut() {
                            Some(Some(n)) => {
                                let m = format!("{n}. ");
                                *list_stack.last_mut().unwrap() = Some(*n + 1);
                                m
                            }
                            _ => "• ".to_string(),
                        };
                        current.push_str(&"  ".repeat(depth));
                        current.push_str(&self.theme.fg("accent", &marker));
                    }
                    Tag::BlockQuote(_) => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        quote_depth += 1;
                    }
                    Tag::Link { .. } => styles.push("\x1b[4m"),
                    Tag::Table(_) => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        in_table = true;
                        table_rows.clear();
                    }
                    Tag::TableRow | Tag::TableHead => table_row.clear(),
                    Tag::TableCell => current.clear(),
                    _ => {}
                },
                Event::End(tag) => match tag {
                    TagEnd::Heading(_) => {
                        let level = heading.take().unwrap_or(HeadingLevel::H3);
                        let hashes = "#".repeat(level as usize);
                        let text = format!("{hashes} {current}");
                        out.push(self.theme.fg("mdHeading", &self.theme.bold(&text)));
                        current.clear();
                    }
                    TagEnd::Paragraph => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme)
                    }
                    TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                        styles.pop();
                        current.push_str("\x1b[22m\x1b[23m\x1b[29m");
                        for s in &styles {
                            current.push_str(s);
                        }
                    }
                    TagEnd::CodeBlock => {
                        in_code_block = false;
                        out.extend(self.render_code_block(
                            &code_buffer,
                            code_language.as_deref(),
                            width,
                            is_streaming,
                            mermaid_mode,
                            true,
                        ));
                        code_language = None;
                    }
                    TagEnd::List(_) => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        list_stack.pop();
                    }
                    TagEnd::Item => flush(&mut current, &mut out, quote_depth, width, &self.theme),
                    TagEnd::BlockQuote(_) => {
                        flush(&mut current, &mut out, quote_depth, width, &self.theme);
                        quote_depth = quote_depth.saturating_sub(1);
                    }
                    TagEnd::Link => {
                        styles.pop();
                        current.push_str("\x1b[24m");
                    }
                    TagEnd::TableCell => {
                        table_row.push(crate::text::strip_ansi(&current));
                        current.clear();
                    }
                    TagEnd::TableRow | TagEnd::TableHead => {
                        table_rows.push(table_row.clone());
                        table_row.clear();
                    }
                    TagEnd::Table => {
                        in_table = false;
                        out.extend(self.render_table(&table_rows, width));
                        table_rows.clear();
                    }
                    _ => {}
                },
                Event::Text(text) => {
                    if in_code_block {
                        code_buffer.push_str(&text);
                    } else {
                        current.push_str(&text);
                    }
                }
                Event::Code(code) => {
                    current.push_str(&self.theme.fg("mdCode", &format!("`{code}`")));
                    for s in &styles {
                        current.push_str(s);
                    }
                }
                Event::InlineMath(math) => {
                    if let Ok(rendered) = mdwright_latex::render_unicode_math(&math)
                        && rendered.lines().len() == 1
                    {
                        current.push_str(&self.theme.fg("mdCode", &rendered.as_text()));
                    } else {
                        let rendered = mdwright_latex::translate_latex_to_unicode(&math);
                        if rendered.is_lossless() {
                            current.push_str(&self.theme.fg("mdCode", rendered.text()));
                        } else {
                            current.push_str(&format!("${math}$"));
                        }
                    }
                }
                Event::DisplayMath(math) => {
                    flush(&mut current, &mut out, quote_depth, width, &self.theme);
                    match mdwright_latex::render_unicode_math(&math) {
                        Ok(rendered) if rendered.width() <= width => {
                            out.extend(
                                rendered
                                    .lines()
                                    .iter()
                                    .map(|line| self.theme.fg("mdCode", line)),
                            );
                        }
                        _ => out.push(self.theme.fg("mdCode", &format!("$${math}$$"))),
                    }
                }
                Event::SoftBreak => current.push(' '),
                Event::HardBreak => flush(&mut current, &mut out, quote_depth, width, &self.theme),
                Event::Rule => {
                    flush(&mut current, &mut out, quote_depth, width, &self.theme);
                    out.push(self.theme.fg("dim", &"─".repeat(width.min(40))));
                }
                _ => {}
            }
        }
        flush(&mut current, &mut out, quote_depth, width, &self.theme);
        if in_code_block && !code_buffer.is_empty() {
            out.extend(self.render_code_block(
                &code_buffer,
                code_language.as_deref(),
                width,
                is_streaming,
                mermaid_mode,
                false,
            ));
        }
        while out.first().is_some_and(|l| l.is_empty()) {
            out.remove(0);
        }
        out
    }

    fn render_code_block(
        &self,
        source: &str,
        language: Option<&str>,
        width: usize,
        is_streaming: bool,
        mermaid_mode: MermaidMode,
        complete_fence: bool,
    ) -> Vec<String> {
        let source_lines = || {
            source
                .trim_end_matches('\n')
                .split('\n')
                .map(|line| format!("{}{}", self.code_indent, self.theme.fg("mdCode", line)))
                .collect::<Vec<_>>()
        };
        let is_mermaid = language.is_some_and(|language| language.eq_ignore_ascii_case("mermaid"));
        let render_allowed = is_mermaid
            && mermaid_mode != MermaidMode::Off
            && (!is_streaming || mermaid_mode == MermaidMode::Streaming);
        if !render_allowed {
            return source_lines();
        }

        let inner_width = width.saturating_sub(self.code_indent.len()).max(10);
        match mermaid_text::render_with_width(source.trim(), Some(inner_width)) {
            Ok(diagram)
                if !diagram.trim().is_empty()
                    && diagram
                        .lines()
                        .all(|line| crate::text::display_width(line) <= inner_width) =>
            {
                diagram
                    .trim_end_matches('\n')
                    .lines()
                    .map(|line| format!("{}{}", self.code_indent, self.theme.fg("mdCode", line)))
                    .collect()
            }
            Ok(_) | Err(_) if is_streaming || !complete_fence => source_lines(),
            Ok(_) => source_lines(),
            Err(error) => {
                let mut lines = source_lines();
                lines.push(self.theme.fg(
                    "warning",
                    &format!("  Mermaid diagram not rendered: {error}"),
                ));
                lines
            }
        }
    }

    fn render_table(&self, rows: &[Vec<String>], width: usize) -> Vec<String> {
        if rows.is_empty() {
            return Vec::new();
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        for row in rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i]
                    .max(crate::text::display_width(cell))
                    .min(width / cols.max(1));
            }
        }
        let mut out = Vec::new();
        for (ri, row) in rows.iter().enumerate() {
            let mut line = String::new();
            for (i, w) in widths.iter().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                line.push_str(&crate::text::fit_to_width(cell, *w));
                if i + 1 < cols {
                    line.push_str("  ");
                }
            }
            if ri == 0 {
                out.push(self.theme.bold(&line));
                out.push(self.theme.fg(
                    "dim",
                    &"─".repeat(crate::text::display_width(&line).min(width)),
                ));
            } else {
                out.push(line);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::strip_ansi;

    fn render_plain(md: &str) -> Vec<String> {
        MarkdownRenderer::new(Theme::dark())
            .render(md, 60)
            .iter()
            .map(|l| strip_ansi(l))
            .collect()
    }

    fn render_mermaid_plain(md: &str, streaming: bool, mode: MermaidMode) -> Vec<String> {
        MarkdownRenderer::new(Theme::dark())
            .render_with_state(md, 80, streaming, mode)
            .iter()
            .map(|line| strip_ansi(line))
            .collect()
    }

    #[test]
    fn headings_and_paragraphs() {
        let lines = render_plain("# Title\n\nBody text here.");
        assert_eq!(lines[0], "# Title");
        assert!(lines.contains(&"Body text here.".to_string()));
    }

    #[test]
    fn lists_render_markers() {
        let lines = render_plain("- one\n- two\n  1. nested");
        let text = lines.join("\n");
        assert!(text.contains("• one"));
        assert!(text.contains("• two"));
        assert!(text.contains("1. nested"));
    }

    #[test]
    fn code_blocks_indented() {
        let lines = render_plain("```\nlet x = 1;\n```");
        assert!(lines.iter().any(|l| l == "  let x = 1;"));
    }

    #[test]
    fn unterminated_fence_still_renders() {
        let lines = render_plain("```rust\npartial code");
        assert!(lines.iter().any(|l| l.contains("partial code")));
    }

    #[test]
    fn table_renders_aligned() {
        let lines = render_plain("| a | b |\n|---|---|\n| 1 | 2 |");
        assert!(lines[0].starts_with('a'));
        assert!(lines.iter().any(|l| l.starts_with('1')));
    }

    #[test]
    fn mermaid_streaming_mode_renders_terminal_art() {
        let lines = render_mermaid_plain(
            "```mermaid\ngraph LR\n  A --> B\n```",
            true,
            MermaidMode::Streaming,
        );
        let text = lines.join("\n");
        assert!(text.contains('A'));
        assert!(text.contains('B'));
        assert!(!text.contains("graph LR"));
    }

    #[test]
    fn mermaid_final_mode_keeps_source_while_streaming() {
        let lines = render_mermaid_plain(
            "```Mermaid\ngraph LR\n  A --> B\n```",
            true,
            MermaidMode::Final,
        );
        assert!(lines.join("\n").contains("graph LR"));

        let lines = render_mermaid_plain(
            "```Mermaid\ngraph LR\n  A --> B\n```",
            false,
            MermaidMode::Final,
        );
        assert!(!lines.join("\n").contains("graph LR"));
    }

    #[test]
    fn mermaid_off_mode_always_keeps_source() {
        let lines = render_mermaid_plain(
            "```mermaid\ngraph LR\n  A --> B\n```",
            false,
            MermaidMode::Off,
        );
        assert!(lines.join("\n").contains("graph LR"));
    }

    #[test]
    fn invalid_mermaid_warns_only_after_the_final_fence() {
        let source = "```mermaid\nthis is not mermaid\n```";
        let streaming = render_mermaid_plain(source, true, MermaidMode::Streaming).join("\n");
        assert!(!streaming.contains("not rendered"));
        let final_text = render_mermaid_plain(source, false, MermaidMode::Streaming).join("\n");
        assert!(final_text.contains("Mermaid diagram not rendered"));
    }

    #[test]
    fn latex_math_renders_as_terminal_unicode() {
        let lines = render_plain("Use $x^2 + \\pi$.\n\n$$\\frac{a}{b}$$");
        let text = lines.join("\n");
        assert!(text.contains("x²+π"), "{text:?}");
        assert!(!text.contains("\\pi"));
        assert!(text.contains('a'));
        assert!(text.contains('b'));
        assert!(text.contains('─'));
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_markdown_streaming() {
        let paragraph = "This is a deterministic streaming paragraph with **bold text**, `code`, and a [link](https://example.com).\n\n";
        let markdown = paragraph.repeat(400);
        let renderer = MarkdownRenderer::new(Theme::dark());
        kiss_bench::measure(
            "markdown_full_40k",
            11,
            2,
            "one_approximately_40kb_document",
            || {
                renderer
                    .render_with_state(&markdown, 100, false, MermaidMode::Off)
                    .len()
            },
        );

        let boundaries = (1..=200)
            .map(|part| {
                let mut end = markdown.len() * part / 200;
                while !markdown.is_char_boundary(end) {
                    end -= 1;
                }
                end
            })
            .collect::<Vec<_>>();
        kiss_bench::measure(
            "markdown_growing_40k_200",
            9,
            1,
            "200_streaming_prefix_renders",
            || {
                boundaries
                    .iter()
                    .map(|end| {
                        renderer
                            .render_with_state(&markdown[..*end], 100, true, MermaidMode::Off)
                            .len()
                    })
                    .sum::<usize>()
            },
        );

        let mut cache = StreamingMarkdownCache::default();
        kiss_bench::measure(
            "markdown_incremental_40k_200",
            9,
            1,
            "200_streaming_prefix_renders",
            || {
                boundaries
                    .iter()
                    .map(|end| {
                        cache
                            .render(&renderer, &markdown[..*end], 100, MermaidMode::Off)
                            .len()
                    })
                    .sum::<usize>()
            },
        );
    }

    #[test]
    fn incremental_markdown_matches_full_streaming_render() {
        let renderer = MarkdownRenderer::new(Theme::dark());
        let markdown = "# Heading\n\nFirst **paragraph**.\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n\nTail text";
        let mut cache = StreamingMarkdownCache::default();
        for end in markdown
            .char_indices()
            .map(|(index, character)| index + character.len_utf8())
        {
            assert_eq!(
                cache.render(&renderer, &markdown[..end], 80, MermaidMode::Off),
                renderer.render_with_state(&markdown[..end], 80, true, MermaidMode::Off),
                "prefix ending at byte {end}"
            );
        }
    }

    #[test]
    fn incremental_markdown_reloads_late_link_definitions() {
        let renderer = MarkdownRenderer::new(Theme::dark());
        let mut cache = StreamingMarkdownCache::default();
        let initial = "Read [the guide][guide].\n\nMore text.\n\n";
        cache.render(&renderer, initial, 80, MermaidMode::Off);
        let complete = format!("{initial}[guide]: https://example.com/guide\n");

        assert_eq!(
            cache.render(&renderer, &complete, 80, MermaidMode::Off),
            renderer.render_with_state(&complete, 80, true, MermaidMode::Off)
        );
    }
}
