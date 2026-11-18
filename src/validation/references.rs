//! Compile-time check of the names an expression references.
//!
//! `cel` has no type checker yet (cel-rust#350): [`cel::Env::compile`] only
//! parses, and a reference to a function or variable the environment lacks
//! fails only when it is evaluated. A reference evaluation never reaches
//! (`true || isIPv4(s)`, `self.x > 0 || slef.y == 1`, the body of a
//! comprehension over an empty list, a `messageExpression` of a rule that
//! holds) would then pass, where the apiserver, which checks every reference
//! when the CRD or policy is registered, rejects it. This walks the whole AST
//! and refuses a call to any function the apiserver does not declare, and any
//! identifier that is not declared where it appears, so such an expression
//! fails to compile client-side too.
//!
//! The function names accepted are the ones the apiserver declares
//! (`apiserver_functions.txt`, from the `k8s.io/apiserver` base environment,
//! checked against a live apiserver by `tests/apiserver_parity.rs`), not the
//! ones kube-cel implements. CRD rules, their `messageExpression`s and policy
//! expressions share the list: the apiserver declares the same functions for
//! all three, the `authorizer` library included (what a policy adds is the
//! `authorizer` variable). A function the apiserver declares but kube-cel does
//! not implement (`optional.unwrap`, `<list>.unwrapOpt()`, the `authorizer`
//! library, a function whose kube-cel feature is disabled) therefore compiles,
//! as the apiserver accepts it, and fails closed with `UnsupportedReference`
//! only if evaluation reaches the call.
//!
//! An identifier is declared when an enclosing comprehension binds it (the
//! variables of `all`, `exists`, `map`, `sortBy`, `optMap` and the other
//! macros, inside the macro only), when the [`Scope`] declares it as a
//! variable (`self` and `oldSelf` in a CRD rule, `object`, `request`, ... in a
//! policy), or when it names a type ([`TYPE_IDENTIFIERS`]). A qualified name
//! (`a.b.c`) resolves as cel-go resolves it: as a declared name itself
//! (`authorizer.requestResource`, `net.IP`) if it is one, else as field
//! selections on its leading part, which must then be declared. A function
//! namespace is no identifier: `format.dns1123Label()` calls the function
//! `format.dns1123Label`. A message construction `T{..}` must name a message
//! type the context declares. Every list here was measured with
//! `k8s.io/apiserver` v0.36.1.
//!
//! It checks names only. Type, arity and call-shape errors (`[1, 2.0].isSorted()`,
//! `'a'.lowerAscii(42)`, `family(ip(..))`), and a field a type does not have
//! (`variables.undeclared` in a policy), still need a type checker: see the
//! `docs/apiserver-divergence.md`. The one call shape it does refuse is a
//! global `min(..)`/`max(..)`: the apiserver only has `<list>.min()`/
//! `<list>.max()`, while `cel` evaluates any member function called as a
//! global, so `min([1, 2])` would run.

use std::{collections::HashSet, sync::LazyLock};

use cel::common::ast::{EntryExpr, Expr, IdedExpr};

/// Functions the apiserver has as `<list>.f()` but kube-cel also takes as
/// `f(..)`.
const MEMBER_ONLY: &[&str] = &["min", "max"];

/// The type names an expression can reference as values in every context
/// (`type(x) == int`), and the one enum constant, as the apiserver's checker
/// resolves them and its planner then accepts them. Not `dyn` (a function, no
/// identifier), nor `duration`, `timestamp`, `google.protobuf.Any` or the
/// wrapper types, which the checker resolves but the planner refuses
/// ("reference to undefined type"), nor `kubernetes.Quantity` and kube-cel's
/// other object types, which neither declares. Measured name by name against
/// both contexts.
const TYPE_IDENTIFIERS: &[&str] = &[
    "bool",
    "bytes",
    "double",
    "int",
    "uint",
    "string",
    "list",
    "map",
    "null_type",
    "type",
    "optional_type",
    "google.protobuf.Duration",
    "google.protobuf.Empty",
    "google.protobuf.Timestamp",
    "google.protobuf.Struct.FieldsEntry",
    "google.protobuf.NullValue.NULL_VALUE",
    "net.IP",
    "net.CIDR",
];

/// The message types `T{..}` can construct in every context: the protobuf
/// well-known types the apiserver's type provider knows, but for
/// `google.protobuf.FieldMask` (and `NullValue`, an enum), `Struct`'s map
/// entry type, and `duration`, `timestamp` and `null_type`, which the
/// checker takes as constructors though not as values (`int{}` it refuses).
/// Measured name by name against both contexts.
const MESSAGE_TYPES: &[&str] = &[
    "google.protobuf.Any",
    "google.protobuf.BoolValue",
    "google.protobuf.BytesValue",
    "google.protobuf.DoubleValue",
    "google.protobuf.Duration",
    "google.protobuf.Empty",
    "google.protobuf.FloatValue",
    "google.protobuf.Int32Value",
    "google.protobuf.Int64Value",
    "google.protobuf.ListValue",
    "google.protobuf.StringValue",
    "google.protobuf.Struct",
    "google.protobuf.Struct.FieldsEntry",
    "google.protobuf.Timestamp",
    "google.protobuf.UInt32Value",
    "google.protobuf.UInt64Value",
    "google.protobuf.Value",
    "duration",
    "timestamp",
    "null_type",
];

/// The accumulator name cel-go's standard macros check iteration variables
/// against (`parser.AccumulatorName`): `all`, `exists`, `exists_one`, `map`,
/// `filter`, the two-variable forms and `sortBy`'s key list refuse it as a
/// variable ("iteration variable overwrites accumulator variable"), although
/// their accumulator is now the unspellable `@result`. `optMap` binds its
/// variable otherwise and takes it.
const LEGACY_ACCUMULATOR: &str = "__result__";

/// The accumulator `cel`'s macros use, by which a comprehension tells itself
/// apart as one of the standard macros' (or a two-variable form's).
const ACCUMULATOR: &str = "@result";

