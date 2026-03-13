#![cfg(feature = "validation")]

//! Integration tests for the compilation module.
//!
//! These tests use realistic CRD schema JSON to verify end-to-end
//! rule extraction, compilation, and evaluation.

use cel::{Context, Value};
use kube_cel::{
    compilation::{CompilationError, compile_schema},
    values::json_to_cel,
};
use serde_json::json;

/// Helper: compile rules from a schema, bind `self` from JSON, and evaluate
/// the first successfully compiled program.
fn compile_and_eval_first(schema: serde_json::Value, self_val: serde_json::Value) -> Value {
    let compiled = compile_schema(&schema);
    let cr = compiled.validations.into_iter().next().unwrap().unwrap();

    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
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
                    {
                        "rule": "self.replicas >= self.minReplicas",
                        "message": "replicas must be >= minReplicas"
                    }
                ]
            }
        }
    });

    // Extract rules from the spec-level schema
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
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value("self", json_to_cel(&self_val));
    assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));
}

#[test]
fn compile_and_eval_with_json_to_cel() {
    let schema = json!({
        "x-kubernetes-validations": [
            {"rule": "self.name.size() > 0", "message": "name required"}
        ]
    });
    let result = compile_and_eval_first(schema, json!({"name": "my-app"}));
    assert_eq!(result, Value::Bool(true));
}

#[test]
fn transition_rule_compile_and_eval() {
    let schema = json!({
        "x-kubernetes-validations": [
            {
                "rule": "self.replicas >= oldSelf.replicas",
                "message": "cannot scale down",
                "reason": "FieldValueForbidden"
            }
        ]
    });

    let compiled_schema = compile_schema(&schema);
    let compiled = compiled_schema.validations.into_iter().next().unwrap().unwrap();

    assert!(compiled.is_transition_rule);
    assert_eq!(compiled.rule.message.as_deref(), Some("cannot scale down"));
    assert_eq!(compiled.rule.reason.as_deref(), Some("FieldValueForbidden"));

    // Evaluate with self and oldSelf
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value("self", json_to_cel(&json!({"replicas": 5})));
    ctx.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
    assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));

    // Scale down should fail
    let mut ctx2 = Context::default();
    kube_cel::register_all(&mut ctx2);
    ctx2.add_variable_from_value("self", json_to_cel(&json!({"replicas": 1})));
    ctx2.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
    assert_eq!(compiled.program.execute(&ctx2).unwrap(), Value::Bool(false));
}

#[test]
fn message_and_reason_preserved() {
    let schema = json!({
        "x-kubernetes-validations": [
            {
                "rule": "self.x > 0",
                "message": "x must be positive",
                "messageExpression": "\"x is \" + string(self.x)",
                "reason": "FieldValueInvalid",
                "fieldPath": ".spec.x"
            }
        ]
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

    // First rule: valid, evaluate it
    let cr = compiled.validations[0].as_ref().unwrap();
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value("self", json_to_cel(&json!({"a": 5})));
    assert_eq!(cr.program.execute(&ctx).unwrap(), Value::Bool(true));

    // Second rule: parse error
    assert!(matches!(
        compiled.validations[1].as_ref().unwrap_err(),
        CompilationError::Parse { .. }
    ));

    // Third rule: valid
    assert!(compiled.validations[2].is_ok());
}

#[test]
fn realistic_crd_with_multiple_validation_levels() {
    // A CRD-like schema where validations exist at different levels
    let crd_schema = json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "properties": {
                    "replicas": {"type": "integer"},
                    "template": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"}
                        },
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

    // Compile spec-level rules
    let spec_compiled = compile_schema(&crd_schema["properties"]["spec"]);
    assert_eq!(spec_compiled.validations.len(), 1);
    let spec_cr = spec_compiled.validations.into_iter().next().unwrap().unwrap();

    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value(
        "self",
        json_to_cel(&json!({"replicas": 3, "template": {"name": "web"}})),
    );
    assert_eq!(spec_cr.program.execute(&ctx).unwrap(), Value::Bool(true));

    // Compile template-level rules
    let tmpl_compiled = compile_schema(&crd_schema["properties"]["spec"]["properties"]["template"]);
    assert_eq!(tmpl_compiled.validations.len(), 1);
    let tmpl_cr = tmpl_compiled.validations.into_iter().next().unwrap().unwrap();

    let mut ctx2 = Context::default();
    kube_cel::register_all(&mut ctx2);
    ctx2.add_variable_from_value("self", json_to_cel(&json!({"name": "web"})));
    assert_eq!(tmpl_cr.program.execute(&ctx2).unwrap(), Value::Bool(true));

    // Empty name should fail
    let mut ctx3 = Context::default();
    kube_cel::register_all(&mut ctx3);
    ctx3.add_variable_from_value("self", json_to_cel(&json!({"name": ""})));
    assert_eq!(tmpl_cr.program.execute(&ctx3).unwrap(), Value::Bool(false));
}

#[test]
#[cfg(feature = "strings")]
fn compiled_rule_with_extension_functions() {
    let schema = json!({
        "x-kubernetes-validations": [
            {"rule": "self.name.trim().lowerAscii().size() > 0"}
        ]
    });

    let result = compile_and_eval_first(schema, json!({"name": "  Hello  "}));
    assert_eq!(result, Value::Bool(true));
}
