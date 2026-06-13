//! Compilation of Kubernetes CRD `x-kubernetes-validations` rules into CEL programs.
//!
//! This module parses validation rules from CRD schemas and compiles them into
//! [`cel::Program`] instances that can be evaluated against resource data.

use std::collections::HashMap;

use cel::Program;

use crate::validation::values::SchemaFormat;

/// A single CRD `x-kubernetes-validations` rule.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// The CEL expression to evaluate.
    pub rule: String,
    /// Static error message returned when validation fails.
    #[serde(default)]
    pub message: Option<String>,
    /// CEL expression that produces a dynamic error message.
    #[serde(default)]
    pub message_expression: Option<String>,
    /// Machine-readable reason for the validation failure (e.g. "FieldValueForbidden").
    #[serde(default)]
    pub reason: Option<String>,
    /// JSONPath to the field that caused the failure.
    #[serde(default)]
    pub field_path: Option<String>,
    /// Whether `oldSelf` is optional. When `true`, transition rules are
    /// evaluated even on create (with `oldSelf` bound to null).
    #[serde(default)]
    pub optional_old_self: Option<bool>,
}

/// The result of successfully compiling a [`Rule`].
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change.
#[derive(Debug)]
#[non_exhaustive]
pub struct CompilationResult {
    /// The compiled CEL program.
    pub program: Program,
    /// The original rule that was compiled.
    pub rule: Rule,
    /// Whether the rule references `oldSelf` (transition rule).
    pub is_transition_rule: bool,
    /// Pre-compiled `messageExpression` program (if present and valid).
    /// `None` if no `messageExpression` was specified or if it failed to compile.
    pub message_program: Option<Program>,
}

/// Errors that can occur during rule compilation.
#[derive(Debug)]
#[non_exhaustive]
pub enum CompilationError {
    /// CEL expression failed to parse.
    Parse {
        /// The original CEL expression that failed to compile.
        rule: String,
        /// The boxed parse error reported by the CEL compiler. Boxed (rather
        /// than carrying the concrete `cel::ParseErrors`) so the pre-1.0 `cel`
        /// type is not frozen into this public enum variant; reach it via
        /// [`Error::source`](std::error::Error::source).
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// JSON value could not be deserialized into a [`Rule`].
    InvalidRule(serde_json::Error),
    /// Schema nesting exceeded the maximum depth. The over-deep subtree was
    /// refused rather than silently truncated, so a too-deep schema cannot
    /// quietly drop the validation rules nested beneath the cap (fail-closed).
    SchemaTooDeep {
        /// The nesting depth at which the limit was exceeded.
        depth: usize,
    },
}

impl std::fmt::Display for CompilationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompilationError::Parse { rule, source } => {
                write!(f, "failed to compile CEL rule \"{rule}\": {source}")
            }
            CompilationError::InvalidRule(err) => {
                write!(f, "invalid rule definition: {err}")
            }
            CompilationError::SchemaTooDeep { depth } => {
                write!(
                    f,
                    "schema nesting depth {depth} exceeds the maximum of {MAX_SCHEMA_DEPTH}"
                )
            }
        }
    }
}

impl std::error::Error for CompilationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CompilationError::Parse { source, .. } => Some(source.as_ref()),
            CompilationError::InvalidRule(err) => Some(err),
            CompilationError::SchemaTooDeep { .. } => None,
        }
    }
}

/// Compile a single [`Rule`] into a [`CompilationResult`].
///
/// Returns [`CompilationError::Parse`] if the CEL expression is invalid.
pub(crate) fn compile_rule(rule: &Rule) -> Result<CompilationResult, CompilationError> {
    let program = Program::compile(&rule.rule).map_err(|e| CompilationError::Parse {
        rule: rule.rule.clone(),
        source: Box::new(e),
    })?;
    let is_transition_rule = program.references().has_variable("oldSelf");

    // Best-effort: compile messageExpression if present, ignore failures
    let message_program = rule
        .message_expression
        .as_deref()
        .and_then(|expr| Program::compile(expr).ok());

    Ok(CompilationResult {
        program,
        rule: rule.clone(),
        is_transition_rule,
        message_program,
    })
}

/// Extract `x-kubernetes-validations` rules from a schema node and compile them.
///
/// If the schema has no `x-kubernetes-validations` key or it is not an array,
/// returns an empty `Vec`. Each rule is compiled independently — failures in one
/// rule do not prevent others from compiling.
pub(crate) fn compile_schema_validations(
    schema: &serde_json::Value,
) -> Vec<Result<CompilationResult, CompilationError>> {
    let rules = match schema.get("x-kubernetes-validations") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => return Vec::new(),
    };

    rules
        .iter()
        .map(|raw| {
            let rule: Rule = serde_json::from_value(raw.clone()).map_err(CompilationError::InvalidRule)?;
            compile_rule(&rule)
        })
        .collect()
}

