//! The `/workflows` progress view and the lines that announce a run.
//!
//! The view renders from a snapshot and redraws only when the run's version
//! counter moves, so a run with hundreds of agents costs nothing to display
//! while it is idle.

use kiss_coding::workflows::{
    AgentSnapshot, AgentStatus, RunRecord, RunSnapshot, RunStatus, RunSummary, WorkflowPlan,
};
use kiss_tui::{Key, KeyEvent, Theme, text};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

/// What the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The run and its phases.
    Run,
    /// One phase and its agents.
    Phase,
    /// One agent's prompt and answer.
    Agent,
}

/// Which agents a phase shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Working,
    Failed,
}

impl Filter {
    fn next(self) -> Filter {
        match self {
            Filter::All => Filter::Working,
            Filter::Working => Filter::Failed,
            Filter::Failed => Filter::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Filter::All => "all",
            Filter::Working => "working",
            Filter::Failed => "failed",
        }
    }

    fn keeps(self, agent: &AgentSnapshot) -> bool {
        match self {
            Filter::All => true,
            Filter::Working => !agent.status.is_finished(),
            Filter::Failed => matches!(agent.status, AgentStatus::Failed | AgentStatus::Stopped),
        }
    }
}

/// What the caller should do after a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewAction {
    None,
    Close,
    /// The user asked to save this run's script.
    Save,
}

/// The state of one open progress view.
pub(crate) struct WorkflowView {
    pub run: Arc<RunRecord>,
    focus: Focus,
    phase: usize,
    agent: usize,
    scroll: usize,
    filter: Filter,
    /// Rendered lines for one (width, run version, view state), so an unchanged
    /// frame is not rebuilt.
    cache: Option<(usize, u64, u64, Vec<String>)>,
}

impl WorkflowView {
    pub(crate) fn new(run: Arc<RunRecord>) -> WorkflowView {
        WorkflowView {
            run,
            focus: Focus::Run,
            phase: 0,
            agent: 0,
            scroll: 0,
            filter: Filter::All,
            cache: None,
        }
    }

    /// A key that identifies the current selection, so the cache knows when the
    /// view changed even though the run did not.
    fn state_key(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (self.focus as u8).hash(&mut hasher);
        self.phase.hash(&mut hasher);
        self.agent.hash(&mut hasher);
        self.scroll.hash(&mut hasher);
        (self.filter as u8).hash(&mut hasher);
        hasher.finish()
    }

