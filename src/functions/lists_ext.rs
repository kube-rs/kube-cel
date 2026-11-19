//! cel-go's `ext.Lists` at version 3, as the apiserver's base environment
//! installs it: `slice`, `flatten`, `distinct`, `reverse`, `sort`,
//! `lists.range` and the `sortBy` macro, declared on the [`Env`].
//!
//! kube-cel carries its own rather than `cel`'s `lists` extension because
//! `cel` sorts stably and cel-go does not. `sort()` and `sortBy()` reorder the
//! indices of a list with go1.26.2's `sort.Slice` (see [`go_sort`]), as
//! cel-go's `sortListByAssociatedKeys` does, so elements with equal keys end up
//! where the apiserver puts them. The other functions keep the results `cel`'s
//! extension gives, which are cel-go's, but for `lists.range` of a negative
//! size, which is `[]` here as in cel-go.
//!
//! [`go_sort`]: crate::functions::go_sort

use cel::{
    DeclarationError, Env, ExecutionError,
    common::{
        ast::{CallExpr, ComprehensionExpr, Expr, IdedExpr, ListExpr, LiteralValue, operators},
        types::{CelInt, CelList, INT_TYPE, Kind, LIST_TYPE},
        value::{CowVal, Val},
    },
    parser::{Macro, MacroExprHelper, ParseError},
};

use crate::functions::go_sort::go_sort_slice;

/// The overload `sortBy` expands to, named so that no source can call it, and
/// kube-cel's own rather than cel-go's `@sortByAssociatedKeys`.
const SORT_BY_KEYS: &str = "@kubecel.sortByAssociatedKeys";

/// The variable `sortBy` binds its target to, so that it is evaluated once
/// (cel-go's name).
const SORT_BY_INPUT: &str = "@__sortBy_input__";

/// The accumulator of the key list `sortBy` builds (the name `cel`'s own
/// macros use).
const ACCUMULATOR: &str = "@result";

/// The most elements `lists.range` creates, as `cel`'s extension allows.
/// cel-go has no limit; the apiserver's cost limit refuses far smaller ranges.
const MAX_RANGE_SIZE: i64 = 1_000_000;

/// Declares the library on `env`.
pub(crate) fn declare(env: &mut Env) -> Result<(), DeclarationError> {
    env.add_member_overload(
        "slice",
        "list.slice(int,int)",
        LIST_TYPE,
        vec![INT_TYPE, INT_TYPE],
        slice,
    )?;
    env.add_member_overload("flatten", "list.flatten()", LIST_TYPE, vec![], flatten)?;
    env.add_member_overload(
        "flatten",
        "list.flatten(int)",
        LIST_TYPE,
        vec![INT_TYPE],
        flatten_depth,
    )?;
    env.add_member_overload("distinct", "list.distinct()", LIST_TYPE, vec![], distinct)?;
    env.add_member_overload("reverse", "list.reverse()", LIST_TYPE, vec![], reverse)?;
    env.add_member_overload("sort", "list.sort()", LIST_TYPE, vec![], sort)?;
    env.add_member_overload(
        SORT_BY_KEYS,
        "list.@kubecel.sortByAssociatedKeys(list)",
        LIST_TYPE,
        vec![LIST_TYPE],
        sort_by_keys,
    )?;
    env.add_overload("lists.range", "lists.range(int)", vec![INT_TYPE], range)?;
    env.add_macro(Macro::receiver("sortBy", 2, sort_by_macro))?;
    Ok(())
}

