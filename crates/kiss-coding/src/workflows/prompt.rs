//! The reference the model works from when it writes a workflow script.
//!
//! This text is sent only on a turn where workflow mode is armed, because it is
//! long and most turns do not need it. It has to be accurate: it is the only
//! description of the language the model gets, and every inaccuracy costs a
//! round trip through a parse error. The test at the bottom parses every
//! example here with the real parser, so this file cannot drift away from the
//! implementation.

use crate::settings::WorkflowSize;

/// Find the complete word or phrase that asks for a dynamic workflow.
///
/// This function is shared by interactive, print, and JSON prompt entry
/// points, so the same user text has the same meaning in every mode.
pub fn workflow_trigger(text: &str) -> Option<&'static str> {
    let lowered = text.to_lowercase();
    let words = lowered
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    for (phrase, phrase_words) in [
        ("use a workflow", &["use", "a", "workflow"][..]),
        ("run a workflow", &["run", "a", "workflow"][..]),
        ("as a workflow", &["as", "a", "workflow"][..]),
        ("dynamic workflow", &["dynamic", "workflow"][..]),
    ] {
        if words
            .windows(phrase_words.len())
            .any(|window| window == phrase_words)
        {
            return Some(phrase);
        }
    }
    words.contains(&"ultracode").then_some("ultracode")
}

/// Two complete scripts, kept separate so the test can parse each one.
pub(crate) const FAN_OUT_EXAMPLE: &str = r#"export const meta = {
  name: 'audit-tools',
  description: 'Audit every tool file for missing path checks',
  phases: [{ title: 'Discover' }, { title: 'Audit' }, { title: 'Report' }],
}

phase('Discover')
const found = await agent('List every .rs file under crates/kiss-coding/src/tools. Return the paths.', {
  schema: {
    type: 'object',
    required: ['files'],
    properties: { files: { type: 'array', items: { type: 'string' } } },
  },
})
log(`auditing ${found.files.length} files`)

phase('Audit')
const findings = await pipeline(
  found.files,
  file => agent(`Audit ${file} for paths used without validation. Report each one.`, { label: file }),
  finding => agent(`Try to refute this finding. Reply "confirmed" or "refuted" and why.\n\n${finding}`),
)

phase('Report')
const kept = findings.filter(Boolean)
return await agent(`Merge these audits into one ranked list, most severe first.\n\n${kept.join('\n\n')}`)
"#;

/// A fixed fan-out, where the agent count is known before the run starts.
pub(crate) const PERSPECTIVES_EXAMPLE: &str = r#"export const meta = {
  name: 'three-angles',
  description: 'Draft a plan from three angles, then choose between them',
  phases: [{ title: 'Draft' }, { title: 'Choose' }],
}

phase('Draft')
const drafts = await parallel([
  () => agent(`Plan this work for correctness above all.\n\n${args}`, { label: 'correctness' }),
  () => agent(`Plan this work for the smallest change.\n\n${args}`, { label: 'smallest' }),
  () => agent(`Plan this work for long-term maintenance.\n\n${args}`, { label: 'maintenance' }),
])

phase('Choose')
const usable = drafts.filter(Boolean)
if (usable.length === 0) {
  return 'every draft failed'
}
return await agent(`Weigh these plans against each other and recommend one.\n\n${usable.join('\n\n---\n\n')}`)
"#;

