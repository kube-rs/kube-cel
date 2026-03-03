#![cfg(feature = "validation")]

//! Integration tests validating that `json_to_cel` output works end-to-end
//! with CEL program compilation and evaluation, including `self`/`oldSelf`
//! variable binding and kube-cel extension functions.

use cel::{Context, Program, Value};
use kube_cel::values::json_to_cel;
use serde_json::json;
use std::sync::Arc;

/// Helper: create a context with kube-cel functions, bind `self` from JSON,
/// compile the expression, and return the evaluation result.
fn eval_self(json_val: serde_json::Value, expr: &str) -> Value {
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value("self", json_to_cel(&json_val));
    Program::compile(expr).unwrap().execute(&ctx).unwrap()
}

/// Helper: same as `eval_self` but also binds `oldSelf`.
fn eval_transition(json_self: serde_json::Value, json_old: serde_json::Value, expr: &str) -> Value {
    let mut ctx = Context::default();
    kube_cel::register_all(&mut ctx);
    ctx.add_variable_from_value("self", json_to_cel(&json_self));
    ctx.add_variable_from_value("oldSelf", json_to_cel(&json_old));
    Program::compile(expr).unwrap().execute(&ctx).unwrap()
}

// ── Scalar comparison ───────────────────────────────────────────────

#[test]
fn scalar_comparison() {
    assert_eq!(eval_self(json!(10), "self >= 0"), Value::Bool(true));
    assert_eq!(eval_self(json!(-1), "self >= 0"), Value::Bool(false));
}

// ── Field access ────────────────────────────────────────────────────

#[test]
fn field_access_int() {
    let obj = json!({"replicas": 3});
    assert_eq!(eval_self(obj, "self.replicas"), Value::Int(3));
}

#[test]
fn field_access_string() {
    let obj = json!({"name": "my-app"});
    assert_eq!(
        eval_self(obj, "self.name"),
        Value::String(Arc::new("my-app".into()))
    );
}

// ── Nested field access ─────────────────────────────────────────────

#[test]
fn nested_field_comparison() {
    let obj = json!({
        "spec": {
            "replicas": 5,
            "minReplicas": 2
        }
    });
    assert_eq!(
        eval_self(obj, "self.spec.replicas >= self.spec.minReplicas"),
        Value::Bool(true)
    );
}

// ── oldSelf transition rule ─────────────────────────────────────────

#[test]
fn transition_rule_oldself() {
    let new = json!({"replicas": 5});
    let old = json!({"replicas": 3});
    assert_eq!(
        eval_transition(new, old, "self.replicas >= oldSelf.replicas"),
        Value::Bool(true)
    );
}

#[test]
fn transition_rule_downscale_rejected() {
    let new = json!({"replicas": 1});
    let old = json!({"replicas": 3});
    assert_eq!(
        eval_transition(new, old, "self.replicas >= oldSelf.replicas"),
        Value::Bool(false)
    );
}

// ── Program::references().has_variable("oldSelf") detection ─────────

#[test]
fn detect_oldself_reference() {
    let transition_expr = "self.replicas >= oldSelf.replicas";
    let non_transition_expr = "self.replicas >= 0";

    let prog1 = Program::compile(transition_expr).unwrap();
    assert!(prog1.references().has_variable("oldSelf"));
    assert!(prog1.references().has_variable("self"));

    let prog2 = Program::compile(non_transition_expr).unwrap();
    assert!(!prog2.references().has_variable("oldSelf"));
    assert!(prog2.references().has_variable("self"));
}

// ── kube-cel extension functions ────────────────────────────────────

#[test]
#[cfg(feature = "strings")]
fn extension_trim_lower_ascii() {
    let obj = json!({"name": "  Hello World  "});
    assert_eq!(
        eval_self(obj, "self.name.trim().lowerAscii()"),
        Value::String(Arc::new("hello world".into()))
    );
}

#[test]
#[cfg(feature = "lists")]
fn extension_is_sorted() {
    let obj = json!({"items": [1, 2, 3, 4]});
    assert_eq!(eval_self(obj, "self.items.isSorted()"), Value::Bool(true));

    let obj2 = json!({"items": [3, 1, 2]});
    assert_eq!(eval_self(obj2, "self.items.isSorted()"), Value::Bool(false));
}

// ── Array indexing ──────────────────────────────────────────────────

#[test]
fn array_indexing() {
    let obj = json!({
        "containers": [
            {"name": "nginx"},
            {"name": "sidecar"}
        ]
    });
    assert_eq!(
        eval_self(obj, "self.containers[0].name"),
        Value::String(Arc::new("nginx".into()))
    );
}

// ── Null comparison ─────────────────────────────────────────────────

#[test]
fn null_comparison() {
    let obj = json!({"extra": null});
    assert_eq!(eval_self(obj, "self.extra == null"), Value::Bool(true));
}

#[test]
fn non_null_comparison() {
    let obj = json!({"extra": "present"});
    assert_eq!(eval_self(obj, "self.extra == null"), Value::Bool(false));
}

// ── has() macro ─────────────────────────────────────────────────────

#[test]
fn has_macro_present() {
    let obj = json!({"name": "test"});
    assert_eq!(eval_self(obj, "has(self.name)"), Value::Bool(true));
}

#[test]
fn has_macro_missing() {
    let obj = json!({"name": "test"});
    assert_eq!(eval_self(obj, "has(self.missing)"), Value::Bool(false));
}

// ── size() function ─────────────────────────────────────────────────

#[test]
fn size_of_list() {
    let obj = json!({"items": [1, 2, 3]});
    assert_eq!(eval_self(obj, "size(self.items)"), Value::Int(3));
}

#[test]
fn size_of_string() {
    let obj = json!({"name": "hello"});
    assert_eq!(eval_self(obj, "size(self.name)"), Value::Int(5));
}
