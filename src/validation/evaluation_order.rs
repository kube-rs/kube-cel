//! Compile-time refusal of the quantity calls whose result depends on
//! evaluation order.
//!
//! The apiserver's quantities share and update state: `q.add(x)` and
//! `q.sub(x)` write their result into `q`'s `inf.Dec` (which the result
//! shares), and `q.compareTo(x)`, `q.isGreaterThan(x)` and `q.isLessThan(x)`
//! turn an `int64Amount` receiver into an `inf.Dec` when `x` is one. kube-cel
//! does the same (see `functions::quantity`), so an expression that evaluates
//! its parts in the order cel-go does gets the apiserver's result. `cel` 0.16
//! does not always use that order:
//!
//! - it evaluates a receiver call's arguments before its receiver, and skips
//!   the receiver when an argument fails; cel-go evaluates the receiver first,
//!   and with one argument evaluates both whatever fails;
//! - it skips a two-operand call's second operand when the first fails (`?`
//!   on each); cel-go evaluates both (`evalBinary`, `evalEq`).
//!
//! Pure CEL cannot tell, but a quantity update can: one that runs in cel-go
//! and not in `cel`, or after an observation in one and before it in the
//! other, changes what a later observation sees. So [`check`] refuses, as a
//! compile error, an expression with an update in such a position:
//!
//! - in a part of a receiver call (the receiver or an argument) while another
//!   part could fail or read a quantity;
//! - in the second operand of a two-operand call while the first could fail.
//!
//! A part that can neither fail nor read a quantity is a comprehension
//! variable, a literal, or `quantity(<literal>)` with a literal that parses:
//! next to such parts the order cannot show. `&&`, `||`, `?:`, list and map
//! literals, indexing and comprehensions evaluate in the same order in both and
//! are not positions. Positions evaluation never reaches are refused too: the
//! check reads the expression, not a run.
//!
//! An update skipped because an earlier part failed matters as well, where
//! the failure is kube-cel's alone: `cel` evaluating the alternative of
//! `or` / `orValue` (kube-cel evaluates it lazily, as cel-go does, see
//! `functions::cel_optional`), or an error kube-cel has where the apiserver
//! has a value. kube-cel notes each such error it knows of (a timestamp offset
//! of a day or more, a URL component that is not UTF-8, a declared function it
//! does not implement, a quantity outside its range), and the validation
//! engine refuses a rule that reached one even where CEL absorbed it. The
//! check relies on kube-cel failing exactly where cel-go does otherwise.
//!
//! An update only matters when the updated quantity is reachable from another
//! part of the expression. A quantity is reachable twice only through a
//! variable (a comprehension's, which may hold a quantity a list literal or
//! `dyn` handed on): `quantity(s)` makes a new one, and `add` / `sub` on a new
//! one return a new one (sharing its `inf.Dec` with no one else). So an update
//! whose receiver is `quantity(..)` (or `semver(..)`, which has no state)
//! followed by any `.add(..)` / `.sub(..)` is not one; any other receiver,
//! such as a comprehension variable, `dyn(q)` or `[q][0]`, is. The names are
//! matched without types, so a `compareTo` on a semver in such a position is
//! refused as well.

use std::collections::HashSet;

use cel::common::ast::{EntryExpr, Expr, IdedExpr, LiteralValue, operators};

/// Calls that update their receiver's state in the apiserver.
const UPDATES: &[&str] = &["add", "sub", "compareTo", "isGreaterThan", "isLessThan"];

/// Calls whose result is a new value with no state shared with anything else.
const NEW_VALUES: &[&str] = &["quantity", "semver"];

/// Why [`check`] refused an expression.
#[derive(Debug)]
pub(crate) struct OrderDependentUpdate;

impl std::fmt::Display for OrderDependentUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "kube-cel cannot evaluate this expression in the apiserver's order: an add(), sub(), \
             compareTo(), isGreaterThan() or isLessThan() on a quantity held by a variable is in a \
             call's receiver or arguments next to a part that could fail or read a quantity, or in \
             the second operand of a two-operand call whose first could fail; cel evaluates these in \
             another order than cel-go, and the apiserver's quantities share state",
        )
    }
}

impl std::error::Error for OrderDependentUpdate {}

/// Refuses `expr` if it has an update in a position whose evaluation order
/// differs (see the [module docs](self)).
pub(crate) fn check(expr: &IdedExpr) -> Result<(), OrderDependentUpdate> {
    let mut walker = Walker {
        bound: Vec::new(),
        qualified: qualified_functions(),
    };
    if walker.risky(expr) {
        Err(OrderDependentUpdate)
    } else {
        Ok(())
    }
}

