//! apiserver divergence map.
//!
//! Pins every known way the [`Validator`] verdict differs from the upstream
//! Kubernetes apiserver, with the direction of the divergence:
//!
//! - **fail-closed** — kube-cel reports an error where the apiserver would
//!   accept the object (a false positive; safe, but not faithful).
//! - **fail-open** — kube-cel accepts where the apiserver would reject (a false
//!   negative; dangerous). There should be **none** of these, with two
//!   documented exception classes, which stay until `cel` ships the missing
//!   piece: the errors only a type checker catches (cel-rust#350), see
//!   [`no_type_checker_fails_open`], and the rules only a cost model refuses
//!   (on `cel`'s roadmap, cel-rust discussion #230), see
//!   [`no_cost_model_fails_open`]. Besides those, until `cel` carries
//!   cel-rust#462 (before 0.9.0), a list or map `cel` builds compared with a
//!   schema-bound one, see [`cel_built_operand_until_cel_rust_462`], and until
//!   it carries cel-rust#460 (before 0.9.0), the timestamp getters without a
//!   time zone, see [`timestamp_getters_without_a_zone_until_cel_rust_460`].
//!   Rules on schema nodes the object does not reach used to be another, and
//!   `format()`'s output a third; since 0.9 every rule in the schema is
//!   compiled before the object is walked, and `format()` ports cel-go v0.26's
//!   formatter.
//!
//! Each test locks the *current measured* behavior so a divergence cannot
//! change silently. The human-readable summary lives in the README
//! "apiserver divergence" table — keep the two in sync.
//!
//! Measured against `cel` 0.16, with ground truth from the apiserver's base
//! CEL environment (`k8s.io/apiserver` v0.36.1,
//! `environment.MustBaseEnvSet(1.36).NewExpressionsEnv()`). `cel` has no type
//! checker, so kube-cel walks each rule's AST at compile time and refuses a
//! call to a function the apiserver does not declare (`cel.bind`, `math.*`,
//! `isIPv4`, `base64.*`, a global `min(..)`, ...), and an identifier not
//! declared where it appears, as [`ErrorKind::CompilationFailure`], even in a
//! branch evaluation never reaches, as the apiserver does at registration. A function the apiserver
//! declares but kube-cel does not implement compiles, and fails closed only
//! when evaluation reaches it, see
//! [`declared_unimplemented_functions_fail_closed_when_reached`].
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
    assert_rule_refused(rule);
    assert_policy_refused(rule);
}

/// Asserts the Validator (`validate` and `validate_compiled`) reports a
/// `CompilationFailure` for `rule` as a CRD rule.
fn assert_rule_refused(rule: &str) {
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
}

/// Asserts the VAP evaluator fails to compile `expression`.
fn assert_policy_refused(expression: &str) {
    let compiled = VapEvaluator::builder()
        .build()
        .compile_expressions(&[VapExpression {
            expression: expression.into(),
            message: None,
            message_expression: None,
        }]);
    assert!(
        compiled[0].is_err(),
        "{expression}: expected the VAP expression to fail to compile"
    );
}

// ── Outside the apiserver's base CEL environment — all fail CLOSED ──────────
// apiserver: rejects the CRD or policy at registration ("undeclared reference",
//            or "found no matching overload" for a global `min`/`max`): its base
//            environment has no `ext.Bindings`, `ext.Math` or `ext.Encoders`, no
//            `jsonpatch` (a MutatingAdmissionPolicy library), no
//            `isIPv4`/`isIPv6`/`isCIDRv4`/`isCIDRv6` (kube-cel's own before 0.9),
//            and `min`/`max` only as list members.
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

// ── Undeclared identifiers — all fail CLOSED ─────────────────────────────────
// apiserver: rejects the CRD or policy at registration ("undeclared reference
//            to 'x'") for an identifier not declared where it appears: a typo,
//            a macro's variable outside the macro, a policy variable in a CRD
//            rule (`authorizer`, `object`) or a CRD variable in a policy
//            (`self`), even in a branch evaluation never reaches.
// kube-cel:  CompilationFailure (a compile error for a policy), from the same
//            compile-time walk, which tracks what each comprehension binds.
//            Before 0.9 these passed client-side unless evaluation reached
//            them: fail-OPEN (filed under the no-type-checker class).

#[test]
fn undeclared_identifiers_fail_closed() {
    assert_rule_refused("self.items.size() > 0 || slef.x == 1");
    assert_rule_refused("true || undeclared == 1");
    assert_rule_refused("true || authorizer.group('').resource('pods').check('get').allowed()");
    assert_rule_refused("true || object.x == 1");
    assert_rule_refused("self.items.all(x, x > 0) && (true || x == 1)");
    assert_rule_refused("true || [].all(i, v, slef)");
    // kube-cel's `RootContext` bound these before 0.9; the apiserver
    // declares none of them.
    assert_rule_refused("true || apiVersion == 'v1'");
    assert_rule_refused("true || apiGroup == ''");
    assert_rule_refused("true || kind == 'Probe'");

    // The `messageExpression` of a rule that holds is checked too.
    let schema = json!({
        "type": "object",
        "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "'bad: ' + string(slef)" }]
    });
    let errors = Validator::new().validate(&schema, &json!({}), None).unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::CompilationFailure),
        "expected CompilationFailure, got {errors:?}"
    );

    assert_policy_refused("true || self.x == 1");
    assert_policy_refused("true || oldSelf.x == 1");
    assert_policy_refused("true || slef.x == 1");
    let compiled = VapEvaluator::builder()
        .build()
        .compile_expressions(&[VapExpression {
            expression: "true".into(),
            message: None,
            message_expression: Some("'bad: ' + string(slef)".into()),
        }]);
    assert!(
        compiled[0].is_err(),
        "expected the VAP messageExpression to fail to compile"
    );
}

/// `.x` naming a macro's variable `x`: cel-go resolves it to the macro's
/// variable, `cel` looks it up outside the macros, where it may be another
/// value altogether: `[dyn(1)].all(namespaceObject, .namespaceObject == null)`
/// is false on v1.36.1 (the macro's `1`), while kube-cel's policy
/// `namespaceObject` is null. kube-cel refuses such a reference at compile
/// time, so a rule the apiserver registers and evaluates is rejected.
#[test]
fn dotted_macro_variable_fails_closed() {
    assert_rule_refused("self.items.all(x, .x > 0)");
    assert_rule_refused("true || [1].all(self, .self > 0)");
    assert_policy_refused("[dyn(1)].all(namespaceObject, .namespaceObject == null)");
    let compiled = VapEvaluator::builder()
        .has_param_kind(true)
        .build()
        .compile_expressions(&[VapExpression {
            expression: "[dyn(1)].all(params, .params == null)".into(),
            message: None,
            message_expression: None,
        }]);
    assert!(
        compiled[0].is_err(),
        "`.params` naming a macro's variable must not compile"
    );
}