/// The context an expression is compiled in, which decides the variables it
/// can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// A CRD `x-kubernetes-validations` rule or its `messageExpression`
    /// (`apiextensions-apiserver` `schema/cel`, `prepareEnvSet`).
    Crd,
    /// A ValidatingAdmissionPolicy expression or its `messageExpression`
    /// (`k8s.io/apiserver` `admission/plugin/cel`, `createEnvForOpts`), with
    /// the apiserver's optional declarations: `params` only for a policy with
    /// a `paramKind`, `authorizer` for a validation but not its
    /// `messageExpression`, `variables` only for a policy with
    /// `spec.variables` (which the validation engine does not model, so it
    /// never declares them).
    Vap {
        params: bool,
        authorizer: bool,
        variables: bool,
    },
}

impl Scope {
    /// Whether the apiserver declares the variable `name` in this context.
    fn declares_variable(self, name: &str) -> bool {
        match self {
            Scope::Crd => matches!(name, "self" | "oldSelf"),
            Scope::Vap {
                params,
                authorizer,
                variables,
            } => match name {
                "object" | "oldObject" | "request" | "namespaceObject" => true,
                "params" => params,
                "authorizer" | "authorizer.requestResource" => authorizer,
                "variables" => variables,
                _ => false,
            },
        }
    }

    /// The message types this context declares beyond [`MESSAGE_TYPES`]: the
    /// types of a policy's `request` and `namespaceObject` (`variables`' type,
    /// `kubernetes.variables`, comes with `spec.variables`). A CRD rule's own
    /// are `self`'s, under names generated per CRD.
    /// Whether `T{..}` can construct `name` in this context.
    fn declares_message_type(self, name: &str) -> bool {
        MESSAGE_TYPES.contains(&name)
            || self.message_types().contains(&name)
            || matches!(self, Scope::Vap { variables: true, .. }) && name == "kubernetes.variables"
    }

    fn message_types(self) -> &'static [&'static str] {
        match self {
            Scope::Crd => &[],
            Scope::Vap { .. } => &[
                "kubernetes.AdmissionRequest",
                "kubernetes.GroupVersionKind",
                "kubernetes.GroupVersionResource",
                "kubernetes.UserInfo",
                "kubernetes.Namespace",
                "kubernetes.NamespaceSpec",
                "kubernetes.NamespaceStatus",
                "kubernetes.NamespaceMetadata",
                "kubernetes.NamespaceCondition",
            ],
        }
    }

    /// What an identifier can be in this context, for the error message.
    fn readable(self) -> String {
        match self {
            Scope::Crd => "a CRD rule declares `self` and `oldSelf`".to_owned(),
            Scope::Vap {
                params,
                authorizer,
                variables,
            } => {
                let mut names = vec!["`object`", "`oldObject`", "`request`"];
                if params {
                    names.push("`params`");
                }
                names.push("`namespaceObject`");
                if authorizer {
                    names.push("`authorizer`");
                }
                if variables {
                    names.push("`variables`");
                }
                let last = names.pop().expect("a policy declares variables");
                let kind = if authorizer {
                    "expression"
                } else {
                    "messageExpression"
                };
                let mut readable = format!("a policy {kind} declares {} and {last}", names.join(", "));
                if !params {
                    readable.push_str(", `params` when the policy has a `paramKind`");
                }
                if !variables {
                    readable.push_str(
                        " (`variables` comes with `spec.variables`, which kube-cel does not model)",
                    );
                }
                readable
            }
        }
    }
}

/// The functions the apiserver declares, from `apiserver_functions.txt`.
pub(crate) fn apiserver_functions() -> &'static HashSet<&'static str> {
    static NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
        include_str!("apiserver_functions.txt")
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    });
    &NAMES
}

/// Why [`check`] refused an expression.
#[derive(Debug)]
pub(crate) enum UndeclaredReference {
    /// A call to a function the apiserver does not declare, as written:
    /// `isIPv4`, `math.ceil`.
    Function(String),
    /// A global call to a function the apiserver only has as a member.
    GlobalForm(&'static str),
    /// An identifier not declared where it appears, as written: `slef`, `x`
    /// outside the macro that binds it, `authorizer` in a CRD rule.
    Identifier { name: String, scope: Scope },
    /// A message construction of a type the context does not declare.
    MessageType(String),
    /// `__result__` as the iteration variable of a standard macro.
    AccumulatorOverwrite,
    /// `.x` naming a macro's variable `x`, as written: the apiserver reads the
    /// macro's `x`, `cel` a name outside the macros.
    DottedMacroVariable(String),
}

impl std::fmt::Display for UndeclaredReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndeclaredReference::Function(name) => {
                write!(
                    f,
                    "call to '{name}': no such function in the Kubernetes CEL environment"
                )
            }
            UndeclaredReference::GlobalForm(name) => write!(
                f,
                "no global '{name}' in the Kubernetes CEL environment; call it on a list: <list>.{name}()"
            ),
            UndeclaredReference::Identifier { name, scope } => write!(
                f,
                "undeclared reference to '{name}': {}, besides a macro's variables inside the macro \
                 and type names such as `int`",
                scope.readable()
            ),
            UndeclaredReference::MessageType(name) => {
                write!(
                    f,
                    "undeclared reference to '{name}': no such message type in the Kubernetes CEL environment"
                )
            }
            UndeclaredReference::DottedMacroVariable(name) => write!(
                f,
                "'{name}' names a macro's variable, which kube-cel cannot evaluate as the apiserver does \
                 (it looks a leading-dot name up outside the macros); write it without the dot"
            ),
            UndeclaredReference::AccumulatorOverwrite => write!(
                f,
                "iteration variable overwrites accumulator variable: a macro's variable cannot be named \
                 '{LEGACY_ACCUMULATOR}'"
            ),
        }
    }
}

impl std::error::Error for UndeclaredReference {}

/// Checks every function call and identifier in `expr` against what the
/// apiserver declares in `scope`.
pub(crate) fn check(expr: &IdedExpr, scope: Scope) -> Result<(), UndeclaredReference> {
    Checker {
        declared: apiserver_functions(),
        scope,
        bound: Vec::new(),
        collected: None,
    }
    .walk(expr)
}

