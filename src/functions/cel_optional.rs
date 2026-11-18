//! cel-go's short-circuiting `or` / `orValue` on optionals.
//!
//! cel-go evaluates `x.or(y)` and `x.orValue(y)` lazily (`evalOptionalOr`,
//! `evalOptionalOrValue`): `x` first, and `y` only when `x` is an optional
//! holding no value. `cel` 0.16 evaluates `y` first, then `x`, so a `y` that
//! fails fails the call where cel-go returns `x`'s value, and a `y` that
//! updates a quantity's state does so where cel-go does not. [`declare`] adds
//! macros that rewrite them, at compile time, to
//!
//! ```text
//! x.or(y)      → cel.bind(@x, x, @kube.optionalNone(@x) ? y : @x)
//! x.orValue(y) → cel.bind(@x, x, @kube.optionalNone(@x) ? y : @kube.optionalValue(@x))
//! ```
//!
//! written out as the comprehension `cel.bind` stands for. As in cel-go, an
//! `x` that is not an optional is the result as it is.

use cel::{
    DeclarationError, Env, ExecutionError,
    common::{
        ast::{CallExpr, ComprehensionExpr, Expr, IdedExpr, ListExpr, LiteralValue, operators},
        decls::Signature,
        types::{CelBool, CelOptional, DYN_TYPE},
        value::CowVal,
    },
    parser::{Macro, MacroExprHelper},
};

const OPTIONAL_NONE: &str = "@kube.optionalNone";
const OPTIONAL_VALUE: &str = "@kube.optionalValue";
/// The variable the receiver is bound to: as it starts with `@`, CEL source
/// cannot refer to it.
const RECEIVER: &str = "@__kube_optional__";

/// Adds the macros and helpers described in the [module docs](self).
pub(crate) fn declare(env: &mut Env) -> Result<(), DeclarationError> {
    env.add_overload(
        OPTIONAL_NONE,
        OPTIONAL_NONE,
        Signature::new(vec![DYN_TYPE]),
        optional_none,
    )?;
    env.add_overload(
        OPTIONAL_VALUE,
        OPTIONAL_VALUE,
        Signature::new(vec![DYN_TYPE]),
        optional_value,
    )?;
    for (name, unwrap) in [("or", false), ("orValue", true)] {
        env.add_macro(Macro::receiver(name, 1, move |h, target, args| {
            let receiver = target.take().expect("a receiver macro has a target");
            let alternative = args.pop().expect("one argument");
            Ok(Some(lazy(h, receiver, alternative, unwrap)))
        }))?;
    }
    Ok(())
}

fn ident(h: &mut MacroExprHelper<'_>) -> IdedExpr {
    h.next_expr(Expr::Ident(RECEIVER.to_owned()))
}

fn call(h: &mut MacroExprHelper<'_>, name: &str, args: Vec<IdedExpr>) -> IdedExpr {
    h.next_expr(Expr::Call(CallExpr {
        target: None,
        func_name: name.to_owned(),
        args,
    }))
}

/// `cel.bind(@x, receiver, @kube.optionalNone(@x) ? alternative : @x)`, with
/// `@kube.optionalValue(@x)` for the last if `unwrap`.
fn lazy(h: &mut MacroExprHelper<'_>, receiver: IdedExpr, alternative: IdedExpr, unwrap: bool) -> IdedExpr {
    let bound = ident(h);
    let none = call(h, OPTIONAL_NONE, vec![bound]);
    let bound = ident(h);
    let present = if unwrap {
        call(h, OPTIONAL_VALUE, vec![bound])
    } else {
        bound
    };
    let result = call(h, operators::CONDITIONAL, vec![none, alternative, present]);
    let iter_range = h.next_expr(Expr::List(ListExpr::new(Vec::new())));
    let loop_cond = h.next_expr(Expr::Literal(LiteralValue::Boolean(false.into())));
    let loop_step = ident(h);
    h.next_expr(Expr::Comprehension(Box::new(ComprehensionExpr {
        iter_range,
        iter_var: "#unused".to_owned(),
        iter_var2: None,
        accu_var: RECEIVER.to_owned(),
        accu_init: receiver,
        loop_cond,
        loop_step,
        result,
    })))
}

fn single<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let n = args.len();
    let [arg]: [CowVal<'b, 'v>; 1] = args
        .try_into()
        .map_err(|_| ExecutionError::invalid_argument_count(1, n))?;
    Ok(arg)
}

/// Whether the value is an optional holding no value.
fn optional_none<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    let none = arg
        .downcast_ref::<CelOptional>()
        .is_some_and(|o| o.option().is_none());
    Ok(CowVal::owned(CelBool::from(none)))
}

/// An optional's value; any other value as it is.
fn optional_value<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let arg = single(args)?;
    match arg {
        CowVal::Borrowed(b) => Ok(match b.downcast_ref::<CelOptional>().and_then(|o| o.option()) {
            Some(inner) => CowVal::Borrowed(inner),
            None => CowVal::Borrowed(b),
        }),
        CowVal::Owned(o) => Ok(match o.downcast_ref::<CelOptional>().and_then(|o| o.option()) {
            Some(inner) => CowVal::Owned(inner.clone_as_boxed()),
            None => CowVal::Owned(o),
        }),
    }
}

#[cfg(test)]
mod tests {
    use cel::Value;

    use crate::functions::eval;

    fn holds(expr: &str) {
        assert_eq!(eval(expr), Ok(Value::Bool(true)), "{expr}");
    }

    // Expected values are cel-go's (k8s.io/apiserver v0.36.1).

    #[test]
    fn or_and_or_value_short_circuit() {
        holds("optional.of(1).or(optional.of(1 / 0)).value() == 1");
        holds("optional.of(1).orValue(1 / 0) == 1");
        holds("optional.none().or(optional.of(2)).value() == 2");
        holds("optional.none().orValue(2) == 2");
        holds("!optional.none().or(optional.none()).hasValue()");
        holds("{'a': 1}[?'a'].orValue(0) == 1 && {'a': 1}[?'b'].orValue(0) == 0");
        holds("optional.of(optional.of(1)).orValue(optional.none()).value() == 1");
        assert!(eval("optional.none().orValue(1 / 0)").is_err());
        assert!(eval("(1 / 0 == 0 ? optional.none() : optional.none()).or(optional.of(1))").is_err());
    }

    /// cel-go returns a receiver that is not an optional as it is.
    #[test]
    fn a_receiver_that_is_not_an_optional_is_the_result() {
        holds("dyn(dyn(1).or(optional.of(2))) == 1");
        holds("dyn(dyn(1).orValue(2)) == 1");
    }
}
