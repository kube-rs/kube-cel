//! Kubernetes CEL list extension functions.
//!
//! Provides list functions available in Kubernetes CEL expressions,
//! matching the behavior of `k8s.io/apiserver/pkg/cel/library/lists.go`.

use cel::{
    ExecutionError, FunctionContext, ResolveResult,
    common::{
        types::{CelInt, CelOptional},
        value::{CowVal, Val},
    },
    extractors::{Arguments, This},
    objects::Value,
};
use std::{cmp::Ordering, sync::Arc};

use crate::functions::{
    Registrar,
    val_args::{RawFunction, list_items, list_target, snapshot, val_target},
    value_ops::{go_compare, val_add},
};

/// Register all list extension functions. Each takes any list `Val`, the
/// validation engine's schema-aware ones included (see `val_args`).
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    r.add(
        "isSorted",
        snapshot(|this, args| is_sorted(This(list_target("isSorted", this, args)?.0))),
    );
    r.add(
        "sum",
        snapshot(|this, args| sum(This(list_target("sum", this, args)?.0))),
    );
    // A legacy `Arguments` is every argument, the one a global call takes as
    // its target included, so `min([1, 2])` stays refused.
    r.add(
        "min",
        snapshot(|this, args| {
            let all = Arguments(Arc::new(args.clone()));
            list_min(This(list_target("min", this, args)?.0), all)
        }),
    );
    r.add(
        "max",
        snapshot(|this, args| {
            let all = Arguments(Arc::new(args.clone()));
            list_max(This(list_target("max", this, args)?.0), all)
        }),
    );
    // indexOf/lastIndexOf are registered via dispatch module to handle
    // name collision between string and list versions.
    r.add("first", Box::new(list_first) as RawFunction);
    r.add("last", Box::new(list_last) as RawFunction);
    // slice, sort, flatten, reverse, distinct and lists.range are cel-go's
    // `ext.Lists`, declared on the Env (see `lists_ext`).
}

/// `<list>.isSorted() -> bool`
///
/// Returns true if the list elements are in sorted (ascending) order.
/// As the apiserver's `isSorted`: every element must be one of cel-go's
/// comparable types (`traits.Comparer`), and only a pair that compares greater
/// unsorts the list; a pair cel-go cannot compare (a NaN, or two types it does
/// not order together) does not.
fn is_sorted(This(this): This<Arc<Vec<Value>>>) -> ResolveResult {
    let mut prev: Option<&Value> = None;
    for next in this.iter() {
        comparable("isSorted", next)?;
        if let Some(prev) = prev
            && go_compare(prev, next) == Some(Ordering::Greater)
        {
            return Ok(Value::Bool(false));
        }
        prev = Some(next);
    }
    Ok(Value::Bool(true))
}

/// An element of `isSorted`, `min` or `max`: one of cel-go's comparable types
/// (`traits.Comparer`), else an error, as the apiserver gives even for a list
/// of one (`[null].min()`).
fn comparable(func: &str, v: &Value) -> Result<(), ExecutionError> {
    match v {
        Value::Int(_)
        | Value::UInt(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Bytes(_)
        | Value::Bool(_)
        | Value::Timestamp(_)
        | Value::Duration(_) => Ok(()),
        _ => Err(ExecutionError::function_error(
            func,
            format!("no such overload: {func} on a list of {}", v.type_of()),
        )),
    }
}

/// The apiserver's `cmp` behind `min` and `max`: the first element, replaced by
/// each later one that compares `prefer` against the current result. A pair
/// cel-go cannot compare replaces nothing (`[double('NaN'), 1.0].min()` is NaN).
fn extreme(func: &str, list: &[Value], prefer: Ordering) -> ResolveResult {
    let mut result: Option<&Value> = None;
    for next in list {
        comparable(func, next)?;
        result = match result {
            Some(current) if go_compare(current, next) != Some(prefer) => Some(current),
            _ => Some(next),
        };
    }
    result
        .cloned()
        .ok_or_else(|| ExecutionError::function_error(func, format!("{func} called on empty list")))
}

/// `<list>.sum() -> T`
///
/// Returns the sum of all elements. Empty list returns 0 for int, 0u for uint, 0.0 for double.
fn sum(This(this): This<Arc<Vec<Value>>>) -> ResolveResult {
    if this.is_empty() {
        return Ok(Value::Int(0));
    }

    let mut acc = this[0].clone();
    for item in this.iter().skip(1) {
        acc = val_add(&acc, item)?;
    }
    Ok(acc)
}

/// Refuses arguments to `<list>.name()`, which takes none. `cel` would
/// otherwise ignore them, and take `name(list)` as `list.name()`: the
/// apiserver has neither form, and before 0.9 kube-cel's `min` / `max` failed
/// on both.
fn no_arguments(name: &str, args: &[Value]) -> Result<(), ExecutionError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(ExecutionError::function_error(
            name,
            format!("{name} takes no arguments: call it as <list>.{name}()"),
        ))
    }
}

