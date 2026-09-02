//! The evaluator.
//!
//! A tree walk over the flat arena in [`crate::ast`]. It is `async` because
//! `agent()` waits for a real model, so every recursive step returns a boxed
//! future. Recursion depth is passed explicitly rather than kept in a shared
//! counter, so that concurrent branches inside `parallel()` do not add up into
//! a false depth error.
//!
//! The evaluator never panics on script input. Every container access is
//! bounds-checked and both call depth and total steps are capped, because the
//! release profile aborts on panic and cannot recover from a stack overflow.

use crate::ast::{BinOp, Expr, ExprId, FnBody, FnId, LogOp, Pos, Span, Stmt, TemplatePart, UnOp};
use crate::lexer::Symbol;
use crate::progress::RunState;
use crate::runner::{AgentRunner, Journal, Limits};
use crate::script::{Global, Script};
use crate::value::{Builtin, Value};
use futures::future::BoxFuture;
use serde_json::Value as Json;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

/// The deepest chain of function calls a running script may make.
const MAX_CALL_DEPTH: u32 = 64;

/// A workflow run that failed. A failure ends the run; an agent that fails
/// merely returns null.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunError {
    pub message: String,
    pub line: u32,
    pub column: u32,
}

impl RunError {
    fn at(pos: Pos, message: impl Into<String>) -> RunError {
        RunError {
            message: message.into(),
            line: pos.line,
            column: pos.column,
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}:{}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for RunError {}

/// One lexical scope. Variables are found by integer comparison over a short
/// vector, which beats hashing at the handful of names a workflow scope holds.
pub(crate) struct Scope {
    vars: Mutex<Vec<(Symbol, Value)>>,
    parent: Option<Arc<Scope>>,
}

impl Scope {
    pub(crate) fn root() -> Arc<Scope> {
        Arc::new(Scope {
            vars: Mutex::new(Vec::new()),
            parent: None,
        })
    }

    fn child(parent: &Arc<Scope>) -> Arc<Scope> {
        Arc::new(Scope {
            vars: Mutex::new(Vec::new()),
            parent: Some(parent.clone()),
        })
    }

    fn declare(&self, name: Symbol, value: Value) {
        let Ok(mut vars) = self.vars.lock() else {
            return;
        };
        match vars.iter_mut().find(|(known, _)| *known == name) {
            Some(slot) => slot.1 = value,
            None => vars.push((name, value)),
        }
    }

    fn get(&self, name: Symbol) -> Option<Value> {
        let mut scope = self;
        loop {
            if let Ok(vars) = scope.vars.lock()
                && let Some((_, value)) = vars.iter().find(|(known, _)| *known == name)
            {
                return Some(value.clone());
            }
            scope = scope.parent.as_deref()?;
        }
    }

    /// Assign to an existing binding. Returns false when the name is unknown.
    fn assign(&self, name: Symbol, value: Value) -> bool {
        let mut scope = self;
        loop {
            if let Ok(mut vars) = scope.vars.lock()
                && let Some(slot) = vars.iter_mut().find(|(known, _)| *known == name)
            {
                slot.1 = value;
                return true;
            }
            match scope.parent.as_deref() {
                Some(parent) => scope = parent,
                None => return false,
            }
        }
    }
}

/// How a statement finished.
enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

pub(crate) struct Interp {
    script: Arc<Script>,
    state: Arc<RunState>,
    runner: Arc<dyn AgentRunner>,
    limits: Limits,
    args: Value,
    cwd: Arc<str>,
    steps: AtomicU64,
    next_index: AtomicU32,
    permits: Arc<Semaphore>,
    journal: Mutex<Journal>,
}

impl Interp {
    pub(crate) fn new(
        script: Arc<Script>,
        state: Arc<RunState>,
        runner: Arc<dyn AgentRunner>,
        limits: Limits,
        args: Json,
        cwd: String,
        journal: Journal,
    ) -> Interp {
        Interp {
            script,
            state,
            runner,
            limits,
            args: Value::from_json(&args),
            cwd: Arc::from(cwd.as_str()),
            steps: AtomicU64::new(0),
            next_index: AtomicU32::new(0),
            permits: Arc::new(Semaphore::new(limits.max_concurrency)),
            journal: Mutex::new(journal),
        }
    }

    pub(crate) fn journal(&self) -> Journal {
        self.journal
            .lock()
            .map(|journal| journal.clone())
            .unwrap_or_default()
    }

    /// Run the script body and return the value it returns.
    pub(crate) async fn run(&self) -> Result<Json, RunError> {
        let scope = Scope::root();
        let body = self.script.body;
        match self.eval_block(&scope, body, 0).await? {
            Flow::Return(value) => Ok(value.to_json()),
            _ => Ok(Json::Null),
        }
    }

    fn step(&self, pos: Pos) -> Result<(), RunError> {
        let steps = self.steps.fetch_add(1, Ordering::Relaxed);
        if steps >= self.limits.max_steps {
            return Err(RunError::at(
                pos,
                format!(
                    "this script ran for more than {} steps and was stopped",
                    self.limits.max_steps
                ),
            ));
        }
        Ok(())
    }

    // ----- statements -------------------------------------------------------

    fn eval_block<'a>(
        &'a self,
        scope: &'a Arc<Scope>,
        span: Span,
        depth: u32,
    ) -> BoxFuture<'a, Result<Flow, RunError>> {
        Box::pin(async move {
            for id in self.script.ast.stmts_in(span) {
                match self.eval_stmt(scope, *id, depth).await? {
                    Flow::Normal => {}
                    other => return Ok(other),
                }
            }
            Ok(Flow::Normal)
        })
    }

