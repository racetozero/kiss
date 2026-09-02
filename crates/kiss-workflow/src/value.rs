//! Runtime values.
//!
//! Cloning a value never deep-copies: text and containers are reference
//! counted, so passing a list to an agent costs one atomic increment.
//!
//! Containers use `Arc<Mutex<..>>` rather than `Rc<RefCell<..>>` because a run
//! is driven from a `tokio` task and its future must be `Send`. No lock is ever
//! held across an `await`: code that iterates a container copies the elements
//! out first, which is cheap because each element is itself a handle.

use crate::ast::FnId;
use crate::script::Global;
use serde_json::{Map, Number, Value as Json};
use std::sync::{Arc, Mutex};

/// Object fields in insertion order.
///
/// A workflow object holds a handful of fields, so a vector with a linear scan
/// beats a hash map and keeps field order stable for `JSON.stringify` and
/// `Object.keys`.
pub(crate) type Fields = Vec<(Arc<str>, Value)>;

/// A callable that the script did not define.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
    Agent,
    Parallel,
    Pipeline,
    Phase,
    Log,
    MathMin,
    MathMax,
    MathFloor,
    MathCeil,
    MathAbs,
    MathRound,
    /// Rejected when called, to explain the determinism rule.
    MathRandom,
    JsonStringify,
    JsonParse,
    ObjectKeys,
    ObjectValues,
    ObjectEntries,
    ArrayIsArray,
    NumberCast,
    StringCast,
    BooleanCast,
    ParseInt,
    ParseFloat,
    IsNaN,
    /// Rejected when called, to explain the determinism rule.
    DateNow,
}

impl Builtin {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Builtin::Agent => "agent",
            Builtin::Parallel => "parallel",
            Builtin::Pipeline => "pipeline",
            Builtin::Phase => "phase",
            Builtin::Log => "log",
            Builtin::MathMin => "Math.min",
            Builtin::MathMax => "Math.max",
            Builtin::MathFloor => "Math.floor",
            Builtin::MathCeil => "Math.ceil",
            Builtin::MathAbs => "Math.abs",
            Builtin::MathRound => "Math.round",
            Builtin::MathRandom => "Math.random",
            Builtin::JsonStringify => "JSON.stringify",
            Builtin::JsonParse => "JSON.parse",
            Builtin::ObjectKeys => "Object.keys",
            Builtin::ObjectValues => "Object.values",
            Builtin::ObjectEntries => "Object.entries",
            Builtin::ArrayIsArray => "Array.isArray",
            Builtin::NumberCast => "Number",
            Builtin::StringCast => "String",
            Builtin::BooleanCast => "Boolean",
            Builtin::ParseInt => "parseInt",
            Builtin::ParseFloat => "parseFloat",
            Builtin::IsNaN => "isNaN",
            Builtin::DateNow => "Date.now",
        }
    }
}

/// One value in a running script.
#[derive(Clone)]
pub(crate) enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Text(Arc<str>),
    Array(Arc<Mutex<Vec<Value>>>),
    Object(Arc<Mutex<Fields>>),
    /// A script-defined arrow function together with the scope it captured.
    Function(FnId, Arc<crate::interp::Scope>),
    Builtin(Builtin),
    /// `Math`, `JSON`, `Object`, `Array`, or `Date`: a name that only exists to
    /// be followed by a dot.
    Namespace(Global),
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

impl Value {
    pub(crate) fn array(items: Vec<Value>) -> Value {
        Value::Array(Arc::new(Mutex::new(items)))
    }

    pub(crate) fn object(fields: Fields) -> Value {
        Value::Object(Arc::new(Mutex::new(fields)))
    }

    pub(crate) fn text(text: impl AsRef<str>) -> Value {
        Value::Text(Arc::from(text.as_ref()))
    }

