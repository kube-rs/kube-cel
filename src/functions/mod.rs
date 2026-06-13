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

/// Registers all compiled-in Kubernetes CEL extension functions into `ctx`.
///
/// Internal implementation behind [`KubeCelExt::register_all`](crate::KubeCelExt::register_all);
/// kept as a free function so the in-crate callers (vap, validation, …) can
/// invoke it without importing the trait.
pub(crate) fn register_all(ctx: &mut cel::Context<'_>) {
    #[cfg(feature = "strings")]
    strings::register(ctx);

    #[cfg(feature = "lists")]
    lists::register(ctx);

    #[cfg(feature = "sets")]
    sets::register(ctx);

    #[cfg(feature = "regex_funcs")]
    regex_funcs::register(ctx);

    #[cfg(feature = "urls")]
    urls::register(ctx);

    #[cfg(feature = "ip")]
    ip::register(ctx);

    #[cfg(feature = "semver_funcs")]
    semver_funcs::register(ctx);

    #[cfg(feature = "format")]
    format::register(ctx);

    #[cfg(feature = "quantity")]
    quantity::register(ctx);

    #[cfg(feature = "jsonpatch")]
    jsonpatch::register(ctx);

    #[cfg(feature = "named_format")]
    named_format::register(ctx);

    #[cfg(feature = "math")]
    math::register(ctx);

    #[cfg(feature = "encoders")]
    encoders::register(ctx);

    // Dispatch: registers functions with name collisions (indexOf, reverse,
    // min/max, string, ip, isGreaterThan, etc.). Order-independent since
    // individual modules no longer register these conflicting names.
    dispatch::register(ctx);
}
