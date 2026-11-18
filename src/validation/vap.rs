//! Client-side evaluation of Kubernetes ValidatingAdmissionPolicy CEL expressions.
//!
//! Binds every VAP variable but `authorizer` (it needs an API server; an
//! expression that reads it fails with `UnsupportedReference`, even where CEL
//! absorbs the error) and `variables` (kube-cel does not model
//! `spec.variables`, so an expression naming it does not compile).
//!
//! # Example
//!
//! ```rust
//! use kube_cel::{AdmissionRequest, VapEvaluator, VapExpression};
//! use serde_json::json;
//!
//! let evaluator = VapEvaluator::builder()
//!     .object(json!({"spec": {"replicas": 3}}))
//!     .request(AdmissionRequest {
//!         operation: "CREATE".into(),
//!         username: "admin".into(),
//!         ..Default::default()
//!     })
//!     .build();
//!
//! let results = evaluator.evaluate(&[VapExpression {
//!     expression: "object.spec.replicas >= 0".into(),
//!     message: Some("replicas must be non-negative".into()),
//!     message_expression: None,
//! }]);
//!
//! assert!(results[0].passed);
//! ```

use std::{collections::HashMap, sync::Arc};

use cel::{
    Context, ExecutionError, Program, Value,
    common::value::CowVal,
    context::VariableResolver,
    objects::{Key, Map},
};

use crate::validation::{Scope, values::json_to_cel_raw};

/// Group/Version/Kind identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct GroupVersionKind {
    /// API group (e.g., `"apps"`). Empty string for the core group.
    pub group: String,
    /// API version (e.g., `"v1"`).
    pub version: String,
    /// Resource kind (e.g., `"Deployment"`).
    pub kind: String,
}

/// Group/Version/Resource identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct GroupVersionResource {
    /// API group (e.g., `"apps"`). Empty string for the core group.
    pub group: String,
    /// API version (e.g., `"v1"`).
    pub version: String,
    /// Resource name (e.g., `"deployments"`).
    pub resource: String,
}

/// A request context for VAP evaluation.
///
/// Mirrors the `request` variable available in Kubernetes ValidatingAdmissionPolicy
/// CEL expressions. This is a flat projection for CEL binding, **not** the admission
/// webhook wire type; if you have a `kube_core::admission::AdmissionRequest<T>`,
/// convert it via `to_cel_request()`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionRequest {
    /// The admission operation: `"CREATE"`, `"UPDATE"`, `"DELETE"`, or `"CONNECT"`.
    pub operation: String,
    /// The authenticated username of the requesting user.
    pub username: String,
    /// The UID of the requesting user.
    pub uid: String,
    /// The group memberships of the requesting user.
    pub groups: Vec<String>,
    /// The name of the resource being admitted.
    pub name: String,
    /// The namespace of the resource being admitted.
    pub namespace: String,
    /// Whether the request is a dry-run.
    pub dry_run: bool,
    /// The kind of the object being admitted.
    pub kind: GroupVersionKind,
    /// The resource being admitted.
    pub resource: GroupVersionResource,
}

/// A single CEL validation expression from a ValidatingAdmissionPolicy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VapExpression {
    /// The CEL expression to evaluate. Must evaluate to a boolean.
    pub expression: String,
    /// Static error message returned when the expression evaluates to `false`.
    pub message: Option<String>,
    /// CEL expression evaluated to produce the error message.
    /// Takes precedence over `message` when evaluation succeeds.
    pub message_expression: Option<String>,
}

/// The result of evaluating a single [`VapExpression`].
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct VapResult {
    /// The original CEL expression.
    pub expression: String,
    /// Whether the expression evaluated to `true` (admission allowed).
    pub passed: bool,
    /// Error message when `passed` is `false`. `None` if the expression passed.
    pub message: Option<String>,
}

/// A pre-compiled VAP expression for repeated evaluation.
///
/// Created by [`VapEvaluator::compile_expressions`]. Since [`cel::Program`] is
/// `!Clone`, wrap in [`Arc`] for shared ownership if needed.
pub struct CompiledVapExpression {
    program: Program,
    expression: String,
    message: Option<String>,
    message_program: Option<Program>,
    /// Whether `params` was declared when this compiled: the policy has a
    /// `paramKind`. Evaluation binds `params` to `null` when it was and the
    /// evaluator has no parameter object.
    params_declared: bool,
}

