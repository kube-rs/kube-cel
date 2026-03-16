# kube-cel Roadmap Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring kube-cel from "CEL function library with basic validation" to "complete client-side K8s CEL validation toolkit" — filling spec gaps, adding static analysis, and supporting ValidatingAdmissionPolicy.

**Architecture:** Four phases of incremental improvements. Phase 0-1 are schema/validation pipeline fixes (0.5.1). Phase 2 adds static analysis on the CEL AST (0.5.2). Phase 3 adds VAP support as a new module (0.5.3). Each phase is independently shippable. All changes are additive (patch bumps). The only semver consideration is adding `#[non_exhaustive]` to `SchemaFormat` in Task 1 — acceptable for pre-1.0.

**Tech Stack:** Rust, `cel` crate 0.13 (AST via `Program::expression()`), `serde_json`, `chrono`

---

## File Structure Overview

**New files:**
- `src/defaults.rs` — schema default value injection (Phase 1)
- `src/analysis.rs` — static analysis: scope validation + cost estimation (Phase 2)
- `src/vap.rs` — ValidatingAdmissionPolicy evaluator (Phase 3)

**Modified files:**
- `src/compilation.rs` — `CompiledSchema` gets new fields (all phases)
- `src/validation.rs` — walker + evaluator enhancements (Phase 0-1)
- `src/values.rs` — `SchemaFormat::IntOrString`, embedded-resource injection (Phase 0-1)
- `src/lib.rs` — new module declarations (Phase 1-3)
- `Cargo.toml` — no new deps needed until Phase 3

---

## Chunk 1: Phase 0 — Quick Wins (spec compliance, 0.5.1)

### Task 1: Add `x-kubernetes-int-or-string` to SchemaFormat

**Files:**
- Modify: `src/values.rs:22-41` (SchemaFormat enum + from_schema)
- Modify: `src/values.rs:170-186` (convert_string_with_format)
- Test: `src/values.rs` (inline tests)

- [ ] **Step 1: Write failing test for int-or-string schema format detection**

```rust
// In src/values.rs mod tests
#[test]
fn int_or_string_schema_format_detected() {
    let schema = json!({
        "x-kubernetes-int-or-string": true
    });
    assert_eq!(SchemaFormat::from_schema(&schema), SchemaFormat::IntOrString);
}

#[test]
fn int_or_string_int_value_preserved() {
    let schema = json!({"x-kubernetes-int-or-string": true});
    let result = json_to_cel_with_schema(&json!(8080), &schema);
    assert_eq!(result, Value::Int(8080));
}

#[test]
fn int_or_string_string_value_preserved() {
    let schema = json!({"x-kubernetes-int-or-string": true});
    let result = json_to_cel_with_schema(&json!("http"), &schema);
    assert_eq!(result, Value::String(Arc::new("http".into())));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --features validation int_or_string -v`
Expected: FAIL — `SchemaFormat::IntOrString` does not exist

- [ ] **Step 3: Implement SchemaFormat::IntOrString**

In `src/values.rs`, add `#[non_exhaustive]` (prevents downstream exhaustive matches — future-proofing) and the new variant:

```rust
#[non_exhaustive]
pub enum SchemaFormat {
    DateTime,
    Duration,
    /// `x-kubernetes-int-or-string: true` — field can be int or string.
    /// This is primarily a marker to prevent `format: "date-time"` etc. from being
    /// interpreted when `x-kubernetes-int-or-string` is set. The actual int/string
    /// conversion happens through the normal Number/String branches in json_to_cel.
    IntOrString,
    #[default]
    None,
}

impl SchemaFormat {
    pub(crate) fn from_schema(schema: &serde_json::Value) -> Self {
        if schema.get("x-kubernetes-int-or-string").and_then(|v| v.as_bool()) == Some(true) {
            return SchemaFormat::IntOrString;
        }
        match schema.get("format").and_then(|f| f.as_str()) {
            Some("date-time") => SchemaFormat::DateTime,
            Some("duration") => SchemaFormat::Duration,
            _ => SchemaFormat::None,
        }
    }
}
```

Add arm to `convert_string_with_format`:

```rust
SchemaFormat::IntOrString => Value::String(Arc::new(s.to_string())),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --features validation int_or_string -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/values.rs
git commit -m "feat: add SchemaFormat::IntOrString for x-kubernetes-int-or-string fields"
```

---

### Task 2: Walk `allOf`/`oneOf`/`anyOf` in schema composition

**Files:**
- Modify: `src/compilation.rs:140-198` (CompiledSchema + compile_schema)
- Modify: `src/validation.rs:122-247` (walk_schema + walk_compiled)
- Test: `src/validation.rs` (inline tests)

- [ ] **Step 1: Write failing test for allOf walking**

```rust
// In src/validation.rs mod tests
#[test]
fn all_of_validations_evaluated() {
    let schema = json!({
        "type": "object",
        "properties": {
            "x": {"type": "integer"},
            "y": {"type": "integer"}
        },
        "allOf": [
            {
                "x-kubernetes-validations": [
                    {"rule": "self.x >= 0", "message": "x must be non-negative"}
                ]
            },
            {
                "x-kubernetes-validations": [
                    {"rule": "self.y >= 0", "message": "y must be non-negative"}
                ]
            }
        ]
    });
    let obj = json!({"x": -1, "y": -1});
    let errors = validate(&schema, &obj, None);
    assert_eq!(errors.len(), 2);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation all_of_validations -v`
Expected: FAIL — 0 errors found, expected 2

- [ ] **Step 3: Add allOf/oneOf/anyOf fields to CompiledSchema**

In `src/compilation.rs`:

