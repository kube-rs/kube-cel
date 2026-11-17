//! Math extension library for Kubernetes CEL.
//!
//! Provides math functions matching `cel-go/ext/math.go` and
//! `k8s.io/apiserver/pkg/cel/library/cost.go` math extensions.

use cel::{ExecutionError, ResolveResult, extractors::Arguments, objects::Value};

use crate::functions::Registrar;

/// Register all math extension functions.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    // Rounding
    r.add("math.ceil", math_ceil);
    r.add("math.floor", math_floor);
    r.add("math.round", math_round);
    r.add("math.trunc", math_trunc);

    // Numeric
    r.add("math.abs", math_abs);
    r.add("math.sign", math_sign);

    // Inspection
    r.add("math.isInf", math_is_inf);
    r.add("math.isNaN", math_is_nan);
    r.add("math.isFinite", math_is_finite);

    // Bitwise
    r.add("math.bitAnd", math_bit_and);
    r.add("math.bitOr", math_bit_or);
    r.add("math.bitXor", math_bit_xor);
    r.add("math.bitNot", math_bit_not);
    r.add("math.bitShiftLeft", math_bit_shift_left);
    r.add("math.bitShiftRight", math_bit_shift_right);

    // Square root
    r.add("math.sqrt", math_sqrt);

    // Variadic min/max
    r.add("math.greatest", math_greatest);
    r.add("math.least", math_least);
}

// ---------------------------------------------------------------------------
// Rounding
// ---------------------------------------------------------------------------

/// `math.ceil(double) -> double`
fn math_ceil(v: f64) -> ResolveResult {
    Ok(Value::Float(v.ceil()))
}

/// `math.floor(double) -> double`
fn math_floor(v: f64) -> ResolveResult {
    Ok(Value::Float(v.floor()))
}

/// `math.round(double) -> double`
fn math_round(v: f64) -> ResolveResult {
    Ok(Value::Float(v.round()))
}

/// `math.trunc(double) -> double`
fn math_trunc(v: f64) -> ResolveResult {
    Ok(Value::Float(v.trunc()))
}

// ---------------------------------------------------------------------------
// Numeric
// ---------------------------------------------------------------------------

/// `math.abs(T) -> T` where T is int, uint, or double.
fn math_abs(Arguments(args): Arguments) -> ResolveResult {
    let arg = args
        .first()
        .ok_or_else(|| ExecutionError::function_error("math.abs", "missing argument"))?;
    match arg {
        Value::Int(n) => {
            Ok(Value::Int(n.checked_abs().ok_or_else(|| {
                ExecutionError::function_error("math.abs", "integer overflow")
            })?))
        }
        Value::UInt(n) => Ok(Value::UInt(*n)),
        Value::Float(f) => Ok(Value::Float(f.abs())),
        _ => Err(ExecutionError::function_error(
            "math.abs",
            "expected int, uint, or double",
        )),
    }
}

