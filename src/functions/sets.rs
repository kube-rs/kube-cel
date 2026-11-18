//! Kubernetes CEL sets extension functions.
//!
//! Provides set operations on lists, matching `cel-go/ext/sets.go`.
//! These are namespaced functions called as `sets.contains(a, b)`.

use cel::{
    ExecutionError, FunctionContext,
    common::{
        types::CelBool,
        value::{CowVal, Val},
    },
};

use crate::functions::{
    Registrar,
    val_args::{RawFunction, holds, list_items},
};

/// Register all set extension functions. They read their lists as `Val`s, so
/// a schema-aware element (a set in a list) keeps its own equality.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add("sets.contains", Box::new(sets_contains) as RawFunction);
    r.add("sets.equivalent", Box::new(sets_equivalent) as RawFunction);
    r.add("sets.intersects", Box::new(sets_intersects) as RawFunction);
}

/// The two list arguments of a `sets` function.
fn two_lists<'f, 'c, 'call>(
    ftx: &'f FunctionContext<'c, 'call>,
    name: &str,
) -> Result<(&'f (dyn Val + 'call), &'f (dyn Val + 'call)), ExecutionError> {
    match ftx.args.as_slice() {
        [a, b] => Ok((a.as_ref(), b.as_ref())),
        args => Err(ExecutionError::function_error(
            name,
            format!("expected 2 arguments, got {}", args.len()),
        )),
    }
}

/// Whether `list` holds every element of `sublist`: cel-go's `setsContains`,
/// `list.Contains(item)` for each item.
fn contains_all(name: &str, list: &dyn Val, sublist: &dyn Val) -> Result<bool, ExecutionError> {
    for item in list_items(name, sublist)? {
        if !holds(name, list, item)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `sets.contains(list, list) -> bool`
///
/// Returns true if the first list contains all elements of the second list.
fn sets_contains<'c, 'call>(
    ftx: &mut FunctionContext<'c, 'call>,
) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let (a, b) = two_lists(ftx, "sets.contains")?;
    Ok(CowVal::owned(CelBool::from(contains_all("sets.contains", a, b)?)))
}

/// `sets.equivalent(list, list) -> bool`
///
/// Returns true if both lists contain the same set of elements
/// (ignoring duplicates and order).
fn sets_equivalent<'c, 'call>(
    ftx: &mut FunctionContext<'c, 'call>,
) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let (a, b) = two_lists(ftx, "sets.equivalent")?;
    let equivalent = contains_all("sets.equivalent", a, b)? && contains_all("sets.equivalent", b, a)?;
    Ok(CowVal::owned(CelBool::from(equivalent)))
}

/// `sets.intersects(list, list) -> bool`
///
/// Returns true if the two lists share at least one common element: cel-go's
/// `setsIntersects`, the second list's `Contains` for each item of the first.
fn sets_intersects<'c, 'call>(
    ftx: &mut FunctionContext<'c, 'call>,
) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let (a, b) = two_lists(ftx, "sets.intersects")?;
    let mut intersects = false;
    for item in list_items("sets.intersects", a)? {
        if holds("sets.intersects", b, item)? {
            intersects = true;
            break;
        }
    }
    Ok(CowVal::owned(CelBool::from(intersects)))
}