    /// The agents of the selected phase that pass the filter.
    fn visible_agents(snapshot: &RunSnapshot, phase: usize, filter: Filter) -> Vec<&AgentSnapshot> {
        snapshot
            .phases
            .get(phase)
            .map(|phase| {
                phase
                    .agents
                    .iter()
                    .filter(|agent| filter.keeps(agent))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> ViewAction {
        let snapshot = self.run.snapshot();
        match key.key {
            Key::Escape | Key::Left => match self.focus {
                Focus::Run => return ViewAction::Close,
                Focus::Phase => {
                    self.focus = Focus::Run;
                    self.agent = 0;
                }
                Focus::Agent => {
                    self.focus = Focus::Phase;
                    self.scroll = 0;
                }
            },
            Key::Enter | Key::Right => match self.focus {
                Focus::Run => {
                    if !snapshot.phases.is_empty() {
                        self.focus = Focus::Phase;
                        self.agent = 0;
                    }
                }
                Focus::Phase => {
                    if !Self::visible_agents(&snapshot, self.phase, self.filter).is_empty() {
                        self.focus = Focus::Agent;
                        self.scroll = 0;
                    }
                }
                Focus::Agent => {}
            },
            Key::Up => self.move_selection(&snapshot, -1),
            Key::Down => self.move_selection(&snapshot, 1),
            Key::Char('k') if self.focus == Focus::Agent => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            Key::Char('j') if self.focus == Focus::Agent => {
                self.scroll = self.scroll.saturating_add(1);
            }
            Key::Char('f') => {
                self.filter = self.filter.next();
                self.agent = 0;
                if self.focus == Focus::Agent {
                    self.focus = Focus::Phase;
                }
            }
            Key::Char('p') => {
                if self.run.is_paused() {
                    self.run.resume();
                } else {
                    self.run.pause();
                }
            }
            Key::Char('x') => match self.focus {
                // On the run, `x` stops everything. Inside a phase it stops the
                // one selected agent, whose `agent()` call then returns null
                // and lets the rest of the script carry on.
                Focus::Run => self.run.stop(),
                Focus::Phase | Focus::Agent => {
                    if let Some(agent) =
                        Self::visible_agents(&snapshot, self.phase, self.filter).get(self.agent)
                    {
                        self.run.stop_agent(agent.id);
                    }
                }
            },
            Key::Char('r') if self.focus != Focus::Run => {
                if let Some(agent) =
                    Self::visible_agents(&snapshot, self.phase, self.filter).get(self.agent)
                {
                    self.run.restart_agent(agent.id);
                }
            }
            Key::Char('s') => return ViewAction::Save,
            _ => {}
        }
        ViewAction::None
    }

    fn move_selection(&mut self, snapshot: &RunSnapshot, delta: isize) {
        let step = |current: usize, len: usize| -> usize {
            if len == 0 {
                return 0;
            }
            let next = current as isize + delta;
            next.clamp(0, len as isize - 1) as usize
        };
        match self.focus {
            Focus::Run => {
                self.phase = step(self.phase, snapshot.phases.len());
                self.agent = 0;
            }
            Focus::Phase => {
                let agents = Self::visible_agents(snapshot, self.phase, self.filter).len();
                self.agent = step(self.agent, agents);
            }
            Focus::Agent => {
                self.scroll = if delta < 0 {
                    self.scroll.saturating_sub(1)
                } else {
                    self.scroll.saturating_add(1)
                };
            }
        }
    }

    pub(crate) fn render(&mut self, width: usize, theme: &Theme) -> Vec<String> {
        let snapshot = self.run.snapshot();
        let version = *self.run.subscribe().borrow();
        let state = self.state_key();
        if let Some((cached_width, cached_version, cached_state, lines)) = &self.cache
            && *cached_width == width
            && *cached_version == version
            && *cached_state == state
        {
            return lines.clone();
        }

        let lines = match self.focus {
            Focus::Agent => self.render_agent(&snapshot, width, theme),
            _ => self.render_phases(&snapshot, width, theme),
        };
        self.cache = Some((width, version, state, lines.clone()));
        lines
    }

    fn render_header(&self, snapshot: &RunSnapshot, width: usize, theme: &Theme) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "{} {}",
            theme.fg("accent", &theme.bold(&self.run.name)),
            theme.fg(
                status_color(snapshot.status),
                &format!(
                    "{} · {}/{} agents · {} tokens · {}",
                    snapshot.status.label(),
                    snapshot.finished_agents(),
                    snapshot.total_agents(),
                    format_tokens(snapshot.tokens),
                    format_elapsed(snapshot.elapsed),
                )
            ),
        ));
        if !self.run.description.is_empty() {
            lines.push(theme.fg(
                "muted",
                &text::truncate_to_width(&self.run.description, width),
            ));
        }
        if let Some(error) = &snapshot.error {
            for line in text::wrap_text(error, width.saturating_sub(2)) {
                lines.push(theme.fg("error", &format!("✗ {line}")));
            }
        }
        // Only the last few log lines: a long-running script can write many.
        let log_start = snapshot.log.len().saturating_sub(3);
        for message in &snapshot.log[log_start..] {
            lines.push(theme.fg(
                "muted",
                &text::truncate_to_width(&format!("※ {message}"), width),
            ));
        }
        lines.push(String::new());
        lines
    }

    fn render_phases(&self, snapshot: &RunSnapshot, width: usize, theme: &Theme) -> Vec<String> {
        let mut lines = self.render_header(snapshot, width, theme);

        for (index, phase) in snapshot.phases.iter().enumerate() {
            let selected = index == self.phase;
            let marker = if selected && self.focus == Focus::Run {
                "▸"
            } else if selected {
                "▾"
            } else {
                " "
            };
            let label = format!(
                "{marker} {:<24} {:>3}/{:<3} {:>8}",
                text::truncate_to_width(&phase.title, 24),
                phase.finished_agents(),
                phase.agents.len(),
                format_tokens(phase.tokens),
            );
            lines.push(if selected {
                theme.fg(
                    "accent",
                    &theme.bold(&text::truncate_to_width(&label, width)),
                )
            } else {
                theme.fg("toolTitle", &text::truncate_to_width(&label, width))
            });

            // The selected phase shows its agents.
            if !selected {
                continue;
            }
            let agents = Self::visible_agents(snapshot, index, self.filter);
            if agents.is_empty() {
                let note = match self.filter {
                    Filter::All => "    no agents yet",
                    _ => "    no agents match this filter",
                };
                lines.push(theme.fg("dim", note));
                continue;
            }
            // Keep the selected agent on screen in a long phase.
            let window = 12;
            let start = self
                .agent
                .saturating_sub(window - 1)
                .min(agents.len().saturating_sub(window.min(agents.len())));
            for (offset, agent) in agents.iter().skip(start).take(window).enumerate() {
                let position = start + offset;
                let chosen = position == self.agent && self.focus != Focus::Run;
                let row = format!(
                    "  {} {:<32} {:<10} {:>8} {:>7}",
                    agent_marker(agent.status),
                    text::truncate_to_width(&agent.label, 32),
                    agent.status.label(),
                    format_tokens(agent.tokens),
                    format_elapsed(agent.elapsed),
                );
                let row = text::truncate_to_width(&row, width);
                lines.push(if chosen {
                    theme.fg("accent", &theme.bold(&row))
                } else {
                    theme.fg("toolOutput", &row)
                });
            }
            if agents.len() > start + window {
                lines.push(theme.fg(
                    "dim",
                    &format!("    … {} more", agents.len() - start - window),
                ));
            }
        }

        lines.push(String::new());
        lines.push(theme.fg("dim", &self.footer(width)));
        lines
    }

