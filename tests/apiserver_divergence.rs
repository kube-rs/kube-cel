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
//! Measured against `cel` 0.16, with ground truth from the apiserver's base
//! CEL environment (`k8s.io/apiserver` v0.36.1,
//! `environment.MustBaseEnvSet(1.36).NewExpressionsEnv()`). `cel` has no type
//! checker, so a rule the apiserver rejects at CRD registration because it
//! references something its environment lacks (`cel.bind`, `math.*`,
//! `isIPv4`, ...) still **parses** in kube-cel and fails at evaluation with an
//! "Undeclared reference" error. Through the Validator that surfaces as
//! [`ErrorKind::UnsupportedReference`], *not* `CompilationFailure` and not a
//! generic `EvaluationError`, which is reserved for genuine runtime errors in a
//! supported rule. Either way the object is rejected: fail-closed.
#![cfg(feature = "validation")]

use kube_cel::{ErrorKind, ValidationErrors, Validator};
use serde_json::json;

/// Runs a single `x-kubernetes-validations` rule against `{"items": [3,1,2]}`
/// and returns the resulting errors.
fn run_rule(rule: &str) -> Result<(), ValidationErrors> {
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

/// Asserts a rule fails closed with an `UnsupportedReference`: the apiserver
/// rejects the CRD at registration because its environment lacks what the rule
/// references; kube-cel parses it and rejects the object at evaluation.
fn assert_unsupported_reference(feature: &str, rule: &str) {
    let errors = run_rule(rule).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::UnsupportedReference),
        "{feature}: expected fail-closed UnsupportedReference, got {errors:?}"
    );
}

// ── Outside the apiserver's base CEL environment — all fail CLOSED ──────────
// apiserver: rejects the CRD at registration ("undeclared reference"); its
//            base environment has no `ext.Bindings`, no `ext.Math`, and none of
//            kube-cel's `isIPv4`/`isIPv6`/`isCIDRv4`/`isCIDRv6` conveniences.
// kube-cel:  UnsupportedReference ("Undeclared reference"), object rejected.
//            The validation environment leaves these out, so a rule using one
//            can never pass client-side (it used to, for `math.*` and
//            `isIPv4`-style functions: a fail-OPEN divergence).

#[test]
fn cel_bind_fails_closed() {
    assert_unsupported_reference("cel.bind", "cel.bind(s, self.items.size(), s > 0)");
}

#[test]
fn math_library_fails_closed() {
    assert_unsupported_reference("math.ceil", "math.ceil(1.2) > 0.0");
    assert_unsupported_reference("math.greatest", "math.greatest(self.items) == 3");
    assert_unsupported_reference("math.abs", "math.abs(self.items[0]) == 3");
}

#[test]
fn ip_version_helpers_fail_closed() {
    assert_unsupported_reference("isIPv4", "isIPv4('192.168.0.1')");
    assert_unsupported_reference("isIPv6", "isIPv6('::1')");
    assert_unsupported_reference("isCIDRv4", "isCIDRv4('10.0.0.0/8')");
    assert_unsupported_reference("isCIDRv6", "isCIDRv6('fd00::/8')");
}

/// The apiserver has no string `reverse()` (its `reverse` is the list one):
/// the CRD is rejected at registration ("found no matching overload"). kube-cel
/// has no matching overload at evaluation either, so the object is rejected,
/// as an `EvaluationError` rather than an `UnsupportedReference`.
#[test]
fn string_reverse_fails_closed() {
    let errors = run_rule("'abc'.reverse() == 'cba'").unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::EvaluationError),
        "string reverse(): expected fail-closed EvaluationError, got {errors:?}"
    );
}

// ── `lists` extension and two-variable comprehensions — faithful since 0.9 ──
// apiserver: evaluates (`ext.Lists` and `ext.TwoVarComprehensions` are in the
//            base environment).
// kube-cel:  evaluates too, through `cel`'s upstream extensions. Before 0.9
//            these surfaced as UnsupportedReference (fail-closed).

/// Asserts the rule evaluates to `true` against `{"items": [3, 1, 2]}`.
fn assert_passes(rule: &str) {
    let result = run_rule(rule);
    assert!(
        result.is_ok(),
        "{rule}: expected the rule to pass, got {result:?}"
    );
}

/// Asserts the rule evaluates to `false`: proof the passing case above is a
/// real evaluation, not a blanket accept.
fn assert_fails_validation(rule: &str) {
    let errors = run_rule(rule).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::ValidationFailure),
        "{rule}: expected ValidationFailure, got {errors:?}"
    );
}

#[test]
fn sort_by_evaluates() {
    assert_passes("self.items.sortBy(x, x) == [1, 2, 3]");
    assert_passes("self.items.sortBy(x, -x) == [3, 2, 1]");
    assert_fails_validation("self.items.sortBy(x, x) == self.items");
}

#[test]
fn two_var_comprehension_evaluates() {
    assert_passes("self.items.all(i, v, v > 0)");
    assert_passes("self.items.exists(i, v, i == 1 && v == 1)");
    assert_passes("self.items.transformList(i, v, v * 2) == [6, 2, 4]");
    assert_fails_validation("self.items.all(i, v, i < 2)");
}

#[test]
fn upstream_list_functions_evaluate() {
    assert_passes("self.items.sort() == [1, 2, 3]");
    assert_passes("self.items.reverse() == [2, 1, 3]");
    assert_passes("self.items.slice(0, 2) == [3, 1]");
    assert_passes("[self.items, [4]].flatten() == [3, 1, 2, 4]");
    assert_passes("(self.items + self.items).distinct() == [3, 1, 2]");
    assert_passes("lists.range(3) == [0, 1, 2]");
}

