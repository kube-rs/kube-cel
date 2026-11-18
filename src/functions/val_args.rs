//! Function arguments read as `cel` [`Val`]s, so kube-cel's functions take the
//! values `cel` cannot turn into a [`Value`]: the schema-aware lists and
//! objects the validation engine binds for a CRD rule
//! (`x-kubernetes-list-type: set` / `map`, objects with declared properties),
//! and any list holding one.
//!
//! `cel` converts a function's arguments to [`Value`]s through its built-in
//! types only, so a legacy extractor (`This<Arc<Vec<Value>>>`, `Arguments`)
//! fails on those values before the function runs. A function registered
//! through [`snapshot`] gets an ordered [`to_value`] copy instead, which is
//! enough where the result does not depend on how the elements compare; a
//! function that compares elements or returns one works on the `Val`s
//! themselves, keeping their own equality.

use std::sync::Arc;

use cel::{
    ExecutionError, FunctionContext, ResolveResult, Value,
    common::{
        types::Kind,
        value::{CowVal, Val},
    },
    extractors::IntoResolveResult,
    objects::{Key, Map},
};

/// A function as `cel` stores it, taking the call's [`FunctionContext`] and
/// returning a [`Val`].
pub(crate) type RawFunction = Box<
    dyn for<'c, 'call> Fn(&mut FunctionContext<'c, 'call>) -> Result<CowVal<'c, 'call>, ExecutionError>
        + Send
        + Sync,
>;

/// `val` as a [`Value`]: as `cel` converts it, but a list or a map through its
/// elements, so a list or map of an application's own type, or a list holding
/// one, converts too. An ordered snapshot: the copy compares as a plain list or
/// map. A map whose iteration yields a key it then does not hold (an object's
/// `null` property) fails, as the apiserver's formatter does.
pub(crate) fn to_value(val: &dyn Val) -> Result<Value, ExecutionError> {
    match val.get_type().kind() {
        Kind::List => {
            let Some(iterable) = val.as_iterable() else {
                return Value::try_from(val);
            };
            let mut items = iterable.iter();
            let mut out = Vec::new();
            while let Some(item) = items.next() {
                out.push(to_value(item)?);
            }
            Ok(Value::List(Arc::new(out)))
        }
        Kind::Map => {
            let (Some(iterable), Some(indexer)) = (val.as_iterable(), val.as_indexer()) else {
                return Value::try_from(val);
            };
            let mut keys = iterable.iter();
            let mut out = std::collections::HashMap::new();
            while let Some(key) = keys.next() {
                let value = to_value(indexer.get(key)?.as_ref())?;
                let key = match Value::try_from(key)? {
                    Value::String(s) => Key::String(s),
                    Value::Int(i) => Key::Int(i),
                    Value::UInt(u) => Key::Uint(u),
                    Value::Bool(b) => Key::Bool(b),
                    other => {
                        return Err(ExecutionError::function_error(
                            "map",
                            format!("unsupported map key {other:?}"),
                        ));
                    }
                };
                out.insert(key, value);
            }
            Ok(Value::Map(Map { map: Arc::new(out) }))
        }
        _ => Value::try_from(val),
    }
}

/// A function of the [`to_value`] snapshots of the call's target, if any,
/// and arguments: the legacy body of a function whose result does not depend
/// on how elements compare (`join`, `sum`, `format`, ..).
#[allow(dead_code)] // unused when no function feature is enabled
pub(crate) fn snapshot<F>(f: F) -> RawFunction
where
    F: Fn(Option<Value>, Vec<Value>) -> ResolveResult + Send + Sync + 'static,
{
    Box::new(move |ftx| {
        let this = ftx.this.as_ref().map(|v| to_value(v.as_ref())).transpose()?;
        let args = ftx
            .args
            .iter()
            .map(|a| to_value(a.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        f(this, args).into_resolve_result()
    })
}

/// The call's target and the remaining arguments, as a legacy `This`
/// extractor takes them: the target, or else the first argument.
#[allow(dead_code)]
pub(crate) fn split_target(
    this: Option<Value>,
    mut args: Vec<Value>,
) -> Result<(Value, Vec<Value>), ExecutionError> {
    match this {
        Some(this) => Ok((this, args)),
        None if !args.is_empty() => {
            let this = args.remove(0);
            Ok((this, args))
        }
        None => Err(ExecutionError::missing_argument_or_target()),
    }
}

/// The snapshot of a call's target (or first argument) as the list a legacy
/// `This<Arc<Vec<Value>>>` extracts, and the remaining arguments.
#[allow(dead_code)]
pub(crate) fn list_target(
    name: &str,
    this: Option<Value>,
    args: Vec<Value>,
) -> Result<(Arc<Vec<Value>>, Vec<Value>), ExecutionError> {
    match split_target(this, args)? {
        (Value::List(list), args) => Ok((list, args)),
        (other, _) => Err(ExecutionError::function_error(
            name,
            format!("expected a list, got {:?}", other.type_of()),
        )),
    }
}

/// The call's target `Val`, or else its first argument, and the remaining
/// arguments.
#[allow(dead_code)]
pub(crate) fn val_target<'f, 'c, 'call>(
    ftx: &'f FunctionContext<'c, 'call>,
) -> Result<(&'f (dyn Val + 'call), &'f [CowVal<'c, 'call>]), ExecutionError> {
    match &ftx.this {
        Some(this) => Ok((this.as_ref(), ftx.args.as_slice())),
        None => match ftx.args.split_first() {
            Some((first, rest)) => Ok((first.as_ref(), rest)),
            None => Err(ExecutionError::missing_argument_or_target()),
        },
    }
}

/// The elements of a list `Val`, borrowed.
#[allow(dead_code)]
pub(crate) fn list_items<'b, 'v>(
    name: &str,
    list: &'b (dyn Val + 'v),
) -> Result<Vec<&'b (dyn Val + 'v)>, ExecutionError> {
    let iterable = list
        .as_iterable()
        .filter(|_| list.get_type().kind() == Kind::List)
        .ok_or_else(|| {
            ExecutionError::function_error(name, format!("expected a list, got {}", list.get_type().name()))
        })?;
    let mut items = iterable.iter();
    let mut out = Vec::new();
    while let Some(item) = items.next() {
        out.push(item);
    }
    Ok(out)
}

/// Whether `list` holds `value`, the list's element on the left of `==`, as
/// the apiserver's schema-backed lists compare (`unstructuredList.Contains`).
#[allow(dead_code)]
pub(crate) fn holds(name: &str, list: &dyn Val, value: &dyn Val) -> Result<bool, ExecutionError> {
    Ok(list_items(name, list)?.iter().any(|item| item.equals(value)))
}
