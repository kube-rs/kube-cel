//! Runtime type dispatch for CEL functions with name collisions.
//!
//! The `cel` crate registers functions by name only (no typed overloads).
//! When the same function name applies to multiple types (e.g., `indexOf` for
//! both strings and lists), this module provides unified dispatch functions
//! that route to the correct implementation based on the runtime type of `this`.

use cel::{
    ExecutionError, ResolveResult,
    extractors::{Arguments, This},
    objects::Value,
};

use crate::functions::Registrar;

/// Register dispatch functions for names shared across multiple types.
/// Registration order is independent
/// of individual module registrations since modules no longer register
/// these conflicting names.
///
/// A name is registered only when a feature that implements it is on: with
/// neither `strings` nor `lists` there is no `indexOf` to dispatch to, so
/// registering one would only shadow a user's function of that name.
pub(crate) fn register(r: &mut Registrar<'_, '_, '_>) {
    #[cfg(any(feature = "strings", feature = "lists"))]
    {
        r.add("indexOf", index_of);
        r.add("lastIndexOf", last_index_of);
    }

    // Comparison/arithmetic: shared between semver_funcs and quantity
    #[cfg(any(feature = "semver_funcs", feature = "quantity"))]
    {
        r.add("isGreaterThan", is_greater_than);
        r.add("isLessThan", is_less_than);
        r.add("compareTo", compare_to);
    }

    // ip: string → parse IP, CIDR → extract network address
    #[cfg(feature = "ip")]
    r.add("ip", ip_dispatch);
}

// ---------------------------------------------------------------------------
// indexOf / lastIndexOf
// ---------------------------------------------------------------------------

#[cfg(any(feature = "strings", feature = "lists"))]
#[allow(unused_variables)]
fn index_of(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        #[cfg(feature = "strings")]
        Value::String(s) => crate::functions::strings::string_index_of(This(s), Arguments(args)),

        #[cfg(feature = "lists")]
        Value::List(list) => crate::functions::lists::list_index_of(&list, &args),

        _ => Err(ExecutionError::function_error(
            "indexOf",
            format!("indexOf not supported on type {:?}", this.type_of()),
        )),
    }
}

#[cfg(any(feature = "strings", feature = "lists"))]
#[allow(unused_variables)]
fn last_index_of(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match this {
        #[cfg(feature = "strings")]
        Value::String(s) => crate::functions::strings::string_last_index_of(This(s), Arguments(args)),

        #[cfg(feature = "lists")]
        Value::List(list) => crate::functions::lists::list_last_index_of(&list, &args),

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
        #[cfg(any(feature = "semver_funcs", feature = "quantity"))]
        #[allow(unused_variables)]
        fn $fn_name(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
            let arg = args
                .first()
                .cloned()
                .ok_or_else(|| ExecutionError::function_error($name, "missing argument"))?;
            match &this {
                #[cfg(feature = "semver_funcs")]
                Value::Opaque(o)
                    if o.downcast_ref::<crate::functions::semver_funcs::KubeSemver>()
                        .is_some() =>
                {
                    $semver_fn(This(this), arg)
                }
                #[cfg(feature = "quantity")]
                Value::Opaque(o)
                    if o.downcast_ref::<crate::functions::quantity::KubeQuantity>()
                        .is_some() =>
                {
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
    crate::functions::semver_funcs::semver_is_greater_than,
    crate::functions::quantity::cel_is_greater_than
);
opaque_comparison_dispatch!(
    is_less_than,
    "isLessThan",
    crate::functions::semver_funcs::semver_is_less_than,
    crate::functions::quantity::cel_is_less_than
);
opaque_comparison_dispatch!(
    compare_to,
    "compareTo",
    crate::functions::semver_funcs::semver_compare_to,
    crate::functions::quantity::cel_compare_to
);

// ---------------------------------------------------------------------------
// ip (string → parse IP, CIDR → extract network address)
// ---------------------------------------------------------------------------

#[cfg(feature = "ip")]
fn ip_dispatch(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    match &this {
        Value::Opaque(o) if o.downcast_ref::<crate::functions::ip::KubeCIDR>().is_some() => {
            crate::functions::ip::cidr_ip(This(this))
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
            let addr = crate::functions::ip::parse_ip_addr(&s)
                .map_err(|e| ExecutionError::function_error("ip", e))?;
            Ok(Value::Opaque(std::sync::Arc::new(
                crate::functions::ip::KubeIP::new(addr),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(dead_code)]
    use cel::Value;

    fn eval(expr: &str) -> Value {
        crate::functions::eval(expr).unwrap()
    }

    fn eval_err(expr: &str) -> cel::ExecutionError {
        crate::functions::eval(expr).unwrap_err()
    }

    // --- Dispatch error tests ---

    #[test]
    #[cfg(feature = "strings")]
    fn test_index_of_unsupported_type() {
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
