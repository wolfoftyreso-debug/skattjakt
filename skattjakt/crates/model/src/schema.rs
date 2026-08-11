//! Structured-output validation.
//!
//! The provider is asked to emit JSON matching a schema, but "asked" is not
//! "guaranteed": a truncated response, a provider without schema enforcement,
//! or a future model can all produce something else. Section 6 of the
//! engineering principles says uncertainty must lead to verification rather
//! than confident output, so every response is validated before it is allowed
//! to reach the pipeline.
//!
//! This is a deliberately small subset of JSON Schema — the constructs the
//! task schemas actually use. An unsupported keyword is ignored rather than
//! silently treated as satisfied by something else.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Validates `value` against `schema`, returning every violation found rather
/// than stopping at the first, so a malformed response can be diagnosed in one
/// pass.
pub fn validate(value: &Value, schema: &Value) -> Result<(), Vec<SchemaError>> {
    let mut errors = Vec::new();
    check(value, schema, "$", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check(value: &Value, schema: &Value, path: &str, errors: &mut Vec<SchemaError>) {
    let Some(schema) = schema.as_object() else {
        return;
    };

    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        if !type_matches(value, expected) {
            errors.push(SchemaError {
                path: path.to_string(),
                message: format!("expected {expected}, found {}", type_name(value)),
            });
            // The value is the wrong shape; descending further would only
            // produce noise.
            return;
        }
    }

    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            errors.push(SchemaError {
                path: path.to_string(),
                message: format!("value {value} is not one of the permitted options"),
            });
        }
    }

    if let Some(object) = value.as_object() {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for field in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(field) {
                    errors.push(SchemaError {
                        path: format!("{path}.{field}"),
                        message: "required field is missing".to_string(),
                    });
                }
            }
        }

        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, subschema) in properties {
                if let Some(child) = object.get(name) {
                    check(child, subschema, &format!("{path}.{name}"), errors);
                }
            }

            // `additionalProperties: false` is how the task schemas keep a
            // model from inventing fields the pipeline will silently ignore.
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                for name in object.keys() {
                    if !properties.contains_key(name) {
                        errors.push(SchemaError {
                            path: format!("{path}.{name}"),
                            message: "unexpected field".to_string(),
                        });
                    }
                }
            }
        }
    }

    if let Some(array) = value.as_array() {
        if let Some(items) = schema.get("items") {
            for (i, item) in array.iter().enumerate() {
                check(item, items, &format!("{path}[{i}]"), errors);
            }
        }
    }
}

fn type_matches(value: &Value, expected: &str) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        // JSON has one number type; the distinction matters for money fields.
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate_schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["candidates"],
            "properties": {
                "candidates": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["title", "category", "rationale"],
                        "properties": {
                            "title": {"type": "string"},
                            "category": {"type": "string", "enum": ["tax", "costs", "risk"]},
                            "rationale": {"type": "string"},
                            "confidence_hint": {"type": "number"}
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn a_well_formed_response_validates() {
        let value = json!({
            "candidates": [
                {"title": "Periodiseringsfond", "category": "tax", "rationale": "överskott"}
            ]
        });
        assert!(validate(&value, &candidate_schema()).is_ok());
    }

    #[test]
    fn a_missing_required_field_is_reported_with_its_path() {
        let value = json!({"candidates": [{"title": "T", "category": "tax"}]});
        let errors = validate(&value, &candidate_schema()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, "$.candidates[0].rationale");
    }

    #[test]
    fn an_invented_field_is_rejected() {
        let value = json!({
            "candidates": [{
                "title": "T", "category": "tax", "rationale": "r",
                "estimated_saving": 100000
            }]
        });
        let errors = validate(&value, &candidate_schema()).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.path == "$.candidates[0].estimated_saving"));
    }

    #[test]
    fn a_value_outside_the_enum_is_rejected() {
        let value = json!({
            "candidates": [{"title": "T", "category": "invented", "rationale": "r"}]
        });
        let errors = validate(&value, &candidate_schema()).unwrap_err();
        assert!(errors
            .iter()
            .any(|e| e.message.contains("not one of the permitted")));
    }

    #[test]
    fn a_wrong_type_is_reported_once_not_cascaded() {
        let value = json!({"candidates": "not an array"});
        let errors = validate(&value, &candidate_schema()).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("expected array"));
    }

    #[test]
    fn integers_and_numbers_are_distinguished() {
        let schema = json!({"type": "object", "properties": {"n": {"type": "integer"}}});
        assert!(validate(&json!({"n": 5}), &schema).is_ok());
        assert!(validate(&json!({"n": 5.5}), &schema).is_err());
    }

    #[test]
    fn multiple_violations_are_all_reported_in_one_pass() {
        let value = json!({
            "candidates": [
                {"title": "T"},
                {"category": "nope", "rationale": "r"}
            ]
        });
        let errors = validate(&value, &candidate_schema()).unwrap_err();
        assert!(errors.len() >= 3, "got {errors:?}");
    }

    #[test]
    fn optional_fields_may_be_absent() {
        let value = json!({
            "candidates": [{"title": "T", "category": "risk", "rationale": "r"}]
        });
        assert!(validate(&value, &candidate_schema()).is_ok());
    }
}