impl std::fmt::Debug for CompiledVapExpression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledVapExpression")
            .field("expression", &self.expression)
            .field("message", &self.message)
            .field("has_message_program", &self.message_program.is_some())
            .field("params_declared", &self.params_declared)
            .finish_non_exhaustive()
    }
}

/// An error produced when a VAP expression fails to compile.
///
/// Returned in the `Err` arm of [`VapEvaluator::compile_expressions`]. The
/// cause (the `cel` parse error, or a call to a function outside the
/// apiserver's base CEL environment) is available via [`std::error::Error::source`];
/// the concrete `cel` type is kept out of the public surface (held behind a
/// boxed `dyn Error`) so it is not frozen into the API.
///
/// `#[non_exhaustive]`: VAP compilation has a single failure mode today; new
/// fields may be added without a breaking change.
#[derive(Debug)]
#[non_exhaustive]
pub struct VapError {
    /// The VAP expression that failed to compile.
    pub expression: String,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for VapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "failed to compile VAP expression \"{}\": {}",
            self.expression, self.source
        )
    }
}

impl std::error::Error for VapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// Why the apiserver refuses a validation's static `message` at registration,
/// if it does: blank but not empty, or holding a line break once trimmed
/// (checked on v1.36.1).
fn message_error(message: &str) -> Option<&'static str> {
    let trimmed = message.trim();
    if !message.is_empty() && trimmed.is_empty() {
        Some("must be non-empty if specified")
    } else if trimmed.contains(['\n', '\r']) {
        Some("must not contain line breaks")
    } else {
        None
    }
}

/// Client-side evaluator for Kubernetes ValidatingAdmissionPolicy CEL expressions.
///
/// Binds `object`, `oldObject`, `request`, `namespaceObject` and, for a
/// policy with a `paramKind`, `params` into a CEL context, then evaluates one
/// or more [`VapExpression`]s. As on the apiserver, the policy decides what is
/// declared (`params` only with a `paramKind`, see
/// [`VapEvaluatorBuilder::has_param_kind`]) and the request supplies the
/// values; one not supplied is `null`.
///
/// Construct via [`VapEvaluator::builder()`].
#[derive(Debug)]
pub struct VapEvaluator {
    object: serde_json::Value,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
    /// Whether the policy has a `paramKind`, which declares `params`.
    param_kind: bool,
    /// The parameter object; `None` with `param_kind` binds `params` to null.
    params: Option<serde_json::Value>,
    namespace_object: Option<serde_json::Value>,
}

/// Builder for [`VapEvaluator`].
#[derive(Debug, Default)]
pub struct VapEvaluatorBuilder {
    object: Option<serde_json::Value>,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
    param_kind: Option<bool>,
    params: Option<serde_json::Value>,
    namespace_object: Option<serde_json::Value>,
}

impl VapEvaluatorBuilder {
    /// Set the object being admitted (`object` variable).
    pub fn object(mut self, obj: serde_json::Value) -> Self {
        self.object = Some(obj);
        self
    }

    /// Set the previous version of the object (`oldObject` variable).
    /// If not set, `oldObject` will be `null` (typical for CREATE operations).
    pub fn old_object(mut self, obj: serde_json::Value) -> Self {
        self.old_object = Some(obj);
        self
    }

    /// Set the admission request context (`request` variable).
    pub fn request(mut self, req: AdmissionRequest) -> Self {
        self.request = req;
        self
    }

