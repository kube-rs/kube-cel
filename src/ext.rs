//! Registration surface for the Kubernetes CEL extension functions.
//!
//! [`KubeCelExt`] is the single entry point for journey A ("add the Kubernetes
//! CEL functions to my [`cel::Context`]"). Granularity is controlled at
//! compile time through cargo features, not at runtime — there is intentionally
//! no per-library `with_strings()`/`with_lists()` method. Enable only the
//! features you need (see the crate-level docs for the feature model).

use cel::Context;

/// Registers the compiled-in Kubernetes CEL extension functions onto a
/// [`cel::Context`].
///
/// Which functions are registered is determined by the enabled cargo features
/// (see the [crate-level documentation](crate) for the feature model). This
/// registers the **whole** compiled-in set in one call, mirroring the
/// `KnownLibraries()` convention of the Kubernetes apiserver.
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
pub trait KubeCelExt: Sized {
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
