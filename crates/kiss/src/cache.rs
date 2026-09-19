//! Historical provider KV-cache rates from persisted session usage.

use anyhow::{Context as _, Result};
use kiss_agent::AgentMessage;
use kiss_ai::Usage;
use kiss_coding::{SessionEntry, SessionManager};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

const GRAPH_WIDTH: usize = 32;
const GRAPH_HEIGHT: usize = 4;
const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

#[derive(Clone)]
struct CacheSample {
    identity: String,
    timestamp: i64,
    provider: String,
    usage: Usage,
}

struct SessionHistory {
    id: String,
    name: Option<String>,
    started: String,
    samples: Vec<CacheSample>,
}

#[derive(Default)]
struct CacheTotals {
    input: u64,
    read: u64,
    write: u64,
}

impl CacheTotals {
    fn add(&mut self, usage: &Usage) {
        self.input = self.input.saturating_add(usage.input);
        self.read = self.read.saturating_add(usage.cache_read);
        self.write = self.write.saturating_add(usage.cache_write);
    }

    fn rate(&self) -> f64 {
        let total = self
            .input
            .saturating_add(self.read)
            .saturating_add(self.write);
        if total == 0 {
            0.0
        } else {
            self.read as f64 / total as f64 * 100.0
        }
    }
}

pub(crate) fn usage_rate(usage: &Usage) -> Option<f64> {
    cache_metrics_available(usage).then(|| {
        CacheTotals {
            input: usage.input,
            read: usage.cache_read,
            write: usage.cache_write,
        }
        .rate()
    })
}

pub(crate) fn render_manager(manager: &SessionManager, provider: Option<&str>) -> String {
    render(
        &format!("session {}", short_id(manager.session_id())),
        vec![history(manager)],
        provider,
    )
}

pub(crate) fn render_saved(
    session_dir: &Path,
    session: Option<&str>,
    provider: Option<&str>,
) -> Result<String> {
    let paths: Vec<PathBuf> = if let Some(reference) = session {
        let path = PathBuf::from(reference);
        if path.is_file() {
            vec![path]
        } else {
            let matches = SessionManager::list_all(session_dir)?
                .into_iter()
                .filter(|listing| listing.id.starts_with(reference))
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [listing] => vec![listing.path.clone()],
                [] => anyhow::bail!(
                    "session '{reference}' was not found; use an existing session ID prefix or a JSONL session file path"
                ),
                _ => anyhow::bail!(
                    "session prefix '{reference}' matched more than one session; use more ID characters or a JSONL session file path"
                ),
            }
        }
    } else {
        SessionManager::list_all(session_dir)?
            .into_iter()
            .map(|listing| listing.path)
            .collect()
    };

    let mut sessions = Vec::with_capacity(paths.len());
    for path in paths {
        let manager = SessionManager::open(&path)
            .with_context(|| format!("could not read session {}", path.display()))?;
        sessions.push(history(&manager));
    }
    let title = session.map_or_else(
        || "all sessions".to_string(),
        |reference| format!("session {reference}"),
    );
    Ok(render(&title, sessions, provider))
}

fn history(manager: &SessionManager) -> SessionHistory {
    let mut provider = None;
    let mut samples = Vec::new();
    for entry in manager.entries() {
        match entry {
            SessionEntry::ModelChange {
                provider: changed, ..
            } => provider = Some(changed.clone()),
            SessionEntry::Message {
                base,
                message: AgentMessage::Assistant(assistant),
                ..
            } => {
                provider = Some(assistant.provider.clone());
                if cache_metrics_available(&assistant.usage) {
                    samples.push(CacheSample {
                        identity: sample_identity(
                            &base.id,
                            assistant.timestamp,
                            &assistant.provider,
                            &assistant.usage,
                        ),
                        timestamp: assistant.timestamp,
                        provider: assistant.provider.clone(),
                        usage: assistant.usage,
                    });
                }
            }
            SessionEntry::Compaction {
                base,
                usage: Some(usage),
                ..
            }
            | SessionEntry::BranchSummary {
                base,
                usage: Some(usage),
                ..
            } if cache_metrics_available(usage) => {
                let provider = provider.clone().unwrap_or_else(|| "unknown".into());
                let timestamp = chrono::DateTime::parse_from_rfc3339(&base.timestamp)
                    .map(|time| time.timestamp_millis())
                    .unwrap_or_default();
                samples.push(CacheSample {
                    identity: sample_identity(&base.id, timestamp, &provider, usage),
                    timestamp,
                    provider,
                    usage: *usage,
                });
            }
            _ => {}
        }
    }
    samples.sort_by_key(|sample| sample.timestamp);
    SessionHistory {
        id: manager.session_id().to_string(),
        name: manager.session_name(),
        started: manager.header().timestamp.clone(),
        samples,
    }
}