#[cfg(test)]
mod tests {
    use cel::Value;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    #[test]
    fn test_contains() {
        assert_eq!(eval("sets.contains([1, 2, 3], [1, 2])"), Value::Bool(true));
        assert_eq!(eval("sets.contains([1, 2, 3], [4])"), Value::Bool(false));
        assert_eq!(eval("sets.contains([1, 2, 3], [])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent() {
        assert_eq!(eval("sets.equivalent([1, 2, 3], [3, 2, 1])"), Value::Bool(true));
        assert_eq!(eval("sets.equivalent([1, 2, 2], [1, 2])"), Value::Bool(true));
        assert_eq!(eval("sets.equivalent([1, 2], [1, 3])"), Value::Bool(false));
    }

    #[test]
    fn test_intersects() {
        assert_eq!(eval("sets.intersects([1, 2], [2, 3])"), Value::Bool(true));
        assert_eq!(eval("sets.intersects([1, 2], [3, 4])"), Value::Bool(false));
        assert_eq!(eval("sets.intersects([], [1])"), Value::Bool(false));
    }

    // --- Edge case tests ---

    #[test]
    fn test_equivalent_empty() {
        assert_eq!(eval("sets.equivalent([], [])"), Value::Bool(true));
    }

    #[test]
    fn test_intersects_both_empty() {
        assert_eq!(eval("sets.intersects([], [])"), Value::Bool(false));
    }

    #[test]
    fn test_contains_strings() {
        assert_eq!(
            eval("sets.contains(['a', 'b', 'c'], ['a', 'c'])"),
            Value::Bool(true)
        );
        assert_eq!(eval("sets.contains(['a', 'b'], ['d'])"), Value::Bool(false));
    }

    #[test]
    fn test_intersects_strings() {
        assert_eq!(eval("sets.intersects(['a', 'b'], ['b', 'c'])"), Value::Bool(true));
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_contains_negated() {
        assert_eq!(eval("!sets.contains([1], [2])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent_negated() {
        assert_eq!(eval("!sets.equivalent([2, 1], [1])"), Value::Bool(true));
    }

    #[test]
    fn test_intersects_negated() {
        assert_eq!(eval("!sets.intersects([], [])"), Value::Bool(true));
        assert_eq!(eval("!sets.intersects([1], [2])"), Value::Bool(true));
    }

    #[test]
    fn test_contains_empty_superset() {
        assert_eq!(eval("sets.contains([], [])"), Value::Bool(true));
    }

    #[test]
    fn test_contains_with_duplicates() {
        assert_eq!(eval("sets.contains([1, 1, 2, 2, 3], [1, 2])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent_with_duplicates() {
        assert_eq!(eval("sets.equivalent([1, 1, 2], [2, 2, 1])"), Value::Bool(true));
    }

    // --- Issue #5 reproduction: cel-go parity for numeric coercion & nested lists ---
    // Mirrors cases from cel-go ext/sets_test.go. All MUST return true to match
    // cel-go's standard-equality semantics used by traits.Lister.Contains().

    #[test]
    fn test_equivalent_numeric_coercion_unit() {
        // clux issue: [1] vs [1u, 1.0] — same numeric value, different types
        assert_eq!(eval("sets.equivalent([1], [1u, 1.0])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent_numeric_coercion_full() {
        // cel-go sets_test.go: [1, 2, 3] vs [3u, 2.0, 1]
        assert_eq!(
            eval("sets.equivalent([1, 2, 3], [3u, 2.0, 1])"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_contains_numeric_coercion() {
        // cel-go sets_test.go
        assert_eq!(eval("sets.contains([1, 2], [2u, 2.0])"), Value::Bool(true));
        assert_eq!(eval("sets.contains([1, 2u], [2, 2.0])"), Value::Bool(true));
        assert_eq!(
            eval("sets.contains([1, 2.0, 3u], [1.0, 2u, 3])"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_intersects_numeric_coercion() {
        // Coercion-only — no element in lhs has an exact-type match in rhs.
        assert_eq!(eval("sets.intersects([1u], [1.0])"), Value::Bool(true));
        assert_eq!(eval("sets.intersects([1.0], [1])"), Value::Bool(true));
        // cel-go ext/sets_test.go cases (also contain a same-type 2 element).
        assert_eq!(eval("sets.intersects([1, 2], [2u, 2, 2.0])"), Value::Bool(true));
        assert_eq!(eval("sets.intersects([1, 2], [1u, 2, 2.3])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent_nested_list_identity() {
        // Real defect exposed: any nested-list pair currently compares unequal.
        assert_eq!(eval("sets.equivalent([['a']], [['a']])"), Value::Bool(true));
    }

    #[test]
    fn test_equivalent_nested_list_with_numeric_coercion() {
        // cel-go sets_test.go: [[1.0], [2, 3]] vs [[1], [2, 3.0]]
        assert_eq!(
            eval("sets.equivalent([[1.0], [2, 3]], [[1], [2, 3.0]])"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_contains_nested_list() {
        // cel-go sets_test.go: [[1], [2, 3]] contains [[2, 3.0]]
        assert_eq!(
            eval("sets.contains([[1], [2, 3]], [[2, 3.0]])"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_intersects_nested_list() {
        // cel-go sets_test.go: [[1], [2, 3]] intersects [[1, 2], [2, 3.0]]
        assert_eq!(
            eval("sets.intersects([[1], [2, 3]], [[1, 2], [2, 3.0]])"),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_equivalent_nested_list_genuinely_different() {
        // clux's literal example: sets ARE different ({'a',['b']} vs {'b',['a']}).
        // cel-go also returns false here. Kept as a "right answer for the right
        // reason" regression after the val_eq fix.
        assert_eq!(
            eval("sets.equivalent(['a', ['b']], ['b', ['a']])"),
            Value::Bool(false)
        );
    }
}