/// A pre-compiled schema tree. Compile once with [`compile_schema`], then
/// validate many objects via [`Validator::validate_compiled`](crate::Validator::validate_compiled).
///
/// # Note
///
/// `CompiledSchema` is not `Clone` because [`cel::Program`] is `!Clone`.
/// Wrap in [`Arc`](std::sync::Arc) for shared ownership across threads.
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change. Read its fields directly or via the
/// accessor methods below.
#[derive(Debug)]
#[non_exhaustive]
pub struct CompiledSchema {
    /// Compiled validation rules at this schema node.
    pub validations: Vec<Result<CompilationResult, CompilationError>>,
    /// Compiled child property schemas.
    pub properties: HashMap<String, CompiledSchema>,
    /// Compiled array items schema.
    pub items: Option<Box<CompiledSchema>>,
    /// Compiled additionalProperties schema.
    pub additional_properties: Option<Box<CompiledSchema>>,
    /// The `format` hint from the schema (e.g., `date-time`, `duration`).
    pub format: SchemaFormat,
    /// Compiled `allOf` branch schemas.
    pub all_of: Vec<CompiledSchema>,
    /// Compiled `oneOf` branch schemas.
    pub one_of: Vec<CompiledSchema>,
    /// Compiled `anyOf` branch schemas.
    pub any_of: Vec<CompiledSchema>,
    /// `maxLength` from the schema (for cost estimation).
    pub max_length: Option<u64>,
    /// `maxItems` from the schema (for cost estimation).
    pub max_items: Option<u64>,
    /// `maxProperties` from the schema (for cost estimation).
    pub max_properties: Option<u64>,
    /// Whether `x-kubernetes-preserve-unknown-fields: true` is set on this node.
    /// When true, `additionalProperties` walking is skipped.
    pub preserve_unknown_fields: bool,
    /// Whether `x-kubernetes-embedded-resource: true` is set on this node.
    /// When true, `apiVersion`, `kind`, and `metadata` keys are injected with
    /// defaults if absent during value conversion.
    pub embedded_resource: bool,
}

impl CompiledSchema {
    /// Returns references to all compilation errors in this node's validations.
    #[must_use]
    pub fn compilation_errors(&self) -> Vec<&CompilationError> {
        self.validations.iter().filter_map(|r| r.as_ref().err()).collect()
    }

    /// Returns `true` if any validation rule at this node failed to compile.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.validations.iter().any(|r| r.is_err())
    }
}

/// Maximum schema nesting depth to prevent unbounded recursion.
///
/// Shared across compile (`compilation`), validate (`validation`), and default
/// injection (`defaults`) so the three depth guards stay in lockstep.
pub(crate) const MAX_SCHEMA_DEPTH: usize = 64;

fn compile_schema_array(schema: &serde_json::Value, key: &str, depth: usize) -> Vec<CompiledSchema> {
    schema
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|s| compile_schema_inner(s, depth)).collect())
        .unwrap_or_default()
}

/// Recursively compile all `x-kubernetes-validations` rules in a schema tree.
///
/// Returns a [`CompiledSchema`] that can be reused across multiple validation
/// calls, avoiding repeated compilation.
#[must_use]
pub fn compile_schema(schema: &serde_json::Value) -> CompiledSchema {
    compile_schema_inner(schema, 0)
}