    fn render_agent(&self, snapshot: &RunSnapshot, width: usize, theme: &Theme) -> Vec<String> {
        let agents = Self::visible_agents(snapshot, self.phase, self.filter);
        let Some(agent) = agents.get(self.agent) else {
            return self.render_phases(snapshot, width, theme);
        };

        let mut lines = Vec::new();
        lines.push(format!(
            "{} {}",
            theme.fg(
                "accent",
                &theme.bold(&text::truncate_to_width(&agent.label, width / 2))
            ),
            theme.fg(
                agent_color(agent.status),
                &format!(
                    "{} · {} tokens · {}",
                    agent.status.label(),
                    format_tokens(agent.tokens),
                    format_elapsed(agent.elapsed),
                )
            ),
        ));
        lines.push(String::new());

        let mut body: Vec<String> = Vec::new();
        body.push(theme.fg("toolTitle", &theme.bold("Prompt")));
        for line in text::wrap_text(&agent.prompt, width.saturating_sub(4)) {
            body.push(theme.fg("toolOutput", &format!("  {line}")));
        }
        body.push(String::new());
        match (&agent.result, &agent.error) {
            (Some(result), _) => {
                body.push(theme.fg("toolTitle", &theme.bold("Result")));
                for line in text::wrap_text(result, width.saturating_sub(4)) {
                    body.push(theme.fg("toolOutput", &format!("  {line}")));
                }
            }
            (None, Some(error)) => {
                body.push(theme.fg("error", &theme.bold("Error")));
                for line in text::wrap_text(error, width.saturating_sub(4)) {
                    body.push(theme.fg("error", &format!("  {line}")));
                }
            }
            (None, None) => body.push(theme.fg("dim", "  no answer yet")),
        }

        // Scroll the body, keeping the header fixed.
        let window = 20;
        let start = self.scroll.min(body.len().saturating_sub(1));
        lines.extend(body.iter().skip(start).take(window).cloned());
        if body.len() > start + window {
            lines.push(theme.fg(
                "dim",
                &format!("  … {} more lines", body.len() - start - window),
            ));
        }

        lines.push(String::new());
        lines.push(theme.fg("dim", &self.footer(width)));
        lines
    }

    fn footer(&self, width: usize) -> String {
        let keys = match self.focus {
            Focus::Agent => "j/k scroll · esc back · x stop · r restart · p pause · s save",
            Focus::Phase => {
                "↑↓ select · enter open · esc back · f filter · x stop · r restart · p pause · s save"
            }
            Focus::Run => "↑↓ phase · enter open · esc close · p pause · x stop run · s save",
        };
        let filter = if self.filter == Filter::All {
            String::new()
        } else {
            format!(" · showing {}", self.filter.label())
        };
        let paused = if self.run.is_paused() {
            " · PAUSED"
        } else {
            ""
        };
        text::truncate_to_width(&format!("{keys}{filter}{paused}"), width)
    }
}

fn status_color(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "accent",
        RunStatus::Paused => "warning",
        RunStatus::Completed => "success",
        RunStatus::Failed => "error",
        RunStatus::Stopped => "muted",
    }
}

fn agent_color(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "dim",
        AgentStatus::Running => "accent",
        AgentStatus::Completed | AgentStatus::Reused => "success",
        AgentStatus::Failed => "error",
        AgentStatus::Stopped => "warning",
    }
}

fn agent_marker(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "◌",
        AgentStatus::Running => "●",
        AgentStatus::Completed => "✔",
        AgentStatus::Reused => "↺",
        AgentStatus::Failed => "✗",
        AgentStatus::Stopped => "◯",
    }
}

