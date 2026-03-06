//! Runtime type dispatch for CEL functions with name collisions.
//!
//! The `cel` crate registers functions by name only (no typed overloads).
//! When the same function name applies to multiple types (e.g., `indexOf` for
//! both strings and lists), this module provides unified dispatch functions
//! that route to the correct implementation based on the runtime type of `this`.

use std::sync::Arc;

use cel::extractors::{Arguments, This};
use cel::objects::Value;
use cel::{Context, ExecutionError, ResolveResult};

/// Register dispatch functions for names shared across multiple types or
/// that override cel built-in functions. Registration order is independent
/// of individual module registrations since modules no longer register
/// these conflicting names.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("indexOf", index_of);
    ctx.add_function("lastIndexOf", last_index_of);

    // Comparison/arithmetic: shared between semver_funcs and quantity
    ctx.add_function("isGreaterThan", is_greater_than);
    ctx.add_function("isLessThan", is_less_than);
    ctx.add_function("compareTo", compare_to);

    // ip: string → parse IP, CIDR → extract network address
    #[cfg(feature = "ip")]
    ctx.add_function("ip", ip_dispatch);

    // string: IP/CIDR → string representation
    #[cfg(feature = "ip")]
    ctx.add_function("string", string_dispatch);

    // reverse: string → reversed string, list → reversed list
    #[cfg(any(feature = "strings", feature = "lists"))]
    ctx.add_function("reverse", reverse);

    // min/max: list method vs cel built-in variadic
    #[cfg(feature = "lists")]
    {
        ctx.add_function("min", min_dispatch);
        ctx.add_function("max", max_dispatch);
    }
}

// ---------------------------------------------------------------------------
// indexOf / lastIndexOf
// ---------------------------------------------------------------------------

#[allow(unused_variables)]
fn index_of(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        #[cfg(feature = "strings")]
        Value::String(s) => crate::strings::string_index_of(This(s), Arguments(args)),

        #[cfg(feature = "lists")]
        Value::List(list) => crate::lists::list_index_of(&list, &args),

        _ => Err(ExecutionError::function_error(
            "indexOf",
            format!("indexOf not supported on type {:?}", this.type_of()),
        )),
    }
}

#[allow(unused_variables)]
fn last_index_of(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        #[cfg(feature = "strings")]
        Value::String(s) => crate::strings::string_last_index_of(This(s), Arguments(args)),

        #[cfg(feature = "lists")]
        Value::List(list) => crate::lists::list_last_index_of(&list, &args),

        _ => Err(ExecutionError::function_error(
            "lastIndexOf",
            format!("lastIndexOf not supported on type {:?}", this.type_of()),
        )),
    }
}

// ---------------------------------------------------------------------------
// isGreaterThan / isLessThan / compareTo
// ---------------------------------------------------------------------------

/// Generate a dispatch function that routes to semver or quantity based on opaque type.
macro_rules! opaque_comparison_dispatch {
    ($fn_name:ident, $name:literal, $semver_fn:path, $quantity_fn:path) => {
        #[allow(unused_variables)]
        fn $fn_name(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
            let arg = args
                .first()
                .cloned()
                .ok_or_else(|| ExecutionError::function_error($name, "missing argument"))?;
            match &this {
                #[cfg(feature = "semver_funcs")]
                Value::Opaque(o)
                    if o.downcast_ref::<crate::semver_funcs::KubeSemver>()
                        .is_some() =>
                {
                    $semver_fn(This(this), arg)
                }
                #[cfg(feature = "quantity")]
                Value::Opaque(o) if o.downcast_ref::<crate::quantity::KubeQuantity>().is_some() => {
                    $quantity_fn(This(this), arg)
                }
                _ => Err(ExecutionError::function_error(
                    $name,
                    format!("{} not supported on type {:?}", $name, this.type_of()),
                )),
            }
        }
    };
}

opaque_comparison_dispatch!(
    is_greater_than,
    "isGreaterThan",
    crate::semver_funcs::semver_is_greater_than,
    crate::quantity::cel_is_greater_than
);
opaque_comparison_dispatch!(
    is_less_than,
    "isLessThan",
    crate::semver_funcs::semver_is_less_than,
    crate::quantity::cel_is_less_than
);
opaque_comparison_dispatch!(
    compare_to,
    "compareTo",
    crate::semver_funcs::semver_compare_to,
    crate::quantity::cel_compare_to
);

