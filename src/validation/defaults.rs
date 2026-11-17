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

/// A schema node's `default`, unless it is `null`: the apiserver skips a nil
/// default everywhere (defaulting/algorithm.go `Default`), also inside a parent
/// it has just defaulted.
fn default_of(schema: &serde_json::Value) -> Option<&serde_json::Value> {
    schema.get("default").filter(|d| !d.is_null())
}

/// Whether `v` is JSON `null` under a non-`nullable` schema node — the apiserver
/// condition (`isNonNullableNull`, defaulting/algorithm.go) for replacing a
/// present-but-null field with its `default`. A `nullable: true` node keeps null.
fn is_non_nullable_null(v: &serde_json::Value, schema: &serde_json::Value) -> bool {
    v.is_null() && schema.get("nullable").and_then(serde_json::Value::as_bool) != Some(true)
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
            // 1. Fill declared-property defaults when the field is missing OR
            //    present-but-null and non-nullable (apiserver `isNonNullableNull`).
            for (key, prop_schema) in props {
                let needs_default = match result.get(key) {
                    None => true,
                    Some(v) => is_non_nullable_null(v, prop_schema),
                };
                if needs_default && let Some(default_val) = default_of(prop_schema) {
                    result.insert(key.clone(), default_val.clone());
                }
            }
            // 2. Recurse into every child now present — including defaults just
            //    inserted by step 1 — so nested defaults inside an inserted object
            //    default get filled too. Mirrors the apiserver's `for k := range x`
            //    over the mutated map. A declared property uses its own schema and
            //    was already defaulted above; any other key falls to
            //    `additionalProperties`, where a present-but-null map value takes
            //    the map's `default` before recursion.
            let keys: Vec<String> = result.keys().cloned().collect();
            for key in keys {
                if let Some(child_schema) = props.get(&key) {
                    let val = result[&key].clone();
                    result.insert(key, apply_defaults_inner(child_schema, &val, depth + 1));
                } else if let Some(add_schema) = additional {
                    let mut val = result[&key].clone();
                    if is_non_nullable_null(&val, add_schema)
                        && let Some(default_val) = default_of(add_schema)
                    {
                        val = default_val.clone();
                    }
                    result.insert(key, apply_defaults_inner(add_schema, &val, depth + 1));
                }
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items_schema) = schema.get("items") {
                let items: Vec<_> = arr
                    .iter()
                    .map(|item| {
                        // A present-but-null non-nullable item takes items.default
                        // before recursion (apiserver defaults each array element).
                        let base = if is_non_nullable_null(item, items_schema) {
                            default_of(items_schema).unwrap_or(item)
                        } else {
                            item
                        };
                        apply_defaults_inner(items_schema, base, depth + 1)
                    })
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
            // 1. Fill declared-property defaults when the field is missing OR
            //    present-but-null and non-nullable (default lives on the child node).
            for (key, child) in &compiled.properties {
                let needs_default = match result.get(key) {
                    None => true,
                    Some(v) => v.is_null() && !child.nullable,
                };
                if needs_default && let Some(default_val) = &child.default {
                    result.insert(key.clone(), default_val.clone());
                }
            }
            // 2. Recurse into every child now present — including defaults just
            //    inserted by step 1 — so nested defaults inside an inserted object
            //    default get filled (matches apiserver `for k := range x`). A
            //    present-but-null map value takes `additionalProperties.default`.
            let keys: Vec<String> = result.keys().cloned().collect();
            for key in keys {
                if let Some(child) = compiled.properties.get(&key) {
                    let val = result[&key].clone();
                    result.insert(key, apply_defaults_compiled_inner(child, &val, depth + 1));
                } else if let Some(add) = additional {
                    let mut val = result[&key].clone();
                    if val.is_null()
                        && !add.nullable
                        && let Some(default_val) = &add.default
                    {
                        val = default_val.clone();
                    }
                    result.insert(key, apply_defaults_compiled_inner(add, &val, depth + 1));
                }
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items) = &compiled.items {
                let elems: Vec<_> = arr
                    .iter()
                    .map(|item| {
                        let base = if item.is_null() && !items.nullable {
                            items.default.as_ref().unwrap_or(item)
                        } else {
                            item
                        };
                        apply_defaults_compiled_inner(items, base, depth + 1)
                    })
                    .collect();
                serde_json::Value::Array(elems)
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
    fn null_default_is_skipped() {
        // The apiserver skips a nil default, also inside a parent it defaulted.
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "default": {},
                    "properties": {"x": {"type": "string", "nullable": true, "default": null}}
                },
                "y": {"type": "string", "nullable": true, "default": null}
            }
        });
        let want = json!({"spec": {}});
        assert_eq!(apply_defaults(&schema, &json!({})), want);
        let compiled = crate::validation::compilation::compile_schema(&schema);
        assert_eq!(apply_defaults_compiled(&compiled, &json!({})), want);
    }

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

    // ── present-but-null default injection (apiserver isNonNullableNull) ──

    #[test]
    fn apply_defaults_replaces_null_declared_property() {
        // A present-but-null non-nullable field takes its default (not just a
        // missing one), matching apiserver defaulting/algorithm.go.
        let schema = json!({
            "type": "object",
            "properties": { "timeout": {"type": "string", "default": "30s"} }
        });
        let value = json!({"timeout": null});
        assert_eq!(apply_defaults(&schema, &value), json!({"timeout": "30s"}));
    }

    #[test]
    fn apply_defaults_keeps_null_when_nullable() {
        // `nullable: true` → isNonNullableNull is false → null is preserved.
        let schema = json!({
            "type": "object",
            "properties": { "timeout": {"type": "string", "default": "30s", "nullable": true} }
        });
        let value = json!({"timeout": null});
        assert_eq!(apply_defaults(&schema, &value), json!({"timeout": null}));
    }

    #[test]
    fn apply_defaults_replaces_null_map_value() {
        // A null map value under additionalProperties takes the map's default.
        let schema = json!({
            "type": "object",
            "additionalProperties": {"type": "string", "default": "d"}
        });
        let value = json!({"a": null, "b": "keep"});
        assert_eq!(apply_defaults(&schema, &value), json!({"a": "d", "b": "keep"}));
    }

    #[test]
    fn apply_defaults_replaces_null_array_item() {
        // A null array element takes items.default before recursion.
        let schema = json!({
            "type": "object",
            "properties": {
                "list": {"type": "array", "items": {"type": "string", "default": "x"}}
            }
        });
        let value = json!({"list": ["a", null, "c"]});
        assert_eq!(apply_defaults(&schema, &value), json!({"list": ["a", "x", "c"]}));
    }

    #[test]
    fn apply_defaults_null_injection_compiled_parity() {
        let schema = json!({
            "type": "object",
            "properties": {
                "timeout": {"type": "string", "default": "30s"},
                "keepNull": {"type": "string", "default": "z", "nullable": true},
                "list": {"type": "array", "items": {"type": "string", "default": "x"}}
            },
            "additionalProperties": {"type": "string", "default": "d"}
        });
        let value = json!({"timeout": null, "keepNull": null, "list": [null], "extra": null});
        let via_schema = apply_defaults(&schema, &value);
        let compiled = crate::validation::compilation::compile_schema(&schema);
        let via_compiled = apply_defaults_compiled(&compiled, &value);
        assert_eq!(via_schema, via_compiled);
        assert_eq!(
            via_compiled,
            json!({"timeout": "30s", "keepNull": null, "list": ["x"], "extra": "d"})
        );
    }
}
