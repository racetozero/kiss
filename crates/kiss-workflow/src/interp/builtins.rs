//! The builtin functions and the methods available on values.
//!
//! `agent()` is the only builtin that reaches outside the script. Everything
//! else is pure, so a script's shape is decided entirely by data the run has
//! already collected.

use super::{DETERMINISM_MESSAGE, Interp, RunError, to_number};
use crate::ast::Pos;
use crate::progress::AgentStatus;
use crate::runner::{AgentId, AgentOutcome, AgentRequest};
use crate::value::{Builtin, Value, format_number};
use futures::future::BoxFuture;
use serde_json::Value as Json;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// The options object accepted by `agent()`.
#[derive(Debug, Default)]
struct AgentOptions {
    label: Option<String>,
    phase: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    schema: Option<Json>,
    timeout_ms: Option<u64>,
    retries: u32,
}

impl AgentOptions {
    fn read(value: Option<&Value>, pos: Pos) -> Result<AgentOptions, RunError> {
        let Some(value) = value else {
            return Ok(AgentOptions::default());
        };
        if matches!(value, Value::Null) {
            return Ok(AgentOptions::default());
        }
        let Some(fields) = value.fields() else {
            return Err(RunError::at(
                pos,
                format!(
                    "the second argument to `agent` must be an options object, but found {}",
                    value.kind()
                ),
            ));
        };
        let mut options = AgentOptions::default();
        for (name, field) in fields {
            match name.as_ref() {
                "label" => options.label = Some(field.display()),
                "phase" => options.phase = Some(field.display()),
                "model" => options.model = Some(field.display()),
                "effort" => options.effort = Some(field.display()),
                "schema" => options.schema = Some(field.to_json()),
                "timeoutMs" => options.timeout_ms = field.as_number().map(|ms| ms.max(0.0) as u64),
                "retries" => {
                    options.retries = field.as_number().unwrap_or(0.0).clamp(0.0, 5.0) as u32;
                }
                other => {
                    return Err(RunError::at(
                        pos,
                        format!(
                            "`{other}` is not an option for `agent`; the options are \
                             label, phase, model, effort, schema, timeoutMs, and retries"
                        ),
                    ));
                }
            }
        }
        Ok(options)
    }
}

