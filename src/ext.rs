//! Registration surface for the Kubernetes CEL extension functions.
//!
//! [`KubeCelExt`] is the single entry point for journey A ("add the Kubernetes
//! CEL functions to my [`cel::Context`]"). Granularity is controlled at
//! compile time through cargo features, not at runtime — there is intentionally
//! no per-library `with_strings()`/`with_lists()` method. Enable only the
//! features you need (see the crate-level docs for the feature model).

use std::sync::Arc;

use cel::{Context, Env};

mod sealed {
    pub trait Sealed {}
    impl Sealed for cel::Context<'_, '_> {}
}

/// Registers the compiled-in Kubernetes CEL extension functions onto a
/// [`cel::Context`].
///
/// Which functions are registered is determined by the enabled cargo features
/// (see the [crate-level documentation](crate) for the feature model). This
/// registers the **whole** compiled-in set in one call, following the
/// bundle-the-whole-set philosophy of the Kubernetes apiserver's
/// `KnownLibraries()` (which enumerates the standard CEL libraries as one
/// group; kube-cel likewise exposes its functions as a single batch rather
/// than piecemeal).
///
/// `all` means "all **compiled-in**", not "all Kubernetes functions". In a
/// narrowed build (`default-features = false, features = ["strings"]`),
/// `with_all()` registers only the `strings` group — the call site still reads
/// `with_all()`, but the set it registers shrinks with the feature flags. This
/// is intentional (the name tracks the compiled surface, mirroring
/// `KnownLibraries()`); enable the `full` feature to compile in the whole set
/// (see the crate-level feature model).
///
/// # Examples
///
/// Builder style (journey A one-liner):
///
/// ```rust
/// use kube_cel::{cel, KubeCelExt};
///
/// let ctx = cel::Context::default().with_all();
/// # let _ = ctx;
/// ```
///
/// Borrowed style, for an existing context:
///
/// ```rust
/// use kube_cel::{cel, KubeCelExt};
///
/// let mut ctx = cel::Context::default();
/// ctx.register_all();
/// # let _ = ctx;
/// ```
///
/// # Upstream sources
///
/// The registered functions track two upstream Kubernetes CEL libraries:
///
/// | Feature | Functions | Upstream |
/// |---|---|---|
/// | `strings` | string helpers (`charAt`, `indexOf`, `lowerAscii`, …) | [cel-go `ext.Strings`] |
/// | `lists` | list helpers (`isSorted`, `sum`, `min`, `max`, …) | [cel-go `ext.Lists`] |
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
/// This trait is [sealed]: it is implemented only for [`cel::Context`] and
/// cannot be implemented for downstream types, which lets kube-cel add methods
/// in a non-breaking way.
///
/// [sealed]: https://rust-lang.github.io/api-guidelines/future-proofing.html#c-sealed
pub trait KubeCelExt: sealed::Sealed + Sized {
    /// Registers all compiled-in Kubernetes CEL functions into this borrowed
    /// context, returning `&mut Self` for chaining.
    ///
    /// # Errors
    ///
    /// Fails with [`RegisterError::ChildScope`] on a child scope (made by
    /// [`Context::new_inner_scope`]), where `cel` would register nothing, and
    /// with [`RegisterError::Rejected`] when `cel` refuses one of the names,
    /// typically because the context's [`cel::Env`] already declares an
    /// overload of that name. Registration stops at the first failure, so the
    /// context may hold some of the functions.
    ///
    /// A root context on [`Context::default`] or on [`new_env`](crate::new_env)
    /// never fails.
    fn try_register_all(&mut self) -> Result<&mut Self, RegisterError>;

    /// Builder form of [`try_register_all`](KubeCelExt::try_register_all).
    ///
    /// # Errors
    ///
    /// Fails as [`try_register_all`](KubeCelExt::try_register_all) does.
    fn try_with_all(mut self) -> Result<Self, RegisterError> {
        self.try_register_all()?;
        Ok(self)
    }

    /// Registers all compiled-in Kubernetes CEL functions into this borrowed
    /// context, returning `&mut Self` for chaining.
    ///
    /// # Panics
    ///
    /// Panics where [`try_register_all`](KubeCelExt::try_register_all) fails:
    /// on a child scope, or on a context whose [`cel::Env`] already declares
    /// one of the names. Neither happens on a root [`Context::default`] or on
    /// [`new_env`](crate::new_env).
    fn register_all(&mut self) -> &mut Self {
        if let Err(err) = self.try_register_all() {
            panic!("kube-cel: {err}");
        }
        self
    }

    /// Builder sugar over [`register_all`](KubeCelExt::register_all):
    /// `cel::Context::default().with_all()`.
    ///
    /// # Panics
    ///
    /// Panics as [`register_all`](KubeCelExt::register_all) does.
    fn with_all(mut self) -> Self {
        self.register_all();
        self
    }
}

impl KubeCelExt for Context<'_, '_> {
    fn try_register_all(&mut self) -> Result<&mut Self, RegisterError> {
        crate::register_all(self)?;
        Ok(self)
    }
}

