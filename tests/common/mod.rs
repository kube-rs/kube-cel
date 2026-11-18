//! Shared parity-case table — the single source of truth for the live parity
//! runner `apiserver_parity.rs` (kind/kubectl). Each row is pure data: a
//! CRD-expressible schema (carrying an `x-kubernetes-validations` rule), an
//! instance object, and the expected outcome. A case is written ONCE here and
//! proven against the real kube-apiserver.
//!
//! This module is included (`mod common;`) by each parity test crate, so it is
//! compiled per-runner. It pulls in `kube_cel` + `serde_json`, both available
//! whenever the `validation` feature is on (which every parity runner requires).

// Each runner uses a subset of these helpers, so unused items are expected.
#![allow(dead_code)]

use kube_cel::{Validator, compile_schema};
use serde_json::{Value, json};

/// The runtime CEL-admission verdict a control plane (or kube-cel) returns for a
/// CR that registered successfully.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Verdict {
    Accept,
    Reject,
}

/// The bucket a `(schema, object)` case is expected to land in. Mirrors the
/// fidelity-sweep buckets (see the plan doc): `Accept`/`Reject` are the faithful
/// runtime verdicts both engines must agree on. The other two are cases the
/// apiserver refuses at CRD registration (no valid CRD can carry the rule):
/// `RegistrationRejected` where kube-cel rejects the object too, and
/// `RegistrationRejectedFailOpen` where it accepts it, the two documented
/// fail-open classes (no type checker in `cel`, cel-rust#350; no cost model,
/// cel-rust discussion #230; see `tests/apiserver_divergence.rs`).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Expect {
    Accept,
    Reject,
    RegistrationRejected,
    RegistrationRejectedFailOpen,
}

impl Expect {
    /// The runtime verdict this expectation pins, for the register-able cases.
    /// Panics on the registration-rejected ones, which never reach the
    /// apiserver's runtime evaluation.
    pub fn verdict(self) -> Verdict {
        match self {
            Expect::Accept => Verdict::Accept,
            Expect::Reject => Verdict::Reject,
            Expect::RegistrationRejected | Expect::RegistrationRejectedFailOpen => {
                panic!("a registration-rejected case has no runtime verdict")
            }
        }
    }
}

/// One parity case: a CRD `openAPIV3Schema` + an instance + the expected bucket.
/// `kind`/`plural` must be unique across the table (each case gets its own CRD).
pub struct ParityCase {
    pub kind: &'static str,
    pub plural: &'static str,
    pub schema: Value,
    pub object: Value,
    pub expect: Expect,
    /// One-line rationale, surfaced in runner output for traceability.
    pub note: &'static str,
}

/// kube-cel's verdict via plain `validate` (which, as of 0.8, applies schema
/// defaults before evaluating CEL — matching the apiserver's admission order).
pub fn kubecel_verdict(schema: &Value, object: &Value, old: Option<&Value>) -> Verdict {
    match Validator::new().validate(schema, object, old) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
}

/// kube-cel's verdict via the explicit `validate_with_defaults` alias. Should be
/// identical to [`kubecel_verdict`] since 0.8; runners assert both to guard the
/// alias against drift.
pub fn kubecel_verdict_with_defaults(schema: &Value, object: &Value, old: Option<&Value>) -> Verdict {
    match Validator::new().validate_with_defaults(schema, object, old) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
}

/// kube-cel's verdict via `compile_schema` + `validate_compiled`, the
/// pre-compiled path, which must agree with [`kubecel_verdict`].
pub fn kubecel_compiled_verdict(schema: &Value, object: &Value, old: Option<&Value>) -> Verdict {
    match Validator::new().validate_compiled(&compile_schema(schema), object, old) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
}

/// An update case: the apiserver stores `old` first (a real create), then
/// server-dry-runs `object` as a full replace of it, so rules that read
/// `oldSelf` run. kube-cel gets the same pair as `(object, Some(old))`. `old`
/// must itself pass validation on create. `expect` is `Accept` or `Reject`.
pub struct TransitionCase {
    pub kind: &'static str,
    pub plural: &'static str,
    pub schema: Value,
    pub old: Value,
    pub object: Value,
    pub expect: Expect,
    pub note: &'static str,
}

/// Update cases (transition rules, `optionalOldSelf`, list-item correlation).
pub fn transition_cases() -> Vec<TransitionCase> {
    vec![
        // optionalOldSelf binds `oldSelf` as `optional.of(old)` on update
        // (`validationActivationWithOldSelf`), so `hasValue()`/`value()` work.
        TransitionCase {
            kind: "TxOptionalOf",
            plural: "txoptionalofs",
            schema: json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "x-kubernetes-validations": [{
                    "rule": "!oldSelf.hasValue() || oldSelf.value().n <= self.n",
                    "message": "n cannot decrease",
                    "optionalOldSelf": true
                }]
            }),
            old: json!({"n": 1}),
            object: json!({"n": 3}),
            expect: Expect::Accept,
            note: "optionalOldSelf on update: oldSelf = optional.of(old), value().n = 1 <= 3 → ACCEPT",
        },
        // An old value that is JSON null is no old value. The apiserver's
        // node `oldObj` is then nil, so `optionalOldSelf` binds `optional.none()`
        // (`validationActivationWithOldSelf`), not `optional.of(null)`.
        TransitionCase {
            kind: "TxOldNullOptional",
            plural: "txoldnulloptionals",
            schema: json!({
                "type": "object",
                "properties": {
                    "sub": {
                        "type": "object",
                        "nullable": true,
                        "properties": { "a": { "type": "string" } },
                        "x-kubernetes-validations": [{
                            "rule": "!oldSelf.hasValue()",
                            "message": "oldSelf must be none",
                            "optionalOldSelf": true
                        }]
                    }
                }
            }),
            old: json!({"sub": null}),
            object: json!({"sub": {"a": "x"}}),
            expect: Expect::Accept,
            note: "old sub is null → optionalOldSelf binds optional.none() → ACCEPT",
        },
        // The same with a plain transition rule: a null old value leaves it no
        // comparable old value and is skipped (`compiled.UsesOldSelf && oldObj == nil`).
        TransitionCase {
            kind: "TxOldNullPlain",
            plural: "txoldnullplains",
            schema: json!({
                "type": "object",
                "properties": {
                    "sub": {
                        "type": "object",
                        "nullable": true,
                        "properties": { "a": { "type": "string" } },
                        "x-kubernetes-validations": [{ "rule": "self.a == oldSelf.a", "message": "a is immutable" }]
                    }
                }
            }),
            old: json!({"sub": null}),
            object: json!({"sub": {"a": "x"}}),
            expect: Expect::Accept,
            note: "old sub is null → transition rule skipped → ACCEPT",
        },
        // List items correlate old↔new only for `x-kubernetes-list-type: map`,
        // by key (`MakeMapList`): a reorder pairs each item with its own old self.
        // (The list is not called `items`: kubectl reads a top-level `items` as a
        // List of objects.)
        TransitionCase {
            kind: "TxMapListByKey",
            plural: "txmaplistbykeys",
            schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "x-kubernetes-list-type": "map",
                        "x-kubernetes-list-map-keys": ["name"],
                        "items": {
                            "type": "object",
                            "required": ["name"],
                            "properties": { "name": { "type": "string" }, "v": { "type": "integer" } },
                            "x-kubernetes-validations": [{ "rule": "self.v >= oldSelf.v", "message": "v cannot decrease" }]
                        }
                    }
                }
            }),
            old: json!({"entries": [{"name": "a", "v": 1}, {"name": "b", "v": 10}]}),
            object: json!({"entries": [{"name": "b", "v": 10}, {"name": "a", "v": 1}]}),
            expect: Expect::Accept,
            note: "map list reordered: items correlate by key, not index → ACCEPT",
        },
        // With several map keys the apiserver correlates by a string key
        // built with Go `%f` for floats (`multiKeyStrategy`), so key values that
        // agree to six decimals are the same item and the transition rule runs.
        TransitionCase {
            kind: "TxMultiKeyFloat",
            plural: "txmultikeyfloats",
            schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "x-kubernetes-list-type": "map",
                        "x-kubernetes-list-map-keys": ["g", "f"],
                        "items": {
                            "type": "object",
                            "required": ["g", "f"],
                            "properties": {
                                "g": { "type": "string" },
                                "f": { "type": "number" },
                                "v": { "type": "integer" }
                            },
                            "x-kubernetes-validations": [{ "rule": "self.v >= oldSelf.v", "message": "v cannot decrease" }]
                        }
                    }
                }
            }),
            old: json!({"entries": [{"g": "x", "f": 1.0000001, "v": 5}]}),
            object: json!({"entries": [{"g": "x", "f": 1.0000002, "v": 1}]}),
            expect: Expect::Reject,
            note: "multi-key floats compare as %f (1.000000 both) → correlated, 1 < 5 → REJECT",
        },
        // A number key that does not fit an int64 is a float64 in the apiserver
        // (JSON decoding keeps only int64-sized integers), so 2^63 and 2^63 + 1
        // are the same key there and the items correlate.
        TransitionCase {
            kind: "TxKeyU64",
            plural: "txkeyu64s",
            schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "x-kubernetes-list-type": "map",
                        "x-kubernetes-list-map-keys": ["k"],
                        "items": {
                            "type": "object",
                            "required": ["k"],
                            "properties": { "k": { "type": "number" }, "v": { "type": "integer" } },
                            "x-kubernetes-validations": [{ "rule": "self.v >= oldSelf.v", "message": "v cannot decrease" }]
                        }
                    }
                }
            }),
            old: json!({"entries": [{"k": 9223372036854775808u64, "v": 5}]}),
            object: json!({"entries": [{"k": 9223372036854775809u64, "v": 1}]}),
            expect: Expect::Reject,
            note: "keys 2^63 and 2^63+1 are one float64 key → correlated, 1 < 5 → REJECT",
        },
        TransitionCase {
            kind: "TxKeyNegZero",
            plural: "txkeynegzeros",
            schema: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "x-kubernetes-list-type": "map",
                        "x-kubernetes-list-map-keys": ["k"],
                        "items": {
                            "type": "object",
                            "required": ["k"],
                            "properties": { "k": { "type": "number" }, "v": { "type": "integer" } },
                            "x-kubernetes-validations": [{ "rule": "self.v >= oldSelf.v", "message": "v cannot decrease" }]
                        }
                    }
                }
            }),
            old: json!({"entries": [{"k": -0.0, "v": 5}]}),
            object: json!({"entries": [{"k": 0.0, "v": 1}]}),
            expect: Expect::Reject,
            note: "single key -0.0 and 0.0 compare equal → correlated, 1 < 5 → REJECT",
        },
    ]
}

/// An integer field holding `rule`, nested `levels` objects deep under `c`.
fn deep_schema(levels: usize, rule: &str) -> Value {
    let mut deep =
        json!({ "type": "integer", "x-kubernetes-validations": [{ "rule": rule, "message": "deep" }] });
    for _ in 0..levels {
        deep = json!({ "type": "object", "properties": { "c": deep } });
    }
    deep
}

