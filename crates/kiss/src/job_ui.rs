//! The compact progress view for `/loop` and `/autoresearch` jobs.

use crate::workflow_ui::{format_elapsed, format_tokens};
use kiss_coding::iterative::{IterativeRuntime, JobRecord, JobStatus};
use kiss_tui::{Key, KeyEvent, Theme, text};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobViewAction {
    None,
    Close,
}

pub(crate) struct JobView {
    runtime: Arc<IterativeRuntime>,
    selected: usize,
    detail: bool,
    scroll: usize,
}

impl JobView {
    pub(crate) fn new(runtime: Arc<IterativeRuntime>) -> Self {
        let selected = runtime.summaries().len().saturating_sub(1);
        Self {
            runtime,
            selected,
            detail: false,
            scroll: 0,
        }
    }

    fn selected_job(&self) -> Option<Arc<JobRecord>> {
        let summaries = self.runtime.summaries();
        summaries
            .get(self.selected.min(summaries.len().saturating_sub(1)))
            .and_then(|summary| self.runtime.get(summary.id))
    }

    pub(crate) fn handle_key(&mut self, key: &KeyEvent) -> JobViewAction {
        match key.key {
            Key::Escape | Key::Left if self.detail => {
                self.detail = false;
                self.scroll = 0;
            }
            Key::Escape | Key::Left => return JobViewAction::Close,
            Key::Enter | Key::Right => {
                self.detail = true;
                self.scroll = 0;
            }
            Key::Up if self.detail => self.scroll = self.scroll.saturating_sub(1),
            Key::Down if self.detail => self.scroll = self.scroll.saturating_add(1),
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => {
                self.selected =
                    (self.selected + 1).min(self.runtime.summaries().len().saturating_sub(1));
            }
            Key::Char('p') => {
                if let Some(job) = self.selected_job() {
                    if job.is_paused() {
                        job.resume();
                    } else {
                        job.pause();
                    }
                }
            }
            Key::Char('x') => {
                if let Some(job) = self.selected_job() {
                    job.stop();
                }
            }
            _ => {}
        }
        JobViewAction::None
    }

    pub(crate) fn render(&mut self, width: usize, theme: &Theme) -> Vec<String> {
        let summaries = self.runtime.summaries();
        if summaries.is_empty() {
            return vec![theme.fg("muted", "No loop or autoresearch jobs have run.")];
        }
        self.selected = self.selected.min(summaries.len() - 1);
        if self.detail {
            let Some(job) = self.runtime.get(summaries[self.selected].id) else {
                self.detail = false;
                return self.render(width, theme);
            };
            return render_detail(&job.snapshot(), self.scroll, width, theme);
        }

        let mut lines = vec![theme.fg("accent", &theme.bold("Jobs")), String::new()];
        for (index, job) in summaries.iter().enumerate() {
            let marker = if index == self.selected { "▸" } else { " " };
            let row = format!(
                "{marker} #{:<3} {:<12} {:<9} {:>3}/{:<3} {:>8}",
                job.id,
                job.kind.label(),
                job.status.label(),
                job.iteration,
                job.limit,
                format_tokens(job.tokens),
            );
            lines.push(if index == self.selected {
                theme.fg("accent", &theme.bold(&text::truncate_to_width(&row, width)))
            } else {
                theme.fg("toolOutput", &text::truncate_to_width(&row, width))
            });
            if index == self.selected {
                lines.push(theme.fg(
                    "muted",
                    &text::truncate_to_width(&format!("    {}", job.goal), width),
                ));
            }
        }
        lines.push(String::new());
        lines.push(theme.fg(
            "dim",
            &text::truncate_to_width(
                "↑↓ select · enter open · p pause/resume · x stop · esc close",
                width,
            ),
        ));
        lines
    }
}

fn render_detail(
    job: &kiss_coding::iterative::JobSnapshot,
    scroll: usize,
    width: usize,
    theme: &Theme,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{} {}",
        theme.fg(
            "accent",
            &theme.bold(&format!("{} #{}", job.kind.label(), job.id))
        ),
        theme.fg(
            status_color(job.status),
            &format!(
                "{} · iteration {}/{} · {} tokens · {}",
                job.status.label(),
                job.iteration,
                job.limit,
                format_tokens(job.tokens),
                format_elapsed(job.elapsed),
            )
        )
    )];
    lines.push(theme.fg("muted", &format!("session {}", job.session_id)));
    lines.push(String::new());
    lines.push(theme.fg("toolTitle", &theme.bold("Goal")));
    for line in text::wrap_text(&job.goal, width.saturating_sub(2)) {
        lines.push(format!("  {line}"));
    }
    lines.push(String::new());
    if let Some(error) = &job.error {
        lines.push(theme.fg("error", &theme.bold("Error")));
        for line in text::wrap_text(error, width.saturating_sub(2)) {
            lines.push(theme.fg("error", &format!("  {line}")));
        }
    } else {
        lines.push(theme.fg("toolTitle", &theme.bold("Latest result")));
        let result = job.latest_result.as_deref().unwrap_or("No result yet.");
        let body = text::wrap_text(result, width.saturating_sub(2));
        lines.extend(
            body.iter()
                .skip(scroll.min(body.len().saturating_sub(1)))
                .take(20)
                .map(|line| theme.fg("toolOutput", &format!("  {line}"))),
        );
    }
    lines.push(String::new());
    lines.push(theme.fg(
        "dim",
        &text::truncate_to_width("↑↓ scroll · esc jobs · p pause/resume · x stop", width),
    ));
    lines
}

fn status_color(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "dim",
        JobStatus::Running => "accent",
        JobStatus::Paused => "warning",
        JobStatus::Completed => "success",
        JobStatus::Failed => "error",
        JobStatus::Stopped => "muted",
    }
}

pub(crate) fn progress_line(runtime: &IterativeRuntime, theme: &Theme, width: usize) -> String {
    let active = runtime.active_count();
    let noun = if active == 1 { "job" } else { "jobs" };
    theme.fg(
        "accent",
        &text::truncate_to_width(
            &format!("● {active} iterative {noun} running · /jobs opens the job view"),
            width,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_coding::iterative::{JobKind, JobSnapshot};
    use std::time::Duration;

    #[test]
    fn status_colors_cover_every_terminal_state() {
        assert_eq!(status_color(JobStatus::Running), "accent");
        assert_eq!(status_color(JobStatus::Completed), "success");
        assert_eq!(status_color(JobStatus::Failed), "error");
        assert_eq!(status_color(JobStatus::Stopped), "muted");
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_job_detail_render() {
        let job = JobSnapshot {
            id: 7,
            kind: JobKind::Autoresearch,
            goal: "Reduce renderer latency while all focused tests stay green.".repeat(8),
            status: JobStatus::Running,
            iteration: 12,
            limit: 25,
            tokens: 42_000,
            elapsed: Duration::from_secs(95),
            session_id: "00000000-0000-0000-0000-000000000007".into(),
            latest_result: Some(
                "Measured the same benchmark, kept the faster change, and recorded the result. "
                    .repeat(80),
            ),
            error: None,
        };
        let theme = Theme::dark();
        kiss_bench::measure(
            "job_detail_render",
            21,
            10,
            "one_large_job_snapshot",
            || render_detail(&job, 0, 100, &theme).len(),
        );
    }
}
