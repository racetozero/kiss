//! App-level keybindings: defaults plus `~/.kiss/agent/keybindings.json`
//! overrides mapping action names to key specs.

use crate::keys::KeyEvent;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Submit,
    Newline,
    QueueFollowUp,
    Dequeue,
    Abort,
    Quit,
    CycleModel,
    CycleModelBackward,
    SelectModel,
    CycleThinking,
    ToggleThinking,
    CopyLastResponse,
    OpenTree,
    ExternalEditor,
    HistoryPrev,
    HistoryNext,
    ExpandTools,
    Clear,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::Submit => "submit",
            Action::Newline => "newline",
            Action::QueueFollowUp => "queueFollowUp",
            Action::Dequeue => "dequeue",
            Action::Abort => "abort",
            Action::Quit => "quit",
            Action::CycleModel => "cycleModel",
            Action::CycleModelBackward => "cycleModelBackward",
            Action::SelectModel => "selectModel",
            Action::CycleThinking => "cycleThinking",
            Action::ToggleThinking => "toggleThinking",
            Action::CopyLastResponse => "copyLastResponse",
            Action::OpenTree => "openTree",
            Action::ExternalEditor => "externalEditor",
            Action::HistoryPrev => "historyPrev",
            Action::HistoryNext => "historyNext",
            Action::ExpandTools => "expandTools",
            Action::Clear => "clear",
        }
    }

    fn all() -> &'static [Action] {
        &[
            Action::Submit,
            Action::Newline,
            Action::QueueFollowUp,
            Action::Dequeue,
            Action::Abort,
            Action::Quit,
            Action::CycleModel,
            Action::CycleModelBackward,
            Action::SelectModel,
            Action::CycleThinking,
            Action::ToggleThinking,
            Action::CopyLastResponse,
            Action::OpenTree,
            Action::ExternalEditor,
            Action::HistoryPrev,
            Action::HistoryNext,
            Action::ExpandTools,
            Action::Clear,
        ]
    }

    fn default_spec(&self) -> &'static str {
        match self {
            Action::Submit => "enter",
            Action::Newline => "shift+enter",
            Action::QueueFollowUp => "ctrl+enter",
            Action::Dequeue => "alt+up",
            Action::Abort => "escape",
            Action::Quit => "ctrl+d",
            Action::CycleModel => "ctrl+p",
            Action::CycleModelBackward => "shift+ctrl+p",
            Action::SelectModel => "ctrl+l",
            Action::CycleThinking => "shift+tab",
            Action::ToggleThinking => "ctrl+t",
            Action::CopyLastResponse => "ctrl+x",
            Action::OpenTree => "",
            Action::ExternalEditor => "ctrl+g",
            Action::HistoryPrev => "up",
            Action::HistoryNext => "down",
            Action::ExpandTools => "ctrl+o",
            Action::Clear => "ctrl+c",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Keybindings {
    map: HashMap<KeyEvent, Action>,
}

impl Default for Keybindings {
    fn default() -> Self {
        let mut map = HashMap::new();
        for action in Action::all() {
            if let Some(key) = KeyEvent::parse(action.default_spec()) {
                map.insert(key, *action);
            }
        }
        Keybindings { map }
    }
}

impl Keybindings {
    /// Apply user overrides: {"cycleModel": "ctrl+m", ...}.
    pub fn load_overrides(&mut self) {
        let Some(path) = dirs::home_dir().map(|h| h.join(".kiss/agent/keybindings.json")) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(overrides) = serde_json::from_str::<HashMap<String, String>>(&text) else {
            return;
        };
        for (action_name, spec) in overrides {
            let Some(action) = Action::all()
                .iter()
                .find(|a| a.name() == action_name)
                .copied()
            else {
                continue;
            };
            let Some(key) = KeyEvent::parse(&spec) else {
                continue;
            };
            self.map.retain(|_, a| *a != action);
            self.map.insert(key, action);
        }
    }

    pub fn action_for(&self, key: &KeyEvent) -> Option<Action> {
        self.map.get(key).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_resolve() {
        let kb = Keybindings::default();
        assert_eq!(
            kb.action_for(&KeyEvent::parse("enter").unwrap()),
            Some(Action::Submit)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("ctrl+p").unwrap()),
            Some(Action::CycleModel)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("shift+ctrl+p").unwrap()),
            Some(Action::CycleModelBackward)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("shift+tab").unwrap()),
            Some(Action::CycleThinking)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("ctrl+d").unwrap()),
            Some(Action::Quit)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("shift+enter").unwrap()),
            Some(Action::Newline)
        );
        assert_eq!(
            kb.action_for(&KeyEvent::parse("ctrl+enter").unwrap()),
            Some(Action::QueueFollowUp)
        );
        assert_eq!(kb.action_for(&KeyEvent::parse("alt+enter").unwrap()), None);
    }
}
