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
/// `RegistrationRejectedFailOpen` where it accepts it, the one documented
/// fail-open class (no type checker in `cel`, cel-rust#350; see
/// `tests/apiserver_divergence.rs`).
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
                    "rule": "'hello'.lastIndexOf('o', 5) == -1 && 'hello'.indexOf('', -5) == -5 && 'hello'.lastIndexOf('', 100) == 100 && 'hello'.lastIndexOf('', -3) == -3 && 'hello'.lastIndexOf('o', 4) == 4 && 'hello'.lastIndexOf('') == 5 && 'héllo'.lastIndexOf('l', 2) == 2",
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
        // ── cel 0.16 validation environment (kube-rs/kube-cel#14) ─────────────
        // The validation env enables `cel`'s upstream `lists` extension and the
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
            note: "sortBy (upstream lists macro) evaluates → ACCEPT",
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
            note: "upstream lists sort/reverse/slice/range evaluate → ACCEPT",
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
        // to a function the validation env lacks does not compile, and the
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
            note: "isIPv4 is a kube-cel extra, not in the base env → rejected at registration",
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
        // No type checker in `cel` (cel-rust#350): the one documented fail-open
        // class. The apiserver type-checks at registration; kube-cel accepts.
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
        ParityCase {
            kind: "PundeclaredIdent",
            plural: "pundeclaredidents",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "true || undeclared == 1", "message": "ident" }]
            }),
            object: json!({}),
            expect: Expect::RegistrationRejectedFailOpen,
            note: "undeclared identifier in an unevaluated branch → rejected at registration; kube-cel accepts",
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
            expect: Expect::RegistrationRejectedFailOpen,
            note: "`authorizer` is a policy variable, undeclared in a CRD rule → rejected at registration; kube-cel accepts",
        },
    ]
}
