//! Shared value comparison and arithmetic helpers.
//!
//! Equality delegates to `cel::Value`'s own `PartialEq`, which provides
//! cross-type numeric coercion (Int/UInt/Float) and recurses structurally
//! through List/Map, matching cel-go's equality used by
//! `traits.Lister.Contains()` and `==`. Ordering is cel-go's `Compare`
//! ([`go_compare`]), which errors where `cel`'s `PartialOrd` has no answer and
//! orders bytes, which it does not.

#![allow(dead_code)]

use cel::{ExecutionError, objects::Value};
use std::cmp::Ordering;

/// cel-go's `Compare` (`traits.Comparer`) of `a` with `b`, `None` where it
/// returns an error: a NaN on either side, or types it does not order together.
/// Int, uint and double compare with each other by value, bytes byte-wise.
pub(crate) fn go_compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Float(x), _) | (_, Value::Float(x)) if x.is_nan() => None,
        (Value::Bytes(x), Value::Bytes(y)) => Some(x.cmp(y)),
        (
            Value::Int(_) | Value::UInt(_) | Value::Float(_),
            Value::Int(_) | Value::UInt(_) | Value::Float(_),
        )
        | (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Timestamp(_), Value::Timestamp(_))
        | (Value::Duration(_), Value::Duration(_)) => a.partial_cmp(b),
        _ => None,
    }
}

pub(crate) fn val_eq(a: &Value, b: &Value) -> bool {
    a == b
}

pub(crate) fn val_add(a: &Value, b: &Value) -> Result<Value, ExecutionError> {
    match (a, b) {
        // cel-go errors on integer overflow rather than wrapping; a raw `a + b`
        // here panics in debug builds and silently wraps in release.
        (Value::Int(a), Value::Int(b)) => a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| ExecutionError::function_error("sum", "integer overflow")),
        (Value::UInt(a), Value::UInt(b)) => a
            .checked_add(*b)
            .map(Value::UInt)
            .ok_or_else(|| ExecutionError::function_error("sum", "unsigned integer overflow")),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
        _ => Err(ExecutionError::function_error(
            "sum",
            "cannot sum values of this type",
        )),
    }
}
