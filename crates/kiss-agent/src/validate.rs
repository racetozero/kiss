//! Tool-argument validation against the tool's JSON schema.

use serde_json::Value;

/// Validate `args` against `schema`. Returns a human-readable error naming
/// the failing paths so the model can correct the call.
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
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("Invalid tool arguments: {}", errors.join("; ")))
    }
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
}
