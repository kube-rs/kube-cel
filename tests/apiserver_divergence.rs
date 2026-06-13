//! apiserver divergence map.
//!
//! Pins every known way the [`Validator`] verdict differs from the upstream
//! Kubernetes apiserver, with the direction of the divergence:
//!
//! - **fail-closed** — kube-cel reports an error where the apiserver would
//!   accept the object (a false positive; safe, but not faithful).
//! - **fail-open** — kube-cel accepts where the apiserver would reject (a false
//!   negative; dangerous). There should be **none** of these; the depth case
//!   used to be one and was fixed in S4 (P1-A).
//!
//! Each test locks the *current measured* behavior so a divergence cannot
//! change silently. The human-readable summary lives in the README
//! "apiserver divergence" table — keep the two in sync.
//!
//! Measured against `cel` 0.13: the unsupported CEL macros below **parse**
//! successfully but fail at evaluation with an "Undeclared reference" error, so
//! through the Validator they surface as [`ErrorKind::EvaluationError`], *not*
//! `CompilationFailure`.
#![cfg(feature = "validation")]

use kube_cel::validation::{ErrorKind, Validator};
use serde_json::json;

/// Runs a single `x-kubernetes-validations` rule against `{"items": [3,1,2]}`
/// and returns the resulting errors.
fn run_rule(rule: &str) -> Vec<kube_cel::ValidationError> {
    let schema = json!({
        "type": "object",
        "properties": {
            "items": { "type": "array", "items": {"type": "integer"} }
        },
        "x-kubernetes-validations": [ {"rule": rule, "message": "divergence probe"} ]
    });
    let object = json!({"items": [3, 1, 2]});
    Validator::new().validate(&schema, &object, None)
}

/// Asserts a rule fails closed: the apiserver would accept it, but kube-cel
/// rejects it with an `EvaluationError` because `cel` 0.13 lacks the macro.
fn assert_unsupported_macro(feature: &str, rule: &str) {
    let errors = run_rule(rule);
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::EvaluationError),
        "{feature}: expected fail-closed EvaluationError (unsupported macro), got {errors:?}"
    );
}

// ── Unsupported CEL macros (cel-crate gated) — all fail CLOSED ──────────────
// apiserver: supported, rule evaluates normally.
// kube-cel:  EvaluationError ("Undeclared reference"), object rejected.

#[test]
fn sort_by_fails_closed() {
    assert_unsupported_macro("sortBy", "self.items.sortBy(x, x) == [1, 2, 3]");
}

#[test]
fn cel_bind_fails_closed() {
    assert_unsupported_macro("cel.bind", "cel.bind(s, self.items.size(), s > 0)");
}

#[test]
fn two_var_comprehension_fails_closed() {
    // K8s 1.33+ two-argument comprehensions. The one-arg form works (see
    // `one_arg_comprehension_is_supported` below); only the two-arg form diverges.
    assert_unsupported_macro("all(i, v, …)", "self.items.all(i, v, v > 0)");
    assert_unsupported_macro(
        "transformList",
        "self.items.transformList(i, v, v * 2).size() == 3",
    );
}

/// Control: the single-argument comprehension IS supported, so it does not
/// diverge — proving the two-arg failures above are about the two-var form,
/// not comprehensions in general.
#[test]
fn one_arg_comprehension_is_supported() {
    let errors = run_rule("self.items.all(x, x > 0)");
    assert!(
        errors.is_empty(),
        "one-arg all() should be supported, got {errors:?}"
    );
}

// ── Schema depth cap — fails CLOSED since S4 (P1-A); used to fail OPEN ───────
// apiserver: enforces deep rules (and rejects over-limit schemas at registration).
// kube-cel:  SchemaTooDeep error past MAX_SCHEMA_DEPTH (64).

#[test]
fn depth_cap_fails_closed() {
    // Nest one integer field with a violated `self >= 0` rule past the cap.
    let mut schema = json!({
        "type": "integer",
        "x-kubernetes-validations": [{"rule": "self >= 0", "message": "leaf"}]
    });
    let mut object = json!(-1);
    for _ in 0..70 {
        schema = json!({ "type": "object", "properties": { "c": schema } });
        object = json!({ "c": object });
    }
    let errors = Validator::new().validate(&schema, &object, None);
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::SchemaTooDeep),
        "depth past the cap must fail closed with SchemaTooDeep, got {errors:?}"
    );
}
