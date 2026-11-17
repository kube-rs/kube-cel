//! apiserver divergence map.
//!
//! Pins every known way the [`Validator`] verdict differs from the upstream
//! Kubernetes apiserver, with the direction of the divergence:
//!
//! - **fail-closed** — kube-cel reports an error where the apiserver would
//!   accept the object (a false positive; safe, but not faithful).
//! - **fail-open** — kube-cel accepts where the apiserver would reject (a false
//!   negative; dangerous). There should be **none** of these, with two
//!   documented exceptions: the errors only a type checker catches, which `cel`
//!   does not have yet (cel-rust#350), see [`no_type_checker_fails_open`]; and,
//!   until it is fixed before 0.9.0 (#14), rules on schema nodes the object does
//!   not reach, see [`unreached_schema_node_fails_open`]. The depth case used to
//!   be one and was fixed in S4 (P1-A), for objects that reach the depth.
//!
//! Each test locks the *current measured* behavior so a divergence cannot
//! change silently. The human-readable summary lives in the README
//! "apiserver divergence" table — keep the two in sync.
//!
//! Measured against `cel` 0.16, with ground truth from the apiserver's base
//! CEL environment (`k8s.io/apiserver` v0.36.1,
//! `environment.MustBaseEnvSet(1.36).NewExpressionsEnv()`). `cel` has no type
//! checker, so kube-cel walks each rule's AST at compile time and refuses a
//! call to a function the apiserver's base environment lacks (`cel.bind`,
//! `math.*`, `isIPv4`, `base64.*`, a global `min(..)`, ...) as
//! [`ErrorKind::CompilationFailure`], even in a branch evaluation never
//! reaches, as the apiserver does at registration.
#![cfg(feature = "validation")]

use kube_cel::{ErrorKind, ValidationErrors, Validator, VapEvaluator, VapExpression, compile_schema};
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

/// Asserts the expression is refused at compile time on every path: the
/// Validator (`validate` and `validate_compiled`) reports a
/// `CompilationFailure` for it as a CRD rule, and the VAP evaluator fails to
/// compile it. The apiserver rejects such a CRD or policy at registration.
fn assert_refused(rule: &str) {
    let errors = run_rule(rule).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::CompilationFailure),
        "{rule}: expected CompilationFailure from validate, got {errors:?}"
    );

    let schema = json!({
        "type": "object",
        "properties": { "items": { "type": "array", "items": {"type": "integer"} } },
        "x-kubernetes-validations": [ {"rule": rule} ]
    });
    let errors = Validator::new()
        .validate_compiled(&compile_schema(&schema), &json!({"items": [3, 1, 2]}), None)
        .unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::CompilationFailure),
        "{rule}: expected CompilationFailure from validate_compiled, got {errors:?}"
    );

    let compiled = VapEvaluator::builder()
        .build()
        .compile_expressions(&[VapExpression {
            expression: rule.into(),
            message: None,
            message_expression: None,
        }]);
    assert!(
        compiled[0].is_err(),
        "{rule}: expected the VAP expression to fail to compile"
    );
}

// ── Outside the apiserver's base CEL environment — all fail CLOSED ──────────
// apiserver: rejects the CRD or policy at registration ("undeclared reference",
//            or "found no matching overload" for a global `min`/`max`): its base
//            environment has no `ext.Bindings`, `ext.Math` or `ext.Encoders`, no
//            `jsonpatch` (a MutatingAdmissionPolicy library), none of kube-cel's
//            `isIPv4`/`isIPv6`/`isCIDRv4`/`isCIDRv6` conveniences, and `min`/`max`
//            only as list members.
// kube-cel:  CompilationFailure, from a compile-time walk of the whole AST, so a
//            call evaluation never reaches (`true || ..`, the body of a
//            comprehension over an empty list) is refused too. Before 0.9,
//            `math.*`, `base64.*`, `jsonpatch.*`, `isIPv4`-style functions and
//            global `min`/`max` passed client-side: fail-OPEN.

#[test]
fn cel_bind_fails_closed() {
    assert_refused("cel.bind(s, self.items.size(), s > 0)");
    assert_refused("true ? true : cel.bind(x, 2, x > 0)");
}

#[test]
fn math_library_fails_closed() {
    assert_refused("math.ceil(1.2) > 0.0");
    assert_refused("math.greatest(self.items) == 3");
    assert_refused("math.abs(self.items[0]) == 3");
    assert_refused("true || math.ceil(1.2) > 0.0");
}