/// The shared parity table. Phase 2 of the fidelity sweep appends rows here; the
/// runners do not change.
pub fn cases() -> Vec<ParityCase> {
    vec![
        // ── ESCAPING fidelity (kube-rs/kube-cel#8) ───────────────────────────
        // `map[string]string` (additionalProperties object) with the forbidden
        // literal key present: both must REJECT (map keys not field-escaped).
        ParityCase {
            kind: "PmapObj",
            plural: "pmapobjs",
            schema: json!({
                "type": "object",
                "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
                "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
            }),
            object: json!({"m": {"a.b/c": "x"}}),
            expect: Expect::Reject,
            note: "typed map with forbidden literal key → REJECT",
        },
        // Free-form map (`additionalProperties: true`) makes `self.m` opaque: the
        // apiserver rejects `'k' in self.m` at REGISTRATION ("no matching
        // overload for '@in'"). So the #8 escaping concern is unreachable through
        // a valid CRD for free-form maps; kube-cel's broader fix is harmless
        // belt-and-suspenders, not a reachable-divergence fix.
        ParityCase {
            kind: "PmapTrue",
            plural: "pmaptrues",
            schema: json!({
                "type": "object",
                "properties": { "m": { "type": "object", "additionalProperties": true } },
                "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
            }),
            object: json!({"m": {"harmless": "x"}}),
            expect: Expect::RegistrationRejectedFailOpen,
            note: "`in` on a free-form map is a type error → rejected at registration; kube-cel has no type checker and accepts",
        },
        // `[]map[string]string` — forbidden key inside a LIST ELEMENT's map.
        // Exercises escaping skip through `items` recursion. Both REJECT.
        ParityCase {
            kind: "PlistMap",
            plural: "plistmaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": { "type": "array", "items": { "type": "object", "additionalProperties": {"type": "string"} } }
                },
                "x-kubernetes-validations": [{ "rule": "self.l.all(e, !('a.b/c' in e))", "message": "forbidden key" }]
            }),
            object: json!({"l": [{"a.b/c": "x"}]}),
            expect: Expect::Reject,
            note: "forbidden key in a list-element map → REJECT",
        },
        // `map[string]map[string]string` — forbidden key inside a NESTED map
        // value. Exercises escaping skip through `additionalProperties` recursion
        // AND that the outer map key is itself literal. Both REJECT.
        ParityCase {
            kind: "PmapMap",
            plural: "pmapmaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "additionalProperties": { "type": "object", "additionalProperties": {"type": "string"} }
                    }
                },
                "x-kubernetes-validations": [{ "rule": "self.m.all(k, !('a.b/c' in self.m[k]))", "message": "forbidden key" }]
            }),
            object: json!({"m": {"grp": {"a.b/c": "x"}}}),
            expect: Expect::Reject,
            note: "forbidden key in a nested map value → REJECT",
        },
        // Map key colliding with a CEL reserved word (`in`). The rule REQUIRES the
        // key, so a literal (unescaped) key means both ACCEPT.
        ParityCase {
            kind: "PmapResv",
            plural: "pmapresvs",
            schema: json!({
                "type": "object",
                "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
                "x-kubernetes-validations": [{ "rule": "'in' in self.m", "message": "needs in" }]
            }),
            object: json!({"m": {"in": "x"}}),
            expect: Expect::Accept,
            note: "literal reserved-word map key satisfies the rule → ACCEPT",
        },
        // Control: a harmless map key satisfies the forbidden-key rule → ACCEPT.
        ParityCase {
            kind: "PmapOk",
            plural: "pmapoks",
            schema: json!({
                "type": "object",
                "properties": { "m": { "type": "object", "additionalProperties": {"type": "string"} } },
                "x-kubernetes-validations": [{ "rule": "!('a.b/c' in self.m)", "message": "forbidden key" }]
            }),
            object: json!({"m": {"harmless": "x"}}),
            expect: Expect::Accept,
            note: "harmless map key → ACCEPT (control)",
        },
        // Declared struct fields keep field-name escaping: a dotted property
        // `foo.bar` is reachable ONLY via its escaped identifier
        // `foo__dot__bar`. The rule lives on `spec` (so `self` = spec); kube-cel
        // evaluates the nested rule against the full schema just like the
        // apiserver. Both ACCEPT — the regression-guard direction of #8.
        ParityCase {
            kind: "PstructEsc",
            plural: "pstructescs",
            schema: json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "type": "object",
                        "properties": { "foo.bar": { "type": "string" } },
                        "x-kubernetes-validations": [{ "rule": "self.foo__dot__bar == 'ok'", "message": "bad" }]
                    }
                }
            }),
            object: json!({"spec": {"foo.bar": "ok"}}),
            expect: Expect::Accept,
            note: "dotted declared field reached via escaped identifier → ACCEPT",
        },
        // Single-underscore declared field. The apiserver escapes ONLY `__`
        // (matcher `(__|[-./]|…)`), so `my_field` stays literal and the rule
        // references it as-is. Pins the P0 fix: pre-fix kube-cel over-escaped the
        // object key to `my__field`, so `self.my_field` errored → REJECT while the
        // apiserver ACCEPTs. Both must ACCEPT now.
        ParityCase {
            kind: "PstructUnderscore",
            plural: "pstructunderscores",
            schema: json!({
                "type": "object",
                "properties": { "my_field": { "type": "string" } },
                "x-kubernetes-validations": [{ "rule": "self.my_field == 'ok'", "message": "bad" }]
            }),
            object: json!({"my_field": "ok"}),
            expect: Expect::Accept,
            note: "single-underscore field is NOT escaped (apiserver escapes only __) → ACCEPT",
        },
        // Double-underscore declared field: `a__b` IS escaped, to
        // `a__underscores__b`. Pins the other half of the P0 fix.
        ParityCase {
            kind: "PstructDunder",
            plural: "pstructdunders",
            schema: json!({
                "type": "object",
                "properties": { "a__b": { "type": "string" } },
                "x-kubernetes-validations": [{ "rule": "self.a__underscores__b == 'ok'", "message": "bad" }]
            }),
            object: json!({"a__b": "ok"}),
            expect: Expect::Accept,
            note: "double-underscore field reached via __underscores__ escape → ACCEPT",
        },
        // ── DEFAULTS fidelity (kube-rs/kube-cel#9, fixed in 0.8) ──────────────
        // The apiserver applies schema `default`s BEFORE evaluating CEL. As of
        // 0.8, plain `validate` does too, so these now assert PARITY.
        //
        // Fail-CLOSED direction (pre-0.8 plain `validate` REJECTed): `x` has a
        // default; the object omits it; `self.x == 'd'` holds once defaulted.
        ParityCase {
            kind: "Pdefclosed",
            plural: "pdefcloseds",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } },
                "x-kubernetes-validations": [{ "rule": "self.x == 'd'", "message": "x must be d" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "default applied → self.x == 'd' → ACCEPT (was fail-closed)",
        },
        // Fail-OPEN direction (the dangerous one; pre-0.8 plain `validate`
        // ACCEPTed an input the apiserver REJECTS). Defaulting makes `x` present
        // → `!has(self.x)` false → REJECT.
        ParityCase {
            kind: "Pdefopen",
            plural: "pdefopens",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } },
                "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "default makes x present → !has false → REJECT (was fail-open)",
        },
        // Nested default under a declared object property: defaulting recurses
        // through nested structs.
        ParityCase {
            kind: "Pdefnest",
            plural: "pdefnests",
            schema: json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "type": "object",
                        "properties": { "x": { "type": "string", "default": "d" } },
                        "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
                    }
                }
            }),
            object: json!({ "spec": {} }),
            expect: Expect::Reject,
            note: "nested default makes spec.x present → REJECT",
        },
        // Missing PARENT with an object `default`: the apiserver inserts the
        // default `{}` then RECURSES into it, filling nested defaults (its second
        // loop is `for k := range x` over the mutated map). Pins the P1 fix —
        // pre-fix kube-cel recursed over the ORIGINAL object, so a defaulted
        // parent's own children never filled: `self.spec.x` errored → REJECT while
        // the apiserver ACCEPTs. (Distinct from `Pdefnest`, where spec is present.)
        ParityCase {
            kind: "Pdefmissing",
            plural: "pdefmissings",
            schema: json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "type": "object",
                        "default": {},
                        "properties": { "x": { "type": "string", "default": "d" } }
                    }
                },
                "x-kubernetes-validations": [{ "rule": "self.spec.x == 'd'", "message": "spec.x must be d" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "missing parent gets default {} then nested default fills spec.x=d → ACCEPT (recurse-into-inserted)",
        },
        // A non-nullable field that is present but null takes its default
        // (defaulting/algorithm.go `isNonNullableNull`), so the rule sees 'd'.
        ParityCase {
            kind: "Pdefnull",
            plural: "pdefnulls",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } },
                "x-kubernetes-validations": [{ "rule": "self.x == 'd'", "message": "x must be d" }]
            }),
            object: json!({"x": null}),
            expect: Expect::Accept,
            note: "present-but-null non-nullable field takes its default → ACCEPT",
        },
        // A `default: null` is no default: the apiserver skips nil defaults
        // (defaulting/algorithm.go), also inside a parent it just defaulted, so
        // `spec` becomes `{}` and `spec.x` stays absent.
        ParityCase {
            kind: "PdefNullChild",
            plural: "pdefnullchilds",
            schema: json!({
                "type": "object",
                "properties": {
                    "spec": {
                        "type": "object",
                        "default": {},
                        "properties": { "x": { "type": "string", "nullable": true, "default": null } }
                    }
                },
                "x-kubernetes-validations": [{ "rule": "!has(self.spec.x)", "message": "x must stay absent" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "default: null is skipped inside a defaulted parent → spec.x absent → ACCEPT",
        },
        // Defaulted field inside ARRAY items: defaulting recurses into items.
        ParityCase {
            kind: "Pdefarr",
            plural: "pdefarrs",
            schema: json!({
                "type": "object",
                "properties": {
                    "list": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "x": { "type": "string", "default": "d" } },
                            "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
                        }
                    }
                }
            }),
            object: json!({ "list": [ {} ] }),
            expect: Expect::Reject,
            note: "item default makes x present → REJECT",
        },
        // THE additionalProperties case (#9 nested-map gap). Defaulting recurses
        // into map VALUES, which pre-0.8 was fail-open even via
        // `validate_with_defaults`. (A default under additionalProperties at the
        // schema ROOT is rejected at registration, so the map must be nested.)
        ParityCase {
            kind: "Pdefap",
            plural: "pdefaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "additionalProperties": {
                            "type": "object",
                            "properties": { "x": { "type": "string", "default": "d" } },
                            "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
                        }
                    }
                }
            }),
            object: json!({ "m": { "a": {} } }),
            expect: Expect::Reject,
            note: "map-value default makes x present → REJECT (#9 nested-AP gap)",
        },
        // `required` + default coexist: registration accepted, default wins, field
        // is present, so omitting it is ACCEPTed (default satisfies the rule).
        ParityCase {
            kind: "Pdefreq",
            plural: "pdefreqs",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "string", "default": "d" } },
                "required": ["x"],
                "x-kubernetes-validations": [{ "rule": "self.x == 'd'", "message": "x must be d" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "default fills required field → self.x == 'd' → ACCEPT",
        },
        // ── BINDING fidelity (audit 2026-07-25) ───────────────────────────────
        // D1 — `type: number` holding an integral JSON number. The apiserver's
        // `UnstructuredToVal` maps int/int32/int64/float32/float64 alike to
        // `types.Double` under `type: number` (YAML→JSON collapses `1.0` to `1`),
        // so `self.x / 2.0` is 0.5. kube-cel bound `Value::Int(1)` from the JSON
        // representation alone, ignoring the schema type.
        ParityCase {
            kind: "PnumDouble",
            plural: "pnumdoubles",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "number" } },
                "x-kubernetes-validations": [{ "rule": "self.x / 2.0 == 0.5", "message": "half" }]
            }),
            object: json!({"x": 1}),
            expect: Expect::Accept,
            note: "type:number binds double, so 1/2.0 == 0.5 → ACCEPT (D1)",
        },
        // The binding itself: an integral `1` under `type: number` is a double,
        // not an int. (`self.x / 2.0` cannot tell: on an int it errors, which
        // rejects too.)
        ParityCase {
            kind: "PnumNotInt",
            plural: "pnumnotints",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "number" } },
                "x-kubernetes-validations": [{ "rule": "type(self.x) == double", "message": "not double" }]
            }),
            object: json!({"x": 1}),
            expect: Expect::Accept,
            note: "type:number binds an integral 1 as double, not int → ACCEPT",
        },
        // `type` decides the binding before `format`: a string format on a
        // `type: number` field changes nothing.
        ParityCase {
            kind: "PnumFormat",
            plural: "pnumformats",
            schema: json!({
                "type": "object",
                "properties": {
                    "d": { "type": "number", "format": "date" },
                    "t": { "type": "number", "format": "date-time" },
                    "u": { "type": "number", "format": "duration" },
                    "b": { "type": "number", "format": "byte" }
                },
                "x-kubernetes-validations": [{
                    "rule": "type(self.d) == double && type(self.t) == double && type(self.u) == double && type(self.b) == double",
                    "message": "not double"
                }]
            }),
            object: json!({"d": 1, "t": 1, "u": 1, "b": 1}),
            expect: Expect::Accept,
            note: "type:number with a string format still binds double → ACCEPT",
        },
        // `type: integer` is unaffected — the control for D1.
        ParityCase {
            kind: "PintStays",
            plural: "pintstays",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "self.x / 2 == 0", "message": "int div" }]
            }),
            object: json!({"x": 1}),
            expect: Expect::Accept,
            note: "type:integer keeps integer division → ACCEPT (D1 control)",
        },
        // D2 — `format: date` binds as a Timestamp upstream
        // (`time.Parse(strfmt.RFC3339FullDate, str)`), so the timestamp
        // accessors work. kube-cel left it a string → the rule errored.
        ParityCase {
            kind: "PdateFmt",
            plural: "pdatefmts",
            schema: json!({
                "type": "object",
                "properties": { "d": { "type": "string", "format": "date" } },
                "x-kubernetes-validations": [{ "rule": "self.d.getFullYear() == 2024", "message": "year" }]
            }),
            object: json!({"d": "2024-03-05"}),
            expect: Expect::Accept,
            note: "format:date binds as timestamp → getFullYear()==2024 → ACCEPT (D2)",
        },
        // A `format: date` string that does not parse is an error, not a
        // string: the apiserver's binding fails (and its OpenAPI format check
        // refuses the object even when no rule reads the field).
        ParityCase {
            kind: "PdateBad",
            plural: "pdatebads",
            schema: json!({
                "type": "object",
                "properties": { "d": { "type": "string", "format": "date" } },
                "x-kubernetes-validations": [{ "rule": "self.d == self.d", "message": "d" }]
            }),
            object: json!({"d": "2024-02-30"}),
            expect: Expect::Reject,
            note: "invalid date (day out of range) is an error → REJECT",
        },
        ParityCase {
            kind: "PdateBadUnread",
            plural: "pdatebadunreads",
            schema: json!({
                "type": "object",
                "properties": { "d": { "type": "string", "format": "date" } },
                "x-kubernetes-validations": [{ "rule": "true", "message": "always" }]
            }),
            object: json!({"d": "2024-3-05"}),
            expect: Expect::Reject,
            note: "invalid date no rule reads is still refused → REJECT",
        },
        // ── FIDELITY-SWEEP findings (2026-06-19), fixed in 0.8 ────────────────
        // Each row reproduces a measured divergence with the OLD (buggy) expected
        // value, so it pins that kube-cel now agrees with the apiserver.
        //
        // F1b — `%s` on a map emitted `{\"a\": 1}` (space after colon); cel-go
        // emits `{\"a\":1}`. Old equality now false on both → REJECT.
        ParityCase {
            kind: "PfmtMap",
            plural: "pfmtmaps",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "'%s'.format([{'a': 1}]) == '{\"a\": 1}'", "message": "map" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "format %s on map is {\"a\":1} (no space) → REJECT (F1b)",
        },
        // F2 — `lastIndexOf(sub, i)` treated `i` as an exclusive end; cel-go
        // treats it as the inclusive last START index, so `('abcabc','abc',3)`
        // is 3, not 0. Old equality now false on both → REJECT.
        ParityCase {
            kind: "PstrLast",
            plural: "pstrlasts",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "'abcabc'.lastIndexOf('abc', 3) == 0", "message": "last" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "lastIndexOf offset is inclusive start (=3), not 0 → REJECT (F2)",
        },
        // F3 — `format: byte` now binds as CEL `bytes` (apiserver decodes it), so
        // `size()` is the decoded length. "aGVsbG8=" → "hello" → 5. Was
        // fail-CLOSED (kube-cel saw the 8-char string and over-rejected).
        ParityCase {
            kind: "PbyteSize",
            plural: "pbytesizes",
            schema: json!({
                "type": "object",
                "properties": { "b": { "type": "string", "format": "byte" } },
                "x-kubernetes-validations": [{ "rule": "self.b.size() == 5", "message": "byte" }]
            }),
            object: json!({ "b": "aGVsbG8=" }),
            expect: Expect::Accept,
            note: "format:byte binds as bytes → size()==5 → ACCEPT (F3)",
        },
        // ── Kubernetes functions vs apiserver v1.36.1 ─────────────────────────
        //
        // Quantity comparison: i128::MAX against i128::MAX + 3 written with an
        // exponent. The scales differ, aligning them overflowed i128, and
        // kube-cel fell back to an f64 compare, which called them equal. The
        // apiserver compares exactly.
        ParityCase {
            kind: "PquantBigCmp",
            plural: "pquantbigcmps",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "quantity('170141183460469231731687303715884105727').compareTo(quantity('17014118346046923173168730371588410573e1')) == -1",
                    "message": "cmp"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "quantities 3 apart beyond i128 scaling compare exactly (-1) → ACCEPT",
        },
        // URI validity: `isURL`, `url()` and `format.uri()` are Go's
        // `url.ParseRequestURI` (go1.26.2 in kube-apiserver v1.36.1): a bad
        // `%` escape, a control byte, a leading space, and (http/https only) a
        // second colon in the host are refused. The WHATWG parse kube-cel used
        // repaired or accepted each of them.
        ParityCase {
            kind: "PurlRequestUri",
            plural: "purlrequesturis",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!isURL('/foo%zz') && !isURL('/foo\\n') && !isURL('/foo\\t') && !isURL(' http://example.com') && !isURL('http://a:1:2/') && isURL('foo://a:1:2/') && isURL('mailto:%zz')",
                    "message": "isURL"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "isURL follows Go url.ParseRequestURI → ACCEPT",
        },
        ParityCase {
            kind: "PfmtUri",
            plural: "pfmturis",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.uri().validate('/foo%zz').hasValue() && format.uri().validate(' http://example.com').hasValue() && !format.uri().validate('/foo').hasValue()",
                    "message": "uri"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "format.uri() follows Go url.ParseRequestURI → ACCEPT",
        },
        // A `format: duration` value past the int64 range is an error in the
        // apiserver (`strfmt.ParseDuration`); kube-cel saturated it to the
        // largest duration.
        ParityCase {
            kind: "PdurOverflow",
            plural: "pduroverflows",
            schema: json!({
                "type": "object",
                "properties": { "d": { "type": "string", "format": "duration" } },
                "x-kubernetes-validations": [{ "rule": "self.d == duration('2562047h47m16.854775807s')", "message": "d" }]
            }),
            object: json!({"d": "9223372036854775808µs"}),
            expect: Expect::Reject,
            note: "duration past int64 is an error, not the largest duration → REJECT",
        },
        // `strfmt.ParseDuration` falls back to word units Go's own parser
        // lacks, so `1d` is a day there (kube-cel used to leave it a string).
        ParityCase {
            kind: "PdurDay",
            plural: "pdurdays",
            schema: json!({
                "type": "object",
                "properties": { "d": { "type": "string", "format": "duration" } },
                "x-kubernetes-validations": [{ "rule": "self.d == duration('24h')", "message": "d" }]
            }),
            object: json!({"d": "1d"}),
            expect: Expect::Accept,
            note: "format:duration 1d is 24h (strfmt fallback) → ACCEPT",
        },
        // The URL accessors return Go's `url.URL` fields: the apiserver checks
        // the string with `url.ParseRequestURI`, parses it with `url.Parse`
        // (fragment split off, `//host/p` with a host), and compares `String()`.
        ParityCase {
            kind: "PurlAccessors",
            plural: "purlaccessorses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "url('http://h:80/').getPort() == '80' && url('http://h:80/').getHost() == 'h:80' && url('/p').getHost() == '' && url('*').getEscapedPath() == '*' && url('http://h/a#frag').getEscapedPath() == '/a' && url('//foo/bar').getHostname() == 'foo' && url('HTTP://H/').getHostname() == 'H' && url('http://H:80/') != url('http://h/') && url('/a?x=1;y=2&z=3').getQuery() == {'z': ['3']} && url('http://exa%C3%A9mple.com/').getHost() == 'exaémple.com'",
                    "message": "url"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "url accessors return Go's url.URL fields → ACCEPT",
        },
        // `format.named` returns `optional(Format)`; `format.byte()` is strict
        // standard base64 (govalidator.IsBase64): padding required, empty refused.
        ParityCase {
            kind: "PfmtNamedByte",
            plural: "pfmtnamedbytes",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.named('uuid').hasValue() && !format.named('unknown').hasValue() && !format.byte().validate('aGVsbG8=').hasValue() && format.byte().validate('aGVsbG8').hasValue() && format.byte().validate('').hasValue()",
                    "message": "named/byte"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "format.named is optional, format.byte needs padded standard base64 → ACCEPT",
        },
        // `format.date()` / `format.datetime()` refuse a `+`-signed component
        // and a leap second (strfmt IsDate / IsDateTime).
        ParityCase {
            kind: "PfmtDateSign",
            plural: "pfmtdatesigns",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.date().validate('+024-01-01').hasValue() && format.date().validate('2024-+1-01').hasValue() && format.datetime().validate('2024-01-15T+1:30:00Z').hasValue() && format.datetime().validate('2024-01-15T10:30:60Z').hasValue() && !format.datetime().validate('2024-01-15T10:30:59Z').hasValue()",
                    "message": "date"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "signed date components and leap seconds are invalid → ACCEPT",
        },
        // Lenient semver normalizes like the apiserver's normalizeAndParse:
        // only a lowercase `v` is stripped, and a fourth component stays an error.
        ParityCase {
            kind: "PsemverLenient",
            plural: "psemverlenients",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!isSemver('V1.0.0', true) && !isSemver('1.2.3.4', true) && isSemver('v1.2', true) && isSemver('01.02.03', true)",
                    "message": "semver"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "lenient semver: lowercase v only, no fourth component → ACCEPT",
        },
        // A non-ASCII character inside the seconds field is an invalid
        // date-time; kube-cel sliced the time on byte offsets and panicked.
        ParityCase {
            kind: "PfmtDatetimeNonAscii",
            plural: "pfmtdatetimenonasciis",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.datetime().validate('2024-01-15T12:30:0é:00Z').hasValue() && format.datetime().validate('2024-01-15T12:3é:00Z').hasValue()",
                    "message": "datetime"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "non-ASCII in a date-time time field is invalid, not a panic → ACCEPT",
        },
        // The name formats are apimachinery's validators, messages verbatim
        // (`.validate(s).value()` reads them); uuid is strfmt's IsUUID.
        ParityCase {
            kind: "PfmtNameMessages",
            plural: "pfmtnamemessages",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.dns1035LabelPrefix().validate('').hasValue() && !format.uuid().validate('550e8400e29b41d4a716446655440000').hasValue() && !format.dns1123SubdomainPrefix().validate('a.b-').hasValue() && format.dns1123Label().validate('a.b').value() == ['must not contain dots'] && format.qualifiedName().validate('/x').value() == ['prefix part must be non-empty'] && format.uuid().validate('x').value() == ['does not match the UUID format'] && format.labelValue().validate('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa').value() == ['must be no more than 63 bytes'] && format.dns1123Label().validate('aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa').value() == ['must be no more than 63 characters'] && format.dns1123Label().validate('A').value() == [\"a lowercase RFC 1123 label must consist of lower case alphanumeric characters or '-', and must start and end with an alphanumeric character (e.g. 'my-name',  or '123-abc', regex used for validation is '[a-z0-9]([-a-z0-9]*[a-z0-9])?')\"]",
                    "message": "names"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "name formats validate and word errors as apimachinery → ACCEPT",
        },
        // format.uri() reports Go's *url.Error from ParseRequestURI: `parse
        // "<s>": <why>`, `%q`-quoted parts, netip's ParseAddr errors.
        ParityCase {
            kind: "PfmtUriMessages",
            plural: "pfmturimessages",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "format.uri().validate('foo').value() == ['parse \"foo\": invalid URI for request'] && format.uri().validate('http://[x]/').value() == ['parse \"http://[x]/\": invalid host: ParseAddr(\"x\"): unable to parse IP'] && format.uri().validate('http://[1.2.3.4]/').value() == ['parse \"http://[1.2.3.4]/\": invalid IP-literal'] && format.uri().validate('http://h:x/').value() == ['parse \"http://h:x/\": invalid port \":x\" after host'] && format.uri().validate('http://a b/').value() == ['parse \"http://a b/\": invalid character \" \" in host name'] && format.uri().validate('/%zz').value() == ['parse \"/%zz\": invalid URL escape \"%zz\"'] && !format.uri().validate('http://[fe80::1%25en0]:8080/').hasValue()",
                    "message": "uri"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "format.uri() words errors as Go's url.ParseRequestURI → ACCEPT",
        },
        // format.datetime() is kube-openapi's strfmt.IsDateTime: split on every
        // `t` (two pieces read), `.` before the fraction digits is any
        // character, the offset is not range-checked; apiserver messages.
        ParityCase {
            kind: "PfmtDatetimeStrfmt",
            plural: "pfmtdatetimestrfmts",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!format.datetime().validate('2024-01-15T10:30:00+99:99').hasValue() && !format.datetime().validate('2024-01-15T10:30:00,5Z').hasValue() && !format.datetime().validate('2024-01-15T10:30:0012Z').hasValue() && !format.datetime().validate('2024-01-15T10:30:00ZtXYZ').hasValue() && !format.datetime().validate('2024-01-15T12:30:00é00Z').hasValue() && format.datetime().validate('2024-01-15T23:59:60Z').hasValue() && format.datetime().validate('2024-01-15tt10:30:00Z').hasValue() && format.datetime().validate('x').value() == ['invalid datetime'] && format.date().validate('x').value() == ['invalid date']",
                    "message": "datetime"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "format.datetime() follows strfmt.IsDateTime → ACCEPT",
        },
        // A quantity's exponent is an int64 (past it, not a quantity); within
        // kube-cel's supported ±1000 exponents and scales parse as on the
        // apiserver, `0e1000` included.
        ParityCase {
            kind: "PquantExpRange",
            plural: "pquantexpranges",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "isQuantity('1e1000') && isQuantity('1e-1000') && isQuantity('1.5e-999') && isQuantity('0e1000') && !isQuantity('1e9223372036854775808') && !isQuantity('1e+-5')",
                    "message": "exponent"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "quantity exponents within ±1000 parse, past int64 do not → ACCEPT",
        },
        // Quantities follow k8s.io/apimachinery's `resource.Quantity`: no
        // trimming, sub-nano values rounded up to nano, binary quantities
        // capped at MaxInt64, `isInteger`/`asInteger` as Go's `AsInt64` (an
        // int64Amount of a non-negative scale only), `asApproximateFloat` with
        // Go's `math.Pow10`, and `sign` a global function.
        ParityCase {
            kind: "PquantGo",
            plural: "pquantgos",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!isQuantity(' 1 ') && isQuantity('-') && quantity('1e-10') == quantity('1n') && quantity('-1e-10') == quantity('-1n') && !quantity('1000m').isInteger() && !quantity('1.0').isInteger() && !quantity('1e40').isInteger() && quantity('1Ti').asInteger() == 1099511627776 && !quantity('1Pi').isInteger() && quantity('10Ei').compareTo(quantity('9223372036854775807')) == 0 && sign(quantity('-1')) == -1 && quantity('100n').asApproximateFloat() == 1.0000000000000001e-7 && quantity('500m').add(quantity('500m')).compareTo(quantity('1')) == 0 && quantity('1.5Gi').compareTo(quantity('1610612736')) == 0",
                    "message": "quantity"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "quantities follow apimachinery's resource.Quantity → ACCEPT",
        },
        // add / sub copy the receiver (`copy := *q`); a receiver holding an
        // inf.Dec shares it with the copy, which the sum is written into.
        ParityCase {
            kind: "PquantSharedDec",
            plural: "pquantshareddecs",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "![quantity('1.5Gi')].all(q, !q.add(1).isInteger() && q.compareTo(quantity('1.5Gi')) == 0) && [quantity('1.5Gi')].all(q, q.add(1).compareTo(q) == 0 && q.compareTo(quantity('1610612737')) == 0) && [quantity('1.5Gi')].all(q, q.add(q).compareTo(quantity('3Gi')) == 0 && q.compareTo(quantity('3Gi')) == 0) && [quantity('1.5Gi')].all(q, q.sub(1).add(1).compareTo(q) == 0 && q.compareTo(quantity('1.5Gi')) == 0) && [quantity('1')].all(q, q.add(1).compareTo(quantity('2')) == 0 && q.compareTo(quantity('1')) == 0) && [quantity('1.5Gi')].all(q, !(q.add(1) != q))",
                    "message": "shared"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "add/sub on an inf.Dec quantity update it in place, as Go's shared *inf.Dec → ACCEPT",
        },
        // compareTo / isGreaterThan / isLessThan call `q.Cmp(y)`, whose
        // `q.AsDec()` turns an int64Amount receiver into an inf.Dec for good
        // when `y` holds one; `==` compares copies.
        ParityCase {
            kind: "PquantCmpPromotes",
            plural: "pquantcmppromotes",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "[quantity('1')].all(q, q.compareTo(quantity('1.5Gi')) < 0 && !q.isInteger()) && [quantity('1')].all(q, q.isLessThan(quantity('1.5Gi')) && q.add(1).compareTo(q) == 0 && q.compareTo(quantity('2')) == 0) && [quantity('1')].all(q, quantity('1.5Gi').compareTo(q) > 0 && q.isInteger()) && [quantity('1')].all(q, q == quantity('1.5Gi') || q.isInteger()) && [quantity('1')].all(q, q.compareTo(quantity('2')) < 0 && q.isInteger())",
                    "message": "promote"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "comparing with an inf.Dec makes the receiver one, as Go's Cmp → ACCEPT",
        },
        ParityCase {
            kind: "PquantCmpAsInteger",
            plural: "pquantcmpasintegers",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "[quantity('1')].all(q, q.isLessThan(quantity('0.5Gi')) && q.asInteger() == 1)",
                    "message": "asInteger"
                }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "asInteger after isLessThan(<inf.Dec>) is an error → REJECT",
        },
        ParityCase {
            kind: "PquantNaN",
            plural: "pquantnans",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "quantity('0e1000').asApproximateFloat() == quantity('0e1000').asApproximateFloat()",
                    "message": "nan"
                }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "0e1000 is 0 × Pow10(1000), NaN, unequal to itself → REJECT",
        },
        ParityCase {
            kind: "PquantAsInt64",
            plural: "pquantasint64s",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "quantity('500m').add(quantity('500m')).asInteger() == 1",
                    "message": "asInteger"
                }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "500m + 500m is 1000 × 10^-3, which AsInt64 refuses: an error → REJECT",
        },
        // The apiserver's `Equal` asserts a `Quantity` value on the right; the
        // pointer an `add`/`sub` returns makes `==` an error there.
        ParityCase {
            kind: "PquantEqPointer",
            plural: "pquanteqpointers",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "quantity('2') == quantity('1').add(1)", "message": "eq" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "== with an add/sub result on the right is an error → REJECT",
        },
        ParityCase {
            kind: "PquantSignMember",
            plural: "pquantsignmembers",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "quantity('1').sign() == 1", "message": "sign" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "sign is a global function only → rejected at registration",
        },
        // Semver follows blang/semver v4: a numeric pre-release identifier is
        // a uint64, and comparison and `==` ignore build metadata.
        ParityCase {
            kind: "PsemverBlang",
            plural: "psemverblangs",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!isSemver('1.0.0-18446744073709551616') && !isSemver('1.0.0-99999999999999999999999', true) && isSemver('1.0.0-18446744073709551615') && semver('1.0.0+a') == semver('1.0.0+b') && semver('1.0.0-rc.1+build.2').compareTo(semver('1.0.0-rc.1')) == 0 && !semver('1.0.0+b').isGreaterThan(semver('1.0.0+a')) && !semver('1.0.0').isLessThan(semver('1.0.0+a'))",
                    "message": "semver"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "semver follows blang/semver v4 → ACCEPT",
        },
        // isSorted / min / max as the apiserver's lists library: a pair
        // cel-go cannot compare (a NaN) replaces or unsorts nothing, bytes
        // order, and every element must be comparable, even alone. (On a
        // bytes literal the apiserver's cost estimate refuses the rule, so
        // bytes come from a bounded `format: byte` list.)
        ParityCase {
            kind: "PlistCompare",
            plural: "plistcompares",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "[double('NaN'), 1.0].isSorted() && [1.0, double('NaN'), 0.5].min() == 0.5 && [1.0, double('NaN')].max() == 1.0 && [double('NaN'), 1.0].min() != 1.0",
                    "message": "compare"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "NaN pairs are skipped in isSorted/min/max → ACCEPT",
        },
        ParityCase {
            kind: "PlistBytesMin",
            plural: "plistbytesmins",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": {
                        "type": "array",
                        "maxItems": 3,
                        "items": { "type": "string", "format": "byte", "maxLength": 8 }
                    }
                },
                "x-kubernetes-validations": [{
                    "rule": "self.l.min().size() == 1 && self.l.max() == b'c' && !self.l.isSorted()",
                    "message": "bytes"
                }]
            }),
            object: json!({ "l": ["Yw==", "YQ==", "Yg=="] }),
            expect: Expect::Accept,
            note: "format: byte list elements are bytes, which min/max/isSorted order → ACCEPT",
        },
        ParityCase {
            kind: "PlistNullMin",
            plural: "plistnullmins",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "[null].min() == null", "message": "null" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "null is not comparable, even as the only element → REJECT",
        },
        ParityCase {
            kind: "PlistNullSorted",
            plural: "plistnullsorteds",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "[null, null].isSorted()", "message": "null" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "isSorted on nulls is an error → REJECT",
        },
        // ── strings / format vs cel-go v0.26 ──────────────────────────────────
        // kube-cel keeps its own strings and format libraries; the apiserver's
        // are cel-go v0.26 `ext.Strings`.
        //
        // indexOf / lastIndexOf: with an offset, an empty needle returns the offset as is
        // (even negative or past the end), and `lastIndexOf` returns -1 for an
        // offset at or past the length.
        ParityCase {
            kind: "PstrIndexOffset",
            plural: "pstrindexoffsets",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'hello'.lastIndexOf('o', 5) == -1 && 'hello'.indexOf('', -5) == -5 && 'hello'.lastIndexOf('', 100) == 100 && 'hello'.lastIndexOf('', -3) == -3 && 'hello'.lastIndexOf('o', 4) == 4 && 'hello'.lastIndexOf('') == 5 && 'héllo'.lastIndexOf('l', 2) == 2 && 'abc'.indexOf('b', 4294967296) == -1",
                    "message": "index"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "indexOf/lastIndexOf offsets follow cel-go v0.26 → ACCEPT",
        },
        // `%s` converts bytes with ConvertToType(String), which
        // errors on invalid UTF-8; kube-cel printed U+FFFD instead.
        ParityCase {
            kind: "PfmtBadUtf8",
            plural: "pfmtbadutf8s",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "'%s'.format([b'\\xff']) != 'x'", "message": "bytes" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "%s on invalid UTF-8 bytes is an evaluation error → REJECT",
        },
        // `%s` follows cel-go v0.26's formatter (ext/formatting.go):
        // a top-level double is Go `%g` (scientific from exponent 6), a nested
        // one `%.6f` with infinities quoted, map keys sorted, and nested
        // durations, timestamps and bytes written as CEL literals.
        ParityCase {
            kind: "PfmtNested",
            plural: "pfmtnesteds",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'%s'.format([1000000.0]) == '1e+06' && '%s'.format([123456.0]) == '123456' && '%s'.format([[1.5]]) == '[1.500000]' && '%s'.format([[1.0/0.0]]) == '[\"+Inf\"]' && '%s'.format([{'b': 1, 'a': 2}]) == '{\"a\":2, \"b\":1}' && '%s'.format([[duration('1h')]]) == '[duration(\"3600s\")]' && '%s'.format([[timestamp('2024-01-15T10:30:00Z')]]) == '[timestamp(\"2024-01-15T10:30:00Z\")]' && '%s'.format([[b'abc']]) == '[b\"abc\"]' && '%s'.format([['a\"b']]) == '[\"a\\\\\"b\"]'",
                    "message": "nested"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "%s renders doubles and nested values like cel-go v0.26 → ACCEPT",
        },
        // `format()` ports cel-go v0.26's formatter. `%f` / `%e` go through
        // golang.org/x/text's en-US number formatter: thousands grouped, a
        // negative zero unsigned, `∞`, a precision kept in an i16 (`%.65536f`
        // rounds to 0 places); `%e` shows seven significant digits with a
        // superscript exponent and reads the precision as a field width.
        ParityCase {
            kind: "PfmtFloatVerbs",
            plural: "pfmtfloatverbses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'%.2f'.format([1500.0]) == '1,500.00' && '%.2f'.format([-0.001]) == '-0.00' && '%f'.format(['-Infinity']) == '-∞' && '%.65536f'.format([1.5]) == '2' && '%.2e'.format([1500.0]) == '1.500000×10⁰³' && '%.20e'.format([-1500.0]) == '      -1.500000×10⁰³' && '%e'.format([double('NaN')]) == '   NaN'",
                    "message": "float verbs"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "%f / %e print through x/text's en-US formatter → ACCEPT",
        },
        // `%f` takes a double (or "NaN" / "Infinity" / "-Infinity"), not an int.
        ParityCase {
            kind: "PfmtFloatOnInt",
            plural: "pfmtfloatonints",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "'%f'.format([dyn(1)]) != 'x'", "message": "int" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "%f on an int is an evaluation error → REJECT",
        },
        // A negative int in another base is a sign and the magnitude.
        ParityCase {
            kind: "PfmtSignedInts",
            plural: "pfmtsignedintses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'%b'.format([-5]) == '-101' && '%o'.format([-8]) == '-10' && '%x'.format([-255]) == '-ff' && '%X'.format([-255]) == '-FF'",
                    "message": "signed"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "negative %b / %o / %x / %X are sign and magnitude → ACCEPT",
        },
        // `%s`: Go's shortest digits break a tie towards even; go1.26.2's
        // strconv.IsPrint (Unicode 15.0.0) escapes U+0378 (unassigned) and
        // U+1C89 (assigned in Unicode 16); a type value is written bare.
        ParityCase {
            kind: "PfmtSResiduals",
            plural: "pfmtsresidualses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'%s'.format([1000000000000000.25]) == '1.0000000000000002e+15' && '%s'.format([['\\u0378', '\\u1c89']]) == '[\"\\\\u0378\", \"\\\\u1c89\"]' && '%s'.format([[type(1), type(null)]]) == '[int, null_type]'",
                    "message": "s"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "%s tie-break, escaping and type values follow cel-go v0.26 → ACCEPT",
        },
        // A negative count means no limit: Go's `strings.Replace` and
        // `FindAllString` take `n < 0` as "all". kube-cel clamped `replace`'s
        // count to 0 and replaced nothing.
        ParityCase {
            kind: "PstrNegativeCount",
            plural: "pstrnegativecounts",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "'aaa'.replace('a', 'b', -1) == 'bbb' && 'aaa'.replace('a', 'b', 0) == 'aaa' && 'aaa'.replace('a', 'b', 9223372036854775807) == 'bbb' && 'aaa'.findAll('a', -1).size() == 3 && 'aaa'.findAll('a', 0).size() == 0",
                    "message": "count"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "replace/findAll with a negative count apply to every match → ACCEPT",
        },
        // ── cel 0.16 validation environment (kube-rs/kube-cel#14) ─────────────
        // The validation env enables cel-go's `ext.Lists` (kube-cel's port) and the
        // two-variable comprehensions, which the apiserver's base environment
        // has; before 0.9 `sortBy` and the two-variable macros were
        // UnsupportedReference (fail-closed). Bounded lists keep the apiserver's
        // cost estimate under the per-rule budget.
        ParityCase {
            kind: "PsortBy",
            plural: "psortbys",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "integer"} } },
                "x-kubernetes-validations": [{ "rule": "self.l.sortBy(x, -x) == [3, 2, 1]", "message": "sortBy" }]
            }),
            object: json!({ "l": [1, 3, 2] }),
            expect: Expect::Accept,
            note: "sortBy (lists macro) evaluates → ACCEPT",
        },
        ParityCase {
            kind: "PsortByNo",
            plural: "psortbynos",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "integer"} } },
                "x-kubernetes-validations": [{ "rule": "self.l.sortBy(x, x) == self.l", "message": "sorted" }]
            }),
            object: json!({ "l": [1, 3, 2] }),
            expect: Expect::Reject,
            note: "sortBy on an unsorted list differs from the list → REJECT",
        },
        ParityCase {
            kind: "PtwoVar",
            plural: "ptwovars",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "integer"} } },
                "x-kubernetes-validations": [{
                    "rule": "self.l.all(i, v, v >= i) && self.l.transformList(i, v, v * 2) == [2, 6, 4]",
                    "message": "two-var"
                }]
            }),
            object: json!({ "l": [1, 3, 2] }),
            expect: Expect::Accept,
            note: "two-variable all/transformList evaluate → ACCEPT",
        },
        ParityCase {
            kind: "PtwoVarNo",
            plural: "ptwovarnos",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "integer"} } },
                "x-kubernetes-validations": [{ "rule": "self.l.all(i, v, i < 2)", "message": "two-var" }]
            }),
            object: json!({ "l": [1, 3, 2] }),
            expect: Expect::Reject,
            note: "two-variable all with a failing index predicate → REJECT",
        },
        ParityCase {
            kind: "PlistsExt",
            plural: "plistsexts",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "integer"} } },
                "x-kubernetes-validations": [{
                    "rule": "self.l.sort() == [1, 2, 3] && self.l.reverse() == [2, 3, 1] && self.l.slice(0, 2) == [1, 3] && lists.range(3) == [0, 1, 2]",
                    "message": "lists"
                }]
            }),
            object: json!({ "l": [1, 3, 2] }),
            expect: Expect::Accept,
            note: "lists sort/reverse/slice/range evaluate → ACCEPT",
        },
        // A map literal that repeats a key keeps the last entry, and `1` and
        // `1u` are two keys, as in cel-go; `cel` refuses both by default. (Both
        // keys print as `1`, so their order in `%s` follows map iteration, on
        // the apiserver too: only the size is checked.)
        ParityCase {
            kind: "PmapDupKeys",
            plural: "pmapdupkeyses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "{'k': 1, 'k': 2}['k'] == 2 && {1: 'a', 1: 'b'}.size() == 1 && '%s'.format([{1: 'a', 1u: 'b'}]).size() == 14",
                    "message": "keys"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "repeated map keys keep the last entry, 1 and 1u stay apart → ACCEPT",
        },
        // `sort()` and `sortBy()` order equal keys as Go's `sort.Slice` does
        // (go1.26.2): kept in order up to 12 elements, reordered from 13.
        ParityCase {
            kind: "PsortByTies",
            plural: "psortbytieses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "lists.range(20).sortBy(x, x % 2)[0] == 10 && lists.range(13).sortBy(x, x % 2) == [8, 12, 2, 0, 4, 10, 6, 3, 7, 9, 5, 11, 1] && lists.range(12).sortBy(x, x % 2) == [0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11]",
                    "message": "ties"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "sortBy orders equal keys as Go's sort.Slice → ACCEPT",
        },
        ParityCase {
            kind: "PsortSignedZero",
            plural: "psortsignedzeros",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "lists.range(20).map(x, x % 2 == 0 ? (x == 0 ? -0.0 : 0.0) : 1.0).sort().map(x, 1.0 / x < 0.0).indexOf(true) == 5",
                    "message": "zero"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "sort() moves a -0.0 among equal zeros as Go's sort.Slice → ACCEPT",
        },
        // cel-go makes `[]` of a negative range, and sorts a NaN (no
        // comparison with it is "less").
        ParityCase {
            kind: "PlistsEdges",
            plural: "plistsedgeses",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "lists.range(-1) == [] && [double('NaN')].sort().size() == 1 && [1.0, double('NaN'), 2.0].sort().size() == 3",
                    "message": "edges"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "lists.range(-1) is [] and a NaN sorts → ACCEPT",
        },
        // `string(<IP>)` is an Env overload in 0.9 (the member form is gone).
        ParityCase {
            kind: "PstrIp",
            plural: "pstrips",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 64 } },
                "x-kubernetes-validations": [{ "rule": "string(ip(self.a)) == '10.0.0.1'", "message": "ip" }]
            }),
            object: json!({ "a": "10.0.0.1" }),
            expect: Expect::Accept,
            note: "global string(ip(..)) → ACCEPT",
        },
        // Outside the apiserver's base environment: rejected at registration.
        // kube-cel rejects them too (see tests/apiserver_divergence.rs): a call
        // to a function the apiserver does not declare does not compile, and the
        // member `.string()` / string `reverse()` have no overload at
        // evaluation. `math.*` and `isIPv4` used to pass client-side (fail-open).
        ParityCase {
            kind: "PcelBind",
            plural: "pcelbinds",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "cel.bind(x, 2, x > 0)", "message": "bind" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "cel.bind is not in the base env → rejected at registration",
        },
        ParityCase {
            kind: "Pmath",
            plural: "pmaths",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "math.ceil(1.2) > 0.0", "message": "math" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "math.* is not in the base env → rejected at registration",
        },
        ParityCase {
            kind: "PisIPv4",
            plural: "pisipv4s",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 64 } },
                "x-kubernetes-validations": [{ "rule": "isIPv4(self.a)", "message": "ipv4" }]
            }),
            object: json!({ "a": "10.0.0.1" }),
            expect: Expect::RegistrationRejected,
            note: "isIPv4 (kube-cel's own before 0.9) is not in the base env → rejected at registration",
        },
        ParityCase {
            kind: "PipMember",
            plural: "pipmembers",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 64 } },
                "x-kubernetes-validations": [{ "rule": "ip(self.a).string() == '10.0.0.1'", "message": "ip" }]
            }),
            object: json!({ "a": "10.0.0.1" }),
            expect: Expect::RegistrationRejected,
            note: "member <IP>.string() has no overload → rejected at registration",
        },
        ParityCase {
            kind: "PstrRev",
            plural: "pstrrevs",
            schema: json!({
                "type": "object",
                "properties": { "s": { "type": "string", "maxLength": 64 } },
                "x-kubernetes-validations": [{ "rule": "self.s.reverse() == 'cba'", "message": "reverse" }]
            }),
            object: json!({ "s": "abc" }),
            expect: Expect::RegistrationRejected,
            note: "string reverse() has no overload → rejected at registration",
        },
        // A call evaluation never reaches is checked at registration too.
        // kube-cel refuses a function outside the base environment anywhere in
        // the AST at compile time; before, these passed client-side (fail-open).
        ParityCase {
            kind: "PshortIPv4",
            plural: "pshortipv4s",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 64 } },
                "x-kubernetes-validations": [{ "rule": "self.a == '' || isIPv4(self.a)", "message": "ipv4" }]
            }),
            object: json!({ "a": "" }),
            expect: Expect::RegistrationRejected,
            note: "isIPv4 behind a short-circuit → still rejected at registration",
        },
        ParityCase {
            kind: "PemptyTwoVar",
            plural: "pemptytwovars",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 10, "items": {"type": "string", "maxLength": 64} } },
                "x-kubernetes-validations": [{ "rule": "self.l.all(i, v, isIPv4(v))", "message": "ipv4" }]
            }),
            object: json!({ "l": [] }),
            expect: Expect::RegistrationRejected,
            note: "isIPv4 in a comprehension over an empty list → still rejected at registration",
        },
        ParityCase {
            kind: "Pbase64",
            plural: "pbase64s",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "base64.encode(b'hello') == 'aGVsbG8='", "message": "base64" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "base64.* (ext.Encoders) is not in the base env → rejected at registration",
        },
        ParityCase {
            kind: "PjsonPatch",
            plural: "pjsonpatches",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "jsonpatch.escapeKey('a/b') == 'a~1b'", "message": "jsonpatch" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "jsonpatch.* is a MutatingAdmissionPolicy library → rejected at registration",
        },
        ParityCase {
            kind: "PminGlobal",
            plural: "pminglobals",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "min(5, 3) == 3", "message": "min" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "global min(..) has no overload (only <list>.min()) → rejected at registration",
        },
        ParityCase {
            kind: "PmsgIPv4",
            plural: "pmsgipv4s",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "string(isIPv4('1.2.3.4'))" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "isIPv4 in a messageExpression → rejected at registration",
        },
        // Functions the apiserver declares but kube-cel does not implement pass
        // the compile-time name check, so a call evaluation does not reach is
        // accepted on both sides. (Reached, kube-cel rejects the object with
        // UnsupportedReference while the apiserver evaluates it: fail-closed,
        // pinned in tests/apiserver_divergence.rs.)
        ParityCase {
            kind: "PoptUnwrapShort",
            plural: "poptunwrapshorts",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 20 } },
                "x-kubernetes-validations": [{
                    "rule": "self.a == '' || optional.unwrap([optional.of(self.a), optional.none()]) == [self.a]",
                    "message": "unwrap"
                }]
            }),
            object: json!({ "a": "" }),
            expect: Expect::Accept,
            note: "optional.unwrap behind a short-circuit → declared, so both accept",
        },
        ParityCase {
            kind: "PunwrapOptShort",
            plural: "punwrapoptshorts",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "string", "maxLength": 20 } },
                "x-kubernetes-validations": [{
                    "rule": "self.a == '' || [optional.of(self.a), optional.none()].unwrapOpt() == [self.a]",
                    "message": "unwrapOpt"
                }]
            }),
            object: json!({ "a": "" }),
            expect: Expect::Accept,
            note: "<list>.unwrapOpt() behind a short-circuit → declared, so both accept",
        },
        ParityCase {
            kind: "PmsgUnwrap",
            plural: "pmsgunwraps",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "true",
                    "messageExpression": "optional.unwrap([optional.of('x')])[0] + ' rejected'"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "optional.unwrap in the messageExpression of a rule that holds → both accept",
        },
        ParityCase {
            kind: "PauthzCheckShort",
            plural: "pauthzcheckshorts",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || dyn(self).check('get').allowed()", "message": "authz" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "the authorizer library is in the base env, so a CRD rule may name it → both accept",
        },
        // No type checker in `cel` (cel-rust#350): a documented fail-open class.
        // The apiserver type-checks at registration; kube-cel accepts.
        ParityCase {
            kind: "PmixedSorted",
            plural: "pmixedsorteds",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "[1, 2.0].isSorted()", "message": "sorted" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejectedFailOpen,
            note: "mixed-type list literal is a type error → rejected at registration; kube-cel accepts",
        },
        ParityCase {
            kind: "PmsgInt",
            plural: "pmsgints",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "1" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejectedFailOpen,
            note: "non-string messageExpression → rejected at registration; kube-cel accepts",
        },
        // ── Undeclared identifiers ───────────────────────────────────────
        // The apiserver rejects an identifier not declared where it appears,
        // even in a branch evaluation never reaches; kube-cel's compile-time
        // walk refuses it too (before 0.9 these passed client-side). The
        // `Pident*` Accept rows guard against false rejection: a macro's
        // variables in scope, type names, function namespaces.
        ParityCase {
            kind: "PundeclaredIdent",
            plural: "pundeclaredidents",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || undeclared == 1", "message": "ident" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "undeclared identifier in an unevaluated branch → rejected at registration",
        },
        ParityCase {
            kind: "PauthzVarShort",
            plural: "pauthzvarshorts",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "true || authorizer.group('').resource('pods').check('get').allowed()",
                    "message": "authz"
                }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "`authorizer` is a policy variable, undeclared in a CRD rule → rejected at registration",
        },
        ParityCase {
            kind: "PidentObject",
            plural: "pidentobjects",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || object.x == 1", "message": "object" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "`object` is a policy variable, undeclared in a CRD rule → rejected at registration",
        },
        ParityCase {
            kind: "PidentTypo",
            plural: "pidenttypos",
            schema: json!({
                "type": "object",
                "properties": { "replicas": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "self.replicas > 0 || slef.x == 1", "message": "typo" }]
            }),
            object: json!({ "replicas": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a typo behind a branch that holds → rejected at registration",
        },
        ParityCase {
            kind: "PidentMsgTypo",
            plural: "pidentmsgtypos",
            schema: json!({
                "type": "object",
                "properties": {
                    "replicas": { "type": "integer" },
                    "name": { "type": "string", "maxLength": 8 }
                },
                "x-kubernetes-validations": [{
                    "rule": "self.replicas > 0",
                    "messageExpression": "'name: ' + slef.name"
                }]
            }),
            object: json!({ "replicas": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a typo in the messageExpression of a rule that holds → rejected at registration",
        },
        ParityCase {
            kind: "PidentMacroVar",
            plural: "pidentmacrovars",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 4, "items": { "type": "string", "maxLength": 8 } } },
                "x-kubernetes-validations": [{ "rule": "self.l.all(x, x != '') && (true || x == '')", "message": "scope" }]
            }),
            object: json!({ "l": ["a"] }),
            expect: Expect::RegistrationRejected,
            note: "a macro's variable outside the macro → rejected at registration",
        },
        ParityCase {
            kind: "PidentSortByVar",
            plural: "pidentsortbyvars",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "[2, 1].sortBy(x, x)[0] == 1 || x == 1", "message": "scope" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "sortBy's variable outside the macro → rejected at registration",
        },
        ParityCase {
            kind: "PidentDyn",
            plural: "pidentdyns",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || [dyn].size() == 1", "message": "dyn" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "`dyn` is a function, not a type name → rejected at registration",
        },
        ParityCase {
            kind: "PidentDuration",
            plural: "pidentdurations",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || [duration].size() == 1", "message": "duration" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "`duration` as a type value: the planner refuses it → rejected at registration",
        },
        ParityCase {
            kind: "PidentStruct",
            plural: "pidentstructs",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || Foo{a: 1} == 1", "message": "struct" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "constructing an undeclared message type → rejected at registration",
        },
        ParityCase {
            kind: "PidentMacroScopes",
            plural: "pidentmacroscopes",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": { "type": "array", "maxItems": 4, "items": { "type": "string", "maxLength": 8 } },
                    "m": { "type": "object", "maxProperties": 4, "additionalProperties": { "type": "string", "maxLength": 8 } },
                    "n": { "type": "integer" }
                },
                "x-kubernetes-validations": [
                    { "rule": "self.l.all(x, self.l.exists(y, x == y))" },
                    { "rule": "self.l.exists(i, v, i >= 0 && v != '')" },
                    { "rule": "self.m.transformMap(k, v, v + k).size() >= 0" },
                    { "rule": "self.l.map(x, x).filter(y, y != '').size() >= 0" },
                    { "rule": "[1].all(self, self > 0) && [1].all(int, int > 0)" },
                    { "rule": "[3, 1, 2].sortBy(x, -x) == [3, 2, 1]" },
                    { "rule": "self.?n.optMap(v, v + 1).orValue(1) > 0" }
                ]
            }),
            object: json!({ "l": ["a", "b"], "m": { "a": "b" }, "n": 1 }),
            expect: Expect::Accept,
            note: "macro variables in scope, nested, shadowing `self` and `int` → accepted",
        },
        ParityCase {
            kind: "PidentMsgScope",
            plural: "pidentmsgscopes",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "maxItems": 4, "items": { "type": "string", "maxLength": 8 } } },
                "x-kubernetes-validations": [
                    { "rule": "true", "messageExpression": "self.l.exists(x, x == 'a') ? 'has a' : 'no a'" },
                    { "rule": "true", "messageExpression": "self.l.all(i, v, v != '') ? 'ok' : 'empty'" },
                    { "rule": "true", "messageExpression": "self.?l.optMap(l, l.size() > 0).orValue(false) ? 'a' : 'b'" }
                ]
            }),
            object: json!({ "l": ["a"] }),
            expect: Expect::Accept,
            note: "macro variables in scope in a messageExpression → accepted",
        },
        ParityCase {
            kind: "PidentTypes",
            plural: "pidenttypes",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [
                    { "rule": "type(1) == int && type('a') == string && type(true) == bool && type(1u) == uint && type(1.0) == double" },
                    { "rule": "type(b'a') == bytes && type([1]) == list && type({1: 1}) == map && type(null) == null_type && type(int) == type" },
                    { "rule": "type(optional.of(1)) == optional_type && type(duration('1s')) == google.protobuf.Duration && type(timestamp('2024-01-01T00:00:00Z')) == google.protobuf.Timestamp" },
                    { "rule": "true || [net.IP].size() == 1 && [net.CIDR].size() == 1 && [google.protobuf.Empty].size() == 1 && [.int].size() == 1 && google.protobuf.NullValue.NULL_VALUE == 0" },
                    { "rule": "true || google.protobuf.Duration{seconds: 1} == duration('1s')" }
                ]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "type names as values, and a declared message type → accepted",
        },
        ParityCase {
            kind: "PidentQualifiedFns",
            plural: "pidentqualifiedfns",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "!format.dns1123Label().validate('abc').hasValue() && optional.of(1).hasValue() && lists.range(3).size() == 3 && strings.quote('a') == '\"a\"' && sets.contains([1], [1]) && ip.isCanonical('1.2.3.4') && .optional.none().orValue(1) == 1"
                }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "namespaced functions (format., optional., lists., strings., sets., ip.) are no identifiers → accepted",
        },
        ParityCase {
            kind: "ProotApiVersion",
            plural: "prootapiversions",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || apiVersion == 'x.test/v1'", "message": "root" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "no `apiVersion` variable (kube-cel's RootContext bound it before 0.9) → rejected at registration",
        },
        ParityCase {
            kind: "ProotApiGroup",
            plural: "prootapigroups",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || apiGroup == 'x.test'", "message": "root" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "no `apiGroup` variable (kube-cel's RootContext bound it before 0.9) → rejected at registration",
        },
        ParityCase {
            kind: "ProotKind",
            plural: "prootkinds",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || kind == 'ProotKind'", "message": "root" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "no `kind` variable (kube-cel's RootContext bound it before 0.9) → rejected at registration",
        },
        ParityCase {
            kind: "ProotSelfFields",
            plural: "prootselffields",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{
                    "rule": "self.apiVersion == 'x.test/v1' && self.kind == 'ProotSelfFields' && self.metadata.name == 'probe'"
                }]
            }),
            object: json!({ "apiVersion": "x.test/v1", "kind": "ProotSelfFields", "metadata": { "name": "probe" } }),
            expect: Expect::Accept,
            note: "a root rule reads the object's own apiVersion, kind and metadata.name → accepted",
        },
        ParityCase {
            kind: "PidentNestedTypes",
            plural: "pidentnestedtypes",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [
                    { "rule": "true || [google.protobuf.Struct.FieldsEntry].size() == 1" },
                    { "rule": "true || [google.protobuf.Struct.FieldsEntry{}].size() == 1 && [duration{}].size() == 1 && [timestamp{}].size() == 1 && [null_type{}].size() == 1" }
                ]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "Struct's entry type as a value and a message, and the duration/timestamp/null_type constructors → accepted",
        },
        ParityCase {
            kind: "PidentBinderEdges",
            plural: "pidentbinderedges",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [
                    { "rule": "[1].all(.x, true)" },
                    { "rule": "optional.of(1).optMap(__result__, __result__ + 1).value() == 2" }
                ]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "a variable bound as `.x` but unread, and optMap binding `__result__` → accepted",
        },
        ParityCase {
            kind: "PidentDottedBinder",
            plural: "pidentdottedbinders",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || [1].all(.x, x > 0)", "message": "binder" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "a variable bound as `.x` does not declare `x` → rejected at registration",
        },
        ParityCase {
            kind: "PidentLegacyAccu",
            plural: "pidentlegacyaccus",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "[1].all(__result__, true)", "message": "accumulator" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "`__result__` as a standard macro's variable overwrites the accumulator → rejected at registration",
        },
        // No cost model in `cel` (cel-rust discussion #230): the other
        // documented fail-open class. The apiserver estimates each rule's cost
        // at registration; kube-cel evaluates the rule.
        ParityCase {
            kind: "PcostEstimate",
            plural: "pcostestimates",
            schema: json!({
                "type": "object",
                "properties": { "l": { "type": "array", "items": { "type": "string" } } },
                "x-kubernetes-validations": [{ "rule": "self.l.all(x, self.l.exists(y, x == y))", "message": "cost" }]
            }),
            object: json!({"l": ["a", "b"]}),
            expect: Expect::RegistrationRejectedFailOpen,
            note: "a list with no maxItems iterated twice exceeds the estimated cost budget → rejected at registration; kube-cel accepts",
        },
        // ── Rules under schema combinators ───────────────────────────────
        // The apiserver refuses `x-kubernetes-validations` anywhere under
        // `allOf`/`anyOf`/`oneOf`/`not` ("must be empty to be structural");
        // kube-cel used to evaluate those under the first three and ignore
        // those under `not`.
        ParityCase {
            kind: "PcombAllOf",
            plural: "pcomballofs",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "allOf": [{ "x-kubernetes-validations": [{ "rule": "self.a > 0", "message": "a" }] }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a rule under allOf (the object satisfies it) → rejected at registration as not structural",
        },
        ParityCase {
            kind: "PcombAnyOf",
            plural: "pcombanyofs",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "anyOf": [{ "x-kubernetes-validations": [{ "rule": "self.a > 0", "message": "a" }] }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a rule under anyOf (the object satisfies it) → rejected at registration as not structural",
        },
        ParityCase {
            kind: "PcombOneOf",
            plural: "pcomboneofs",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "oneOf": [{ "x-kubernetes-validations": [{ "rule": "self.a > 0", "message": "a" }] }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a rule under oneOf (the object satisfies it) → rejected at registration as not structural",
        },
        ParityCase {
            kind: "PcombNot",
            plural: "pcombnots",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "not": { "x-kubernetes-validations": [{ "rule": "self.a > 0", "message": "a" }] }
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a rule under not (the object satisfies it) → rejected at registration as not structural",
        },
        ParityCase {
            kind: "PcombNested",
            plural: "pcombnesteds",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "allOf": [{ "properties": { "a": { "x-kubernetes-validations": [{ "rule": "self > 0", "message": "a" }] } } }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a rule on a property inside allOf (the object satisfies it) → rejected at registration as not structural",
        },
        ParityCase {
            kind: "PcombNoRules",
            plural: "pcombnorules",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "anyOf": [{ "required": ["a"] }],
                "allOf": [{ "x-kubernetes-validations": [] }],
                "not": { "properties": { "a": { "maximum": 0 } } },
                "x-kubernetes-validations": [{ "rule": "self.a > 0", "message": "a" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::Accept,
            note: "combinators without rules (or with an empty list) beside a root rule → accepted",
        },
        // ── Schema and value fidelity ─────────────────────────────────────
        // `x-kubernetes-preserve-unknown-fields` does not stop the apiserver
        // from defaulting into, and evaluating the rules under,
        // `additionalProperties`; kube-cel skipped both.
        ParityCase {
            kind: "PpresApDefault",
            plural: "ppresapdefaults",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "x-kubernetes-preserve-unknown-fields": true,
                        "additionalProperties": {
                            "type": "object",
                            "properties": { "x": { "type": "integer", "default": 5 } }
                        }
                    }
                },
                "x-kubernetes-validations": [{ "rule": "self.m.a.x == 5", "message": "defaulted" }]
            }),
            object: json!({ "m": { "a": {} } }),
            expect: Expect::Accept,
            note: "a default under additionalProperties with preserve-unknown-fields is applied → accepted",
        },
        ParityCase {
            kind: "PpresApRule",
            plural: "ppresaprules",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "x-kubernetes-preserve-unknown-fields": true,
                        "maxProperties": 3,
                        "additionalProperties": {
                            "type": "integer",
                            "x-kubernetes-validations": [{ "rule": "self >= 0", "message": "non-negative" }]
                        }
                    }
                }
            }),
            object: json!({ "m": { "a": -1 } }),
            expect: Expect::Reject,
            note: "a rule under additionalProperties with preserve-unknown-fields is evaluated → rejected",
        },
        // A `null` the apiserver prunes before defaulting: a field or map value
        // under a node neither `nullable` nor defaulted. And nothing is
        // evaluated at a null node.
        ParityCase {
            kind: "PnullPruneField",
            plural: "pnullprunefields",
            schema: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer" },
                    "o": { "type": "object", "properties": { "y": { "type": "integer" } } }
                },
                "x-kubernetes-validations": [{ "rule": "!has(self.x) && !has(self.o.y)", "message": "pruned" }]
            }),
            object: json!({ "x": null, "o": { "y": null } }),
            expect: Expect::Accept,
            note: "a non-nullable null field without a default is pruned → accepted",
        },
        ParityCase {
            kind: "PnullPruneMap",
            plural: "pnullprunemaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": { "type": "object", "maxProperties": 3, "additionalProperties": { "type": "integer" } }
                },
                "x-kubernetes-validations": [{ "rule": "self.m.size() == 1 && !('a' in self.m)", "message": "pruned" }]
            }),
            object: json!({ "m": { "a": null, "b": 1 } }),
            expect: Expect::Accept,
            note: "a non-nullable null map value without a default is pruned → accepted",
        },
        ParityCase {
            kind: "PnullPruneDefault",
            plural: "pnullprunedefaults",
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "integer", "default": 5 } },
                "x-kubernetes-validations": [{ "rule": "self.x == 5", "message": "defaulted" }]
            }),
            object: json!({ "x": null }),
            expect: Expect::Accept,
            note: "a non-nullable null with a default takes the default → accepted",
        },
        ParityCase {
            kind: "PnullNode",
            plural: "pnullnodes",
            schema: json!({
                "type": "object",
                "properties": {
                    "o": {
                        "type": "object",
                        "nullable": true,
                        "properties": {
                            "a": { "type": "integer", "x-kubernetes-validations": [{ "rule": "false", "message": "a" }] }
                        },
                        "x-kubernetes-validations": [{ "rule": "false", "message": "o" }]
                    }
                }
            }),
            object: json!({ "o": null }),
            expect: Expect::Accept,
            note: "no rule runs at a null node, nor below it → accepted",
        },
        // A field name CEL cannot spell (`1abc`, `a b`) is left out of the
        // apiserver's declarations, but still compared in object equality.
        ParityCase {
            kind: "PfieldUnspellable",
            plural: "pfieldunspellables",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": {
                        "type": "array",
                        "maxItems": 3,
                        "items": {
                            "type": "object",
                            "properties": { "1abc": { "type": "integer" }, "a b": { "type": "integer" }, "ok": { "type": "integer" } }
                        }
                    }
                },
                "x-kubernetes-validations": [{ "rule": "self.l[0] != self.l[1] && self.l[1] != self.l[2]", "message": "eq" }]
            }),
            object: json!({ "l": [{ "1abc": 1, "ok": 1 }, { "1abc": 2, "ok": 1 }, { "1abc": 2, "a b": 1, "ok": 1 }] }),
            expect: Expect::Accept,
            note: "objects differing only in a field name CEL cannot spell are not equal → accepted",
        },
        // Rule fields the apiserver refuses at registration, beside the
        // expressions (apiextensions validation.go): a blank or multi-line
        // `message`, a multi-line rule without one, an unsupported `reason`,
        // a `fieldPath` that names no field of the rule's node.
        ParityCase {
            kind: "PruleFieldBlankMessage",
            plural: "prulefieldblankmessages",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true", "message": "   " }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a blank message → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldMessageBreak",
            plural: "prulefieldmessagebreaks",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true", "message": "a\nb" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a message with a line break → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldMultiline",
            plural: "prulefieldmultilines",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true &&\ntrue" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a multi-line rule without a message → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldReason",
            plural: "prulefieldreasons",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true", "reason": "FieldValueTooLong" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "an unsupported reason → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldPathMissing",
            plural: "prulefieldpathmissings",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true", "fieldPath": ".nope" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a fieldPath naming no field → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldPathNoDot",
            plural: "prulefieldpathnodots",
            schema: json!({
                "type": "object",
                "properties": { "a": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "true", "fieldPath": "a" }]
            }),
            object: json!({ "a": 1 }),
            expect: Expect::RegistrationRejected,
            note: "a fieldPath without a leading dot → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldPathNullMap",
            plural: "prulefieldpathnullmaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "additionalProperties": null,
                        "x-kubernetes-validations": [{ "rule": "true", "fieldPath": ".k" }]
                    }
                }
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "a null additionalProperties is no map, so `.k` names no field → rejected at registration",
        },
        ParityCase {
            kind: "PruleFieldPathMaps",
            plural: "prulefieldpathmaps",
            schema: json!({
                "type": "object",
                "properties": {
                    "m": {
                        "type": "object",
                        "properties": {},
                        "maxProperties": 3,
                        "additionalProperties": { "type": "object", "properties": { "x": { "type": "string", "maxLength": 8 } } },
                        "x-kubernetes-validations": [
                            { "rule": "true", "fieldPath": ".k" },
                            { "rule": "true", "fieldPath": ".k.x" }
                        ]
                    },
                    "closed": {
                        "type": "object",
                        "additionalProperties": false,
                        "x-kubernetes-validations": [{ "rule": "true", "fieldPath": ".k" }]
                    }
                },
                "x-kubernetes-validations": [
                    { "rule": "true", "fieldPath": ".m.k" },
                    { "rule": "true", "fieldPath": ".m['k'].x" }
                ]
            }),
            object: json!({ "m": { "k": { "x": "v" } } }),
            expect: Expect::Accept,
            note: "empty properties beside a map schema, and additionalProperties: false, still name any key → accepted",
        },
        ParityCase {
            kind: "PruleFieldValid",
            plural: "prulefieldvalids",
            schema: json!({
                "type": "object",
                "properties": {
                    "a": { "type": "integer" },
                    "m": {
                        "type": "object",
                        "maxProperties": 3,
                        "additionalProperties": { "type": "object", "properties": { "x": { "type": "integer" } } }
                    }
                },
                "x-kubernetes-validations": [
                    { "rule": "true &&\ntrue", "message": "multi-line, with a message" },
                    { "rule": "true", "message": " padded " },
                    { "rule": "true", "message": "trailing line break\n" },
                    { "rule": "true", "reason": "FieldValueForbidden", "fieldPath": ".a" },
                    { "rule": "true", "fieldPath": "['a']" },
                    { "rule": "true", "fieldPath": ".m.anykey.x" },
                    { "rule": "true", "fieldPath": ".m['a.b'].x" }
                ]
            }),
            object: json!({ "a": 1, "m": { "k": { "x": 1 } } }),
            expect: Expect::Accept,
            note: "valid messages, reason and fieldPaths (a property, a quoted name, a map key) → accepted",
        },
        // ── Schema nodes the object does not reach (kube-rs/kube-cel#14) ─────
        // The apiserver compiles every rule, and checks the nesting depth, when
        // it registers the CRD, whatever objects later look like. Each row's
        // object leaves the bad node unreached: an omitted property, the items
        // of an empty list, a rule nested deeper than 64 levels.
        ParityCase {
            kind: "PunreachedCall",
            plural: "punreachedcalls",
            schema: json!({
                "type": "object",
                "properties": {
                    "a": { "type": "string", "x-kubernetes-validations": [{ "rule": "isIPv4(self)", "message": "ipv4" }] }
                }
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "unsupported call on an omitted property → rejected at registration",
        },
        ParityCase {
            kind: "PunreachedSyntax",
            plural: "punreachedsyntaxes",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": {
                        "type": "array",
                        "items": { "type": "string", "x-kubernetes-validations": [{ "rule": "self >", "message": "syntax" }] }
                    }
                }
            }),
            object: json!({"l": []}),
            expect: Expect::RegistrationRejected,
            note: "syntax error in the items of an empty list → rejected at registration",
        },
        ParityCase {
            kind: "PunreachedMsgExpr",
            plural: "punreachedmsgexprs",
            schema: json!({
                "type": "object",
                "properties": {
                    "a": {
                        "type": "string",
                        "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "invalid >=" }]
                    }
                }
            }),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "messageExpression syntax error on an omitted property → rejected at registration",
        },
        // The apiserver has no nesting limit (v1.36.1 registered a 400-level
        // CRD), so a valid rule past 64 levels below a shallow object passes on
        // both sides; kube-cel still compiles it, and refuses one that does not
        // compile. (An object that reaches past 64 levels is kube-cel's
        // fail-closed `SchemaTooDeep`, in tests/apiserver_divergence.rs.)
        ParityCase {
            kind: "PunreachedDeep",
            plural: "punreacheddeeps",
            schema: deep_schema(70, "self >= 0"),
            object: json!({}),
            expect: Expect::Accept,
            note: "valid rule 70 levels down, shallow object → ACCEPT (no apiserver nesting limit)",
        },
        ParityCase {
            kind: "PunreachedDeepSyntax",
            plural: "punreacheddeepsyntaxes",
            schema: deep_schema(70, "self >"),
            object: json!({}),
            expect: Expect::RegistrationRejected,
            note: "syntax error 70 levels down, shallow object → rejected at registration",
        },
        // ── `oldSelf` registration rules (apiextensions validation.go) ────────
        // A rule reading `oldSelf` in the items of a list that is not
        // `x-kubernetes-list-type: map` (whose items never correlate), and
        // `optionalOldSelf` on a rule that does not read `oldSelf`, are refused
        // when the CRD registers.
        ParityCase {
            kind: "PoldSelfAtomicItems",
            plural: "poldselfatomicitemses",
            schema: json!({
                "type": "object",
                "properties": {
                    "e": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": { "v": { "type": "integer" } },
                            "x-kubernetes-validations": [{ "rule": "self.v >= oldSelf.v", "message": "v" }]
                        }
                    }
                }
            }),
            object: json!({"e": [{"v": 1}]}),
            expect: Expect::RegistrationRejected,
            note: "oldSelf in the items of an atomic list → rejected at registration",
        },
        ParityCase {
            kind: "PoptOldSelfUnused",
            plural: "poptoldselfunuseds",
            schema: json!({
                "type": "object",
                "properties": { "n": { "type": "integer" } },
                "x-kubernetes-validations": [{ "rule": "self.n >= 0", "message": "n", "optionalOldSelf": false }]
            }),
            object: json!({"n": 1}),
            expect: Expect::RegistrationRejected,
            note: "optionalOldSelf on a rule without oldSelf → rejected at registration",
        },
        // An empty `messageExpression` is no messageExpression: the apiserver
        // registers it and falls back to `message` (kube-cel compiled it).
        ParityCase {
            kind: "PemptyMsgExpr",
            plural: "pemptymsgexprs",
            schema: json!({
                "type": "object",
                "properties": {
                    "a": { "type": "string", "x-kubernetes-validations": [{ "rule": "true", "messageExpression": "" }] }
                }
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "empty messageExpression is unset → ACCEPT",
        },
        // The apiserver counts any reference to `oldSelf` as a transition rule,
        // a comprehension variable of that name included.
        ParityCase {
            kind: "PlocalOldSelfItems",
            plural: "plocaloldselfitemses",
            schema: json!({
                "type": "object",
                "properties": {
                    "l": {
                        "type": "array",
                        "maxItems": 1,
                        "items": { "type": "integer", "x-kubernetes-validations": [{ "rule": "[1].all(oldSelf, oldSelf > 0)", "message": "l" }] }
                    }
                }
            }),
            object: json!({"l": [1]}),
            expect: Expect::RegistrationRejected,
            note: "comprehension variable named oldSelf in non-map list items → rejected at registration",
        },
        ParityCase {
            kind: "PlocalOldSelfOptional",
            plural: "plocaloldselfoptionals",
            schema: json!({
                "type": "object",
                "x-kubernetes-validations": [{ "rule": "[1].all(oldSelf, oldSelf > 0)", "message": "o", "optionalOldSelf": true }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "comprehension variable named oldSelf makes optionalOldSelf legal → ACCEPT",
        },
        ParityCase {
            kind: "PlocalOldSelfCreate",
            plural: "plocaloldselfcreates",
            schema: json!({
                "type": "object",
                "x-kubernetes-validations": [{ "rule": "[1].all(oldSelf, oldSelf > 5)", "message": "o" }]
            }),
            object: json!({}),
            expect: Expect::Accept,
            note: "a rule naming oldSelf is a transition rule, skipped on create → ACCEPT",
        },
    ]
}

/// One policy-registration case: a ValidatingAdmissionPolicy validation the
/// apiserver either registers or rejects, and kube-cel's `VapEvaluator` must
/// compile or refuse alike. The runner wraps it in a policy with no
/// `spec.variables` (kube-cel does not model them) and, with `param_kind`, a
/// `paramKind`, which kube-cel gets as `.has_param_kind(true)`.
pub struct VapCase {
    /// The policy name; unique across the table.
    pub name: &'static str,
    pub expression: &'static str,
    pub message: Option<&'static str>,
    pub message_expression: Option<&'static str>,
    pub param_kind: bool,
    pub registers: bool,
    pub note: &'static str,
}

/// Policy-registration cases: the variables a policy expression declares, a
/// macro's variables, type names.
pub fn vap_cases() -> Vec<VapCase> {
    let case = |name, expression, message_expression, registers, note| VapCase {
        name,
        expression,
        message: None,
        message_expression,
        param_kind: true,
        registers,
        note,
    };
    vec![
        case(
            "vap-variables",
            "object.metadata.name != '' || oldObject == null || request.name == '' || params == null || namespaceObject == null",
            None,
            true,
            "the policy variables → registered",
        ),
        case(
            "vap-authorizer",
            "true || authorizer.group('').resource('pods').check('get').allowed() || authorizer.requestResource.check('get').allowed()",
            None,
            true,
            "`authorizer` and `authorizer.requestResource` → registered",
        ),
        case(
            "vap-composition",
            "true || variables.foo == 1",
            None,
            false,
            "`variables` in a policy without spec.variables → rejected",
        ),
        case(
            "vap-composition-root",
            "true || variables == null",
            None,
            false,
            "the `variables` root without spec.variables → rejected",
        ),
        case(
            "vap-request",
            "true || request.kind.group == '' && request.userInfo.username == ''",
            None,
            true,
            "fields of `request` → registered",
        ),
        case(
            "vap-macro-scope",
            "true || [object].all(o, o != null) && [1].all(object, object > 0)",
            None,
            true,
            "a macro variable, one shadowing `object` → registered",
        ),
        case(
            "vap-types",
            "type(1) == int && (true || [net.IP].size() == 1 && [kubernetes.AdmissionRequest{}].size() == 1)",
            None,
            true,
            "type names, and a message type a policy declares → registered",
        ),
        case(
            "vap-message",
            "true",
            Some(
                "object.metadata.name + ' ' + string(params == null) + ' ' + string(namespaceObject == null)",
            ),
            true,
            "a messageExpression reading policy variables → registered",
        ),
        case(
            "vap-self",
            "true || self.x == 1",
            None,
            false,
            "`self` is a CRD variable, undeclared in a policy → rejected",
        ),
        case(
            "vap-oldself",
            "true || oldSelf.x == 1",
            None,
            false,
            "`oldSelf` is a CRD variable, undeclared in a policy → rejected",
        ),
        case(
            "vap-typo",
            "true || slef.x == 1",
            None,
            false,
            "a typo in an unevaluated branch → rejected",
        ),
        case(
            "vap-macro-var",
            "[1].all(x, x > 0) && (true || x == 1)",
            None,
            false,
            "a macro's variable outside the macro → rejected",
        ),
        case(
            "vap-struct",
            "true || [kubernetes.Quantity{}].size() == 1",
            None,
            false,
            "constructing a type the policy does not declare → rejected",
        ),
        case(
            "vap-message-typo",
            "true",
            Some("'bad: ' + string(slef)"),
            false,
            "a typo in the messageExpression → rejected",
        ),
        case(
            "vap-params",
            "true || params.data.max == '1'",
            None,
            true,
            "`params` in a policy with a paramKind → registered",
        ),
        VapCase {
            name: "vap-params-no-paramkind",
            expression: "true || params.data.max == '1'",
            message: None,
            message_expression: None,
            param_kind: false,
            registers: false,
            note: "`params` in a policy without a paramKind → rejected",
        },
        case(
            "vap-message-authorizer",
            "true",
            Some("true ? 'ok' : (authorizer.requestResource.check('get').allowed() ? 'yes' : 'no')"),
            false,
            "`authorizer` in a messageExpression, which has none → rejected",
        ),
        case(
            "vap-message-authorizer-macro",
            "true",
            Some("[1].all(authorizer, authorizer > 0) ? 'yes' : 'no'"),
            true,
            "a macro binding `authorizer` in a messageExpression → registered",
        ),
        case(
            "vap-legacy-accu",
            "[1].all(__result__, true)",
            None,
            false,
            "`__result__` as a standard macro's variable → rejected",
        ),
        case(
            "vap-nested-types",
            "true || [google.protobuf.Struct.FieldsEntry{}].size() == 1 && [duration{}].size() == 1",
            None,
            true,
            "Struct's entry type and the duration constructor → registered",
        ),
        VapCase {
            name: "vap-message-blank",
            expression: "true",
            message: Some("   "),
            message_expression: None,
            param_kind: true,
            registers: false,
            note: "a blank static message → rejected",
        },
        VapCase {
            name: "vap-message-break",
            expression: "true",
            message: Some("a\nb"),
            message_expression: None,
            param_kind: true,
            registers: false,
            note: "a static message with a line break → rejected",
        },
        VapCase {
            name: "vap-message-padded",
            expression: "true",
            message: Some(" x "),
            message_expression: None,
            param_kind: true,
            registers: true,
            note: "a padded static message → registered",
        },
        VapCase {
            name: "vap-no-paramkind",
            expression: "object.metadata.name != '' || [1].all(params, params > 0)",
            message: None,
            message_expression: None,
            param_kind: false,
            registers: true,
            note: "no `params` read in a policy without a paramKind, a macro's `params` → registered",
        },
    ]
}