```rust
pub struct CompiledSchema {
    pub validations: Vec<Result<CompilationResult, CompilationError>>,
    pub properties: HashMap<String, CompiledSchema>,
    pub items: Option<Box<CompiledSchema>>,
    pub additional_properties: Option<Box<CompiledSchema>>,
    pub format: SchemaFormat,
    /// Compiled `allOf` branch schemas.
    pub all_of: Vec<CompiledSchema>,
    /// Compiled `oneOf` branch schemas.
    pub one_of: Vec<CompiledSchema>,
    /// Compiled `anyOf` branch schemas.
    pub any_of: Vec<CompiledSchema>,
}
```

Add helper and populate in `compile_schema`:

```rust
fn compile_schema_array(schema: &serde_json::Value, key: &str) -> Vec<CompiledSchema> {
    schema.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(compile_schema).collect())
        .unwrap_or_default()
}

// Inside compile_schema():
let all_of = compile_schema_array(schema, "allOf");
let one_of = compile_schema_array(schema, "oneOf");
let any_of = compile_schema_array(schema, "anyOf");
```

- [ ] **Step 4: Walk allOf/oneOf/anyOf in walk_schema**

> **Design note:** All three keywords are walked identically (evaluate all branches).
> For `allOf` this is semantically correct (all branches apply).
> For `oneOf`/`anyOf`, the K8s API server also evaluates `x-kubernetes-validations`
> from all composition branches — branch selection only affects structural schema
> validation, not CEL rule evaluation. So identical treatment is intentional.

In `src/validation.rs`, after the `additionalProperties` block in `walk_schema`:

```rust
// Walk allOf/oneOf/anyOf branches — all treated identically for CEL evaluation
for keyword in &["allOf", "oneOf", "anyOf"] {
    if let Some(branches) = schema.get(keyword).and_then(|v| v.as_array()) {
        for branch in branches {
            self.walk_schema(branch, value, old_value, path.clone(), errors, base_ctx);
        }
    }
}
```

In `walk_compiled`, after the `additional_properties` block:

```rust
for branch in compiled.all_of.iter()
    .chain(compiled.one_of.iter())
    .chain(compiled.any_of.iter())
{
    self.walk_compiled(branch, value, old_value, path.clone(), errors, base_ctx);
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test --features validation all_of_validations -v`
Expected: PASS

- [ ] **Step 6: Add oneOf/anyOf tests**

```rust
#[test]
fn one_of_validations_evaluated() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"type": "integer"}},
        "oneOf": [{
            "x-kubernetes-validations": [
                {"rule": "self.x != 0", "message": "x must not be zero"}
            ]
        }]
    });
    let obj = json!({"x": 0});
    let errors = validate(&schema, &obj, None);
    assert_eq!(errors.len(), 1);
}

#[test]
fn nested_all_of_properties_walked() {
    let schema = json!({
        "type": "object",
        "allOf": [{
            "properties": {
                "name": {
                    "type": "string",
                    "x-kubernetes-validations": [
                        {"rule": "self.size() > 0", "message": "name required"}
                    ]
                }
            }
        }]
    });
    let obj = json!({"name": ""});
    let errors = validate(&schema, &obj, None);
    assert_eq!(errors.len(), 1);
}
```

