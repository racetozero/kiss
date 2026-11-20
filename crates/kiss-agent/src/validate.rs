//! Tool-argument validation against a tool's JSON schema.

use serde_json::Value;

/// Validate `args` against `schema`. Returns a human-readable error naming
/// the failing paths so the model can correct the call.
#[cfg(feature = "native-tools")]
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<(), String> {
    // Most calls pass, and most tool schemas are plain. The full validator
    // first checks each schema against the JSON Schema meta-schema, which
    // takes about 10 ms on first use and then keeps several megabytes for
    // the life of the process.
    if certainly_valid(schema, args) {
        return Ok(());
    }
    validate_with_full_vocabulary(schema, args)
}

#[cfg(feature = "native-tools")]
fn validate_with_full_vocabulary(schema: &Value, args: &Value) -> Result<(), String> {
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        // A broken schema is a bug in the tool, not the model's fault;
        // let the call through rather than hard-failing the turn.
        Err(_) => return Ok(()),
    };
    let errors: Vec<String> = validator
        .iter_errors(args)
        .map(|e| {
            let path = e.instance_path.to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .take(5)
        .collect();
    finish(errors)
}

/// True when `value` satisfies `schema` and that can be shown with a few
/// keywords alone. False means "not known": the value is not valid, the
/// schema is not well formed, or the schema has a keyword that is not in
/// this list. The caller then asks the full validator, so this check must
/// never accept a value that the full validator rejects. Where the two
/// could differ, such as a float with an integer value, it returns false.
#[cfg(feature = "native-tools")]
fn certainly_valid(schema: &Value, value: &Value) -> bool {
    let schema = match schema {
        Value::Bool(accepts) => return *accepts,
        Value::Object(schema) => schema,
        _ => return false,
    };
    let properties = match schema.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => return false,
    };
    schema.iter().all(|(keyword, rule)| match keyword.as_str() {
        // These keywords say nothing about which values are valid.
        "description" | "title" | "default" | "examples" | "$comment" => true,
        "type" => match rule {
            Value::String(kind) => certainly_has_type(value, kind),
            Value::Array(kinds) => kinds.iter().any(|kind| {
                kind.as_str()
                    .is_some_and(|kind| certainly_has_type(value, kind))
            }),
            _ => false,
        },
        "enum" => rule
            .as_array()
            .is_some_and(|allowed| allowed.contains(value)),
        "const" => rule == value,
        "required" => {
            let Some(names) = rule.as_array() else {
                return false;
            };
            let object = value.as_object();
            names.iter().all(|name| {
                name.as_str()
                    .is_some_and(|name| object.is_none_or(|object| object.contains_key(name)))
            })
        }
        "properties" => value.as_object().is_none_or(|object| {
            object.iter().all(|(name, child)| {
                properties
                    .and_then(|properties| properties.get(name))
                    .is_none_or(|child_schema| certainly_valid(child_schema, child))
            })
        }),
        "additionalProperties" => value.as_object().is_none_or(|object| {
            object
                .iter()
                .filter(|(name, _)| properties.is_none_or(|known| !known.contains_key(*name)))
                .all(|(_, child)| certainly_valid(rule, child))
        }),
        // An array here is the tuple form of older drafts.
        "items" => {
            !rule.is_array()
                && value
                    .as_array()
                    .is_none_or(|items| items.iter().all(|item| certainly_valid(rule, item)))
        }
        _ => false,
    })
}

#[cfg(feature = "native-tools")]
fn certainly_has_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        // The full validator also accepts a float such as 1.0.
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// Browser agents accept host-provided schemas but deliberately avoid pulling
/// the large native JSON Schema resolver (and its random URL identifiers) into
/// WebAssembly. This validator covers the vocabulary used by KISS tool inputs:
/// type, required, properties, arrays/items, enum, and additionalProperties.
#[cfg(not(feature = "native-tools"))]
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    validate_portable(schema, args, "", &mut errors);
    finish(errors)
}

