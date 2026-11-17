//! Kubernetes CEL extension-function libraries (Tier 1 — the primitive).
//!
//! Each submodule ports one upstream Kubernetes CEL library and exposes a
//! `register` entry; [`register_all`] aggregates the compiled-in set. This is
//! the faithful-port half of the crate — see the crate-level "Versioning and
//! stability" docs (Tier 1). The public entry point is
//! [`KubeCelExt`](crate::KubeCelExt); these modules are internal.

#[cfg(feature = "strings")] mod strings;

#[cfg(feature = "lists")] mod lists;

#[cfg(feature = "sets")] mod sets;

#[cfg(feature = "regex_funcs")] mod regex_funcs;

#[cfg(feature = "urls")] mod urls;

#[cfg(feature = "ip")] mod ip;

#[cfg(feature = "semver_funcs")] mod semver_funcs;

#[cfg(feature = "format")] mod format;

#[cfg(feature = "quantity")] mod quantity;

#[cfg(feature = "jsonpatch")] mod jsonpatch;

#[cfg(feature = "named_format")] mod named_format;

#[cfg(feature = "math")] mod math;

#[cfg(feature = "encoders")] mod encoders;

mod dispatch;
mod value_ops;

/// Adds functions to a root [`cel::Context`], keeping the first failure instead
/// of dropping it.
///
/// `cel` 0.16 made [`cel::Context::add_function`] fallible (it refuses a name the
/// context's `Env` already declares as an overload) and still returns `Ok(())`
/// without registering anything on a child scope. Every module registers
/// through this type so neither case can pass silently: a child scope is
/// rejected up front, and the first refused name is reported by
/// [`finish`](Registrar::finish).
pub(crate) struct Registrar<'c, 'p, 'v> {
    ctx: &'c mut cel::Context<'p, 'v>,
    profile: Profile,
    error: Option<crate::RegisterError>,
    names: Vec<String>,
}

/// Which function set a [`Registrar`] registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    /// Every compiled-in function: the public [`KubeCelExt`](crate::KubeCelExt)
    /// and [`new_context`](crate::new_context) surface.
    General,
    /// The validation engine's set, mirroring the apiserver's base CEL
    /// environment: see [`VALIDATION_EXCLUDED`] and [`register_profile`].
    /// The validation engine also refuses, at compile time, a call to any
    /// function this set leaves out.
    #[cfg_attr(not(feature = "validation"), allow(dead_code))]
    Validation,
}

/// Names the [`Profile::Validation`] set leaves out, on top of the whole
/// `math`, `jsonpatch` and `encoders` (`base64.*`) libraries.
///
/// - `slice`, `flatten`, `distinct`, `reverse`, `sort`, `lists.range`: the
///   validation `Env` enables `cel`'s upstream `lists` extension, which declares
///   these as overloads (so `cel` would refuse kube-cel's versions anyway). Its
///   implementations are used instead; string `reverse` goes away with
///   kube-cel's `reverse`, as in the apiserver.
/// - `isIPv4`, `isIPv6`, `isCIDRv4`, `isCIDRv6`: kube-cel conveniences the
///   apiserver does not have. A rule using one must not pass client-side when
///   the apiserver rejects it.
const VALIDATION_EXCLUDED: &[&str] = &[
    "slice",
    "flatten",
    "distinct",
    "reverse",
    "sort",
    "lists.range",
    "isIPv4",
    "isIPv6",
    "isCIDRv4",
    "isCIDRv6",
];

impl Profile {
    fn excludes(self, name: &str) -> bool {
        match self {
            Profile::General => false,
            Profile::Validation => VALIDATION_EXCLUDED.contains(&name),
        }
    }
}

impl<'c, 'p, 'v> Registrar<'c, 'p, 'v> {
    pub(crate) fn new(ctx: &'c mut cel::Context<'p, 'v>, profile: Profile) -> Self {
        let error = (!matches!(ctx, cel::Context::Root { .. })).then_some(crate::RegisterError::ChildScope);
        Self {
            ctx,
            profile,
            error,
            names: Vec::new(),
        }
    }

    /// Adds `f` as `name`, unless an earlier step already failed or the
    /// profile leaves `name` out.
    pub(crate) fn add<T: 'static, F>(&mut self, name: &str, f: F)
    where
        F: cel::extractors::IntoFunction<T> + 'static + Send + Sync,
    {
        if self.error.is_some() || self.profile.excludes(name) {
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

/// Declares the overloads kube-cel needs on the [`cel::Env`] rather than on a
/// context: those of names the standard library already declares (`string`),
/// which `cel` refuses as context functions.
#[allow(unused_variables)]
pub(crate) fn declare_overloads(env: &mut cel::Env) -> Result<(), cel::DeclarationError> {
    #[cfg(feature = "ip")]
    ip::declare_string_overloads(env)?;

    Ok(())
}

/// Registers all compiled-in Kubernetes CEL extension functions into `ctx`.
///
/// Internal implementation behind [`KubeCelExt::try_register_all`](crate::KubeCelExt::try_register_all);
/// kept as a free function so the in-crate callers (vap, validation, …) can
/// invoke it without importing the trait.
pub(crate) fn register_all(ctx: &mut cel::Context<'_, '_>) -> Result<(), crate::RegisterError> {
    register_profile(ctx, Profile::General).map(drop)
}

/// Registers the `profile`'s function set into `ctx` and returns the names it
/// registered.
///
/// [`Profile::Validation`] leaves out the `math`, `jsonpatch` and `encoders`
/// libraries and the names in [`VALIDATION_EXCLUDED`], none of which the
/// apiserver's base environment has.
pub(crate) fn register_profile(
    ctx: &mut cel::Context<'_, '_>,
    profile: Profile,
) -> Result<Vec<String>, crate::RegisterError> {
    let mut r = Registrar::new(ctx, profile);

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

    #[cfg(feature = "jsonpatch")]
    if profile == Profile::General {
        jsonpatch::register(&mut r);
    }

    #[cfg(feature = "named_format")]
    named_format::register(&mut r);

    #[cfg(feature = "math")]
    if profile == Profile::General {
        math::register(&mut r);
    }

    #[cfg(feature = "encoders")]
    if profile == Profile::General {
        encoders::register(&mut r);
    }

    // Dispatch: registers functions with name collisions (indexOf, reverse,
    // min/max, ip, isGreaterThan, etc.). Order-independent since individual
    // modules no longer register these conflicting names.
    dispatch::register(&mut r);

    r.finish()
}

/// Test helper: registers only the given module entries onto `ctx`, panicking
/// on failure.
#[cfg(test)]
#[allow(dead_code)] // unused when no function feature is enabled
pub(crate) fn register_parts(ctx: &mut cel::Context<'_, '_>, parts: &[fn(&mut Registrar<'_, '_, '_>)]) {
    let mut r = Registrar::new(ctx, Profile::General);
    for part in parts {
        part(&mut r);
    }
    r.finish().unwrap();
}