/// The qualified functions the apiserver declares (`optional.of`,
/// `sets.contains`, ..): `optional.of(x)` is a call to `optional.of`, not `of`
/// on the identifier `optional`, and `cel` resolves it so too.
fn qualified_functions() -> HashSet<&'static str> {
    include_str!("apiserver_functions.txt")
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#') && line.contains('.'))
        .collect()
}

struct Walker {
    /// The comprehension variables in scope.
    bound: Vec<String>,
    qualified: HashSet<&'static str>,
}

fn bare(name: &str) -> &str {
    name.strip_prefix('.').unwrap_or(name)
}

impl Walker {
    /// Whether evaluating `e` can neither fail nor read a quantity: a
    /// comprehension variable, a literal, or `quantity(<literal>)` with a
    /// literal that parses. The order of such a part against another cannot
    /// show. The operand of an ordering comparison, which `new_env()` wraps in
    /// `@kube.ordered(..)`, is inert as the operand is: the wrapper fails on
    /// nothing, and the NaN it notes refuses the rule whatever the order.
    fn is_inert(&self, e: &IdedExpr) -> bool {
        match &e.expr {
            Expr::Ident(name) => self.bound.iter().any(|b| b == name),
            Expr::Literal(_) => true,
            Expr::Call(call) if call.target.is_none() && call.func_name == "@kube.ordered" => {
                matches!(call.args.as_slice(), [operand] if self.is_inert(operand))
            }
            Expr::Call(call) => {
                call.target.is_none()
                    && bare(&call.func_name) == "quantity"
                    && matches!(
                        call.args.as_slice(),
                        [IdedExpr { expr: Expr::Literal(LiteralValue::String(s)), .. }]
                            if crate::functions::quantity_parses(s.inner())
                    )
            }
            _ => false,
        }
    }

    /// Whether `target.name(..)` is a call to a qualified function
    /// (`optional.of(x)`): `cel` then never evaluates the target, as cel-go
    /// does not.
    fn is_qualified(&self, target: &IdedExpr, name: &str) -> bool {
        qualified_name(target).is_some_and(|q| self.qualified.contains(format!("{q}.{name}").as_str()))
    }

    /// Whether evaluating `e` makes a new value no other part of the
    /// expression can reach: `quantity(..)`, `semver(..)`, a literal, or
    /// `add` / `sub` on such a value.
    fn is_new(&self, e: &IdedExpr) -> bool {
        match &e.expr {
            Expr::Literal(_) => true,
            Expr::Call(call) => match &call.target {
                None => NEW_VALUES.contains(&bare(&call.func_name)),
                Some(target) => {
                    matches!(call.func_name.as_str(), "add" | "sub")
                        && call.args.len() == 1
                        && self.is_new(target)
                }
            },
            _ => false,
        }
    }

    /// The receiver an update call `call` updates, if it is one: the target of
    /// `q.add(x)`, or the first argument of `add(q, x)` (`cel` runs a member
    /// function called globally).
    fn updated<'e>(&self, call: &'e cel::common::ast::CallExpr) -> Option<&'e IdedExpr> {
        if !UPDATES.contains(&bare(&call.func_name)) {
            return None;
        }
        match &call.target {
            Some(target) if !self.is_qualified(target, &call.func_name) => Some(target),
            Some(_) => None,
            None => call.args.first(),
        }
    }

    /// Whether `e` contains an update of a quantity another part of the
    /// expression may reach. Comprehension scopes do not matter here: any
    /// receiver but a new value counts.
    fn updates(&self, e: &IdedExpr) -> bool {
        if let Expr::Call(call) = &e.expr
            && self.updated(call).is_some_and(|receiver| !self.is_new(receiver))
        {
            return true;
        }
        children(e).into_iter().any(|c| self.updates(c))
    }

    /// Whether `e` has an update in a position whose evaluation order
    /// differs between `cel` and cel-go.
    fn risky(&mut self, e: &IdedExpr) -> bool {
        match &e.expr {
            Expr::Call(call) => {
                let position = match &call.target {
                    Some(target) if !call.args.is_empty() && !self.is_qualified(target, &call.func_name) => {
                        let parts: Vec<&IdedExpr> =
                            std::iter::once(target.as_ref()).chain(&call.args).collect();
                        parts.iter().enumerate().any(|(i, part)| {
                            self.updates(part)
                                && parts
                                    .iter()
                                    .enumerate()
                                    .any(|(j, other)| j != i && !self.is_inert(other))
                        })
                    }
                    Some(_) | None => {
                        call.args.len() == 2
                            && !matches!(
                                call.func_name.as_str(),
                                operators::LOGICAL_AND
                                    | operators::LOGICAL_OR
                                    | operators::INDEX
                                    | operators::OPT_INDEX
                                    | operators::OPT_SELECT
                            )
                            && !self.is_inert(&call.args[0])
                            && self.updates(&call.args[1])
                    }
                };
                position
                    || call.target.as_deref().is_some_and(|t| self.risky(t))
                    || call.args.iter().any(|a| self.risky(a))
            }
            Expr::Comprehension(c) => {
                if self.risky(&c.iter_range) || self.risky(&c.accu_init) {
                    return true;
                }
                let depth = self.bound.len();
                self.bound.push(c.accu_var.clone());
                if self.risky(&c.result) {
                    self.bound.truncate(depth);
                    return true;
                }
                self.bound.push(c.iter_var.clone());
                self.bound.extend(c.iter_var2.clone());
                let risky = self.risky(&c.loop_cond) || self.risky(&c.loop_step);
                self.bound.truncate(depth);
                risky
            }
            _ => children(e).into_iter().any(|c| self.risky(c)),
        }
    }
}