- [ ] **Step 7: Run full test suite and commit**

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/compilation.rs src/validation.rs
git commit -m "feat: walk allOf/oneOf/anyOf branches for x-kubernetes-validations"
```

---

### Task 3: Root-level variables (`apiVersion`, `apiGroup`, `kind`)

**Files:**
- Modify: `src/validation.rs` (new `RootContext` struct, new methods, binding logic)
- Test: `src/validation.rs` (inline tests)

- [ ] **Step 1: Write failing test for root-level variable binding**

```rust
#[test]
fn root_context_variables_bound() {
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "x-kubernetes-validations": [{
            "rule": "apiVersion == 'apps/v1'",
            "message": "wrong api version"
        }]
    });
    let obj = json!({"name": "test"});
    let root_ctx = RootContext {
        api_version: "apps/v1".into(),
        api_group: "apps".into(),
        kind: "Deployment".into(),
    };
    let errors = Validator::new().validate_with_context(&schema, &obj, None, Some(&root_ctx));
    assert!(errors.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation root_context -v`
Expected: FAIL — `RootContext` does not exist

- [ ] **Step 3: Implement RootContext and validate_with_context**

In `src/validation.rs`, add the struct:

```rust
/// CRD-level context variables available at the root schema node.
///
/// These are derived from the CRD definition, not from the object being validated.
/// Available as root-level CEL variables: `apiVersion`, `apiGroup`, `kind`.
#[derive(Clone, Debug, Default)]
pub struct RootContext {
    /// CRD API version (e.g., `"apps/v1"`).
    pub api_version: String,
    /// CRD API group (e.g., `"apps"`). Empty string for core resources.
    pub api_group: String,
    /// CRD kind (e.g., `"Deployment"`).
    pub kind: String,
}
```

Add new methods to `Validator`:

```rust
/// Validate with CRD-level root context variables.
#[must_use]
pub fn validate_with_context(
    &self,
    schema: &serde_json::Value,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
    root_ctx: Option<&RootContext>,
) -> Vec<ValidationError> {
    let mut base_ctx = Context::default();
    crate::register_all(&mut base_ctx);
    let mut errors = Vec::new();
    self.walk_schema_inner(schema, object, old_object, String::new(), &mut errors, &base_ctx, root_ctx);
    errors
}
```

Thread `root_ctx: Option<&RootContext>` through `walk_schema`/`walk_compiled` and `evaluate_compiled_results`. Bind root variables when `path.is_empty()`:

```rust
// Inside evaluate_compiled_results, after binding self/oldSelf:
if path.is_empty() {
    if let Some(rc) = root_ctx {
        node_ctx.add_variable_from_value("apiVersion",
            cel::Value::String(std::sync::Arc::new(rc.api_version.clone())));
        node_ctx.add_variable_from_value("apiGroup",
            cel::Value::String(std::sync::Arc::new(rc.api_group.clone())));
        node_ctx.add_variable_from_value("kind",
            cel::Value::String(std::sync::Arc::new(rc.kind.clone())));
    }
}
```

The existing `validate()` and `validate_compiled()` delegate to the new inner methods with `root_ctx: None`, preserving backward compatibility.

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation root_context -v`
Expected: PASS

- [ ] **Step 5: Add edge case tests**

> **Implementation note:** The test `root_context_not_available_in_nested_rules` expects
> `EvaluationError` when `apiVersion` is referenced in a nested rule where it's not bound.
> Verify during implementation that the `cel` crate returns an error (not `Null`) for
> unbound top-level identifiers. If it returns `Null`, adjust the assertion to check for
> `ErrorKind::InvalidResult` or `ErrorKind::ValidationFailure` instead.

```rust
#[test]
fn root_context_empty_api_group_for_core() {
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "x-kubernetes-validations": [{
            "rule": "apiGroup == ''",
            "message": "not core"
        }]
    });
    let obj = json!({"name": "test"});
    let root_ctx = RootContext {
        api_version: "v1".into(),
        api_group: "".into(),
        kind: "Pod".into(),
    };
    let errors = Validator::new().validate_with_context(&schema, &obj, None, Some(&root_ctx));
    assert!(errors.is_empty());
}

#[test]
fn root_context_not_available_in_nested_rules() {
    // apiVersion should only be available at root, not in nested schema nodes
    let schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {"x": {"type": "integer"}},
                "x-kubernetes-validations": [{
                    "rule": "apiVersion == 'v1'",
                    "message": "should error"
                }]
            }
        }
    });
    let obj = json!({"spec": {"x": 1}});
    let root_ctx = RootContext {
        api_version: "v1".into(),
        api_group: "".into(),
        kind: "Pod".into(),
    };
    let errors = Validator::new().validate_with_context(&schema, &obj, None, Some(&root_ctx));
    // Should produce EvaluationError since apiVersion is not bound at nested level
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind, ErrorKind::EvaluationError);
}

#[test]
fn validate_without_root_context_still_works() {
    let schema = json!({
        "type": "object",
        "properties": {"x": {"type": "integer"}},
        "x-kubernetes-validations": [{"rule": "self.x >= 0", "message": "bad"}]
    });
    let obj = json!({"x": -1});
    let errors = validate(&schema, &obj, None);
    assert_eq!(errors.len(), 1);
}
```

- [ ] **Step 6: Run full test suite and commit**

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/validation.rs
git commit -m "feat: add RootContext for apiVersion/apiGroup/kind root-level variables"
```

---

> **Follow-up:** Also add `validate_compiled_with_context` for users who pre-compile
> schemas and also need root context variables. Same pattern as `validate_with_context`
> but taking `&CompiledSchema` instead of `&serde_json::Value`.

---

## Chunk 2: Phase 1 — Schema-Aware Validation (0.5.1)

### Task 4: `x-kubernetes-preserve-unknown-fields` flag

**Files:**
- Modify: `src/compilation.rs:140-198`
- Modify: `src/validation.rs` (walker guards)
- Test: inline tests

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn preserve_unknown_fields_skips_additional_properties_walk() {
    // When preserve-unknown-fields is true AND additionalProperties has rules,
    // unknown fields should NOT be walked through additionalProperties rules
    let schema = json!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true,
        "additionalProperties": {
            "type": "integer",
            "x-kubernetes-validations": [
                {"rule": "self >= 0", "message": "must be non-negative"}
            ]
        }
    });
    let obj = json!({"unknown_field": -1});
    let errors = validate(&schema, &obj, None);
    // With preserve-unknown-fields, additionalProperties rules should be skipped
    assert!(errors.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation preserve_unknown -v`
Expected: FAIL — 1 error (additionalProperties rule fires)

- [ ] **Step 3: Add flag to CompiledSchema and guard walkers**

In `src/compilation.rs`, add to `CompiledSchema`:

```rust
/// Whether `x-kubernetes-preserve-unknown-fields` is set on this node.
pub preserve_unknown_fields: bool,
```

In `compile_schema`, populate:

```rust
let preserve_unknown_fields = schema
    .get("x-kubernetes-preserve-unknown-fields")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
```

In `src/validation.rs`, guard the additionalProperties block in `walk_schema`:

```rust
// Only walk additionalProperties if preserve-unknown-fields is NOT set
let preserve = schema
    .get("x-kubernetes-preserve-unknown-fields")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
if !preserve {
    if let (Some(additional_schema), Some(obj)) = (/* ... */) {
        // existing additionalProperties walking
    }
}
```

Similarly in `walk_compiled`, guard with `!compiled.preserve_unknown_fields`.

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation preserve_unknown -v && cargo test --features validation -v`
Expected: All PASS

- [ ] **Step 5: Commit**

```bash
git add src/compilation.rs src/validation.rs
git commit -m "feat: respect x-kubernetes-preserve-unknown-fields in schema walking"
```

---

### Task 5: `x-kubernetes-embedded-resource` support

**Files:**
- Modify: `src/compilation.rs` (flag on CompiledSchema)
- Modify: `src/values.rs` (inject apiVersion/kind/metadata defaults)
- Test: inline tests in values.rs and validation.rs

- [ ] **Step 1: Write failing test**

```rust
// In src/validation.rs mod tests
#[test]
fn embedded_resource_fields_accessible() {
    let schema = json!({
        "type": "object",
        "x-kubernetes-embedded-resource": true,
        "properties": {
            "spec": {"type": "object"}
        },
        "x-kubernetes-validations": [{
            "rule": "self.apiVersion.size() >= 0",
            "message": "apiVersion must exist"
        }]
    });
    // Object without apiVersion/kind/metadata
    let obj = json!({"spec": {}});
    let errors = validate(&schema, &obj, None);
    // Should pass because embedded-resource injects apiVersion as empty string
    assert!(errors.is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation embedded_resource -v`
Expected: FAIL — EvaluationError (key not found)

- [ ] **Step 3: Implement embedded-resource field injection**

In `src/compilation.rs`, add to `CompiledSchema`:

```rust
pub embedded_resource: bool,
```

Populate in `compile_schema`:

```rust
let embedded_resource = schema
    .get("x-kubernetes-embedded-resource")
    .and_then(|v| v.as_bool())
    .unwrap_or(false);
```

In `src/values.rs`, in `json_to_cel_with_schema` Object arm, after building the map:

```rust
serde_json::Value::Object(obj) => {
    // ... existing map building ...

    // Inject embedded resource fields if absent
    if schema.get("x-kubernetes-embedded-resource").and_then(|v| v.as_bool()) == Some(true) {
        for field in &["apiVersion", "kind"] {
            let key = Key::String(Arc::new(field.to_string()));
            map.entry(key).or_insert_with(|| Value::String(Arc::new(String::new())));
        }
        let meta_key = Key::String(Arc::new("metadata".to_string()));
        map.entry(meta_key).or_insert_with(|| {
            Value::Map(Map { map: Arc::new(HashMap::new()) })
        });
    }

    Value::Map(Map { map: Arc::new(map) })
}
```

Similarly in `json_to_cel_with_compiled`, in the Object arm, after building the map:

```rust
serde_json::Value::Object(obj) => {
    // ... existing map building ...

    // Inject embedded resource fields if absent
    if compiled.embedded_resource {
        for field in &["apiVersion", "kind"] {
            let key = Key::String(Arc::new(field.to_string()));
            map.entry(key).or_insert_with(|| Value::String(Arc::new(String::new())));
        }
        let meta_key = Key::String(Arc::new("metadata".to_string()));
        map.entry(meta_key).or_insert_with(|| {
            Value::Map(Map { map: Arc::new(HashMap::new()) })
        });
    }

    Value::Map(Map { map: Arc::new(map) })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation embedded_resource -v`
Expected: PASS

- [ ] **Step 5: Add test for existing fields not overwritten**

```rust
#[test]
fn embedded_resource_preserves_existing_fields() {
    let schema = json!({
        "type": "object",
        "x-kubernetes-embedded-resource": true,
        "x-kubernetes-validations": [{
            "rule": "self.apiVersion == 'v1'",
            "message": "wrong version"
        }]
    });
    let obj = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "test"}});
    let errors = validate(&schema, &obj, None);
    assert!(errors.is_empty());
}
```

- [ ] **Step 6: Run full tests and commit**

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/compilation.rs src/values.rs
git commit -m "feat: inject apiVersion/kind/metadata for x-kubernetes-embedded-resource"
```

---

### Task 6: Default value injection

**Files:**
- Create: `src/defaults.rs`
- Modify: `src/lib.rs` (module declaration)
- Modify: `src/validation.rs` (call apply_defaults before walking)
- Test: `src/defaults.rs` (inline tests)

- [ ] **Step 1: Write failing test**

```rust
// In src/defaults.rs
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation apply_defaults -v`
Expected: FAIL — module does not exist

- [ ] **Step 3: Implement apply_defaults**

Create `src/defaults.rs`:

> **Known limitation:** `apply_defaults` only looks at top-level `properties` for defaults.
> It does not walk into `allOf`/`oneOf`/`anyOf` branches to find additional property defaults.
> This matches the common case but may miss defaults defined in composition branches.
> Enhancement tracked as a follow-up to Task 2.

```rust
//! Schema default value injection.
//!
//! Recursively applies `default` values from an OpenAPI schema to a JSON value,
//! filling in missing fields. This matches the Kubernetes API server behavior
//! where defaults are applied before CEL validation rules execute.

/// Apply schema `default` values to a JSON value, returning a new value with
/// missing fields filled in.
///
/// This is a recursive pre-processing pass. It does not modify the input.
#[must_use]
pub fn apply_defaults(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(obj) => {
            let mut result = obj.clone();

            if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
                for (key, prop_schema) in props {
                    match result.get(key) {
                        Some(child) => {
                            // Recursively apply defaults to existing children
                            let defaulted = apply_defaults(prop_schema, child);
                            result.insert(key.clone(), defaulted);
                        }
                        None => {
                            // Missing field: inject default if present
                            if let Some(default_val) = prop_schema.get("default") {
                                result.insert(key.clone(), default_val.clone());
                            }
                        }
                    }
                }
            }

            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            if let Some(items_schema) = schema.get("items") {
                let items: Vec<_> = arr.iter()
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
```

Add to `src/lib.rs`:

```rust
#[cfg(feature = "validation")] pub mod defaults;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation apply_defaults -v`
Expected: PASS

- [ ] **Step 5: Add more tests**

```rust
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
```

- [ ] **Step 6: Run tests and commit**

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/defaults.rs src/lib.rs
git commit -m "feat: add apply_defaults for schema default value injection"
```

- [ ] **Step 7: Integrate into Validator (optional convenience)**

Add `validate_with_defaults` method to `Validator` that calls `apply_defaults` before walking. This is an additive API — existing `validate()` behavior is unchanged.

```rust
/// Validate with schema defaults applied to the object first.
///
/// Equivalent to calling [`defaults::apply_defaults`] followed by [`validate`].
#[must_use]
pub fn validate_with_defaults(
    &self,
    schema: &serde_json::Value,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
) -> Vec<ValidationError> {
    let defaulted = crate::defaults::apply_defaults(schema, object);
    let defaulted_old = old_object.map(|o| crate::defaults::apply_defaults(schema, o));
    self.validate(schema, &defaulted, defaulted_old.as_ref())
}
```

- [ ] **Step 8: Test integration and commit**

```rust
#[test]
fn validate_with_defaults_fills_missing_then_validates() {
    let schema = json!({
        "type": "object",
        "properties": {
            "replicas": {
                "type": "integer",
                "default": 1,
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        }
    });
    // Without defaults, replicas is missing → no validation runs
    let errors = validate(&schema, &json!({}), None);
    assert!(errors.is_empty());

    // With defaults, replicas=1 is injected → validation runs and passes
    let errors = Validator::new().validate_with_defaults(&schema, &json!({}), None);
    assert!(errors.is_empty());
}
```

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/validation.rs src/defaults.rs
git commit -m "feat: add validate_with_defaults convenience method"
```

---

## Chunk 3: Phase 2 — Static Analysis (0.5.2)

### Task 7: Variable scope validation

**Files:**
- Create: `src/analysis.rs`
- Modify: `src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Write failing test**

```rust
// In src/analysis.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_wrong_scope_variable() {
        let warnings = check_rule_scope("request.userInfo.username == 'admin'", ScopeContext::CrdValidation);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("request"));
    }

    #[test]
    fn self_and_old_self_are_valid() {
        let warnings = check_rule_scope("self.replicas >= oldSelf.replicas", ScopeContext::CrdValidation);
        assert!(warnings.is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --features validation detect_wrong_scope -v`
Expected: FAIL — module does not exist

- [ ] **Step 3: Implement scope validation**

Create `src/analysis.rs`:

```rust
//! Static analysis for CEL validation rules.
//!
//! Provides compile-time checks that go beyond syntax validation:
//! variable scope validation and cost estimation.

use cel::Program;

/// The context in which a CEL rule is evaluated, determining which variables are in scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeContext {
    /// CRD `x-kubernetes-validations` — only `self`, `oldSelf` (and root vars at root level).
    CrdValidation,
    /// ValidatingAdmissionPolicy — `object`, `oldObject`, `request`, `params`, etc.
    AdmissionPolicy,
}

/// A warning produced by static analysis.
#[derive(Clone, Debug)]
pub struct AnalysisWarning {
    /// The CEL expression that triggered the warning.
    pub rule: String,
    /// Human-readable warning message.
    pub message: String,
    /// Warning category.
    pub kind: WarningKind,
}

/// Categories of static analysis warnings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// Variable referenced that is not available in the given scope.
    WrongScope,
    /// Estimated cost may exceed the K8s budget.
    CostExceeded,
}

