//! Tool-argument validation against a tool's JSON schema.

use serde_json::Value;

/// Validate `args` against `schema`. Returns a human-readable error naming
/// the failing paths so the model can correct the call.
#[cfg(feature = "native-tools")]
pub fn validate_arguments(schema: &Value, args: &Value) -> Result<(), String> {
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
        ("number" | "integer", Value::String(text)) if !text.trim().is_empty() => {
            let text = text.trim();
            if let Ok(integer) = text.parse::<i64>() {
                Value::from(integer)
            } else {
                match text.parse::<f64>() {
                    // An integer property keeps the whole part, as Pi does.
                    Ok(number) if number.is_finite() && kind == "integer" => {
                        Value::from(number.trunc() as i64)
                    }
                    Ok(number) if number.is_finite() => Value::from(number),
                    _ => value.clone(),
                }
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
