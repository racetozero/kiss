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