/// Valid root-level variables for each scope context.
fn valid_variables(scope: ScopeContext) -> &'static [&'static str] {
    match scope {
        ScopeContext::CrdValidation => &["self", "oldSelf", "apiVersion", "apiGroup", "kind"],
        ScopeContext::AdmissionPolicy => &[
            "self", "oldSelf", "object", "oldObject", "request",
            "params", "namespaceObject", "authorizer", "variables",
        ],
    }
}

/// Check a CEL expression for variable scope violations.
///
/// Returns warnings for any variables referenced that are not valid in the given scope.
#[must_use]
pub fn check_rule_scope(rule: &str, scope: ScopeContext) -> Vec<AnalysisWarning> {
    let program = match Program::compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![], // syntax errors are caught by compilation
    };

    let valid = valid_variables(scope);
    let mut warnings = Vec::new();

    for var in program.references().variables() {
        if !valid.contains(&var) {
            warnings.push(AnalysisWarning {
                rule: rule.to_string(),
                message: format!(
                    "variable '{var}' is not available in {scope:?} context; valid variables: {valid:?}"
                ),
                kind: WarningKind::WrongScope,
            });
        }
    }

    warnings
}
```

Add to `src/lib.rs`:

```rust
#[cfg(feature = "validation")] pub mod analysis;
```

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation detect_wrong_scope -v && cargo test --features validation self_and_old -v`
Expected: PASS