    fn eval_stmt<'a>(
        &'a self,
        scope: &'a Arc<Scope>,
        id: crate::ast::StmtId,
        depth: u32,
    ) -> BoxFuture<'a, Result<Flow, RunError>> {
        Box::pin(async move {
            let pos = self.script.ast.stmt_position(id);
            self.step(pos)?;
            if self.state.stop_token().is_cancelled() {
                return Err(RunError::at(pos, "the run was stopped"));
            }

            match self.script.ast.stmt(id).clone() {
                Stmt::Declare { name, value } => {
                    let value = self.eval_expr(scope, value, depth).await?;
                    scope.declare(name, value);
                    Ok(Flow::Normal)
                }
                Stmt::Assign { target, value } => {
                    let value = self.eval_expr(scope, value, depth).await?;
                    self.assign(scope, target, value, depth).await?;
                    Ok(Flow::Normal)
                }
                Stmt::Expr(value) => {
                    self.eval_expr(scope, value, depth).await?;
                    Ok(Flow::Normal)
                }
                Stmt::If {
                    test,
                    consequent,
                    alternate,
                } => {
                    let taken = if self.eval_expr(scope, test, depth).await?.truthy() {
                        consequent
                    } else {
                        alternate
                    };
                    let inner = Scope::child(scope);
                    self.eval_block(&inner, taken, depth).await
                }
                Stmt::ForOf {
                    name,
                    iterable,
                    body,
                } => {
                    let list = self.eval_expr(scope, iterable, depth).await?;
                    let items = list.elements().ok_or_else(|| {
                        RunError::at(
                            pos,
                            format!("`for ... of` needs an array, but found {}", list.kind()),
                        )
                    })?;
                    for item in items {
                        let inner = Scope::child(scope);
                        inner.declare(name, item);
                        match self.eval_block(&inner, body, depth).await? {
                            Flow::Break => break,
                            Flow::Return(value) => return Ok(Flow::Return(value)),
                            Flow::Normal | Flow::Continue => {}
                        }
                    }
                    Ok(Flow::Normal)
                }
                Stmt::While { test, body } => {
                    while self.eval_expr(scope, test, depth).await?.truthy() {
                        self.step(pos)?;
                        let inner = Scope::child(scope);
                        match self.eval_block(&inner, body, depth).await? {
                            Flow::Break => break,
                            Flow::Return(value) => return Ok(Flow::Return(value)),
                            Flow::Normal | Flow::Continue => {}
                        }
                    }
                    Ok(Flow::Normal)
                }
                Stmt::Return(Some(value)) => {
                    Ok(Flow::Return(self.eval_expr(scope, value, depth).await?))
                }
                Stmt::Return(None) => Ok(Flow::Return(Value::Null)),
                Stmt::Break => Ok(Flow::Break),
                Stmt::Continue => Ok(Flow::Continue),
            }
        })
    }

    async fn assign(
        &self,
        scope: &Arc<Scope>,
        target: ExprId,
        value: Value,
        depth: u32,
    ) -> Result<(), RunError> {
        let pos = self.script.ast.expr_position(target);
        match self.script.ast.expr(target).clone() {
            Expr::Name(name) => {
                if !scope.assign(name, value) {
                    return Err(RunError::at(
                        pos,
                        format!(
                            "`{}` was assigned before it was declared",
                            self.script.interner.resolve(name)
                        ),
                    ));
                }
                Ok(())
            }
            Expr::Member { object, name } => {
                let object = self.eval_expr(scope, object, depth).await?;
                let field = self.script.interner.resolve(name);
                if !matches!(object, Value::Object(_)) {
                    return Err(RunError::at(
                        pos,
                        format!("cannot set `{field}` on {}", object.kind()),
                    ));
                }
                object.set_field(field, value);
                Ok(())
            }
            Expr::Index { object, index } => {
                let object = self.eval_expr(scope, object, depth).await?;
                let index = self.eval_expr(scope, index, depth).await?;
                match (&object, &index) {
                    (Value::Array(items), Value::Number(position)) => {
                        let Ok(mut items) = items.lock() else {
                            return Ok(());
                        };
                        let position = *position;
                        if position < 0.0 || position.fract() != 0.0 {
                            return Err(RunError::at(
                                pos,
                                "an array index must be a whole number that is not negative",
                            ));
                        }
                        let position = position as usize;
                        if position >= items.len() {
                            // Growing by assignment would leave holes, which
                            // this value model has no way to represent.
                            return Err(RunError::at(
                                pos,
                                format!(
                                    "index {position} is past the end of an array of {} items; \
                                     use `push` to add to it",
                                    items.len()
                                ),
                            ));
                        }
                        items[position] = value;
                        Ok(())
                    }
                    (Value::Object(_), _) => {
                        object.set_field(&index.display(), value);
                        Ok(())
                    }
                    _ => Err(RunError::at(
                        pos,
                        format!("cannot assign into {}", object.kind()),
                    )),
                }
            }
            _ => Err(RunError::at(pos, "this expression cannot be assigned to")),
        }
    }

    // ----- expressions ------------------------------------------------------

    fn eval_expr<'a>(
        &'a self,
        scope: &'a Arc<Scope>,
        id: ExprId,
        depth: u32,
    ) -> BoxFuture<'a, Result<Value, RunError>> {
        Box::pin(async move {
            let pos = self.script.ast.expr_position(id);
            self.step(pos)?;

            match self.script.ast.expr(id).clone() {
                Expr::Null => Ok(Value::Null),
                Expr::Bool(value) => Ok(Value::Bool(value)),
                Expr::Number(value) => Ok(Value::Number(value)),
                Expr::Text(text) => Ok(Value::Text(text)),
                Expr::Name(name) => self.resolve_name(scope, name, pos),
                Expr::Template(span) => {
                    let mut out = String::new();
                    for part in self.script.ast.parts_in(span).to_vec() {
                        match part {
                            TemplatePart::Chunk(text) => out.push_str(&text),
                            TemplatePart::Expr(expr) => {
                                let value = self.eval_expr(scope, expr, depth).await?;
                                out.push_str(&value.display());
                            }
                        }
                    }
                    Ok(Value::text(out))
                }
                Expr::Array(span) => {
                    let ids = self.script.ast.exprs_in(span).to_vec();
                    let mut items = Vec::with_capacity(ids.len());
                    for item in ids {
                        items.push(self.eval_expr(scope, item, depth).await?);
                    }
                    Ok(Value::array(items))
                }
                Expr::Object(span) => {
                    let fields = self.script.ast.fields_in(span).to_vec();
                    let mut out = Vec::with_capacity(fields.len());
                    for field in fields {
                        let value = self.eval_expr(scope, field.value, depth).await?;
                        out.push((self.script.interner.shared(field.name), value));
                    }
                    Ok(Value::object(out))
                }
                Expr::Member { object, name } => {
                    let object = self.eval_expr(scope, object, depth).await?;
                    let field = self.script.interner.resolve(name);
                    self.member(&object, field, pos)
                }
                Expr::Index { object, index } => {
                    let object = self.eval_expr(scope, object, depth).await?;
                    let index = self.eval_expr(scope, index, depth).await?;
                    self.index(&object, &index, pos)
                }
                Expr::Call { callee, args } => self.call(scope, callee, args, pos, depth).await,
                Expr::Arrow(function) => Ok(Value::Function(function, scope.clone())),
                Expr::Await(inner) => {
                    // Every call in this language already resolves before it
                    // returns, so `await` reads as documentation.
                    self.eval_expr(scope, inner, depth).await
                }
                Expr::Unary { op, operand } => {
                    let value = self.eval_expr(scope, operand, depth).await?;
                    Ok(match op {
                        UnOp::Not => Value::Bool(!value.truthy()),
                        UnOp::Negate => Value::Number(-to_number(&value)),
                    })
                }
                Expr::Binary { op, left, right } => {
                    let left = self.eval_expr(scope, left, depth).await?;
                    let right = self.eval_expr(scope, right, depth).await?;
                    Ok(binary(op, &left, &right))
                }
                Expr::Logical { op, left, right } => {
                    let left = self.eval_expr(scope, left, depth).await?;
                    let take_right = match op {
                        LogOp::And => left.truthy(),
                        LogOp::Or => !left.truthy(),
                        LogOp::NullCoalesce => matches!(left, Value::Null),
                    };
                    if take_right {
                        self.eval_expr(scope, right, depth).await
                    } else {
                        Ok(left)
                    }
                }
                Expr::Conditional {
                    test,
                    consequent,
                    alternate,
                } => {
                    let taken = if self.eval_expr(scope, test, depth).await?.truthy() {
                        consequent
                    } else {
                        alternate
                    };
                    self.eval_expr(scope, taken, depth).await
                }
                Expr::New { callee } => {
                    let name = match self.script.ast.expr(callee) {
                        Expr::Name(symbol) => self.script.interner.resolve(*symbol).to_string(),
                        Expr::Call { callee, .. } => match self.script.ast.expr(*callee) {
                            Expr::Name(symbol) => self.script.interner.resolve(*symbol).to_string(),
                            _ => String::new(),
                        },
                        _ => String::new(),
                    };
                    if name == "Date" {
                        return Err(RunError::at(pos, DETERMINISM_MESSAGE).into_help());
                    }
                    Err(RunError::at(
                        pos,
                        "`new` is not supported; a workflow script builds plain objects and arrays",
                    ))
                }
            }
        })
    }

    fn resolve_name(&self, scope: &Arc<Scope>, name: Symbol, pos: Pos) -> Result<Value, RunError> {
        if let Some(value) = scope.get(name) {
            return Ok(value);
        }
        let global = self
            .script
            .globals
            .get(name as usize)
            .copied()
            .flatten()
            .ok_or_else(|| {
                RunError::at(
                    pos,
                    format!("`{}` is not defined", self.script.interner.resolve(name)),
                )
            })?;
        Ok(match global {
            Global::Agent => Value::Builtin(Builtin::Agent),
            Global::Parallel => Value::Builtin(Builtin::Parallel),
            Global::Pipeline => Value::Builtin(Builtin::Pipeline),
            Global::Phase => Value::Builtin(Builtin::Phase),
            Global::Log => Value::Builtin(Builtin::Log),
            Global::Args => self.args.clone(),
            Global::Cwd => Value::Text(self.cwd.clone()),
            Global::NumberCast => Value::Builtin(Builtin::NumberCast),
            Global::StringCast => Value::Builtin(Builtin::StringCast),
            Global::BooleanCast => Value::Builtin(Builtin::BooleanCast),
            Global::ParseInt => Value::Builtin(Builtin::ParseInt),
            Global::ParseFloat => Value::Builtin(Builtin::ParseFloat),
            Global::IsNaN => Value::Builtin(Builtin::IsNaN),
            namespace => Value::Namespace(namespace),
        })
    }

    /// Read a field, an array length, or a namespace member.
    fn member(&self, object: &Value, name: &str, pos: Pos) -> Result<Value, RunError> {
        if let Value::Namespace(namespace) = object {
            return namespace_member(*namespace, name)
                .map(Value::Builtin)
                .ok_or_else(|| {
                    RunError::at(pos, format!("`{name}` is not available on this namespace"))
                });
        }
        if name == "length" {
            return match object {
                Value::Array(items) => Ok(Value::Number(
                    items.lock().map(|items| items.len()).unwrap_or(0) as f64,
                )),
                Value::Text(text) => Ok(Value::Number(text.chars().count() as f64)),
                _ => Err(RunError::at(
                    pos,
                    format!("{} has no `length`", object.kind()),
                )),
            };
        }
        match object {
            Value::Object(_) => Ok(object.field(name).unwrap_or(Value::Null)),
            Value::Null => Err(RunError::at(
                pos,
                format!(
                    "cannot read `{name}` from null; an agent that failed returns null, \
                     so test the value first"
                ),
            )),
            _ if is_method_name(name) => Err(RunError::at(
                pos,
                format!("`{name}` is a method and must be called, as `value.{name}(...)`"),
            )),
            _ => Err(RunError::at(
                pos,
                format!("cannot read `{name}` from {}", object.kind()),
            )),
        }
    }

    fn index(&self, object: &Value, index: &Value, pos: Pos) -> Result<Value, RunError> {
        match object {
            Value::Array(items) => {
                let Some(position) = index.as_number() else {
                    return Err(RunError::at(pos, "an array index must be a number"));
                };
                let Ok(items) = items.lock() else {
                    return Ok(Value::Null);
                };
                if position < 0.0 || position.fract() != 0.0 {
                    return Ok(Value::Null);
                }
                Ok(items.get(position as usize).cloned().unwrap_or(Value::Null))
            }
            Value::Object(_) => Ok(object.field(&index.display()).unwrap_or(Value::Null)),
            Value::Text(text) => {
                let Some(position) = index.as_number() else {
                    return Err(RunError::at(pos, "a string index must be a number"));
                };
                if position < 0.0 {
                    return Ok(Value::Null);
                }
                Ok(text
                    .chars()
                    .nth(position as usize)
                    .map(|character| Value::text(character.to_string()))
                    .unwrap_or(Value::Null))
            }
            Value::Null => Err(RunError::at(
                pos,
                "cannot index null; an agent that failed returns null, so test the value first",
            )),
            _ => Err(RunError::at(pos, format!("cannot index {}", object.kind()))),
        }
    }

    // ----- calls ------------------------------------------------------------

    async fn call(
        &self,
        scope: &Arc<Scope>,
        callee: ExprId,
        args: Span,
        pos: Pos,
        depth: u32,
    ) -> Result<Value, RunError> {
        // A call on a member is a method call, so the receiver is evaluated
        // once and bound rather than being looked up as a standalone value.
        if let Expr::Member { object, name } = self.script.ast.expr(callee).clone() {
            let receiver = self.eval_expr(scope, object, depth).await?;
            let method = self.script.interner.resolve(name).to_string();
            if let Value::Namespace(namespace) = receiver {
                let builtin = namespace_member(namespace, &method).ok_or_else(|| {
                    RunError::at(
                        pos,
                        format!("`{method}` is not available on this namespace"),
                    )
                })?;
                let values = self.eval_args(scope, args, depth).await?;
                return self.call_builtin(builtin, values, pos, depth).await;
            }
            let values = self.eval_args(scope, args, depth).await?;
            return self
                .call_method(receiver, &method, values, pos, depth)
                .await;
        }

        let function = self.eval_expr(scope, callee, depth).await?;
        let values = self.eval_args(scope, args, depth).await?;
        self.call_value(function, values, pos, depth).await
    }

    async fn eval_args(
        &self,
        scope: &Arc<Scope>,
        args: Span,
        depth: u32,
    ) -> Result<Vec<Value>, RunError> {
        let ids = self.script.ast.exprs_in(args).to_vec();
        let mut values = Vec::with_capacity(ids.len());
        for id in ids {
            values.push(self.eval_expr(scope, id, depth).await?);
        }
        Ok(values)
    }

    fn call_value<'a>(
        &'a self,
        function: Value,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> BoxFuture<'a, Result<Value, RunError>> {
        Box::pin(async move {
            match function {
                Value::Function(id, captured) => {
                    self.call_function(id, &captured, args, pos, depth).await
                }
                Value::Builtin(builtin) => self.call_builtin(builtin, args, pos, depth).await,
                other => Err(RunError::at(
                    pos,
                    format!("{} is not a function and cannot be called", other.kind()),
                )),
            }
        })
    }

    fn call_function<'a>(
        &'a self,
        id: FnId,
        captured: &'a Arc<Scope>,
        args: Vec<Value>,
        pos: Pos,
        depth: u32,
    ) -> BoxFuture<'a, Result<Value, RunError>> {
        Box::pin(async move {
            if depth >= MAX_CALL_DEPTH {
                return Err(RunError::at(
                    pos,
                    format!("functions called each other more than {MAX_CALL_DEPTH} deep"),
                ));
            }
            let Some(def) = self.script.ast.function(id) else {
                return Err(RunError::at(pos, "this function is missing"));
            };
            let scope = Scope::child(captured);
            for (position, name) in def.params.iter().enumerate() {
                scope.declare(*name, args.get(position).cloned().unwrap_or(Value::Null));
            }
            match &def.body {
                FnBody::Expr(value) => self.eval_expr(&scope, *value, depth + 1).await,
                FnBody::Block(block) => match self.eval_block(&scope, *block, depth + 1).await? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(Value::Null),
                },
            }
        })
    }
}

