//! Schema default value injection.
//!
//! Recursively applies `default` values from an OpenAPI schema to a JSON value,
//! filling in missing fields. This matches the Kubernetes API server behavior
//! where defaults are applied before CEL validation rules execute.
//!
//! Walks `properties`, array `items`, and `additionalProperties` (map values),
//! mirroring the validator's tree walk so the defaulted object the rules see
//! matches what the apiserver evaluates.
//!
//! **Known limitation:** Does not walk into `allOf`/`oneOf`/`anyOf` branches.
//! Structural schemas forbid `default` inside those logical junctors, so the
//! apiserver does not default there either — the gap is believed unreachable.

/// Apply schema `default` values to a JSON value, returning a new value with
/// missing fields filled in.
///
/// This is a recursive pre-processing pass. It does not modify the input.
///
/// # Limits
///
/// Defaulting stops at the same maximum nesting depth the validators use;
/// fields nested deeper are returned unchanged. Unlike the validators this pass
/// has no error channel, so the cap is silent here. The cap is shared, so a
/// subsequent [`Validator`](crate::Validator) pass over the same schema fails
/// closed with [`ErrorKind::SchemaTooDeep`](crate::ErrorKind::SchemaTooDeep)
/// at the same boundary.
#[must_use]
pub fn apply_defaults(schema: &serde_json::Value, value: &serde_json::Value) -> serde_json::Value {
    apply_defaults_inner(schema, value, 0)
}

fn apply_defaults_inner(
    schema: &serde_json::Value,
    value: &serde_json::Value,
    depth: usize,
) -> serde_json::Value {
    if depth > crate::validation::compilation::MAX_SCHEMA_DEPTH {
        return value.clone();
    }

    match value {
        serde_json::Value::Object(obj) => {
            let props = schema.get("properties").and_then(|p| p.as_object());

            // A map node defaults INTO its existing values (it never invents map
            // entries from a default). Skipped when preserve-unknown-fields is set,
            // matching the validator's walk.
            let preserve_unknown = schema
                .get("x-kubernetes-preserve-unknown-fields")
                .and_then(|v| v.as_bool())
                == Some(true);
            let additional = if preserve_unknown {
                None
            } else {
                schema.get("additionalProperties").filter(|a| a.is_object())
            };

            let empty = serde_json::Map::new();
            let props = props.unwrap_or(&empty);
            if props.is_empty() && additional.is_none() {
                return value.clone();
            }

            let mut result = obj.clone();
            // 1. Fill missing declared-property defaults.
            for (key, prop_schema) in props {
                if !result.contains_key(key)
                    && let Some(default_val) = prop_schema.get("default")
                {
                    result.insert(key.clone(), default_val.clone());
                }
            }
            // 2. Recurse into existing children: a declared property uses its own
            //    schema; any other key falls to `additionalProperties` (map value).
            for (key, val) in obj {
                if let Some(prop_schema) = props.get(key) {
                    result.insert(key.clone(), apply_defaults_inner(prop_schema, val, depth + 1));
                } else if let Some(add_schema) = additional {
                    result.insert(key.clone(), apply_defaults_inner(add_schema, val, depth + 1));
                }
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items_schema) = schema.get("items") {
                let items: Vec<_> = arr
                    .iter()
                    .map(|item| apply_defaults_inner(items_schema, item, depth + 1))
                    .collect();
                serde_json::Value::Array(items)
            } else {
                value.clone()
            }
        }
        _ => value.clone(),
    }
}

/// Apply `default`s using a pre-[compiled](crate::compile_schema) schema tree.
///
/// The compiled-validation entry points cannot re-read the raw JSON schema, so
/// this mirrors [`apply_defaults`] over a [`CompiledSchema`], reading each node's
/// retained `default`. Kept in lockstep with [`apply_defaults_inner`] so both
/// validation paths default identically.
pub(crate) fn apply_defaults_compiled(
    compiled: &crate::validation::compilation::CompiledSchema,
    value: &serde_json::Value,
) -> serde_json::Value {
    apply_defaults_compiled_inner(compiled, value, 0)
}

fn apply_defaults_compiled_inner(
    compiled: &crate::validation::compilation::CompiledSchema,
    value: &serde_json::Value,
    depth: usize,
) -> serde_json::Value {
    if depth > crate::validation::compilation::MAX_SCHEMA_DEPTH {
        return value.clone();
    }

    match value {
        serde_json::Value::Object(obj) => {
            let additional = if compiled.preserve_unknown_fields {
                None
            } else {
                compiled.additional_properties.as_deref()
            };
            if compiled.properties.is_empty() && additional.is_none() {
                return value.clone();
            }

            let mut result = obj.clone();
            // 1. Fill missing declared-property defaults (default lives on the child node).
            for (key, child) in &compiled.properties {
                if !result.contains_key(key)
                    && let Some(default_val) = &child.default
                {
                    result.insert(key.clone(), default_val.clone());
                }
            }
            // 2. Recurse into existing children: declared property, else map value.
            for (key, val) in obj {
                if let Some(child) = compiled.properties.get(key) {
                    result.insert(key.clone(), apply_defaults_compiled_inner(child, val, depth + 1));
                } else if let Some(add) = additional {
                    result.insert(key.clone(), apply_defaults_compiled_inner(add, val, depth + 1));
                }
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items) = &compiled.items {
                let items: Vec<_> = arr
                    .iter()
                    .map(|item| apply_defaults_compiled_inner(items, item, depth + 1))
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

    #[test]
    fn apply_defaults_walks_additional_properties() {
        // Map values get their own missing defaults filled (kube-rs/kube-cel#9
        // nested-additionalProperties fidelity gap).
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } }
            }
        });
        let value = json!({ "a": {}, "b": { "x": "keep" } });
        let result = apply_defaults(&schema, &value);
        assert_eq!(result, json!({ "a": { "x": "d" }, "b": { "x": "keep" } }));
    }

    #[test]
    fn apply_defaults_skips_additional_properties_under_preserve_unknown() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-preserve-unknown-fields": true,
            "additionalProperties": {
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } }
            }
        });
        let value = json!({ "a": {} });
        // preserve-unknown: not a structural map → do not default into values.
        assert_eq!(apply_defaults(&schema, &value), json!({ "a": {} }));
    }

    #[test]
    fn apply_defaults_compiled_matches_schema_defaulter() {
        // The compiled defaulter must produce the same result as the schema one,
        // including through additionalProperties and array items.
        let schema = json!({
            "type": "object",
            "properties": {
                "list": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "p": { "type": "integer", "default": 80 } }
                    }
                },
                "m": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": { "x": { "type": "string", "default": "d" } }
                    }
                }
            }
        });
        let value = json!({ "list": [ {} ], "m": { "a": {} } });
        let via_schema = apply_defaults(&schema, &value);
        let compiled = crate::validation::compilation::compile_schema(&schema);
        let via_compiled = apply_defaults_compiled(&compiled, &value);
        assert_eq!(via_schema, via_compiled);
        assert_eq!(
            via_compiled,
            json!({ "list": [ { "p": 80 } ], "m": { "a": { "x": "d" } } })
        );
    }
}