/// `.self` and `.oldSelf` in a CRD rule: the leading dot names the rule's
/// `self` / `oldSelf`, and the apiserver evaluates `.self.kind == self.kind`
/// to true (Go oracle, v0.36.1). kube-cel binds them per schema node, in an
/// inner scope of its shared root context, and `cel` looks a leading-dot name
/// up at the root only, so evaluation reaching one fails with
/// `UnsupportedReference`. (A policy's variables are bound at the root, so
/// `.object` evaluates.)
#[test]
fn dotted_self_fails_closed() {
    let errors = run_rule(".self.items.size() == 3").unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::UnsupportedReference),
        ".self: {errors:?}"
    );
    let schema = json!({
        "type": "object",
        "properties": { "n": { "type": "integer" } },
        "x-kubernetes-validations": [{ "rule": ".oldSelf.n <= self.n" }]
    });
    let errors = Validator::new()
        .validate(&schema, &json!({"n": 2}), Some(&json!({"n": 1})))
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::UnsupportedReference),
        ".oldSelf: {errors:?}"
    );
    let results = VapEvaluator::builder()
        .object(json!({"x": 1}))
        .build()
        .evaluate(&[VapExpression {
            expression: ".object.x == object.x".into(),
            message: None,
            message_expression: None,
        }]);
    assert!(results[0].passed, ".object evaluates in a policy");
}