/// The message every determinism guard shares.
const DETERMINISM_MESSAGE: &str = "a workflow script must be repeatable, so that a stopped run can resume without \
     re-running work that already finished";

impl RunError {
    /// Attach the advice that goes with the determinism rule.
    fn into_help(mut self) -> RunError {
        self.message.push_str(
            ". Pass a timestamp or a random seed in through `args` instead of reading one here",
        );
        self
    }
}

fn namespace_member(namespace: Global, name: &str) -> Option<Builtin> {
    Some(match (namespace, name) {
        (Global::Math, "min") => Builtin::MathMin,
        (Global::Math, "max") => Builtin::MathMax,
        (Global::Math, "floor") => Builtin::MathFloor,
        (Global::Math, "ceil") => Builtin::MathCeil,
        (Global::Math, "abs") => Builtin::MathAbs,
        (Global::Math, "round") => Builtin::MathRound,
        (Global::Math, "random") => Builtin::MathRandom,
        (Global::Json, "stringify") => Builtin::JsonStringify,
        (Global::Json, "parse") => Builtin::JsonParse,
        (Global::ObjectNamespace, "keys") => Builtin::ObjectKeys,
        (Global::ObjectNamespace, "values") => Builtin::ObjectValues,
        (Global::ObjectNamespace, "entries") => Builtin::ObjectEntries,
        (Global::ArrayNamespace, "isArray") => Builtin::ArrayIsArray,
        (Global::DateNamespace, "now") => Builtin::DateNow,
        _ => return None,
    })
}

