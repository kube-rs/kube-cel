//! Entry points for the Kubernetes CEL extension functions.
//!
//! [`new_env`] builds kube-cel's [`cel::Env`] and [`new_context`] a context on
//! it with kube-cel's functions registered: the path for evaluating Kubernetes
//! CEL outside the validation engine, which uses the same two. Granularity is
//! controlled at compile time through cargo features, not at runtime — there is
//! intentionally no per-library `with_strings()`/`with_lists()` method. Enable
//! only the features you need (see the crate-level docs for the feature model).

use std::sync::Arc;

use cel::{Context, Env};

mod sealed {
    pub trait Sealed {}
    impl Sealed for cel::Context<'_, '_> {}
}

/// Registers kube-cel's context functions onto a [`cel::Context`] you already
/// have.
///
/// Deprecated: this registers only the functions that live on a context. It
/// does not install the part of kube-cel's environment that lives on the
/// [`cel::Env`] (`cel`'s upstream `lists` extension, the `sortBy` and
/// two-variable comprehension macros, `string(<IP>)` / `string(<CIDR>)`), so on
/// [`Context::default`] those are missing. Build the environment with
/// [`new_env`] and the context with [`new_context`] instead. The trait will be
/// removed once kube-cel's functions move to `Env` overloads, see
/// [kube-rs/kube-cel#17](https://github.com/kube-rs/kube-cel/issues/17).
///
/// Which functions are registered is determined by the enabled cargo features
/// (see the [crate-level documentation](crate) for the feature model).
///
/// ```rust
/// # #![allow(deprecated)]
/// use kube_cel::{cel, KubeCelExt};
///
/// let ctx = cel::Context::default().with_all();
/// # let _ = ctx;
/// ```
///
/// This trait is [sealed]: it is implemented only for [`cel::Context`] and
/// cannot be implemented for downstream types.
///
/// [sealed]: https://rust-lang.github.io/api-guidelines/future-proofing.html#c-sealed
#[deprecated(
    since = "0.9.0",
    note = "registers only the Context functions, not the Env part (upstream lists, the sortBy and \
            two-variable comprehension macros, string(<IP>)); use new_env() and new_context(). Will \
            be removed once functions move to Env overloads, see kube-rs/kube-cel#17"
)]
pub trait KubeCelExt: sealed::Sealed + Sized {
    /// Registers kube-cel's context functions into this borrowed context,
    /// returning `&mut Self` for chaining. Deprecated: see [`KubeCelExt`].
    ///
    /// # Panics
    ///
    /// Panics on a child scope (made by [`Context::new_inner_scope`]), where
    /// `cel` would register nothing, and when `cel` refuses one of the names,
    /// typically because the context's [`cel::Env`] already declares an
    /// overload of that name. Neither happens on a root [`Context::default`]
    /// or on [`new_env`].
    #[deprecated(
        since = "0.9.0",
        note = "registers only the Context functions, not the Env part (upstream lists, the sortBy \
                and two-variable comprehension macros, string(<IP>)); use new_env() and \
                new_context(). Will be removed once functions move to Env overloads, see \
                kube-rs/kube-cel#17"
    )]
    fn register_all(&mut self) -> &mut Self;

    /// Builder sugar over [`register_all`](KubeCelExt::register_all):
    /// `cel::Context::default().with_all()`. Deprecated: see [`KubeCelExt`].
    ///
    /// # Panics
    ///
    /// Panics as [`register_all`](KubeCelExt::register_all) does.
    #[deprecated(
        since = "0.9.0",
        note = "registers only the Context functions, not the Env part (upstream lists, the sortBy \
                and two-variable comprehension macros, string(<IP>)); use new_env() and \
                new_context(). Will be removed once functions move to Env overloads, see \
                kube-rs/kube-cel#17"
    )]
    fn with_all(mut self) -> Self {
        #[allow(deprecated)]
        self.register_all();
        self
    }
}

#[allow(deprecated)]
impl KubeCelExt for Context<'_, '_> {
    fn register_all(&mut self) -> &mut Self {
        assert!(
            matches!(self, Context::Root { .. }),
            "kube-cel: cannot register functions on a child scope; register them on the root context"
        );
        if let Err(err) = crate::register_all(self) {
            panic!("kube-cel: {err}");
        }
        self
    }
}