/// Turn loosely typed arguments into the values that `schema` most likely
/// means, before validation, as Pi does for every tool. A numeric string
/// becomes a number, a single value becomes a one-item array, a `null` in an
/// optional property is dropped, and a union takes the first branch that the
/// coerced value satisfies. A value that cannot be coerced stays as it is,
/// so validation reports it.
pub fn coerce_arguments(schema: &Value, mut value: Value) -> Value {
    if !schema.is_object() {
        return value;
    }
    for member in schema
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        value = coerce_arguments(member, value);
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(branches) = schema.get(key).and_then(Value::as_array)
            && !branches
                .iter()
                .any(|branch| validate_arguments(branch, &value).is_ok())
            && let Some(coerced) = branches.iter().find_map(|branch| {
                let coerced = coerce_arguments(branch, value.clone());
                validate_arguments(branch, &coerced)
                    .is_ok()
                    .then_some(coerced)
            })
        {
            value = coerced;
        }
    }
    let allowed: Vec<&Value> = schema
        .get("const")
        .into_iter()
        .chain(
            schema
                .get("enum")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .collect();
    if !allowed.is_empty()
        && !allowed.contains(&&value)
        && let Some(found) = allowed.iter().find(|allowed| {
            ["string", "number", "boolean"].iter().any(|kind| {
                matches_type(allowed, kind) && coerce_primitive(&value, kind) == ***allowed
            })
        })
    {
        value = (*found).clone();
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !types.is_empty() && !types.iter().any(|kind| matches_type(&value, kind)) {
        if types.contains(&"array") {
            value = Value::Array(vec![value]);
        } else if let Some(coerced) = types
            .iter()
            .map(|kind| coerce_primitive(&value, kind))
            .find(|coerced| types.iter().any(|kind| matches_type(coerced, kind)))
        {
            value = coerced;
        }
    }
    match &mut value {
        Value::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            // An optional `null` that the property rejects means "omitted".
            object.retain(|name, child| {
                !child.is_null()
                    || required.contains(&name.as_str())
                    || properties
                        .and_then(|properties| properties.get(name))
                        .is_none_or(|child_schema| validate_arguments(child_schema, child).is_ok())
            });
            for (name, child) in object.iter_mut() {
                let child_schema = properties
                    .and_then(|properties| properties.get(name))
                    .or_else(|| {
                        schema
                            .get("additionalProperties")
                            .filter(|schema| schema.is_object())
                    });
                if let Some(child_schema) = child_schema {
                    *child = coerce_arguments(child_schema, child.take());
                }
            }
        }
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items.iter_mut() {
                    *item = coerce_arguments(item_schema, item.take());
                }
            }
        }
        _ => {}
    }
    value
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// The value as `kind` when it converts without loss, as in Ajv's
/// `coerceTypes`, otherwise the value unchanged.
fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match (kind, value) {
        ("number" | "integer", Value::Null) => Value::from(0),
        ("number" | "integer", Value::Bool(flag)) => Value::from(u8::from(*flag)),
        // serde_json's number parser is already in the build. Rust's float
        // parser would add about 12 KB (gzip) to the browser WebAssembly module.
        ("number" | "integer", Value::String(text)) => {
            match serde_json::from_str::<serde_json::Number>(text.trim()) {
                Ok(number) if kind == "number" || number.is_i64() || number.is_u64() => {
                    Value::Number(number)
                }
                // An integer property keeps the whole part, as Pi does.
                Ok(number) => number.as_f64().map_or_else(
                    || value.clone(),
                    |number| Value::from(number.trunc() as i64),
                ),
                Err(_) => value.clone(),
            }
        }
        ("boolean", Value::Null) => Value::Bool(false),
        ("boolean", Value::String(text)) if text == "true" => Value::Bool(true),
        ("boolean", Value::String(text)) if text == "false" => Value::Bool(false),
        ("boolean", Value::Number(number)) if number.as_i64() == Some(1) => Value::Bool(true),
        ("boolean", Value::Number(number)) if number.as_i64() == Some(0) => Value::Bool(false),
        ("string", Value::Null) => Value::String(String::new()),
        ("string", Value::Number(number)) => Value::String(number.to_string()),
        ("string", Value::Bool(flag)) => Value::String(flag.to_string()),
        ("null", Value::String(text)) if text.is_empty() => Value::Null,
        ("null", Value::Bool(false)) => Value::Null,
        ("null", Value::Number(number)) if number.as_f64() == Some(0.0) => Value::Null,
        _ => value.clone(),
    }
}