    /// Declare whether the policy has a `paramKind` (`spec.paramKind`), which
    /// is what declares `params`.
    ///
    /// The apiserver declares `params` only for a policy with a `paramKind`,
    /// and refuses an expression naming it in one without, even in a branch
    /// never evaluated (`true || params.x == 1`). With `true`, expressions
    /// may name `params`, and it is `null` when no parameter object is set
    /// ([`params`](Self::params)), as for a binding with no `paramRef`. With
    /// `false`, an expression naming `params` does not compile.
    ///
    /// Unset, it follows [`params`](Self::params): a policy has a `paramKind`
    /// when a parameter object is set. Pass the policy's
    /// `spec.paramKind.is_some()` to compile its expressions as the apiserver
    /// does whether or not a parameter object is at hand, such as once, ahead
    /// of the requests:
    ///
    /// ```rust
    /// use kube_cel::{VapEvaluator, VapExpression};
    /// use serde_json::json;
    ///
    /// let expressions = [VapExpression {
    ///     expression: "params == null || object.spec.replicas <= params.data.max".into(),
    ///     message: None,
    ///     message_expression: None,
    /// }];
    /// // The policy has a paramKind: `params` is declared while compiling.
    /// let compiled = VapEvaluator::builder()
    ///     .has_param_kind(true)
    ///     .build()
    ///     .compile_expressions(&expressions);
    /// assert!(compiled[0].is_ok());
    ///
    /// // Per request, the parameter object the binding resolved...
    /// let evaluator = VapEvaluator::builder()
    ///     .object(json!({"spec": {"replicas": 3}}))
    ///     .params(json!({"data": {"max": 5}}))
    ///     .build();
    /// assert!(evaluator.evaluate_compiled(&compiled)[0].passed);
    ///
    /// // ...or none, for a binding with no paramRef: `params` is null, since
    /// // the compiled expressions declared it.
    /// let evaluator = VapEvaluator::builder()
    ///     .object(json!({"spec": {"replicas": 9}}))
    ///     .build();
    /// assert!(evaluator.evaluate_compiled(&compiled)[0].passed);
    /// ```
    ///
    /// `has_param_kind(false)` together with [`params`](Self::params), in
    /// either order, is a policy without a `paramKind`: `params` is not
    /// declared and the parameter object is not bound.
    pub fn has_param_kind(mut self, has_param_kind: bool) -> Self {
        self.param_kind = Some(has_param_kind);
        self
    }

    /// Set the parameter object (`params` variable) for this request.
    ///
    /// Unless [`has_param_kind`](Self::has_param_kind) says otherwise, this
    /// also marks the policy as having a `paramKind`, so expressions may name
    /// `params`. `Value::Null` binds `params` to `null`, as for a binding with
    /// no `paramRef`. (A `paramRef` whose parameter object is not found is
    /// the caller's to resolve: the apiserver then skips the binding
    /// (`parameterNotFoundAction: Allow`) or denies the request (`Deny`)
    /// without evaluating it.)
    pub fn params(mut self, p: serde_json::Value) -> Self {
        self.params = Some(p);
        self
    }

    /// Set the namespace object (`namespaceObject` variable). When not set,
    /// `namespaceObject` is `null`, as for a cluster-scoped request.
    pub fn namespace_object(mut self, ns: serde_json::Value) -> Self {
        self.namespace_object = Some(ns);
        self
    }

    /// Consume the builder and produce a [`VapEvaluator`].
    pub fn build(self) -> VapEvaluator {
        let param_kind = self.param_kind.unwrap_or(self.params.is_some());
        VapEvaluator {
            object: self.object.unwrap_or(serde_json::Value::Null),
            old_object: self.old_object,
            request: self.request,
            param_kind,
            // An explicit `has_param_kind(false)` drops the parameter object:
            // a policy without a paramKind has none.
            params: self.params.filter(|_| param_kind),
            namespace_object: self.namespace_object,
        }
    }
}

/// The policy variables the apiserver binds and kube-cel does not:
/// `authorizer` and `authorizer.requestResource`. Looking one up stays an
/// `UndeclaredReference`, and is noted as unsupported: the apiserver has a
/// value there, so a rule that reads one fails even where CEL absorbs the
/// error, which could otherwise skip a quantity update the apiserver makes. A
/// comprehension variable of that name is found before this.
struct UnboundAuthorizer;

static UNBOUND_AUTHORIZER: UnboundAuthorizer = UnboundAuthorizer;

impl VariableResolver for UnboundAuthorizer {
    fn resolve<'b>(&'b self, variable: &str) -> Option<CowVal<'b, 'b>> {
        let name = variable.strip_prefix('.').unwrap_or(variable);
        if name == "authorizer" || name.starts_with("authorizer.") {
            crate::functions::note_unsupported_error(|| {
                ExecutionError::UndeclaredReference(Arc::new("authorizer".to_owned()))
            });
        }
        None
    }
}

