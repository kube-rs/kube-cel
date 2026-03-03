//! JSONPatch key escaping for Kubernetes CEL.
//!
//! Provides `jsonpatch.escapeKey()` to escape JSONPatch path keys
//! per RFC 6901 (`~` → `~0`, `/` → `~1`).

use cel::objects::Value;
use cel::{Context, ResolveResult};
use std::sync::Arc;

/// Register the jsonpatch extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("jsonpatch.escapeKey", escape_key);
}

/// `jsonpatch.escapeKey(<string>) -> string`
///
/// Escapes a string for use as a JSONPatch path key per RFC 6901.
/// `~` is replaced with `~0` first, then `/` is replaced with `~1`.
fn escape_key(s: Arc<String>) -> ResolveResult {
    let escaped = s.replace('~', "~0").replace('/', "~1");
    Ok(Value::String(Arc::new(escaped)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    fn eval_str(expr: &str) -> String {
        match eval(expr) {
            Value::String(s) => (*s).clone(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    #[test]
    fn test_escape_tilde_and_slash() {
        assert_eq!(
            eval_str("jsonpatch.escapeKey('k8s.io/my~label')"),
            "k8s.io~1my~0label"
        );
    }

    #[test]
    fn test_escape_tilde_only() {
        assert_eq!(eval_str("jsonpatch.escapeKey('a~b')"), "a~0b");
    }

    #[test]
    fn test_escape_slash_only() {
        assert_eq!(eval_str("jsonpatch.escapeKey('a/b')"), "a~1b");
    }

    #[test]
    fn test_escape_no_special_chars() {
        assert_eq!(eval_str("jsonpatch.escapeKey('hello')"), "hello");
    }

    #[test]
    fn test_escape_empty_string() {
        assert_eq!(eval_str("jsonpatch.escapeKey('')"), "");
    }

    #[test]
    fn test_escape_multiple() {
        assert_eq!(eval_str("jsonpatch.escapeKey('~/~/')"), "~0~1~0~1");
    }

    #[test]
    fn test_escape_order_matters() {
        // ~ must be escaped before / to avoid double-escaping
        // Input: ~1 → should become ~01 (escape ~ to ~0, then 1 stays)
        assert_eq!(eval_str("jsonpatch.escapeKey('~1')"), "~01");
    }
}
