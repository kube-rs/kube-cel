//! Kubernetes CEL semantic versioning extension functions.
//!
//! Provides semver parsing, comparison, and accessor functions,
//! matching `k8s.io/apiserver/pkg/cel/library/semverlib.go`.

use cel::{
    ExecutionError, ResolveResult,
    extractors::{Arguments, This},
    objects::{Opaque, Value},
};
use std::{cmp::Ordering, sync::Arc};

use crate::functions::Registrar;

/// A Kubernetes CEL Semver value wrapping `semver::Version`.
#[derive(Debug, Clone)]
pub struct KubeSemver(semver::Version);

impl KubeSemver {
    /// blang/semver v4's `Version.Compare`, which the apiserver calls: build
    /// metadata takes no part (`1.0.0+a` and `1.0.0+b` compare equal), unlike
    /// `semver::Version`'s `Ord`.
    fn compare(&self, other: &Self) -> Ordering {
        let (a, b) = (&self.0, &other.0);
        (a.major, a.minor, a.patch, &a.pre).cmp(&(b.major, b.minor, b.patch, &b.pre))
    }
}

/// The apiserver's `Equal` is blang's `EQ`, `Compare(..) == 0`.
impl PartialEq for KubeSemver {
    fn eq(&self, other: &Self) -> bool {
        self.compare(other) == Ordering::Equal
    }
}

impl Eq for KubeSemver {}

impl Opaque for KubeSemver {
    fn runtime_type_name(&self) -> &str {
        "kubernetes.Semver"
    }
}

/// Register all semver extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("semver", parse_semver);
    r.add("isSemver", is_semver);
    r.add("major", semver_major);
    r.add("minor", semver_minor);
    r.add("patch", semver_patch);
    // isGreaterThan, isLessThan, compareTo registered via dispatch
}

/// Normalize a version string for lenient parsing, mirroring the apiserver's
/// `normalizeAndParse` (library/semverlib.go) before it hands off to a strict
/// parser:
/// - strip a single leading lowercase `v` (NOT `V` — apiserver uses
///   `TrimPrefix(s, "v")`)
/// - split into at most 3 dot-parts (major, minor, patch+prerelease+build) so a
///   4th component like `1.2.3.4` is preserved and then rejected, not truncated
/// - strip leading zeros from any part longer than one char, re-prepending `0`
///   when the trimmed part no longer starts with a digit (`00` -> `0`)
/// - a shortened version (<3 parts) carrying `+`/`-` metadata is invalid; else
///   pad with `.0` up to 3 parts
///
/// The result is handed to the strict semver parser, which rejects anything the
/// normalization above left malformed.
fn normalize(s: &str) -> String {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts: Vec<String> = s.splitn(3, '.').map(str::to_string).collect();
    for p in &mut parts {
        if p.len() > 1 {
            let trimmed = p.trim_start_matches('0');
            *p = if trimmed.as_bytes().first().is_none_or(|b| !b.is_ascii_digit()) {
                format!("0{trimmed}")
            } else {
                trimmed.to_string()
            };
        }
    }
    if parts.len() < 3 {
        // A shortened version cannot carry PreRelease/Build metadata; leave it
        // for the strict parser to reject.
        if parts.last().is_some_and(|p| p.contains(['+', '-'])) {
            return s.to_string();
        }
        while parts.len() < 3 {
            parts.push("0".to_string());
        }
    }
    parts.join(".")
}

/// Check if the lenient flag is set.
/// Arguments contains ALL args (including the string consumed by This),
/// so the bool flag is the last element.
fn is_lenient(args: &[Value]) -> bool {
    matches!(args.last(), Some(Value::Bool(true)))
}

/// Parse a semver string, using strict or lenient mode.
fn do_parse(s: &str, lenient: bool) -> Result<semver::Version, String> {
    let version = if lenient {
        semver::Version::parse(&normalize(s))
    } else {
        semver::Version::parse(s)
    }
    .map_err(|e| e.to_string())?;
    // blang/semver reads a numeric pre-release identifier as a uint64;
    // `semver` takes any length.
    if !version.pre.is_empty()
        && let Some(id) = version
            .pre
            .split('.')
            .find(|id| id.bytes().all(|b| b.is_ascii_digit()) && id.parse::<u64>().is_err())
    {
        return Err(format!(
            "numeric pre-release identifier {id} is larger than a uint64"
        ));
    }
    Ok(version)
}