impl VapEvaluator {
    /// Create a new [`VapEvaluatorBuilder`].
    pub fn builder() -> VapEvaluatorBuilder {
        VapEvaluatorBuilder::default()
    }

    /// Build a CEL [`Context`] with all VAP variables bound: `params` to the
    /// parameter object, else to `null` when `params_declared`.
    fn build_context(&self, params_declared: bool) -> Context<'static, 'static> {
        let mut ctx = crate::validation::base_context();

        // Bind `object`
        let _ = ctx.add_variable("object", json_to_cel_raw(&self.object));

        // Bind `oldObject` — null when not provided (e.g. CREATE)
        let old_object_val = match &self.old_object {
            Some(v) => json_to_cel_raw(v),
            None => Value::Null,
        };
        let _ = ctx.add_variable("oldObject", old_object_val);

        // Bind `request`
        let _ = ctx.add_variable("request", request_to_cel(&self.request));

        // Bind `params`: the parameter object, or null for a policy with a
        // paramKind and none (the apiserver's nil `VersionedParams`).
        match &self.params {
            Some(params) => {
                let _ = ctx.add_variable("params", json_to_cel_raw(params));
            }
            None if params_declared => {
                let _ = ctx.add_variable("params", Value::Null);
            }
            None => {}
        }

        // Bind `namespaceObject` — null when not provided (a cluster-scoped
        // request), as the apiserver declares it for every policy.
        let namespace_val = match &self.namespace_object {
            Some(ns) => json_to_cel_raw(ns),
            None => Value::Null,
        };
        let _ = ctx.add_variable("namespaceObject", namespace_val);

        ctx.set_variable_resolver(&UNBOUND_AUTHORIZER);
        ctx
    }

    /// Pre-compile validation expressions for repeated evaluation.
    ///
    /// Returns one entry per input expression. Failed compilations are
    /// represented as `Err(`[`VapError`]`)`, which carries the offending
    /// expression and the cause: the `cel` parse error, a call to a function
    /// outside the apiserver's base CEL environment, or an identifier a policy
    /// does not declare (`self`, a typo), even one evaluation would never
    /// reach (via [`source`](std::error::Error::source)), or a static
    /// `message` the apiserver refuses (blank, or holding a line break once
    /// trimmed). A policy declares
    /// `object`, `oldObject`, `request` and `namespaceObject`, `authorizer`
    /// in a validation expression but not in its `messageExpression`, and
    /// `params` when it has a `paramKind`
    /// ([`VapEvaluatorBuilder::has_param_kind`]). `variables` is declared only
    /// with `spec.variables`, which kube-cel does not model, so an expression
    /// naming it does not compile. Each compiled expression keeps whether
    /// `params` was declared, for [`evaluate_compiled`](Self::evaluate_compiled).
    #[must_use]
    pub fn compile_expressions(
        &self,
        expressions: &[VapExpression],
    ) -> Vec<Result<CompiledVapExpression, VapError>> {
        let params = self.param_kind;
        // The apiserver declares `authorizer` for a validation, not for its
        // messageExpression.
        let scope = Scope::Vap {
            params,
            authorizer: true,
            variables: false,
        };
        let message_scope = Scope::Vap {
            params,
            authorizer: false,
            variables: false,
        };
        expressions
            .iter()
            .map(|expr| {
                if let Some(detail) = expr.message.as_deref().and_then(message_error) {
                    return Err(VapError {
                        expression: expr.expression.clone(),
                        source: format!(
                            "message {:?}: {detail}",
                            expr.message.as_deref().unwrap_or_default()
                        )
                        .into(),
                    });
                }
                let program =
                    crate::validation::compile(&expr.expression, scope).map_err(|source| VapError {
                        expression: expr.expression.clone(),
                        source,
                    })?;
                // A messageExpression that fails to compile fails closed (the
                // apiserver rejects such a policy at registration), mirroring the
                // expression path above rather than silently dropping it.
                let message_program = match expr.message_expression.as_deref() {
                    Some(me) => {
                        Some(
                            crate::validation::compile(me, message_scope).map_err(|source| VapError {
                                expression: me.to_string(),
                                source,
                            })?,
                        )
                    }
                    None => None,
                };
                Ok(CompiledVapExpression {
                    program,
                    expression: expr.expression.clone(),
                    message: expr.message.clone(),
                    message_program,
                    params_declared: params,
                })
            })
            .collect()
    }