/// Expands `list.sortBy(e, key)`, as cel-go does, into
/// `cel.bind(@__sortBy_input__, list,
/// @__sortBy_input__.@kubecel.sortByAssociatedKeys(@__sortBy_input__.map(e, key)))`,
/// written out as the comprehensions `cel.bind` and `map` stand for.
fn sort_by_macro(
    helper: &mut MacroExprHelper<'_>,
    target: &mut Option<IdedExpr>,
    args: &mut Vec<IdedExpr>,
) -> Result<Option<IdedExpr>, ParseError> {
    let list = target.take().expect("sortBy is a receiver macro");
    if matches!(list.expr, Expr::Literal(_) | Expr::Map(_) | Expr::Struct(_)) {
        return Err(helper.new_error(
            list.id,
            "sortBy can only be applied to a list, identifier, comprehension, call or select expression",
        ));
    }
    let key = args.pop().expect("sortBy takes two arguments");
    let IdedExpr { id, expr } = args.pop().expect("sortBy takes two arguments");
    let Expr::Ident(iter_var) = expr else {
        return Err(helper.new_error(id, "argument must be a simple name"));
    };

    // @__sortBy_input__.map(iter_var, key)
    let input = |helper: &mut MacroExprHelper<'_>| helper.next_expr(Expr::Ident(SORT_BY_INPUT.to_owned()));
    let iter_range = input(helper);
    let accu_init = helper.next_expr(Expr::List(ListExpr::new(Vec::new())));
    let loop_cond = helper.next_expr(Expr::Literal(LiteralValue::Boolean(true.into())));
    let accu = helper.next_expr(Expr::Ident(ACCUMULATOR.to_owned()));
    let item = helper.next_expr(Expr::List(ListExpr::new(vec![key])));
    let loop_step = helper.next_expr(Expr::Call(CallExpr {
        func_name: operators::ADD.to_owned(),
        target: None,
        args: vec![accu, item],
    }));
    let result = helper.next_expr(Expr::Ident(ACCUMULATOR.to_owned()));
    let keys = helper.next_expr(Expr::Comprehension(Box::new(ComprehensionExpr {
        iter_range,
        iter_var,
        iter_var2: None,
        accu_var: ACCUMULATOR.to_owned(),
        accu_init,
        loop_cond,
        loop_step,
        result,
    })));

    let receiver = input(helper);
    let sorted = helper.next_expr(Expr::Call(CallExpr {
        func_name: SORT_BY_KEYS.to_owned(),
        target: Some(Box::new(receiver)),
        args: vec![keys],
    }));

    // cel.bind(@__sortBy_input__, list, sorted): no iterations, the
    // accumulator holding the bound value.
    let iter_range = helper.next_expr(Expr::List(ListExpr::new(Vec::new())));
    let loop_cond = helper.next_expr(Expr::Literal(LiteralValue::Boolean(false.into())));
    let loop_step = helper.next_expr(Expr::Ident(SORT_BY_INPUT.to_owned()));
    Ok(Some(helper.next_expr(Expr::Comprehension(Box::new(
        ComprehensionExpr {
            iter_range,
            iter_var: "#unused".to_owned(),
            iter_var2: None,
            accu_var: SORT_BY_INPUT.to_owned(),
            accu_init: list,
            loop_cond,
            loop_step,
            result: sorted,
        },
    )))))
}

/// `args` as the `N` arguments of an overload.
fn take_args<'b, 'v, const N: usize>(
    args: Vec<CowVal<'b, 'v>>,
) -> Result<[CowVal<'b, 'v>; N], ExecutionError> {
    args.try_into()
        .map_err(|args: Vec<_>| ExecutionError::invalid_argument_count(N, args.len()))
}

/// The elements of `list`, borrowed.
fn elements<'b, 'v>(list: &'b (dyn Val + 'v)) -> Result<Vec<&'b (dyn Val + 'v)>, ExecutionError> {
    let iterable = list.as_iterable().ok_or_else(|| ExecutionError::UnexpectedType {
        got: list.get_type().name().to_owned(),
        want: "list".to_owned(),
    })?;
    let mut items = iterable.iter();
    let mut out = Vec::new();
    while let Some(item) = items.next() {
        out.push(item);
    }
    Ok(out)
}

fn owned_list<'b, 'v>(elements: Vec<Box<dyn Val + 'v>>) -> CowVal<'b, 'v> {
    CowVal::owned(CelList::from(elements))
}

fn int_arg(val: &dyn Val) -> Result<i64, ExecutionError> {
    val.downcast_ref::<CelInt>()
        .map(|n| *n.inner())
        .ok_or_else(|| ExecutionError::UnexpectedType {
            got: val.get_type().name().to_owned(),
            want: "int".to_owned(),
        })
}

/// The elements from index `start` up to, but excluding, `end`.
fn slice<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list, start, end] = take_args(args)?;
    let (start, end) = (int_arg(start.as_ref())?, int_arg(end.as_ref())?);
    let invalid = |reason: String| {
        ExecutionError::function_error("slice", format!("cannot slice({start}, {end}), {reason}"))
    };
    if start < 0 || end < 0 {
        return Err(invalid("negative indexes not supported".to_owned()));
    }
    if start > end {
        return Err(invalid(
            "start index must be less than or equal to end index".to_owned(),
        ));
    }
    let items = elements(list.as_ref())?;
    let len = items.len() as i64;
    if len < end {
        return Err(invalid(format!("list is length {len}")));
    }
    // Both bounds are within the list here.
    let (start, end) = (start as usize, end as usize);
    Ok(owned_list(
        items[start..end]
            .iter()
            .map(|item| item.clone_as_boxed())
            .collect(),
    ))
}

