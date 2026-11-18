//! Kubernetes CEL extension-function libraries (Tier 1 — the primitive).
//!
//! Each submodule ports one upstream Kubernetes CEL library and exposes a
//! `register` entry; [`register_all`] aggregates the compiled-in set, and
//! [`declare_env`] declares what lives on the `Env`. This is the faithful-port
//! half of the crate — see the crate-level "Versioning and stability" docs
//! (Tier 1). The public entry points are [`new_env`](crate::new_env) and
//! [`new_context`](crate::new_context); these modules are internal.

#[cfg(feature = "strings")] mod strings;

#[cfg(feature = "lists")] mod lists;

#[cfg(feature = "sets")] mod sets;

#[cfg(feature = "regex_funcs")] mod regex_funcs;

#[cfg(feature = "urls")] mod urls;

#[cfg(feature = "ip")] mod ip;

#[cfg(feature = "semver_funcs")] mod semver_funcs;

#[cfg(feature = "format")] mod format;
#[cfg(any(feature = "format", feature = "urls", feature = "named_format"))]
mod go_isprint;
#[cfg(feature = "lists")] mod go_sort;
#[cfg(feature = "lists")] mod lists_ext;

#[cfg(feature = "quantity")] mod quantity;

#[cfg(any(feature = "urls", feature = "named_format"))] mod go_url;
#[cfg(feature = "named_format")] mod named_format;

#[cfg(any(
    feature = "strings",
    feature = "lists",
    feature = "semver_funcs",
    feature = "quantity",
    feature = "ip"
))]
mod dispatch;
mod value_ops;

/// Adds functions to a root [`cel::Context`], keeping the first failure instead
/// of dropping it.
///
/// `cel` 0.16 made [`cel::Context::add_function`] fallible (it refuses a name the
/// context's `Env` already declares as an overload). Every module registers
/// through this type so a refusal cannot pass silently: the first refused name
/// is reported by [`finish`](Registrar::finish). `cel` registers nothing on a
/// child scope, so callers hand this a root context.
pub(crate) struct Registrar<'c, 'p, 'v> {
    ctx: &'c mut cel::Context<'p, 'v>,
    error: Option<crate::RegisterError>,
    names: Vec<String>,
}

impl<'c, 'p, 'v> Registrar<'c, 'p, 'v> {
    pub(crate) fn new(ctx: &'c mut cel::Context<'p, 'v>) -> Self {
        debug_assert!(
            matches!(ctx, cel::Context::Root { .. }),
            "a child scope registers nothing"
        );
        Self {
            ctx,
            error: None,
            names: Vec::new(),
        }
    }

    /// Adds `f` as `name`, unless an earlier step already failed.
    #[allow(dead_code)] // unused when no function feature is enabled
    pub(crate) fn add<T: 'static, F>(&mut self, name: &str, f: F)
    where
        F: cel::extractors::IntoFunction<T> + 'static + Send + Sync,
    {
        if self.error.is_some() {
            return;
        }
        match self.ctx.add_function(name, f) {
            Ok(()) => self.names.push(name.to_owned()),
            Err(source) => {
                self.error = Some(crate::RegisterError::Rejected {
                    function: name.to_owned(),
                    source: Box::new(source),
                });
            }
        }
    }

    /// The names registered, or the first failure.
    pub(crate) fn finish(self) -> Result<Vec<String>, crate::RegisterError> {
        self.error.map_or(Ok(self.names), Err)
    }
}

/// Declares the part of kube-cel's environment that lives on the [`cel::Env`]
/// rather than on a context, as the apiserver's base environment has it
/// (`k8s.io/apiserver/pkg/cel/environment`, `MustBaseEnvSet(..)`):
///
/// - with the `lists` feature, kube-cel's port of cel-go's `ext.Lists`
///   (`slice`, `flatten`, `distinct`, `reverse`, `sort`, `lists.range` and the
///   `sortBy` macro, see `lists_ext`), which sorts as Go's `sort.Slice` does;
/// - the two-variable comprehension macros (`all(i, v, ..)`, `transformList`,
///   `transformMap`, ..);
/// - with the `ip` feature, `string(<IP>)` and `string(<CIDR>)`: `string` is a
///   standard-library overload, which `cel` refuses as a context function.
pub(crate) fn declare_env(env: &mut cel::Env) -> Result<(), cel::DeclarationError> {
    #[cfg(feature = "lists")]
    lists_ext::declare(env)?;
    env.add_extension(cel::extensions::two_var_comprehensions)?;

    #[cfg(feature = "ip")]
    ip::declare_string_overloads(env)?;

    Ok(())
}