    /// The name of this value's kind, used in error messages.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "a boolean",
            Value::Number(_) => "a number",
            Value::Text(_) => "a string",
            Value::Array(_) => "an array",
            Value::Object(_) => "an object",
            Value::Function(..) | Value::Builtin(_) => "a function",
            Value::Namespace(_) => "a built-in namespace",
        }
    }

    /// JavaScript truthiness, which the script's `if`, `&&`, and `||` follow.
    pub(crate) fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => *value != 0.0 && !value.is_nan(),
            Value::Text(text) => !text.is_empty(),
            _ => true,
        }
    }

    /// Copy an array's elements out from under its lock.
    ///
    /// Callers iterate the copy so that no lock is held across an `await`. Each
    /// element is a handle, so the copy is a run of reference-count bumps.
    pub(crate) fn elements(&self) -> Option<Vec<Value>> {
        match self {
            Value::Array(items) => Some(items.lock().ok()?.clone()),
            _ => None,
        }
    }

    pub(crate) fn fields(&self) -> Option<Fields> {
        match self {
            Value::Object(fields) => Some(fields.lock().ok()?.clone()),
            _ => None,
        }
    }

    pub(crate) fn field(&self, name: &str) -> Option<Value> {
        let Value::Object(fields) = self else {
            return None;
        };
        let fields = fields.lock().ok()?;
        fields
            .iter()
            .find(|(key, _)| key.as_ref() == name)
            .map(|(_, value)| value.clone())
    }

    pub(crate) fn set_field(&self, name: &str, value: Value) {
        let Value::Object(fields) = self else {
            return;
        };
        let Ok(mut fields) = fields.lock() else {
            return;
        };
        match fields.iter_mut().find(|(key, _)| key.as_ref() == name) {
            Some(slot) => slot.1 = value,
            None => fields.push((Arc::from(name), value)),
        }
    }

    pub(crate) fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// The string form used by `String(value)`, template interpolation, and
    /// `+` when either side is a string.
    pub(crate) fn display(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Bool(value) => value.to_string(),
            Value::Number(value) => format_number(*value),
            Value::Text(text) => text.to_string(),
            Value::Array(items) => match items.lock() {
                Ok(items) => items
                    .iter()
                    .map(Value::display)
                    .collect::<Vec<_>>()
                    .join(","),
                Err(_) => String::new(),
            },
            Value::Object(_) => "[object Object]".into(),
            Value::Function(..) => "[function]".into(),
            Value::Builtin(builtin) => format!("[function {}]", builtin.name()),
            Value::Namespace(_) => "[namespace]".into(),
        }
    }

    /// Strict equality, matching `===`.
    pub(crate) fn strict_equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(left), Value::Bool(right)) => left == right,
            (Value::Number(left), Value::Number(right)) => left == right,
            (Value::Text(left), Value::Text(right)) => left == right,
            (Value::Array(left), Value::Array(right)) => Arc::ptr_eq(left, right),
            (Value::Object(left), Value::Object(right)) => Arc::ptr_eq(left, right),
            (Value::Builtin(left), Value::Builtin(right)) => left == right,
            _ => false,
        }
    }

    /// Convert to JSON for an agent request or a workflow result.
    ///
    /// A function has no JSON form and becomes null, which is also what an
    /// agent that failed returns, so a script sees one empty value either way.
    pub(crate) fn to_json(&self) -> Json {
        match self {
            Value::Null | Value::Function(..) | Value::Builtin(_) | Value::Namespace(_) => {
                Json::Null
            }
            Value::Bool(value) => Json::Bool(*value),
            Value::Number(value) => number_to_json(*value),
            Value::Text(text) => Json::String(text.to_string()),
            Value::Array(items) => match items.lock() {
                Ok(items) => Json::Array(items.iter().map(Value::to_json).collect()),
                Err(_) => Json::Array(Vec::new()),
            },
            Value::Object(fields) => match fields.lock() {
                Ok(fields) => {
                    let mut map = Map::with_capacity(fields.len());
                    for (name, value) in fields.iter() {
                        map.insert(name.to_string(), value.to_json());
                    }
                    Json::Object(map)
                }
                Err(_) => Json::Object(Map::new()),
            },
        }
    }

    pub(crate) fn from_json(value: &Json) -> Value {
        match value {
            Json::Null => Value::Null,
            Json::Bool(value) => Value::Bool(*value),
            Json::Number(value) => Value::Number(value.as_f64().unwrap_or(f64::NAN)),
            Json::String(text) => Value::text(text),
            Json::Array(items) => Value::array(items.iter().map(Value::from_json).collect()),
            Json::Object(map) => Value::object(
                map.iter()
                    .map(|(name, value)| (Arc::from(name.as_str()), Value::from_json(value)))
                    .collect(),
            ),
        }
    }
}

