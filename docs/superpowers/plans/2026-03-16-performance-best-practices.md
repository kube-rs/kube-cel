# kube-cel 0.5.2 Performance & Best Practices Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate redundant allocations and re-computations in the validation hot path, apply Rust best practices across all 0.5.1 modules.

**Architecture:** Move one-time work (context creation, function registration, expression compilation) out of per-call paths into construction/init. Fix unnecessary clones. Apply `#[inline]` to hot helpers. Unify analysis API. All changes are internal — no public API signatures change (only `Validator` struct internals and `apply_defaults` return type stay the same).

**Tech Stack:** Rust, `cel` crate 0.13, `serde_json`, `serde::Deserialize`

---

## File Structure

**Modified files:**
- `src/validation.rs` — `Validator` holds pre-built `Context`, `thread_local!` for convenience functions
- `src/vap.rs` — pre-compile expressions, move context creation to `evaluate` preamble once
- `src/compilation.rs` — `Rule::deserialize(raw)` instead of `raw.clone()`
- `src/analysis.rs` — `analyze_rule` combined function
- `src/values.rs` — hoist `schema.get("items")` out of loop
- `src/defaults.rs` — clone-on-write optimization

---

## Chunk 1: Validator Context Caching

### Task 1: Move `register_all` into `Validator::new()`

**Files:**
- Modify: `src/validation.rs:80-89` (Validator struct + new)
- Modify: `src/validation.rs:96-116` (validate_with_context)
- Modify: `src/validation.rs:137-157` (validate_compiled_with_context)
- Modify: `src/validation.rs:163-172` (validate_with_defaults)
- Test: `src/validation.rs` (existing tests)

- [ ] **Step 1: Run existing tests to establish baseline**

Run: `cargo test --all-features`
Expected: All 476 tests pass

- [ ] **Step 2: Change Validator struct to hold Context**

Replace the ZST with a context-holding struct:

```rust
pub struct Validator {
    base_ctx: Context<'static>,
}

impl Validator {
    /// Create a new `Validator` with all K8s CEL functions pre-registered.
    ///
    /// The context is built once and reused across all validation calls.
    pub fn new() -> Self {
        let mut ctx = Context::default();
        crate::register_all(&mut ctx);
        Self { base_ctx: ctx }
    }
}
```

