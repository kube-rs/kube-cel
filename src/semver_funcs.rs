//! Kubernetes CEL semantic versioning extension functions.
//!
//! Provides semver parsing, comparison, and accessor functions,
//! matching `k8s.io/apiserver/pkg/cel/library/semverlib.go`.

use cel::extractors::This;
use cel::objects::{Opaque, Value};
use cel::{Context, ExecutionError, ResolveResult};
use std::cmp::Ordering;
use std::sync::Arc;

/// A Kubernetes CEL Semver value wrapping `semver::Version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubeSemver(semver::Version);

impl Opaque for KubeSemver {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.Semver"
    }
}

/// Register all semver extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("semver", parse_semver);
    ctx.add_function("isSemver", is_semver);
    ctx.add_function("major", semver_major);
    ctx.add_function("minor", semver_minor);
    ctx.add_function("patch", semver_patch);
    // isGreaterThan, isLessThan, compareTo registered via dispatch
}

/// Normalize a version string before parsing:
/// - Strip leading 'v' or 'V'
/// - Pad missing minor/patch (e.g., "1" -> "1.0.0", "1.2" -> "1.2.0")
fn normalize(s: &str) -> String {
    let s = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    let parts: Vec<&str> = s.splitn(2, '-').collect();
    let version_part = parts[0];
    let pre_part = parts.get(1);

    let dots: Vec<&str> = version_part.split('.').collect();
    let normalized = match dots.len() {
        1 => format!("{}.0.0", dots[0]),
        2 => format!("{}.{}.0", dots[0], dots[1]),
        _ => version_part.to_string(),
    };

    match pre_part {
        Some(pre) => format!("{normalized}-{pre}"),
        None => normalized,
    }
}

/// `semver(<string>) -> Semver`
fn parse_semver(s: Arc<String>) -> ResolveResult {
    let normalized = normalize(&s);
    let version = semver::Version::parse(&normalized).map_err(|e| {
        ExecutionError::function_error("semver", format!("invalid semver '{s}': {e}"))
    })?;
    Ok(Value::Opaque(Arc::new(KubeSemver(version))))
}

/// `isSemver(<string>) -> bool`
fn is_semver(s: Arc<String>) -> ResolveResult {
    let normalized = normalize(&s);
    Ok(Value::Bool(semver::Version::parse(&normalized).is_ok()))
}

/// Helper to extract KubeSemver from an opaque Value.
fn extract_semver(val: &Value) -> Result<&KubeSemver, ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeSemver>()
            .ok_or_else(|| ExecutionError::function_error("semver", "expected Semver type")),
        _ => Err(ExecutionError::function_error(
            "semver",
            "expected Semver type",
        )),
    }
}

/// `<Semver>.major() -> int`
fn semver_major(This(this): This<Value>) -> ResolveResult {
    let sv = extract_semver(&this)?;
    Ok(Value::Int(sv.0.major as i64))
}

/// `<Semver>.minor() -> int`
fn semver_minor(This(this): This<Value>) -> ResolveResult {
    let sv = extract_semver(&this)?;
    Ok(Value::Int(sv.0.minor as i64))
}

/// `<Semver>.patch() -> int`
fn semver_patch(This(this): This<Value>) -> ResolveResult {
    let sv = extract_semver(&this)?;
    Ok(Value::Int(sv.0.patch as i64))
}

/// `<Semver>.isGreaterThan(<Semver>) -> bool`
pub(crate) fn semver_is_greater_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_semver(&this)?;
    let b = extract_semver(&other)?;
    Ok(Value::Bool(a.0 > b.0))
}

/// `<Semver>.isLessThan(<Semver>) -> bool`
pub(crate) fn semver_is_less_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_semver(&this)?;
    let b = extract_semver(&other)?;
    Ok(Value::Bool(a.0 < b.0))
}