- [ ] **Step 5: Add edge case tests**

```rust
#[test]
fn admission_policy_scope_allows_request() {
    let warnings = check_rule_scope("request.userInfo.username == 'admin'", ScopeContext::AdmissionPolicy);
    assert!(warnings.is_empty());
}

#[test]
fn crd_scope_rejects_object_variable() {
    let warnings = check_rule_scope("object.metadata.name == 'test'", ScopeContext::CrdValidation);
    assert_eq!(warnings.len(), 1);
}

#[test]
fn invalid_syntax_returns_empty() {
    let warnings = check_rule_scope("self.x >=", ScopeContext::CrdValidation);
    assert!(warnings.is_empty());
}
```

- [ ] **Step 6: Run tests and commit**

Run: `cargo test --features validation -v`
Expected: All PASS

```bash
git add src/analysis.rs src/lib.rs
git commit -m "feat: add variable scope validation for CRD and admission policy contexts"
```

---

### Task 8: Cost estimation

**Files:**
- Modify: `src/compilation.rs` (add maxLength/maxItems to CompiledSchema)
- Modify: `src/analysis.rs` (add cost estimation)
- Test: inline tests

- [ ] **Step 1: Add schema bound fields to CompiledSchema**

In `src/compilation.rs`, add to `CompiledSchema`:

