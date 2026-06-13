//! Registration surface for the Kubernetes CEL extension functions.
//!
//! [`KubeCelExt`] is the single entry point for journey A ("add the Kubernetes
//! CEL functions to my [`cel::Context`]"). Granularity is controlled at
//! compile time through cargo features, not at runtime — there is intentionally
//! no per-library `with_strings()`/`with_lists()` method. Enable only the
//! features you need (see the crate-level docs for the feature model).

use cel::Context;

mod sealed {
    pub trait Sealed {}
    impl Sealed for cel::Context<'_> {}
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
/// | `math` | `math.greatest`, `math.least`, … | [cel-go `ext.Math`] |
/// | `encoders` | `base64.encode`, `base64.decode` | [cel-go `ext.Encoders`] |
/// | `urls` | `url`, `isURL`, `getScheme`, … | [k8s apiserver library] |
/// | `ip` | `ip`, `cidr`, `isIP`, `isCIDR`, … | [k8s apiserver library] |
/// | `semver_funcs` | `semver`, `isSemver`, `major`, … | [k8s apiserver library] |
/// | `format` | `format` | [k8s apiserver library] |
/// | `named_format` | named-format validation | [k8s apiserver library] |
/// | `quantity` | `quantity`, `isQuantity`, … | [k8s apiserver library] |
/// | `jsonpatch` | `jsonpatch.escapeKey` | [k8s apiserver library] |
///
/// [cel-go `ext.Strings`]: https://github.com/google/cel-go/blob/master/ext/README.md#strings
/// [cel-go `ext.Lists`]: https://github.com/google/cel-go/blob/master/ext/README.md#lists
/// [cel-go `ext.Sets`]: https://github.com/google/cel-go/blob/master/ext/README.md#sets
/// [cel-go `ext.Math`]: https://github.com/google/cel-go/blob/master/ext/README.md#math
/// [cel-go `ext.Encoders`]: https://github.com/google/cel-go/blob/master/ext/README.md#encoders
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
    fn register_all(&mut self) -> &mut Self;

    /// Builder sugar over [`register_all`](KubeCelExt::register_all):
    /// `cel::Context::default().with_all()`.
    fn with_all(mut self) -> Self {
        self.register_all();
        self
    }
}

impl KubeCelExt for Context<'_> {
    fn register_all(&mut self) -> &mut Self {
        crate::register_all(self);
        self
    }
}