/// The identifiers in `expr` not declared where they appear in `scope`, each
/// once, as written: what [`check`] refuses first, collected over the whole
/// expression for static analysis' scope warnings.
pub(crate) fn undeclared_identifiers(expr: &IdedExpr, scope: Scope) -> Vec<String> {
    let mut checker = Checker {
        declared: apiserver_functions(),
        scope,
        bound: Vec::new(),
        collected: Some(Vec::new()),
    };
    checker.walk(expr).expect("a collecting walk refuses nothing");
    checker.collected.unwrap_or_default()
}

struct Checker<'e> {
    declared: &'static HashSet<&'static str>,
    scope: Scope,
    /// The variables the enclosing comprehensions bind, innermost last.
    bound: Vec<&'e str>,
    /// When set, undeclared identifiers are collected here instead of
    /// refused, and other refusals are skipped.
    collected: Option<Vec<String>>,
}

impl<'e> Checker<'e> {
    /// Whether the apiserver declares the function `name`. A leading `.` makes
    /// a name absolute (`.size(..)`, `.optional.of(..)`); `cel` drops it to
    /// resolve.
    fn known(&self, name: &str) -> bool {
        self.declared.contains(without_leading_dot(name))
    }

    /// Whether `name` is a variable of the scope or a type name: what an
    /// identifier or qualified name resolves to outside the comprehensions.
    fn declared_global(&self, name: &str) -> bool {
        let name = without_leading_dot(name);
        self.scope.declares_variable(name) || TYPE_IDENTIFIERS.contains(&name)
    }

    fn ident(&mut self, name: &str) -> Result<(), UndeclaredReference> {
        let bare = without_leading_dot(name);
        // A name starting with `@` is one a macro binds (`@result`); no source
        // can spell it. A variable bound as `.x` is never found.
        if bare.starts_with('@') {
            return Ok(());
        }
        if self.bound.contains(&bare) {
            // cel-go resolves `.x` to the macro's `x`, while `cel` looks it up
            // outside the macros, where it may be another value altogether (a
            // policy's null `params`): refused rather than evaluated wrong.
            return if bare.len() < name.len() {
                self.refuse(UndeclaredReference::DottedMacroVariable(name.to_owned()))
            } else {
                Ok(())
            };
        }
        if self.declared_global(bare) {
            Ok(())
        } else {
            self.refuse(UndeclaredReference::Identifier {
                name: name.to_owned(),
                scope: self.scope,
            })
        }
    }

    /// Refuses the expression, or, when collecting undeclared identifiers,
    /// records one and goes on (anything else is not a scope warning).
    fn refuse(&mut self, reason: UndeclaredReference) -> Result<(), UndeclaredReference> {
        match (&mut self.collected, reason) {
            (Some(names), UndeclaredReference::Identifier { name, .. }) => {
                if !names.contains(&name) {
                    names.push(name);
                }
                Ok(())
            }
            (Some(_), _) => Ok(()),
            (None, reason) => Err(reason),
        }
    }

    fn walk(&mut self, e: &'e IdedExpr) -> Result<(), UndeclaredReference> {
        match &e.expr {
            Expr::Unspecified | Expr::Literal(_) => Ok(()),
            Expr::Ident(name) => self.ident(name),
            Expr::Select(select) => {
                // cel-go resolves `a.b.c` as a declared name first
                // (`authorizer.requestResource`, `net.IP`), before reading it
                // as field selections on `a.b`. `has(a.b)` selects a field.
                if !select.test && qualified_name(e).is_some_and(|q| self.declared_global(&q)) {
                    return Ok(());
                }
                self.walk(&select.operand)
            }
            Expr::List(list) => list.elements.iter().try_for_each(|e| self.walk(e)),
            Expr::Map(map) => map.entries.iter().try_for_each(|e| self.walk_entry(&e.expr)),
            Expr::Struct(s) => {
                if !self
                    .scope
                    .declares_message_type(without_leading_dot(&s.type_name))
                {
                    self.refuse(UndeclaredReference::MessageType(s.type_name.clone()))?;
                }
                s.entries.iter().try_for_each(|e| self.walk_entry(&e.expr))
            }
            // As cel-go scopes it: the range and the accumulator's initial
            // value outside, the accumulator in the loop and the result, the
            // iteration variables in the loop only. Each is bound under the
            // name as written: `[1].all(.x, x > 0)` binds `.x`, which neither
            // `x` nor `.x` resolves to.
            Expr::Comprehension(c) => {
                if c.accu_var == ACCUMULATOR
                    && (c.iter_var == LEGACY_ACCUMULATOR
                        || c.iter_var2.as_deref() == Some(LEGACY_ACCUMULATOR))
                {
                    self.refuse(UndeclaredReference::AccumulatorOverwrite)?;
                }
                self.walk(&c.iter_range)?;
                self.walk(&c.accu_init)?;
                let outer = self.bound.len();
                self.bound.push(&c.accu_var);
                self.bound.push(&c.iter_var);
                if let Some(iter_var2) = &c.iter_var2 {
                    self.bound.push(iter_var2);
                }
                self.walk(&c.loop_cond)?;
                self.walk(&c.loop_step)?;
                self.bound.truncate(outer + 1);
                self.walk(&c.result)?;
                self.bound.truncate(outer);
                Ok(())
            }
            Expr::Call(call) => {
                let name = call.func_name.as_str();
                // Operators (`_+_`, `@in`, ..) and the helpers macros expand to
                // (`cel.@mapInsert`, ..) are spelled so no source can call them.
                if is_identifier_path(name) {
                    match &call.target {
                        None => {
                            let bare = without_leading_dot(name);
                            if let Some(member_only) = MEMBER_ONLY.iter().find(|n| **n == bare) {
                                self.refuse(UndeclaredReference::GlobalForm(member_only))?;
                            } else if !self.known(name) {
                                self.refuse(UndeclaredReference::Function(name.to_owned()))?;
                            }
                        }
                        // As `cel` resolves it: `a.b.f(..)` calls the function
                        // `a.b.f` when one is declared, else `f` on `a.b`.
                        Some(target) => {
                            let qualified = qualified_name(target).map(|q| format!("{q}.{name}"));
                            match qualified {
                                Some(qualified) if self.known(&qualified) => {}
                                // Only a declared namespace function leaves
                                // its receiver unread: when collecting, an
                                // unknown function's receiver is still read,
                                // as cel-go reports `missing` in
                                // `[missing].noSuchFunction()` (and `math` in
                                // `math.greatest(..)`).
                                qualified if !self.known(name) => {
                                    self.refuse(UndeclaredReference::Function(
                                        qualified.unwrap_or_else(|| name.to_owned()),
                                    ))?;
                                    self.walk(target)?;
                                }
                                _ => self.walk(target)?,
                            }
                        }
                    }
                } else if let Some(target) = &call.target {
                    self.walk(target)?;
                }
                call.args.iter().try_for_each(|e| self.walk(e))
            }
        }
    }

