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
//! kube-cel = { version = "0.6", default-features = false, features = ["strings", "lists"] }
//! ```
//!
//! The validation pipeline (CRD `x-kubernetes-validations`, VAP, static analysis)
//! lives behind the `validation` feature (see below when it is enabled).
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
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
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
kube-cel = { version = "0.6", features = ["validation"] }
```

```rust,ignore
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
let errors = Validator::new().validate(&schema, &object, None);
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

#[cfg(feature = "strings")] mod strings;

#[cfg(feature = "lists")] mod lists;

#[cfg(feature = "sets")] mod sets;

#[cfg(feature = "regex_funcs")] mod regex_funcs;

#[cfg(feature = "urls")] mod urls;

#[cfg(feature = "ip")] mod ip;

#[cfg(feature = "semver_funcs")] mod semver_funcs;

#[cfg(feature = "format")] mod format;

#[cfg(feature = "quantity")] mod quantity;

#[cfg(feature = "jsonpatch")] mod jsonpatch;

#[cfg(feature = "named_format")] mod named_format;

#[cfg(feature = "math")] mod math;

#[cfg(feature = "encoders")] mod encoders;

// The validation pipeline is exposed as a flat set of crate-root re-exports
// (below), not as public submodules — the internal file layout is not part of
// the public API. See the "Versioning and stability" docs (Tier 2).
#[cfg(feature = "validation")] mod escaping;

#[cfg(feature = "validation")] mod values;

#[cfg(feature = "validation")] mod compilation;

#[cfg(feature = "validation")] mod validation;

#[cfg(feature = "validation")] mod defaults;

#[cfg(feature = "validation")] mod analysis;

#[cfg(feature = "validation")] mod vap;

mod dispatch;
mod ext;
mod value_ops;

pub use ext::KubeCelExt;

#[cfg(feature = "validation")]
pub use crate::{
    analysis::{
        AnalysisWarning, ScopeContext, WarningKind, analyze_rule, check_rule_scope, estimate_rule_cost,
    },
    compilation::{CompilationError, CompilationResult, CompiledSchema, Rule, compile_schema},
    defaults::apply_defaults,
    validation::{ErrorKind, RootContext, ValidationError, Validator, validate, validate_compiled},
    values::SchemaFormat,
    vap::{
        AdmissionRequest, CompiledVapExpression, GroupVersionKind, GroupVersionResource, VapEvaluator,
        VapEvaluatorBuilder, VapExpression, VapResult,
    },
};

/// Registers all compiled-in Kubernetes CEL extension functions into `ctx`.
///
/// Internal implementation behind [`KubeCelExt::register_all`]; kept as a free
/// function so the in-crate callers (dispatch, vap, validation, …) can invoke it
/// without importing the trait.
pub(crate) fn register_all(ctx: &mut cel::Context<'_>) {
    #[cfg(feature = "strings")]
    strings::register(ctx);

    #[cfg(feature = "lists")]
    lists::register(ctx);

    #[cfg(feature = "sets")]
    sets::register(ctx);

    #[cfg(feature = "regex_funcs")]
    regex_funcs::register(ctx);

    #[cfg(feature = "urls")]
    urls::register(ctx);

    #[cfg(feature = "ip")]
    ip::register(ctx);

    #[cfg(feature = "semver_funcs")]
    semver_funcs::register(ctx);

    #[cfg(feature = "format")]
    format::register(ctx);

    #[cfg(feature = "quantity")]
    quantity::register(ctx);

    #[cfg(feature = "jsonpatch")]
    jsonpatch::register(ctx);

    #[cfg(feature = "named_format")]
    named_format::register(ctx);

    #[cfg(feature = "math")]
    math::register(ctx);

    #[cfg(feature = "encoders")]
    encoders::register(ctx);

    // Dispatch: registers functions with name collisions (indexOf, reverse,
    // min/max, string, ip, isGreaterThan, etc.). Order-independent since
    // individual modules no longer register these conflicting names.
    dispatch::register(ctx);
}

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