/// Why the Kubernetes CEL functions could not be registered onto a
/// [`cel::Context`].
///
/// Returned by [`KubeCelExt::try_register_all`], [`KubeCelExt::try_with_all`]
/// and [`new_context`](crate::new_context).
///
/// `#[non_exhaustive]`: new failure modes may be added without a breaking
/// change.
#[derive(Debug)]
#[non_exhaustive]
pub enum RegisterError {
    /// The context is a child scope, made by [`Context::new_inner_scope`].
    /// `cel` only registers functions on a root context: on a child scope it
    /// reports success and registers nothing. Register on the root context
    /// before creating inner scopes.
    ChildScope,
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
            RegisterError::ChildScope => {
                f.write_str("cannot register functions on a child scope; register them on the root context")
            }
            RegisterError::Rejected { function, source } => {
                write!(f, "cannot register function \"{function}\": {source}")
            }
        }
    }
}

impl std::error::Error for RegisterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegisterError::ChildScope => None,
            RegisterError::Rejected { source, .. } => Some(source.as_ref()),
        }
    }
}

/// Builds kube-cel's [`cel::Env`]: [`Env::stdlib`] plus the overloads kube-cel
/// has to declare on the environment instead of registering on a context.
///
/// Today these are `string(<IP>)` and `string(<CIDR>)` (feature `ip`).
/// `string` is a standard-library overload, so `cel` refuses it as a context
/// function: on a plain [`Context::default`], `string(ip('10.0.0.1'))` fails
/// with "no such overload". As in the apiserver, only the global form exists;
/// `ip('10.0.0.1').string()` is an error.
///
/// The result is an owned [`Env`], so further libraries, macros or overloads
/// can be added before it is shared. Compile with [`Env::compile`] on the same
/// environment, and evaluate on a context from [`new_context`]:
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
/// ```
///
/// kube-cel ships only functions the Kubernetes apiserver declares. `cel`'s
/// own extensions, such as `math` and `encoders` (cel-go's `ext.Math` and
/// `ext.Encoders`, which the apiserver does not have), go on the same way.
/// `math.greatest` / `math.least` are macros, so compile with [`Env::compile`]:
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
    crate::functions::declare_overloads(&mut env)
        .expect("kube-cel's Env overloads do not collide with the cel standard library");
    env
}

/// Builds a root [`cel::Context`] on `env` with all compiled-in Kubernetes CEL
/// functions registered: `Context::with_env(env)` followed by
/// [`try_register_all`](KubeCelExt::try_register_all).
///
/// Pass an environment from [`new_env`] (or one built on it) for the full
/// function set, including the overloads that live on the [`Env`].
///
/// # Errors
///
/// Fails with [`RegisterError::Rejected`] when `env` already declares an
/// overload under one of kube-cel's function names. An environment from
/// [`new_env`] never does.
pub fn new_context<'p, 'v>(env: Arc<Env>) -> Result<Context<'p, 'v>, RegisterError> {
    let mut ctx = Context::with_env(env);
    crate::register_all(&mut ctx)?;
    Ok(ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Value;

    fn eval(expr: &str) -> Result<Value, cel::ExecutionError> {
        let env = Arc::new(new_env());
        let ctx = new_context(Arc::clone(&env)).unwrap();
        env.compile(expr).unwrap().execute(&ctx)
    }

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
        let expr = "string(ip('10.0.0.1'))";
        assert_eq!(eval(expr), Ok(Value::String(Arc::new("10.0.0.1".into()))));
        // On a plain stdlib context the IP overload is not declared.
        let ctx = Context::default().with_all();
        assert!(cel::Program::compile(expr).unwrap().execute(&ctx).is_err());
    }

    #[test]
    fn default_context_registers_everything() {
        let mut ctx = Context::default();
        assert!(ctx.try_register_all().is_ok());
    }

    #[test]
    fn child_scope_is_rejected() {
        let root = Context::default();
        let mut child = root.new_inner_scope();
        assert!(matches!(child.try_register_all(), Err(RegisterError::ChildScope)));
    }

    #[test]
    #[should_panic(expected = "child scope")]
    fn register_all_panics_on_child_scope() {
        let root = Context::default();
        let mut child = root.new_inner_scope();
        child.register_all();
    }

    #[test]
    #[cfg(feature = "lists")]
    fn name_declared_on_the_env_is_rejected() {
        let mut env = new_env();
        cel::extensions::lists(&mut env).unwrap();
        let Err(err) = new_context(Arc::new(env)) else {
            panic!("expected registration to fail");
        };
        let RegisterError::Rejected { function, .. } = &err else {
            panic!("expected Rejected, got {err:?}");
        };
        assert!(
            ["slice", "flatten", "distinct", "sort", "lists.range", "reverse"].contains(&function.as_str()),
            "{function}"
        );
        assert!(std::error::Error::source(&err).is_some());
    }
}