    fn walk_entry(&mut self, entry: &'e EntryExpr) -> Result<(), UndeclaredReference> {
        match entry {
            EntryExpr::StructField(field) => self.walk(&field.value),
            EntryExpr::MapEntry(entry) => {
                self.walk(&entry.key)?;
                self.walk(&entry.value)
            }
        }
    }
}

/// The name a leading-dot reference (`.x`, `.f(1)`) resolves by.
fn without_leading_dot(name: &str) -> &str {
    name.strip_prefix('.').unwrap_or(name)
}

/// Whether `name` could be written in CEL source as a (qualified) function name.
fn is_identifier_path(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// The qualified name `expr` spells (`a.b` for `a.b`), if it is an identifier
/// or field selections on one.
fn qualified_name(expr: &IdedExpr) -> Option<String> {
    match &expr.expr {
        Expr::Ident(name) => Some(name.clone()),
        Expr::Select(select) if !select.test => {
            qualified_name(&select.operand).map(|q| format!("{q}.{}", select.field))
        }
        _ => None,
    }
}

/// Whether `expr` references an identifier `name` anywhere, a comprehension
/// variable of that name included: the apiserver's `UsesOldSelf` scans the
/// checker's reference map, which records every resolved identifier, so
/// `[1].all(oldSelf, oldSelf > 0)` "uses oldSelf" there.
pub(crate) fn references_ident(expr: &IdedExpr, name: &str) -> bool {
    match &expr.expr {
        Expr::Ident(ident) => ident.strip_prefix('.').unwrap_or(ident) == name,
        Expr::Unspecified | Expr::Literal(_) => false,
        Expr::Select(select) => references_ident(&select.operand, name),
        Expr::List(list) => list.elements.iter().any(|e| references_ident(e, name)),
        Expr::Map(map) => map.entries.iter().any(|e| entry_references_ident(&e.expr, name)),
        Expr::Struct(s) => s.entries.iter().any(|e| entry_references_ident(&e.expr, name)),
        Expr::Comprehension(c) => [&c.iter_range, &c.accu_init, &c.loop_cond, &c.loop_step, &c.result]
            .into_iter()
            .any(|e| references_ident(e, name)),
        Expr::Call(call) => {
            call.target.as_deref().is_some_and(|t| references_ident(t, name))
                || call.args.iter().any(|e| references_ident(e, name))
        }
    }
}

fn entry_references_ident(entry: &EntryExpr, name: &str) -> bool {
    match entry {
        EntryExpr::StructField(field) => references_ident(&field.value, name),
        EntryExpr::MapEntry(entry) => {
            references_ident(&entry.key, name) || references_ident(&entry.value, name)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use super::{MESSAGE_TYPES, Scope, TYPE_IDENTIFIERS, UndeclaredReference, apiserver_functions, check};
    use crate::{
        functions::register_all,
        validation::{compile, validation_env},
    };

    /// Functions the apiserver declares that kube-cel does not implement even
    /// with every feature on. A call to one compiles and fails closed with
    /// `UnsupportedReference` if evaluation reaches it.
    const DECLARED_ONLY: &[&str] = &[
        // cel-go's optional library, version 1.
        "optional.unwrap",
        "unwrapOpt",
        // The authorizer library (`authorizer` and `authorizer.requestResource`
        // are policy variables kube-cel does not bind).
        "allowed",
        "check",
        "error",
        "errored",
        "fieldSelector",
        "group",
        "labelSelector",
        "name",
        "namespace",
        "path",
        "reason",
        "resource",
        "serviceAccount",
        "subresource",
    ];

    /// Whether kube-cel can call `name` in a rule: `cel` declares it on the
    /// validation env, or kube-cel registers it on a context.
    fn implemented(name: &str, registered: &BTreeSet<String>) -> bool {
        validation_env().function(name).is_some() || registered.contains(name)
    }

    fn registered() -> BTreeSet<String> {
        let mut ctx = cel::Context::with_env(Arc::clone(validation_env()));
        register_all(&mut ctx)
            .expect("kube-cel's functions register onto its Env")
            .into_iter()
            .collect()
    }

    /// Every function kube-cel registers is one the apiserver declares:
    /// kube-cel ships no function Kubernetes lacks, and the name check would
    /// refuse a call to one in a rule. General use and validation share the
    /// set, so this covers both.
    #[test]
    fn registered_functions_are_declared_by_the_apiserver() {
        let stray: Vec<String> = registered()
            .into_iter()
            .filter(|name| !apiserver_functions().contains(name.as_str()))
            .collect();
        assert!(
            stray.is_empty(),
            "kube-cel registers functions the apiserver does not declare: {stray:?}"
        );
    }

    /// Declared = implemented ∪ declared-only, with the two disjoint. Fails
    /// when an updated `apiserver_functions.txt` adds a name kube-cel does not
    /// implement, or when kube-cel starts implementing one listed in
    /// [`DECLARED_ONLY`].
    #[test]
    #[cfg(all(
        feature = "strings",
        feature = "lists",
        feature = "sets",
        feature = "regex_funcs",
        feature = "urls",
        feature = "ip",
        feature = "semver_funcs",
        feature = "format",
        feature = "quantity",
        feature = "named_format"
    ))]
    fn declared_functions_are_implemented_or_declared_only() {
        let registered = registered();
        let missing: BTreeSet<&str> = apiserver_functions()
            .iter()
            .copied()
            .filter(|name| !implemented(name, &registered))
            .collect();
        let listed: BTreeSet<&str> = DECLARED_ONLY.iter().copied().collect();
        assert_eq!(
            missing, listed,
            "declared by the apiserver but not implemented, vs DECLARED_ONLY"
        );
    }

    /// Calls to functions the apiserver declares but kube-cel does not
    /// implement compile. Each was compiled and evaluated with the Go oracle
    /// (`k8s.io/apiserver` v0.36.1); the `authorizer` rows in the policy
    /// compiler, the others in the CRD rule compiler.
    #[test]
    fn accepts_declared_functions_kube_cel_does_not_implement() {
        assert_accepted(&[
            "self == '' || optional.unwrap([optional.of(self), optional.none()]) == [self]",
            "true || [optional.of(1), optional.none()].unwrapOpt() == [1]",
            ".optional.unwrap([optional.of(1)]) == [1]",
        ]);
        assert_accepted_in(
            Scope::Vap {
                params: false,
                authorizer: true,
                variables: false,
            },
            &[
                "true || authorizer.group('').resource('pods').check('get').allowed()",
                "authorizer.serviceAccount('default', 'sa').group('').resource('pods').subresource('log').namespace('default').name('p').check('get').allowed()",
                "authorizer.group('').resource('pods').fieldSelector('metadata.name=p').labelSelector('a=b').check('list').allowed()",
                "authorizer.path('/healthz').check('get').allowed() && authorizer.requestResource.check('get').allowed()",
                "!authorizer.group('').resource('pods').check('get').errored() && authorizer.group('').resource('pods').check('get').error() == ''",
                "authorizer.group('').resource('pods').check('get').reason() == ''",
            ],
        );
    }

    /// The check must refuse none of `exprs`, all of which the apiserver's base
    /// environment accepts: each was evaluated to `true` with the Go oracle
    /// (`k8s.io/apiserver` v0.36.1, `MustBaseEnvSet(1.36).NewExpressionsEnv()`).
    fn assert_accepted(exprs: &[&str]) {
        assert_accepted_in(Scope::Crd, exprs);
    }

    /// The check must refuse none of `exprs`, all of which the apiserver
    /// accepts in `scope`: each was compiled with the Go oracle (`k8s.io/apiserver`
    /// v0.36.1), a CRD rule through `ValidateCustomResourceDefinition`, a
    /// policy expression through the admission compiler with params, the
    /// authorizer and a variable `foo`.
    fn assert_accepted_in(scope: Scope, exprs: &[&str]) {
        for expr in exprs {
            if let Err(err) = compile(expr, scope) {
                panic!("{expr}: refused: {err}");
            }
        }
    }

    /// The check must refuse every one of `exprs` in `scope`, with an
    /// undeclared reference, as the apiserver does (measured as for
    /// [`assert_accepted_in`]).
    fn assert_refused_in(scope: Scope, exprs: &[&str]) {
        for expr in exprs {
            let program = validation_env()
                .compile(expr)
                .unwrap_or_else(|e| panic!("{expr}: {e}"));
            match check(program.expression(), scope) {
                Err(
                    UndeclaredReference::Identifier { .. }
                    | UndeclaredReference::MessageType(_)
                    | UndeclaredReference::AccumulatorOverwrite
                    | UndeclaredReference::DottedMacroVariable(_),
                ) => {}
                other => panic!("{expr}: expected an undeclared identifier or type, got {other:?}"),
            }
        }
    }

    /// Variables a macro binds, used where they are in scope.
    #[test]
    fn accepts_macro_variables_in_scope() {
        assert_accepted(&[
            "self.items.all(x, x != '')",
            "self.items.all(x, self.items.exists(y, x == y))",
            "self.items.exists_one(x, x == 'a') || true",
            "self.items.map(x, x.size()).size() >= 0 && self.items.filter(x, x != '').size() >= 0",
            "self.items.map(x, x != '', x).size() >= 0",
            "self.items.all(i, v, i >= 0 && v != '') && self.items.exists(i, v, i == 0)",
            "self.items.existsOne(i, v, i == 0) || true",
            "self.m.transformMap(k, v, v + k).size() >= 0 && self.items.transformList(i, v, i > 0, v).size() >= 0",
            "self.items.transformMapEntry(i, v, {v: i}).size() >= 0",
            "optional.of(1).optMap(x, x + 1).value() == 2",
            "optional.of(1).optFlatMap(x, optional.of(x + 1)).value() == 2",
            "self.?name.optMap(n, n.size()).orValue(0) >= 0",
            "true || optional.of(1).optMap(x, [x].all(y, y == x)).value()",
            // Nested and shadowing, and the same name bound twice in a row.
            "[1].all(x, [1].all(y, x == y)) && [1].all(y, true)",
            "true || [1].all(x, [2].all(x, x > 0))",
            "true || [[1]].all(x, x.exists(x, x == 1))",
            "true || [1].map(x, x).map(x, x).size() == 1",
            "true || [1].all(x, x == 1) == [2].exists(x, x == 2)",
            // A macro variable may shadow a variable or a type name.
            "[1].all(self, self > 0) && [1].all(int, int > 0)",
            // Outside any macro, `.self` is `self`.
            ".self.replicas >= 0 && (true || self.replicas == .oldSelf.replicas)",
        ]);
    }

    #[test]
    #[cfg(feature = "lists")]
    fn accepts_sort_by_variables_in_scope() {
        assert_accepted(&[
            "[3, 1, 2].sortBy(x, x) == [1, 2, 3]",
            "true || [[1]].map(l, l.sortBy(x, -x)).size() == 1",
            "true || [1].sortBy(x, x).all(y, y > 0)",
            "true || [1, 2].sortBy(e, [e].map(y, y)[0]).size() == 2",
        ]);
    }

    /// Type names, as values and as message types, in both contexts.
    #[test]
    fn accepts_type_identifiers() {
        for scope in [Scope::Crd, Scope::Vap {
            params: false,
            authorizer: true,
            variables: false,
        }] {
            assert_accepted_in(scope, &[
                "type(1) == int && type('a') == string && type(true) == bool && type(1u) == uint && type(1.0) == double",
                "type(b'a') == bytes && type([1]) == list && type({1: 1}) == map && type(null) == null_type && type(int) == type",
                "type(optional.of(1)) == optional_type",
                "type(duration('1s')) == google.protobuf.Duration && type(timestamp('2024-01-01T00:00:00Z')) == google.protobuf.Timestamp",
                "google.protobuf.NullValue.NULL_VALUE == 0",
                "true || [google.protobuf.Empty].size() == 1",
                "true || [.int].size() == 1 && [.google.protobuf.Timestamp].size() == 1",
                "true || [net.IP].size() == 1 && [net.CIDR].size() == 1",
                "true || google.protobuf.Duration{seconds: 1} == duration('1s')",
                "true || [.google.protobuf.Duration{}].size() == 1",
            ]);
            for message in MESSAGE_TYPES {
                assert_accepted_in(scope, &[&format!("true || [{message}{{}}].size() == 1")]);
            }
        }
        let vap = Scope::Vap {
            params: false,
            authorizer: true,
            variables: false,
        };
        for message in vap.message_types() {
            assert_accepted_in(vap, &[&format!("true || [{message}{{}}].size() == 1")]);
        }
    }

    #[test]
    fn accepts_policy_variables() {
        assert_accepted_in(
            Scope::Vap {
                params: true,
                authorizer: true,
                variables: false,
            },
            &[
                "object.x == 1 || oldObject == null || request.name == '' || params == null || namespaceObject == null",
                "true || authorizer.requestResource.check('get').allowed()",
                "[1].all(variables, variables > 0)",
                "true || request.kind.group == ''",
                "true || [object].all(o, o != null) && [1].all(object, object > 0)",
            ],
        );
    }

    #[test]
    fn refuses_undeclared_identifiers_in_a_crd_rule() {
        assert_refused_in(Scope::Crd, &[
            "self.replicas > 0 || slef.x == 1",
            "true || undeclared == 1",
            "true || authorizer.group('').resource('pods').check('get').allowed()",
            "true || object.x == 1",
            "true || has(slef.name)",
            "true || slef.?name.orValue('') == ''",
            "true || {slef: 1}.size() == 1",
            "true || [slef].size() == 1",
            "true || slef[0] == 1",
            "true || -slef == 1",
            "true || (slef ? 1 : 2) == 1",
            "true || [slef].all(x, true)",
            "true || [1].map(x, slef).size() == 1",
            "true || google.protobuf.Duration{seconds: slef} == duration('1s')",
            // `@result` is the accumulator; `__result__` is no name at all.
            "true || __result__ == 1",
            "self.items.all(x, __result__)",
            // A macro variable outside its macro.
            "self.items.all(x, x != '') && x == ''",
            "self.items.map(x, x).all(y, x != '')",
            "self.items.all(i, v, i >= 0) && v == ''",
            "true || [1].transformList(i, v, i).size() == 1 && i == 0",
            "[1].exists(x, x == 1) ? true : x == 1",
            "optional.of(1).optMap(x, x + 1).value() == 2 && (true || x == 1)",
            "true || optional.of(slef).optMap(x, x).hasValue()",
            // Names that are not identifiers: `dyn` is a function, `duration`
            // and `google.protobuf.Any` are refused by the planner, a
            // namespace or a kube-cel object type is not declared.
            "true || [dyn].size() == 1",
            "true || [duration].size() == 1",
            "true || [timestamp].size() == 1",
            "true || [google.protobuf.Any].size() == 1",
            "true || [google.protobuf.FieldMask].size() == 1",
            "true || [kubernetes.URL].size() == 1",
            "true || [net].size() == 1",
            "true || [net.Foo].size() == 1",
            "true || [optional].size() == 1",
            // Message types the context does not declare.
            "true || Foo{a: 1} == 1",
            "true || [google.protobuf.FieldMask{}].size() == 1",
            "true || [net.IP{}].size() == 1",
            "true || [int{}].size() == 1",
            "true || [kubernetes.AdmissionRequest{}].size() == 1",
        ]);
    }

    #[test]
    #[cfg(feature = "lists")]
    fn refuses_sort_by_variables_out_of_scope() {
        assert_refused_in(Scope::Crd, &[
            "[3, 1, 2].sortBy(x, -x)[0] == 3 && true || x == 1",
            "true || [1].sortBy(x, slef).size() == 1",
            "true || [slef].sortBy(x, x).size() == 1",
        ]);
    }

    #[test]
    fn refuses_undeclared_identifiers_in_a_policy() {
        assert_refused_in(
            Scope::Vap {
                params: true,
                authorizer: true,
                variables: false,
            },
            &[
                "true || self.x == 1",
                "true || oldSelf.x == 1",
                "true || slef.x == 1",
                "true || [kubernetes.Quantity{}].size() == 1",
                "true || [kubernetes.authorization.Authorizer{}].size() == 1",
                // No `spec.variables` (checked on v1.36.1: "undeclared
                // reference to 'variables'").
                "true || variables.foo == 1",
                "true || variables == null",
                "true || [kubernetes.variables{}].size() == 1",
            ],
        );
    }

    /// Every type name and message type, as a value and as a construction,
    /// in both contexts (`google.protobuf.Struct.FieldsEntry`, `duration{}`
    /// included), and `duration` as a value refused.
    #[test]
    fn accepts_every_declared_type_name() {
        for scope in [Scope::Crd, Scope::Vap {
            params: false,
            authorizer: true,
            variables: false,
        }] {
            for name in TYPE_IDENTIFIERS {
                assert_accepted_in(scope, &[&format!("true || [{name}].size() == 1")]);
            }
            for name in MESSAGE_TYPES {
                assert_accepted_in(scope, &[&format!("true || [{name}{{}}].size() == 1")]);
            }
            assert_refused_in(scope, &[
                "true || [duration].size() == 1",
                "true || [int{}].size() == 1",
            ]);
        }
    }

    /// A policy's messageExpression has no `authorizer` (the apiserver
    /// declares it for the validation only); a macro may still bind the name.
    #[test]
    fn policy_message_expression_has_no_authorizer() {
        let message = Scope::Vap {
            params: true,
            authorizer: false,
            variables: false,
        };
        assert_refused_in(message, &[
            "true ? 'ok' : (authorizer.group('').resource('pods').check('get').allowed() ? 'yes' : 'no')",
            "true ? 'ok' : (authorizer.requestResource.check('get').allowed() ? 'yes' : 'no')",
        ]);
        assert_accepted_in(message, &[
            "[1].all(authorizer, authorizer > 0) ? 'yes' : 'no'",
            "namespaceObject == null || params == null ? 'empty' : object.metadata.name",
        ]);
    }

    /// A variable bound as `.x` is bound under that name, which no reference
    /// resolves to.
    #[test]
    fn dotted_binders_bind_no_plain_name() {
        assert_refused_in(Scope::Crd, &[
            "true || [1].all(.x, x > 0)",
            "true || [1].all(.x, .x > 0)",
            "true || [1].map(.x, x).size() == 1",
            "true || optional.of(1).optMap(.x, x + 1).value() == 2",
            "true || optional.of(1).optMap(.x, .x + 1).value() == 2",
            "true || [1].all(.i, v, i == 0)",
            "true || [1].all(i, .v, v > 0)",
        ]);
        assert_accepted(&["[1].all(.x, true)"]);
    }

    /// `.x` naming a macro's variable `x` is refused: the apiserver reads the
    /// macro's `x` (`[dyn(1)].all(namespaceObject, .namespaceObject == null)`
    /// is false on v1.36.1), `cel` a name outside the macros, such as a
    /// policy's null `namespaceObject`.
    #[test]
    fn dotted_reference_to_a_macro_variable_is_refused() {
        assert_refused_in(Scope::Crd, &[
            "[1].all(x, .x > 0)",
            "self.items.all(x, .x != '')",
            "true || [1].all(self, .self > 0)",
            "true || [1].all(x, [2].all(y, .x == y))",
            "true || optional.of(1).optMap(v, .v + 1).value() == 2",
        ]);
        let policy = Scope::Vap {
            params: true,
            authorizer: true,
            variables: false,
        };
        assert_refused_in(policy, &[
            "[dyn(1)].all(namespaceObject, .namespaceObject == null)",
            "[dyn(1)].all(params, .params == null)",
        ]);
        // A leading dot outside the macro binding the name is the global one.
        assert_accepted_in(policy, &[
            "[1].all(x, x > 0) && .namespaceObject == null",
            "[1].all(params, params > 0) && .params == null",
        ]);
    }

    /// cel-go's standard macros refuse `__result__` as their variable;
    /// `optMap` and `optFlatMap` take it.
    #[test]
    fn standard_macros_refuse_the_legacy_accumulator() {
        assert_refused_in(Scope::Crd, &[
            "[1].all(__result__, true)",
            "true || [1].exists(__result__, true)",
            "true || [1].exists_one(__result__, true)",
            "true || [1].map(__result__, __result__).size() == 1",
            "true || [1].map(__result__, true, 1).size() == 1",
            "true || [1].filter(__result__, true).size() == 1",
        ]);
        assert_accepted(&[
            "optional.of(1).optMap(__result__, __result__ + 1).value() == 2",
            "true || optional.of(1).optFlatMap(__result__, optional.of(__result__)).hasValue()",
        ]);
    }

    #[test]
    #[cfg(feature = "lists")]
    fn sort_by_refuses_dotted_and_legacy_variables() {
        assert_refused_in(Scope::Crd, &[
            "true || [1].sortBy(.x, x).size() == 1",
            "[2, 1].sortBy(__result__, __result__)[0] == 1",
        ]);
    }

    /// `params` exists only in a policy with a `paramKind`: the apiserver
    /// refuses `true || params.x == 1` in one without (checked on v1.36.1),
    /// and a macro may still bind the name.
    #[test]
    fn params_needs_a_param_kind() {
        let expr = "true || params.x == 1";
        assert_accepted_in(
            Scope::Vap {
                params: true,
                authorizer: true,
                variables: false,
            },
            &[expr],
        );
        assert_refused_in(
            Scope::Vap {
                params: false,
                authorizer: true,
                variables: false,
            },
            &[expr, "params == null"],
        );
        assert_accepted_in(
            Scope::Vap {
                params: false,
                authorizer: true,
                variables: false,
            },
            &["[1].all(params, params > 0)"],
        );
    }

    #[test]
    fn accepts_the_standard_environment() {
        assert_accepted(&[
            "size('abc') == 3 && 'abc'.size() == 3",
            "int('1') + int(uint(2)) == 3 && double(1) == 1.0 && uint(1) == 1u",
            "string(1) == '1' && bytes('a') == b'a' && bool('true')",
            "type(1) == int && dyn(1) == 1",
            "'abc'.contains('b') && 'abc'.startsWith('a') && 'abc'.endsWith('c') && 'abc'.matches('^a')",
            "duration('1h') > duration('1m') && timestamp('2024-01-01T00:00:00Z').getFullYear() == 2024",
            "timestamp('2024-01-01T00:00:00Z').getHours() == 0 && duration('1h').getMinutes() == 60",
            "has({'a': 1}.a) && [1, 2].all(x, x > 0) && [1, 2].exists(x, x == 2) && [1, 2].exists_one(x, x == 2)",
            "[1, 2].map(x, x * 2) == [2, 4] && [1, 2].filter(x, x > 1) == [2] && [1, 2].map(x, x > 0, x) == [1, 2]",
            "optional.of(1).hasValue() && optional.none().orValue(2) == 2 && optional.of(1).value() == 1",
            "optional.ofNonZeroValue(0).or(optional.of(1)).value() == 1 && optional.of(1).optMap(x, x + 1).value() == 2",
            "{'a': 1}.?a.orValue(0) == 1 && [1, 2][?0].value() == 1",
            "[1, 2].all(i, v, v > i) && [1, 2].exists(i, v, v == 2) && [1, 2].transformList(i, v, v * 2) == [2, 4]",
            "{'a': 1}.transformMap(k, v, v + 1) == {'a': 2} && [1, 2].transformMapEntry(i, v, {string(i): v}) == {'0': 1, '1': 2}",
            "[1, 2].existsOne(i, v, v == 2) && [1, 2].transformList(i, v, v > 1, v) == [2]",
            "1 in [1, 2] && 'a' in {'a': 1} && !(1 > 2) && -1 < 0 && 1 % 2 == 1 && 1 / 1 == 1 && (true ? 1 : 2) == 1",
            ".size('a') == 1 && .optional.of(1).value() == 1",
        ]);
    }

    #[test]
    #[cfg(feature = "lists")]
    fn accepts_the_lists_extension() {
        assert_accepted(&[
            "[3, 1, 2].sortBy(x, x) == [1, 2, 3] && lists.range(3) == [0, 1, 2] && [1, 2, 3].slice(0, 2) == [1, 2]",
            "[[1], [2]].flatten() == [1, 2] && [1, 1].distinct() == [1] && [2, 1].sort() == [1, 2] && [1, 2].reverse() == [2, 1]",
            "[1, 2].isSorted() && [1, 2].sum() == 3 && [1, 2].min() == 1 && [1, 2].max() == 2 && [1, 2].indexOf(2) == 1 && [1, 2].lastIndexOf(2) == 1",
            ".lists.range(3) == [0, 1, 2]",
        ]);
    }

    #[test]
    #[cfg(all(
        feature = "strings",
        feature = "format",
        feature = "regex_funcs",
        feature = "urls",
        feature = "ip",
        feature = "quantity",
        feature = "semver_funcs",
        feature = "sets",
        feature = "named_format"
    ))]
    fn accepts_the_kubernetes_libraries() {
        assert_accepted(&[
            "'%s-%d'.format(['a', 1]) == 'a-1' && 'a,b'.split(',') == ['a', 'b'] && ['a', 'b'].join('-') == 'a-b'",
            "'abc'.charAt(1) == 'b' && 'abc'.indexOf('b') == 1 && 'abcb'.lastIndexOf('b') == 3 && 'ABC'.lowerAscii() == 'abc'",
            "'abc'.upperAscii() == 'ABC' && 'aXc'.replace('X', 'b') == 'abc' && 'abc'.substring(1) == 'bc' && ' a '.trim() == 'a'",
            "strings.quote('a') == '\"a\"'",
            "'abc'.find('b') == 'b' && 'abab'.findAll('b') == ['b', 'b'] && 'abab'.findAll('b', 1) == ['b']",
            "url('https://example.com/p?q=1').getHost() == 'example.com' && isURL('https://example.com') && url('https://a.com/b').getEscapedPath() == '/b'",
            "url('https://a.com:8080').getPort() == '8080' && url('https://a.com').getScheme() == 'https' && url('https://a.com/?x=1').getQuery() == {'x': ['1']}",
            "url('https://a.com').getHostname() == 'a.com'",
            "isIP('1.2.3.4') && ip('1.2.3.4').family() == 4 && ip('1.2.3.4').isLoopback() == false && ip.isCanonical('1.2.3.4')",
            "ip('10.0.0.1').isGlobalUnicast() && !ip('1.2.3.4').isUnspecified() && !ip('1.2.3.4').isLinkLocalMulticast() && !ip('1.2.3.4').isLinkLocalUnicast()",
            "isCIDR('10.0.0.0/8') && cidr('10.0.0.0/8').containsIP(ip('10.0.0.1')) && cidr('10.0.0.0/8').containsCIDR(cidr('10.1.0.0/16'))",
            "cidr('10.0.0.0/8').prefixLength() == 8 && cidr('10.0.0.1/8').masked() == cidr('10.0.0.0/8') && cidr('10.0.0.0/8').ip() == ip('10.0.0.0')",
            "string(ip('1.2.3.4')) == '1.2.3.4' && string(cidr('10.0.0.0/8')) == '10.0.0.0/8'",
            "isQuantity('1Gi') && quantity('1Gi').isGreaterThan(quantity('1Mi')) && quantity('1').isLessThan(quantity('2')) && quantity('1').compareTo(quantity('1')) == 0",
            "quantity('1').add(quantity('1')).compareTo(quantity('2')) == 0 && quantity('2').sub(1).compareTo(quantity('1')) == 0 && sign(quantity('1')) == 1 && quantity('1').isInteger()",
            "quantity('1').asInteger() == 1 && quantity('1.5').asApproximateFloat() == 1.5",
            "isSemver('1.0.0') && semver('1.2.3').major() == 1 && semver('1.2.3').minor() == 2 && semver('1.2.3').patch() == 3",
            "semver('1.0.0').isGreaterThan(semver('0.9.0')) && semver('1.0.0').isLessThan(semver('2.0.0')) && semver('1.0.0').compareTo(semver('1.0.0')) == 0",
            "sets.contains([1, 2], [1]) && sets.equivalent([1, 2], [2, 1]) && sets.intersects([1], [1, 2])",
            ".sets.equivalent([1], [1])",
            "!format.dns1123Label().validate('abc').hasValue() && !format.named('dns1123Label').value().validate('abc').hasValue()",
            "!format.dns1123Subdomain().validate('a.b').hasValue() && !format.labelValue().validate('a').hasValue() && !format.qualifiedName().validate('a').hasValue()",
            "!format.uuid().validate('123e4567-e89b-12d3-a456-426614174000').hasValue() && !format.date().validate('2024-01-01').hasValue()",
            "!format.byte().validate('aGk=').hasValue() && !format.uri().validate('https://a.com').hasValue() && !format.datetime().validate('2024-01-01T00:00:00Z').hasValue()",
            "!format.dns1035Label().validate('abc').hasValue() && !format.dns1123LabelPrefix().validate('abc').hasValue() && !format.dns1035LabelPrefix().validate('abc').hasValue() && !format.dns1123SubdomainPrefix().validate('abc').hasValue()",
        ]);
    }
}