/// `<list>.min() -> T`
///
/// Returns the minimum element. Errors on empty list. The apiserver has no
/// global `min(..)`, and neither has kube-cel.
fn list_min(This(this): This<Arc<Vec<Value>>>, Arguments(args): Arguments) -> ResolveResult {
    no_arguments("min", &args)?;
    extreme("min", &this, Ordering::Greater)
}

/// `<list>.max() -> T`
///
/// Returns the maximum element. Errors on empty list. The apiserver has no
/// global `max(..)`, and neither has kube-cel.
fn list_max(This(this): This<Arc<Vec<Value>>>, Arguments(args): Arguments) -> ResolveResult {
    no_arguments("max", &args)?;
    extreme("max", &this, Ordering::Less)
}

/// `<list>.indexOf(T) -> int`: the index of the first element equal to
/// `target`, or -1. The element is on the left of `==`, as in the apiserver
/// (`lister.Get(i).Equal(item)`), so a set element compares as a set.
pub(crate) fn list_index_of<'c, 'call>(
    list: &dyn Val,
    target: &dyn Val,
) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let index = list_items("indexOf", list)?
        .iter()
        .position(|item| item.equals(target))
        .map_or(-1, |i| i as i64);
    Ok(CowVal::owned(CelInt::from(index)))
}

/// `<list>.lastIndexOf(T) -> int`: as [`list_index_of`], the last one.
pub(crate) fn list_last_index_of<'c, 'call>(
    list: &dyn Val,
    target: &dyn Val,
) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let index = list_items("lastIndexOf", list)?
        .iter()
        .rposition(|item| item.equals(target))
        .map_or(-1, |i| i as i64);
    Ok(CowVal::owned(CelInt::from(index)))
}

/// `<list>.first() -> optional<T>`: `optional.of` the first element, the
/// element itself (a set stays a set), or `optional.none()`.
fn list_first<'c, 'call>(ftx: &mut FunctionContext<'c, 'call>) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let (list, _) = val_target(ftx)?;
    let items = list_items("first", list)?;
    Ok(CowVal::owned(match items.first() {
        Some(item) => CelOptional::of(item.clone_as_boxed()),
        None => CelOptional::none(),
    }))
}

