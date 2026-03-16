//! Schema tree walking and CEL rule evaluation for Kubernetes CRD validation.
//!
//! This module provides [`Validator`] which recursively walks an OpenAPI schema,
//! compiles `x-kubernetes-validations` rules, evaluates them against object data,
//! and collects [`ValidationError`]s.

use crate::{
    compilation::{CompilationError, CompilationResult, CompiledSchema, compile_schema_validations},
    values::{json_to_cel_with_compiled, json_to_cel_with_schema},
};
use cel::Context;

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

/// The kind of error that occurred during validation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub enum ErrorKind {
    /// CEL expression syntax error.
    CompilationFailure,
    /// Malformed rule JSON.
    InvalidRule,
    /// Rule evaluated to `false`.
    ValidationFailure,
    /// Rule returned a non-bool value.
    InvalidResult,
    /// Runtime evaluation error.
    EvaluationError,
}

/// An error produced when a CEL validation rule fails.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ValidationError {
    /// The CEL expression that failed.
    pub rule: String,
    /// Human-readable error message.
    pub message: String,
    /// JSON path to the field (e.g., "spec.replicas").
    pub field_path: String,
    /// Machine-readable reason (e.g., "FieldValueInvalid").
    pub reason: Option<String>,
    /// Classification of the error.
    pub kind: ErrorKind,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field_path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.field_path, self.message)
        }
    }
}

impl std::error::Error for ValidationError {}

/// Validates Kubernetes objects against CRD schema CEL validation rules.
///
/// Walks the OpenAPI schema tree, compiles `x-kubernetes-validations` rules at
/// each node, and evaluates them against the corresponding object values.
///
/// For repeated validation against the same schema, use [`compile_schema`](crate::compilation::compile_schema) +
/// [`validate_compiled`](Validator::validate_compiled) to avoid re-compilation.
///
/// # Thread Safety
///
/// `Validator` is `Send` and can be moved across threads.
pub struct Validator {
    base_ctx: Context<'static>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator").finish()
    }
}

impl Validator {
    /// Create a new `Validator` with all K8s CEL functions pre-registered.
    pub fn new() -> Self {
        let mut ctx = Context::default();
        crate::register_all(&mut ctx);
        Self { base_ctx: ctx }
    }

    /// Validate an object against a CRD schema's CEL validation rules.
    ///
    /// Compiles rules on each call. For repeated validation against the same
    /// schema, prefer [`compile_schema`](crate::compilation::compile_schema) + [`validate_compiled`](Self::validate_compiled).
    #[must_use]
    pub fn validate(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
    ) -> Vec<ValidationError> {
        self.validate_with_context(schema, object, old_object, None)
    }

    /// Validate an object against a CRD schema's CEL validation rules, with optional root context.
    ///
    /// Like [`validate`](Self::validate), but also binds `apiVersion`, `apiGroup`, and `kind`
    /// as root-level CEL variables when a [`RootContext`] is provided.
    #[must_use]
    pub fn validate_with_context(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        self.walk_schema(
            schema,
            object,
            old_object,
            String::new(),
            &mut errors,
            &self.base_ctx,
            root_ctx,
        );
        errors
    }

    /// Validate an object using a pre-compiled schema tree.
    ///
    /// Use [`compile_schema`](crate::compilation::compile_schema) to build the [`CompiledSchema`], then call this
    /// method for each object to validate — rules are compiled only once.
    #[must_use]
    pub fn validate_compiled(
        &self,
        compiled: &CompiledSchema,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
    ) -> Vec<ValidationError> {
        self.validate_compiled_with_context(compiled, object, old_object, None)
    }