// ---------------------------------------------------------------------------
// ip (string → parse IP, CIDR → extract network address)
// ---------------------------------------------------------------------------

#[cfg(feature = "ip")]
fn ip_dispatch(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match &this {
        Value::Opaque(o) if o.downcast_ref::<crate::ip::KubeCIDR>().is_some() => {
            crate::ip::cidr_ip(This(this))
        }
        _ => {
            // Fallback: treat first argument (or this for global call) as string
            let s = match args.first() {
                Some(Value::String(s)) => s.clone(),
                _ => match this {
                    Value::String(s) => s,
                    _ => {
                        return Err(ExecutionError::function_error(
                            "ip",
                            "expected string or CIDR argument",
                        ));
                    }
                },
            };
            let addr = crate::ip::parse_ip_addr(&s)
                .map_err(|e| ExecutionError::function_error("ip", e))?;
            Ok(Value::Opaque(std::sync::Arc::new(crate::ip::KubeIP::new(
                addr,
            ))))
        }
    }
}

// ---------------------------------------------------------------------------
// string (IP/CIDR → string, plus cel built-in fallback)
// ---------------------------------------------------------------------------
//
// Overriding cel's built-in `string()` is unavoidable: K8s CEL spec requires
// `ip("1.2.3.4").string()` to work, but cel's built-in rejects Opaque types.
// Since the function registry has no overload support, we must replace it and
// reimplement the standard type conversions.
//
// The standard conversions below mirror `cel::functions::string` (cel 0.12).
// cel::functions::string is pub but requires &FunctionContext which is not
// available in the extractor-based API, so direct delegation is not possible.

#[cfg(feature = "ip")]
fn string_dispatch(This(this): This<Value>) -> ResolveResult {
    match &this {
        // Opaque types: K8s CEL extensions
        Value::Opaque(o) if o.downcast_ref::<crate::ip::KubeIP>().is_some() => {
            crate::ip::ip_string(This(this))
        }
        Value::Opaque(o) if o.downcast_ref::<crate::ip::KubeCIDR>().is_some() => {
            crate::ip::cidr_string(This(this))
        }
        // Standard types: mirrors cel::functions::string (cel 0.12)
        _ => builtin_string_fallback(this),
    }
}

/// Reimplements cel's built-in `string()` for standard types.
/// Must stay in sync with `cel::functions::string` (cel 0.12).
fn builtin_string_fallback(this: Value) -> ResolveResult {
    match this {
        Value::String(_) => Ok(this),
        Value::Int(n) => Ok(Value::String(Arc::new(n.to_string()))),
        Value::UInt(n) => Ok(Value::String(Arc::new(n.to_string()))),
        Value::Float(f) => Ok(Value::String(Arc::new(f.to_string()))),
        Value::Bytes(ref b) => Ok(Value::String(Arc::new(
            String::from_utf8_lossy(b.as_slice()).into(),
        ))),
        Value::Timestamp(ref t) => Ok(Value::String(Arc::new(t.to_rfc3339()))),
        Value::Duration(ref d) => {
            Ok(Value::String(Arc::new(format_cel_duration(
                d.num_nanoseconds()
                    .unwrap_or(d.num_seconds() * 1_000_000_000),
            ))))
        }
        _ => Err(ExecutionError::function_error(
            "string",
            format!("cannot convert {:?} to string", this.type_of()),
        )),
    }
}

