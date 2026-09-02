//! A parsed, ready-to-run workflow script.

use crate::ast::{Ast, Expr, ExprId, FnBody, Span, Stmt};
use crate::diagnostic::Diagnostic;
use crate::lexer::{Interner, Lexer, Symbol};
use crate::parser;
use serde_json::{Map, Value};
use std::sync::Arc;

/// A name the script does not declare itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Global {
    Agent,
    Parallel,
    Pipeline,
    Phase,
    Log,
    Args,
    Cwd,
    Math,
    Json,
    ObjectNamespace,
    ArrayNamespace,
    /// Present only so that `Date.now()` and `new Date()` can explain the
    /// determinism rule rather than failing as an unknown name.
    DateNamespace,
    NumberCast,
    StringCast,
    BooleanCast,
    ParseInt,
    ParseFloat,
    IsNaN,
}

fn global_for(name: &str) -> Option<Global> {
    Some(match name {
        "agent" => Global::Agent,
        "parallel" => Global::Parallel,
        "pipeline" => Global::Pipeline,
        "phase" => Global::Phase,
        "log" => Global::Log,
        "args" => Global::Args,
        "cwd" => Global::Cwd,
        "Math" => Global::Math,
        "JSON" => Global::Json,
        "Object" => Global::ObjectNamespace,
        "Array" => Global::ArrayNamespace,
        "Date" => Global::DateNamespace,
        "Number" => Global::NumberCast,
        "String" => Global::StringCast,
        "Boolean" => Global::BooleanCast,
        "parseInt" => Global::ParseInt,
        "parseFloat" => Global::ParseFloat,
        "isNaN" => Global::IsNaN,
        _ => return None,
    })
}

/// The `export const meta = { ... }` block at the top of a script.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Meta {
    pub name: String,
    pub description: String,
    /// Phase titles declared in `meta`, in order.
    pub phases: Vec<String>,
}

/// A parsed workflow script.
///
/// Parsing is separate from running so that a script can be shown to the user
/// for approval, and so that a bad script produces a diagnostic the model can
/// act on before any agent starts.
pub struct Script {
    pub(crate) ast: Ast,
    pub(crate) body: Span,
    pub(crate) interner: Interner,
    /// Global meaning for each interned symbol, indexed by symbol. Resolving a
    /// free name is one bounds-checked index rather than a string comparison.
    pub(crate) globals: Vec<Option<Global>>,
    meta: Meta,
    source: Arc<str>,
    phase_titles: Vec<String>,
    estimated_agents: Option<u32>,
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("meta", &self.meta)
            .field("statements", &self.body.len)
            .finish()
    }
}