```rust
/// `maxLength` from the schema (used for cost estimation).
pub max_length: Option<u64>,
/// `maxItems` from the schema (used for cost estimation).
pub max_items: Option<u64>,
/// `maxProperties` from the schema (used for cost estimation).
pub max_properties: Option<u64>,
```

Populate in `compile_schema`:

```rust
let max_length = schema.get("maxLength").and_then(|v| v.as_u64());
let max_items = schema.get("maxItems").and_then(|v| v.as_u64());
let max_properties = schema.get("maxProperties").and_then(|v| v.as_u64());
```

- [ ] **Step 2: Write failing test for cost estimation**

```rust
// In src/analysis.rs
#[test]
fn unbounded_list_comprehension_warns() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {"type": "string"}
                // No maxItems!
            }
        }
    });
    let warnings = estimate_rule_cost(
        "self.items.all(item, item.size() > 0)",
        &compile_schema(&schema),
    );
    assert!(warnings.iter().any(|w| w.kind == WarningKind::CostExceeded));
}

#[test]
fn bounded_list_no_warning() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "maxItems": 10,
                "items": {"type": "string", "maxLength": 64}
            }
        }
    });
    let warnings = estimate_rule_cost(
        "self.items.all(item, item.size() > 0)",
        &compile_schema(&schema),
    );
    assert!(warnings.is_empty());
}
```

- [ ] **Step 3: Implement cost estimation**

In `src/analysis.rs`, add cost estimation using AST walking:

```rust
use cel::common::ast::Expr;
use crate::compilation::CompiledSchema;

/// Default assumed maximum for unbounded lists (when maxItems is not set).
const DEFAULT_MAX_ITEMS: u64 = 1000;
/// Default assumed maximum for unbounded strings (when maxLength is not set).
const DEFAULT_MAX_LENGTH: u64 = 1000;
/// K8s cost budget per expression.
const K8S_COST_BUDGET: u64 = 1_000_000;
/// Cost multiplier for string traversal operations.
const STRING_TRAVERSAL_FACTOR: f64 = 0.1;

/// Estimate the cost of a CEL rule and warn if it may exceed K8s budget.
///
/// **This is a coarse heuristic, not an accurate cost model.** It does not resolve
/// which sub-schema corresponds to which sub-expression (e.g., it cannot tell which
/// array field a comprehension iterates over when multiple arrays exist). It scans
/// all properties for array/string bounds and uses the first match.
///
/// Despite this limitation, the heuristic catches the most common issue: unbounded
/// list comprehensions without `maxItems`, which is the #1 cause of K8s cost rejections.
/// A future version could walk the AST with schema-context threading for precision.
#[must_use]
pub fn estimate_rule_cost(rule: &str, schema: &CompiledSchema) -> Vec<AnalysisWarning> {
    let program = match Program::compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let expr = program.expression();
    let mut warnings = Vec::new();
    let cost = estimate_expr_cost(&expr.expr, schema);

    if cost > K8S_COST_BUDGET {
        warnings.push(AnalysisWarning {
            rule: rule.to_string(),
            message: format!(
                "estimated cost {cost} exceeds K8s budget {K8S_COST_BUDGET}; \
                 consider adding maxItems/maxLength to schema bounds"
            ),
            kind: WarningKind::CostExceeded,
        });
    }

    // Also warn about missing bounds that inflate the estimate
    check_missing_bounds(&expr.expr, schema, rule, &mut warnings);

    warnings
}

fn estimate_expr_cost(expr: &Expr, schema: &CompiledSchema) -> u64 {
    match expr {
        Expr::Comprehension(comp) => {
            let list_size = schema_list_size(schema);
            let body_cost = estimate_expr_cost(&comp.loop_step.expr, schema);
            list_size * body_cost.max(1)
        }
        Expr::Call(call) => {
            let base = 1u64;
            let arg_cost: u64 = call.args.iter()
                .map(|a| estimate_expr_cost(&a.expr, schema))
                .sum();
            if is_string_traversal(&call.func_name) {
                let str_len = schema_string_length(schema);
                base + (str_len as f64 * STRING_TRAVERSAL_FACTOR) as u64 + arg_cost
            } else {
                base + arg_cost
            }
        }
        Expr::Select(sel) => {
            1 + estimate_expr_cost(&sel.operand.expr, schema)
        }
        Expr::List(list) => {
            list.elements.iter()
                .map(|e| estimate_expr_cost(&e.expr, schema))
                .sum::<u64>()
                .max(1)
        }
        _ => 1,
    }
}

fn schema_list_size(schema: &CompiledSchema) -> u64 {
    // Check properties for array fields
    for prop in schema.properties.values() {
        if prop.items.is_some() {
            return prop.max_items.unwrap_or(DEFAULT_MAX_ITEMS);
        }
    }
    schema.max_items.unwrap_or(DEFAULT_MAX_ITEMS)
}

fn schema_string_length(schema: &CompiledSchema) -> u64 {
    schema.max_length.unwrap_or(DEFAULT_MAX_LENGTH)
}

fn is_string_traversal(func: &str) -> bool {
    matches!(func, "contains" | "startsWith" | "endsWith" | "matches"
        | "find" | "findAll" | "replace" | "split" | "indexOf" | "lastIndexOf")
}

fn check_missing_bounds(expr: &Expr, schema: &CompiledSchema, rule: &str, warnings: &mut Vec<AnalysisWarning>) {
    if let Expr::Comprehension(_) = expr {
        for prop in schema.properties.values() {
            if prop.items.is_some() && prop.max_items.is_none() {
                warnings.push(AnalysisWarning {
                    rule: rule.to_string(),
                    message: "list field has no maxItems bound; cost estimate uses worst-case default".into(),
                    kind: WarningKind::CostExceeded,
                });
                break;
            }
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --features validation cost -v`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/analysis.rs src/compilation.rs
git commit -m "feat: add CEL rule cost estimation with schema-bound awareness"
```

---

## Chunk 4: Phase 3 — ValidatingAdmissionPolicy (0.5.3)

### Task 9: VAP evaluator module

**Files:**
- Create: `src/vap.rs`
- Modify: `src/lib.rs`
- Test: inline tests

- [ ] **Step 1: Define VAP types and builder**

Create `src/vap.rs`:

```rust
//! Client-side evaluation of Kubernetes ValidatingAdmissionPolicy CEL expressions.
//!
//! Supports all VAP variables except `authorizer` (requires API server).

