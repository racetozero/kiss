//! One run of one script.

use crate::interp::{Interp, RunError};
use crate::progress::{RunSnapshot, RunState, RunStatus};
use crate::runner::{AgentId, AgentRunner, Journal, Limits};
use crate::script::Script;
use serde_json::Value as Json;
use std::sync::Arc;
use tokio::sync::watch;

/// A prepared run.
///
/// Building a workflow starts nothing. [`Workflow::run`] does the work, while
/// the control methods and [`Workflow::snapshot`] can be called from another
/// task at any time, which is what lets a user interface watch and steer a run
/// that is already going.
pub struct Workflow {
    script: Arc<Script>,
    state: Arc<RunState>,
    interp: Interp,
}

impl Workflow {
    pub fn new(
        script: Script,
        args: Json,
        cwd: String,
        runner: Arc<dyn AgentRunner>,
        limits: Limits,
    ) -> Workflow {
        Workflow::with_journal(script, args, cwd, runner, limits, Journal::default())
    }

    /// Build a run that reuses results from an earlier run of the same script.
    pub fn with_journal(
        script: Script,
        args: Json,
        cwd: String,
        runner: Arc<dyn AgentRunner>,
        limits: Limits,
        journal: Journal,
    ) -> Workflow {
        let limits = limits.sanitized();
        let script = Arc::new(script);
        let state = RunState::new(script.declared_phases());
        let interp = Interp::new(
            script.clone(),
            state.clone(),
            runner,
            limits,
            args,
            cwd,
            journal,
        );
        Workflow {
            script,
            state,
            interp,
        }
    }

    pub fn script(&self) -> &Script {
        &self.script
    }

    /// Run the script to completion.
    ///
    /// A stopped run is not an error: it returns whatever the script had
    /// produced, so completed work is not thrown away.
    pub async fn run(&self) -> Result<Json, RunError> {
        let result = self.interp.run().await;
        match &result {
            Ok(_) => self.state.finish(RunStatus::Completed, None),
            Err(error) if self.state.stop_token().is_cancelled() => {
                self.state.finish(RunStatus::Stopped, None);
                return Ok(Json::Null);
            }
            Err(error) => self
                .state
                .finish(RunStatus::Failed, Some(error.to_string())),
        }
        result
    }

    /// Results worth keeping if this run is launched again.
    pub fn journal(&self) -> Journal {
        self.interp.journal()
    }

    pub fn snapshot(&self) -> Arc<RunSnapshot> {
        self.state.snapshot()
    }

