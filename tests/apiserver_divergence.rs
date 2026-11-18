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
//!   [`no_cost_model_fails_open`]. Rules on schema nodes the object does not
//!   reach used to be another, and `format()`'s output a third; since 0.9
//!   every rule in the schema is compiled before the object is walked, and
//!   `format()` ports cel-go v0.26's formatter.
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
//! `isIPv4`, `base64.*`, a global `min(..)`, ...) as
//! [`ErrorKind::CompilationFailure`], even in a branch evaluation never
//! reaches, as the apiserver does at registration. A function the apiserver
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
//            the apiserver. Reaching the call rejects the object with
//            UnsupportedReference (kube-cel binds no `authorizer`). The
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

    // Reached: the apiserver evaluates both to true; kube-cel rejects.
    for rule in [
        "optional.unwrap([optional.of(1), optional.none()]) == [1]",
        "[optional.of(1), optional.none()].unwrapOpt() == [1]",
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
        [true, true, false],
        "the reached authorizer call fails closed"
    );
}

// ── No type checker — fails OPEN (a documented exception class) ──────────────
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
    // An undeclared identifier in a branch that is never evaluated. A CRD rule
    // has no `authorizer` (a policy variable); the functions called on it are
    // declared for CRD rules too, so only the identifier is wrong.
    assert_fails_open("true || undeclared == 1");
    assert_fails_open("true || authorizer.group('').resource('pods').check('get').allowed()");

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
//            a wrapped value.

#[test]
#[cfg(feature = "quantity")]
fn quantity_arithmetic_past_i128_fails_closed() {
    for rule in [
        "quantity('2e38').add(quantity('1')).isGreaterThan(quantity('2e38'))",
        "quantity('2e38').sub(quantity('-1')).isGreaterThan(quantity('2e38'))",
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
//            string is still not a quantity (`isQuantity` false).

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

// ── Standard timestamp() / duration() parsing — fails CLOSED ───────────────
// apiserver: cel-go parses with Go's `time.Parse(time.RFC3339, ..)` and
//            `time.ParseDuration`, which take an offset of hour 24 or minute
//            60 (normalized: `+23:60` is `+24:00`), a `,` before the fraction,
//            a one-digit hour, and the units `µs` / `μs` (k8s.io/apiserver
//            v0.36.1 `MustBaseEnvSet(1.36)`).
// kube-cel:  `cel`'s standard conversions refuse them: EvaluationError.

#[test]
fn stdlib_time_parsing_fails_closed() {
    for rule in [
        "timestamp('2024-01-15T10:30:00+24:00') == timestamp('2024-01-14T10:30:00Z')",
        "timestamp('2024-01-15T10:30:00+23:60') == timestamp('2024-01-14T10:30:00Z')",
        "timestamp('2024-01-15T10:30:00-24:00') == timestamp('2024-01-16T10:30:00Z')",
        "timestamp('2024-01-15T10:30:00+00:60') == timestamp('2024-01-15T09:30:00Z')",
        "'%s'.format([timestamp('2024-01-15T10:30:00+24:00')]) == '2024-01-15T10:30:00+24:00'",
        "timestamp('2024-01-15T10:30:00,5Z') == timestamp('2024-01-15T10:30:00.5Z')",
        "timestamp('2024-01-15T1:30:00Z') == timestamp('2024-01-15T01:30:00Z')",
        "duration('1µs') == duration('1us')",
        "duration('1μs') == duration('1us')",
    ] {
        let errors = run_rule(rule).unwrap_err();
        assert!(
            errors.iter().all(|e| e.kind == ErrorKind::EvaluationError),
            "{rule}: {errors:?}"
        );
    }
}

// ── URL components that are not UTF-8 — fail CLOSED ────────────────────────
// apiserver: `getHost()`, `getHostname()` and `getQuery()` return Go's unescaped
//            bytes as a string, valid UTF-8 or not (`url('http://h%80x/')`
//            has the host bytes `h\x80x`; k8s.io/apiserver v0.36.1).
// kube-cel:  a CEL string must be UTF-8, so such a component is an
//            EvaluationError rather than a different string.

#[test]
#[cfg(feature = "urls")]
fn url_component_not_utf8_fails_closed() {
    for rule in [
        "url('http://h%80x/').getHost() != 'x'",
        "url('http://h%80x/').getHostname() != 'x'",
        "url('/p?k=%FF').getQuery().size() == 1",
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
