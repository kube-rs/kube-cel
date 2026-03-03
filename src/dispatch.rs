//! Runtime type dispatch for CEL functions with name collisions.
//!
//! The `cel` crate registers functions by name only (no typed overloads).
//! When the same function name applies to multiple types (e.g., `indexOf` for
//! both strings and lists), this module provides unified dispatch functions
//! that route to the correct implementation based on the runtime type of `this`.

use cel::extractors::{Arguments, This};
use cel::objects::Value;
use cel::{Context, ExecutionError, ResolveResult};

/// Register all dispatch functions. Must be called after individual module registrations
/// to overwrite any conflicting single-type registrations.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("indexOf", index_of);
    ctx.add_function("lastIndexOf", last_index_of);

    // Comparison/arithmetic: shared between semver_funcs and quantity
    ctx.add_function("isGreaterThan", is_greater_than);
    ctx.add_function("isLessThan", is_less_than);
    ctx.add_function("compareTo", compare_to);

    #[cfg(feature = "quantity")]
    {
        ctx.add_function("add", add);
        ctx.add_function("sub", sub);
    }

    // ip: string → parse IP, CIDR → extract network address
    #[cfg(feature = "ip")]
    ctx.add_function("ip", ip_dispatch);

    // string: IP/CIDR → string representation
    #[cfg(feature = "ip")]
    ctx.add_function("string", string_dispatch);

    // reverse: string → reversed string, list → reversed list
    #[cfg(any(feature = "strings", feature = "lists"))]
    ctx.add_function("reverse", reverse);
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

#[allow(unused_variables)]
fn is_greater_than(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let arg = args
        .first()
        .cloned()
        .ok_or_else(|| ExecutionError::function_error("isGreaterThan", "missing argument"))?;

    match &this {
        #[cfg(feature = "semver_funcs")]
        Value::Opaque(o)
            if o.downcast_ref::<crate::semver_funcs::KubeSemver>()
                .is_some() =>
        {
            crate::semver_funcs::semver_is_greater_than(This(this), arg)
        }
        #[cfg(feature = "quantity")]
        Value::Opaque(o) if o.downcast_ref::<crate::quantity::KubeQuantity>().is_some() => {
            crate::quantity::cel_is_greater_than(This(this), arg)
        }
        _ => Err(ExecutionError::function_error(
            "isGreaterThan",
            format!("isGreaterThan not supported on type {:?}", this.type_of()),
        )),
    }
}

#[allow(unused_variables)]
fn is_less_than(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let arg = args
        .first()
        .cloned()
        .ok_or_else(|| ExecutionError::function_error("isLessThan", "missing argument"))?;

    match &this {
        #[cfg(feature = "semver_funcs")]
        Value::Opaque(o)
            if o.downcast_ref::<crate::semver_funcs::KubeSemver>()
                .is_some() =>
        {
            crate::semver_funcs::semver_is_less_than(This(this), arg)
        }
        #[cfg(feature = "quantity")]
        Value::Opaque(o) if o.downcast_ref::<crate::quantity::KubeQuantity>().is_some() => {
            crate::quantity::cel_is_less_than(This(this), arg)
        }
        _ => Err(ExecutionError::function_error(
            "isLessThan",
            format!("isLessThan not supported on type {:?}", this.type_of()),
        )),
    }
}

#[allow(unused_variables)]
fn compare_to(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    let arg = args
        .first()
        .cloned()
        .ok_or_else(|| ExecutionError::function_error("compareTo", "missing argument"))?;

    match &this {
        #[cfg(feature = "semver_funcs")]
        Value::Opaque(o)
            if o.downcast_ref::<crate::semver_funcs::KubeSemver>()
                .is_some() =>
        {
            crate::semver_funcs::semver_compare_to(This(this), arg)
        }
        #[cfg(feature = "quantity")]
        Value::Opaque(o) if o.downcast_ref::<crate::quantity::KubeQuantity>().is_some() => {
            crate::quantity::cel_compare_to(This(this), arg)
        }
        _ => Err(ExecutionError::function_error(
            "compareTo",
            format!("compareTo not supported on type {:?}", this.type_of()),
        )),
    }
}

// ---------------------------------------------------------------------------
// add / sub (quantity only, but accepts Quantity or int)
// ---------------------------------------------------------------------------

#[cfg(feature = "quantity")]
fn add(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    crate::quantity::cel_add(This(this), Arguments(args))
}

#[cfg(feature = "quantity")]
fn sub(This(this): This<Value>, Arguments(args): Arguments) -> ResolveResult {
    crate::quantity::cel_sub(This(this), Arguments(args))
}

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
// string (IP → string, CIDR → string)
// ---------------------------------------------------------------------------

#[cfg(feature = "ip")]
fn string_dispatch(This(this): This<Value>) -> ResolveResult {
    match &this {
        Value::Opaque(o) if o.downcast_ref::<crate::ip::KubeIP>().is_some() => {
            crate::ip::ip_string(This(this))
        }
        Value::Opaque(o) if o.downcast_ref::<crate::ip::KubeCIDR>().is_some() => {
            crate::ip::cidr_string(This(this))
        }
        _ => Err(ExecutionError::function_error(
            "string",
            format!("string not supported on type {:?}", this.type_of()),
        )),
    }
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