impl Script {
    /// Parse `source`, or return the first error with the position that caused
    /// it.
    pub fn parse(source: &str) -> Result<Script, Diagnostic> {
        let mut interner = Interner::default();
        let tokens = Lexer::tokenize(source, &mut interner)?;
        let parsed = parser::parse(&tokens, &mut interner)?;

        let meta = match parsed.meta {
            Some(expr) => read_meta(&parsed.ast, &interner, expr)?,
            None => {
                return Err(
                    Diagnostic::new(1, 1, "this script has no `meta` block").with_help(
                        "start the script with \
                     `export const meta = { name: '...', description: '...' }`",
                    ),
                );
            }
        };

        let globals = (0..interner.len())
            .map(|symbol| global_for(interner.resolve(symbol as Symbol)))
            .collect();

        let mut script = Script {
            ast: parsed.ast,
            body: parsed.body,
            interner,
            globals,
            meta,
            source: Arc::from(source),
            phase_titles: Vec::new(),
            estimated_agents: None,
        };
        script.phase_titles = script.collect_phase_titles();
        script.estimated_agents = script.count_agents();
        Ok(script)
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every phase title the run will show, in the order they first appear.
    ///
    /// Titles declared in `meta` come first, then any `phase()` title the
    /// script uses that `meta` did not declare.
    pub fn declared_phases(&self) -> &[String] {
        &self.phase_titles
    }

    /// How many agents the run will start, when that is knowable before it
    /// runs.
    ///
    /// `None` means the count depends on data the script has not fetched yet,
    /// such as a list of files an earlier agent returns. The approval prompt
    /// says "unbounded" rather than guessing, because a number shown there has
    /// to be one the user can rely on.
    pub fn estimated_agents(&self) -> Option<u32> {
        self.estimated_agents
    }

    fn symbol_is(&self, symbol: Symbol, global: Global) -> bool {
        self.globals
            .get(symbol as usize)
            .copied()
            .flatten()
            .is_some_and(|found| found == global)
    }

    fn collect_phase_titles(&self) -> Vec<String> {
        let mut titles = self.meta.phases.clone();
        for expr in &self.ast.exprs {
            let Expr::Call { callee, args } = expr else {
                continue;
            };
            let Expr::Name(symbol) = self.ast.expr(*callee) else {
                continue;
            };
            if !self.symbol_is(*symbol, Global::Phase) {
                continue;
            }
            let Some(first) = self.ast.exprs_in(*args).first() else {
                continue;
            };
            if let Expr::Text(title) = self.ast.expr(*first)
                && !titles.iter().any(|known| known == title.as_ref())
            {
                titles.push(title.to_string());
            }
        }
        titles
    }

    /// Count `agent()` call sites that are certain to run exactly once.
    fn count_agents(&self) -> Option<u32> {
        let mut count = 0u32;
        self.walk_statements(self.body, &mut count)?;
        Some(count)
    }

    fn walk_statements(&self, span: Span, count: &mut u32) -> Option<()> {
        for id in self.ast.stmts_in(span) {
            match self.ast.stmt(*id) {
                Stmt::Declare { value, .. } => self.walk_expr(*value, count)?,
                Stmt::Assign { target, value } => {
                    self.walk_expr(*target, count)?;
                    self.walk_expr(*value, count)?;
                }
                Stmt::Expr(value) => self.walk_expr(*value, count)?,
                Stmt::If {
                    test,
                    consequent,
                    alternate,
                } => {
                    self.walk_expr(*test, count)?;
                    // Both branches are counted, so a conditional fan-out is
                    // reported at its largest rather than its smallest.
                    self.walk_statements(*consequent, count)?;
                    self.walk_statements(*alternate, count)?;
                }
                // A loop runs an unknown number of times. If it can start an
                // agent at all, the total is not knowable in advance.
                Stmt::ForOf { iterable, body, .. } => {
                    self.walk_expr(*iterable, count)?;
                    if self.contains_agent_call(*body) {
                        return None;
                    }
                }
                Stmt::While { test, body } => {
                    self.walk_expr(*test, count)?;
                    if self.contains_agent_call(*body) {
                        return None;
                    }
                }
                Stmt::Return(Some(value)) => self.walk_expr(*value, count)?,
                Stmt::Return(None) | Stmt::Break | Stmt::Continue => {}
            }
        }
        Some(())
    }

    fn walk_expr(&self, id: ExprId, count: &mut u32) -> Option<()> {
        match self.ast.expr(id) {
            Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Text(_) | Expr::Name(_) => {}
            Expr::Template(span) => {
                for part in self.ast.parts_in(*span) {
                    if let crate::ast::TemplatePart::Expr(expr) = part {
                        self.walk_expr(*expr, count)?;
                    }
                }
            }
            Expr::Array(span) => {
                for item in self.ast.exprs_in(*span) {
                    self.walk_expr(*item, count)?;
                }
            }
            Expr::Object(span) => {
                for field in self.ast.fields_in(*span) {
                    self.walk_expr(field.value, count)?;
                }
            }
            Expr::Member { object, .. } => self.walk_expr(*object, count)?,
            Expr::Index { object, index } => {
                self.walk_expr(*object, count)?;
                self.walk_expr(*index, count)?;
            }
            Expr::Call { callee, args } => {
                if let Expr::Name(symbol) = self.ast.expr(*callee) {
                    if self.symbol_is(*symbol, Global::Agent) {
                        *count += 1;
                    }
                    // `parallel([() => agent(...), ...])` over a literal array
                    // calls each function exactly once, so the count is known.
                    if self.symbol_is(*symbol, Global::Parallel)
                        && let Some(first) = self.ast.exprs_in(*args).first()
                        && let Expr::Array(items) = self.ast.expr(*first)
                    {
                        for item in self.ast.exprs_in(*items) {
                            match self.ast.expr(*item) {
                                Expr::Arrow(function) => {
                                    let def = self.ast.function(*function)?;
                                    match &def.body {
                                        FnBody::Expr(value) => self.walk_expr(*value, count)?,
                                        FnBody::Block(block) => {
                                            self.walk_statements(*block, count)?
                                        }
                                    }
                                }
                                _ => self.walk_expr(*item, count)?,
                            }
                        }
                        return Some(());
                    }
                }
                self.walk_expr(*callee, count)?;
                for arg in self.ast.exprs_in(*args) {
                    self.walk_expr(*arg, count)?;
                }
            }
            // A function value may be called any number of times, or none.
            Expr::Arrow(function) => {
                let def = self.ast.function(*function)?;
                let reachable = match &def.body {
                    FnBody::Expr(value) => self.expr_contains_agent_call(*value),
                    FnBody::Block(block) => self.contains_agent_call(*block),
                };
                if reachable {
                    return None;
                }
            }
            Expr::Await(value)
            | Expr::Unary { operand: value, .. }
            | Expr::New { callee: value } => self.walk_expr(*value, count)?,
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.walk_expr(*left, count)?;
                self.walk_expr(*right, count)?;
            }
            Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                self.walk_expr(*test, count)?;
                self.walk_expr(*consequent, count)?;
                self.walk_expr(*alternate, count)?;
            }
        }
        Some(())
    }

    fn contains_agent_call(&self, span: Span) -> bool {
        self.ast
            .stmts_in(span)
            .iter()
            .any(|id| match self.ast.stmt(*id) {
                Stmt::Declare { value, .. } | Stmt::Expr(value) | Stmt::Return(Some(value)) => {
                    self.expr_contains_agent_call(*value)
                }
                Stmt::Assign { target, value } => {
                    self.expr_contains_agent_call(*target) || self.expr_contains_agent_call(*value)
                }
                Stmt::If {
                    test,
                    consequent,
                    alternate,
                } => {
                    self.expr_contains_agent_call(*test)
                        || self.contains_agent_call(*consequent)
                        || self.contains_agent_call(*alternate)
                }
                Stmt::ForOf { iterable, body, .. } => {
                    self.expr_contains_agent_call(*iterable) || self.contains_agent_call(*body)
                }
                Stmt::While { test, body } => {
                    self.expr_contains_agent_call(*test) || self.contains_agent_call(*body)
                }
                Stmt::Return(None) | Stmt::Break | Stmt::Continue => false,
            })
    }

    fn expr_contains_agent_call(&self, id: ExprId) -> bool {
        let mut found = false;
        let mut pending = vec![id];
        // An explicit stack rather than recursion: this runs on script data and
        // the release profile aborts on a stack overflow.
        while let Some(current) = pending.pop() {
            match self.ast.expr(current) {
                Expr::Call { callee, args } => {
                    if let Expr::Name(symbol) = self.ast.expr(*callee)
                        && self.symbol_is(*symbol, Global::Agent)
                    {
                        found = true;
                        break;
                    }
                    pending.push(*callee);
                    pending.extend_from_slice(self.ast.exprs_in(*args));
                }
                Expr::Template(span) => {
                    for part in self.ast.parts_in(*span) {
                        if let crate::ast::TemplatePart::Expr(expr) = part {
                            pending.push(*expr);
                        }
                    }
                }
                Expr::Array(span) => pending.extend_from_slice(self.ast.exprs_in(*span)),
                Expr::Object(span) => {
                    pending.extend(self.ast.fields_in(*span).iter().map(|field| field.value));
                }
                Expr::Member { object, .. } => pending.push(*object),
                Expr::Index { object, index } => {
                    pending.push(*object);
                    pending.push(*index);
                }
                Expr::Arrow(function) => {
                    if let Some(def) = self.ast.function(*function) {
                        match &def.body {
                            FnBody::Expr(value) => pending.push(*value),
                            FnBody::Block(block) => {
                                if self.contains_agent_call(*block) {
                                    found = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                Expr::Await(value)
                | Expr::Unary { operand: value, .. }
                | Expr::New { callee: value } => pending.push(*value),
                Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                    pending.push(*left);
                    pending.push(*right);
                }
                Expr::Conditional {
                    test,
                    consequent,
                    alternate,
                } => {
                    pending.push(*test);
                    pending.push(*consequent);
                    pending.push(*alternate);
                }
                Expr::Null | Expr::Bool(_) | Expr::Number(_) | Expr::Text(_) | Expr::Name(_) => {}
            }
        }
        found
    }
}

/// Read the `meta` object literal, which must contain only literal values.
fn read_meta(ast: &Ast, interner: &Interner, expr: ExprId) -> Result<Meta, Diagnostic> {
    let pos = ast.expr_position(expr);
    let value = literal_value(ast, interner, expr).ok_or_else(|| {
        Diagnostic::new(
            pos.line,
            pos.column,
            "`meta` may contain only literal values",
        )
        .with_help("remove any variable, function call, or expression from the `meta` block")
    })?;
    let Value::Object(fields) = value else {
        return Err(Diagnostic::new(
            pos.line,
            pos.column,
            "`meta` must be an object literal",
        ));
    };

    let name = fields
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            Diagnostic::new(
                pos.line,
                pos.column,
                "`meta` needs a non-empty `name` string",
            )
            .with_help("the name becomes the slash command when the workflow is saved")
        })?
        .to_string();
    let description = fields
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| {
            Diagnostic::new(
                pos.line,
                pos.column,
                "`meta` needs a non-empty `description` string",
            )
            .with_help("one sentence saying what the workflow does")
        })?
        .to_string();

    let mut phases = Vec::new();
    if let Some(Value::Array(items)) = fields.get("phases") {
        for item in items {
            let title = match item {
                Value::String(title) => title.clone(),
                Value::Object(entry) => entry
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                _ => String::new(),
            };
            if !title.is_empty() {
                phases.push(title);
            }
        }
    }

    Ok(Meta {
        name,
        description,
        phases,
    })
}