/// kube-cel's own Kubernetes list functions (feature `lists`) stay alongside
/// the upstream ones.
#[test]
#[cfg(feature = "lists")]
fn kubernetes_list_functions_coexist() {
    assert_passes("!self.items.isSorted() && self.items.sum() == 6 && self.items.min() == 1");
}

/// Control: the single-argument comprehension is supported alongside the
/// two-variable form above; enabling the two-variable macros must not shadow it.
#[test]
fn one_arg_comprehension_is_supported() {
    let result = run_rule("self.items.all(x, x > 0)");
    assert!(
        result.is_ok(),
        "one-arg all() should be supported, got {result:?}"
    );
}

/// Control: a genuine runtime error in a *supported* rule stays an
/// `EvaluationError`, NOT `UnsupportedReference` — the two are kept distinct so
/// callers can tell a coverage gap apart from a real evaluation failure.
#[test]
fn runtime_error_is_not_an_unsupported_reference() {
    // int vs string comparison: compiles, errors at runtime (not undeclared).
    let errors = run_rule("self.items[0] > 'a'").unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::EvaluationError),
        "expected a genuine EvaluationError, got {errors:?}"
    );
    assert!(
        !errors.iter().any(|e| e.kind == ErrorKind::UnsupportedReference),
        "a real runtime error must not be classified as UnsupportedReference, got {errors:?}"
    );
}

/// A runtime evaluation failure preserves the underlying `cel` error in the
/// `ValidationError` cause chain (`std::error::Error::source()`), rather than
/// flattening it into the message string and dropping the typed cause.
#[test]
fn runtime_error_chains_its_cause() {
    use std::error::Error;
    let errors = run_rule("self.items[0] > 'a'").unwrap_err();
    let err = errors
        .iter()
        .find(|e| e.kind == ErrorKind::EvaluationError)
        .expect("expected an EvaluationError");
    assert!(
        err.source().is_some(),
        "EvaluationError should chain the underlying cel cause via source()"
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
    let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::SchemaTooDeep),
        "depth past the cap must fail closed with SchemaTooDeep, got {errors:?}"
    );
}

// ── messageExpression compile failure — fails CLOSED since S11; used to fail OPEN
// apiserver: rejects the CRD at registration when `messageExpression` does not
//            compile, so the type can never be created.
// kube-cel:  CompilationFailure (the rule's dynamic message cannot compile),
//            object rejected. Before S11 this was silently dropped and the rule
//            evaluated with the static message — a fail-open divergence.

#[test]
fn invalid_message_expression_fails_closed() {
    // The `rule` is valid and the object satisfies it; only the messageExpression
    // is broken, so the only possible error is the messageExpression compilation.
    let schema = json!({
        "type": "object",
        "properties": { "items": { "type": "array", "items": {"type": "integer"} } },
        "x-kubernetes-validations": [{
            "rule": "size(self.items) > 0",
            "message": "needs items",
            "messageExpression": "invalid >="
        }]
    });
    let object = json!({"items": [1, 2, 3]});
    let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::CompilationFailure),
        "a rule whose messageExpression fails to compile must fail closed, got {errors:?}"
    );
}

// ── additionalProperties map-key escaping — faithful since #8; used to fail OPEN
// apiserver: only declared `properties` (struct field names) are field-name
//            escaped; `additionalProperties` map keys pass through literally
//            (`staging/src/k8s.io/apiserver/pkg/cel/common/values.go` MapValue),
//            so `'a.b/c' in self.m` matches the literal user key and the object
//            is REJECTED.
// kube-cel:  before #8, every object key was escaped, so the instance key
//            `a.b/c` became `a__dot__b__slash__c`, `'a.b/c' in self.m` was always
//            false, and the object was ACCEPTED — a fail-OPEN divergence.
//
// Ground truth measured against a live kind cluster (k8s v1.36.1): applying a CR
// with `spec.m = {"a.b/c": "x"}` against this exact schema is rejected by the
// apiserver with the rule's message; a CR with a harmless key is accepted.

/// The map carries the forbidden literal key. apiserver rejects; kube-cel must
/// now reject too (`ValidationFailure`), not pass via an escaped key.
#[test]
fn additional_properties_map_key_fails_faithfully() {
    let schema = json!({
        "type": "object",
        "properties": {
            "m": { "type": "object", "additionalProperties": {"type": "string"} }
        },
        "x-kubernetes-validations": [{
            "rule": "!('a.b/c' in self.m)",
            "message": "key a.b/c is forbidden in m"
        }]
    });
    let object = json!({"m": {"a.b/c": "x"}});
    let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::ValidationFailure),
        "forbidden map key must be rejected (apiserver parity), got {errors:?}"
    );
}

/// Control: a harmless key satisfies the same rule, so the object is accepted —
/// proving the rejection above is about the key, not a blanket failure.
#[test]
fn additional_properties_harmless_map_key_passes() {
    let schema = json!({
        "type": "object",
        "properties": {
            "m": { "type": "object", "additionalProperties": {"type": "string"} }
        },
        "x-kubernetes-validations": [{
            "rule": "!('a.b/c' in self.m)",
            "message": "key a.b/c is forbidden in m"
        }]
    });
    let object = json!({"m": {"harmless": "x"}});
    assert!(
        Validator::new().validate(&schema, &object, None).is_ok(),
        "a harmless map key should pass the rule"
    );
}