/// Why [`new_context`] could not build a context on an environment.
///
/// `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change.
#[derive(Debug)]
#[non_exhaustive]
pub enum RegisterError {
    /// The environment was not built by [`new_env`], so it lacks the part of
    /// kube-cel's environment that lives on the [`cel::Env`] (`cel`'s upstream
    /// `lists` extension, the `sortBy` and two-variable comprehension macros,
    /// `string(<IP>)` / `string(<CIDR>)`, as the enabled features have them).
    /// Start from [`new_env`] and add to it.
    IncompleteEnv,
    /// `cel` refused to add `function`, typically because the context's
    /// [`cel::Env`] already declares an overload of that name.
    Rejected {
        /// The function name that was refused.
        function: String,
        /// The boxed error `cel` reported. Boxed (rather than carrying the
        /// concrete `cel::DeclarationError`) so the pre-1.0 `cel` type is not
        /// frozen into this public enum variant; reach it via
        /// [`Error::source`](std::error::Error::source).
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterError::IncompleteEnv => f.write_str(
                "the environment was not built by kube_cel::new_env(), so it lacks kube-cel's Env part",
            ),
            RegisterError::Rejected { function, source } => {
                write!(f, "cannot register function \"{function}\": {source}")
            }
        }
    }
}

impl std::error::Error for RegisterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegisterError::IncompleteEnv => None,
            RegisterError::Rejected { source, .. } => Some(source.as_ref()),
        }
    }
}

/// The name kube-cel's [`Env`] part is added to an environment under (see
/// [`Env::has_library`]).
const ENV_LIBRARY: &str = "kube-cel.env";

/// The part of kube-cel's environment that lives on the [`Env`]: `cel`'s
/// upstream `lists` extension, the two-variable comprehension macros and the
/// `string(<IP>)` / `string(<CIDR>)` overloads, as the enabled features have
/// them (see `functions::declare_env`).
///
/// A named [`cel::Library`], so an environment records that it has the whole
/// of it: `cel` adds the name only once the library registered without error.
struct EnvLibrary;

impl cel::Library for EnvLibrary {
    fn name(&self) -> Option<&str> {
        Some(ENV_LIBRARY)
    }

    fn register(&self, env: &mut Env) -> Result<(), cel::DeclarationError> {
        crate::functions::declare_env(env)
    }
}

/// Builds kube-cel's [`cel::Env`]: the environment the Kubernetes apiserver
/// compiles CEL in, as far as kube-cel models it, and the one kube-cel's
/// validation engine uses.
///
/// On top of [`Env::stdlib`] it has, as the enabled features allow:
///
/// - `cel`'s upstream `lists` extension (feature `lists`): `slice`, `flatten`,
///   `distinct`, `reverse`, `sort`, `lists.range` and the `sortBy` macro, as
///   cel-go's `ext.Lists`;
/// - the two-variable comprehension macros: `all(i, v, ..)`, `exists(i, v, ..)`,
///   `existsOne(i, v, ..)`, `transformList`, `transformMap`, `transformMapEntry`;
/// - `string(<IP>)` and `string(<CIDR>)` (feature `ip`). As in the apiserver,
///   only the global form exists: `ip('10.0.0.1').string()` is an error.
///
/// The functions that live on a context are registered by [`new_context`],
/// which needs an environment from this function. The result is an owned
/// [`Env`], so `cel`'s extensions or overloads of your own can be added before
/// it is shared. Compile with [`Env::compile`] on the same environment, which
/// expands its macros ([`cel::Program::compile`] knows only the standard
/// ones), and evaluate on a context from [`new_context`]:
///
/// ```rust
/// use std::sync::Arc;
/// use kube_cel::cel::Value;
///
/// let env = Arc::new(kube_cel::new_env());
/// let ctx = kube_cel::new_context(Arc::clone(&env)).unwrap();
/// let program = env.compile("string(cidr('10.0.0.0/8').ip())").unwrap();
/// # #[cfg(feature = "ip")]
/// assert_eq!(program.execute(&ctx), Ok(Value::String(Arc::new("10.0.0.0".into()))));
/// let program = env.compile("[3, 1, 2].sortBy(x, -x) == [3, 2, 1]").unwrap();
/// # #[cfg(feature = "lists")]
/// assert_eq!(program.execute(&ctx), Ok(Value::Bool(true)));
/// ```
///
/// kube-cel ships only functions the Kubernetes apiserver declares. `cel`'s
/// own extensions, such as `math` and `encoders` (cel-go's `ext.Math` and
/// `ext.Encoders`, which the apiserver does not have), go on the same way.
/// `math.greatest` / `math.least` are macros too:
///
/// ```rust
/// use std::sync::Arc;
/// use kube_cel::cel::{self, Value};
///
/// let mut env = kube_cel::new_env();
/// env.add_extension(cel::extensions::math).unwrap();
/// env.add_extension(cel::extensions::encoders).unwrap();
/// let env = Arc::new(env);
/// let ctx = kube_cel::new_context(Arc::clone(&env)).unwrap();
/// let program = env
///     .compile("math.greatest(1, 3, 2) == 3 && base64.encode(b'hi') == 'aGk='")
///     .unwrap();
/// assert_eq!(program.execute(&ctx), Ok(Value::Bool(true)));
/// ```
#[must_use]
pub fn new_env() -> Env {
    let mut env = Env::stdlib();
    env.add_library(EnvLibrary)
        .expect("kube-cel's Env part does not collide with the cel standard library");
    env
}

