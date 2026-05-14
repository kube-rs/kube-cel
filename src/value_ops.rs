//! Shared value comparison and arithmetic helpers.
//!
//! Equality and ordering delegate to `cel::Value`'s own `PartialEq` and
//! `PartialOrd`. Those upstream impls provide cross-type numeric coercion
//! (Int/UInt/Float) and recurse structurally through List/Map, matching
//! cel-go's standard equality/ordering semantics used by
//! `traits.Lister.Contains()` and the `==` / `<` operators.

#![allow(dead_code)]

use cel::{ExecutionError, objects::Value};
use std::cmp::Ordering;

pub(crate) fn compare_values(a: &Value, b: &Value) -> Result<Ordering, ExecutionError> {
    a.partial_cmp(b).ok_or_else(|| {
        ExecutionError::function_error("compare", "cannot compare values of different types")
    })
}

pub(crate) fn val_eq(a: &Value, b: &Value) -> bool {
    a == b
}

pub(crate) fn val_lt(a: &Value, b: &Value) -> Result<bool, ExecutionError> {
    Ok(compare_values(a, b)? == Ordering::Less)
}

pub(crate) fn val_le(a: &Value, b: &Value) -> Result<bool, ExecutionError> {
    Ok(compare_values(a, b)? != Ordering::Greater)
}

pub(crate) fn val_add(a: &Value, b: &Value) -> Result<Value, ExecutionError> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
        (Value::UInt(a), Value::UInt(b)) => Ok(Value::UInt(a + b)),
        (Value::Float(a), Value::Float(b)) => Ok(Value::Float(a + b)),
        _ => Err(ExecutionError::function_error(
            "sum",
            "cannot sum values of this type",
        )),
    }
}