fn flatten<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list] = take_args(args)?;
    flatten_to(list.as_ref(), 1)
}

fn flatten_depth<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list, depth] = take_args(args)?;
    flatten_to(list.as_ref(), int_arg(depth.as_ref())?)
}

/// `list` with the elements of its nested lists spliced in, `depth` levels
/// deep. Only lists are flattened: a map is iterable too, but kept whole.
fn flatten_to<'b, 'v>(list: &(dyn Val + 'v), depth: i64) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let depth = u64::try_from(depth)
        .map_err(|_| ExecutionError::function_error("flatten", "level must be non-negative"))?;
    let mut flat = Vec::new();
    flatten_into(list, depth, &mut flat)?;
    Ok(owned_list(flat))
}

fn flatten_into<'v>(
    list: &(dyn Val + 'v),
    depth: u64,
    flat: &mut Vec<Box<dyn Val + 'v>>,
) -> Result<(), ExecutionError> {
    for element in elements(list)? {
        if depth > 0 && element.get_type().kind() == Kind::List {
            flatten_into(element, depth - 1, flat)?;
        } else {
            flat.push(element.clone_as_boxed());
        }
    }
    Ok(())
}

/// The first of each set of equal elements, in order. Equality is CEL's, so
/// `1`, `1u` and `1.0` are one element.
fn distinct<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list] = take_args(args)?;
    let items = elements(list.as_ref())?;
    // cel-go's `distinctList` returns an empty list itself, so an empty set
    // stays a set (its `+` a union).
    if items.is_empty() {
        return Ok(list);
    }
    let mut unique: Vec<&dyn Val> = Vec::new();
    for item in items {
        if !unique.iter().any(|seen| item.equals(*seen)) {
            unique.push(item);
        }
    }
    Ok(owned_list(
        unique.into_iter().map(|item| item.clone_as_boxed()).collect(),
    ))
}

fn reverse<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list] = take_args(args)?;
    Ok(owned_list(
        elements(list.as_ref())?
            .into_iter()
            .rev()
            .map(|item| item.clone_as_boxed())
            .collect(),
    ))
}

/// The elements in ascending order: cel-go's `sortList`, which sorts the list
/// by itself as keys.
fn sort<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list] = take_args(args)?;
    let items = elements(list.as_ref())?;
    // cel-go's `sortListByAssociatedKeys` returns an empty list itself, so an
    // empty set stays a set. (`sortBy` binds its target first and comes out
    // a plain list on the apiserver, as here.)
    if items.is_empty() {
        return Ok(list);
    }
    let order = sorted_indices(&items, "sort")?;
    Ok(owned_list(
        order.into_iter().map(|i| items[i].clone_as_boxed()).collect(),
    ))
}

/// The elements of `list` in the ascending order of `keys`, the key of each
/// element at its index: the expansion of `sortBy`.
fn sort_by_keys<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [list, keys] = take_args(args)?;
    let items = elements(list.as_ref())?;
    let keys = elements(keys.as_ref())?;
    if keys.len() != items.len() {
        return Err(ExecutionError::function_error(
            "sortBy",
            format!(
                "{SORT_BY_KEYS}() expected a list of the same size as the associated keys list, but got {} \
                 and {} elements respectively",
                items.len(),
                keys.len()
            ),
        ));
    }
    let order = sorted_indices(&keys, "sortBy")?;
    Ok(owned_list(
        order.into_iter().map(|i| items[i].clone_as_boxed()).collect(),
    ))
}

/// cel-go's `sortListByAssociatedKeys`: the indices of `keys`, reordered by Go's
/// `sort.Slice` with "key i < key j" as `less`. The first key must be
/// comparable. A key of another type than the first is an error, found when the
/// sort compares it; any other comparison that is not "less" (a NaN's) is
/// false, so `[double('NaN')].sort()` sorts.
fn sorted_indices(keys: &[&dyn Val], function: &str) -> Result<Vec<usize>, ExecutionError> {
    let mut order: Vec<usize> = (0..keys.len()).collect();
    let Some(first) = keys.first() else {
        return Ok(order);
    };
    if first.as_comparer().is_none() {
        return Err(ExecutionError::function_error(
            function,
            "list elements must be comparable",
        ));
    }
    let elem_type = first.get_type();
    let mut mixed = false;
    go_sort_slice(&mut order, |&i, &j| {
        let (a, b) = (keys[i], keys[j]);
        if a.get_type() != elem_type || b.get_type() != elem_type {
            mixed = true;
            return false;
        }
        a.as_comparer()
            .is_some_and(|c| c.compare(b) == Ok(std::cmp::Ordering::Less))
    });
    if mixed {
        return Err(ExecutionError::function_error(
            function,
            "list elements must have the same type",
        ));
    }
    Ok(order)
}