Remove `#[derive(Clone, Debug)]` from Validator (Context doesn't impl Clone/Debug).
Add manual `Debug`:

```rust
impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator").finish()
    }
}
```

Update the doc comment — remove "Validator is Send + Sync" claim (Context is Send but may not be Sync). Add instead: "Create one `Validator` per thread, or wrap in `Arc<Mutex<Validator>>`."

- [ ] **Step 3: Update validate methods to use `self.base_ctx`**

In `validate_with_context`:
```rust
// REMOVE:
// let mut base_ctx = Context::default();
// crate::register_all(&mut base_ctx);

// USE:
self.walk_schema(schema, object, old_object, String::new(), &mut errors, &self.base_ctx, root_ctx);
```

Same for `validate_compiled_with_context` and `validate_with_defaults`.

- [ ] **Step 4: Update convenience functions to use `thread_local!`**

The free functions at the bottom of validation.rs (`validate()`, `validate_compiled()`) currently create `Validator::new()` per call. Cache with `thread_local!`:

```rust
/// Convenience function to validate without creating a [`Validator`] instance.
#[must_use]
pub fn validate(
    schema: &serde_json::Value,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
) -> Vec<ValidationError> {
    thread_local! {
        static VALIDATOR: Validator = Validator::new();
    }
    VALIDATOR.with(|v| v.validate(schema, object, old_object))
}

/// Convenience function to validate using a pre-compiled schema.
#[must_use]
pub fn validate_compiled(
    compiled: &CompiledSchema,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
) -> Vec<ValidationError> {
    thread_local! {
        static VALIDATOR: Validator = Validator::new();
    }
    VALIDATOR.with(|v| v.validate_compiled(compiled, object, old_object))
}
```

- [ ] **Step 5: Update Default impl**

```rust
impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test --all-features`
Expected: All 476 tests pass — behavior is identical, only allocation pattern changed.

- [ ] **Step 7: Commit**

```bash
git add src/validation.rs
git commit -m "perf: move register_all to Validator::new(), cache in thread_local for convenience fns"
```

---

## Chunk 2: Compilation & Value Conversion Fixes

### ~~Task 2: Remove `raw.clone()` in `compile_schema_validations`~~ SKIPPED

> `serde_json::Value` implements `Deserializer` only for owned values, not `&Value`.
> `Rule::deserialize(raw)` requires owned `Value`, so the `.clone()` is unavoidable.
> serde_json does not provide a borrowing deserializer for `&Value`.

---

### Task 2: Hoist `schema.get("items")` out of array loop

**Files:**
- Modify: `src/values.rs:108-116`

- [ ] **Step 1: Hoist the lookup**

```rust
// In json_to_cel_with_schema, Array arm:
serde_json::Value::Array(arr) => {
    let items_schema = schema.get("items");
    let items: Vec<Value> = arr
        .iter()
        .map(|item| match items_schema {
            Some(s) => json_to_cel_with_schema(item, s),
            None => json_to_cel(item),
        })
        .collect();
    Value::List(Arc::new(items))
}
```

- [ ] **Step 2: Run tests and commit**

Run: `cargo test --all-features`

```bash
git add src/values.rs
git commit -m "perf: hoist schema.get(items) outside array iteration loop"
```

---

### Task 3: `apply_defaults` — skip clone when no changes needed

**Files:**
- Modify: `src/defaults.rs:15-51`

- [ ] **Step 1: Add needs_defaults check before cloning**

```rust
pub fn apply_defaults(schema: &serde_json::Value, value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(obj) => {
            let props = match schema.get("properties").and_then(|p| p.as_object()) {
                Some(p) => p,
                None => return value.clone(), // no properties schema → no defaults possible
            };

            // Check if any defaults need to be applied before cloning
            let needs_clone = props.iter().any(|(key, prop_schema)| {
                (!obj.contains_key(key) && prop_schema.get("default").is_some())
                    || (obj.contains_key(key) && has_nested_defaults(prop_schema))
            });

            if !needs_clone {
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

/// Check if a schema has any nested `default` values.
fn has_nested_defaults(schema: &serde_json::Value) -> bool {
    if schema.get("default").is_some() {
        return true;
    }
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        return props.values().any(|p| p.get("default").is_some());
    }
    false
}
```

- [ ] **Step 2: Run tests and commit**

Run: `cargo test --all-features`
Expected: All existing defaults tests pass unchanged.

```bash
git add src/defaults.rs
git commit -m "perf: skip cloning in apply_defaults when no defaults are needed"
```

---

## Chunk 3: VAP Pre-compilation

### Task 4: Pre-compile VAP expressions

**Files:**
- Modify: `src/vap.rs`

- [ ] **Step 1: Add compiled expression type**

```rust
/// A pre-compiled VAP expression. Compile once, evaluate many times.
pub struct CompiledVapExpression {
    /// The compiled CEL program for the validation expression.
    program: Program,
    /// Original expression string (for error reporting).
    expression: String,
    /// Static fallback message.
    message: Option<String>,
    /// Pre-compiled messageExpression program (if present and valid).
    message_program: Option<Program>,
}
```

- [ ] **Step 2: Add compile method**

```rust
impl VapEvaluator {
    /// Pre-compile validation expressions for repeated evaluation.
    ///
    /// Expressions that fail to compile are included in the result with the
    /// compilation error — they will produce failure results on evaluate.
    #[must_use]
    pub fn compile_expressions(&self, expressions: &[VapExpression]) -> Vec<Result<CompiledVapExpression, String>> {
        expressions.iter().map(|expr| {
            let program = Program::compile(&expr.expression)
                .map_err(|e| format!("compilation error: {e}"))?;
            let message_program = expr.message_expression.as_deref()
                .and_then(|me| Program::compile(me).ok());
            Ok(CompiledVapExpression {
                program,
                expression: expr.expression.clone(),
                message: expr.message.clone(),
                message_program,
            })
        }).collect()
    }

    /// Evaluate pre-compiled expressions.
    #[must_use]
    pub fn evaluate_compiled(&self, compiled: &[Result<CompiledVapExpression, String>]) -> Vec<VapResult> {
        let mut ctx = Context::default();
        crate::register_all(&mut ctx);
        // ... bind variables same as evaluate() ...
        compiled.iter().map(|c| match c {
            Ok(ce) => self.eval_compiled_one(ce, &ctx),
            Err(e) => VapResult {
                expression: String::new(),
                passed: false,
                message: Some(e.clone()),
            },
        }).collect()
    }
}
```

- [ ] **Step 3: Refactor existing `evaluate()` to use compile+eval internally**

```rust
pub fn evaluate(&self, expressions: &[VapExpression]) -> Vec<VapResult> {
    let compiled = self.compile_expressions(expressions);
    self.evaluate_compiled(&compiled)
}
```

- [ ] **Step 4: Add test for pre-compilation**

```rust
#[test]
fn vap_compiled_expressions_reusable() {
    let evaluator = VapEvaluator::builder()
        .object(json!({"spec": {"replicas": 3}}))
        .request(AdmissionRequest { operation: "CREATE".into(), ..Default::default() })
        .build();

    let expressions = vec![VapExpression {
        expression: "object.spec.replicas >= 0".into(),
        message: Some("bad".into()),
        message_expression: None,
    }];

    let compiled = evaluator.compile_expressions(&expressions);
    assert!(compiled[0].is_ok());

    // Evaluate multiple times with same compiled expressions
    let r1 = evaluator.evaluate_compiled(&compiled);
    let r2 = evaluator.evaluate_compiled(&compiled);
    assert!(r1[0].passed);
    assert!(r2[0].passed);
}
```

- [ ] **Step 5: Run tests and commit**

Run: `cargo test --all-features`

```bash
git add src/vap.rs
git commit -m "perf: add compile_expressions/evaluate_compiled to avoid repeated CEL parsing"
```

---

## Chunk 4: Analysis Unification & Micro-optimizations

### Task 5: Add combined `analyze_rule` function

**Files:**
- Modify: `src/analysis.rs`

- [ ] **Step 1: Add `analyze_rule` that compiles once, runs both analyses**

```rust
/// Run all available static analyses on a CEL rule.
///
/// Compiles the rule once and performs both scope validation and cost estimation.
/// This is more efficient than calling [`check_rule_scope`] and [`estimate_rule_cost`]
/// separately, which each compile the rule independently.
#[must_use]
pub fn analyze_rule(
    rule: &str,
    schema: &CompiledSchema,
    scope: ScopeContext,
) -> Vec<AnalysisWarning> {
    let program = match Program::compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let mut warnings = Vec::new();

    // Scope validation
    let valid = valid_variables(scope);
    for var in program.references().variables() {
        if !valid.contains(&var) {
            warnings.push(AnalysisWarning {
                rule: rule.to_string(),
                message: format!(
                    "variable '{}' is not available in {:?} context; valid variables: {:?}",
                    var, scope, valid
                ),
                kind: WarningKind::WrongScope,
            });
        }
    }

    // Cost estimation
    let expr = program.expression();
    let cost = estimate_expr_cost(&expr.expr, schema);
    if cost > K8S_COST_BUDGET {
        warnings.push(AnalysisWarning {
            rule: rule.to_string(),
            message: format!(
                "estimated cost {} exceeds K8s budget {}; consider adding maxItems/maxLength to schema bounds",
                cost, K8S_COST_BUDGET
            ),
            kind: WarningKind::CostExceeded,
        });
    }
    check_missing_bounds(&expr.expr, schema, rule, &mut warnings);

    warnings
}
```

- [ ] **Step 2: Add test**

```rust
#[test]
fn analyze_rule_combined() {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {"type": "string"}
            }
        }
    });
    let compiled = compile_schema(&schema);
    // This rule has both a scope issue (request) and a cost issue (unbounded list)
    let warnings = analyze_rule(
        "request.name == 'test'",
        &compiled,
        ScopeContext::CrdValidation,
    );
    assert!(warnings.iter().any(|w| w.kind == WarningKind::WrongScope));
}

#[test]
fn analyze_rule_compiles_once() {
    // Same rule analyzed for both scope and cost in one call
    let schema = json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {"type": "string"}
            }
        }
    });
    let compiled = compile_schema(&schema);
    let warnings = analyze_rule(
        "self.items.all(item, item.size() > 0)",
        &compiled,
        ScopeContext::CrdValidation,
    );
    // Should have cost/bounds warnings but no scope warnings
    assert!(warnings.iter().all(|w| w.kind != WarningKind::WrongScope));
    assert!(warnings.iter().any(|w| w.kind == WarningKind::MissingBounds));
}
```

- [ ] **Step 3: Run tests and commit**

Run: `cargo test --all-features`

```bash
git add src/analysis.rs
git commit -m "feat: add analyze_rule combined function to compile CEL once for all analyses"
```

---

### Task 6: Add `#[inline]` to hot-path helpers

**Files:**
- Modify: `src/values.rs` (convert_number, convert_string_with_format)
- Modify: `src/validation.rs` (join_path, join_path_index, effective_path)
- Modify: `src/escaping.rs` (escape_field_name)

- [ ] **Step 1: Add `#[inline]` annotations**

In `src/values.rs`:
```rust
#[inline]
fn convert_number(n: &serde_json::Number) -> Value { ... }

#[inline]
fn convert_string_with_format(s: &str, format: &SchemaFormat) -> Value { ... }
```

In `src/validation.rs`:
```rust
#[inline]
fn effective_path(base_path: &str, rule_field_path: Option<&str>) -> String { ... }

#[inline]
fn join_path(base: &str, segment: &str) -> String { ... }

#[inline]
fn join_path_index(base: &str, index: usize) -> String { ... }
```

In `src/escaping.rs`:
```rust
#[inline]
pub fn escape_field_name(name: &str) -> String { ... }
```

- [ ] **Step 2: Run tests, fmt, clippy, commit**

Run: `just check`

```bash
git add src/values.rs src/validation.rs src/escaping.rs
git commit -m "perf: add #[inline] to hot-path value conversion and path helpers"
```

---

### Task 7: Reduce path cloning in allOf/oneOf/anyOf loops

**Files:**
- Modify: `src/validation.rs` (walk_schema allOf loop, walk_compiled allOf loop)

- [ ] **Step 1: Use last-branch optimization**

In `walk_schema`, replace:
```rust
for keyword in &["allOf", "oneOf", "anyOf"] {
    if let Some(branches) = schema.get(keyword).and_then(|v| v.as_array()) {
        for branch in branches {
            self.walk_schema(branch, value, old_value, path.clone(), errors, base_ctx, root_ctx);
        }
    }
}
```

With:
```rust
for keyword in &["allOf", "oneOf", "anyOf"] {
    if let Some(branches) = schema.get(keyword).and_then(|v| v.as_array()) {
        if let Some((last, rest)) = branches.split_last() {
            for branch in rest {
                self.walk_schema(branch, value, old_value, path.clone(), errors, base_ctx, root_ctx);
            }
            self.walk_schema(last, value, old_value, path.clone(), errors, base_ctx, root_ctx);
        }
    }
}
```

Wait — the `path` is also used after the loop (for the function return), so we can't move it. But we CAN avoid one clone if we handle the "only one branch" case:

Actually the simplest optimization: collect all branches across all 3 keywords, then use `split_last`:

```rust
// Collect all composition branches
let composition_branches: Vec<&serde_json::Value> = ["allOf", "oneOf", "anyOf"]
    .iter()
    .filter_map(|kw| schema.get(kw))
    .filter_map(|v| v.as_array())
    .flatten()
    .collect();

if let Some((last, rest)) = composition_branches.split_last() {
    for branch in rest {
        self.walk_schema(branch, value, old_value, path.clone(), errors, base_ctx, root_ctx);
    }
    // Last branch: still clone because walk_schema takes owned path
    // but this is still cleaner and sets up for future &str refactor
    self.walk_schema(last, value, old_value, path.clone(), errors, base_ctx, root_ctx);
}
```

Same pattern in `walk_compiled`.

> **Note:** The full `path: String → &str` refactor would save more allocations but touches every walker signature — defer to a future release.

- [ ] **Step 2: Run tests and commit**

Run: `cargo test --all-features`

```bash
git add src/validation.rs
git commit -m "refactor: consolidate composition branch iteration in walkers"
```

---

### Task 8: Final lint + fmt pass

- [ ] **Step 1: Run full CI checks**

Run: `just check`
Expected: All pass (fmt, clippy, test-all, test-no-default, doc, feature-check)

- [ ] **Step 2: If any issues, fix and re-run**

- [ ] **Step 3: Commit any remaining fixes**

```bash
git add -A
git commit -m "chore: fmt and clippy fixes for 0.5.2"
```

---

## Release Checklist

After all tasks pass:

- [ ] `just bump 0.5.2`
- [ ] Fill in CHANGELOG.md:
  - **Changed**: `Validator::new()` now pre-registers all CEL functions (was per-call)
  - **Changed**: `Validator` no longer derives `Clone` (holds pre-built context)
  - **Added**: `VapEvaluator::compile_expressions()` / `evaluate_compiled()` for pre-compiled VAP evaluation
  - **Added**: `analysis::analyze_rule()` — combined scope + cost analysis with single compilation
  - **Performance**: Eliminated redundant function registration on every validate call
  - **Performance**: Eliminated redundant CEL compilation in analysis functions
  - **Performance**: Reduced cloning in `apply_defaults`, `compile_schema_validations`, `json_to_cel_with_schema`
- [ ] Fix README `cel = "0.13"` if not already done
- [ ] `just release`

---

## Breaking Change Note

`Validator` loses `Clone` derive (since `Context` is not Clone). This is technically a semver concern but:
1. Pre-1.0 crate — patch-level breaks are acceptable
2. `Validator::new()` is cheap relative to validation — users should create new instances rather than clone
3. The kube-rs derive macro doesn't store or clone Validators

Document in CHANGELOG as a known change.
