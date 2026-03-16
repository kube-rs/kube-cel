//! Client-side evaluation of Kubernetes ValidatingAdmissionPolicy CEL expressions.
//!
//! Supports all VAP variables except `authorizer` (requires API server).
//!
//! # Example
//!
//! ```rust
//! use kube_cel::vap::{AdmissionRequest, VapEvaluator, VapExpression};
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
    Context, Program, Value,
    objects::{Key, Map},
};

use crate::values::json_to_cel;

/// Group/Version/Kind identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupVersionKind {
    pub group: String,
    pub version: String,
    pub kind: String,
}

/// Group/Version/Resource identifier.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GroupVersionResource {
    pub group: String,
    pub version: String,
    pub resource: String,
}

/// A request context for VAP evaluation.
///
/// Mirrors the `request` variable available in Kubernetes ValidatingAdmissionPolicy
/// CEL expressions.
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
}

/// Client-side evaluator for Kubernetes ValidatingAdmissionPolicy CEL expressions.
///
/// Binds `object`, `oldObject`, `request`, and optionally `params` and
/// `namespaceObject` into a CEL context, then evaluates one or more
/// [`VapExpression`]s.
///
/// Construct via [`VapEvaluator::builder()`].
pub struct VapEvaluator {
    object: serde_json::Value,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
    params: Option<serde_json::Value>,
    namespace_object: Option<serde_json::Value>,
}

/// Builder for [`VapEvaluator`].
#[derive(Default)]
pub struct VapEvaluatorBuilder {
    object: Option<serde_json::Value>,
    old_object: Option<serde_json::Value>,
    request: AdmissionRequest,
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

    /// Set the policy parameters (`params` variable).
    /// Only bound when provided.
    pub fn params(mut self, p: serde_json::Value) -> Self {
        self.params = Some(p);
        self
    }

    /// Set the namespace object (`namespaceObject` variable).
    /// Only bound when provided.
    pub fn namespace_object(mut self, ns: serde_json::Value) -> Self {
        self.namespace_object = Some(ns);
        self
    }

    /// Consume the builder and produce a [`VapEvaluator`].
    pub fn build(self) -> VapEvaluator {
        VapEvaluator {
            object: self.object.unwrap_or(serde_json::Value::Null),
            old_object: self.old_object,
            request: self.request,
            params: self.params,
            namespace_object: self.namespace_object,
        }
    }
}

impl VapEvaluator {
    /// Create a new [`VapEvaluatorBuilder`].
    pub fn builder() -> VapEvaluatorBuilder {
        VapEvaluatorBuilder::default()
    }

    /// Build a CEL [`Context`] with all VAP variables bound.
    fn build_context(&self) -> Context<'static> {
        let mut ctx = Context::default();
        crate::register_all(&mut ctx);

        // Bind `object`
        let _ = ctx.add_variable("object", json_to_cel(&self.object));

        // Bind `oldObject` — null when not provided (e.g. CREATE)
        let old_object_val = match &self.old_object {
            Some(v) => json_to_cel(v),
            None => Value::Null,
        };
        let _ = ctx.add_variable("oldObject", old_object_val);

        // Bind `request`
        let _ = ctx.add_variable("request", request_to_cel(&self.request));

        // Bind `params` only when provided
        if let Some(params) = &self.params {
            let _ = ctx.add_variable("params", json_to_cel(params));
        }

        // Bind `namespaceObject` only when provided
        if let Some(ns) = &self.namespace_object {
            let _ = ctx.add_variable("namespaceObject", json_to_cel(ns));
        }

        ctx
    }

    /// Pre-compile validation expressions for repeated evaluation.
    ///
    /// Returns one entry per input expression. Failed compilations are
    /// represented as `Err(String)` containing the error message.
    #[must_use]
    pub fn compile_expressions(
        &self,
        expressions: &[VapExpression],
    ) -> Vec<Result<CompiledVapExpression, String>> {
        expressions
            .iter()
            .map(|expr| {
                let program =
                    Program::compile(&expr.expression).map_err(|e| format!("compilation error: {e}"))?;
                let message_program = expr
                    .message_expression
                    .as_deref()
                    .and_then(|me| Program::compile(me).ok());
                Ok(CompiledVapExpression {
                    program,
                    expression: expr.expression.clone(),
                    message: expr.message.clone(),
                    message_program,
                })
            })
            .collect()
    }

    /// Evaluate pre-compiled expressions against the bound context.
    ///
    /// The context is built once and all compiled expressions are executed
    /// against it. Expressions that failed compilation (represented as
    /// `Err`) are returned as failed [`VapResult`]s.
    #[must_use]
    pub fn evaluate_compiled(&self, compiled: &[Result<CompiledVapExpression, String>]) -> Vec<VapResult> {
        let ctx = self.build_context();

        compiled
            .iter()
            .map(|c| match c {
                Ok(ce) => match ce.program.execute(&ctx) {
                    Ok(Value::Bool(true)) => VapResult {
                        expression: ce.expression.clone(),
                        passed: true,
                        message: None,
                    },
                    Ok(Value::Bool(false)) => {
                        let msg = ce
                            .message_program
                            .as_ref()
                            .and_then(|prog| match prog.execute(&ctx) {
                                Ok(Value::String(s)) => Some((*s).clone()),
                                _ => None,
                            })
                            .or_else(|| ce.message.clone())
                            .unwrap_or_else(|| {
                                format!("validation expression '{}' evaluated to false", ce.expression)
                            });
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
                    expression: String::new(),
                    passed: false,
                    message: Some(e.clone()),
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
        assert!(results[0].message.as_ref().unwrap().contains("compilation error"));
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