/// Write a number as JSON.
///
/// Every number in a script is a 64-bit float, but a count that reaches an
/// agent's prompt or a saved result should read as `2` rather than `2.0`, so a
/// whole number is written as an integer.
fn number_to_json(value: f64) -> Json {
    if value.is_finite() && value == value.trunc() && value.abs() <= i64::MAX as f64 {
        return Json::Number(Number::from(value as i64));
    }
    Number::from_f64(value).map_or(Json::Null, Json::Number)
}

/// Format a number the way JavaScript does, so an index used in a prompt reads
/// as `3` rather than `3.0`.
pub(crate) fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == value.trunc() && value.abs() < 1e21 {
        return format!("{}", value as i64);
    }
    let mut text = format!("{value}");
    if text.ends_with(".0") {
        text.truncate(text.len() - 2);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_numbers_print_without_a_decimal_point() {
        assert_eq!(format_number(3.0), "3");
        assert_eq!(format_number(-0.0), "0");
        assert_eq!(format_number(3.5), "3.5");
        assert_eq!(format_number(f64::NAN), "NaN");
        assert_eq!(format_number(f64::INFINITY), "Infinity");
    }

    #[test]
    fn truthiness_matches_javascript() {
        assert!(!Value::Null.truthy());
        assert!(!Value::Number(0.0).truthy());
        assert!(!Value::text("").truthy());
        assert!(Value::text("x").truthy());
        // An empty array is truthy, which is why scripts test `.length`.
        assert!(Value::array(Vec::new()).truthy());
    }

    #[test]
    fn containers_compare_by_identity_not_contents() {
        let first = Value::array(vec![Value::Number(1.0)]);
        let second = Value::array(vec![Value::Number(1.0)]);
        assert!(!first.strict_equals(&second));
        assert!(first.strict_equals(&first.clone()));
    }

    #[test]
    fn json_round_trips_through_the_value_model() {
        let json = serde_json::json!({"files": ["a.rs", "b.rs"], "count": 2, "ok": true});
        let value = Value::from_json(&json);
        assert_eq!(value.to_json(), json);
        assert_eq!(
            value.field("count").and_then(|value| value.as_number()),
            Some(2.0)
        );
    }

    #[test]
    fn whole_numbers_serialize_as_integers() {
        // A count in a result or a prompt must read as `2`, not `2.0`.
        assert_eq!(Value::Number(2.0).to_json(), serde_json::json!(2));
        assert_eq!(Value::Number(2.5).to_json(), serde_json::json!(2.5));
        assert_eq!(Value::Number(f64::NAN).to_json(), serde_json::Value::Null);
        assert_eq!(
            Value::Number(f64::INFINITY).to_json(),
            serde_json::Value::Null
        );
    }

    #[test]
    fn object_fields_keep_their_insertion_order() {
        let value = Value::object(vec![
            (Arc::from("b"), Value::Number(1.0)),
            (Arc::from("a"), Value::Number(2.0)),
        ]);
        let names: Vec<String> = value
            .fields()
            .unwrap()
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(names, ["b", "a"]);
    }
}