fn cache_metrics_available(usage: &Usage) -> bool {
    (usage.cache_read_available || usage.cache_read > 0 || usage.cache_write > 0)
        && usage
            .input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write)
            > 0
}

fn sample_identity(entry_id: &str, timestamp: i64, provider: &str, usage: &Usage) -> String {
    format!(
        "{entry_id}:{timestamp}:{provider}:{}:{}:{}",
        usage.input, usage.cache_read, usage.cache_write
    )
}

fn render(title: &str, mut sessions: Vec<SessionHistory>, provider: Option<&str>) -> String {
    for session in &mut sessions {
        session
            .samples
            .retain(|sample| provider.is_none_or(|value| sample.provider == value));
    }
    sessions.sort_by(|left, right| left.started.cmp(&right.started));
    let mut seen = HashSet::new();
    let mut all = sessions
        .iter()
        .flat_map(|session| session.samples.iter().cloned())
        .filter(|sample| seen.insert(sample.identity.clone()))
        .collect::<Vec<_>>();
    all.sort_by_key(|sample| sample.timestamp);

    let mut lines = vec![format!("Cache rate — {title}")];
    lines.push("cache-read input / all input".into());
    if all.is_empty() {
        let suffix = provider.map_or(String::new(), |value| format!(" for provider {value}"));
        lines.push(format!(
            "No upstream cache metrics were found{suffix}. Run a model request with a provider that reports cache token usage."
        ));
        return lines.join("\n");
    }

    lines.push(String::new());
    lines.push(history_chart("Total history", &all));

    let mut providers: BTreeMap<&str, Vec<CacheSample>> = BTreeMap::new();
    for sample in &all {
        providers
            .entry(&sample.provider)
            .or_default()
            .push(sample.clone());
    }
    lines.push(String::new());
    lines.push("Provider history".into());
    for (provider, samples) in providers {
        lines.push(history_chart(provider, &samples));
    }

    lines.push(String::new());
    lines.push("By session".into());
    for session in sessions
        .iter()
        .filter(|session| !session.samples.is_empty())
    {
        let day = session.started.get(..10).unwrap_or(&session.started);
        let name = session
            .name
            .as_deref()
            .map(|name| format!(" · {name}"))
            .unwrap_or_default();
        lines.push(rate_bar(
            &format!("{day} {}{name}", short_id(&session.id)),
            &session.samples,
        ));
    }
    lines.join("\n")
}

fn history_chart(label: &str, samples: &[CacheSample]) -> String {
    let (totals, rates) = cumulative_rates(samples);
    format!(
        "{label}\n{}\n      {:.1}% · {} / {} input tokens · {} responses",
        area_chart(&rates),
        totals.rate(),
        totals.read,
        totals
            .input
            .saturating_add(totals.read)
            .saturating_add(totals.write),
        samples.len(),
    )
}

fn rate_bar(label: &str, samples: &[CacheSample]) -> String {
    let (totals, _) = cumulative_rates(samples);
    let filled = (totals.rate() * 24.0 / 100.0).round() as usize;
    format!(
        "  {label}\n    {}{}  {:.1}% · {} / {} input tokens · {} responses",
        "█".repeat(filled),
        "░".repeat(24 - filled),
        totals.rate(),
        totals.read,
        totals
            .input
            .saturating_add(totals.read)
            .saturating_add(totals.write),
        samples.len(),
    )
}

fn cumulative_rates(samples: &[CacheSample]) -> (CacheTotals, Vec<f64>) {
    let mut totals = CacheTotals::default();
    let rates = samples
        .iter()
        .map(|sample| {
            totals.add(&sample.usage);
            totals.rate()
        })
        .collect();
    (totals, rates)
}

