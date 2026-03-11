//! Kubernetes CEL extension functions for the `cel` crate.
//!
//! This crate provides the Kubernetes-specific CEL (Common Expression Language) functions
//! that are available in Kubernetes CRD validation rules, built on top of the `cel` crate.
//!
//! # Usage
//!
//! ```rust
//! use cel::Context;
//! use kube_cel::register_all;
//!
//! let mut ctx = Context::default();
//! register_all(&mut ctx);
//! ```
//!
//! # CRD Validation Pipeline (feature = `validation`)
//!
//! Compile and evaluate `x-kubernetes-validations` CEL rules client-side,
//! without an API server.
//!
//! ```toml
//! kube-cel = { version = "0.4", features = ["validation"] }
//! ```
//!
//! ```rust,ignore
//! use kube_cel::validation::Validator;
//! use serde_json::json;
//!
//! let schema = json!({
//!     "type": "object",
//!     "x-kubernetes-validations": [
//!         {"rule": "self.replicas >= 0", "message": "must be non-negative"}
//!     ],
//!     "properties": { "replicas": {"type": "integer"} }
//! });
//!
//! let object = json!({"replicas": -1});
//! let errors = Validator::new().validate(&schema, &object, None);
//! assert_eq!(errors.len(), 1);
//! ```
//!
//! For repeated validation against the same schema, pre-compile with
//! [`compilation::compile_schema`] and use [`validation::Validator::validate_compiled`].

#[cfg(feature = "strings")]
pub mod strings;

#[cfg(feature = "lists")]
pub mod lists;

#[cfg(feature = "sets")]
pub mod sets;

#[cfg(feature = "regex_funcs")]
pub mod regex_funcs;

#[cfg(feature = "urls")]
pub mod urls;

#[cfg(feature = "ip")]
pub mod ip;

#[cfg(feature = "semver_funcs")]
pub mod semver_funcs;

#[cfg(feature = "format")]
pub mod format;

#[cfg(feature = "quantity")]
pub mod quantity;

#[cfg(feature = "jsonpatch")]
pub mod jsonpatch;

#[cfg(feature = "named_format")]
pub mod named_format;

#[cfg(feature = "math")]
pub mod math;

#[cfg(feature = "encoders")]
pub mod encoders;

#[cfg(feature = "validation")]
pub mod escaping;

#[cfg(feature = "validation")]
pub mod values;

#[cfg(feature = "validation")]
pub mod compilation;

#[cfg(feature = "validation")]
pub mod validation;

mod dispatch;
mod value_ops;

/// Register all available Kubernetes CEL extension functions into the given context.
pub fn register_all(ctx: &mut cel::Context<'_>) {
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

    #[allow(unused_imports)]
    use std::sync::Arc;

    use cel::{Context, Program, Value};

    #[allow(dead_code)]
    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register_all(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_integration_strings() {
        assert_eq!(
            eval("'hello'.charAt(1)"),
            Value::String(Arc::new("e".into()))
        );
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