use std::sync::Arc;
use cel::{Context, Program, Value};
use crate::values::json_to_cel;

/// A request context for VAP evaluation.
#[derive(Clone, Debug, Default)]
pub struct AdmissionRequest {
    /// The operation being performed: "CREATE", "UPDATE", "DELETE", "CONNECT".
    pub operation: String,
    /// The requesting user's name.
    pub username: String,
    /// The requesting user's UID.
    pub uid: String,
    /// The requesting user's groups.
    pub groups: Vec<String>,
    /// The resource name.
    pub name: String,
    /// The resource namespace.
    pub namespace: String,
    /// Whether this is a dry run.
    pub dry_run: bool,
    /// The resource GVK.
    pub kind: GroupVersionKind,
    /// The resource GVR.
    pub resource: GroupVersionResource,
}

#[derive(Clone, Debug, Default)]
pub struct GroupVersionKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

#[derive(Clone, Debug, Default)]
pub struct GroupVersionResource {
    pub group: String,
    pub version: String,
    pub resource: String,
}

/// Builder for constructing a VAP evaluation context.
pub struct VapEvaluator {
    object: serde_json::Value,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
    params: Option<serde_json::Value>,
    namespace_object: Option<serde_json::Value>,
}

/// Result of evaluating a single VAP validation expression.
#[derive(Clone, Debug)]
pub struct VapResult {
    /// The CEL expression that was evaluated.
    pub expression: String,
    /// Whether the validation passed.
    pub passed: bool,
    /// Error message if validation failed.
    pub message: Option<String>,
}

impl VapEvaluator {
    pub fn builder() -> VapEvaluatorBuilder {
        VapEvaluatorBuilder::default()
    }

    /// Evaluate a list of validation expressions.
    ///
    /// Note: expressions are compiled on each call. For repeated evaluation,
    /// a future `CompiledVapEvaluator` could pre-compile expressions.
    #[must_use]
    pub fn evaluate(&self, expressions: &[VapExpression]) -> Vec<VapResult> {
        let mut ctx = Context::default();
        crate::register_all(&mut ctx);

        // Bind variables
        ctx.add_variable_from_value("object", json_to_cel(&self.object));
        if let Some(ref old) = self.old_object {
            ctx.add_variable_from_value("oldObject", json_to_cel(old));
        } else {
            ctx.add_variable_from_value("oldObject", Value::Null);
        }
        ctx.add_variable_from_value("request", self.request_to_cel());
        if let Some(ref params) = self.params {
            ctx.add_variable_from_value("params", json_to_cel(params));
        }
        if let Some(ref ns) = self.namespace_object {
            ctx.add_variable_from_value("namespaceObject", json_to_cel(ns));
        }

        expressions.iter().map(|expr| {
            match Program::compile(&expr.expression) {
                Ok(program) => match program.execute(&ctx) {
                    Ok(Value::Bool(true)) => VapResult {
                        expression: expr.expression.clone(),
                        passed: true,
                        message: None,
                    },
                    Ok(Value::Bool(false)) => {
                        let msg = expr.message_expression.as_deref()
                            .and_then(|me| Program::compile(me).ok())
                            .and_then(|prog| match prog.execute(&ctx) {
                                Ok(Value::String(s)) => Some((*s).clone()),
                                _ => None,
                            })
                            .or_else(|| expr.message.clone())
                            .unwrap_or_else(|| format!("failed: {}", expr.expression));
                        VapResult {
                            expression: expr.expression.clone(),
                            passed: false,
                            message: Some(msg),
                        }
                    }
                    Ok(_) => VapResult {
                        expression: expr.expression.clone(),
                        passed: false,
                        message: Some("expression did not evaluate to bool".into()),
                    },
                    Err(e) => VapResult {
                        expression: expr.expression.clone(),
                        passed: false,
                        message: Some(format!("evaluation error: {e}")),
                    },
                },
                Err(e) => VapResult {
                    expression: expr.expression.clone(),
                    passed: false,
                    message: Some(format!("compilation error: {e}")),
                },
            }
        }).collect()
    }

