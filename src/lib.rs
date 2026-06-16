//! Kubernetes CEL extension functions for the `cel` crate.
//!
//! This crate provides the Kubernetes-specific CEL (Common Expression Language) functions
//! that are available in Kubernetes CRD validation rules, built on top of the `cel` crate.
//!
//! # Usage
//!
//! Register the compiled-in functions onto a [`cel::Context`] via the
//! [`KubeCelExt`] extension trait:
//!
//! ```rust
//! use kube_cel::{cel, KubeCelExt};
//!
//! let ctx = cel::Context::default().with_all();
//! # let _ = ctx;
//! ```
//!
//! See [`KubeCelExt`] for the borrowed-context form and the
//! function-group → upstream-source table.
//!
//! # Version coherence
//!
//! This crate's public signatures use [`cel::Context`] and [`cel::Value`], so a
//! `cel` version mismatch between your crate and `kube-cel` surfaces as a cryptic
//! `Context` type mismatch. To avoid it, import `cel` **through** this crate
//! rather than declaring a separate `cel` dependency:
//!
//! ```rust
//! use kube_cel::cel; // re-export guaranteed to match kube-cel's `cel`
//! # let _ = cel::Context::default();
//! ```
//!
//! # Feature model
//!
//! Granularity is controlled at compile time through cargo features — there is
//! no runtime per-library registration method. The `default` feature set enables
//! every extension-function group. To narrow the surface you must disable the
//! defaults explicitly, otherwise the listed features are simply added on top of
//! the (already complete) default set and have no narrowing effect:
//!
//! ```toml
//! # Only the string + list helpers:
//! kube-cel = { version = "0.7", default-features = false, features = ["strings", "lists"] }
//! ```
//!
//! The validation pipeline (CRD `x-kubernetes-validations`, VAP, static analysis)
//! lives behind the `validation` feature (see below when it is enabled); it is
//! **not** part of `default`.
//!
//! The `full` umbrella feature enables everything — all extension-function
//! groups *and* the `validation` engine. Use it to restore the whole surface
//! after narrowing, or to opt into validation alongside the default functions:
//!
//! ```toml
//! kube-cel = { version = "0.7", features = ["full"] }
//! ```
//!
//! # Versioning and stability
//!
//! kube-cel is pre-1.0 and **cannot reach 1.0 until the `cel` crate does** — its
//! public surface exposes [`cel::Context`]/[`cel::Value`], and a crate cannot be
//! stable while its public dependencies are not (Rust API Guidelines C-STABLE).
//! After `cel` 1.0, kube-cel 1.x tracks `cel` 1.y; a `cel` major forces a
//! kube-cel major. Two stability tiers: **Tier 1** (committed) is the
//! registration surface — [`KubeCelExt`] and the `cel` re-export; **Tier 2**
//! (evolving, `validation` feature) is the validation engine, whose surface may
//! still change across pre-1.0 minors. See the README for details.
#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(missing_docs)]
// The validation-pipeline section links to feature-gated items, so it is only
// emitted when `validation` is enabled. Keeping it out of the always-compiled
// `//!` block is what keeps `cargo doc --no-deps` (default features) free of
// broken intra-doc links.
#![cfg_attr(
    feature = "validation",
    doc = r#"
# CRD Validation Pipeline (feature = `validation`)

Compile and evaluate `x-kubernetes-validations` CEL rules client-side,
without an API server.

```toml
kube-cel = { version = "0.7", features = ["validation"] }
```

```rust
use kube_cel::Validator;
use serde_json::json;

let schema = json!({
    "type": "object",
    "x-kubernetes-validations": [
        {"rule": "self.replicas >= 0", "message": "must be non-negative"}
    ],
    "properties": { "replicas": {"type": "integer"} }
});

let object = json!({"replicas": -1});
let errors = Validator::new().validate(&schema, &object, None).unwrap_err();
assert_eq!(errors.len(), 1);
```

For repeated validation against the same schema, pre-compile with
[`compile_schema`] and use [`Validator::validate_compiled`].
"#
)]

/// Re-export of the [`cel`] crate, for version coherence (see the crate-level
/// docs). Importing `cel` types via `kube_cel::cel` guarantees they match the
/// `cel` version this crate was built against.
pub use cel;