/// Registers all compiled-in Kubernetes CEL extension functions into `ctx`
/// and returns the names it registered: the context half of kube-cel's
/// environment, next to [`declare_env`].
///
/// Internal implementation behind [`new_context`](crate::new_context) and the
/// deprecated `KubeCelExt`.
pub(crate) fn register_all(ctx: &mut cel::Context<'_, '_>) -> Result<Vec<String>, crate::RegisterError> {
    #[allow(unused_mut)] // nothing to register when no function feature is enabled
    let mut r = Registrar::new(ctx);

    #[cfg(feature = "strings")]
    strings::register(&mut r);

    #[cfg(feature = "lists")]
    lists::register(&mut r);

    #[cfg(feature = "sets")]
    sets::register(&mut r);

    #[cfg(feature = "regex_funcs")]
    regex_funcs::register(&mut r);

    #[cfg(feature = "urls")]
    urls::register(&mut r);

    #[cfg(feature = "ip")]
    ip::register(&mut r);

    #[cfg(feature = "semver_funcs")]
    semver_funcs::register(&mut r);

    #[cfg(feature = "format")]
    format::register(&mut r);

    #[cfg(feature = "quantity")]
    quantity::register(&mut r);

    #[cfg(feature = "named_format")]
    named_format::register(&mut r);

    // Dispatch: registers functions with name collisions (indexOf, ip,
    // isGreaterThan, etc.). Order-independent since individual modules no
    // longer register these conflicting names.
    #[cfg(any(
        feature = "strings",
        feature = "lists",
        feature = "semver_funcs",
        feature = "quantity",
        feature = "ip"
    ))]
    dispatch::register(&mut r);

    r.finish()
}

thread_local! {
    /// Set when an evaluation reaches a quantity kube-cel cannot treat as the
    /// apiserver does: past its supported range (an `add`/`sub` result past
    /// `i128` included), or the right side of an equality being an `add`/`sub`
    /// result (an error there).
    static UNSUPPORTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Notes that the running evaluation reached a value kube-cel does not
/// support. The function also returns an error, but CEL can absorb that
/// (`quantity(s) == q || true`); [`tracking_unsupported`] lets the validation
/// engine refuse the rule anyway.
#[cfg_attr(not(feature = "quantity"), allow(dead_code))]
pub(crate) fn note_unsupported() {
    UNSUPPORTED.with(|u| u.set(true));
}

/// Runs `f` (one evaluation) and reports whether it reached a value kube-cel
/// does not support, see [`note_unsupported`]. A call evaluation does not
/// reach (`true || quantity(s) == q`) is not noted.
#[cfg_attr(not(feature = "validation"), allow(dead_code))]
pub(crate) fn tracking_unsupported<R>(f: impl FnOnce() -> R) -> (R, bool) {
    let outer = UNSUPPORTED.with(|u| u.replace(false));
    let result = f();
    let reached = UNSUPPORTED.with(|u| u.replace(outer));
    (result, reached)
}

/// Test helper: evaluates `expr` the public way, on [`new_env`](crate::new_env)
/// and [`new_context`](crate::new_context), compiled with [`cel::Env::compile`].
#[cfg(test)]
pub(crate) fn eval(expr: &str) -> Result<cel::Value, cel::ExecutionError> {
    let env = std::sync::Arc::new(crate::new_env());
    let ctx = crate::new_context(std::sync::Arc::clone(&env)).expect("kube-cel's context on new_env()");
    env.compile(expr)
        .expect("the test expression compiles")
        .execute(&ctx)
}

#[cfg(test)]
mod tests {
    use super::register_all;

    /// A dispatcher is registered only with a feature it can dispatch to: with
    /// every function feature off, `indexOf` or `compareTo` would otherwise be
    /// registered and fail on every call.
    #[test]
    fn dispatchers_need_a_feature_that_implements_them() {
        let names = register_all(&mut cel::Context::default()).unwrap();
        let has = |name: &str| names.iter().any(|n| n == name);
        let search = cfg!(any(feature = "strings", feature = "lists"));
        let compare = cfg!(any(feature = "semver_funcs", feature = "quantity"));
        for (name, expected) in [
            ("indexOf", search),
            ("lastIndexOf", search),
            ("isGreaterThan", compare),
            ("isLessThan", compare),
            ("compareTo", compare),
        ] {
            assert_eq!(has(name), expected, "{name}");
        }
    }
}
