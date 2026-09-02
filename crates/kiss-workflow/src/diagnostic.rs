//! Script errors that a model can act on: a position, a message, and help.

use std::fmt;

/// One script error with the source position that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: u32,
    pub column: u32,
    pub message: String,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(line: u32, column: u32, message: impl Into<String>) -> Self {
        Diagnostic {
            line,
            column,
            message: message.into(),
            help: None,
        }
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    /// Render the error with the offending source line and a caret.
    ///
    /// This block is what the `run_workflow` tool returns to the model when a
    /// script does not parse, so it must name both the problem and a supported
    /// alternative.
    pub fn render(&self, source: &str) -> String {
        let mut out = format!(
            "workflow script error at line {}, column {}\n\n",
            self.line, self.column
        );
        if let Some(text) = source.lines().nth(self.line.saturating_sub(1) as usize) {
            let gutter = format!("{:>4} | ", self.line);
            out.push_str(&gutter);
            out.push_str(text);
            out.push('\n');
            let pad = gutter.len() + self.column.saturating_sub(1) as usize;
            out.push_str(&" ".repeat(pad));
            out.push_str("^\n");
        }
        out.push_str(&self.message);
        if let Some(help) = &self.help {
            out.push_str("\nhelp: ");
            out.push_str(help);
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}:{}: {}", self.line, self.column, self.message)?;
        if let Some(help) = &self.help {
            write!(f, " (help: {help})")?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_points_at_the_offending_column() {
        let source = "const a = 1\nfor (let i = 0; i < 3; i++) {}\n";
        let rendered = Diagnostic::new(2, 6, "counted for-loops are not supported")
            .with_help("use `for (const item of list)`")
            .render(source);
        assert!(rendered.contains("line 2, column 6"));
        assert!(rendered.contains("for (let i = 0"));
        assert!(rendered.contains("^"));
        assert!(rendered.contains("help: use `for (const item of list)`"));
        let caret_line = rendered
            .lines()
            .find(|line| line.trim() == "^")
            .expect("caret line");
        // The caret sits under column 6 of the source line, past the gutter.
        assert_eq!(caret_line.find('^'), Some("   2 | ".len() + 5));
    }

    #[test]
    fn render_survives_an_out_of_range_line() {
        let rendered = Diagnostic::new(99, 1, "unexpected end of script").render("const a = 1\n");
        assert!(rendered.contains("unexpected end of script"));
    }
}