#[test]
fn ip_version_helpers_fail_closed() {
    assert_refused("isIPv4('192.168.0.1')");
    assert_refused("isIPv6('::1')");
    assert_refused("isCIDRv4('10.0.0.0/8')");
    assert_refused("isCIDRv6('fd00::/8')");
    assert_refused("true || isIPv4('192.168.0.1')");
    assert_refused("true || .isIPv4('192.168.0.1')");
    assert_refused("self.items.filter(x, x > 5).all(x, isIPv4(string(x)))");
    assert_refused("[].all(i, v, isIPv4(v))");
}

#[test]
fn encoders_and_jsonpatch_fail_closed() {
    assert_refused("base64.encode(b'hello') == 'aGVsbG8='");
    assert_refused("true || base64.decode('aGVsbG8=') == b'hello'");
    assert_refused("jsonpatch.escapeKey('a/b') == 'a~1b'");
}

#[test]
fn global_min_max_fails_closed() {
    assert_refused("min(5, 3) == 3");
    assert_refused("max(self.items) == 3");
    assert_refused("true || min(1, 2) == 1");
    assert_refused(".min(1, 2) == 1");
}

/// A `messageExpression` goes through the same check, although it is only
/// evaluated when its rule fails: here the rule holds.
#[test]
fn unsupported_call_in_message_expression_fails_closed() {
    let schema = json!({
        "type": "object",
        "x-kubernetes-validations": [{
            "rule": "true",
            "messageExpression": "string(isIPv4('1.2.3.4'))"
        }]
    });
    let errors = Validator::new().validate(&schema, &json!({}), None).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::CompilationFailure),
        "expected CompilationFailure, got {errors:?}"
    );

    let compiled = VapEvaluator::builder()
        .build()
        .compile_expressions(&[VapExpression {
            expression: "true".into(),
            message: None,
            message_expression: Some("string(math.ceil(1.2))".into()),
        }]);
    assert!(
        compiled[0].is_err(),
        "expected the VAP messageExpression to fail to compile"
    );
}

/// The apiserver has no string `reverse()` (its `reverse` is the list one):
/// the CRD is rejected at registration ("found no matching overload"). kube-cel
/// has no matching overload at evaluation either, so the object is rejected, as
/// an `EvaluationError`; without the `lists` feature there is no `reverse` at
/// all and the rule does not compile.
#[test]
fn string_reverse_fails_closed() {
    let want = if cfg!(feature = "lists") {
        ErrorKind::EvaluationError
    } else {
        ErrorKind::CompilationFailure
    };
    let errors = run_rule("'abc'.reverse() == 'cba'").unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == want),
        "string reverse(): expected fail-closed {want:?}, got {errors:?}"
    );
}

/// The apiserver has `string(<IP>)` and `string(<CIDR>)` only: the member form
/// is rejected at registration ("found no matching overload"). kube-cel finds
/// no overload of that form at evaluation and rejects the object.
#[test]
#[cfg(feature = "ip")]
fn ip_member_string_fails_closed() {
    for rule in [
        "ip('10.0.0.1').string() == '10.0.0.1'",
        "cidr('10.0.0.0/8').string() == '10.0.0.0/8'",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().any(|e| e.kind == ErrorKind::UnsupportedReference),
            "{rule}: expected fail-closed UnsupportedReference, got {errors:?}"
        );
    }
}

// ── No type checker — fails OPEN (the one documented exception) ──────────────
// apiserver: type-checks every expression at registration, so a type, arity or
//            call-shape error, or an undeclared identifier, rejects the CRD
//            even in a branch that is never evaluated.
// kube-cel:  `cel` has no type checker yet (cel-rust#350), and kube-cel's
//            compile-time walk checks function names only. Such a rule passes
//            whenever evaluation does not trip over the error. Pinned so the
//            gap is visible, and so this test fails (and gets updated) once a
//            checker lands.

/// Asserts the rule passes, though the apiserver rejects it at registration.
fn assert_fails_open(rule: &str) {
    let result = run_rule(rule);
    assert!(
        result.is_ok(),
        "{rule}: expected the known fail-open (no type checker) to pass, got {result:?}"
    );
}