pub(crate) fn format_tokens(tokens: u64) -> String {
    match tokens {
        0 => "-".into(),
        tokens if tokens < 1_000 => tokens.to_string(),
        tokens if tokens < 1_000_000 => format!("{:.1}k", tokens as f64 / 1_000.0),
        tokens if tokens < 1_000_000_000 => format!("{:.1}M", tokens as f64 / 1_000_000.0),
        tokens => format!("{:.1}B", tokens as f64 / 1_000_000_000.0),
    }
}

pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    if seconds < 3600 {
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
}

/// The one-line summary shown under the transcript while a run is going.
pub(crate) fn progress_line(
    snapshot: &RunSnapshot,
    name: &str,
    spinner: &str,
    warn_over_agents: Option<u32>,
    theme: &Theme,
) -> String {
    let phase = snapshot
        .active_phase()
        .map(|phase| phase.title.as_str())
        .unwrap_or("starting");
    let body = format!(
        "{spinner} {name} · {phase} · {}/{} agents · {} tokens · ctrl+w opens the workflow view",
        snapshot.finished_agents(),
        snapshot.total_agents(),
        format_tokens(snapshot.tokens),
    );
    // Advisory only: a large run is flagged, never paused.
    let large = warn_over_agents.is_some_and(|limit| snapshot.total_agents() as u32 > limit)
        || snapshot.tokens > 1_500_000;
    if large {
        theme.fg("warning", &format!("△ large workflow · {body}"))
    } else {
        theme.fg("accent", &body)
    }
}

/// The lines shown with the approval prompt, before any agent starts.
pub(crate) fn plan_summary(plan: &WorkflowPlan) -> String {
    let mut out = format!("{}\n{}\n", plan.name, plan.description);
    out.push_str(&format!("\nWill start {}.", plan.agent_estimate()));
    if !plan.phases.is_empty() {
        out.push_str("\nPhases: ");
        out.push_str(&plan.phases.join(" → "));
    }
    out
}

/// One row per run for the `/workflows` picker.
pub(crate) fn summary_detail(summary: &RunSummary) -> String {
    format!(
        "{} · {}/{} agents · {} tokens · {}",
        summary.status.label(),
        summary.finished_agents,
        summary.total_agents,
        format_tokens(summary.tokens),
        format_elapsed(summary.elapsed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_stay_short_enough_for_a_column() {
        assert_eq!(format_tokens(0), "-");
        assert_eq!(format_tokens(940), "940");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(2_400_000), "2.4M");
        // The agent cap bounds a run well below this, so the column cannot be
        // pushed out of shape by a real total.
        assert_eq!(format_tokens(3_500_000_000), "3.5B");
        assert!(format_tokens(999_000_000_000).len() <= 8);
    }

    #[test]
    fn elapsed_time_reads_as_a_duration_not_a_number() {
        assert_eq!(format_elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1m15s");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1h02m");
    }

    #[test]
    fn a_filter_selects_the_agents_it_names() {
        let agent = |status| AgentSnapshot {
            id: 0,
            label: "a".into(),
            status,
            tokens: 0,
            elapsed: Duration::ZERO,
            prompt: String::new(),
            result: None,
            error: None,
        };
        assert!(Filter::All.keeps(&agent(AgentStatus::Completed)));
        assert!(Filter::Working.keeps(&agent(AgentStatus::Running)));
        assert!(!Filter::Working.keeps(&agent(AgentStatus::Completed)));
        assert!(Filter::Failed.keeps(&agent(AgentStatus::Failed)));
        assert!(Filter::Failed.keeps(&agent(AgentStatus::Stopped)));
        assert!(!Filter::Failed.keeps(&agent(AgentStatus::Completed)));
    }

    #[test]
    fn the_filter_key_cycles_back_to_showing_everything() {
        assert_eq!(Filter::All.next(), Filter::Working);
        assert_eq!(Filter::Working.next(), Filter::Failed);
        assert_eq!(Filter::Failed.next(), Filter::All);
    }

    #[test]
    fn a_plan_summary_states_the_agent_count_and_the_phases() {
        let plan = WorkflowPlan {
            name: "audit-routes".into(),
            description: "Audit every route".into(),
            phases: vec!["Discover".into(), "Audit".into()],
            estimated_agents: Some(4),
            source: Arc::from(""),
        };
        let summary = plan_summary(&plan);
        assert!(summary.contains("audit-routes"));
        assert!(summary.contains("Will start 4 agents."));
        assert!(summary.contains("Discover → Audit"));

        let unbounded = WorkflowPlan {
            estimated_agents: None,
            ..plan
        };
        assert!(plan_summary(&unbounded).contains("an unbounded number of agents"));
    }
}