fn children(e: &IdedExpr) -> Vec<&IdedExpr> {
    match &e.expr {
        Expr::Select(select) => vec![&select.operand],
        Expr::Call(call) => call
            .target
            .as_deref()
            .into_iter()
            .chain(call.args.iter())
            .collect(),
        Expr::Comprehension(c) => vec![&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result],
        Expr::List(list) => list.elements.iter().collect(),
        Expr::Map(map) => map.entries.iter().flat_map(|e| entry_children(&e.expr)).collect(),
        Expr::Struct(s) => s.entries.iter().flat_map(|e| entry_children(&e.expr)).collect(),
        Expr::Unspecified | Expr::Ident(_) | Expr::Literal(_) => Vec::new(),
    }
}

fn entry_children(entry: &EntryExpr) -> Vec<&IdedExpr> {
    match entry {
        EntryExpr::StructField(field) => vec![&field.value],
        EntryExpr::MapEntry(entry) => vec![&entry.key, &entry.value],
    }
}

/// The qualified name `expr` spells (`a.b` for `a.b`), if it is an identifier
/// or field selections on one.
fn qualified_name(expr: &IdedExpr) -> Option<String> {
    match &expr.expr {
        Expr::Ident(name) => Some(bare(name).to_owned()),
        Expr::Select(select) if !select.test => {
            qualified_name(&select.operand).map(|q| format!("{q}.{}", select.field))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    fn refused(expr: &str) -> bool {
        let program = crate::validation::validation_env().compile(expr).expect("parses");
        super::check(program.expression()).is_err()
    }

    #[test]
    fn refuses_an_update_of_a_variable_next_to_a_part_that_could_fail() {
        for expr in [
            "[quantity('1')].all(q, sign(q) > q.compareTo(quantity('2')))",
            "[quantity('1')].all(q, quantity('2').add(sign(q)).add(q.add(1)).isInteger())",
            "[quantity('1')].all(q, dyn(q).add(1).compareTo(quantity('1').add(1)) == 0)",
            "[quantity('1')].all(q, quantity('x').add(q.compareTo(quantity('2'))).isInteger())",
            "[quantity('1')].all(q, compareTo(dyn(q), q.add(1)) == sign(q))",
            "[quantity('1')].all(q, sets.contains([sign(q)], [q.compareTo(quantity('2'))]))",
        ] {
            assert!(refused(expr), "{expr}");
        }
    }

    #[test]
    fn accepts_new_values_inert_parts_and_aligned_positions() {
        for expr in [
            "quantity('1').add(quantity('2').compareTo(quantity('3'))).isInteger()",
            "[quantity('1')].all(q, 0 > q.compareTo(quantity('2')))",
            // An ordering comparison's operand is wrapped in `@kube.ordered`.
            "[quantity('1')].all(q, [1].all(i, i > q.compareTo(quantity('2'))))",
            "[quantity('1')].all(q, q.add(1).compareTo(q) == 0)",
            "[quantity('1')].all(q, quantity('1').add(q.compareTo(quantity('2'))).isInteger())",
            "[quantity('1')].all(q, q.isLessThan(quantity('2')) && q.add(1).isInteger())",
            "[quantity('1')].all(q, [sign(q), q.compareTo(quantity('2'))].size() == 2)",
            "[quantity('1')].all(q, sign(q) == 1 ? q.add(1).isInteger() : false)",
            "[quantity('1')].all(q, optional.of(q.add(1)).hasValue())",
            "[quantity('1')].all(q, optional.of(sign(q)).orValue(q.compareTo(quantity('2'))) == 1)",
            "[quantity('1')].map(q, q.add(1)).size() == 1",
            // The call's own update comes after both parts, in either order.
            "[quantity('1')].all(q, [q][0].isLessThan(quantity('1').add(sign(q))))",
        ] {
            assert!(!refused(expr), "{expr}");
        }
        // `sortBy` is a macro of the `lists` feature; it binds its receiver.
        #[cfg(feature = "lists")]
        assert!(!refused(
            "[quantity('2'), quantity('1')].sortBy(q, q.compareTo(quantity('1'))).size() == 2"
        ));
    }
}