/// Builds a root [`cel::Context`] on `env` with kube-cel's context functions
/// registered: with [`new_env`], the whole of kube-cel's environment.
///
/// `env` must start from [`new_env`]: the part of kube-cel's environment that
/// lives on the [`Env`] is there, and this adds the part that lives on a
/// context. An `env` from [`new_env`] with `cel`'s extensions or overloads of
/// your own added is fine, as long as they do not take kube-cel's names.
///
/// Which functions are registered is determined by the enabled cargo features
/// (see the [crate-level documentation](crate) for the feature model). They
/// track these upstream libraries:
///
/// | Feature | Functions | Upstream |
/// |---|---|---|
/// | `strings` | string helpers (`charAt`, `indexOf`, `lowerAscii`, …) | [cel-go `ext.Strings`] |
/// | `lists` | list helpers (`isSorted`, `sum`, `min`, `max`, …); [`new_env`] adds cel-go's `ext.Lists` | [k8s apiserver library], [cel-go `ext.Lists`] |
/// | `sets` | `sets.contains`, `sets.intersects`, `sets.equivalent` | [cel-go `ext.Sets`] |
/// | `regex_funcs` | `find`, `findAll` | [k8s apiserver library] |
/// | `urls` | `url`, `isURL`, `getScheme`, … | [k8s apiserver library] |
/// | `ip` | `ip`, `cidr`, `isIP`, `isCIDR`, … | [k8s apiserver library] |
/// | `semver_funcs` | `semver`, `isSemver`, `major`, … | [k8s apiserver library] |
/// | `format` | `format` | [k8s apiserver library] |
/// | `named_format` | named-format validation | [k8s apiserver library] |
/// | `quantity` | `quantity`, `isQuantity`, … | [k8s apiserver library] |
///
/// [cel-go `ext.Strings`]: https://github.com/google/cel-go/blob/master/ext/README.md#strings
/// [cel-go `ext.Lists`]: https://github.com/google/cel-go/blob/master/ext/README.md#lists
/// [cel-go `ext.Sets`]: https://github.com/google/cel-go/blob/master/ext/README.md#sets
/// [k8s apiserver library]: https://pkg.go.dev/k8s.io/apiserver/pkg/cel/library
///
/// Whether `env` starts from [`new_env`] is told by the name kube-cel's `Env`
/// part is added under (`"kube-cel.env"`, see [`Env::has_library`]), which
/// `cel` records only once all of that part registered. The check catches an
/// environment that does not start from [`new_env`]; it does not stop a
/// [`cel::Library`] of your own added under that name to get past it, which
/// leaves the context without kube-cel's `Env` part. (`cel` exposes overloads
/// by name, id and type name, all of which a caller can repeat, so a check
/// that resists that would need a hidden marker overload compared by function
/// address, which Rust does not guarantee to be unique.)
///
/// # Errors
///
/// Fails with [`RegisterError::IncompleteEnv`] when `env` was not built by
/// [`new_env`] (such as [`Env::stdlib`]), and with [`RegisterError::Rejected`]
/// when `env` declares an overload under one of kube-cel's function names, as
/// `cel`'s `strings` extension does. An environment from [`new_env`] as it is
/// never fails.
pub fn new_context<'p, 'v>(env: Arc<Env>) -> Result<Context<'p, 'v>, RegisterError> {
    if !env.has_library(ENV_LIBRARY) {
        return Err(RegisterError::IncompleteEnv);
    }
    let mut ctx = Context::with_env(env);
    crate::register_all(&mut ctx)?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Value;

    use crate::functions::eval;

    /// The standard `string` overloads are global-only, as in the apiserver:
    /// `string(x)` works, `x.string()` is an error. kube-cel no longer shadows
    /// `string` with a context function.
    fn assert_global_string_only(arg: &str, expected: &str) {
        assert_eq!(
            eval(&format!("string({arg})")),
            Ok(Value::String(Arc::new(expected.into()))),
            "string({arg})"
        );
        assert!(
            eval(&format!("{arg}.string()")).is_err(),
            "{arg}.string() must be an error"
        );
    }

    #[test]
    fn test_string_int() {
        assert_global_string_only("42", "42");
    }

    #[test]
    fn test_string_uint() {
        assert_global_string_only("42u", "42");
    }

    #[test]
    fn test_string_float() {
        assert_global_string_only("3.14", "3.14");
    }

    #[test]
    fn test_string_string() {
        assert_global_string_only("'hello'", "hello");
    }

    #[test]
    fn test_string_bytes() {
        assert_global_string_only("b'abc'", "abc");
    }

    #[test]
    #[cfg(feature = "ip")]
    fn string_of_ip_needs_the_kube_env() {
        assert_eq!(
            eval("string(ip('10.0.0.1'))"),
            Ok(Value::String(Arc::new("10.0.0.1".into())))
        );
    }

    fn holds(expr: &str) {
        assert_eq!(eval(expr), Ok(Value::Bool(true)), "{expr}");
    }

    /// `new_env` has what the apiserver's base environment has on its `Env`:
    /// `cel`'s upstream `lists` extension, with the `sortBy` macro, and the
    /// two-variable comprehension macros.
    #[test]
    #[cfg(feature = "lists")]
    fn new_env_has_the_upstream_lists_extension() {
        holds("[1, 2, 3, 4].slice(1, 3) == [2, 3]");
        holds("[[1, [2]], [3]].flatten(2) == [1, 2, 3]");
        holds("[1, 2, 2, 1].distinct() == [1, 2]");
        holds("[1, 2, 3].reverse() == [3, 2, 1]");
        holds("[3, 1, 2].sort() == [1, 2, 3]");
        holds("lists.range(3) == [0, 1, 2]");
        holds("[3, 1, 2].sortBy(x, -x) == [3, 2, 1]");
        // Kubernetes' own list functions sit alongside.
        holds("[3, 1, 2].isSorted() == false && [3, 1, 2].sum() == 6 && [3, 1, 2].min() == 1");
    }

    #[test]
    fn new_env_has_the_two_variable_comprehensions() {
        holds("{'a': 1, 'b': 2}.all(k, v, v > 0)");
        holds("[10, 20].exists(i, v, i == 1 && v == 20)");
        holds("[1, 2].transformList(i, v, v * 2) == [2, 4]");
        holds("{'a': 1}.transformMap(k, v, v + 1) == {'a': 2}");
    }

    /// The upstream `lists` functions behave as cel-go's, not as kube-cel's
    /// own did before 0.9: pinned for the CHANGELOG entry.
    #[test]
    #[cfg(feature = "lists")]
    fn upstream_lists_behaviour() {
        // No string `reverse()`: the apiserver has none.
        assert!(eval("'abc'.reverse()").is_err());
        // Mixed numeric types do not sort; bytes do.
        assert!(eval("[3, 1u, 2.0].sort()").is_err());
        holds("[b'b', b'a'].sort() == [b'a', b'b']");
        // An extra argument is refused, not ignored.
        assert!(eval("[[1]].flatten(1, 2)").is_err());
        assert!(eval("[1].distinct(99)").is_err());
        assert!(eval("[1].reverse(99)").is_err());
        // `lists.range` makes at most 1,000,000 elements.
        holds("size(lists.range(1000000)) == 1000000");
        assert!(eval("lists.range(1000001)").is_err());
    }

    /// The deprecated `with_all()` registers the context functions on
    /// `Context::default()`, but not the part of kube-cel's environment that
    /// lives on the `Env`, as its documentation says.
    #[test]
    #[cfg(all(feature = "lists", feature = "ip"))]
    #[allow(deprecated)]
    fn deprecated_with_all_registers_only_the_context_functions() {
        let ctx = Context::default().with_all();
        let run = |expr: &str| cel::Program::compile(expr).unwrap().execute(&ctx);
        assert_eq!(run("[3, 1, 2].sum()"), Ok(Value::Int(6)));
        assert!(run("[3, 1, 2].sort()").is_err());
        assert!(run("string(ip('10.0.0.1'))").is_err());
        // On an environment from `new_env` it has the whole set.
        let env = Arc::new(new_env());
        let ctx = Context::with_env(Arc::clone(&env)).with_all();
        assert_eq!(
            env.compile("[3, 1, 2].sortBy(x, x)").unwrap().execute(&ctx),
            Ok(Value::List(Arc::new(vec![1.into(), 2.into(), 3.into()])))
        );
        assert_eq!(
            env.compile("string(ip('10.0.0.1'))").unwrap().execute(&ctx),
            Ok(Value::String(Arc::new("10.0.0.1".into())))
        );
    }

    #[test]
    #[should_panic(expected = "child scope")]
    #[allow(deprecated)]
    fn register_all_panics_on_child_scope() {
        let root = Context::default();
        let mut child = root.new_inner_scope();
        child.register_all();
    }

    /// An environment that does not start from `new_env` lacks kube-cel's
    /// `Env` part: `new_context` refuses it rather than build a context
    /// missing it.
    #[test]
    fn env_not_from_new_env_is_rejected() {
        let Err(err) = new_context(Arc::new(Env::stdlib())) else {
            panic!("expected Env::stdlib() to be refused");
        };
        assert!(matches!(err, RegisterError::IncompleteEnv), "{err:?}");
        assert!(std::error::Error::source(&err).is_none());
    }

    /// The string `reverse()` the CHANGELOG points general users to: an
    /// overload of their own, beside the upstream list `reverse()`.
    #[test]
    fn own_string_reverse_fits_new_env() {
        use cel::common::types::CelString;
        fn reverse(this: &CelString<'_>) -> CelString<'static> {
            CelString::from(this.inner().chars().rev().collect::<String>())
        }
        let mut env = new_env();
        cel::add_member_overload!(env, fn reverse: (CelString) -> CelString).unwrap();
        let env = Arc::new(env);
        let ctx = new_context(Arc::clone(&env)).unwrap();
        let program = env.compile("'abc'.reverse() == 'cba'").unwrap();
        assert_eq!(program.execute(&ctx), Ok(Value::Bool(true)));
        #[cfg(feature = "lists")]
        assert_eq!(
            env.compile("[1, 2].reverse() == [2, 1]").unwrap().execute(&ctx),
            Ok(Value::Bool(true))
        );
    }

    /// `cel`'s own extensions can be added to `new_env`'s environment.
    #[test]
    fn extensions_on_new_env_are_accepted() {
        let mut env = new_env();
        env.add_extension(cel::extensions::math).unwrap();
        env.add_extension(cel::extensions::bindings).unwrap();
        let env = Arc::new(env);
        let ctx = new_context(Arc::clone(&env)).unwrap();
        let program = env.compile("cel.bind(x, math.abs(-2), x * x) == 4").unwrap();
        assert_eq!(program.execute(&ctx), Ok(Value::Bool(true)));
    }

    /// `cel`'s `strings` extension declares overloads under the names of
    /// kube-cel's string functions (cel-go's `ext.Strings`, which kube-cel
    /// registers on the context), so a context on such an environment fails.
    #[test]
    #[cfg(feature = "strings")]
    fn name_declared_on_the_env_is_rejected() {
        let mut env = new_env();
        env.add_extension(cel::extensions::strings).unwrap();
        let Err(err) = new_context(Arc::new(env)) else {
            panic!("expected registration to fail");
        };
        let RegisterError::Rejected { function, .. } = &err else {
            panic!("expected Rejected, got {err:?}");
        };
        assert_eq!(function, "charAt");
        assert!(std::error::Error::source(&err).is_some());
    }
}
