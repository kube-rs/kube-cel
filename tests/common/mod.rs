//! Shared parity-case table — the single source of truth for BOTH parity
//! runners (`apiserver_parity.rs` via kind/kubectl, `envtest_parity.rs` via the
//! in-process `envtest` apiserver). Each row is pure data: a CRD-expressible
//! schema (carrying an `x-kubernetes-validations` rule), an instance object, and
//! the expected outcome. A case is written ONCE here and proven under both
//! control planes; any disagreement between the two runners is itself a finding.
//!
//! This module is included (`mod common;`) by each parity test crate, so it is
//! compiled per-runner. It pulls in `kube_cel` + `serde_json`, both available
//! whenever the `validation` feature is on (which every parity runner requires).

// Each runner uses a subset of these helpers, so unused items are expected.
#![allow(dead_code)]

use kube_cel::Validator;
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
/// runtime verdicts both engines must agree on; `RegistrationRejected` is the
/// kube-cel-only boundary where the apiserver refuses the rule at CRD
/// registration (no valid CRD can carry it).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Expect {
    Accept,
    Reject,
    RegistrationRejected,
}

impl Expect {
    /// The runtime verdict this expectation pins, for the register-able cases.
    /// Panics on `RegistrationRejected`, which never reaches runtime evaluation.
    pub fn verdict(self) -> Verdict {
        match self {
            Expect::Accept => Verdict::Accept,
            Expect::Reject => Verdict::Reject,
            Expect::RegistrationRejected => {
                panic!("RegistrationRejected has no runtime verdict")
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
pub fn kubecel_verdict(schema: &Value, object: &Value) -> Verdict {
    match Validator::new().validate(schema, object, None) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
}

/// kube-cel's verdict via the explicit `validate_with_defaults` alias. Should be
/// identical to [`kubecel_verdict`] since 0.8; runners assert both to guard the
/// alias against drift.
pub fn kubecel_verdict_with_defaults(schema: &Value, object: &Value) -> Verdict {
    match Validator::new().validate_with_defaults(schema, object, None) {
        Ok(()) => Verdict::Accept,
        Err(_) => Verdict::Reject,
    }
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
            object: Value::Null,
            expect: Expect::RegistrationRejected,
            note: "`in` on a free-form map is not expressible → rejected at registration",
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
        // ── FIDELITY-SWEEP findings (2026-06-19), fixed in 0.8 ────────────────
        // Each row reproduces a measured divergence with the OLD (buggy) expected
        // value, so it pins that kube-cel now agrees with the apiserver.
        //
        // F1a — `string.format()` `%e` used Rust's exponent (`1.50e3`); cel-go
        // emits Go's signed, zero-padded `1.50e+03`. The old equality is now
        // false on BOTH sides → REJECT (was fail-open ACCEPT).
        ParityCase {
            kind: "PfmtExp",
            plural: "pfmtexps",
            schema: json!({
                "type": "object",
                "properties": {},
                "x-kubernetes-validations": [{ "rule": "'val: %.2e'.format([1500.0]) == 'val: 1.50e3'", "message": "exp" }]
            }),
            object: json!({}),
            expect: Expect::Reject,
            note: "format %e is Go-style 1.50e+03, not 1.50e3 → REJECT (F1a)",
        },
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
    ]
}