    /// Evaluate pre-compiled expressions against the bound context.
    ///
    /// The context is built once and all compiled expressions are executed
    /// against it. Expressions that failed compilation (represented as
    /// `Err`) are returned as failed [`VapResult`]s.
    ///
    /// The compiled expressions carry the policy's declarations, this
    /// evaluator the request's values: when they were compiled with `params`
    /// declared (a policy with a `paramKind`) and this evaluator has no
    /// parameter object, `params` is `null`, whatever this evaluator's own
    /// [`has_param_kind`](VapEvaluatorBuilder::has_param_kind).
    #[must_use]
    pub fn evaluate_compiled(&self, compiled: &[Result<CompiledVapExpression, VapError>]) -> Vec<VapResult> {
        let params_declared = compiled.iter().flatten().any(|c| c.params_declared);
        let ctx = self.build_context(params_declared);

        compiled
            .iter()
            .map(|c| match c {
                Ok(ce) => match super::execute_supported(&ce.program, &ctx) {
                    Ok(Value::Bool(true)) => VapResult {
                        expression: ce.expression.clone(),
                        passed: true,
                        message: None,
                    },
                    Ok(Value::Bool(false)) => {
                        let expr_result =
                            ce.message_program
                                .as_ref()
                                .and_then(|prog| match prog.execute(&ctx) {
                                    Ok(Value::String(s)) => Some((*s).clone()),
                                    _ => None,
                                });
                        // Same sanitisation as the CRD path (trim + reject empty/
                        // multi-line/oversize), and apiserver's VAP default message.
                        let msg = super::resolve_failure_message(
                            expr_result.as_deref(),
                            ce.message.as_deref(),
                            || format!("failed expression: {}", ce.expression.trim()),
                        );
                        VapResult {
                            expression: ce.expression.clone(),
                            passed: false,
                            message: Some(msg),
                        }
                    }
                    Ok(other) => VapResult {
                        expression: ce.expression.clone(),
                        passed: false,
                        message: Some(format!("expression returned non-boolean: {other:?}")),
                    },
                    Err(e) => VapResult {
                        expression: ce.expression.clone(),
                        passed: false,
                        message: Some(format!("evaluation error: {e}")),
                    },
                },
                Err(e) => VapResult {
                    expression: e.expression.clone(),
                    passed: false,
                    message: Some(e.to_string()),
                },
            })
            .collect()
    }

    /// Evaluate a slice of [`VapExpression`]s against the bound context.
    ///
    /// Returns one [`VapResult`] per expression in the same order.
    /// Expressions that fail to compile or execute are treated as failures
    /// with a descriptive error message.
    #[must_use]
    pub fn evaluate(&self, expressions: &[VapExpression]) -> Vec<VapResult> {
        let compiled = self.compile_expressions(expressions);
        self.evaluate_compiled(&compiled)
    }
}