impl Interp {
    pub(super) fn call_builtin<'a>(
        &'a self,
        builtin: Builtin,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> BoxFuture<'a, Result<Value, RunError>> {
        Box::pin(async move {
            let first = args.first().cloned().unwrap_or(Value::Null);
            match builtin {
                Builtin::Agent => self.builtin_agent(args, pos).await,
                Builtin::Parallel => self.builtin_parallel(first, pos, depth).await,
                Builtin::Pipeline => self.builtin_pipeline(args, pos, depth).await,
                Builtin::Phase => {
                    let title = first.display();
                    if title.trim().is_empty() {
                        return Err(RunError::at(pos, "`phase` needs a title"));
                    }
                    self.state.set_phase(title.trim());
                    Ok(Value::Null)
                }
                Builtin::Log => {
                    self.state.log(first.display());
                    Ok(Value::Null)
                }

                // Determinism guards. These are the two habits most likely to
                // reach a workflow script from ordinary JavaScript, and a
                // silent difference would be worse than an error.
                Builtin::MathRandom | Builtin::DateNow => Err(RunError::at(
                    pos,
                    format!(
                        "`{}` is not available: {DETERMINISM_MESSAGE}",
                        builtin.name()
                    ),
                )
                .into_help()),

                Builtin::MathMin => Ok(fold_numbers(&args, f64::INFINITY, f64::min)),
                Builtin::MathMax => Ok(fold_numbers(&args, f64::NEG_INFINITY, f64::max)),
                Builtin::MathFloor => Ok(Value::Number(to_number(&first).floor())),
                Builtin::MathCeil => Ok(Value::Number(to_number(&first).ceil())),
                Builtin::MathAbs => Ok(Value::Number(to_number(&first).abs())),
                Builtin::MathRound => Ok(Value::Number(to_number(&first).round())),

                Builtin::JsonStringify => {
                    let json = first.to_json();
                    let indent = args.get(1).and_then(Value::as_number).unwrap_or(0.0);
                    let text = if indent > 0.0 {
                        serde_json::to_string_pretty(&json)
                    } else {
                        serde_json::to_string(&json)
                    }
                    .map_err(|error| {
                        RunError::at(
                            pos,
                            format!("this value could not be written as JSON: {error}"),
                        )
                    })?;
                    Ok(Value::text(text))
                }
                Builtin::JsonParse => {
                    let text = first.display();
                    match serde_json::from_str::<Json>(&text) {
                        Ok(json) => Ok(Value::from_json(&json)),
                        Err(error) => Err(RunError::at(
                            pos,
                            format!("this text is not valid JSON: {error}"),
                        )),
                    }
                }

                Builtin::ObjectKeys => Ok(Value::array(
                    object_fields(&first, "Object.keys", pos)?
                        .into_iter()
                        .map(|(name, _)| Value::Text(name))
                        .collect(),
                )),
                Builtin::ObjectValues => Ok(Value::array(
                    object_fields(&first, "Object.values", pos)?
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect(),
                )),
                Builtin::ObjectEntries => Ok(Value::array(
                    object_fields(&first, "Object.entries", pos)?
                        .into_iter()
                        .map(|(name, value)| Value::array(vec![Value::Text(name), value]))
                        .collect(),
                )),

                Builtin::ArrayIsArray => Ok(Value::Bool(matches!(first, Value::Array(_)))),
                Builtin::NumberCast => Ok(Value::Number(to_number(&first))),
                Builtin::StringCast => Ok(Value::text(first.display())),
                Builtin::BooleanCast => Ok(Value::Bool(first.truthy())),
                Builtin::IsNaN => Ok(Value::Bool(to_number(&first).is_nan())),
                Builtin::ParseInt => {
                    let text = first.display();
                    Ok(Value::Number(parse_leading_number(&text, true)))
                }
                Builtin::ParseFloat => {
                    let text = first.display();
                    Ok(Value::Number(parse_leading_number(&text, false)))
                }
            }
        })
    }

    /// Start one child agent and wait for its answer.
    async fn builtin_agent(&self, args: Vec<Value>, pos: Pos) -> Result<Value, RunError> {
        let prompt = match args.first() {
            Some(Value::Text(text)) => text.to_string(),
            Some(other) => other.display(),
            None => String::new(),
        };
        if prompt.trim().is_empty() {
            return Err(RunError::at(pos, "`agent` needs a prompt"));
        }
        let options = AgentOptions::read(args.get(1), pos)?;

        let index: AgentId = self.next_index.fetch_add(1, Ordering::SeqCst);
        if index >= self.limits.max_agents {
            return Err(RunError::at(
                pos,
                format!(
                    "this run reached its limit of {} agents",
                    self.limits.max_agents
                ),
            ));
        }

        let phase = options
            .phase
            .clone()
            .unwrap_or_else(|| self.state.current_phase_title());
        let label = options
            .label
            .clone()
            .unwrap_or_else(|| format!("{phase} {}", index + 1));

        // A remembered result from an earlier run of the same script is reused,
        // but only while this position's prompt still matches.
        let remembered = self
            .journal
            .lock()
            .ok()
            .and_then(|journal| journal.take_matching(index, &prompt));
        if let Some(AgentOutcome::Done(value)) = remembered {
            self.state.register_agent(index, label, prompt);
            self.state
                .agent_finished(index, AgentStatus::Reused, Some(summarize(&value)), None, 0);
            return Ok(Value::from_json(&value));
        }
        // Anything after a position that no longer matches is stale too.
        if let Ok(mut journal) = self.journal.lock() {
            journal.invalidate_from(index);
        }

        let cancel = self.state.register_agent(index, label, prompt.clone());

        // Wait for a free slot, then for the run to be unpaused. Taking the
        // permit first keeps the number of agents in flight bounded even while
        // the run is paused.
        let _permit = match self.permits.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                self.state
                    .agent_finished(index, AgentStatus::Stopped, None, None, 0);
                return Ok(Value::Null);
            }
        };
        if !self.state.wait_while_paused().await {
            self.state
                .agent_finished(index, AgentStatus::Stopped, None, None, 0);
            return Ok(Value::Null);
        }

        self.state.agent_started(index);
        let request = AgentRequest {
            index,
            prompt: prompt.clone(),
            label: options.label,
            phase,
            model: options.model,
            effort: options.effort,
            schema: options.schema,
            timeout_ms: options.timeout_ms,
        };

        let mut outcome = self.runner.run_agent(request.clone(), cancel.clone()).await;
        let mut attempts = 0;
        while attempts < options.retries
            && matches!(outcome, AgentOutcome::Failed(_))
            && !cancel.is_cancelled()
        {
            attempts += 1;
            outcome = self.runner.run_agent(request.clone(), cancel.clone()).await;
        }

        let tokens = self.runner.tokens_used(index);
        match &outcome {
            AgentOutcome::Done(value) => {
                self.state.agent_finished(
                    index,
                    AgentStatus::Completed,
                    Some(summarize(value)),
                    None,
                    tokens,
                );
                if let Ok(mut journal) = self.journal.lock() {
                    journal.record(index, &prompt, &outcome);
                }
                Ok(Value::from_json(value))
            }
            AgentOutcome::Stopped => {
                self.state
                    .agent_finished(index, AgentStatus::Stopped, None, None, tokens);
                Ok(Value::Null)
            }
            AgentOutcome::Failed(error) => {
                self.state.agent_finished(
                    index,
                    AgentStatus::Failed,
                    None,
                    Some(error.clone()),
                    tokens,
                );
                Ok(Value::Null)
            }
        }
    }

    /// Run an array of zero-argument functions at once, keeping input order.
    async fn builtin_parallel(
        &self,
        tasks: Value,
        pos: Pos,
        depth: u32,
    ) -> Result<Value, RunError> {
        let items = tasks.elements().ok_or_else(|| {
            RunError::at(
                pos,
                format!(
                    "`parallel` needs an array of functions, but found {}",
                    tasks.kind()
                ),
            )
        })?;
        self.check_fanout(items.len(), "parallel", pos)?;

        let mut pending = Vec::with_capacity(items.len());
        for task in items {
            pending.push(self.call_value(task, Vec::new(), pos, depth));
        }
        let results = futures::future::join_all(pending).await;
        let mut values = Vec::with_capacity(results.len());
        for result in results {
            values.push(result?);
        }
        Ok(Value::array(values))
    }

    /// Send each item through every stage in turn, with items processed at the
    /// same time and results kept in input order.
    async fn builtin_pipeline(
        &self,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> Result<Value, RunError> {
        let Some(source) = args.first() else {
            return Err(RunError::at(
                pos,
                "`pipeline` needs a list and at least one stage",
            ));
        };
        let items = source.elements().ok_or_else(|| {
            RunError::at(
                pos,
                format!(
                    "`pipeline` needs an array as its first argument, but found {}",
                    source.kind()
                ),
            )
        })?;
        self.check_fanout(items.len(), "pipeline", pos)?;
        let stages: Vec<Value> = args.iter().skip(1).cloned().collect();
        if stages.is_empty() {
            return Err(RunError::at(
                pos,
                "`pipeline` needs at least one stage function after the list",
            ));
        }

        let mut pending = Vec::with_capacity(items.len());
        for item in items {
            pending.push(self.run_stages(item, &stages, pos, depth));
        }
        let results = futures::future::join_all(pending).await;
        let mut values = Vec::with_capacity(results.len());
        for result in results {
            values.push(result?);
        }
        Ok(Value::array(values))
    }

    /// Carry one item through the stages, stopping at the first null.
    ///
    /// A null means an agent was stopped or failed, so the later stages have
    /// nothing to work on and the item's result stays null.
    async fn run_stages(
        &self,
        item: Value,
        stages: &[Value],
        pos: Pos,
        depth: u32,
    ) -> Result<Value, RunError> {
        let mut current = item;
        for stage in stages {
            if matches!(current, Value::Null) {
                return Ok(Value::Null);
            }
            current = self
                .call_value(stage.clone(), vec![current], pos, depth)
                .await?;
        }
        Ok(current)
    }

    fn check_fanout(&self, count: usize, what: &str, pos: Pos) -> Result<(), RunError> {
        if count > self.limits.max_fanout {
            return Err(RunError::at(
                pos,
                format!(
                    "`{what}` was given {count} items, more than the limit of {}; \
                     split the work into smaller batches",
                    self.limits.max_fanout
                ),
            ));
        }
        Ok(())
    }

    // ----- methods on values ------------------------------------------------

    pub(super) fn call_method<'a>(
        &'a self,
        receiver: Value,
        name: &'a str,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> BoxFuture<'a, Result<Value, RunError>> {
        Box::pin(async move {
            match &receiver {
                Value::Array(_) => self.array_method(&receiver, name, args, pos, depth).await,
                Value::Text(text) => string_method(text, name, &args, pos),
                Value::Null => Err(RunError::at(
                    pos,
                    format!(
                        "cannot call `{name}` on null; an agent that failed returns null, \
                         so test the value first"
                    ),
                )),
                other => Err(RunError::at(
                    pos,
                    format!("{} has no method `{name}`", other.kind()),
                )),
            }
        })
    }

    async fn array_method(
        &self,
        receiver: &Value,
        name: &str,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> Result<Value, RunError> {
        let items = receiver.elements().unwrap_or_default();
        let first = args.first().cloned().unwrap_or(Value::Null);

        match name {
            "map" => {
                let mut out = Vec::with_capacity(items.len());
                for (position, item) in items.into_iter().enumerate() {
                    out.push(
                        self.call_value(
                            first.clone(),
                            vec![item, Value::Number(position as f64)],
                            pos,
                            depth,
                        )
                        .await?,
                    );
                }
                Ok(Value::array(out))
            }
            "filter" => {
                let mut out = Vec::new();
                for (position, item) in items.into_iter().enumerate() {
                    let keep = match &first {
                        // `filter(Boolean)` is how a script drops the nulls
                        // left by agents that failed, so it is worth handling
                        // without a call.
                        Value::Builtin(Builtin::BooleanCast) => item.truthy(),
                        _ => self
                            .call_value(
                                first.clone(),
                                vec![item.clone(), Value::Number(position as f64)],
                                pos,
                                depth,
                            )
                            .await?
                            .truthy(),
                    };
                    if keep {
                        out.push(item);
                    }
                }
                Ok(Value::array(out))
            }
            "find" => {
                for (position, item) in items.into_iter().enumerate() {
                    let matched = self
                        .call_value(
                            first.clone(),
                            vec![item.clone(), Value::Number(position as f64)],
                            pos,
                            depth,
                        )
                        .await?
                        .truthy();
                    if matched {
                        return Ok(item);
                    }
                }
                Ok(Value::Null)
            }
            "some" | "every" => {
                let want_all = name == "every";
                for (position, item) in items.into_iter().enumerate() {
                    let matched = self
                        .call_value(
                            first.clone(),
                            vec![item, Value::Number(position as f64)],
                            pos,
                            depth,
                        )
                        .await?
                        .truthy();
                    if matched != want_all {
                        return Ok(Value::Bool(!want_all));
                    }
                }
                Ok(Value::Bool(want_all))
            }
            "push" => {
                if let Value::Array(items) = receiver
                    && let Ok(mut items) = items.lock()
                {
                    items.extend(args);
                    return Ok(Value::Number(items.len() as f64));
                }
                Ok(Value::Number(0.0))
            }
            "join" => {
                let separator = match args.first() {
                    Some(value) => value.display(),
                    None => ",".to_string(),
                };
                Ok(Value::text(
                    items
                        .iter()
                        .map(|item| match item {
                            Value::Null => String::new(),
                            other => other.display(),
                        })
                        .collect::<Vec<_>>()
                        .join(&separator),
                ))
            }
            "slice" => {
                let (start, end) = slice_bounds(&args, items.len());
                Ok(Value::array(
                    items.get(start..end).unwrap_or_default().to_vec(),
                ))
            }
            "includes" => Ok(Value::Bool(
                items.iter().any(|item| item.strict_equals(&first)),
            )),
            "indexOf" => Ok(Value::Number(
                items
                    .iter()
                    .position(|item| item.strict_equals(&first))
                    .map(|position| position as f64)
                    .unwrap_or(-1.0),
            )),
            "concat" => {
                let mut out = items;
                for arg in args {
                    match arg.elements() {
                        Some(more) => out.extend(more),
                        None => out.push(arg),
                    }
                }
                Ok(Value::array(out))
            }
            "flat" => {
                let mut out = Vec::new();
                for item in items {
                    match item.elements() {
                        Some(inner) => out.extend(inner),
                        None => out.push(item),
                    }
                }
                Ok(Value::array(out))
            }
            "reverse" => {
                let mut out = items;
                out.reverse();
                Ok(Value::array(out))
            }
            "sort" => {
                if !matches!(first, Value::Null) {
                    return Err(RunError::at(
                        pos,
                        "`sort` does not take a comparison function here; sort by a text key \
                         with plain `sort()`, or ask an agent to rank the items",
                    ));
                }
                let mut out = items;
                out.sort_by_key(Value::display);
                Ok(Value::array(out))
            }
            other => Err(RunError::at(
                pos,
                format!("an array has no method `{other}`"),
            )),
        }
    }
}

fn object_fields(value: &Value, what: &str, pos: Pos) -> Result<crate::value::Fields, RunError> {
    value.fields().ok_or_else(|| {
        RunError::at(
            pos,
            format!("`{what}` needs an object, but found {}", value.kind()),
        )
    })
}

fn fold_numbers(args: &[Value], initial: f64, combine: fn(f64, f64) -> f64) -> Value {
    let mut result = initial;
    for arg in args {
        // `Math.max(...list)` has no spread here, so an array argument is
        // folded directly, which is what a script means by it.
        match arg.elements() {
            Some(items) => {
                for item in items {
                    result = combine(result, to_number(&item));
                }
            }
            None => result = combine(result, to_number(arg)),
        }
    }
    Value::Number(result)
}

/// Resolve `slice` arguments, including negative offsets from the end.
fn slice_bounds(args: &[Value], length: usize) -> (usize, usize) {
    let resolve = |value: Option<&Value>, fallback: usize| -> usize {
        let Some(number) = value.and_then(Value::as_number) else {
            return fallback;
        };
        if number < 0.0 {
            length.saturating_sub(number.abs() as usize)
        } else {
            (number as usize).min(length)
        }
    };
    let start = resolve(args.first(), 0);
    let end = resolve(args.get(1), length);
    (start, end.max(start))
}

fn string_method(text: &Arc<str>, name: &str, args: &[Value], pos: Pos) -> Result<Value, RunError> {
    let first = args.first().map(Value::display).unwrap_or_default();
    Ok(match name {
        "split" => {
            let parts: Vec<Value> = if first.is_empty() {
                text.chars()
                    .map(|character| Value::text(character.to_string()))
                    .collect()
            } else {
                text.split(first.as_str()).map(Value::text).collect()
            };
            Value::array(parts)
        }
        "trim" => Value::text(text.trim()),
        "toLowerCase" => Value::text(text.to_lowercase()),
        "toUpperCase" => Value::text(text.to_uppercase()),
        "includes" => Value::Bool(text.contains(first.as_str())),
        "startsWith" => Value::Bool(text.starts_with(first.as_str())),
        "endsWith" => Value::Bool(text.ends_with(first.as_str())),
        "indexOf" => Value::Number(
            text.find(first.as_str())
                .map(|byte| text[..byte].chars().count() as f64)
                .unwrap_or(-1.0),
        ),
        "replace" => Value::text(text.replacen(
            first.as_str(),
            &args.get(1).map(Value::display).unwrap_or_default(),
            1,
        )),
        "replaceAll" => Value::text(text.replace(
            first.as_str(),
            &args.get(1).map(Value::display).unwrap_or_default(),
        )),
        "slice" => {
            let characters: Vec<char> = text.chars().collect();
            let (start, end) = slice_bounds(args, characters.len());
            Value::text(
                characters
                    .get(start..end)
                    .unwrap_or_default()
                    .iter()
                    .collect::<String>(),
            )
        }
        "padStart" | "padEnd" => {
            let width = args
                .first()
                .and_then(Value::as_number)
                .unwrap_or(0.0)
                .max(0.0) as usize;
            let filler = args
                .get(1)
                .map(Value::display)
                .filter(|filler| !filler.is_empty())
                .unwrap_or_else(|| " ".to_string());
            let current = text.chars().count();
            if current >= width {
                return Ok(Value::Text(text.clone()));
            }
            let padding: String = filler
                .chars()
                .cycle()
                .take(width.saturating_sub(current))
                .collect();
            if name == "padStart" {
                Value::text(format!("{padding}{text}"))
            } else {
                Value::text(format!("{text}{padding}"))
            }
        }
        other => {
            return Err(RunError::at(
                pos,
                format!("a string has no method `{other}`"),
            ));
        }
    })
}

/// The text an agent produced, shortened for the progress view.
fn summarize(value: &Json) -> String {
    let text = match value {
        Json::String(text) => text.clone(),
        Json::Number(number) => number
            .as_f64()
            .map(format_number)
            .unwrap_or_else(|| number.to_string()),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    const LIMIT: usize = 4000;
    if text.chars().count() <= LIMIT {
        return text;
    }
    let kept: String = text.chars().take(LIMIT).collect();
    format!("{kept}\n… truncated")
}

/// Read a number from the front of some text, the way `parseInt` does.
fn parse_leading_number(text: &str, whole: bool) -> f64 {
    let trimmed = text.trim_start();
    let mut end = 0;
    let mut seen_dot = false;
    for (offset, character) in trimmed.char_indices() {
        let acceptable = character.is_ascii_digit()
            || (offset == 0 && (character == '-' || character == '+'))
            || (!whole && character == '.' && !seen_dot);
        if !acceptable {
            break;
        }
        if character == '.' {
            seen_dot = true;
        }
        end = offset + character.len_utf8();
    }
    trimmed
        .get(..end)
        .and_then(|head| head.parse().ok())
        .unwrap_or(f64::NAN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_bounds_handle_offsets_from_the_end() {
        assert_eq!(slice_bounds(&[], 5), (0, 5));
        assert_eq!(slice_bounds(&[Value::Number(1.0)], 5), (1, 5));
        assert_eq!(
            slice_bounds(&[Value::Number(1.0), Value::Number(3.0)], 5),
            (1, 3)
        );
        assert_eq!(slice_bounds(&[Value::Number(-2.0)], 5), (3, 5));
        // An end before the start yields an empty range rather than a panic.
        assert_eq!(
            slice_bounds(&[Value::Number(4.0), Value::Number(1.0)], 5),
            (4, 4)
        );
        // An offset past the end is clamped.
        assert_eq!(slice_bounds(&[Value::Number(99.0)], 5), (5, 5));
    }

    #[test]
    fn parse_leading_number_stops_at_the_first_other_character() {
        assert_eq!(parse_leading_number("42abc", true), 42.0);
        assert_eq!(parse_leading_number("  -7 ", true), -7.0);
        assert_eq!(parse_leading_number("3.9", true), 3.0);
        assert_eq!(parse_leading_number("3.9", false), 3.9);
        assert!(parse_leading_number("abc", true).is_nan());
    }

    #[test]
    fn a_long_agent_answer_is_shortened_for_the_progress_view() {
        let long = Json::String("x".repeat(9000));
        let shown = summarize(&long);
        assert!(shown.ends_with("… truncated"));
        assert!(shown.chars().count() < 4100);
    }

    #[test]
    fn unknown_agent_options_are_rejected_by_name() {
        let options = Value::object(vec![(Arc::from("labell"), Value::text("x"))]);
        let error = AgentOptions::read(Some(&options), Pos { line: 1, column: 1 }).unwrap_err();
        assert!(error.message.contains("`labell` is not an option"));
        assert!(error.message.contains("timeoutMs"));
    }

    #[test]
    fn agent_options_are_read_from_an_object() {
        let options = Value::object(vec![
            (Arc::from("label"), Value::text("audit a.rs")),
            (Arc::from("model"), Value::text("sonnet")),
            (Arc::from("retries"), Value::Number(2.0)),
        ]);
        let read = AgentOptions::read(Some(&options), Pos { line: 1, column: 1 }).unwrap();
        assert_eq!(read.label.as_deref(), Some("audit a.rs"));
        assert_eq!(read.model.as_deref(), Some("sonnet"));
        assert_eq!(read.retries, 2);
    }
}
