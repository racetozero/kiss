//! Component model: components render to lines; only the renderer writes to
//! the terminal.

/// A UI component. `render` must return lines whose display width does not
/// exceed `width`.
pub trait Component {
    fn render(&mut self, width: usize) -> Vec<String>;
    /// Receive raw input when focused.
    fn handle_input(&mut self, _data: &str) {}
    /// Drop cached render state (theme change, reload).
    fn invalidate(&mut self) {}
}

/// Static text block.
pub struct TextBlock {
    lines: Vec<String>,
}

impl TextBlock {
    pub fn new(text: impl Into<String>) -> Self {
        TextBlock {
            lines: text.into().split('\n').map(String::from).collect(),
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.lines = text.into().split('\n').map(String::from).collect();
    }
}

impl Component for TextBlock {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.lines
            .iter()
            .flat_map(|l| crate::text::wrap_text(l, width))
            .collect()
    }
}

/// Blank spacer.
pub struct Spacer(pub usize);

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new(); self.0]
    }
}
