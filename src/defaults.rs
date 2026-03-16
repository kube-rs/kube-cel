//! Schema default value injection.
//!
//! Recursively applies `default` values from an OpenAPI schema to a JSON value,
//! filling in missing fields. This matches the Kubernetes API server behavior
//! where defaults are applied before CEL validation rules execute.
//!
//! **Known limitation:** Only walks top-level `properties`. Does not walk into
//! `allOf`/`oneOf`/`anyOf` branches to find additional property defaults.

/// Apply schema `default` values to a JSON value, returning a new value with
/// missing fields filled in.
///
/// This is a recursive pre-processing pass. It does not modify the input.
#[must_use]
pub fn apply_defaults(schema: &serde_json::Value, value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(obj) => {
            let props = match schema.get("properties").and_then(|p| p.as_object()) {
                Some(p) => p,
                None => return value.clone(),
            };

            // Check if any work needs to be done before cloning
            let has_missing_defaults = props
                .iter()
                .any(|(key, prop_schema)| !obj.contains_key(key) && prop_schema.get("default").is_some());
            let has_children_to_recurse = props.keys().any(|key| obj.contains_key(key));

            if !has_missing_defaults && !has_children_to_recurse {
                return value.clone();
            }

            let mut result = obj.clone();
            for (key, prop_schema) in props {
                match result.get(key) {
                    Some(child) => {
                        let defaulted = apply_defaults(prop_schema, child);
                        result.insert(key.clone(), defaulted);
                    }
                    None => {
                        if let Some(default_val) = prop_schema.get("default") {
                            result.insert(key.clone(), default_val.clone());
                        }
                    }
                }
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items_schema) = schema.get("items") {
                let items: Vec<_> = arr
                    .iter()
                    .map(|item| apply_defaults(items_schema, item))
                    .collect();
                serde_json::Value::Array(items)
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_defaults_fills_missing_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeout": {"type": "string", "default": "30s"},
                "name": {"type": "string"}
            }
        });
        let value = json!({"name": "test"});
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!({"name": "test", "timeout": "30s"}));
    }

    #[test]
    fn apply_defaults_does_not_overwrite_existing() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeout": {"type": "string", "default": "30s"}
            }
        });
        let value = json!({"timeout": "60s"});
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!({"timeout": "60s"}));
    }

    #[test]
    fn apply_defaults_nested_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {"type": "integer", "default": 1}
                    }
                }
            }
        });
        let value = json!({"spec": {}});
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!({"spec": {"replicas": 1}}));
    }

    #[test]
    fn apply_defaults_array_items() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "port": {"type": "integer", "default": 80}
                }
            }
        });
        let value = json!([{"port": 443}, {}]);
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!([{"port": 443}, {"port": 80}]));
    }

    #[test]
    fn apply_defaults_no_schema_properties() {
        let schema = json!({"type": "object"});
        let value = json!({"x": 1});
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!({"x": 1}));
    }

    #[test]
    fn apply_defaults_non_object_passthrough() {
        let schema = json!({"type": "string", "default": "hello"});
        let value = json!("world");
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!("world"));
    }
}