/// Convert a literal-only expression into JSON, or `None` when it is not one.
fn literal_value(ast: &Ast, interner: &Interner, expr: ExprId) -> Option<Value> {
    Some(match ast.expr(expr) {
        Expr::Null => Value::Null,
        Expr::Bool(value) => Value::Bool(*value),
        Expr::Number(value) => {
            serde_json::Number::from_f64(*value).map_or(Value::Null, Value::Number)
        }
        Expr::Text(text) => Value::String(text.to_string()),
        Expr::Array(span) => Value::Array(
            ast.exprs_in(*span)
                .iter()
                .map(|item| literal_value(ast, interner, *item))
                .collect::<Option<Vec<_>>>()?,
        ),
        Expr::Object(span) => {
            let mut map = Map::new();
            for field in ast.fields_in(*span) {
                map.insert(
                    interner.resolve(field.name).to_string(),
                    literal_value(ast, interner, field.value)?,
                );
            }
            Value::Object(map)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIT: &str = r#"
export const meta = {
  name: 'audit-routes',
  description: 'Audit every tool file for missing path checks',
  phases: [{ title: 'Discover' }, { title: 'Audit' }],
}

phase('Discover')
const found = await agent('List every .rs file under crates/kiss-coding/src/tools.', {
  schema: {
    type: 'object',
    required: ['files'],
    properties: { files: { type: 'array', items: { type: 'string' } } },
  },
})

phase('Audit')
const audits = await pipeline(found.files, file =>
  agent(`Audit ${file} for missing path checks.`, { label: file }),
)

return audits.filter(Boolean)
"#;

    #[test]
    fn the_reference_script_parses() {
        let script = Script::parse(AUDIT).expect("audit script parses");
        assert_eq!(script.meta().name, "audit-routes");
        assert_eq!(
            script.meta().description,
            "Audit every tool file for missing path checks"
        );
        assert_eq!(script.declared_phases(), ["Discover", "Audit"]);
    }

    #[test]
    fn a_data_dependent_fan_out_reports_an_unknown_agent_count() {
        let script = Script::parse(AUDIT).expect("audit script parses");
        // One agent is certain; the pipeline over a list fetched at run time is
        // not, so the total is deliberately not guessed.
        assert_eq!(script.estimated_agents(), None);
    }

    #[test]
    fn a_fixed_fan_out_reports_an_exact_agent_count() {
        let script = Script::parse(
            r#"
export const meta = { name: 'three', description: 'Three fixed agents' }
const results = await parallel([
  () => agent('one'),
  () => agent('two'),
  () => agent('three'),
])
return results
"#,
        )
        .expect("script parses");
        assert_eq!(script.estimated_agents(), Some(3));
    }

    #[test]
    fn a_sequential_script_reports_its_exact_count() {
        let script = Script::parse(
            r#"
export const meta = { name: 'two', description: 'Two agents in order' }
const first = await agent('one')
const second = await agent(`two after ${first}`)
return [first, second]
"#,
        )
        .expect("script parses");
        assert_eq!(script.estimated_agents(), Some(2));
    }

    #[test]
    fn a_loop_that_starts_agents_is_unbounded() {
        let script = Script::parse(
            r#"
export const meta = { name: 'loop', description: 'One agent per item' }
const out = []
for (const item of args) {
  out.push(await agent(`check ${item}`))
}
return out
"#,
        )
        .expect("script parses");
        assert_eq!(script.estimated_agents(), None);
    }

    #[test]
    fn phase_titles_not_declared_in_meta_are_still_listed() {
        let script = Script::parse(
            r#"
export const meta = { name: 'p', description: 'Phases', phases: [{ title: 'First' }] }
phase('First')
const a = await agent('a')
phase('Second')
const b = await agent('b')
return [a, b]
"#,
        )
        .expect("script parses");
        assert_eq!(script.declared_phases(), ["First", "Second"]);
    }

    #[test]
    fn a_missing_meta_block_explains_what_to_write() {
        let error = Script::parse("const a = await agent('x')\nreturn a\n").unwrap_err();
        assert!(error.message.contains("no `meta` block"));
        assert!(
            error
                .help
                .is_some_and(|help| help.contains("export const meta"))
        );
    }

    #[test]
    fn meta_must_hold_only_literals() {
        let error = Script::parse(
            "export const meta = { name: 'x', description: makeDescription() }\nreturn 1\n",
        )
        .unwrap_err();
        assert!(error.message.contains("only literal values"));
    }

    #[test]
    fn meta_requires_a_name_and_a_description() {
        let error = Script::parse("export const meta = { name: 'x' }\nreturn 1\n").unwrap_err();
        assert!(error.message.contains("`description`"));

        let error =
            Script::parse("export const meta = { description: 'x' }\nreturn 1\n").unwrap_err();
        assert!(error.message.contains("`name`"));
    }

    #[test]
    fn unsupported_syntax_names_a_supported_alternative() {
        let error = Script::parse(
            "export const meta = { name: 'x', description: 'y' }\nfor (let i = 0; i < 3; i++) {}\n",
        )
        .unwrap_err();
        assert_eq!(error.line, 2);
        assert!(error.message.contains("for (const item of list)"));
        assert!(
            error
                .help
                .is_some_and(|help| help.contains("pipeline(list, item => agent(...))"))
        );

        let error = Script::parse(
            "export const meta = { name: 'x', description: 'y' }\nimport fs from 'fs'\n",
        )
        .unwrap_err();
        assert!(error.message.contains("`import` is not supported"));
        assert!(
            error
                .help
                .is_some_and(|help| help.contains("loads no modules"))
        );
    }

    #[test]
    fn deep_nesting_is_rejected_rather_than_overflowing_the_stack() {
        let mut source =
            String::from("export const meta = { name: 'x', description: 'y' }\nconst a = ");
        source.push_str(&"(".repeat(200));
        source.push('1');
        source.push_str(&")".repeat(200));
        let error = Script::parse(&source).unwrap_err();
        assert!(error.message.contains("nests too deeply"));
    }
}

#[cfg(test)]
mod benchmarks {
    use super::*;

    /// A script of about 200 lines, the shape a model writes for a large task.
    fn representative_script() -> String {
        let mut source = String::from(
            "export const meta = {\n\
             \x20 name: 'benchmark',\n\
             \x20 description: 'A representative workflow',\n\
             \x20 phases: [{ title: 'Discover' }, { title: 'Audit' }, { title: 'Report' }],\n\
             }\n\n",
        );
        for round in 0..18 {
            source.push_str(&format!(
                "phase('Audit {round}')\n\
                 const found{round} = await agent(`list the files in group {round}`, {{\n\
                 \x20 schema: {{ type: 'object', required: ['files'] }},\n\
                 }})\n\
                 const audits{round} = await pipeline(\n\
                 \x20 found{round}.files,\n\
                 \x20 file => agent(`audit ${{file}} in round {round}`, {{ label: file }}),\n\
                 )\n\
                 if (audits{round}.length > 0) {{\n\
                 \x20 log(`round {round} produced ${{audits{round}.length}} findings`)\n\
                 }}\n\n"
            ));
        }
        source.push_str("return 'done'\n");
        source
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_workflow_parse() {
        let source = representative_script();
        let lines = source.lines().count();
        assert!(
            (190..=230).contains(&lines),
            "expected about 200 lines: {lines}"
        );
        kiss_bench::measure(
            "workflow_script_parse",
            21,
            200,
            "parse_200_line_script",
            || Script::parse(&source).expect("the benchmark script parses"),
        );
    }
}