fn compile_schema_inner(schema: &serde_json::Value, depth: usize) -> CompiledSchema {
    if depth > MAX_SCHEMA_DEPTH {
        // Fail-closed: carry a SchemaTooDeep marker rather than returning a
        // silently-empty node, so `compilation_errors()` and the validators
        // surface the truncation instead of dropping the deep rules.
        return CompiledSchema {
            validations: vec![Err(CompilationError::SchemaTooDeep { depth })],
            properties: HashMap::new(),
            items: None,
            additional_properties: None,
            format: SchemaFormat::None,
            all_of: Vec::new(),
            one_of: Vec::new(),
            any_of: Vec::new(),
            max_length: None,
            max_items: None,
            max_properties: None,
            preserve_unknown_fields: false,
            embedded_resource: false,
        };
    }

    let validations = compile_schema_validations(schema);

    let mut properties = HashMap::new();
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (name, prop_schema) in props {
            properties.insert(name.clone(), compile_schema_inner(prop_schema, depth + 1));
        }
    }

    let items = schema
        .get("items")
        .map(|s| Box::new(compile_schema_inner(s, depth + 1)));

    let additional_properties = schema
        .get("additionalProperties")
        .filter(|a| a.is_object())
        .map(|s| Box::new(compile_schema_inner(s, depth + 1)));

    let format = SchemaFormat::from_schema(schema);

    let all_of = compile_schema_array(schema, "allOf", depth + 1);
    let one_of = compile_schema_array(schema, "oneOf", depth + 1);
    let any_of = compile_schema_array(schema, "anyOf", depth + 1);

    let max_length = schema.get("maxLength").and_then(|v| v.as_u64());
    let max_items = schema.get("maxItems").and_then(|v| v.as_u64());
    let max_properties = schema.get("maxProperties").and_then(|v| v.as_u64());

    let preserve_unknown_fields = schema
        .get("x-kubernetes-preserve-unknown-fields")
        .and_then(|v| v.as_bool())
        == Some(true);

    let embedded_resource = schema
        .get("x-kubernetes-embedded-resource")
        .and_then(|v| v.as_bool())
        == Some(true);

    CompiledSchema {
        validations,
        properties,
        items,
        additional_properties,
        format,
        all_of,
        one_of,
        any_of,
        max_length,
        max_items,
        max_properties,
        preserve_unknown_fields,
        embedded_resource,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compile_simple_rule() {
        let rule = Rule {
            rule: "self.replicas >= 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(!result.is_transition_rule);
    }

    #[test]
    fn detect_transition_rule() {
        let rule = Rule {
            rule: "self.replicas >= oldSelf.replicas".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.is_transition_rule);
    }

    #[test]
    fn detect_non_transition_rule() {
        let rule = Rule {
            rule: "self.replicas > 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(!result.is_transition_rule);
    }

    #[test]
    fn parse_error_on_invalid_cel() {
        let rule = Rule {
            rule: "self.replicas >=".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let err = compile_rule(&rule).unwrap_err();
        assert!(matches!(err, CompilationError::Parse { .. }));
        // Display should contain the rule text
        let msg = err.to_string();
        assert!(msg.contains("self.replicas >="));
    }

    #[test]
    fn deserialize_rule_all_fields() {
        let raw = json!({
            "rule": "self.x > 0",
            "message": "x must be positive",
            "messageExpression": "\"x is \" + string(self.x)",
            "reason": "FieldValueInvalid",
            "fieldPath": ".spec.x",
            "optionalOldSelf": true
        });
        let rule: Rule = serde_json::from_value(raw).unwrap();
        assert_eq!(rule.rule, "self.x > 0");
        assert_eq!(rule.message.as_deref(), Some("x must be positive"));
        assert_eq!(
            rule.message_expression.as_deref(),
            Some("\"x is \" + string(self.x)")
        );
        assert_eq!(rule.reason.as_deref(), Some("FieldValueInvalid"));
        assert_eq!(rule.field_path.as_deref(), Some(".spec.x"));
        assert_eq!(rule.optional_old_self, Some(true));
    }

    #[test]
    fn deserialize_rule_minimal() {
        let raw = json!({"rule": "self.x > 0"});
        let rule: Rule = serde_json::from_value(raw).unwrap();
        assert_eq!(rule.rule, "self.x > 0");
        assert!(rule.message.is_none());
        assert!(rule.message_expression.is_none());
        assert!(rule.reason.is_none());
        assert!(rule.field_path.is_none());
        assert!(rule.optional_old_self.is_none());
    }

    #[test]
    fn schema_validations_extracts_and_compiles() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.replicas >= 0", "message": "must be non-negative"},
                {"rule": "self.name.size() > 0"}
            ]
        });
        let results = compile_schema_validations(&schema);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
    }

    #[test]
    fn schema_validations_no_key() {
        let schema = json!({"type": "object"});
        let results = compile_schema_validations(&schema);
        assert!(results.is_empty());
    }

    #[test]
    fn schema_validations_empty_array() {
        let schema = json!({
            "x-kubernetes-validations": []
        });
        let results = compile_schema_validations(&schema);
        assert!(results.is_empty());
    }

    #[test]
    fn message_expression_compiled() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: Some("x must be positive".into()),
            message_expression: Some("'x is ' + string(self.x)".into()),
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.message_program.is_some());
    }

    #[test]
    fn message_expression_invalid_rejected() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: Some("fallback".into()),
            message_expression: Some("invalid >=".into()),
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        // A messageExpression that fails to compile must fail closed (mirroring
        // the rule path + the apiserver, which rejects such a CRD at
        // registration), not be silently dropped with a fall-back to the static
        // message.
        assert!(
            compile_rule(&rule).is_err(),
            "broken messageExpression must surface as a compilation error"
        );
    }

    #[test]
    fn message_expression_none() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.message_program.is_none());
    }

    #[test]
    fn compile_schema_tree() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [{"rule": "has(self.spec)"}],
            "properties": {
                "spec": {
                    "type": "object",
                    "x-kubernetes-validations": [{"rule": "self.replicas >= 0"}],
                    "properties": {
                        "replicas": {"type": "integer"}
                    }
                }
            }
        });
        let compiled = compile_schema(&schema);
        assert_eq!(compiled.validations.len(), 1);
        assert!(compiled.properties.contains_key("spec"));
        let spec = &compiled.properties["spec"];
        assert_eq!(spec.validations.len(), 1);
        assert!(spec.properties.contains_key("replicas"));
    }

    #[test]
    fn compile_schema_with_items() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "x-kubernetes-validations": [{"rule": "self.name.size() > 0"}]
            }
        });
        let compiled = compile_schema(&schema);
        assert!(compiled.items.is_some());
        assert_eq!(compiled.items.as_ref().unwrap().validations.len(), 1);
    }

    #[test]
    fn compile_schema_empty() {
        let schema = json!({"type": "object"});
        let compiled = compile_schema(&schema);
        assert!(compiled.validations.is_empty());
        assert!(compiled.properties.is_empty());
        assert!(compiled.items.is_none());
        assert!(compiled.additional_properties.is_none());
    }

    #[test]
    fn schema_validations_partial_errors() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.y >="},
                {"rule": "self.z == true"}
            ]
        });
        let results = compile_schema_validations(&schema);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
    }

    #[test]
    fn compilation_errors_method() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.y >="},
                {"rule": "self.z == true"}
            ]
        });
        let compiled = compile_schema(&schema);
        let errors = compiled.compilation_errors();
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], CompilationError::Parse { .. }));
        assert!(compiled.has_errors());
    }

    #[test]
    fn compilation_errors_empty_when_all_valid() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.z == true"}
            ]
        });
        let compiled = compile_schema(&schema);
        assert!(compiled.compilation_errors().is_empty());
        assert!(!compiled.has_errors());
    }
}