/// `==` between a list or object of the schema and a value of another kind
/// (`dyn` gets it past the type checker), or between an object holding fields
/// its schema does not declare and a map that is not of its kind, is the
/// apiserver's no-such-overload error, and cel-go's `!=` is
/// `Equal(..) != true`, so there `!=` holds (Go oracle, kind v1.36.1). `cel`'s
/// equality can only answer true or false, so kube-cel refuses both.
#[test]
fn equality_with_another_kind_fails_closed() {
    let schema = json!({
        "type": "object",
        "properties": {
            "o": { "type": "object", "properties": { "x": { "type": "integer" } } },
            "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "string" } }
        },
        "x-kubernetes-validations": [
            { "rule": "self.o != dyn([1])" },
            { "rule": "self.s != dyn({'a': 1})" }
        ]
    });
    let object = json!({ "o": { "x": 1 }, "s": ["a"] });
    let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
    assert_eq!(errors.len(), 2, "{errors:?}");
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// A two-variable comprehension over an object holding fields its schema
/// does not declare: the apiserver iterates an undeclared key as `null` and
/// binds its value as an error, which a body that ignores it never reads
/// (`all(k, v, true)` holds; Go oracle, kind v1.36.1). `cel` looks the value
/// up as it binds it, and the lookup of `null` fails the comprehension.
#[test]
fn two_variable_comprehension_over_undeclared_fields_fails_closed() {
    let schema = json!({
        "type": "object",
        "properties": { "o": {
            "type": "object",
            "properties": { "x": { "type": "integer" } },
            "x-kubernetes-preserve-unknown-fields": true
        } },
        "x-kubernetes-validations": [{ "rule": "dyn(self.o).all(k, v, true)" }]
    });
    let errors = Validator::new()
        .validate(&schema, &json!({ "o": { "x": 1, "z": 2 } }), None)
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// The apiserver's set `+` keeps state on its receiver: it adds the new keys
/// to the receiver's cached set, which a later `+` or `==` on that receiver
/// reads, and appends into the receiver's slice, whose spare capacity
/// another `+` on the same elements may share (Go oracle, kind v1.36.1).
/// kube-cel computes each `+` afresh and refuses the rule where that state
/// could change the outcome, though here the apiserver accepts.
#[test]
fn set_add_state_fails_closed() {
    let schema = |rule: &str| {
        json!({
            "type": "object",
            "properties": { "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } } },
            "x-kubernetes-validations": [{ "rule": rule }]
        })
    };
    for rule in [
        "[self.s].all(x, (x + [3]).size() == 3 && x == [1, 3])",
        "(self.s + [3]).size() == 3 && (self.s + [4]).size() == 3",
    ] {
        let errors = Validator::new()
            .validate(&schema(rule), &json!({ "s": [1, 2] }), None)
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    // Across the rules of a set node, which share its `self` (one
    // activation): the apiserver's `==` finds the 3 the first rule cached.
    let node = json!({
        "type": "object",
        "properties": { "s": {
            "type": "array",
            "x-kubernetes-list-type": "set",
            "items": { "type": "integer" },
            "x-kubernetes-validations": [
                { "rule": "(self + [3]).size() == 3" },
                { "rule": "self == [1, 3]" }
            ]
        } }
    });
    let errors = Validator::new()
        .validate(&node, &json!({ "s": [1, 2] }), None)
        .unwrap_err();
    assert!(
        errors.len() == 1 && errors[0].kind == ErrorKind::EvaluationError,
        "{errors:?}"
    );
}

/// `+` onto a set or map list binds each added item under the left list's
/// items from its Go value, and a map list's `==` keys each right-hand item
/// by it. kube-cel holds an `additionalProperties` map as a plain map, whose
/// Go value (its decoded map) it cannot tell from a CEL-built map's, so the
/// rule is refused; the apiserver binds or keys the decoded map
/// (`!has(..x)` and `self.m == dyn(self.r)` hold; Go oracle, kind v1.36.1).
#[test]
fn plain_map_against_a_map_list_fails_closed() {
    let schema = json!({
        "type": "object",
        "properties": {
            "l": {
                "type": "array",
                "x-kubernetes-list-type": "map",
                "x-kubernetes-list-map-keys": ["k"],
                "items": { "type": "object", "properties": { "k": { "type": "string" }, "x": { "type": "integer", "nullable": true } } }
            },
            "r": { "type": "array", "items": { "type": "object", "additionalProperties": { "x-kubernetes-int-or-string": true, "nullable": true } } }
        },
        "x-kubernetes-validations": [{ "rule": "!has((self.l + dyn(self.r))[0].x)" }]
    });
    let errors = Validator::new()
        .validate(&schema, &json!({ "l": [], "r": [{ "k": "a", "x": null }] }), None)
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
    // A typeless preserve-unknown value against a plain list field: the
    // apiserver compares the field's decoded list (`dyn(self).u == dyn(self.a)`
    // holds), which kube-cel cannot tell from a CEL-built list.
    let preserved = json!({
        "type": "object",
        "properties": {
            "u": { "x-kubernetes-preserve-unknown-fields": true },
            "a": { "type": "array", "items": { "type": "integer" } }
        },
        "x-kubernetes-validations": [{ "rule": "dyn(self).u == dyn(self.a)" }]
    });
    let errors = Validator::new()
        .validate(&preserved, &json!({ "u": [1], "a": [1] }), None)
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
    let mut equal = schema.clone();
    equal["x-kubernetes-validations"] = json!([{ "rule": "self.l == dyn(self.r)" }]);
    let errors = Validator::new()
        .validate(
            &equal,
            &json!({ "l": [{ "k": "a", "x": 1 }], "r": [{ "k": "a", "x": 1 }] }),
            None,
        )
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// With four `x-kubernetes-list-map-keys` or more, the apiserver keys a map
/// list by Go's `%v` of the key tuple; kube-cel prints strings, integers,
/// booleans and nil as Go does, but not a float, so a map-list `==` or `+`
/// reaching one is refused (the apiserver evaluates it).
#[test]
fn map_list_four_keys_with_a_float_fail_closed() {
    let s = json!({ "type": "string" });
    let schema = json!({
        "type": "object",
        "properties": { "m": {
            "type": "array",
            "x-kubernetes-list-type": "map",
            "x-kubernetes-list-map-keys": ["a", "b", "c", "d"],
            "items": { "type": "object", "properties": { "a": s, "b": s, "c": s, "d": { "type": "number" } } }
        } },
        "x-kubernetes-validations": [{ "rule": "self.m == self.m" }]
    });
    let object = json!({ "m": [{ "a": "a", "b": "b", "c": "c", "d": 1.5 }] });
    let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// A list or map `cel` builds (a literal, a macro's result, a plain list
/// field) compares unequal to any list or map of another type, so on the
/// left of `==` / `!=` it never equals a set list, map list or object, which
/// the apiserver compares by position or key (cel-go `baseList.Equal`,
/// `baseMap.Equal`); and `cel`'s `in` asks the list's element, where cel-go's
/// `baseList.Contains` asks the searched value. cel-rust#462 fixes both (the
/// `in` direction included). These assertions pin today's verdicts, each the
/// apiserver's opposite (Go oracle, kind v1.36.1), so they fail once `cel`
/// carries the fix: then drop this test and its README row, before 0.9.0.
/// kube-cel builds no workaround.
///
/// One more waits on the same fix: `+` onto a plain list field binds what it
/// adds under the list's items on the apiserver, while kube-cel binds a plain
/// list as `cel`'s own list (a schema-aware one would break `[..] == self.l`
/// until cel-rust#462), so an added value keeps its own binding. That
/// assertion flips once kube-cel binds plain lists as schema-aware ones, the
/// work planned for after the `cel` bump.
#[test]
fn cel_built_operand_until_cel_rust_462() {
    let schema = |rule: &str| {
        json!({
            "type": "object",
            "properties": {
                "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "string" } },
                "a": { "type": "array", "items": { "type": "string" } },
                "m": {
                    "type": "array",
                    "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["k"],
                    "items": { "type": "object", "properties": { "k": { "type": "string" } } }
                },
                "o": { "type": "object", "properties": { "y": { "type": "integer" } } },
                "l": { "type": "array", "items": { "type": "number" } },
                "u": { "x-kubernetes-preserve-unknown-fields": true }
            },
            "x-kubernetes-validations": [{ "rule": rule }]
        })
    };
    let object = json!({
        "s": ["b", "a"],
        "a": ["b", "a"],
        "m": [{ "k": "b" }, { "k": "a" }],
        "o": { "y": 1 },
        "l": [],
        "u": "x"
    });
    // (rule, whether the apiserver accepts the object)
    for (rule, apiserver_accepts) in [
        // Fail-open today.
        ("['b', 'a'] != self.s", false),
        ("self.a != self.s", false),
        ("self.s.filter(x, true) != self.s", false),
        ("self.m.map(x, x) != self.m", false),
        ("dyn({'y': 1}) != dyn(self.o)", false),
        ("['a', 'b'] in [self.s]", false),
        ("'x' in [dyn(self).u]", false),
        // Fail-closed today.
        ("['b', 'a'] == self.s", true),
        ("self.s in [['a', 'b']]", true),
    ] {
        let kube_cel_accepts = Validator::new().validate(&schema(rule), &object, None).is_ok();
        assert_eq!(
            kube_cel_accepts, !apiserver_accepts,
            "{rule}: no longer diverges; if `cel` now carries cel-rust#462, drop this test and its README row"
        );
    }
    // The added int is a double on the apiserver, an int in kube-cel today.
    let rule = "type((self.l + dyn([1]))[0]) == int";
    assert!(
        Validator::new().validate(&schema(rule), &object, None).is_ok(),
        "{rule}: no longer diverges; if plain lists now bind as schema-aware lists, drop this assertion and its part of the README row"
    );
}

/// `+` stores what it adds, and the apiserver binds an element under the
/// list's items when it reads it: one they refuse is an error value then,
/// which a comprehension's body may not read (`all(x, true)` holds; Go
/// oracle, kind v1.36.1). kube-cel binds it lazily too, so `size()` and
/// `|| true` agree, but `cel` iterates values, not errors, so iterating it is
/// refused.
#[test]
fn iterating_a_refused_added_element_fails_closed() {
    let schema = |rule: &str| {
        json!({
            "type": "object",
            "properties": { "s": { "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } } },
            "x-kubernetes-validations": [{ "rule": rule }]
        })
    };
    let object = json!({ "s": [] });
    let validator = Validator::new();
    assert!(
        validator
            .validate(
                &schema(
                    "(self.s + dyn(['a'])).size() == 1 && ((self.s + dyn(['a']))[0] == dyn('a') || true)"
                ),
                &object,
                None
            )
            .is_ok()
    );
    let errors = validator
        .validate(&schema("(self.s + dyn(['a'])).all(x, true)"), &object, None)
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// An element `+` added binds lazily, and so does each field of it, but a
/// list or map field binds its values at once: one value its schema refuses
/// makes the whole field an error where read, while the apiserver binds the
/// field's values only as they are read (`x.size() == 2` holds; Go oracle,
/// kind v1.36.1).
#[test]
fn nested_field_of_an_added_element_fails_closed() {
    let map_of = |x: serde_json::Value| {
        json!({
            "type": "array",
            "x-kubernetes-list-type": "map",
            "x-kubernetes-list-map-keys": ["k"],
            "items": { "type": "object", "properties": { "k": { "type": "string" }, "x": x } }
        })
    };
    let schema = json!({
        "type": "object",
        "properties": {
            "l": map_of(json!({ "type": "array", "items": { "type": "integer" } })),
            "r": map_of(json!({ "type": "array", "items": { "x-kubernetes-int-or-string": true } }))
        },
        "x-kubernetes-validations": [{ "rule": "(self.l + dyn(self.r))[0].x.size() == 2" }]
    });
    let errors = Validator::new()
        .validate(
            &schema,
            &json!({ "l": [], "r": [{ "k": "a", "x": ["a", 1] }] }),
            None,
        )
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// The apiserver's `unknownPreserved` fails every operation but `==` and a
/// field test, and `|| true` absorbs the error (Go oracle, kind v1.36.1).
/// `cel` reads the value's type for `type()`, which cannot fail, and for
/// overload lookup and error messages too; kube-cel refuses every such read,
/// so the absorbed forms are refused, but for indexing, which errors
/// without reading the type.
#[test]
fn absorbed_operation_on_a_preserved_value_fails_closed() {
    let schema = |rule: &str| {
        json!({
            "type": "object",
            "properties": { "u": { "x-kubernetes-preserve-unknown-fields": true } },
            "x-kubernetes-validations": [{ "rule": rule }]
        })
    };
    let object = json!({ "u": "x" });
    let validator = Validator::new();
    for rule in [
        "type(dyn(self).u) != string || true",
        "(dyn(self).u.size() == 1) || true",
        "(dyn(self).u + 1 == 2) || true",
    ] {
        let errors = validator.validate(&schema(rule), &object, None).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    assert!(
        validator
            .validate(&schema("dyn(self).u[0] == 1 || true"), &object, None)
            .is_ok()
    );
}

/// A set held in a list or map is bound afresh on each `Get` / `Find`, so a
/// variable holding the whole container carries no `+` state between two
/// reads of the set (Go oracle, kind v1.36.1). `cel` copies the set when it
/// copies its container, through the same `clone_as_boxed` as when a
/// variable holds the set itself (`[self.l[0]]`, where the state does
/// carry), so kube-cel keeps one wrapper and refuses the second `+`.
#[test]
fn set_in_a_held_container_fails_closed() {
    let int_set = json!({ "type": "array", "x-kubernetes-list-type": "set", "items": { "type": "integer" } });
    let schema = |rule: &str| {
        json!({
            "type": "object",
            "properties": {
                "l": { "type": "array", "items": int_set },
                "d": { "type": "object", "additionalProperties": int_set }
            },
            "x-kubernetes-validations": [{ "rule": rule }]
        })
    };
    let object = json!({ "l": [[1, 2]], "d": { "k": [1, 2] } });
    for rule in [
        "[self.l].all(x, (x[0] + [3]).size() == 3 && (x[0] + [3]).size() == 3)",
        "[self.d].all(x, (x['k'] + [3]).size() == 3 && (x['k'] + [3]).size() == 3)",
    ] {
        let errors = Validator::new()
            .validate(&schema(rule), &object, None)
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
}

/// A presence test on a scalar: the apiserver's cel-go answers `has()` with
/// false and `.?` with none (Go oracle, kind v1.36.1), while `cel` refuses
/// it, as cel-spec has it. An `optionalOldSelf` rule's `oldSelf.value()`
/// holds a typeless preserve-unknown value as a plain scalar, so it reaches
/// this too.
#[test]
fn presence_test_on_a_scalar_fails_closed() {
    let schema = |props: serde_json::Value, rule: serde_json::Value| json!({ "type": "object", "properties": props, "x-kubernetes-validations": [rule] });
    let string = json!({ "s": { "type": "string" } });
    for rule in ["!has(dyn(self.s).a)", "!dyn(self.s).?a.hasValue()"] {
        let errors = Validator::new()
            .validate(
                &schema(string.clone(), json!({ "rule": rule })),
                &json!({ "s": "x" }),
                None,
            )
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    let preserved = schema(
        json!({ "u": { "x-kubernetes-preserve-unknown-fields": true } }),
        json!({ "rule": "!oldSelf.hasValue() || !has(dyn(oldSelf.value()).u.a)", "optionalOldSelf": true }),
    );
    let errors = Validator::new()
        .validate(&preserved, &json!({ "u": "x" }), Some(&json!({ "u": "x" })))
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
        "{errors:?}"
    );
}

/// `variables` is declared for a policy with `spec.variables` only
/// (`undeclared reference to 'variables'` without, checked on v1.36.1).
/// kube-cel does not model `spec.variables`, so it refuses every expression
/// naming `variables`, also in a policy that defines it, which the apiserver
/// registers and evaluates.
#[test]
fn policy_variables_fail_closed() {
    assert_policy_refused("true || variables.foo == 1");
    assert_policy_refused("variables.foo == 1");
}

/// Names the apiserver declares and evaluates but `cel` cannot: type names
/// with no `cel` value (`net.IP`, `google.protobuf.Empty`), the protobuf
/// `NullValue` constant and a message construction. Each evaluates to true on
/// the apiserver (Go oracle, v0.36.1). They compile in kube-cel, as the
/// apiserver registers them, and fail closed when evaluation reaches them.
#[test]
#[cfg(feature = "ip")]
fn names_cel_cannot_evaluate_fail_closed() {
    for (rule, want) in [
        ("type(ip('1.2.3.4')) == net.IP", ErrorKind::UnsupportedReference),
        (
            "type(cidr('1.2.3.0/24')) == net.CIDR",
            ErrorKind::UnsupportedReference,
        ),
        (
            "[google.protobuf.Empty].size() == 1",
            ErrorKind::UnsupportedReference,
        ),
        (
            "google.protobuf.NullValue.NULL_VALUE == 0",
            ErrorKind::UnsupportedReference,
        ),
        (
            "google.protobuf.Duration{seconds: 1} == duration('1s')",
            ErrorKind::EvaluationError,
        ),
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == want),
            "{rule}: expected fail-closed {want:?}, got {errors:?}"
        );
    }
}

/// `x-kubernetes-validations` under `allOf`, `anyOf`, `oneOf` or `not`, at any
/// depth below one: the apiserver refuses the CRD as not structural ("must be
/// empty to be structural", parity rows `Pcomb*`). kube-cel rejects every
/// object with a root `CompilationFailure`; before 0.9 it evaluated the rules
/// under the first three and ignored those under `not`, so an object
/// satisfying them passed (fail-open).
#[test]
fn rules_under_combinators_fail_closed() {
    for keyword in ["allOf", "anyOf", "oneOf"] {
        let schema = json!({
            "type": "object",
            "properties": { "items": { "type": "array", "items": {"type": "integer"} } },
            keyword: [{ "x-kubernetes-validations": [{ "rule": "self.items.size() > 0" }] }]
        });
        let errors = Validator::new()
            .validate(&schema, &json!({"items": [3, 1, 2]}), None)
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::CompilationFailure),
            "{keyword}: {errors:?}"
        );
    }
    let schema = json!({
        "type": "object",
        "not": { "properties": { "items": { "x-kubernetes-validations": [{ "rule": "true" }] } } }
    });
    let errors = Validator::new()
        .validate_compiled(&compile_schema(&schema), &json!({}), None)
        .unwrap_err();
    assert!(
        errors.iter().all(|e| e.kind == ErrorKind::CompilationFailure),
        "not: {errors:?}"
    );
}

/// Rule fields the apiserver refuses at registration beside the expressions
/// (parity rows `PruleField*` and `vap-message-*`): a blank or multi-line
/// static message, a multi-line rule without a message, an unsupported
/// `reason`, a `fieldPath` naming no field of the rule's node. kube-cel
/// rejects every object (`InvalidRule`), or fails to compile the policy
/// expression; before 0.9 it accepted them.
#[test]
fn refused_rule_fields_fail_closed() {
    for rule in [
        json!({ "rule": "true", "message": "   " }),
        json!({ "rule": "true", "message": "a\nb" }),
        json!({ "rule": "true &&\ntrue" }),
        json!({ "rule": "true", "reason": "FieldValueTooLong" }),
        json!({ "rule": "true", "fieldPath": ".nope" }),
    ] {
        let schema = json!({
            "type": "object",
            "properties": { "items": { "type": "array", "items": {"type": "integer"} } },
            "x-kubernetes-validations": [rule.clone()]
        });
        let errors = Validator::new()
            .validate(&schema, &json!({"items": [3, 1, 2]}), None)
            .unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::InvalidRule),
            "{rule}: {errors:?}"
        );
    }
    let compiled = VapEvaluator::builder()
        .build()
        .compile_expressions(&[VapExpression {
            expression: "true".into(),
            message: Some("   ".into()),
            message_expression: None,
        }]);
    assert!(compiled[0].is_err(), "a blank static message must not compile");
}

/// The apiserver has no string `reverse()` (its `reverse` is the list one):
/// the CRD is rejected at registration ("found no matching overload"). kube-cel
/// has no matching overload at evaluation either, so the object is rejected, as
/// an `EvaluationError`; without the `lists` feature kube-cel has no `reverse`
/// at all, an `UnsupportedReference`.
#[test]
fn string_reverse_fails_closed() {
    let want = if cfg!(feature = "lists") {
        ErrorKind::EvaluationError
    } else {
        ErrorKind::UnsupportedReference
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

// ── Declared by the apiserver, not implemented by kube-cel — fails CLOSED ────
// apiserver: declares `optional.unwrap`, `<list>.unwrapOpt()` and the
//            `authorizer` library (all three envs: CRD rules, their
//            messageExpressions, policies), and evaluates them.
// kube-cel:  the compile-time name check accepts every name the apiserver
//            declares (src/validation/apiserver_functions.txt), so the CRD or
//            policy compiles and a call evaluation does not reach passes, as on
//            the apiserver. Reaching the call, or a policy's `authorizer`
//            (which kube-cel does not bind), rejects the object with
//            UnsupportedReference, even where CEL absorbs the error: the
//            apiserver has a value there, and an absorbed error could skip a
//            quantity update it makes. The
//            unreached cases are parity rows (PoptUnwrapShort, ..).

#[test]
fn declared_unimplemented_functions_fail_closed_when_reached() {
    // Not reached: accepted, as by the apiserver.
    assert_passes("true || optional.unwrap([optional.of(1)]) == [1]");
    assert_passes("true || [optional.of(1)].unwrapOpt() == [1]");
    let schema = json!({
        "type": "object",
        "x-kubernetes-validations": [{
            "rule": "true",
            "messageExpression": "optional.unwrap([optional.of('x')])[0] + ' rejected'"
        }]
    });
    assert!(
        Validator::new().validate(&schema, &json!({}), None).is_ok()
            && Validator::new()
                .validate_compiled(&compile_schema(&schema), &json!({}), None)
                .is_ok(),
        "optional.unwrap in the messageExpression of a rule that holds must compile"
    );

    // Reached: the apiserver evaluates these to true; kube-cel rejects.
    for rule in [
        "optional.unwrap([optional.of(1), optional.none()]) == [1]",
        "[optional.of(1), optional.none()].unwrapOpt() == [1]",
        "optional.unwrap([optional.of(1)]) == [2] || true",
        "[1].exists(x, size([optional.of(x)].unwrapOpt()) == 2 || x == 1)",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().any(|e| e.kind == ErrorKind::UnsupportedReference),
            "{rule}: expected fail-closed UnsupportedReference, got {errors:?}"
        );
    }

    // Policies: the `authorizer` chain compiles; reached, it fails closed.
    let evaluator = VapEvaluator::builder().build();
    let expr = |expression: &str, message_expression: Option<&str>| VapExpression {
        expression: expression.into(),
        message: None,
        message_expression: message_expression.map(Into::into),
    };
    let exprs = [
        expr(
            "true || authorizer.group('').resource('pods').check('get').allowed()",
            None,
        ),
        expr(
            "true",
            Some("string([optional.of(1), optional.none()].unwrapOpt()[0])"),
        ),
        expr("authorizer.path('/healthz').check('get').allowed()", None),
        // Absorbed, the reached `authorizer` still fails: the apiserver has a
        // value there (an Authorizer), and in the last the absorbed error
        // would otherwise skip the quantity update (false on the apiserver).
        expr("type(authorizer) == type(authorizer) || true", None),
        expr("authorizer.requestResource.check('get').allowed() || true", None),
        expr(
            "[quantity('1')].all(q, ((type(authorizer.group('')) == type(authorizer.group('')) ? \
             q.compareTo(quantity('1.5Gi')) : 0) == 0 || true) && q.isInteger())",
            None,
        ),
        // A comprehension variable of that name is no policy variable.
        expr("[1].all(authorizer, authorizer == 1)", None),
    ];
    let compiled = evaluator.compile_expressions(&exprs);
    assert!(
        compiled.iter().all(Result::is_ok),
        "policy expressions calling declared functions must compile"
    );
    let passed: Vec<bool> = evaluator
        .evaluate_compiled(&compiled)
        .iter()
        .map(|r| r.passed)
        .collect();
    assert_eq!(
        passed,
        [true, true, false, false, false, false, true],
        "the reached authorizer fails closed, absorbed or not"
    );
}

// ── No type checker — fails OPEN (a documented exception class) ──────────────
// apiserver: type-checks every expression at registration, so a type, arity or
//            call-shape error rejects the CRD even in a branch that is never
//            evaluated.
// kube-cel:  `cel` has no type checker yet (cel-rust#350), and kube-cel's
//            compile-time walk checks names only. Such a rule passes
//            whenever evaluation does not trip over the error. Pinned so the
//            gap is visible, and so this test fails (and gets updated) once a
//            checker lands.

/// Asserts the rule passes, though the apiserver rejects it at registration.
fn assert_fails_open(rule: &str) {
    let result = run_rule(rule);
    assert!(
        result.is_ok(),
        "{rule}: expected the documented fail-open to pass, got {result:?}"
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
    // A literal `format()` call: the apiserver's string-format validator,
    // which runs after the type check, refuses more arguments than clauses and
    // an argument a clause cannot take. At runtime cel-go ignores the extra
    // argument, as kube-cel does.
    #[cfg(feature = "format")]
    {
        assert_fails_open("'%s'.format([1, 2]) == '1'");
        assert_fails_open("true || '%d'.format(['a']) == ''");
    }
    // Call shape: `cel` has a one-variable `existsOne(x, p)` macro; the
    // apiserver has only `existsOne(i, v, p)`, so it reads the one-variable
    // form as a call to an undeclared function ("undeclared reference to
    // 'existsOne'"). It passes even when evaluated.
    assert_fails_open("true || [1].existsOne(x, x > 0)");
    assert_fails_open("[1].existsOne(x, x > 0)");
    // A field the schema does not declare ("undefined field 'undeclared'").
    assert_fails_open("true || self.undeclared == 1");

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

// ── No cost model — fails OPEN (a documented exception class) ────────────────
// apiserver: estimates each rule's cost at registration and rejects the CRD
//            past the budget ("estimated rule cost exceeds budget": a list
//            with no `maxItems` iterated twice), and stops an evaluation past
//            the per-expression cost limit, rejecting the object ("actual cost
//            limit exceeded"). Both checked on v1.36.1.
// kube-cel:  `cel` has no cost model yet (on its roadmap, cel-rust discussion
//            #230), so these rules evaluate, and pass. Pinned so the gap is
//            visible, and so this test fails (and gets updated) once one
//            lands. `lists.range` refuses more than 1,000,000 elements, where
//            the apiserver's cost limit stops it already.

#[test]
fn no_cost_model_fails_open() {
    // Estimated cost, refused at registration (parity row `PcostEstimate`).
    let schema = json!({
        "type": "object",
        "properties": { "l": { "type": "array", "items": { "type": "string" } } },
        "x-kubernetes-validations": [{ "rule": "self.l.all(x, self.l.exists(y, x == y))" }]
    });
    let result = Validator::new().validate(&schema, &json!({"l": ["a", "b"]}), None);
    assert!(
        result.is_ok(),
        "the estimated-cost rule is a documented fail-open, got {result:?}"
    );

    // Runtime cost, the object refused at evaluation.
    #[cfg(feature = "lists")]
    {
        assert_fails_open("lists.range(1000000).size() > 0");
        assert_fails_open("lists.range(600000).map(x, x).size() > 0");
    }
}

// ── `lists` extension and two-variable comprehensions — faithful since 0.9 ──
// apiserver: evaluates (`ext.Lists` and `ext.TwoVarComprehensions` are in the
//            base environment).
// kube-cel:  evaluates too: its port of `ext.Lists` (feature `lists`), which
//            orders equal keys as Go's `sort.Slice`, makes `[]` of
//            `lists.range(-1)` and sorts a NaN, and `cel`'s two-variable
//            comprehensions. Before 0.9 `sortBy` and the two-variable macros
//            surfaced as UnsupportedReference (fail-closed).

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
/// functions, upstream ones included. `sortBy` is a macro, not a declared
/// function, so without the feature it parses as a call to an undeclared
/// function and does not compile. `lists.range` is declared, so it compiles
/// and the object is rejected when evaluation reaches it.
#[test]
#[cfg(not(feature = "lists"))]
fn list_functions_need_the_lists_feature() {
    assert_refused("self.items.sortBy(x, x) == [1, 2, 3]");
    let errors = run_rule("lists.range(3) == [0, 1, 2]").unwrap_err();
    assert!(
        errors.iter().any(|e| e.kind == ErrorKind::UnsupportedReference),
        "lists.range without the lists feature: expected UnsupportedReference, got {errors:?}"
    );
}

/// kube-cel's Kubernetes list functions (feature `lists`) stay alongside the
/// `ext.Lists` ones.
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

// ── Quantity arithmetic past i128 — fails CLOSED ───────────────────────────
// apiserver: quantities are exact `inf.Dec`, so
//            `quantity('2e38').add(quantity('1')).isGreaterThan(quantity('2e38'))`
//            holds (k8s.io/apiserver v0.36.1 `MustBaseEnvSet(1.36)`).
// kube-cel:  a quantity is an i128 mantissa and a scale; `add` / `sub` whose
//            result does not fit is an EvaluationError instead of a panic or
//            a wrapped value, even where CEL absorbs the error (`|| true`):
//            the apiserver has written the result into a receiver holding an
//            inf.Dec, which kube-cel left as it was.

#[test]
#[cfg(feature = "quantity")]
fn quantity_arithmetic_past_i128_fails_closed() {
    for rule in [
        "quantity('2e38').add(quantity('1')).isGreaterThan(quantity('2e38'))",
        "quantity('2e38').sub(quantity('-1')).isGreaterThan(quantity('2e38'))",
        "quantity('2e38').add(quantity('1')).isGreaterThan(quantity('2e38')) || true",
        "[quantity('-170141183460469231731687303715884105728')].all(q, (q.sub(1).isInteger() || true) && q.compareTo(quantity('-170141183460469231731687303715884105728')) == 0)",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
}

// ── Quantities past kube-cel's supported range — fail CLOSED ───────────────
// apiserver: reads a quantity's exponent as an int64 and narrows it to int32
//            (`quantity('1e4294967296') == quantity('1')` holds), parses
//            `1e1001` and quantities whose mantissa is past `i128`, and
//            near the int32 bounds computes a power of ten of hundreds of MiB
//            (`0.1e-2147483647`) or panics (k8s.io/apiserver v0.36.1
//            `MustBaseEnvSet(1.36)`).
// kube-cel:  supports exponents and scales within ±1000 and a mantissa
//            within `i128`. A rule or VAP expression that reaches a quantity past
//            them fails with an EvaluationError, even where CEL absorbs the
//            function's error (`|| true`, `exists`, an optional); one that
//            never calls it (`true || ..`) is unaffected, and a malformed
//            string is still not a quantity (`isQuantity` false), as is a
//            number without digits on the apiserver's inf.Dec path (`e-1001`).

#[test]
#[cfg(feature = "quantity")]
fn quantity_past_supported_range_fails_closed() {
    for rule in [
        "isQuantity('1e1001')",
        "isQuantity('10e1000')",
        "quantity('5e1000').add(quantity('5e1000')).compareTo(quantity('1e1000')) > 0",
        "!isQuantity('1e4294967296')",
        "quantity('1e4294967296') == quantity('1')",
        "isQuantity('12345678901234567890123456789012345678901')",
        "isQuantity('1e1001') || true",
        "isQuantity('e1001')",
        "[1].exists(x, quantity('1e1001') == quantity('1')) || true",
        "optional.of(quantity('1e1001')).hasValue() || true",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    for rule in [
        "true || isQuantity('1e1001')",
        "isQuantity('1e1000') && isQuantity('1e-1000')",
        "!isQuantity('1e9223372036854775808')",
        "!isQuantity('99999999999999999999999999999999999999999999999999+-1')",
        "!isQuantity('e-1001') && !isQuantity('.e-1001')",
    ] {
        assert!(run_rule(rule).is_ok(), "{rule}");
    }

    let evaluator = VapEvaluator::builder().build();
    let expr = |expression: &str| VapExpression {
        expression: expression.into(),
        message: None,
        message_expression: None,
    };
    let passed: Vec<bool> = evaluator
        .evaluate(&[
            expr("isQuantity('1e1001') || true"),
            expr("true || isQuantity('1e1001')"),
        ])
        .iter()
        .map(|r| r.passed)
        .collect();
    assert_eq!(passed, [false, true]);
}

// ── A timestamp offset of a day or more — fails CLOSED ─────────────────────
// apiserver: cel-go parses `timestamp(string)` with Go's
//            `time.Parse(time.RFC3339, ..)`, which takes a zone offset up to
//            hour 24 and minute 60 (`+24:60`; `+23:60` is `+24:00`)
//            (k8s.io/apiserver v0.36.1 `MustBaseEnvSet(1.36)`).
// kube-cel:  ports that parser (src/functions/cel_time.rs), but `chrono`
//            holds an offset under 24 hours, so such a timestamp is an
//            EvaluationError, even where CEL absorbs the error: noted as
//            unsupported, as an absorbed error could skip a quantity update the
//            apiserver makes. A literal one still compiles, as on the apiserver.
//            Everything else the conversions take or refuse matches the
//            apiserver (parity rows PtimeGo, PtimeDynamic, ..).

#[test]
fn timestamp_offset_of_a_day_fails_closed() {
    for rule in [
        "timestamp('2024-01-15T10:30:00+24:00') == timestamp('2024-01-14T10:30:00Z')",
        "timestamp('2024-01-15T10:30:00+23:60') == timestamp('2024-01-14T10:30:00Z')",
        "timestamp('2024-01-15T10:30:00-24:00') == timestamp('2024-01-16T10:30:00Z')",
        "'%s'.format([timestamp('2024-01-15T10:30:00+24:00')]) == '2024-01-15T10:30:00+24:00'",
        "timestamp('2024-01-15T10:30:00+24:00') == timestamp(0) || true",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    // Under a day the offset is kept, as cel-go keeps it.
    assert_passes(
        "timestamp('2024-01-15T10:30:00+00:60') == timestamp('2024-01-15T09:30:00Z') \
         && string(timestamp('2024-01-15T10:30:00+23:59')) == '2024-01-15T10:30:00+23:59'",
    );
}

// ── Timestamp getters without a time zone — fail OPEN until cel-rust#460 ──
// apiserver: cel-go reads `getHours()` and the other getters without a zone
//            argument in UTC, as the CEL spec says (k8s.io/apiserver v0.36.1
//            `MustBaseEnvSet(1.36)`).
// kube-cel:  `cel` 0.16 reads them in the timestamp's own offset.

/// The getters without a time zone on a timestamp with a non-zero offset:
/// `cel` reads them in that offset, cel-go and the CEL spec in UTC.
/// cel-rust#460 fixes it. These assertions pin today's verdicts, each the
/// apiserver's opposite (Go oracle), so they fail once `cel` carries the fix:
/// then drop this test and its README row, before 0.9.0. kube-cel builds no
/// workaround.
#[test]
fn timestamp_getters_without_a_zone_until_cel_rust_460() {
    // `2024-01-01T02:30:00+05:45` is `2023-12-31T20:45:00Z`.
    for rule in [
        "timestamp('2024-01-01T02:30:00+05:45').getFullYear() == 2024",
        "timestamp('2024-01-01T02:30:00+05:45').getMonth() == 0",
        "timestamp('2024-01-01T02:30:00+05:45').getDayOfYear() == 0",
        "timestamp('2024-01-01T02:30:00+05:45').getDayOfMonth() == 0",
        "timestamp('2024-01-01T02:30:00+05:45').getDate() == 1",
        "timestamp('2024-01-01T02:30:00+05:45').getDayOfWeek() == 1",
        "timestamp('2024-01-01T02:30:00+05:45').getHours() == 2",
        "timestamp('2024-01-01T02:30:00+05:45').getMinutes() == 30",
    ] {
        assert_passes(rule);
    }
    // The same for a `format: date-time` field.
    let schema = json!({
        "type": "object",
        "properties": { "t": { "type": "string", "format": "date-time" } },
        "x-kubernetes-validations": [{ "rule": "self.t.getHours() == 2" }]
    });
    assert!(
        Validator::new()
            .validate(&schema, &json!({"t": "2024-01-01T02:30:00+05:45"}), None)
            .is_ok()
    );
    // With a zone, and for the seconds, both agree.
    assert_passes(
        "timestamp('2024-01-01T02:30:00+05:45').getHours('UTC') == 20 \
         && timestamp('2024-01-01T02:30:00+05:45').getSeconds() == 0",
    );
}

// ── URL components that are not UTF-8 — fail CLOSED ────────────────────────
// apiserver: `getHost()`, `getHostname()` and `getQuery()` return Go's unescaped
//            bytes as a string, valid UTF-8 or not (`url('http://h%80x/')`
//            has the host bytes `h\x80x`; k8s.io/apiserver v0.36.1).
// kube-cel:  a CEL string must be UTF-8, so such a component is an
//            EvaluationError rather than a different string, even where CEL
//            absorbs the error (noted as unsupported, as an absorbed error
//            could skip a quantity update the apiserver makes).

#[test]
#[cfg(feature = "urls")]
fn url_component_not_utf8_fails_closed() {
    for rule in [
        "url('http://h%80x/').getHost() != 'x'",
        "url('http://h%80x/').getHostname() != 'x'",
        "url('/p?k=%FF').getQuery().size() == 1",
        "url('http://h%80x/').getHost() == 'x' || true",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    // The other components are written escaped, so they are always UTF-8.
    assert!(run_rule("url('http://h%80x/p%FF').getEscapedPath() == '/p%FF'").is_ok());
}

// ── A quantity update whose result depends on the order — fails CLOSED ─────
// apiserver: cel-go evaluates a call's receiver before its arguments, and both
//            operands of a two-operand call even when the first fails, and its
//            quantities share state: `add` / `sub` write into the receiver's
//            inf.Dec, `compareTo` / `isGreaterThan` / `isLessThan` turn an
//            int64Amount receiver into an inf.Dec (k8s.io/apiserver v0.36.1
//            `MustBaseEnvSet(1.36)`).
// kube-cel:  keeps the state, but `cel` 0.16 evaluates the arguments first and
//            skips the second operand after a failure, so an update of a
//            quantity held by a variable (a comprehension's, or reached through
//            `dyn(..)`, `[q][0]`, ..) in a call's receiver or arguments, or in
//            the second operand of a two-operand call, is refused at compile
//            time when another part could fail or read a quantity:
//            CompilationFailure, reached or not. Not refused: an update of a
//            new quantity (`quantity(s)`, `add` / `sub` on one); one next to
//            parts that can neither fail nor read a quantity (a comprehension
//            variable, a literal, `quantity(<literal>)`); one where both
//            evaluate in the same order (`&&`, `||`, `?:`, list and map
//            literals, indexing). Names are matched without types: a semver's
//            `compareTo` in such a position is refused too. Where kube-cel
//            would otherwise accept what the apiserver rejects, the refusal is
//            a parity row (PquantOrderRefused).

#[test]
#[cfg(feature = "quantity")]
fn quantity_update_in_an_order_dependent_position_fails_closed() {
    // The apiserver evaluates each to true.
    for rule in [
        "[quantity('1')].all(q, sign(q) > q.compareTo(quantity('2')))",
        "[quantity('1')].all(q, sign(q) > dyn(q).compareTo(quantity('2')))",
        "[quantity('1')].all(q, ((1 / 0 == q.compareTo(quantity('1.5Gi'))) || true) && !q.isInteger())",
        "[quantity('1')].all(q, quantity('2').add(sign(q)).add(q.compareTo(quantity('3'))).isInteger())",
        "[quantity('1')].all(q, (q.isInteger() ? 'a' : 'b').startsWith(q.compareTo(quantity('1.5Gi')) < 0 ? 'a' : 'b'))",
        "true || [quantity('1')].all(q, sign(q) > q.compareTo(quantity('2')))",
        "[semver('1.0.0')].all(v, sign(quantity('1')) > v.compareTo(semver('2.0.0')))",
    ] {
        assert_refused(rule);
    }
    // Not refused, and true as on the apiserver: a new quantity, or parts that
    // can neither fail nor read a quantity next to the update.
    for rule in [
        "0 > quantity('1').compareTo(quantity('2'))",
        "[quantity('1')].all(q, 0 > q.compareTo(quantity('2')))",
        "[quantity('1')].all(q, quantity('1').add(q.compareTo(quantity('2'))).isInteger())",
        "[quantity('1')].all(q, q.compareTo(quantity('1').add(quantity('1.5Gi'))) < 0)",
        "[quantity('1.5Gi')].all(q, q.add(q).compareTo(quantity('3Gi')) == 0 && q.compareTo(quantity('3Gi')) == 0)",
        "[quantity('1')].all(q, q.compareTo(quantity('1.5Gi')) < 0 && !q.isInteger())",
        "['1', '2'].map(s, quantity(s)).all(q, q.isGreaterThan(quantity('0')) && q.isLessThan(quantity('3')))",
    ] {
        assert_passes(rule);
    }
    // A semver's `compareTo` next to a literal, where `semver` is implemented.
    #[cfg(feature = "semver_funcs")]
    assert_passes("[semver('1.0.0')].all(v, 0 > v.compareTo(semver('2.0.0')))");
}

// ── Quantity `!=` with an add/sub result on the right — fails CLOSED ────────
// apiserver: `add` / `sub` return a pointer, which the right side of `Equal`
//            refuses: `quantity('2') == quantity('1').add(1)` is an error, while
//            `!=` is true (k8s.io/apiserver v0.36.1 `MustBaseEnvSet(1.36)`).
// kube-cel:  `cel`'s `==` and `!=` share one comparison that cannot fail or
//            tell them apart, so the comparison is noted and the rule refused
//            either way.

#[test]
#[cfg(feature = "quantity")]
fn quantity_ne_with_arithmetic_on_the_right_fails_closed() {
    for rule in [
        "quantity('2') != quantity('1').add(1)",
        "quantity('2') == quantity('1').add(1)",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
    // On the left it compares, as in the apiserver.
    assert!(run_rule("quantity('1').add(1) == quantity('2')").is_ok());
}

// ── `oldSelf` registration rules — fail CLOSED since 0.9; used to fail OPEN ──
// apiserver: rejects the CRD at registration for a rule reading `oldSelf` in
//            the items of a non-map list ("oldSelf cannot be used on the
//            uncorrelatable portion of the schema"), and for `optionalOldSelf`
//            on a rule that does not read `oldSelf`.
// kube-cel:  CompilationFailure for every object, checked over the whole schema.

#[test]
fn old_self_registration_rules_fail_closed() {
    let atomic_items = json!({
        "type": "object",
        "properties": {
            "e": { "type": "array", "items": { "type": "integer", "x-kubernetes-validations": [{"rule": "self >= oldSelf"}] } }
        }
    });
    let unused_optional = json!({
        "type": "object",
        "x-kubernetes-validations": [{"rule": "true", "optionalOldSelf": true}]
    });
    for schema in [atomic_items, unused_optional] {
        for errors in [
            Validator::new().validate(&schema, &json!({}), None),
            Validator::new().validate_compiled(&compile_schema(&schema), &json!({}), None),
        ] {
            let errors = errors.unwrap_err();
            assert!(
                errors.iter().all(|e| e.kind == ErrorKind::CompilationFailure),
                "{schema}: {errors:?}"
            );
        }
    }
}

// ── Schema depth cap — fails CLOSED since S4 (P1-A); used to fail OPEN ───────
// apiserver: has no nesting limit: v1.36.1 registers a 400-level CRD and
//            evaluates a rule 70 levels down (accepts `self >= 0` on 1, rejects
//            it on -1).
// kube-cel:  an object that reaches past MAX_SCHEMA_DEPTH (64) gets a
//            SchemaTooDeep error, whether the rule there holds or not. A shallow
//            object passes, as in the apiserver, and every rule below the cap is
//            still compiled, so one that does not compile is refused.

#[test]
fn depth_cap_fails_closed() {
    // Nest one integer field with a `self >= 0` rule past the cap.
    let mut schema = json!({
        "type": "integer",
        "x-kubernetes-validations": [{"rule": "self >= 0", "message": "leaf"}]
    });
    let (mut violated, mut holds) = (json!(-1), json!(1));
    for _ in 0..70 {
        schema = json!({ "type": "object", "properties": { "c": schema } });
        violated = json!({ "c": violated });
        holds = json!({ "c": holds });
    }
    for object in [violated, holds] {
        let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
        assert!(
            errors.iter().any(|e| e.kind == ErrorKind::SchemaTooDeep),
            "depth past the cap must fail closed with SchemaTooDeep, got {errors:?}"
        );
    }
    assert!(Validator::new().validate(&schema, &json!({}), None).is_ok());
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