/// Format nanoseconds matching Go's `time.Duration.String()`.
/// Mirrors `cel::duration::format_duration` which is not pub.
fn format_cel_duration(total_nanos: i64) -> String {
    if total_nanos == 0 {
        return "0s".into();
    }

    let neg = total_nanos < 0;
    let u = total_nanos.unsigned_abs();
    let mut result = String::new();
    if neg {
        result.push('-');
    }

    const NS_SECOND: u64 = 1_000_000_000;
    const NS_MINUTE: u64 = 60 * NS_SECOND;
    const NS_HOUR: u64 = 60 * NS_MINUTE;

    if u >= NS_SECOND {
        let hours = u / NS_HOUR;
        let mins = (u % NS_HOUR) / NS_MINUTE;
        let secs = (u % NS_MINUTE) / NS_SECOND;
        let frac = u % NS_SECOND;
        if hours > 0 {
            result.push_str(&format!("{hours}h"));
        }
        if hours > 0 || mins > 0 {
            result.push_str(&format!("{mins}m"));
        }
        if frac > 0 {
            let frac_s = format!("{frac:09}");
            let frac_s = frac_s.trim_end_matches('0');
            result.push_str(&format!("{secs}.{frac_s}s"));
        } else {
            result.push_str(&format!("{secs}s"));
        }
    } else {
        const NS_MILLISECOND: u64 = 1_000_000;
        const NS_MICROSECOND: u64 = 1_000;
        if u >= NS_MILLISECOND {
            let ms = u as f64 / NS_MILLISECOND as f64;
            let s = format!("{ms:.3}");
            result.push_str(s.trim_end_matches('0').trim_end_matches('.'));
            result.push_str("ms");
        } else if u >= NS_MICROSECOND {
            let us = u as f64 / NS_MICROSECOND as f64;
            let s = format!("{us:.3}");
            result.push_str(s.trim_end_matches('0').trim_end_matches('.'));
            result.push_str("µs");
        } else {
            result.push_str(&format!("{u}ns"));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// reverse (string → reversed string, list → reversed list)
// ---------------------------------------------------------------------------

#[cfg(any(feature = "strings", feature = "lists"))]
#[allow(unused_variables)]
fn reverse(This(this): This<Value>) -> ResolveResult {
    match this {
        #[cfg(feature = "strings")]
        Value::String(s) => crate::strings::string_reverse(This(s)),

        #[cfg(feature = "lists")]
        Value::List(list) => crate::lists::list_reverse_value(This(list)),

        _ => Err(ExecutionError::function_error(
            "reverse",
            format!("reverse not supported on type {:?}", this.type_of()),
        )),
    }
}

// ---------------------------------------------------------------------------
// min / max (list method vs cel built-in variadic)
// ---------------------------------------------------------------------------

#[cfg(feature = "lists")]
fn min_dispatch(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        Value::List(list) if args.is_empty() => crate::lists::list_min(This(list)),
        _ => {
            let mut all_args = vec![this];
            all_args.extend(args.iter().cloned());
            cel::functions::min(Arguments(Arc::new(all_args)))
        }
    }
}

#[cfg(feature = "lists")]
fn max_dispatch(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        Value::List(list) if args.is_empty() => crate::lists::list_max(This(list)),
        _ => {
            let mut all_args = vec![this];
            all_args.extend(args.iter().cloned());
            cel::functions::max(Arguments(Arc::new(all_args)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    #[allow(dead_code)]
    fn eval_err(expr: &str) -> cel::ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        #[cfg(feature = "strings")]
        crate::strings::register(&mut ctx);
        #[cfg(feature = "lists")]
        crate::lists::register(&mut ctx);
        #[cfg(feature = "semver_funcs")]
        crate::semver_funcs::register(&mut ctx);
        #[cfg(feature = "quantity")]
        crate::quantity::register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_index_of_unsupported_type() {
        // indexOf on a non-string, non-list type should error
        eval_err("true.indexOf('x')");
    }

    #[test]
    #[cfg(feature = "strings")]
    fn test_last_index_of_unsupported_type() {
        eval_err("true.lastIndexOf('x')");
    }

    #[test]
    #[cfg(feature = "semver_funcs")]
    fn test_is_greater_than_unsupported_type() {
        eval_err("'hello'.isGreaterThan('world')");
    }

    #[test]
    #[cfg(feature = "semver_funcs")]
    fn test_is_less_than_unsupported_type() {
        eval_err("'hello'.isLessThan('world')");
    }

    #[test]
    #[cfg(feature = "semver_funcs")]
    fn test_compare_to_unsupported_type() {
        eval_err("'hello'.compareTo('world')");
    }
}