    /// Validate an object using a pre-compiled schema tree, with optional root context.
    ///
    /// Like [`validate_compiled`](Self::validate_compiled), but also binds `apiVersion`,
    /// `apiGroup`, and `kind` as root-level CEL variables when a [`RootContext`] is provided.
    #[must_use]
    pub fn validate_compiled_with_context(
        &self,
        compiled: &CompiledSchema,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Vec<ValidationError> {
        let mut errors = Vec::new();
        self.walk_compiled(
            compiled,
            object,
            old_object,
            String::new(),
            &mut errors,
            &self.base_ctx,
            root_ctx,
        );
        errors
    }

    /// Validate with schema defaults applied to the object first.
    ///
    /// Equivalent to calling [`crate::defaults::apply_defaults`] followed by [`validate`].
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

    /// Validate with schema defaults applied and root context variables bound.
    ///
    /// Combines [`crate::defaults::apply_defaults`] with [`validate_with_context`].
    #[must_use]
    pub fn validate_with_defaults_and_context(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Vec<ValidationError> {
        let defaulted = crate::defaults::apply_defaults(schema, object);
        let defaulted_old = old_object.map(|o| crate::defaults::apply_defaults(schema, o));
        self.validate_with_context(schema, &defaulted, defaulted_old.as_ref(), root_ctx)
    }

    // ── Schema-based walking (compiles on each call) ────────────────

    #[allow(clippy::too_many_arguments)]
    fn walk_schema(
        &self,
        schema: &serde_json::Value,
        value: &serde_json::Value,
        old_value: Option<&serde_json::Value>,
        path: String,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_>,
        root_ctx: Option<&RootContext>,
    ) {
        let cel_value = json_to_cel_with_schema(value, schema);
        let cel_old = old_value.map(|o| json_to_cel_with_schema(o, schema));
        self.evaluate_validations(
            schema,
            &cel_value,
            cel_old.as_ref(),
            &path,
            errors,
            base_ctx,
            root_ctx,
        );

        if let (Some(properties), Some(obj)) = (
            schema.get("properties").and_then(|p| p.as_object()),
            value.as_object(),
        ) {
            for (prop_name, prop_schema) in properties {
                if let Some(child_value) = obj.get(prop_name) {
                    let child_old = old_value.and_then(|o| o.get(prop_name));
                    let child_path = join_path(&path, prop_name);
                    self.walk_schema(
                        prop_schema,
                        child_value,
                        child_old,
                        child_path,
                        errors,
                        base_ctx,
                        None,
                    );
                }
            }
        }

        if let (Some(items_schema), Some(arr)) = (schema.get("items"), value.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                let old_item = old_value.and_then(|o| o.as_array()).and_then(|a| a.get(i));
                let item_path = join_path_index(&path, i);
                self.walk_schema(items_schema, item, old_item, item_path, errors, base_ctx, None);
            }
        }

        let preserve_unknown = schema
            .get("x-kubernetes-preserve-unknown-fields")
            .and_then(|v| v.as_bool())
            == Some(true);

        if !preserve_unknown
            && let (Some(additional_schema), Some(obj)) = (
                schema.get("additionalProperties").filter(|a| a.is_object()),
                value.as_object(),
            )
        {
            let known: std::collections::HashSet<&str> = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();

            for (key, val) in obj {
                if known.contains(key.as_str()) {
                    continue;
                }
                let old_val = old_value.and_then(|o| o.get(key));
                let child_path = join_path(&path, key);
                self.walk_schema(
                    additional_schema,
                    val,
                    old_val,
                    child_path,
                    errors,
                    base_ctx,
                    None,
                );
            }
        }

        // Walk allOf/oneOf/anyOf branches — all treated identically for CEL evaluation
        for keyword in &["allOf", "oneOf", "anyOf"] {
            if let Some(branches) = schema.get(keyword).and_then(|v| v.as_array()) {
                for branch in branches {
                    self.walk_schema(branch, value, old_value, path.clone(), errors, base_ctx, root_ctx);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_validations(
        &self,
        schema: &serde_json::Value,
        cel_value: &cel::Value,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_>,
        root_ctx: Option<&RootContext>,
    ) {
        let compiled = compile_schema_validations(schema);
        self.evaluate_compiled_results(&compiled, cel_value, cel_old, path, errors, base_ctx, root_ctx);
    }

    // ── CompiledSchema-based walking ────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn walk_compiled(
        &self,
        compiled: &CompiledSchema,
        value: &serde_json::Value,
        old_value: Option<&serde_json::Value>,
        path: String,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_>,
        root_ctx: Option<&RootContext>,
    ) {
        let cel_value = json_to_cel_with_compiled(value, compiled);
        let cel_old = old_value.map(|o| json_to_cel_with_compiled(o, compiled));
        self.evaluate_compiled_results(
            &compiled.validations,
            &cel_value,
            cel_old.as_ref(),
            &path,
            errors,
            base_ctx,
            root_ctx,
        );

        if let Some(obj) = value.as_object() {
            for (prop_name, child_compiled) in &compiled.properties {
                if let Some(child_value) = obj.get(prop_name) {
                    let child_old = old_value.and_then(|o| o.get(prop_name));
                    let child_path = join_path(&path, prop_name);
                    self.walk_compiled(
                        child_compiled,
                        child_value,
                        child_old,
                        child_path,
                        errors,
                        base_ctx,
                        None,
                    );
                }
            }
        }

        if let (Some(items_compiled), Some(arr)) = (&compiled.items, value.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                let old_item = old_value.and_then(|o| o.as_array()).and_then(|a| a.get(i));
                let item_path = join_path_index(&path, i);
                self.walk_compiled(items_compiled, item, old_item, item_path, errors, base_ctx, None);
            }
        }

        if !compiled.preserve_unknown_fields
            && let (Some(additional_compiled), Some(obj)) =
                (&compiled.additional_properties, value.as_object())
        {
            for (key, val) in obj {
                if compiled.properties.contains_key(key) {
                    continue;
                }
                let old_val = old_value.and_then(|o| o.get(key));
                let child_path = join_path(&path, key);
                self.walk_compiled(
                    additional_compiled,
                    val,
                    old_val,
                    child_path,
                    errors,
                    base_ctx,
                    None,
                );
            }
        }

        for branch in compiled
            .all_of
            .iter()
            .chain(compiled.one_of.iter())
            .chain(compiled.any_of.iter())
        {
            self.walk_compiled(branch, value, old_value, path.clone(), errors, base_ctx, root_ctx);
        }
    }

    // ── Shared evaluation logic ─────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn evaluate_compiled_results(
        &self,
        results: &[Result<CompilationResult, CompilationError>],
        cel_value: &cel::Value,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_>,
        root_ctx: Option<&RootContext>,
    ) {
        // Create a node-level scope once with self/oldSelf bound
        let mut node_ctx = base_ctx.new_inner_scope();
        node_ctx.add_variable_from_value("self", cel_value.clone());
        if let Some(old) = cel_old {
            node_ctx.add_variable_from_value("oldSelf", old.clone());
        }

        if path.is_empty()
            && let Some(rc) = root_ctx
        {
            node_ctx.add_variable_from_value(
                "apiVersion",
                cel::Value::String(std::sync::Arc::new(rc.api_version.clone())),
            );
            node_ctx.add_variable_from_value(
                "apiGroup",
                cel::Value::String(std::sync::Arc::new(rc.api_group.clone())),
            );
            node_ctx
                .add_variable_from_value("kind", cel::Value::String(std::sync::Arc::new(rc.kind.clone())));
        }

        for result in results {
            match result {
                Ok(cr) => {
                    self.evaluate_rule(cr, &node_ctx, cel_old, path, errors);
                }
                Err(CompilationError::Parse { rule, source }) => {
                    errors.push(ValidationError {
                        rule: rule.clone(),
                        message: format!("failed to compile rule \"{rule}\": {source}"),
                        field_path: path.to_string(),
                        reason: None,
                        kind: ErrorKind::CompilationFailure,
                    });
                }
                Err(CompilationError::InvalidRule(e)) => {
                    errors.push(ValidationError {
                        rule: String::new(),
                        message: format!("invalid rule definition: {e}"),
                        field_path: path.to_string(),
                        reason: None,
                        kind: ErrorKind::InvalidRule,
                    });
                }
            }
        }
    }

    fn evaluate_rule(
        &self,
        cr: &CompilationResult,
        node_ctx: &Context<'_>,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        // Handle transition rules
        if cr.is_transition_rule && cel_old.is_none() && cr.rule.optional_old_self != Some(true) {
            return; // skip transition rule without old value
        }

        // optionalOldSelf: true + no old object → child scope with oldSelf = null
        let use_null_old_self = cel_old.is_none() && cr.rule.optional_old_self == Some(true);
        let null_scope;
        let effective_ctx: &Context<'_> = if use_null_old_self {
            null_scope = {
                let mut s = node_ctx.new_inner_scope();
                s.add_variable_from_value("oldSelf", cel::Value::Null);
                s
            };
            &null_scope
        } else {
            node_ctx
        };

        let result = cr.program.execute(effective_ctx);
        let error_path = effective_path(path, cr.rule.field_path.as_deref());

        match result {
            Ok(cel::Value::Bool(true)) => {
                // Validation passed
            }
            Ok(cel::Value::Bool(false)) => {
                let message = self.resolve_message(cr, effective_ctx);
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message,
                    field_path: error_path,
                    reason: cr.rule.reason.clone(),
                    kind: ErrorKind::ValidationFailure,
                });
            }
            Ok(_) => {
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message: format!("rule \"{}\" did not evaluate to bool", cr.rule.rule),
                    field_path: error_path,
                    reason: None,
                    kind: ErrorKind::InvalidResult,
                });
            }
            Err(e) => {
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message: format!("rule evaluation error: {e}"),
                    field_path: error_path,
                    reason: None,
                    kind: ErrorKind::EvaluationError,
                });
            }
        }
    }

    /// Resolve the error message: try messageExpression first, fall back to
    /// static message, then default.
    fn resolve_message(&self, cr: &CompilationResult, ctx: &Context<'_>) -> String {
        if let Some(ref msg_prog) = cr.message_program
            && let Ok(cel::Value::String(s)) = msg_prog.execute(ctx)
        {
            return (*s).clone();
        }
        cr.rule
            .message
            .clone()
            .unwrap_or_else(|| format!("failed rule: {}", cr.rule.rule))
    }
}

impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function to validate without creating a [`Validator`] instance.
///
/// Uses a thread-local [`Validator`] to avoid re-registering CEL functions on each call.
///
/// See [`Validator::validate`] for details.
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
///
/// Uses a thread-local [`Validator`] to avoid re-registering CEL functions on each call.
///
/// See [`Validator::validate_compiled`] for details.
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

// ── Path helpers ────────────────────────────────────────────────────

#[inline]
fn effective_path(base_path: &str, rule_field_path: Option<&str>) -> String {
    match rule_field_path {
        Some(fp) if fp.starts_with('.') => format!("{base_path}{fp}"),
        Some(fp) if !base_path.is_empty() => format!("{base_path}.{fp}"),
        Some(fp) => fp.to_string(),
        None => base_path.to_string(),
    }
}

#[inline]
fn join_path(base: &str, segment: &str) -> String {
    if base.is_empty() {
        segment.to_string()
    } else {
        format!("{base}.{segment}")
    }
}

#[inline]
fn join_path_index(base: &str, index: usize) -> String {
    if base.is_empty() {
        format!("[{index}]")
    } else {
        format!("{base}[{index}]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compilation::compile_schema;
    use serde_json::json;

    fn make_schema(validations: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "replicas": {"type": "integer"},
                "name": {"type": "string"}
            },
            "x-kubernetes-validations": validations
        })
    }

    #[test]
    fn validation_passes() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn validation_fails() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "must be non-negative");
        assert_eq!(errors[0].rule, "self.replicas >= 0");
    }

    #[test]
    fn default_message_when_none() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("self.replicas >= 0"));
    }

    #[test]
    fn reason_preserved() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "bad", "reason": "FieldValueInvalid"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors[0].reason.as_deref(), Some("FieldValueInvalid"));
    }

    #[test]
    fn transition_rule_skipped_without_old_object() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn transition_rule_evaluated_with_old_object() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, Some(&old));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "cannot scale down");
    }

    #[test]
    fn transition_rule_passes() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 5, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, Some(&old));
        assert!(errors.is_empty());
    }

    #[test]
    fn nested_property_field_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {
                            "type": "integer",
                            "x-kubernetes-validations": [
                                {"rule": "self >= 0", "message": "must be non-negative"}
                            ]
                        }
                    }
                }
            }
        });
        let obj = json!({"spec": {"replicas": -1}});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.replicas");
        assert_eq!(errors[0].message, "must be non-negative");
    }

    #[test]
    fn array_items_validation() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"}
                        },
                        "x-kubernetes-validations": [
                            {"rule": "self.name.size() > 0", "message": "name required"}
                        ]
                    }
                }
            }
        });
        let obj = json!({
            "items": [
                {"name": "good"},
                {"name": ""},
                {"name": "also-good"}
            ]
        });
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "items[1]");
        assert_eq!(errors[0].message, "name required");
    }

    #[test]
    fn missing_field_not_validated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "optional_field": {
                    "type": "integer",
                    "x-kubernetes-validations": [
                        {"rule": "self >= 0", "message": "must be non-negative"}
                    ]
                }
            }
        });
        let obj = json!({});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn multiple_rules_partial_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "non-negative"},
            {"rule": "self.name.size() > 0", "message": "name required"}
        ]));
        let obj = json!({"replicas": -1, "name": ""});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn compilation_error_reported() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >="}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("failed to compile"));
    }

    #[test]
    fn no_validations_no_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replicas": {"type": "integer"}
            }
        });
        let obj = json!({"replicas": -1});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn display_with_field_path() {
        let err = ValidationError {
            rule: "self >= 0".into(),
            message: "must be non-negative".into(),
            field_path: "spec.replicas".into(),
            reason: None,
            kind: ErrorKind::ValidationFailure,
        };
        assert_eq!(err.to_string(), "spec.replicas: must be non-negative");
    }

    #[test]
    fn display_without_field_path() {
        let err = ValidationError {
            rule: "self >= 0".into(),
            message: "must be non-negative".into(),
            field_path: String::new(),
            reason: None,
            kind: ErrorKind::ValidationFailure,
        };
        assert_eq!(err.to_string(), "must be non-negative");
    }

    #[test]
    fn validator_default() {
        let v = Validator::default();
        let schema = make_schema(json!([{"rule": "self.replicas >= 0"}]));
        let obj = json!({"replicas": 1, "name": "app"});
        assert!(v.validate(&schema, &obj, None).is_empty());
    }

    #[test]
    fn additional_properties_walking() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "integer",
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        });
        let obj = json!({"a": 1, "b": -1, "c": 5});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "b");
    }

    // ── Phase 5 tests ───────────────────────────────────────────────

    #[test]
    fn message_expression_produces_dynamic_message() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static fallback",
            "messageExpression": "'replicas is ' + string(self.replicas) + ', must be >= 0'"
        }]));
        let obj = json!({"replicas": -5, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "replicas is -5, must be >= 0");
    }

    #[test]
    fn message_expression_falls_back_to_static() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static message",
            "messageExpression": "invalid >="
        }]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        // messageExpression failed to compile → falls back to static message
        assert_eq!(errors[0].message, "static message");
    }

    #[test]
    fn optional_old_self_evaluated_on_create() {
        let schema = make_schema(json!([{
            "rule": "oldSelf == null || self.replicas >= oldSelf.replicas",
            "message": "cannot scale down",
            "optionalOldSelf": true
        }]));
        // Create (no old object): rule is evaluated with oldSelf = null
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty()); // oldSelf == null → true
    }

    #[test]
    fn optional_old_self_with_old_object() {
        let schema = make_schema(json!([{
            "rule": "oldSelf == null || self.replicas >= oldSelf.replicas",
            "message": "cannot scale down",
            "optionalOldSelf": true
        }]));
        let obj = json!({"replicas": 1, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, Some(&old));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "cannot scale down");
    }

    #[test]
    fn optional_old_self_false_still_skips() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= oldSelf.replicas",
            "message": "cannot scale down",
            "optionalOldSelf": false
        }]));
        let obj = json!({"replicas": 1, "name": "app"});
        // optionalOldSelf: false → transition rule skipped on create
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

    #[test]
    fn validate_compiled_matches_validate() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= 0", "message": "non-negative"}
                    ],
                    "properties": {
                        "replicas": {"type": "integer"}
                    }
                }
            }
        });
        let obj = json!({"spec": {"replicas": -1}});

        let errors_schema = validate(&schema, &obj, None);
        let compiled = compile_schema(&schema);
        let errors_compiled = validate_compiled(&compiled, &obj, None);

        assert_eq!(errors_schema.len(), errors_compiled.len());
        assert_eq!(errors_schema[0].message, errors_compiled[0].message);
        assert_eq!(errors_schema[0].field_path, errors_compiled[0].field_path);
    }

    #[test]
    fn validate_compiled_reuse() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.x > 0", "message": "x must be positive"}
            ],
            "properties": {"x": {"type": "integer"}}
        });
        let compiled = compile_schema(&schema);

        // Validate multiple objects with the same compiled schema
        assert_eq!(validate_compiled(&compiled, &json!({"x": 1}), None).len(), 0);
        assert_eq!(validate_compiled(&compiled, &json!({"x": -1}), None).len(), 1);
        assert_eq!(validate_compiled(&compiled, &json!({"x": 5}), None).len(), 0);
        assert_eq!(validate_compiled(&compiled, &json!({"x": 0}), None).len(), 1);
    }

    // ── fieldPath override tests ────────────────────────────────────

    #[test]
    fn fieldpath_overrides_auto_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.x >= 0", "message": "bad", "fieldPath": ".spec.x"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"x": -1}});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.spec.x");
    }

    #[test]
    fn fieldpath_without_dot() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.name.size() > 0", "message": "bad", "fieldPath": "name"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"name": ""}});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.name");
    }

    #[test]
    fn fieldpath_at_root() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"type": "integer"}
            },
            "x-kubernetes-validations": [
                {"rule": "self.x >= 0", "message": "bad", "fieldPath": ".spec.x"}
            ]
        });
        let obj = json!({"x": -1});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, ".spec.x");
    }

    #[test]
    fn fieldpath_none_uses_auto() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.x >= 0", "message": "bad"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"x": -1}});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec");
    }

    // ── ErrorKind tests ─────────────────────────────────────────────

    #[test]
    fn error_kind_compilation_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >="}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::CompilationFailure);
    }

    #[test]
    fn error_kind_validation_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::ValidationFailure);
    }

    #[test]
    fn error_kind_evaluation_error() {
        let schema = make_schema(json!([
            {"rule": "self.missing_field > 0"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::EvaluationError);
    }

    // ── allOf/oneOf/anyOf tests ──────────────────────────────────────

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

    #[test]
    fn all_of_compiled_matches_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "allOf": [{
                "x-kubernetes-validations": [
                    {"rule": "self.x >= 0", "message": "x must be non-negative"}
                ]
            }]
        });
        let obj = json!({"x": -1});
        let errors_schema = validate(&schema, &obj, None);
        let compiled = compile_schema(&schema);
        let errors_compiled = validate_compiled(&compiled, &obj, None);
        assert_eq!(errors_schema.len(), errors_compiled.len());
        assert_eq!(errors_schema[0].message, errors_compiled[0].message);
    }

    // ── x-kubernetes-preserve-unknown-fields tests ──────────────────

    #[test]
    fn preserve_unknown_fields_skips_additional_properties_walk() {
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
        assert!(errors.is_empty());
    }

    #[test]
    fn without_preserve_unknown_fields_additional_properties_still_walked() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "integer",
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        });
        let obj = json!({"unknown_field": -1});
        let errors = validate(&schema, &obj, None);
        assert_eq!(errors.len(), 1);
    }

    // ── x-kubernetes-embedded-resource tests ────────────────────────

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
        let obj = json!({"spec": {}});
        let errors = validate(&schema, &obj, None);
        assert!(errors.is_empty());
    }

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

    #[test]
    fn embedded_resource_compiled_path() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-embedded-resource": true,
            "x-kubernetes-validations": [{
                "rule": "self.kind.size() >= 0",
                "message": "kind must exist"
            }]
        });
        let obj = json!({"spec": {}});
        let compiled = compile_schema(&schema);
        let errors = validate_compiled(&compiled, &obj, None);
        assert!(errors.is_empty());
    }

    // ── RootContext tests ────────────────────────────────────────────

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

    #[test]
    fn root_context_compiled_path() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "x-kubernetes-validations": [{
                "rule": "kind == 'MyResource'",
                "message": "wrong kind"
            }]
        });
        let obj = json!({"x": 1});
        let root_ctx = RootContext {
            api_version: "v1".into(),
            api_group: "example.com".into(),
            kind: "MyResource".into(),
        };
        let compiled = crate::compilation::compile_schema(&schema);
        let errors = Validator::new().validate_compiled_with_context(&compiled, &obj, None, Some(&root_ctx));
        assert!(errors.is_empty());
    }

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
        // Without defaults, replicas is missing -> no validation runs
        let errors = validate(&schema, &json!({}), None);
        assert!(errors.is_empty());

        // With defaults, replicas=1 is injected -> validation runs and passes
        let errors = Validator::new().validate_with_defaults(&schema, &json!({}), None);
        assert!(errors.is_empty());
    }

    #[test]
    fn validate_with_defaults_and_context_combined() {
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
            },
            "x-kubernetes-validations": [
                {"rule": "kind == 'Deployment'", "message": "wrong kind"}
            ]
        });
        let root_ctx = RootContext {
            api_version: "apps/v1".into(),
            api_group: "apps".into(),
            kind: "Deployment".into(),
        };
        // Empty object: defaults fill replicas=1, root context provides kind
        let errors = Validator::new().validate_with_defaults_and_context(
            &schema, &json!({}), None, Some(&root_ctx)
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn validation_error_serializable() {
        let err = ValidationError {
            rule: "self.x >= 0".into(),
            message: "must be non-negative".into(),
            field_path: "spec.x".into(),
            reason: Some("FieldValueInvalid".into()),
            kind: ErrorKind::ValidationFailure,
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["rule"], "self.x >= 0");
        assert_eq!(json["field_path"], "spec.x");
        assert_eq!(json["kind"], "ValidationFailure");
    }
}