/// The ints from `0` up to, but excluding, `n`: none for a negative `n`, as in
/// cel-go, and at most [`MAX_RANGE_SIZE`].
fn range<'b, 'v>(args: Vec<CowVal<'b, 'v>>) -> Result<CowVal<'b, 'v>, ExecutionError> {
    let [n] = take_args(args)?;
    let n = int_arg(n.as_ref())?;
    if n > MAX_RANGE_SIZE {
        return Err(ExecutionError::function_error(
            "lists.range",
            format!("size {n} exceeds maximum allowed ({MAX_RANGE_SIZE})"),
        ));
    }
    Ok(owned_list(
        (0..n.max(0))
            .map(|i| Box::new(CelInt::from(i)) as Box<dyn Val>)
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use cel::Value;

    use crate::functions::eval;

    fn holds(expr: &str) {
        assert_eq!(eval(expr), Ok(Value::Bool(true)), "{expr}");
    }

    #[test]
    fn sort_by_orders_ties_as_go() {
        // go1.26.2 sort.Slice, through cel-go v0.26 on the reference apiserver.
        holds("lists.range(13).sortBy(x, x % 2) == [8, 12, 2, 0, 4, 10, 6, 3, 7, 9, 5, 11, 1]");
        holds("lists.range(12).sortBy(x, x % 2) == [0, 2, 4, 6, 8, 10, 1, 3, 5, 7, 9, 11]");
        holds("lists.range(20).sortBy(x, 0) == lists.range(20)");
        // sort() shares the sorter: a -0.0 among equal zeros moves.
        holds(
            "lists.range(20).map(x, x % 2 == 0 ? (x == 0 ? -0.0 : 0.0) : 1.0).sort()\
             .map(x, 1.0 / x < 0.0).indexOf(true) == 5",
        );
    }

    #[test]
    fn sort_by_macro_scopes() {
        holds("[[2, 1], [4, 3]].map(l, l.sortBy(x, x)) == [[1, 2], [3, 4]]");
        holds("[3, 1, 2].map(x, [x]).sortBy(e, e[0]).map(e, e[0]) == [1, 2, 3]");
        holds("[2, 1].sortBy(x, x).sortBy(y, -y) == [2, 1]");
        // The key may read variables of the enclosing scope.
        holds("[1, 2].all(n, [3, 1, 2].sortBy(x, x * n) == [1, 2, 3])");
    }

    #[test]
    fn sort_by_macro_refuses_what_cel_go_refuses() {
        let env = crate::new_env();
        assert!(env.compile("[1, 2].sortBy(1, x)").is_err());
        assert!(env.compile("{'a': 1}.sortBy(x, x)").is_err());
        assert!(env.compile("1.sortBy(x, x)").is_err());
    }

    #[test]
    fn sort_errors_as_cel_go() {
        // A key of another type, found when compared; a first key that cannot
        // be compared. One element is never compared.
        assert!(eval("[1, 'a'].sort()").is_err());
        assert!(eval("[1, 1u].sort()").is_err());
        assert!(eval("[[1], [2]].sort()").is_err());
        assert!(eval("[[1]].sort()").is_err());
        assert!(eval("lists.range(3).sortBy(x, [1, 'a', 2][x])").is_err());
        holds("[1].sort() == [1] && [].sort() == []");
    }

    #[test]
    fn a_nan_sorts() {
        holds("[double('NaN')].sort().size() == 1");
        holds("[1.0, double('NaN'), 2.0].sort().map(x, x != x ? -1.0 : x) == [1.0, -1.0, 2.0]");
    }

    #[test]
    fn range_is_empty_below_zero_and_capped() {
        holds("lists.range(-1) == [] && lists.range(-9223372036854775808) == [] && lists.range(0) == []");
        holds("size(lists.range(1000000)) == 1000000");
        assert!(eval("lists.range(1000001)").is_err());
    }

    #[test]
    fn cel_lists_extension_does_not_stack_on_new_env() {
        // kube-cel declares the same functions itself.
        let mut env = crate::new_env();
        assert!(env.add_extension(cel::extensions::lists).is_err());
    }
}