/// `math.sign(T) -> T` where T is int, uint, or double.
fn math_sign(Arguments(args): Arguments) -> ResolveResult {
    let arg = args
        .first()
        .ok_or_else(|| ExecutionError::function_error("math.sign", "missing argument"))?;
    match arg {
        Value::Int(n) => Ok(Value::Int(n.signum())),
        Value::UInt(n) => Ok(Value::UInt(if *n == 0 { 0 } else { 1 })),
        Value::Float(f) => {
            if f.is_nan() {
                Ok(Value::Float(f64::NAN))
            } else if *f == 0.0 {
                Ok(Value::Float(0.0))
            } else {
                Ok(Value::Float(f.signum()))
            }
        }
        _ => Err(ExecutionError::function_error(
            "math.sign",
            "expected int, uint, or double",
        )),
    }
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

/// `math.isInf(double) -> bool`
fn math_is_inf(v: f64) -> ResolveResult {
    Ok(Value::Bool(v.is_infinite()))
}

/// `math.isNaN(double) -> bool`
fn math_is_nan(v: f64) -> ResolveResult {
    Ok(Value::Bool(v.is_nan()))
}

/// `math.isFinite(double) -> bool`
fn math_is_finite(v: f64) -> ResolveResult {
    Ok(Value::Bool(v.is_finite()))
}

// ---------------------------------------------------------------------------
// Bitwise
// ---------------------------------------------------------------------------

/// `math.bitAnd(int, int) -> int`
fn math_bit_and(a: i64, b: i64) -> ResolveResult {
    Ok(Value::Int(a & b))
}

/// `math.bitOr(int, int) -> int`
fn math_bit_or(a: i64, b: i64) -> ResolveResult {
    Ok(Value::Int(a | b))
}

/// `math.bitXor(int, int) -> int`
fn math_bit_xor(a: i64, b: i64) -> ResolveResult {
    Ok(Value::Int(a ^ b))
}

/// `math.bitNot(int) -> int`
fn math_bit_not(v: i64) -> ResolveResult {
    Ok(Value::Int(!v))
}

/// `math.bitShiftLeft(int, int) -> int`
///
/// cel-go: negative shift → error; shift ≥ 64 → 0. Go shifts have no upper
/// bound and yield 0 past the type width, whereas Rust's `<<` panics (debug)
/// or masks the count (release), so the wide case is handled explicitly.
fn math_bit_shift_left(v: i64, shift: i64) -> ResolveResult {
    if shift < 0 {
        return Err(ExecutionError::function_error(
            "math.bitShiftLeft",
            "negative shift amount",
        ));
    }
    Ok(Value::Int(if shift >= 64 { 0 } else { v << shift }))
}

/// `math.bitShiftRight(int, int) -> int`
///
/// cel-go: negative shift → error; shift ≥ 64 → 0. The shift is **logical**
/// (`types.Int(types.Uint(v) >> bs)`), so the sign bit is not extended —
/// Rust's `>>` on `i64` is arithmetic, hence the cast through `u64`.
fn math_bit_shift_right(v: i64, shift: i64) -> ResolveResult {
    if shift < 0 {
        return Err(ExecutionError::function_error(
            "math.bitShiftRight",
            "negative shift amount",
        ));
    }
    Ok(Value::Int(if shift >= 64 {
        0
    } else {
        ((v as u64) >> shift) as i64
    }))
}

// ---------------------------------------------------------------------------
// Square root
// ---------------------------------------------------------------------------

/// `math.sqrt(double) -> double`
///
/// Returns NaN for negative input (matching cel-go behavior).
fn math_sqrt(v: f64) -> ResolveResult {
    Ok(Value::Float(v.sqrt()))
}

// ---------------------------------------------------------------------------
// Variadic greatest / least
// ---------------------------------------------------------------------------

/// Compare two numeric values exactly.
///
/// cel-go's `math.greatest`/`math.least` delegate to each value's `Comparer`
/// (exact typed comparison), NOT float promotion — so two `int`s above 2^53
/// that round to the same `f64` still compare correctly. `lists.min`/`max`
/// already route through `value_ops::compare_values`; match them here.
fn numeric_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering, ExecutionError> {
    ensure_numeric(a)?;
    ensure_numeric(b)?;
    super::value_ops::compare_values(a, b)
        .map_err(|_| ExecutionError::function_error("math.greatest/least", "cannot compare values"))
}

fn ensure_numeric(v: &Value) -> Result<(), ExecutionError> {
    match v {
        Value::Int(_) | Value::UInt(_) | Value::Float(_) => Ok(()),
        _ => Err(ExecutionError::function_error(
            "math.greatest/least",
            "expected numeric argument",
        )),
    }
}

/// `math.greatest(T, T...) -> T`
///
/// Returns the greatest of all arguments. Supports int, uint, and double.
fn math_greatest(Arguments(args): Arguments) -> ResolveResult {
    math_extremum(&args, "math.greatest", std::cmp::Ordering::Greater)
}

/// `math.least(T, T...) -> T`
///
/// Returns the least of all arguments. Supports int, uint, and double.
fn math_least(Arguments(args): Arguments) -> ResolveResult {
    math_extremum(&args, "math.least", std::cmp::Ordering::Less)
}

fn math_extremum(args: &[Value], name: &str, target_ord: std::cmp::Ordering) -> ResolveResult {
    if args.is_empty() {
        return Err(ExecutionError::function_error(
            name,
            "at least one argument required",
        ));
    }
    let effective: &[Value] = if args.len() == 1 {
        if let Value::List(list) = &args[0] {
            if list.is_empty() {
                return Err(ExecutionError::function_error(
                    name,
                    "at least one argument required",
                ));
            }
            list.as_ref()
        } else {
            args
        }
    } else {
        args
    };
    let mut result = effective[0].clone();
    for item in &effective[1..] {
        if numeric_cmp(item, &result)? == target_ord {
            result = item.clone();
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cel::{Context, Program};

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        crate::functions::register_parts(&mut ctx, &[register]);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    fn eval_err(expr: &str) -> ExecutionError {
        let mut ctx = Context::default();
        crate::functions::register_parts(&mut ctx, &[register]);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    // -- Rounding --

    #[test]
    fn test_ceil() {
        assert_eq!(eval("math.ceil(1.2)"), Value::Float(2.0));
        assert_eq!(eval("math.ceil(-1.8)"), Value::Float(-1.0));
        assert_eq!(eval("math.ceil(2.0)"), Value::Float(2.0));
    }

    #[test]
    fn test_floor() {
        assert_eq!(eval("math.floor(1.8)"), Value::Float(1.0));
        assert_eq!(eval("math.floor(-1.2)"), Value::Float(-2.0));
        assert_eq!(eval("math.floor(2.0)"), Value::Float(2.0));
    }

    #[test]
    fn test_round() {
        assert_eq!(eval("math.round(1.5)"), Value::Float(2.0));
        assert_eq!(eval("math.round(1.4)"), Value::Float(1.0));
        assert_eq!(eval("math.round(-1.5)"), Value::Float(-2.0));
    }

    #[test]
    fn test_trunc() {
        assert_eq!(eval("math.trunc(1.9)"), Value::Float(1.0));
        assert_eq!(eval("math.trunc(-1.9)"), Value::Float(-1.0));
    }

    // -- Numeric --

    #[test]
    fn test_abs_int() {
        assert_eq!(eval("math.abs(-5)"), Value::Int(5));
        assert_eq!(eval("math.abs(5)"), Value::Int(5));
        assert_eq!(eval("math.abs(0)"), Value::Int(0));
    }

    #[test]
    fn test_abs_float() {
        assert_eq!(eval("math.abs(-2.5)"), Value::Float(2.5));
        assert_eq!(eval("math.abs(2.5)"), Value::Float(2.5));
    }

    #[test]
    fn test_sign_int() {
        assert_eq!(eval("math.sign(-3)"), Value::Int(-1));
        assert_eq!(eval("math.sign(0)"), Value::Int(0));
        assert_eq!(eval("math.sign(5)"), Value::Int(1));
    }

    #[test]
    fn test_sign_float() {
        assert_eq!(eval("math.sign(-3.0)"), Value::Float(-1.0));
        assert_eq!(eval("math.sign(0.0)"), Value::Float(0.0));
        assert_eq!(eval("math.sign(5.0)"), Value::Float(1.0));
    }

    // -- Inspection --

    #[test]
    fn test_is_inf() {
        assert_eq!(eval("math.isInf(1.0 / 0.0)"), Value::Bool(true));
        assert_eq!(eval("math.isInf(1.0)"), Value::Bool(false));
    }

    #[test]
    fn test_is_nan() {
        assert_eq!(eval("math.isNaN(0.0 / 0.0)"), Value::Bool(true));
        assert_eq!(eval("math.isNaN(1.0)"), Value::Bool(false));
    }

    #[test]
    fn test_is_finite() {
        assert_eq!(eval("math.isFinite(1.0)"), Value::Bool(true));
        assert_eq!(eval("math.isFinite(1.0 / 0.0)"), Value::Bool(false));
        assert_eq!(eval("math.isFinite(0.0 / 0.0)"), Value::Bool(false));
    }

    // -- Bitwise --

    #[test]
    fn test_bit_and() {
        assert_eq!(eval("math.bitAnd(3, 5)"), Value::Int(1));
    }

    #[test]
    fn test_bit_or() {
        assert_eq!(eval("math.bitOr(3, 5)"), Value::Int(7));
    }

    #[test]
    fn test_bit_xor() {
        assert_eq!(eval("math.bitXor(3, 5)"), Value::Int(6));
    }

    #[test]
    fn test_bit_not() {
        assert_eq!(eval("math.bitNot(0)"), Value::Int(-1));
    }

    #[test]
    fn test_bit_shift_left() {
        assert_eq!(eval("math.bitShiftLeft(1, 3)"), Value::Int(8));
    }

    #[test]
    fn test_bit_shift_right() {
        assert_eq!(eval("math.bitShiftRight(8, 3)"), Value::Int(1));
    }

    #[test]
    fn test_bit_shift_invalid() {
        // Only negative shifts are errors (cel-go: "negative offset").
        eval_err("math.bitShiftLeft(1, -1)");
        eval_err("math.bitShiftRight(1, -1)");
    }

    #[test]
    fn test_bit_shift_wide_is_zero() {
        // cel-go: shift >= 64 yields 0 (Go's shift has no upper bound), not an error.
        assert_eq!(eval("math.bitShiftLeft(1, 64)"), Value::Int(0));
        assert_eq!(eval("math.bitShiftLeft(1, 100)"), Value::Int(0));
        assert_eq!(eval("math.bitShiftRight(1, 64)"), Value::Int(0));
    }

    #[test]
    fn test_bit_shift_right_is_logical() {
        // Logical (unsigned) shift: the sign bit is not extended.
        // -1 (all ones) >> 1 == i64::MAX; arithmetic shift would give -1.
        assert_eq!(eval("math.bitShiftRight(-1, 1)"), Value::Int(i64::MAX));
        assert_eq!(eval("math.bitShiftRight(-1, 63)"), Value::Int(1));
    }

    // -- Square root --

    #[test]
    fn test_sqrt() {
        assert_eq!(eval("math.sqrt(4.0)"), Value::Float(2.0));
        assert_eq!(eval("math.sqrt(0.0)"), Value::Float(0.0));
        assert_eq!(eval("math.sqrt(2.0)"), Value::Float(std::f64::consts::SQRT_2));
        assert_eq!(eval("math.sqrt(49.0)"), Value::Float(7.0));
    }

    #[test]
    fn test_sqrt_negative_returns_nan() {
        // cel-go returns NaN for negative input
        let result = eval("math.sqrt(-1.0)");
        match result {
            Value::Float(f) => assert!(f.is_nan()),
            other => panic!("expected Float(NaN), got {other:?}"),
        }
    }

    #[test]
    fn test_sqrt_nan_check() {
        // math.isNaN(math.sqrt(-15.34)) == true
        assert_eq!(eval("math.isNaN(math.sqrt(-15.34))"), Value::Bool(true));
    }

    // -- abs overflow --

    #[test]
    fn test_abs_overflow() {
        // i64::MIN has no positive counterpart
        eval_err("math.abs(-9223372036854775808)");
    }

    // -- Variadic --

    #[test]
    fn test_greatest() {
        assert_eq!(eval("math.greatest(1, 3, 2)"), Value::Int(3));
        assert_eq!(eval("math.greatest(1.0, 3.0, 2.0)"), Value::Float(3.0));
    }

    #[test]
    fn test_least() {
        assert_eq!(eval("math.least(1, 3, 2)"), Value::Int(1));
        assert_eq!(eval("math.least(1.0, 3.0, 2.0)"), Value::Float(1.0));
    }

    #[test]
    fn test_greatest_single() {
        assert_eq!(eval("math.greatest(42)"), Value::Int(42));
    }

    #[test]
    fn test_least_single() {
        assert_eq!(eval("math.least(42)"), Value::Int(42));
    }

    #[test]
    fn test_greatest_single_float() {
        assert_eq!(eval("math.greatest(-0.5)"), Value::Float(-0.5));
    }

    #[test]
    fn test_greatest_least_exact_large_ints() {
        // 2^53 and 2^53+1 collapse to the same f64, so float promotion would
        // call them equal and return the wrong argument. Exact comparison must
        // keep them distinct.
        assert_eq!(
            eval("math.greatest(9007199254740992, 9007199254740993)"),
            Value::Int(9007199254740993)
        );
        assert_eq!(
            eval("math.least(9007199254740993, 9007199254740992)"),
            Value::Int(9007199254740992)
        );
    }

    #[test]
    fn test_least_single_float() {
        assert_eq!(eval("math.least(-0.5)"), Value::Float(-0.5));
    }

    // -- Inspection negated forms --

    #[test]
    fn test_is_nan_negated() {
        assert_eq!(eval("!math.isNaN(1.0)"), Value::Bool(true));
    }

    #[test]
    fn test_is_inf_negated() {
        assert_eq!(eval("!math.isInf(1.0)"), Value::Bool(true));
        assert_eq!(eval("!math.isFinite(1.0 / 0.0)"), Value::Bool(true));
    }
}
