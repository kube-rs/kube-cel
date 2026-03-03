//! Math extension library for Kubernetes CEL.
//!
//! Provides math functions matching `cel-go/ext/math.go` and
//! `k8s.io/apiserver/pkg/cel/library/cost.go` math extensions.

use cel::extractors::Arguments;
use cel::objects::Value;
use cel::{Context, ExecutionError, ResolveResult};

/// Register all math extension functions.
pub fn register(ctx: &mut Context<'_>) {
    // Rounding
    ctx.add_function("math.ceil", math_ceil);
    ctx.add_function("math.floor", math_floor);
    ctx.add_function("math.round", math_round);
    ctx.add_function("math.trunc", math_trunc);

    // Numeric
    ctx.add_function("math.abs", math_abs);
    ctx.add_function("math.sign", math_sign);

    // Inspection
    ctx.add_function("math.isInf", math_is_inf);
    ctx.add_function("math.isNaN", math_is_nan);
    ctx.add_function("math.isFinite", math_is_finite);

    // Bitwise
    ctx.add_function("math.bitAnd", math_bit_and);
    ctx.add_function("math.bitOr", math_bit_or);
    ctx.add_function("math.bitXor", math_bit_xor);
    ctx.add_function("math.bitNot", math_bit_not);
    ctx.add_function("math.bitShiftLeft", math_bit_shift_left);
    ctx.add_function("math.bitShiftRight", math_bit_shift_right);

    // Variadic min/max
    ctx.add_function("math.greatest", math_greatest);
    ctx.add_function("math.least", math_least);
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
        Value::Int(n) => Ok(Value::Int(n.checked_abs().ok_or_else(|| {
            ExecutionError::function_error("math.abs", "integer overflow")
        })?)),
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
fn math_bit_shift_left(v: i64, shift: i64) -> ResolveResult {
    if shift < 0 || shift > 63 {
        return Err(ExecutionError::function_error(
            "math.bitShiftLeft",
            "shift amount must be between 0 and 63",
        ));
    }
    Ok(Value::Int(v << shift))
}

/// `math.bitShiftRight(int, int) -> int`
fn math_bit_shift_right(v: i64, shift: i64) -> ResolveResult {
    if shift < 0 || shift > 63 {
        return Err(ExecutionError::function_error(
            "math.bitShiftRight",
            "shift amount must be between 0 and 63",
        ));
    }
    Ok(Value::Int(v >> shift))
}

// ---------------------------------------------------------------------------
// Variadic greatest / least
// ---------------------------------------------------------------------------

/// Compare two numeric values, returning ordering.
fn numeric_cmp(a: &Value, b: &Value) -> Result<std::cmp::Ordering, ExecutionError> {
    // Promote to f64 for cross-type comparison
    let fa = to_f64(a)?;
    let fb = to_f64(b)?;
    fa.partial_cmp(&fb).ok_or_else(|| {
        ExecutionError::function_error("math.greatest/least", "cannot compare NaN values")
    })
}

fn to_f64(v: &Value) -> Result<f64, ExecutionError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::UInt(n) => Ok(*n as f64),
        Value::Float(f) => Ok(*f),
        _ => Err(ExecutionError::function_error(
            "math.greatest/least",
            "expected numeric argument",
        )),
    }
}

/// `math.greatest(T, T...) -> T`
///
/// Returns the greatest of all arguments. Supports int, uint, and double.
/// If all arguments are the same type, the result type is preserved.
fn math_greatest(Arguments(args): Arguments) -> ResolveResult {
    if args.is_empty() {
        return Err(ExecutionError::function_error(
            "math.greatest",
            "at least one argument required",
        ));
    }
    // If single list argument, use it as the args
    let effective_args: &[Value] = if args.len() == 1 {
        if let Value::List(list) = &args[0] {
            if list.is_empty() {
                return Err(ExecutionError::function_error(
                    "math.greatest",
                    "at least one argument required",
                ));
            }
            list.as_ref()
        } else {
            &args
        }
    } else {
        &args
    };
    let mut result = effective_args[0].clone();
    for item in &effective_args[1..] {
        if numeric_cmp(item, &result)? == std::cmp::Ordering::Greater {
            result = item.clone();
        }
    }
    Ok(result)
}

/// `math.least(T, T...) -> T`
///
/// Returns the least of all arguments. Supports int, uint, and double.
fn math_least(Arguments(args): Arguments) -> ResolveResult {
    if args.is_empty() {
        return Err(ExecutionError::function_error(
            "math.least",
            "at least one argument required",
        ));
    }
    let effective_args: &[Value] = if args.len() == 1 {
        if let Value::List(list) = &args[0] {
            if list.is_empty() {
                return Err(ExecutionError::function_error(
                    "math.least",
                    "at least one argument required",
                ));
            }
            list.as_ref()
        } else {
            &args
        }
    } else {
        &args
    };
    let mut result = effective_args[0].clone();
    for item in &effective_args[1..] {
        if numeric_cmp(item, &result)? == std::cmp::Ordering::Less {
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
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    fn eval_err(expr: &str) -> ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
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
        assert_eq!(eval("math.abs(-3.14)"), Value::Float(3.14));
        assert_eq!(eval("math.abs(3.14)"), Value::Float(3.14));
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
        eval_err("math.bitShiftLeft(1, -1)");
        eval_err("math.bitShiftLeft(1, 64)");
        eval_err("math.bitShiftRight(1, -1)");
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
}