    fn request_to_cel(&self) -> Value {
        use std::collections::HashMap;
        use cel::objects::{Key, Map};

        let r = &self.request;
        let mut map = HashMap::new();

        let gvk = |g: &GroupVersionKind| -> Value {
            let mut m = HashMap::new();
            m.insert(Key::String(Arc::new("group".into())), Value::String(Arc::new(g.group.clone())));
            m.insert(Key::String(Arc::new("version".into())), Value::String(Arc::new(g.version.clone())));
            m.insert(Key::String(Arc::new("kind".into())), Value::String(Arc::new(g.kind.clone())));
            Value::Map(Map { map: Arc::new(m) })
        };

        let gvr = |g: &GroupVersionResource| -> Value {
            let mut m = HashMap::new();
            m.insert(Key::String(Arc::new("group".into())), Value::String(Arc::new(g.group.clone())));
            m.insert(Key::String(Arc::new("version".into())), Value::String(Arc::new(g.version.clone())));
            m.insert(Key::String(Arc::new("resource".into())), Value::String(Arc::new(g.resource.clone())));
            Value::Map(Map { map: Arc::new(m) })
        };

        map.insert(Key::String(Arc::new("kind".into())), gvk(&r.kind));
        map.insert(Key::String(Arc::new("resource".into())), gvr(&r.resource));
        map.insert(Key::String(Arc::new("name".into())), Value::String(Arc::new(r.name.clone())));
        map.insert(Key::String(Arc::new("namespace".into())), Value::String(Arc::new(r.namespace.clone())));
        map.insert(Key::String(Arc::new("operation".into())), Value::String(Arc::new(r.operation.clone())));
        map.insert(Key::String(Arc::new("dryRun".into())), Value::Bool(r.dry_run));

        // userInfo
        let mut user_map = HashMap::new();
        user_map.insert(Key::String(Arc::new("username".into())), Value::String(Arc::new(r.username.clone())));
        user_map.insert(Key::String(Arc::new("uid".into())), Value::String(Arc::new(r.uid.clone())));
        user_map.insert(Key::String(Arc::new("groups".into())), Value::List(Arc::new(
            r.groups.iter().map(|g| Value::String(Arc::new(g.clone()))).collect()
        )));
        map.insert(Key::String(Arc::new("userInfo".into())), Value::Map(Map { map: Arc::new(user_map) }));

        Value::Map(Map { map: Arc::new(map) })
    }
}

/// A VAP validation expression.
#[derive(Clone, Debug)]
pub struct VapExpression {
    pub expression: String,
    pub message: Option<String>,
    pub message_expression: Option<String>,
}

/// Builder for VapEvaluator.
#[derive(Default)]
pub struct VapEvaluatorBuilder {
    object: Option<serde_json::Value>,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
    params: Option<serde_json::Value>,
    namespace_object: Option<serde_json::Value>,
}

impl VapEvaluatorBuilder {
    pub fn object(mut self, obj: serde_json::Value) -> Self { self.object = Some(obj); self }
    pub fn old_object(mut self, obj: serde_json::Value) -> Self { self.old_object = Some(obj); self }
    pub fn request(mut self, req: AdmissionRequest) -> Self { self.request = req; self }
    pub fn params(mut self, p: serde_json::Value) -> Self { self.params = Some(p); self }
    pub fn namespace_object(mut self, ns: serde_json::Value) -> Self { self.namespace_object = Some(ns); self }

    pub fn build(self) -> VapEvaluator {
        VapEvaluator {
            object: self.object.unwrap_or(serde_json::Value::Null),
            old_object: self.old_object,
            request: self.request,
            params: self.params,
            namespace_object: self.namespace_object,
        }
    }
}
```

- [ ] **Step 2: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vap_basic_validation_passes() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"metadata": {"name": "test"}, "spec": {"replicas": 3}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();

        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("replicas must be non-negative".into()),
            message_expression: None,
        }]);

        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    #[test]
    fn vap_validation_fails_with_message() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": -1}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();

        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("replicas must be non-negative".into()),
            message_expression: None,
        }]);

        assert!(!results[0].passed);
        assert_eq!(results[0].message.as_deref(), Some("replicas must be non-negative"));
    }

    #[test]
    fn vap_request_variables_accessible() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                username: "admin".into(),
                namespace: "default".into(),
                ..Default::default()
            })
            .build();

        let results = evaluator.evaluate(&[VapExpression {
            expression: "request.operation == 'CREATE' && request.userInfo.username == 'admin'".into(),
            message: None,
            message_expression: None,
        }]);

        assert!(results[0].passed);
    }

    #[test]
    fn vap_old_object_null_on_create() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();

        let results = evaluator.evaluate(&[VapExpression {
            expression: "oldObject == null".into(),
            message: None,
            message_expression: None,
        }]);

        assert!(results[0].passed);
    }

    #[test]
    fn vap_params_accessible() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": 5}}))
            .params(json!({"maxReplicas": 10}))
            .request(AdmissionRequest::default())
            .build();

        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas <= params.maxReplicas".into(),
            message: None,
            message_expression: None,
        }]);

        assert!(results[0].passed);
    }
}
```

- [ ] **Step 3: Add module to lib.rs and run tests**

```rust
#[cfg(feature = "validation")] pub mod vap;
```

Run: `cargo test --features validation vap -v`
Expected: All PASS

- [ ] **Step 4: Commit**

```bash
git add src/vap.rs src/lib.rs
git commit -m "feat: add ValidatingAdmissionPolicy client-side evaluator"
```

---

## Blocked Items (tracked, not planned)

These require upstream `cel` crate changes and cannot be implemented now:

| Item | Blocker | Tracking |
|---|---|---|
| `cel.bind(var, init, expr)` | cel crate macro system | Monitor cel-rust/cel-rust |
| `<list>.sortBy(var, expr)` | cel crate macro/lambda | Same |
| TwoVarComprehensions | cel crate compiler | K8s 1.33+ |
| Full type inference | cel crate has no TypeProvider API | Phase 2 partial workaround via AST |

---

## Release Plan

| Version | Content |
|---|---|
| **0.5.1** | All phases (0-3): IntOrString, allOf/oneOf/anyOf, RootContext, preserve-unknown-fields, embedded-resource, defaults, scope validation, cost estimation, VAP evaluator |

Single patch bump — every change is additive, no existing public API is modified.
Pre-1.0 crate, so `#[non_exhaustive]` on `SchemaFormat` is acceptable in a patch.