/// `<list>.last() -> optional<T>`: as [`list_first`], the last element.
fn list_last<'c, 'call>(ftx: &mut FunctionContext<'c, 'call>) -> Result<CowVal<'c, 'call>, ExecutionError> {
    let (list, _) = val_target(ftx)?;
    let items = list_items("last", list)?;
    Ok(CowVal::owned(match items.last() {
        Some(item) => CelOptional::of(item.clone_as_boxed()),
        None => CelOptional::none(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    #[test]
    fn test_is_sorted() {
        assert_eq!(eval("[1, 2, 3].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[3, 1, 2].isSorted()"), Value::Bool(false));
        assert_eq!(eval("[].isSorted()"), Value::Bool(true));
        assert_eq!(eval("['a', 'b', 'c'].isSorted()"), Value::Bool(true));
    }

    #[test]
    fn test_sum() {
        assert_eq!(eval("[1, 2, 3].sum()"), Value::Int(6));
        assert_eq!(eval("[1.5, 2.5].sum()"), Value::Float(4.0));
        assert_eq!(eval("[].sum()"), Value::Int(0));
    }

    #[test]
    fn test_sum_overflow_errors() {
        // i64::MAX + 1 must error (cel-go integer-overflow semantics), not panic
        // in debug or silently wrap in release.
        let result = crate::functions::eval("[9223372036854775807, 1].sum()");
        assert!(result.is_err(), "sum overflow should error, got {result:?}");
    }

    #[test]
    fn test_min_max() {
        assert_eq!(eval("[3, 1, 2].min()"), Value::Int(1));
        assert_eq!(eval("[3, 1, 2].max()"), Value::Int(3));
    }

    /// The apiserver has `<list>.min()` / `<list>.max()` only, and the `cel`
    /// standard library no global `min` / `max`: a variadic call is an error.
    #[test]
    fn test_min_max_are_list_members_only() {
        eval_err("min(5, 3)");
        eval_err("max(5, 3)");
        eval_err("min([3, 1, 2])");
        eval_err("[3, 1, 2].min(0)");
        eval_err("[3, 1, 2].max(9)");
    }

    #[test]
    fn test_index_of() {
        assert_eq!(eval("[1, 2, 3, 2].indexOf(2)"), Value::Int(1));
        assert_eq!(eval("[1, 2, 3].indexOf(4)"), Value::Int(-1));
    }

    #[test]
    fn test_last_index_of() {
        assert_eq!(eval("[1, 2, 3, 2].lastIndexOf(2)"), Value::Int(3));
    }

    // --- first / last ---

    #[test]
    fn test_first() {
        assert_eq!(eval("[1, 2, 3].first().hasValue()"), Value::Bool(true));
        assert_eq!(eval("[1, 2, 3].first().value()"), Value::Int(1));
    }

    #[test]
    fn test_first_empty() {
        assert_eq!(eval("[].first().hasValue()"), Value::Bool(false));
    }

    #[test]
    fn test_last() {
        assert_eq!(eval("[1, 2, 3].last().hasValue()"), Value::Bool(true));
        assert_eq!(eval("[1, 2, 3].last().value()"), Value::Int(3));
    }

    #[test]
    fn test_last_empty() {
        assert_eq!(eval("[].last().hasValue()"), Value::Bool(false));
    }

    // --- Error & edge case tests ---

    fn eval_err(expr: &str) -> cel::ExecutionError {
        crate::functions::eval(expr).unwrap_err()
    }

    #[test]
    fn test_min_empty_list() {
        eval_err("[].min()");
    }

    #[test]
    fn test_max_empty_list() {
        eval_err("[].max()");
    }

    #[test]
    fn test_min_max_single_element() {
        assert_eq!(eval("[42].min()"), Value::Int(42));
        assert_eq!(eval("[42].max()"), Value::Int(42));
    }

    #[test]
    fn test_min_max_strings() {
        assert_eq!(eval("['c', 'a', 'b'].min()"), Value::String(Arc::new("a".into())));
        assert_eq!(eval("['c', 'a', 'b'].max()"), Value::String(Arc::new("c".into())));
    }

    #[test]
    fn test_is_sorted_with_equal_elements() {
        assert_eq!(eval("[1, 1, 2].isSorted()"), Value::Bool(true));
    }

    // --- cel-go parity tests ---

    #[test]
    fn test_first_single() {
        assert_eq!(eval("[42].first().value()"), Value::Int(42));
    }

    #[test]
    fn test_last_single() {
        assert_eq!(eval("[42].last().value()"), Value::Int(42));
    }

    #[test]
    fn test_first_strings() {
        assert_eq!(
            eval("['a', 'b', 'c'].first().value()"),
            Value::String(Arc::new("a".into()))
        );
    }

    #[test]
    fn test_last_strings() {
        assert_eq!(
            eval("['a', 'b', 'c'].last().value()"),
            Value::String(Arc::new("c".into()))
        );
    }

    // --- Issue #5: cel-go equality parity (val_eq) ---

    #[test]
    fn test_index_of_cross_numeric() {
        // Int 2 should match Float 2.0 — cel-go standard equality coerces.
        assert_eq!(eval("[1, 2.0, 3u].indexOf(2)"), Value::Int(1));
    }

    #[test]
    fn test_last_index_of_cross_numeric() {
        // UInt 1 should match Float 1.0 at index 0 and Int 1 at index 2;
        // lastIndexOf returns the last match.
        assert_eq!(eval("[1.0, 2, 1].lastIndexOf(1u)"), Value::Int(2));
    }

    // --- Issue #5: cel-go ordering parity (val_lt / val_le / compare_values) ---

    #[test]
    fn test_is_sorted_cross_numeric() {
        // cel-go's `<=` coerces across Int/UInt/Float.
        assert_eq!(eval("[1, 2u, 3.0].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[1.0, 2, 3u].isSorted()"), Value::Bool(true));
    }

    #[test]
    fn test_min_cross_numeric() {
        // min([3u, 1, 2.0]) — comparing UInt 3, Int 1, Float 2.0
        assert_eq!(eval("[3u, 1, 2.0].min()"), Value::Int(1));
    }

    #[test]
    fn test_max_cross_numeric() {
        assert_eq!(eval("[1, 2.0, 3u].max()"), Value::UInt(3));
    }

    /// The apiserver's `isSorted` / `min` / `max` (checked against it): a pair
    /// cel-go's `Compare` refuses changes nothing, bytes order, and an element
    /// that is not comparable is an error even alone.
    #[test]
    fn test_comparisons_follow_the_apiserver() {
        let err = |expr: &str| crate::functions::eval(expr).is_err();
        assert_eq!(eval("[1.0, double('NaN'), 0.5].min()"), Value::Float(0.5));
        assert_eq!(eval("[1.0, double('NaN')].max()"), Value::Float(1.0));
        assert!(matches!(eval("[double('NaN'), 1.0].min()"), Value::Float(x) if x.is_nan()));
        assert_eq!(eval("[double('NaN'), 1.0].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[1.0, double('NaN'), 0.5].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[2.0, 1.0, double('NaN')].isSorted()"), Value::Bool(false));
        assert_eq!(eval("[b'c', b'a', b'b'].min() == b'a'"), Value::Bool(true));
        assert_eq!(eval("[b'c', b'a', b'b'].max() == b'c'"), Value::Bool(true));
        assert_eq!(eval("[b'a', b'b'].isSorted()"), Value::Bool(true));
        assert_eq!(eval("[b'b', b'a'].isSorted()"), Value::Bool(false));
        assert!(err("[null].min()"));
        assert!(err("[null, null].isSorted()"));
        assert!(err("[[1]].max()"));
        assert!(err("[].min()"));
    }
}