/// `<Semver>.compareTo(<Semver>) -> int`
///
/// Returns -1 if less than, 0 if equal, 1 if greater than.
pub(crate) fn semver_compare_to(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_semver(&this)?;
    let b = extract_semver(&other)?;
    let result = match a.0.cmp(&b.0) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Value::Int(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    #[test]
    fn test_is_semver() {
        assert_eq!(eval("isSemver('1.2.3')"), Value::Bool(true));
        assert_eq!(eval("isSemver('v1.2.3')"), Value::Bool(true));
        assert_eq!(eval("isSemver('1.0')"), Value::Bool(true));
        assert_eq!(eval("isSemver('not-a-version')"), Value::Bool(false));
    }

    #[test]
    fn test_major_minor_patch() {
        assert_eq!(eval("semver('1.2.3').major()"), Value::Int(1));
        assert_eq!(eval("semver('1.2.3').minor()"), Value::Int(2));
        assert_eq!(eval("semver('1.2.3').patch()"), Value::Int(3));
    }

    #[test]
    fn test_leading_v() {
        assert_eq!(eval("semver('v1.2.3').major()"), Value::Int(1));
    }

    #[test]
    fn test_pad_missing_components() {
        assert_eq!(eval("semver('1').major()"), Value::Int(1));
        assert_eq!(eval("semver('1').minor()"), Value::Int(0));
        assert_eq!(eval("semver('1.2').patch()"), Value::Int(0));
    }

    #[test]
    fn test_is_greater_than() {
        assert_eq!(
            eval("semver('2.0.0').isGreaterThan(semver('1.0.0'))"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("semver('1.0.0').isGreaterThan(semver('2.0.0'))"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_is_less_than() {
        assert_eq!(
            eval("semver('1.0.0').isLessThan(semver('2.0.0'))"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_compare_to() {
        assert_eq!(
            eval("semver('1.0.0').compareTo(semver('1.0.0'))"),
            Value::Int(0)
        );
        assert_eq!(
            eval("semver('1.0.0').compareTo(semver('2.0.0'))"),
            Value::Int(-1)
        );
        assert_eq!(
            eval("semver('2.0.0').compareTo(semver('1.0.0'))"),
            Value::Int(1)
        );
    }

    #[test]
    fn test_prerelease_ordering() {
        // Pre-release < release
        assert_eq!(
            eval("semver('1.0.0-alpha').isLessThan(semver('1.0.0'))"),
            Value::Bool(true)
        );
        // alpha < beta
        assert_eq!(
            eval("semver('1.0.0-alpha').isLessThan(semver('1.0.0-beta'))"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_prerelease_numeric_ordering() {
        // Numeric pre-release identifiers compared numerically
        assert_eq!(
            eval("semver('1.0.0-beta.2').isLessThan(semver('1.0.0-beta.11'))"),
            Value::Bool(true)
        );
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        crate::dispatch::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_semver_invalid_error() {
        eval_err("semver('not-a-version')");
    }

    #[test]
    fn test_leading_capital_v() {
        assert_eq!(eval("semver('V1.2.3').major()"), Value::Int(1));
    }

    #[test]
    fn test_equal_comparison() {
        assert_eq!(
            eval("semver('1.0.0').isGreaterThan(semver('1.0.0'))"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("semver('1.0.0').isLessThan(semver('1.0.0'))"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_partial_version_with_pre_release() {
        // "1.2-alpha" should pad to "1.2.0-alpha"
        assert_eq!(
            eval("semver('1.2-alpha').isLessThan(semver('1.2.0'))"),
            Value::Bool(true)
        );
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_is_semver_empty() {
        assert_eq!(eval("isSemver('')"), Value::Bool(false));
    }

    #[test]
    fn test_semver_equal_self() {
        assert_eq!(
            eval("semver('1.0.0').compareTo(semver('1.0.0'))"),
            Value::Int(0)
        );
    }

    #[test]
    fn test_semver_minor_comparison() {
        assert_eq!(
            eval("semver('1.1.0').isGreaterThan(semver('1.0.0'))"),
            Value::Bool(true)
        );
        assert_eq!(
            eval("semver('1.0.0').isLessThan(semver('1.1.0'))"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_semver_patch_comparison() {
        assert_eq!(
            eval("semver('1.0.1').isGreaterThan(semver('1.0.0'))"),
            Value::Bool(true)
        );
    }
}