fn area_chart(values: &[f64]) -> String {
    if values.is_empty() {
        return String::new();
    }
    let columns = (0..GRAPH_WIDTH)
        .map(|column| {
            let value = values[column * values.len() / GRAPH_WIDTH].clamp(0.0, 100.0);
            (value * (GRAPH_HEIGHT * 8) as f64 / 100.0).round() as usize
        })
        .collect::<Vec<_>>();
    let mut lines = (0..GRAPH_HEIGHT)
        .map(|row| {
            let below = (GRAPH_HEIGHT - row - 1) * 8;
            let graph = columns
                .iter()
                .map(|units| BLOCKS[units.saturating_sub(below).min(8)])
                .collect::<String>();
            format!("{:>3}% │{graph}", (GRAPH_HEIGHT - row) * 100 / GRAPH_HEIGHT)
        })
        .collect::<Vec<_>>();
    lines.push(format!("  0% └{}", "─".repeat(GRAPH_WIDTH)));
    lines.push(format!(
        "      oldest{}newest",
        " ".repeat(GRAPH_WIDTH - 12)
    ));
    lines.join("\n")
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiss_ai::AssistantMessage;

    fn add_sample(
        manager: &mut SessionManager,
        provider: &str,
        timestamp: i64,
        input: u64,
        read: u64,
        write: u64,
        available: bool,
    ) {
        let mut assistant = AssistantMessage::empty("test", provider, "model");
        assistant.timestamp = timestamp;
        assistant.usage = Usage {
            input,
            cache_read: read,
            cache_write: write,
            cache_read_available: available,
            ..Default::default()
        };
        manager
            .append_message(AgentMessage::Assistant(assistant))
            .unwrap();
    }

    #[test]
    fn rate_includes_uncached_reads_and_writes_in_input() {
        let usage = Usage {
            input: 25,
            cache_read: 50,
            cache_write: 25,
            cache_read_available: true,
            ..Default::default()
        };
        assert_eq!(usage_rate(&usage), Some(50.0));
        assert_eq!(usage_rate(&Usage::default()), None);
    }

    #[test]
    fn report_splits_providers_and_shows_cumulative_history() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        add_sample(&mut manager, "anthropic", 1, 75, 25, 0, true);
        add_sample(&mut manager, "openai", 2, 0, 100, 0, true);

        let report = render_manager(&manager, None);
        assert!(report.contains("Provider history"));
        assert!(report.contains("anthropic"));
        assert!(report.contains("openai"));
        assert!(report.contains("62.5% · 125 / 200 input tokens"));
        assert!(report.contains("100% │"));
        assert!(report.contains("0% └"));
    }

    #[test]
    fn provider_filter_changes_total_and_session_rows() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        add_sample(&mut manager, "anthropic", 1, 80, 20, 0, true);
        add_sample(&mut manager, "openai", 2, 0, 100, 0, true);

        let report = render_manager(&manager, Some("anthropic"));
        assert!(report.contains("20.0% · 20 / 100 input tokens"));
        assert!(report.contains("anthropic"));
        assert!(!report.contains("\n  openai\n"));
    }

    #[test]
    fn old_positive_cache_counts_are_still_available() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        add_sample(&mut manager, "legacy", 1, 50, 50, 0, false);
        assert!(render_manager(&manager, None).contains("50.0%"));
    }

    #[test]
    fn all_session_total_does_not_count_forked_entries_twice() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        add_sample(&mut manager, "openai", 1, 50, 50, 0, true);
        let fork = manager.fork_active_branch(manager.leaf_id(), true).unwrap();

        let report = render(
            "all sessions",
            vec![history(&manager), history(&fork)],
            None,
        );
        let total = report.split("Provider history").next().unwrap();
        assert!(total.contains("50 / 100 input tokens · 1 responses"));
    }

    #[test]
    fn saved_report_accepts_a_session_id_prefix() {
        let temporary = tempfile::tempdir().unwrap();
        let session_dir = temporary.path().join("sessions");
        let mut manager =
            SessionManager::create(&temporary.path().join("project"), Some(session_dir.clone()))
                .unwrap();
        add_sample(&mut manager, "anthropic", 1, 20, 80, 0, true);
        let prefix = &manager.session_id()[..8];

        let report = render_saved(&session_dir, Some(prefix), Some("anthropic")).unwrap();
        assert!(report.contains("80.0% · 80 / 100 input tokens"));

        let error = render_saved(&session_dir, Some("missing"), None).unwrap_err();
        assert!(error.to_string().contains("existing session ID prefix"));
    }

    #[test]
    fn unavailable_metrics_do_not_claim_zero_percent() {
        let mut manager = SessionManager::in_memory(Path::new("/test"));
        add_sample(&mut manager, "unknown", 1, 100, 0, 0, false);
        let report = render_manager(&manager, None);
        assert!(report.contains("No upstream cache metrics were found"));
        assert!(!report.contains("0.0%"));
    }

    #[test]
    fn area_chart_uses_a_fixed_scale_and_width() {
        let graph = area_chart(&[0.0, 100.0]);
        let lines = graph.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 6);
        assert_eq!(lines[0], "100% │                ████████████████");
        assert_eq!(lines[4], "  0% └────────────────────────────────");
        assert_eq!(lines[5], "      oldest                    newest");
    }
}