#[test]
fn no_type_checker_fails_open() {
    // Types: mixed-type list literals, and lists of different element types.
    assert_fails_open("[1, 2] == [1.0, 2.0]");
    #[cfg(feature = "lists")]
    assert_fails_open("[1, 2.0].isSorted()");
    // Arity.
    #[cfg(feature = "strings")]
    assert_fails_open("'a'.lowerAscii(42) == 'a'");
    // Call shape: the apiserver has `<IP>.family()`, not `family(<IP>)`, and
    // `string(<IP>)`, not `<IP>.string()` (rejected when evaluated, see
    // `ip_member_string_fails_closed`, but not when it is not).
    #[cfg(feature = "ip")]
    {
        assert_fails_open("family(ip('1.2.3.4')) == 4");
        assert_fails_open("true || ip('1.2.3.4').string() == ''");
    }
    // An undeclared identifier in a branch that is never evaluated.
    assert_fails_open("true || undeclared == 1");

    // A `messageExpression` must be a string; this one is only evaluated when
    // the rule fails, and the rule holds.
    let schema = json!({
        "type": "object",
        "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "1" }]
    });
    assert!(
        Validator::new().validate(&schema, &json!({}), None).is_ok(),
        "a non-string messageExpression is a known fail-open (no type checker)"
    );
}

// ── Schema nodes the object does not reach — fails OPEN (fix before 0.9.0) ───
// apiserver: compiles every rule (and `messageExpression`) in the schema, and
//            enforces the depth limit, when the CRD is registered, whatever the
//            objects later look like.
// kube-cel:  walks the object, so a rule on a node it does not reach (an
//            omitted property, the `items` of an empty list, a node past the
//            depth limit of a shallow object) is never compiled, and the object
//            passes. Pre-existing (not a cel 0.16 change); tracked in #14 as a
//            0.9.0 release blocker. Pinned so this test fails once it is fixed.

#[test]
fn unreached_schema_node_fails_open() {
    let absent = |validation: serde_json::Value| {
        json!({
            "type": "object",
            "properties": {
                "a": { "type": "string", "x-kubernetes-validations": [validation] },
                "l": { "type": "array", "items": { "type": "string", "x-kubernetes-validations": [validation] } }
            }
        })
    };
    for validation in [
        json!({"rule": "isIPv4(self)"}),
        json!({"rule": "self >"}),
        json!({"rule": "true", "messageExpression": "invalid >="}),
    ] {
        let schema = absent(validation.clone());
        assert!(
            Validator::new()
                .validate(&schema, &json!({"l": []}), None)
                .is_ok(),
            "{validation}: expected the known fail-open (unreached node) to pass"
        );
    }

    let mut deep = json!({"type": "integer", "x-kubernetes-validations": [{"rule": "self >= 0"}]});
    for _ in 0..70 {
        deep = json!({ "type": "object", "properties": { "c": deep } });
    }
    assert!(
        Validator::new().validate(&deep, &json!({}), None).is_ok(),
        "an over-deep schema with a shallow object is a known fail-open (unreached node)"
    );
}

// ── `lists` extension and two-variable comprehensions — faithful since 0.9 ──
// apiserver: evaluates (`ext.Lists` and `ext.TwoVarComprehensions` are in the
//            base environment).
// kube-cel:  evaluates too, through `cel`'s upstream extensions (`lists` with
//            the `lists` feature). Before 0.9 `sortBy` and the two-variable
//            macros surfaced as UnsupportedReference (fail-closed).

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
#[cfg(feature = "lists")]
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
#[cfg(feature = "lists")]
fn upstream_list_functions_evaluate() {
    assert_passes("self.items.sort() == [1, 2, 3]");
    assert_passes("self.items.reverse() == [2, 1, 3]");
    assert_passes("self.items.slice(0, 2) == [3, 1]");
    assert_passes("[self.items, [4]].flatten() == [3, 1, 2, 4]");
    assert_passes("(self.items + self.items).distinct() == [3, 1, 2]");
    assert_passes("lists.range(3) == [0, 1, 2]");
}

/// Without the `lists` feature the validation environment has no list
/// functions, upstream ones included: a rule using one does not compile.
#[test]
#[cfg(not(feature = "lists"))]
fn list_functions_need_the_lists_feature() {
    assert_refused("self.items.sortBy(x, x) == [1, 2, 3]");
    assert_refused("lists.range(3) == [0, 1, 2]");
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