fn is_method_name(name: &str) -> bool {
    matches!(
        name,
        "map"
            | "filter"
            | "slice"
            | "join"
            | "push"
            | "includes"
            | "indexOf"
            | "concat"
            | "flat"
            | "find"
            | "some"
            | "every"
            | "reverse"
            | "sort"
            | "split"
            | "trim"
            | "toLowerCase"
            | "toUpperCase"
            | "startsWith"
            | "endsWith"
            | "replace"
            | "replaceAll"
            | "padStart"
            | "padEnd"
    )
}

fn to_number(value: &Value) -> f64 {
    match value {
        Value::Number(value) => *value,
        Value::Bool(true) => 1.0,
        Value::Bool(false) | Value::Null => 0.0,
        Value::Text(text) => text.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

fn binary(op: BinOp, left: &Value, right: &Value) -> Value {
    match op {
        BinOp::Add => {
            // `+` joins when either side is text, and adds otherwise.
            if matches!(left, Value::Text(_)) || matches!(right, Value::Text(_)) {
                let mut joined = left.display();
                joined.push_str(&right.display());
                Value::text(joined)
            } else {
                Value::Number(to_number(left) + to_number(right))
            }
        }
        BinOp::Subtract => Value::Number(to_number(left) - to_number(right)),
        BinOp::Multiply => Value::Number(to_number(left) * to_number(right)),
        BinOp::Divide => Value::Number(to_number(left) / to_number(right)),
        BinOp::Remainder => Value::Number(to_number(left) % to_number(right)),
        BinOp::Equal => Value::Bool(left.strict_equals(right)),
        BinOp::NotEqual => Value::Bool(!left.strict_equals(right)),
        BinOp::Less | BinOp::LessOrEqual | BinOp::Greater | BinOp::GreaterOrEqual => {
            let ordering = match (left, right) {
                (Value::Text(left), Value::Text(right)) => left.as_ref().cmp(right.as_ref()),
                _ => match to_number(left).partial_cmp(&to_number(right)) {
                    Some(ordering) => ordering,
                    // Any comparison with NaN is false in JavaScript.
                    None => return Value::Bool(false),
                },
            };
            Value::Bool(match op {
                BinOp::Less => ordering.is_lt(),
                BinOp::LessOrEqual => ordering.is_le(),
                BinOp::Greater => ordering.is_gt(),
                _ => ordering.is_ge(),
            })
        }
    }
}

// The builtin functions and value methods live in a child module so that this
// file stays the shape of the language. A child module can reach this module's
// private items, so `Interp` keeps its fields private to the crate.
mod builtins;