fn finish(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("Invalid tool arguments: {}", errors.join("; ")))
    }
}

#[cfg(not(feature = "native-tools"))]
fn validate_portable(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    if errors.len() >= 5 || !schema.is_object() {
        return;
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        errors.push(format!("{}: value is not in enum", display_path(path)));
        return;
    }

    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let matches = match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        if !matches {
            errors.push(format!("{}: expected {expected}", display_path(path)));
            return;
        }
    }

    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    errors.push(format!(
                        "{}/{}: required property is missing",
                        path,
                        escape(name)
                    ));
                    if errors.len() >= 5 {
                        return;
                    }
                }
            }
        }

        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (name, child) in object {
                if let Some(child_schema) = properties.get(name) {
                    validate_portable(
                        child_schema,
                        child,
                        &format!("{}/{}", path, escape(name)),
                        errors,
                    );
                } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                    errors.push(format!(
                        "{}/{}: additional property is not allowed",
                        path,
                        escape(name)
                    ));
                }
                if errors.len() >= 5 {
                    return;
                }
            }
        }
    }

    if let (Some(items), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, child) in values.iter().enumerate() {
            validate_portable(items, child, &format!("{path}/{index}"), errors);
            if errors.len() >= 5 {
                return;
            }
        }
    }
}

#[cfg(not(feature = "native-tools"))]
fn display_path(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

#[cfg(not(feature = "native-tools"))]
fn escape(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(feature = "native-tools")]
    #[test]
    fn the_quick_check_accepts_plain_calls_and_leaves_the_rest_to_the_full_validator() {
        let schema = json!({
            "type": "object",
            "description": "Read a file",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "mode": {"enum": ["text", "image"]},
                "tags": {"type": "array", "items": {"type": "string"}},
            },
            "required": ["path"],
            "additionalProperties": false,
        });
        for accepted in [
            json!({"path": "a"}),
            json!({"path": "a", "limit": 3, "mode": "image", "tags": ["x", "y"]}),
        ] {
            assert!(certainly_valid(&schema, &accepted), "{accepted}");
        }
        for unknown in [
            json!({}),
            json!({"path": 4}),
            json!({"path": "a", "other": true}),
            json!({"path": "a", "mode": "binary"}),
            json!({"path": "a", "tags": ["x", 1]}),
            // The full validator accepts a float with an integer value.
            json!({"path": "a", "limit": 3.0}),
        ] {
            assert!(!certainly_valid(&schema, &unknown), "{unknown}");
        }
        assert!(validate_arguments(&schema, &json!({"path": "a", "limit": 3.0})).is_ok());
        // A keyword outside the list always goes to the full validator.
        for schema in [
            json!({"type": "string", "minLength": 1}),
            json!({"oneOf": [{"type": "string"}]}),
            json!({"$schema": "http://json-schema.org/draft-07/schema#", "type": "string"}),
            json!({"type": "array", "items": [{"type": "string"}]}),
            json!(false),
        ] {
            assert!(!certainly_valid(&schema, &json!("text")), "{schema}");
        }
    }

    /// A normal call to a file or shell tool must not start the full
    /// validator. If a tool schema gains a keyword that the quick check does
    /// not know, add the keyword there or accept the cost on purpose.
    #[cfg(feature = "native-tools")]
    #[test]
    fn the_quick_check_accepts_normal_calls_to_the_file_and_shell_tools() {
        use crate::tool::AgentTool as _;
        use crate::tools::{bash::BashTool, edit::EditTool, read::ReadTool, write::WriteTool};
        let cwd = std::path::PathBuf::from(".");
        let calls = [
            (
                ReadTool { cwd: cwd.clone() }.parameters(),
                json!({"path": "src/lib.rs", "offset": 10, "limit": 200}),
            ),
            (
                BashTool::new(cwd.clone()).parameters(),
                json!({"command": "ls", "timeout": 30}),
            ),
            (
                EditTool { cwd: cwd.clone() }.parameters(),
                json!({"path": "a.rs", "edits": [{"oldText": "a", "newText": "b"}]}),
            ),
            (
                WriteTool { cwd }.parameters(),
                json!({"path": "a.rs", "content": "text"}),
            ),
        ];
        for (schema, arguments) in calls {
            assert!(certainly_valid(&schema, &arguments), "{schema}");
        }
    }

    /// The quick check may only accept what the full validator accepts.
    #[cfg(feature = "native-tools")]
    #[test]
    fn the_quick_check_never_accepts_what_the_full_validator_rejects() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |bound: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % bound as u64) as usize
        };
        fn value(next: &mut dyn FnMut(usize) -> usize, depth: usize) -> Value {
            match next(if depth == 0 { 9 } else { 11 }) {
                0 => Value::Null,
                1 => json!(true),
                2 => json!(next(3)),
                3 => json!(-1),
                4 => json!(1.0),
                5 => json!(1.5),
                6 => json!(""),
                7 => Value::from(["a", "b", "text"][next(3)]),
                8 => json!(u64::MAX),
                9 => Value::Array((0..next(3)).map(|_| value(next, depth - 1)).collect()),
                _ => Value::Object(
                    (0..next(4))
                        .map(|_| (["a", "b", "c"][next(3)].to_string(), value(next, depth - 1)))
                        .collect(),
                ),
            }
        }
        fn schema(next: &mut dyn FnMut(usize) -> usize, depth: usize) -> Value {
            if depth == 0 || next(12) == 0 {
                return match next(6) {
                    0 => json!(true),
                    1 => json!(false),
                    // Not a schema.
                    2 => json!(5),
                    _ => json!({}),
                };
            }
            let kinds = [
                "object", "array", "string", "number", "integer", "boolean", "null",
            ];
            let mut object = serde_json::Map::new();
            for _ in 0..next(5) {
                let (keyword, rule) = match next(16) {
                    0..=3 => ("type", Value::from(kinds[next(kinds.len())])),
                    4 => {
                        let pair = vec![kinds[next(kinds.len())], kinds[next(kinds.len())]];
                        ("type", Value::from(pair))
                    }
                    5 => ("enum", json!([value(next, 1), value(next, 1), 1, "a"])),
                    6 => ("const", value(next, 1)),
                    7 => ("required", Value::from(vec![["a", "b", "c"][next(3)]])),
                    8 | 9 => (
                        "properties",
                        json!({"a": schema(next, depth - 1), "b": schema(next, depth - 1)}),
                    ),
                    10 => ("additionalProperties", schema(next, depth - 1)),
                    11 => ("items", schema(next, depth - 1)),
                    12 => ("description", json!("text")),
                    // Keywords and forms that the quick check does not know.
                    13 => ("minLength", json!(1)),
                    14 => (
                        "anyOf",
                        json!([schema(next, depth - 1), {"type": "string"}]),
                    ),
                    // Not well formed.
                    _ => (
                        ["type", "required", "properties", "enum"][next(4)],
                        json!(7),
                    ),
                };
                object.insert(keyword.to_string(), rule);
            }
            Value::Object(object)
        }
        let mut accepted = 0;
        for _ in 0..6_000 {
            let schema = schema(&mut next, 3);
            for _ in 0..4 {
                let value = value(&mut next, 2);
                if certainly_valid(&schema, &value) {
                    accepted += 1;
                    assert_eq!(
                        validate_with_full_vocabulary(&schema, &value),
                        Ok(()),
                        "schema {schema} value {value}"
                    );
                }
            }
        }
        // The test is empty if the quick check accepts almost nothing.
        assert!(accepted > 2_000, "only {accepted} accepted");
    }

    #[test]
    #[ignore = "release-mode performance benchmark"]
    fn benchmark_performance_tool_argument_validation() {
        let plain = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Path to the file to read"},
                "offset": {"type": "number", "description": "Line number to start from"},
                "limit": {"type": "number", "description": "Maximum number of lines"}
            },
            "required": ["path"]
        });
        // `minLength` is outside the quick check, so this schema always
        // goes to the full validator.
        let mut full = plain.clone();
        full["properties"]["path"]["minLength"] = json!(1);
        let args = json!({"path": "src/lib.rs", "offset": 10, "limit": 200});
        kiss_bench::measure(
            "tool_arguments_quick_check",
            15,
            200,
            "read_tool_schema",
            || validate_arguments(&plain, &args),
        );
        let started = std::time::Instant::now();
        validate_arguments(&full, &args).unwrap();
        kiss_bench::report(
            "tool_arguments_full_validator_first_use",
            &mut [started.elapsed().as_nanos()],
            1,
            "first_full_validation_in_the_process",
        );
        kiss_bench::measure(
            "tool_arguments_full_validator",
            15,
            200,
            "read_tool_schema_with_min_length",
            || validate_arguments(&full, &args),
        );
    }

    #[test]
    fn accepts_valid_and_rejects_invalid() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "number"},
            },
            "required": ["path"],
        });
        assert!(validate_arguments(&schema, &json!({"path": "a"})).is_ok());
        assert!(validate_arguments(&schema, &json!({"limit": 3})).is_err());
        assert!(validate_arguments(&schema, &json!({"path": 4})).is_err());
    }

    #[test]
    fn coerces_loose_model_arguments_to_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "offset": {"type": "number"},
                "paths": {"type": "array", "items": {"type": "string"}},
                "force": {"type": "boolean"},
                "label": {"type": "string"},
                "mode": {"enum": [1, 2]},
                "note": {"type": "string"},
                "size": {"anyOf": [{"type": "integer"}, {"type": "string", "enum": ["auto"]}]},
                "extra": {"type": ["integer", "null"]},
            },
            "required": ["limit"],
        });
        let coerced = coerce_arguments(
            &schema,
            json!({
                "limit": "3.5",
                "offset": " 2.25 ",
                "paths": "src/lib.rs",
                "force": "true",
                "label": 7,
                "mode": "2",
                "note": null,
                "size": "12",
                "extra": "",
            }),
        );
        assert_eq!(
            coerced,
            json!({
                "limit": 3,
                "offset": 2.25,
                "paths": ["src/lib.rs"],
                "force": true,
                "label": "7",
                "mode": 2,
                "size": 12,
                "extra": null,
            })
        );
        assert!(validate_arguments(&schema, &coerced).is_ok());
        // Values that do not convert stay, so validation reports them.
        let kept = coerce_arguments(&schema, json!({"limit": "many", "force": "yes"}));
        assert_eq!(kept, json!({"limit": "many", "force": "yes"}));
        assert!(validate_arguments(&schema, &kept).is_err());
        // Valid arguments come back unchanged.
        let valid = json!({"limit": 1, "paths": ["a"], "size": "auto"});
        assert_eq!(coerce_arguments(&schema, valid.clone()), valid);
    }

    #[cfg(not(feature = "native-tools"))]
    #[test]
    fn portable_validation_checks_nested_arrays_and_extra_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "integer"}}
            },
            "additionalProperties": false
        });
        assert!(validate_arguments(&schema, &json!({"items": [1, 2]})).is_ok());
        let error =
            validate_arguments(&schema, &json!({"items": [1, "x"], "extra": true})).unwrap_err();
        assert!(error.contains("/items/1"), "{error}");
        assert!(error.contains("/extra"), "{error}");
    }
}