    /// A receiver that changes whenever the run's state does, so a viewer
    /// redraws on change instead of polling.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.state.subscribe()
    }

    pub fn pause(&self) {
        self.state.pause();
    }

    pub fn resume(&self) {
        self.state.resume();
    }

    pub fn is_paused(&self) -> bool {
        self.state.is_paused()
    }

    /// Stop the whole run, cancelling every agent still working.
    pub fn stop(&self) {
        self.state.stop();
    }

    /// Stop one agent. Its `agent()` call returns null, and the script carries
    /// on.
    pub fn stop_agent(&self, id: AgentId) {
        self.state.stop_agent(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{AgentOutcome, AgentRequest};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio_util::sync::CancellationToken;

    /// A runner that answers immediately and records what it was asked.
    #[derive(Default)]
    struct FakeRunner {
        seen: Mutex<Vec<AgentRequest>>,
        calls: AtomicU32,
        /// Prompts that should fail rather than answer.
        fail_on: Vec<String>,
    }

    impl FakeRunner {
        fn shared() -> Arc<FakeRunner> {
            Arc::new(FakeRunner::default())
        }

        fn prompts(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.prompt.clone())
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl AgentRunner for FakeRunner {
        async fn run_agent(
            &self,
            request: AgentRequest,
            _cancel: CancellationToken,
        ) -> AgentOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let prompt = request.prompt.clone();
            self.seen.lock().unwrap().push(request);
            if self.fail_on.iter().any(|needle| prompt.contains(needle)) {
                return AgentOutcome::Failed("the agent failed".into());
            }
            AgentOutcome::Done(Json::String(format!("answer to {prompt}")))
        }

        fn tokens_used(&self, _index: AgentId) -> u64 {
            10
        }
    }

    async fn run_script(source: &str) -> Result<Json, RunError> {
        run_with(source, FakeRunner::shared(), Json::Null).await
    }

    async fn run_with(source: &str, runner: Arc<FakeRunner>, args: Json) -> Result<Json, RunError> {
        let script = Script::parse(source).expect("script parses");
        Workflow::new(script, args, "/repo".into(), runner, Limits::default())
            .run()
            .await
    }

    const HEADER: &str = "export const meta = { name: 'test', description: 'A test workflow' }\n";

    fn script(body: &str) -> String {
        format!("{HEADER}{body}")
    }

    #[tokio::test]
    async fn a_script_returns_the_value_it_returns() {
        let result = run_script(&script("return 1 + 2")).await.unwrap();
        assert_eq!(result, serde_json::json!(3));
    }

    #[tokio::test]
    async fn an_agent_answer_flows_into_the_script() {
        let result = run_script(&script("const a = await agent('read the file')\nreturn a"))
            .await
            .unwrap();
        assert_eq!(result, Json::String("answer to read the file".into()));
    }

    #[tokio::test]
    async fn parallel_keeps_input_order() {
        let runner = FakeRunner::shared();
        let result = run_with(
            &script(
                "const out = await parallel([\n\
                 () => agent('one'),\n\
                 () => agent('two'),\n\
                 () => agent('three'),\n\
                 ])\nreturn out",
            ),
            runner.clone(),
            Json::Null,
        )
        .await
        .unwrap();
        let answers: Vec<&str> = result
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(
            answers,
            ["answer to one", "answer to two", "answer to three"]
        );
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn pipeline_carries_each_item_through_every_stage() {
        let runner = FakeRunner::shared();
        let result = run_with(
            &script(
                "const out = await pipeline(['a.rs', 'b.rs'],\n\
                 file => agent(`audit ${file}`),\n\
                 finding => agent(`verify ${finding}`),\n\
                 )\nreturn out",
            ),
            runner.clone(),
            Json::Null,
        )
        .await
        .unwrap();
        assert_eq!(result.as_array().map(Vec::len), Some(2));
        let prompts = runner.prompts();
        assert!(prompts.contains(&"audit a.rs".to_string()));
        assert!(prompts.contains(&"verify answer to audit a.rs".to_string()));
        assert!(prompts.contains(&"verify answer to audit b.rs".to_string()));
    }

    #[tokio::test]
    async fn a_failed_agent_becomes_null_and_the_script_continues() {
        let runner = Arc::new(FakeRunner {
            fail_on: vec!["b.rs".into()],
            ..FakeRunner::default()
        });
        let result = run_with(
            &script(
                "const out = await pipeline(['a.rs', 'b.rs'], file => agent(`audit ${file}`))\n\
                 return out",
            ),
            runner,
            Json::Null,
        )
        .await
        .unwrap();
        let items = result.as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_string());
        assert!(items[1].is_null());
    }

    #[tokio::test]
    async fn filter_boolean_drops_the_nulls_a_failure_leaves() {
        let runner = Arc::new(FakeRunner {
            fail_on: vec!["b.rs".into()],
            ..FakeRunner::default()
        });
        let result = run_with(
            &script(
                "const out = await pipeline(['a.rs', 'b.rs'], file => agent(`audit ${file}`))\n\
                 return out.filter(Boolean)",
            ),
            runner,
            Json::Null,
        )
        .await
        .unwrap();
        assert_eq!(result.as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn a_stage_after_a_failure_is_skipped() {
        let runner = Arc::new(FakeRunner {
            fail_on: vec!["audit b.rs".into()],
            ..FakeRunner::default()
        });
        run_with(
            &script(
                "const out = await pipeline(['a.rs', 'b.rs'],\n\
                 file => agent(`audit ${file}`),\n\
                 finding => agent(`verify ${finding}`),\n\
                 )\nreturn out",
            ),
            runner.clone(),
            Json::Null,
        )
        .await
        .unwrap();
        // Three calls, not four: the verify stage never ran for b.rs.
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn args_reach_the_script_as_structured_data() {
        let result = run_with(
            &script("return args.issues.length"),
            FakeRunner::shared(),
            serde_json::json!({"issues": [1, 2, 3]}),
        )
        .await
        .unwrap();
        assert_eq!(result, serde_json::json!(3));
    }

    #[tokio::test]
    async fn phases_group_the_agents_that_follow_them() {
        let script_text = script(
            "phase('Discover')\nconst a = await agent('find')\n\
             phase('Audit')\nconst b = await agent('check')\nreturn [a, b]",
        );
        let parsed = Script::parse(&script_text).unwrap();
        let workflow = Workflow::new(
            parsed,
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits::default(),
        );
        workflow.run().await.unwrap();
        let snapshot = workflow.snapshot();
        assert_eq!(snapshot.phases.len(), 2);
        assert_eq!(snapshot.phases[0].title, "Discover");
        assert_eq!(snapshot.phases[0].agents.len(), 1);
        assert_eq!(snapshot.phases[1].agents.len(), 1);
        assert_eq!(snapshot.status, RunStatus::Completed);
        assert_eq!(snapshot.tokens, 20);
    }

    #[tokio::test]
    async fn log_lines_reach_the_progress_view() {
        let parsed = Script::parse(&script("log('starting the sweep')\nreturn 1")).unwrap();
        let workflow = Workflow::new(
            parsed,
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits::default(),
        );
        workflow.run().await.unwrap();
        assert_eq!(workflow.snapshot().log, ["starting the sweep"]);
    }

    // ----- determinism ------------------------------------------------------

    #[tokio::test]
    async fn the_clock_and_the_random_generator_are_unavailable() {
        let error = run_script(&script("return Date.now()")).await.unwrap_err();
        assert!(error.message.contains("repeatable"));
        assert!(error.message.contains("through `args`"));

        let error = run_script(&script("return Math.random()"))
            .await
            .unwrap_err();
        assert!(error.message.contains("repeatable"));

        let error = run_script(&script("return new Date()")).await.unwrap_err();
        assert!(error.message.contains("repeatable"));
    }

    // ----- limits -----------------------------------------------------------

    #[tokio::test]
    async fn an_oversized_fan_out_is_refused_rather_than_truncated() {
        let parsed = Script::parse(&script(
            "const items = []\n\
             while (items.length < 10) { items.push(items.length) }\n\
             return await pipeline(items, item => agent(`check ${item}`))",
        ))
        .unwrap();
        let workflow = Workflow::new(
            parsed,
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits {
                max_fanout: 4,
                ..Limits::default()
            },
        );
        let error = workflow.run().await.unwrap_err();
        assert!(error.message.contains("more than the limit of 4"));
        assert!(error.message.contains("smaller batches"));
    }

    #[tokio::test]
    async fn the_agent_cap_stops_a_runaway_script() {
        let parsed = Script::parse(&script(
            "let count = 0\nwhile (true) { await agent(`check ${count}`)\ncount = count + 1 }",
        ))
        .unwrap();
        let workflow = Workflow::new(
            parsed,
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits {
                max_agents: 5,
                ..Limits::default()
            },
        );
        let error = workflow.run().await.unwrap_err();
        assert!(error.message.contains("limit of 5 agents"));
    }

    #[tokio::test]
    async fn the_step_cap_stops_a_loop_that_never_ends() {
        let parsed = Script::parse(&script("while (true) { const a = 1 }")).unwrap();
        let workflow = Workflow::new(
            parsed,
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits {
                max_steps: 5_000,
                ..Limits::default()
            },
        );
        let error = workflow.run().await.unwrap_err();
        assert!(error.message.contains("steps and was stopped"));
    }

    #[tokio::test]
    async fn endless_recursion_is_an_error_not_a_crash() {
        let error = run_script(&script("const f = x => f(x)\nreturn f(1)"))
            .await
            .unwrap_err();
        assert!(error.message.contains("deep"));
    }

    // ----- resume -----------------------------------------------------------

    #[tokio::test]
    async fn a_relaunched_run_reuses_results_up_to_the_first_changed_prompt() {
        let first_source = script(
            "const a = await agent('step one')\n\
             const b = await agent('step two')\n\
             const c = await agent('step three')\n\
             return [a, b, c]",
        );
        let runner = FakeRunner::shared();
        let workflow = Workflow::new(
            Script::parse(&first_source).unwrap(),
            Json::Null,
            "/repo".into(),
            runner.clone(),
            Limits::default(),
        );
        workflow.run().await.unwrap();
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);
        let journal = workflow.journal();
        assert_eq!(journal.len(), 3);

        // The second agent's prompt changes, so it and everything after it run
        // again while the first is replayed.
        let edited_source = script(
            "const a = await agent('step one')\n\
             const b = await agent('step two, revised')\n\
             const c = await agent('step three')\n\
             return [a, b, c]",
        );
        let second_runner = FakeRunner::shared();
        let resumed = Workflow::with_journal(
            Script::parse(&edited_source).unwrap(),
            Json::Null,
            "/repo".into(),
            second_runner.clone(),
            Limits::default(),
            journal,
        );
        resumed.run().await.unwrap();
        assert_eq!(second_runner.calls.load(Ordering::SeqCst), 2);
        assert_eq!(second_runner.prompts(), ["step two, revised", "step three"]);

        let snapshot = resumed.snapshot();
        let statuses: Vec<_> = snapshot.phases[0]
            .agents
            .iter()
            .map(|agent| agent.status)
            .collect();
        assert_eq!(statuses[0], crate::progress::AgentStatus::Reused);
        assert_eq!(statuses[1], crate::progress::AgentStatus::Completed);
    }

    #[tokio::test]
    async fn an_unchanged_relaunch_starts_no_agents_at_all() {
        let source = script("const a = await agent('one')\nreturn a");
        let first = Workflow::new(
            Script::parse(&source).unwrap(),
            Json::Null,
            "/repo".into(),
            FakeRunner::shared(),
            Limits::default(),
        );
        let original = first.run().await.unwrap();

        let runner = FakeRunner::shared();
        let again = Workflow::with_journal(
            Script::parse(&source).unwrap(),
            Json::Null,
            "/repo".into(),
            runner.clone(),
            Limits::default(),
            first.journal(),
        );
        assert_eq!(again.run().await.unwrap(), original);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 0);
    }

    // ----- control ----------------------------------------------------------

    #[tokio::test]
    async fn stopping_a_run_ends_it_without_losing_finished_work() {
        let parsed = Script::parse(&script(
            "const out = []\n\
             for (const item of args) { out.push(await agent(`check ${item}`)) }\n\
             return out",
        ))
        .unwrap();
        let workflow = Arc::new(Workflow::new(
            parsed,
            serde_json::json!([1, 2, 3, 4, 5]),
            "/repo".into(),
            FakeRunner::shared(),
            Limits::default(),
        ));
        workflow.stop();
        let result = workflow.run().await.unwrap();
        assert_eq!(result, Json::Null);
        assert_eq!(workflow.snapshot().status, RunStatus::Stopped);
    }

    #[tokio::test]
    async fn a_run_reports_the_line_that_failed() {
        let error = run_script(&script("const a = 1\nreturn a.missingMethod()"))
            .await
            .unwrap_err();
        // Line 1 is the meta block, so the failure is on line 3.
        assert_eq!(error.line, 3);
        assert!(error.message.contains("missingMethod"));
    }

    #[tokio::test]
    async fn reading_a_field_from_a_failed_agent_explains_the_null() {
        let runner = Arc::new(FakeRunner {
            fail_on: vec!["find".into()],
            ..FakeRunner::default()
        });
        let error = run_with(
            &script("const found = await agent('find files')\nreturn found.files"),
            runner,
            Json::Null,
        )
        .await
        .unwrap_err();
        assert!(error.message.contains("an agent that failed returns null"));
    }
}