// Source is grouped by the crate's two tiers (see the "Versioning and
// stability" docs). Both are internal module trees; the public API is the flat
// set of crate-root re-exports below, not the file layout.
//
// `functions/` — Tier 1, the Kubernetes CEL extension-function port (always on,
// each library behind its own feature). Public entry point: `KubeCelExt`.
mod ext;
mod functions;

// `validation/` — Tier 2, the client-side validation engine (`validation`
// feature). Its public items are re-exported flatly below.
#[cfg(feature = "validation")] mod validation;

pub use ext::KubeCelExt;

#[cfg(feature = "validation")]
pub use crate::validation::{
    ErrorKind, RootContext, ValidationError, ValidationErrors, Validator,
    analysis::{
        AnalysisWarning, ScopeContext, WarningKind, analyze_rule, check_rule_scope, estimate_rule_cost,
    },
    compilation::{CompilationError, CompilationResult, CompiledSchema, Rule, compile_schema},
    defaults::apply_defaults,
    validate, validate_compiled,
    values::SchemaFormat,
    vap::{
        AdmissionRequest, CompiledVapExpression, GroupVersionKind, GroupVersionResource, VapError,
        VapEvaluator, VapEvaluatorBuilder, VapExpression, VapResult,
    },
};

/// Registers all compiled-in Kubernetes CEL extension functions into `ctx`.
///
/// Re-exported from [`functions`] so in-crate callers can keep writing
/// `crate::register_all`.
pub(crate) use functions::register_all;

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(unused_imports)] use std::sync::Arc;

    use cel::{Context, Program, Value};

    #[allow(dead_code)]
    fn eval(expr: &str) -> Value {
        let ctx = Context::default().with_all();
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_integration_strings() {
        assert_eq!(eval("'hello'.charAt(1)"), Value::String(Arc::new("e".into())));
        assert_eq!(
            eval("'HELLO'.lowerAscii()"),
            Value::String(Arc::new("hello".into()))
        );
        assert_eq!(
            eval("'  hello  '.trim()"),
            Value::String(Arc::new("hello".into()))
        );
    }

    #[test]
    #[cfg(feature = "lists")]
    fn test_integration_lists() {
        assert_eq!(eval("[1, 2, 3].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[3, 1, 2].isSorted()"), Value::Bool(false));
        assert_eq!(eval("[1, 2, 3].sum()"), Value::Int(6));
    }

    #[test]
    #[cfg(feature = "sets")]
    fn test_integration_sets() {
        assert_eq!(eval("sets.contains([1, 2, 3], [1, 2])"), Value::Bool(true));
        assert_eq!(eval("sets.intersects([1, 2], [2, 3])"), Value::Bool(true));
    }

    #[test]
    #[cfg(feature = "regex_funcs")]
    fn test_integration_regex() {
        assert_eq!(
            eval("'hello world'.find('[a-z]+')"),
            Value::String(Arc::new("hello".into()))
        );
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_dispatch_index_of_string() {
        assert_eq!(eval("'hello world'.indexOf('world')"), Value::Int(6));
        assert_eq!(eval("'hello'.indexOf('x')"), Value::Int(-1));
    }

    #[test]
    #[cfg(feature = "lists")]
    fn test_dispatch_index_of_list() {
        assert_eq!(eval("[1, 2, 3].indexOf(2)"), Value::Int(1));
        assert_eq!(eval("[1, 2, 3].indexOf(4)"), Value::Int(-1));
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_dispatch_last_index_of_string() {
        assert_eq!(eval("'abcabc'.lastIndexOf('abc')"), Value::Int(3));
    }

    #[test]
    #[cfg(feature = "lists")]
    fn test_dispatch_last_index_of_list() {
        assert_eq!(eval("[1, 2, 3, 2].lastIndexOf(2)"), Value::Int(3));
    }

    #[test]
    #[cfg(feature = "format")]
    fn test_integration_format() {
        assert_eq!(
            eval("'hello %s'.format(['world'])"),
            Value::String(Arc::new("hello world".into()))
        );
        assert_eq!(
            eval("'%d items'.format([5])"),
            Value::String(Arc::new("5 items".into()))
        );
    }

    #[test]
    #[cfg(feature = "semver_funcs")]
    fn test_integration_semver() {
        assert_eq!(eval("isSemver('1.2.3')"), Value::Bool(true));
        assert_eq!(eval("semver('1.2.3').major()"), Value::Int(1));
        assert_eq!(
            eval("semver('2.0.0').isGreaterThan(semver('1.0.0'))"),
            Value::Bool(true)
        );
    }
}