/// Build the instructions, including the size advice the user chose.
pub fn authoring_prompt(size: WorkflowSize, max_agents: u32, max_fanout: usize) -> String {
    let size_advice = match size.target_agents() {
        Some(target) => format!(
            "Aim for fewer than {target} agents unless the task clearly needs more. \
             This is guidance, not a limit."
        ),
        None => "Size the workflow to the task; no agent-count guideline is set.".to_string(),
    };

    format!(
        "# Writing a dynamic workflow\n\
         \n\
         The user asked for this task to run as a dynamic workflow. Call the `run_workflow` tool \
         with a script that orchestrates child agents, instead of working through the task turn by \
         turn yourself.\n\
         \n\
         Write the script once and let it run. Its intermediate results stay in script variables \
         rather than in your context, which is what lets one run coordinate far more agents than a \
         conversation can. {size_advice}\n\
         \n\
         ## The language\n\
         \n\
         A script is a small, fixed subset of JavaScript. It is not JavaScript: anything outside \
         this list is an error.\n\
         \n\
         Statements: `const`, `let`, assignment, `if` / `else`, `for (const item of list)`, \
         `while`, `break`, `continue`, `return`, and an expression on its own. Top-level `await` \
         is allowed.\n\
         \n\
         Expressions: numbers, strings, back-quoted template strings with `${{...}}`, `true`, \
         `false`, `null`, array and object literals, `.field`, `[index]`, calls, arrow functions, \
         `await`, `!`, unary `-`, `+ - * / %`, `=== !==`, `< <= > >=`, `&& || ??`, and `a ? b : c`.\n\
         \n\
         Not available, with what to use instead:\n\
         - counted `for` loops, `++`, `--`: use `for (const item of list)` or `pipeline(...)`\n\
         - `==`, `!=`: use `===` and `!==`\n\
         - `function`, `class`, `var`: use `const` and arrow functions\n\
         - `try` / `catch` / `throw`: an agent that fails returns null, so test for null instead\n\
         - `import`, `require`: a script loads no modules and touches no files\n\
         - `typeof`, `instanceof`: use `Array.isArray(value)` or `value === null`\n\
         - `...` spread, `?.`, destructuring, computed object keys\n\
         - `Date.now()`, `Math.random()`, `new Date()`: a script must be repeatable so a stopped \
         run can resume. Pass a timestamp or a seed in through `args`.\n\
         \n\
         ## What a script can call\n\
         \n\
         - `agent(prompt, options?)` starts one child agent and waits for its answer.\n\
         - `parallel(tasks)` runs an array of zero-argument functions at once and returns their \
         results in input order.\n\
         - `pipeline(items, ...stages)` sends every item through each stage in turn, with items \
         processed at the same time and results in input order.\n\
         - `phase(title)` names the group the agents after it belong to, for the progress view.\n\
         - `log(message)` shows one line above the phases in the progress view.\n\
         - `args` is the input the workflow was invoked with. `cwd` is the working directory.\n\
         - `JSON.stringify`, `JSON.parse`, `Object.keys`, `Object.values`, `Object.entries`, \
         `Array.isArray`, `Math.min`, `Math.max`, `Math.floor`, `Math.ceil`, `Math.abs`, \
         `Math.round`, `Number`, `String`, `Boolean`, `parseInt`, `parseFloat`, `isNaN`.\n\
         - On arrays: `length`, `map`, `filter`, `find`, `some`, `every`, `slice`, `join`, `push`, \
         `includes`, `indexOf`, `concat`, `flat`, `reverse`, `sort`.\n\
         - On strings: `length`, `split`, `trim`, `toLowerCase`, `toUpperCase`, `includes`, \
         `indexOf`, `startsWith`, `endsWith`, `slice`, `replace`, `replaceAll`, `padStart`, \
         `padEnd`.\n\
         \n\
         `agent()` options, all optional: `label` for the progress view, `phase` to override the \
         current phase, `model` such as `sonnet` or `haiku`, `effort` such as `low` or `high`, \
         `schema` for a JSON Schema the answer must match, `timeoutMs`, and `retries`.\n\
         \n\
         ## Two rules that decide whether a script works\n\
         \n\
         First, `agent()` returns **null** when that agent is stopped or fails. `parallel` and \
         `pipeline` keep those nulls in place so the results line up with the input. Always drop \
         them before using the results, with `.filter(Boolean)`, and never read a field off an \
         agent result without knowing it is not null.\n\
         \n\
         Second, `agent()` returns **text** unless you pass a `schema`. Pass a schema whenever the \
         script needs to read fields or iterate a list out of the answer; without one you get a \
         string and `.files` on it is an error.\n\
         \n\
         ## Limits\n\
         \n\
         Up to {max_agents} agents in one run, up to {max_fanout} items in a single `parallel` or \
         `pipeline` call, and up to 16 agents running at once. A script that exceeds one of these \
         fails rather than quietly doing less.\n\
         \n\
         ## Write a good workflow, not just a big one\n\
         \n\
         The value of a workflow is the pattern, not the agent count. Prefer shapes that produce a \
         more trustworthy answer than one pass would: have independent agents check or try to \
         refute each other's findings, draft from several angles and weigh them, or repeat a \
         check-and-fix round until it stops making progress. Give each agent one bounded task and \
         enough context to do it without guessing.\n\
         \n\
         Agents share one working directory. Two agents told to edit the same file will conflict, \
         so give parallel writers separate files, or fan out for reading and keep the writing in \
         one place.\n\
         \n\
         ## Example: fan out over files, then verify\n\
         \n\
         {FAN_OUT_EXAMPLE}\n\
         ## Example: several angles, then choose\n\
         \n\
         {PERSPECTIVES_EXAMPLE}\n\
         Give `run_workflow` a short kebab-case `name`, a one-sentence `description`, and the \
         `script`. If the script does not parse, the error names the line and a supported \
         alternative: fix it and call the tool again.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_examples_parse_with_the_real_parser() {
        // The instructions are the only description of the language the model
        // receives. If an example here stopped parsing, every workflow would
        // start with a wasted round trip.
        for (name, source) in [
            ("fan out", FAN_OUT_EXAMPLE),
            ("perspectives", PERSPECTIVES_EXAMPLE),
        ] {
            let script = kiss_workflow::Script::parse(source)
                .unwrap_or_else(|error| panic!("the {name} example must parse: {error}"));
            assert!(!script.meta().name.is_empty());
            assert!(!script.meta().description.is_empty());
        }
    }

    #[test]
    fn the_fan_out_example_declares_the_phases_it_uses() {
        let script = kiss_workflow::Script::parse(FAN_OUT_EXAMPLE).unwrap();
        assert_eq!(script.declared_phases(), ["Discover", "Audit", "Report"]);
        // Its fan-out depends on a list fetched at run time, so the agent count
        // is deliberately not predicted.
        assert_eq!(script.estimated_agents(), None);
    }

    #[test]
    fn the_perspectives_example_has_a_known_agent_count() {
        let script = kiss_workflow::Script::parse(PERSPECTIVES_EXAMPLE).unwrap();
        // Three drafts plus the one that chooses between them.
        assert_eq!(script.estimated_agents(), Some(4));
    }

    #[test]
    fn the_size_guideline_reaches_the_instructions() {
        let small = authoring_prompt(WorkflowSize::Small, 1000, 4096);
        assert!(small.contains("fewer than 5 agents"));

        let unrestricted = authoring_prompt(WorkflowSize::Unrestricted, 1000, 4096);
        assert!(unrestricted.contains("no agent-count guideline"));
    }

    #[test]
    fn the_instructions_state_the_two_rules_that_break_scripts() {
        let text = authoring_prompt(WorkflowSize::Medium, 1000, 4096);
        assert!(text.contains("filter(Boolean)"));
        assert!(text.contains("schema"));
        assert!(text.contains("Date.now()"));
        // The template placeholder must survive formatting rather than being
        // eaten as a format argument.
        assert!(text.contains("${...}"));
    }
}
