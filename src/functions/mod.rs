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
    error: Option<crate::RegisterError>,
}

impl<'c, 'p, 'v> Registrar<'c, 'p, 'v> {
    pub(crate) fn new(ctx: &'c mut cel::Context<'p, 'v>) -> Self {
        let error = (!matches!(ctx, cel::Context::Root { .. })).then_some(crate::RegisterError::ChildScope);
        Self { ctx, error }
    }

    /// Adds `f` as `name`, unless an earlier step already failed.
    pub(crate) fn add<T: 'static, F>(&mut self, name: &str, f: F)
    where
        F: cel::extractors::IntoFunction<T> + 'static + Send + Sync,
    {
        if self.error.is_some() {
            return;
        }
        if let Err(source) = self.ctx.add_function(name, f) {
            self.error = Some(crate::RegisterError::Rejected {
                function: name.to_owned(),
                source: Box::new(source),
            });
        }
    }

    pub(crate) fn finish(self) -> Result<(), crate::RegisterError> {
        self.error.map_or(Ok(()), Err)
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

    #[cfg(feature = "jsonpatch")]
    jsonpatch::register(&mut r);

    #[cfg(feature = "named_format")]
    named_format::register(&mut r);

    #[cfg(feature = "math")]
    math::register(&mut r);

    #[cfg(feature = "encoders")]
    encoders::register(&mut r);

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
    let mut r = Registrar::new(ctx);
    for part in parts {
        part(&mut r);
    }
    r.finish().unwrap();
}