/// White-box end-to-end tests of `compile_schema` + `CompilationResult` that
/// bind `self` via the now-internal `json_to_cel`. Relocated from
/// `tests/compilation_integration.rs` when `json_to_cel` became `pub(crate)`.
#[cfg(test)]
mod end_to_end_tests {
    use cel::{Context, Value};
    use serde_json::json;

    use super::{CompilationError, compile_schema};
    use crate::{register_all, validation::values::json_to_cel};

    /// Compile rules from a schema, bind `self`, evaluate the first program.
    fn compile_and_eval_first(schema: serde_json::Value, self_val: serde_json::Value) -> Value {
        let compiled = compile_schema(&schema);
        let cr = compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx = Context::default();
        register_all(&mut ctx);
        ctx.add_variable_from_value("self", json_to_cel(&self_val));
        cr.program.execute(&ctx).unwrap()
    }

    #[test]
    fn crd_schema_end_to_end() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {"type": "integer"},
                        "minReplicas": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= self.minReplicas", "message": "replicas must be >= minReplicas"}
                    ]
                }
            }
        });

        let spec_schema = &schema["properties"]["spec"];
        let self_val = json!({"replicas": 5, "minReplicas": 2});

        let spec_compiled = compile_schema(spec_schema);
        assert_eq!(spec_compiled.validations.len(), 1);
        let compiled = spec_compiled.validations.into_iter().next().unwrap().unwrap();

        assert!(!compiled.is_transition_rule);
        assert_eq!(
            compiled.rule.message.as_deref(),
            Some("replicas must be >= minReplicas")
        );

        let mut ctx = Context::default();
        register_all(&mut ctx);
        ctx.add_variable_from_value("self", json_to_cel(&self_val));
        assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));
    }

    #[test]
    fn compile_and_eval_with_json_to_cel() {
        let schema = json!({
            "x-kubernetes-validations": [{"rule": "self.name.size() > 0", "message": "name required"}]
        });
        let result = compile_and_eval_first(schema, json!({"name": "my-app"}));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn transition_rule_compile_and_eval() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down", "reason": "FieldValueForbidden"}
            ]
        });

        let compiled_schema = compile_schema(&schema);
        let compiled = compiled_schema.validations.into_iter().next().unwrap().unwrap();

        assert!(compiled.is_transition_rule);
        assert_eq!(compiled.rule.message.as_deref(), Some("cannot scale down"));
        assert_eq!(compiled.rule.reason.as_deref(), Some("FieldValueForbidden"));

        let mut ctx = Context::default();
        register_all(&mut ctx);
        ctx.add_variable_from_value("self", json_to_cel(&json!({"replicas": 5})));
        ctx.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
        assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));

        let mut ctx2 = Context::default();
        register_all(&mut ctx2);
        ctx2.add_variable_from_value("self", json_to_cel(&json!({"replicas": 1})));
        ctx2.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
        assert_eq!(compiled.program.execute(&ctx2).unwrap(), Value::Bool(false));
    }

    #[test]
    fn message_and_reason_preserved() {
        let schema = json!({
            "x-kubernetes-validations": [{
                "rule": "self.x > 0",
                "message": "x must be positive",
                "messageExpression": "\"x is \" + string(self.x)",
                "reason": "FieldValueInvalid",
                "fieldPath": ".spec.x"
            }]
        });

        let compiled_schema = compile_schema(&schema);
        let compiled = compiled_schema.validations.into_iter().next().unwrap().unwrap();

        assert_eq!(compiled.rule.message.as_deref(), Some("x must be positive"));
        assert_eq!(
            compiled.rule.message_expression.as_deref(),
            Some("\"x is \" + string(self.x)")
        );
        assert_eq!(compiled.rule.reason.as_deref(), Some("FieldValueInvalid"));
        assert_eq!(compiled.rule.field_path.as_deref(), Some(".spec.x"));
    }

    #[test]
    fn multiple_rules_mixed_results() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.a > 0"},
                {"rule": "invalid >="},
                {"rule": "self.b == true"}
            ]
        });

        let compiled = compile_schema(&schema);
        assert_eq!(compiled.validations.len(), 3);

        let cr = compiled.validations[0].as_ref().unwrap();
        let mut ctx = Context::default();
        register_all(&mut ctx);
        ctx.add_variable_from_value("self", json_to_cel(&json!({"a": 5})));
        assert_eq!(cr.program.execute(&ctx).unwrap(), Value::Bool(true));

        assert!(matches!(
            compiled.validations[1].as_ref().unwrap_err(),
            CompilationError::Parse { .. }
        ));

        assert!(compiled.validations[2].is_ok());
    }

    #[test]
    fn realistic_crd_with_multiple_validation_levels() {
        let crd_schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {"type": "integer"},
                        "template": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "x-kubernetes-validations": [
                                {"rule": "self.name.size() > 0", "message": "template name required"}
                            ]
                        }
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= 1", "message": "at least one replica"}
                    ]
                }
            }
        });

        let spec_compiled = compile_schema(&crd_schema["properties"]["spec"]);
        assert_eq!(spec_compiled.validations.len(), 1);
        let spec_cr = spec_compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx = Context::default();
        register_all(&mut ctx);
        ctx.add_variable_from_value(
            "self",
            json_to_cel(&json!({"replicas": 3, "template": {"name": "web"}})),
        );
        assert_eq!(spec_cr.program.execute(&ctx).unwrap(), Value::Bool(true));

        let tmpl_compiled = compile_schema(&crd_schema["properties"]["spec"]["properties"]["template"]);
        assert_eq!(tmpl_compiled.validations.len(), 1);
        let tmpl_cr = tmpl_compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx2 = Context::default();
        register_all(&mut ctx2);
        ctx2.add_variable_from_value("self", json_to_cel(&json!({"name": "web"})));
        assert_eq!(tmpl_cr.program.execute(&ctx2).unwrap(), Value::Bool(true));

        let mut ctx3 = Context::default();
        register_all(&mut ctx3);
        ctx3.add_variable_from_value("self", json_to_cel(&json!({"name": ""})));
        assert_eq!(tmpl_cr.program.execute(&ctx3).unwrap(), Value::Bool(false));
    }

    #[test]
    #[cfg(feature = "strings")]
    fn compiled_rule_with_extension_functions() {
        let schema = json!({
            "x-kubernetes-validations": [{"rule": "self.name.trim().lowerAscii().size() > 0"}]
        });
        let result = compile_and_eval_first(schema, json!({"name": "  Hello  "}));
        assert_eq!(result, Value::Bool(true));
    }
}