/// `semver(<string>) -> Semver`
/// `semver(<string>, <bool>) -> Semver`
///
/// Strict (1-arg): requires exact `Major.Minor.Patch` format.
/// Lenient (2-arg, true): accepts v-prefix, partial versions, leading zeros.
fn parse_semver(This(s): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    let version = do_parse(&s, is_lenient(&args))
        .map_err(|e| ExecutionError::function_error("semver", format!("invalid semver '{s}': {e}")))?;
    Ok(Value::Opaque(Arc::new(KubeSemver(version))))
}

/// `isSemver(<string>) -> bool`
/// `isSemver(<string>, <bool>) -> bool`
///
/// Strict (1-arg): requires exact `Major.Minor.Patch` format.
/// Lenient (2-arg, true): accepts v-prefix, partial versions, leading zeros.
fn is_semver(This(s): This<Arc<String>>, Arguments(args): Arguments) -> ResolveResult {
    Ok(Value::Bool(do_parse(&s, is_lenient(&args)).is_ok()))
}

/// Helper to extract KubeSemver from an opaque Value.
fn extract_semver(val: &Value) -> Result<&KubeSemver, ExecutionError> {
    match val {
        Value::Opaque(o) => o
            .downcast_ref::<KubeSemver>()
            .ok_or_else(|| ExecutionError::function_error("semver", "expected Semver type")),
        _ => Err(ExecutionError::function_error("semver", "expected Semver type")),
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
    Ok(Value::Bool(a.compare(b) == Ordering::Greater))
}

/// `<Semver>.isLessThan(<Semver>) -> bool`
pub(crate) fn semver_is_less_than(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_semver(&this)?;
    let b = extract_semver(&other)?;
    Ok(Value::Bool(a.compare(b) == Ordering::Less))
}

/// `<Semver>.compareTo(<Semver>) -> int`
///
/// Returns -1 if less than, 0 if equal, 1 if greater than.
pub(crate) fn semver_compare_to(This(this): This<Value>, other: Value) -> ResolveResult {
    let a = extract_semver(&this)?;
    let b = extract_semver(&other)?;
    let result = match a.compare(b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    };
    Ok(Value::Int(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    #[test]
    fn test_is_semver() {
        assert_eq!(eval("isSemver('1.2.3')"), Value::Bool(true));
        assert_eq!(eval("isSemver('1.2.3-beta.1+build.1')"), Value::Bool(true));
        assert_eq!(eval("isSemver('not-a-version')"), Value::Bool(false));
    }

    #[test]
    fn test_is_semver_strict_rejects() {
        // K8s strict mode rejects v-prefix, partial versions, leading zeros, spaces
        assert_eq!(eval("isSemver('v1.0.0')"), Value::Bool(false));
        assert_eq!(eval("isSemver('1')"), Value::Bool(false));
        assert_eq!(eval("isSemver('1.1')"), Value::Bool(false));
        assert_eq!(eval("isSemver('01.01.01')"), Value::Bool(false));
        assert_eq!(eval("isSemver(' 1.0.0')"), Value::Bool(false));
        assert_eq!(eval("isSemver('1.0.0 ')"), Value::Bool(false));
    }

    #[test]
    fn test_follows_blang_semver() {
        // A numeric pre-release identifier must fit a uint64 (F6).
        assert_eq!(eval("isSemver('1.0.0-18446744073709551615')"), Value::Bool(true));
        assert_eq!(eval("isSemver('1.0.0-18446744073709551616')"), Value::Bool(false));
        assert_eq!(
            eval("isSemver('1.0.0-99999999999999999999999', true)"),
            Value::Bool(false)
        );
        assert!(crate::functions::eval("semver('1.0.0-18446744073709551616')").is_err());
        // Build metadata takes no part in comparison or equality.
        assert_eq!(eval("semver('1.0.0+a') == semver('1.0.0+b')"), Value::Bool(true));
        assert_eq!(
            eval("semver('1.0.0+a').compareTo(semver('1.0.0'))"),
            Value::Int(0)
        );
        assert_eq!(
            eval("semver('1.0.0+b').isGreaterThan(semver('1.0.0+a'))"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("semver('1.0.0-rc.1').isLessThan(semver('1.0.0-rc.1+x'))"),
            Value::Bool(false)
        );
        assert_eq!(
            eval("semver('1.0.0-rc.1') == semver('1.0.0-rc.2')"),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_major_minor_patch() {
        assert_eq!(eval("semver('1.2.3').major()"), Value::Int(1));
        assert_eq!(eval("semver('1.2.3').minor()"), Value::Int(2));
        assert_eq!(eval("semver('1.2.3').patch()"), Value::Int(3));
    }

    #[test]
    fn test_semver_strict_rejects_v_prefix() {
        eval_err("semver('v1.2.3')");
    }

    #[test]
    fn test_semver_strict_rejects_partial() {
        eval_err("semver('1')");
        eval_err("semver('1.2')");
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
        assert_eq!(eval("semver('1.0.0').compareTo(semver('1.0.0'))"), Value::Int(0));
        assert_eq!(eval("semver('1.0.0').compareTo(semver('2.0.0'))"), Value::Int(-1));
        assert_eq!(eval("semver('2.0.0').compareTo(semver('1.0.0'))"), Value::Int(1));
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
        crate::functions::eval(expr).unwrap_err()
    }

    #[test]
    fn test_semver_invalid_error() {
        eval_err("semver('not-a-version')");
    }

    #[test]
    fn test_semver_strict_rejects_capital_v() {
        eval_err("semver('V1.2.3')");
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
    fn test_semver_strict_rejects_partial_with_pre_release() {
        eval_err("semver('1.2-alpha')");
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_is_semver_empty() {
        assert_eq!(eval("isSemver('')"), Value::Bool(false));
    }

    #[test]
    fn test_semver_equal_self() {
        assert_eq!(eval("semver('1.0.0').compareTo(semver('1.0.0'))"), Value::Int(0));
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

    // --- Lenient mode (2-arg) tests ---

    #[test]
    fn test_is_semver_lenient_v_prefix() {
        assert_eq!(eval("isSemver('v1.0.0', true)"), Value::Bool(true));
    }

    #[test]
    fn test_is_semver_lenient_partial() {
        assert_eq!(eval("isSemver('1', true)"), Value::Bool(true));
        assert_eq!(eval("isSemver('1.1', true)"), Value::Bool(true));
    }

    #[test]
    fn test_is_semver_lenient_still_rejects_invalid() {
        assert_eq!(eval("isSemver('', true)"), Value::Bool(false));
        assert_eq!(eval("isSemver('not-a-version', true)"), Value::Bool(false));
    }

    #[test]
    fn test_is_semver_lenient_spaces_rejected() {
        assert_eq!(eval("isSemver(' 1.0.0', true)"), Value::Bool(false));
        assert_eq!(eval("isSemver('1.0.0 ', true)"), Value::Bool(false));
    }

    #[test]
    fn test_semver_lenient_v_prefix() {
        assert_eq!(eval("semver('v1.2.3', true).major()"), Value::Int(1));
    }

    #[test]
    fn test_semver_lenient_partial() {
        assert_eq!(eval("semver('1', true).major()"), Value::Int(1));
        assert_eq!(eval("semver('1', true).minor()"), Value::Int(0));
        assert_eq!(eval("semver('1.2', true).patch()"), Value::Int(0));
    }

    #[test]
    fn test_semver_lenient_equality() {
        assert_eq!(
            eval("semver('v01.01', true) == semver('1.1.0')"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_semver_lenient_false_is_strict() {
        // Passing false should behave like strict mode
        assert_eq!(eval("isSemver('v1.0.0', false)"), Value::Bool(false));
        assert_eq!(eval("isSemver('1', false)"), Value::Bool(false));
    }

    #[test]
    fn test_is_semver_lenient_uppercase_v_rejected() {
        // apiserver normalizeAndParse strips only lowercase `v` (TrimPrefix "v"),
        // so `V1.0.0` stays malformed and is rejected — lowercase still passes.
        assert_eq!(eval("isSemver('V1.0.0', true)"), Value::Bool(false));
        assert_eq!(eval("isSemver('v1.0.0', true)"), Value::Bool(true));
    }

    #[test]
    fn test_is_semver_lenient_four_components_rejected() {
        // normalizeAndParse splits into at most 3 parts, so a 4th component is
        // preserved and rejected by the strict parser, not truncated to 1.2.3.
        assert_eq!(eval("isSemver('1.2.3.4', true)"), Value::Bool(false));
    }
}