/// Convert an [`AdmissionRequest`] to a CEL [`Value::Map`].
///
/// Produces a map with the following shape (mirroring the K8s admission `request` variable):
/// ```text
/// {
///   "operation": string,
///   "name":      string,
///   "namespace": string,
///   "dryRun":    bool,
///   "kind":     { "group": string, "version": string, "kind": string },
///   "resource": { "group": string, "version": string, "resource": string },
///   "userInfo": { "username": string, "uid": string, "groups": list<string> },
/// }
/// ```
fn request_to_cel(req: &AdmissionRequest) -> Value {
    let mut map: HashMap<Key, Value> = HashMap::new();

    map.insert(
        Key::String(Arc::new("operation".into())),
        Value::String(Arc::new(req.operation.clone())),
    );
    map.insert(
        Key::String(Arc::new("name".into())),
        Value::String(Arc::new(req.name.clone())),
    );
    map.insert(
        Key::String(Arc::new("namespace".into())),
        Value::String(Arc::new(req.namespace.clone())),
    );
    map.insert(Key::String(Arc::new("dryRun".into())), Value::Bool(req.dry_run));

    // kind: { group, version, kind }
    let mut kind_map: HashMap<Key, Value> = HashMap::new();
    kind_map.insert(
        Key::String(Arc::new("group".into())),
        Value::String(Arc::new(req.kind.group.clone())),
    );
    kind_map.insert(
        Key::String(Arc::new("version".into())),
        Value::String(Arc::new(req.kind.version.clone())),
    );
    kind_map.insert(
        Key::String(Arc::new("kind".into())),
        Value::String(Arc::new(req.kind.kind.clone())),
    );
    map.insert(
        Key::String(Arc::new("kind".into())),
        Value::Map(Map {
            map: Arc::new(kind_map),
        }),
    );

    // resource: { group, version, resource }
    let mut resource_map: HashMap<Key, Value> = HashMap::new();
    resource_map.insert(
        Key::String(Arc::new("group".into())),
        Value::String(Arc::new(req.resource.group.clone())),
    );
    resource_map.insert(
        Key::String(Arc::new("version".into())),
        Value::String(Arc::new(req.resource.version.clone())),
    );
    resource_map.insert(
        Key::String(Arc::new("resource".into())),
        Value::String(Arc::new(req.resource.resource.clone())),
    );
    map.insert(
        Key::String(Arc::new("resource".into())),
        Value::Map(Map {
            map: Arc::new(resource_map),
        }),
    );

    // userInfo: { username, uid, groups }
    let groups_list: Vec<Value> = req
        .groups
        .iter()
        .map(|g| Value::String(Arc::new(g.clone())))
        .collect();
    let mut user_info_map: HashMap<Key, Value> = HashMap::new();
    user_info_map.insert(
        Key::String(Arc::new("username".into())),
        Value::String(Arc::new(req.username.clone())),
    );
    user_info_map.insert(
        Key::String(Arc::new("uid".into())),
        Value::String(Arc::new(req.uid.clone())),
    );
    user_info_map.insert(
        Key::String(Arc::new("groups".into())),
        Value::List(Arc::new(groups_list)),
    );
    map.insert(
        Key::String(Arc::new("userInfo".into())),
        Value::Map(Map {
            map: Arc::new(user_info_map),
        }),
    );

    Value::Map(Map { map: Arc::new(map) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vap_basic_validation_passes() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"metadata": {"name": "test"}, "spec": {"replicas": 3}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("replicas must be non-negative".into()),
            message_expression: None,
        }]);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    /// A policy's values are the request's decoded JSON: an integer past
    /// `i64` is a `float64` there, so a `double` in CEL.
    #[test]
    fn integer_past_i64_binds_as_double() {
        let evaluator = VapEvaluator::builder()
            .object(json!({ "n": 9223372036854775808u64 }))
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "type(object.n) == double".into(),
            message: None,
            message_expression: None,
        }]);
        assert!(results[0].passed, "{results:?}");
    }

    #[test]
    fn compile_failure_yields_vap_error_with_cause() {
        use std::error::Error;
        let evaluator = VapEvaluator::builder().build();
        let compiled = evaluator.compile_expressions(&[VapExpression {
            expression: "this is ((( not valid".into(),
            message: None,
            message_expression: None,
        }]);
        let err = compiled[0].as_ref().unwrap_err();
        assert_eq!(err.expression, "this is ((( not valid");
        assert!(
            err.source().is_some(),
            "VapError should chain the cel parse error"
        );

        // And evaluate_compiled surfaces the failure with the real expression.
        let results = evaluator.evaluate_compiled(&compiled);
        assert!(!results[0].passed);
        assert_eq!(results[0].expression, "this is ((( not valid");
    }

    #[test]
    fn invalid_message_expression_fails_closed() {
        use std::error::Error;
        // The expression itself is valid; only its messageExpression is broken.
        // The apiserver rejects such a policy at registration, so a broken
        // messageExpression must fail closed here too, not be silently dropped.
        let evaluator = VapEvaluator::builder().object(json!({})).build();
        let compiled = evaluator.compile_expressions(&[VapExpression {
            expression: "true".into(),
            message: Some("fallback".into()),
            message_expression: Some("invalid >=".into()),
        }]);
        let err = compiled[0].as_ref().unwrap_err();
        // The error points at the broken messageExpression and chains its cause.
        assert_eq!(err.expression, "invalid >=");
        assert!(err.source().is_some(), "VapError should chain the cel cause");

        // And evaluate_compiled surfaces it as a failed result (fail closed).
        let results = evaluator.evaluate_compiled(&compiled);
        assert!(!results[0].passed);
    }

    #[test]
    fn vap_validation_fails_with_message() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": -1}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("replicas must be non-negative".into()),
            message_expression: None,
        }]);
        assert!(!results[0].passed);
        assert_eq!(
            results[0].message.as_deref(),
            Some("replicas must be non-negative")
        );
    }

    #[test]
    fn vap_request_variables_accessible() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                username: "admin".into(),
                namespace: "default".into(),
                ..Default::default()
            })
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "request.operation == 'CREATE' && request.userInfo.username == 'admin'".into(),
            message: None,
            message_expression: None,
        }]);
        assert!(results[0].passed);
    }

    #[test]
    fn vap_old_object_null_on_create() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "oldObject == null".into(),
            message: None,
            message_expression: None,
        }]);
        assert!(results[0].passed);
    }

    #[test]
    fn vap_params_accessible() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": 5}}))
            .params(json!({"maxReplicas": 10}))
            .request(AdmissionRequest::default())
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas <= params.maxReplicas".into(),
            message: None,
            message_expression: None,
        }]);
        assert!(results[0].passed);
    }

    fn expr(expression: &str) -> VapExpression {
        VapExpression {
            expression: expression.into(),
            message: None,
            message_expression: None,
        }
    }

    /// The policy declares `params` (`has_param_kind`); the request supplies
    /// its value, `null` when there is none.
    #[test]
    fn param_kind_declares_params_and_binds_null_without_an_object() {
        let declared = VapEvaluator::builder().has_param_kind(true).build();
        assert!(declared.evaluate(&[expr("params == null")])[0].passed);

        // Unset, it follows `params`.
        assert!(
            VapEvaluator::builder()
                .build()
                .compile_expressions(&[expr("params == null")])[0]
                .is_err()
        );
        let inferred = VapEvaluator::builder().params(json!({"x": 1})).build();
        assert!(inferred.evaluate(&[expr("params.x == 1")])[0].passed);
        let null = VapEvaluator::builder().params(serde_json::Value::Null).build();
        assert!(null.evaluate(&[expr("params == null")])[0].passed);
    }

    /// An explicit `false` wins over `params`, in either order: `params` is
    /// not declared and the parameter object is not bound.
    #[test]
    fn explicit_no_param_kind_wins_over_params() {
        for evaluator in [
            VapEvaluator::builder()
                .has_param_kind(false)
                .params(json!({"x": 1}))
                .build(),
            VapEvaluator::builder()
                .params(json!({"x": 1}))
                .has_param_kind(false)
                .build(),
        ] {
            assert!(evaluator.compile_expressions(&[expr("true || params.x == 1")])[0].is_err());
            assert!(evaluator.evaluate(&[expr("object == null")])[0].passed);
        }
    }

    /// Compiled with a paramKind, evaluated on an evaluator with no
    /// parameter object: `params` is null, as the compiled policy declared it.
    #[test]
    fn compiled_declaration_binds_params_on_any_evaluator() {
        let exprs = [expr("params == null || params.x == 1")];
        let compiled = VapEvaluator::builder()
            .has_param_kind(true)
            .build()
            .compile_expressions(&exprs);
        for evaluator in [
            VapEvaluator::builder().build(),
            VapEvaluator::builder().has_param_kind(false).build(),
            VapEvaluator::builder().params(json!({"x": 1})).build(),
        ] {
            assert!(evaluator.evaluate_compiled(&compiled)[0].passed);
        }
        let other = VapEvaluator::builder().params(json!({"x": 2})).build();
        assert!(!other.evaluate_compiled(&compiled)[0].passed);
    }

    /// `namespaceObject` is declared for every policy and null when not set.
    #[test]
    fn namespace_object_defaults_to_null() {
        let evaluator = VapEvaluator::builder().object(json!({})).build();
        assert!(evaluator.evaluate(&[expr("namespaceObject == null")])[0].passed);
        let namespaced = VapEvaluator::builder()
            .namespace_object(json!({"metadata": {"name": "ns"}}))
            .build();
        assert!(namespaced.evaluate(&[expr("namespaceObject.metadata.name == 'ns'")])[0].passed);
    }

    /// The apiserver refuses a blank static message, or one with a line
    /// break once trimmed, at registration; padding and a trailing line
    /// break are fine (checked on v1.36.1).
    #[test]
    fn static_message_shape_is_checked() {
        let with_message = |message: &str| VapExpression {
            expression: "true".into(),
            message: Some(message.into()),
            message_expression: None,
        };
        let evaluator = VapEvaluator::builder().build();
        for message in ["   ", "a\nb", "a\rb"] {
            let compiled = evaluator.compile_expressions(&[with_message(message)]);
            assert!(compiled[0].is_err(), "{message:?}");
            assert!(
                !evaluator.evaluate(&[with_message(message)])[0].passed,
                "{message:?}"
            );
        }
        for message in ["", " x ", "a\n", "\na"] {
            assert!(
                evaluator.compile_expressions(&[with_message(message)])[0].is_ok(),
                "{message:?}"
            );
        }
    }

    /// `variables` comes with `spec.variables`, which kube-cel does not model.
    #[test]
    fn variables_do_not_compile() {
        let compiled = VapEvaluator::builder().build().compile_expressions(&[
            expr("true || variables.foo == 1"),
            expr("[1].all(variables, variables > 0)"),
        ]);
        assert!(compiled[0].is_err());
        assert!(compiled[1].is_ok());
    }

    #[test]
    fn vap_object_keys_not_escaped() {
        // VAP objects are unstructured: special-character map keys (labels,
        // annotations) are accessed by their literal name, not the CRD
        // field-escaped form. Escaping would make this lookup fail.
        let evaluator = VapEvaluator::builder()
            .object(json!({
                "metadata": {"labels": {"app.kubernetes.io/name": "web"}}
            }))
            .request(AdmissionRequest::default())
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.metadata.labels['app.kubernetes.io/name'] == 'web'".into(),
            message: None,
            message_expression: None,
        }]);
        assert!(
            results[0].passed,
            "literal label key should be accessible, got {:?}",
            results[0].message
        );
    }

    #[test]
    fn vap_message_expression() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": -1}}))
            .request(AdmissionRequest::default())
            .build();
        let results = evaluator.evaluate(&[VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("static fallback".into()),
            message_expression: Some("'replicas is ' + string(object.spec.replicas)".into()),
        }]);
        assert!(!results[0].passed);
        assert_eq!(results[0].message.as_deref(), Some("replicas is -1"));
    }

    #[test]
    fn vap_compiled_expressions_reusable() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": 3}}))
            .request(AdmissionRequest {
                operation: "CREATE".into(),
                ..Default::default()
            })
            .build();

        let expressions = vec![VapExpression {
            expression: "object.spec.replicas >= 0".into(),
            message: Some("bad".into()),
            message_expression: None,
        }];

        let compiled = evaluator.compile_expressions(&expressions);
        assert!(compiled[0].is_ok());

        let r1 = evaluator.evaluate_compiled(&compiled);
        let r2 = evaluator.evaluate_compiled(&compiled);
        assert!(r1[0].passed);
        assert!(r2[0].passed);
    }

    #[test]
    fn vap_compiled_error_preserved() {
        let evaluator = VapEvaluator::builder()
            .object(json!({}))
            .request(AdmissionRequest::default())
            .build();

        let expressions = vec![VapExpression {
            expression: "invalid >=".into(),
            message: None,
            message_expression: None,
        }];

        let compiled = evaluator.compile_expressions(&expressions);
        assert!(compiled[0].is_err());

        let results = evaluator.evaluate_compiled(&compiled);
        assert!(!results[0].passed);
        assert!(
            results[0]
                .message
                .as_ref()
                .unwrap()
                .contains("failed to compile VAP expression")
        );
    }

    #[test]
    fn vap_multiple_expressions() {
        let evaluator = VapEvaluator::builder()
            .object(json!({"spec": {"replicas": -1, "name": ""}}))
            .request(AdmissionRequest::default())
            .build();
        let results = evaluator.evaluate(&[
            VapExpression {
                expression: "object.spec.replicas >= 0".into(),
                message: Some("bad replicas".into()),
                message_expression: None,
            },
            VapExpression {
                expression: "object.spec.name.size() > 0".into(),
                message: Some("name required".into()),
                message_expression: None,
            },
        ]);
        assert_eq!(results.len(), 2);
        assert!(!results[0].passed);
        assert!(!results[1].passed);
    }
}
