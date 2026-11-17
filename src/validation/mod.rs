//! Client-side CRD validation engine (Tier 2 — the oracle).
//!
//! The crate root re-exports this module's public items flatly; the submodules
//! here are internal. This file is the engine entry: [`Validator`] recursively
//! walks an OpenAPI schema, compiles `x-kubernetes-validations` rules, evaluates
//! them against object data, and collects [`ValidationError`]s. See the
//! crate-level "Versioning and stability" docs (Tier 2 — evolving).

pub(crate) mod analysis;
pub(crate) mod compilation;
pub(crate) mod defaults;
pub(crate) mod escaping;
pub(crate) mod references;
pub(crate) mod values;
pub(crate) mod vap;

// The live expression probe (`just probe`), a unit test so it can use the
// private validation environment.
#[cfg(test)] mod probe;

use crate::validation::{
    compilation::{
        ChildSchema, CompilationError, CompilationResult, CompiledSchema, child_schemas,
        compile_schema_validations,
    },
    values::{json_to_cel_with_compiled, json_to_cel_with_schema},
};
use cel::{Context, Env, Program};
use std::sync::{Arc, LazyLock};

/// The `cel` environment that CRD rules and VAP expressions compile and run
/// against, shared by every [`Validator`], [`compile_schema`](crate::compile_schema)
/// call and [`VapEvaluator`](crate::VapEvaluator).
///
/// It mirrors the apiserver's base CEL environment
/// (`k8s.io/apiserver/pkg/cel/environment`, `MustBaseEnvSet(..).NewExpressionsEnv()`),
/// as far as `cel` provides it: on top of [`new_env`](crate::new_env) it enables
/// the two-variable comprehensions (`all(i, v, ..)`, `transformList`,
/// `transformMap`, ..) and, with the `lists` feature, `cel`'s upstream `lists`
/// extension (`sortBy`, `slice`, `flatten`, `distinct`, `reverse`, `sort`,
/// `lists.range`). It leaves out `bindings` (`cel.bind`), `network` and `math`,
/// which the apiserver's base environment does not have; [`base_context`]
/// likewise leaves out kube-cel's own functions the apiserver lacks.
///
/// Rules must compile with [`compile`] (not [`cel::Program::compile`], which
/// only knows the standard macros) and run on a context from [`base_context`].
pub(crate) fn validation_env() -> &'static Arc<Env> {
    static ENV: LazyLock<Arc<Env>> = LazyLock::new(|| {
        let mut env = crate::new_env();
        #[cfg(feature = "lists")]
        cel::extensions::lists(&mut env).expect("cel's lists extension fits kube-cel's Env");
        cel::extensions::two_var_comprehensions(&mut env)
            .expect("cel's two-variable comprehensions fit kube-cel's Env");
        Arc::new(env)
    });
    &ENV
}

/// A root context on [`validation_env`] with the validation function set
/// registered (see `Profile::Validation`).
pub(crate) fn base_context() -> Context<'static, 'static> {
    let mut ctx = Context::with_env(Arc::clone(validation_env()));
    crate::functions::register_profile(&mut ctx, crate::functions::Profile::Validation)
        .expect("kube-cel's functions register onto its validation Env");
    ctx
}

/// Compiles a CRD rule, a VAP expression or either's `messageExpression` on
/// [`validation_env`], refusing a call to a function the apiserver does not
/// declare even where evaluation would never reach it, as the apiserver does
/// at registration (see [`references`]).
pub(crate) fn compile(source: &str) -> Result<Program, Box<dyn std::error::Error + Send + Sync>> {
    let program = validation_env().compile(source)?;
    references::check(program.expression())?;
    Ok(program)
}

/// CRD-level context variables available at the root schema node.
///
/// These are derived from the CRD definition, not from the object being validated.
/// Available as root-level CEL variables: `apiVersion`, `apiGroup`, `kind`.
#[derive(Clone, Debug, Default)]
pub struct RootContext {
    /// CRD API version (e.g., `"apps/v1"`).
    pub api_version: String,
    /// CRD API group (e.g., `"apps"`). Empty string for core resources.
    pub api_group: String,
    /// CRD kind (e.g., `"Deployment"`).
    pub kind: String,
}

/// The kind of error that occurred during validation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The rule, or its `messageExpression`, does not compile: a syntax error,
    /// or a call to a function the apiserver's CEL environment does not
    /// declare (`cel.bind`, the `math` library, `base64.*`, kube-cel's
    /// `isIPv4`, a global `min(..)`, ..). The apiserver rejects such a CRD at
    /// registration.
    CompilationFailure,
    /// Malformed rule JSON.
    InvalidRule,
    /// Rule evaluated to `false`.
    ValidationFailure,
    /// Rule returned a non-bool value.
    InvalidResult,
    /// Runtime evaluation error.
    EvaluationError,
    /// Evaluating the rule reached a reference it could not resolve: an
    /// undeclared identifier, a function the apiserver declares but kube-cel
    /// does not implement (`optional.unwrap`, `<list>.unwrapOpt()`, the
    /// `authorizer` library) or implements only behind a disabled feature, or
    /// a function called in a form no overload takes (`ip(..).string()`). A
    /// call to a function the apiserver does not declare at all is a
    /// [`Self::CompilationFailure`] instead. The object is rejected
    /// (fail-closed); distinct from [`Self::EvaluationError`] so callers can
    /// tell a coverage gap apart from a genuine runtime error.
    UnsupportedReference,
    /// Schema nesting exceeded the maximum supported depth.
    /// The over-deep subtree was refused rather than skipped, so validation
    /// fails closed instead of silently passing rules it never evaluated.
    SchemaTooDeep,
}

/// An error produced when a CEL validation rule fails.
///
/// `#[non_exhaustive]`: this is an output type the crate constructs, never the
/// caller. New fields may be added without a breaking change; downstream code
/// reads the public fields and the cause chain via [`std::error::Error::source`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct ValidationError {
    /// The CEL expression that failed.
    pub rule: String,
    /// Human-readable error message.
    pub message: String,
    /// JSON path to the field (e.g., "spec.replicas").
    pub field_path: String,
    /// Machine-readable reason (e.g., "FieldValueInvalid").
    pub reason: Option<String>,
    /// Classification of the error.
    pub kind: ErrorKind,
    /// The underlying cause, exposed via [`std::error::Error::source`].
    ///
    /// `Arc` (not `Box`) so `ValidationError` stays `Clone`; skipped during
    /// serialization because `dyn Error` is not `Serialize`. Carried only for
    /// runtime evaluation failures ([`ErrorKind::EvaluationError`] /
    /// [`ErrorKind::UnsupportedReference`]), where the owned `cel`
    /// `ExecutionError` is the cause. Compile-time causes (parse/JSON errors)
    /// are not chained here: `cel::ParseErrors` is `!Clone` and reached only
    /// behind a shared borrow, so they cannot be moved into an owned
    /// `ValidationError` — their detail is preserved in `message`, and the
    /// typed cause remains reachable via
    /// [`CompiledSchema::compilation_errors`](crate::CompiledSchema::compilation_errors).
    #[serde(skip)]
    source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.field_path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.field_path, self.message)
        }
    }
}

// Manual `PartialEq`/`Eq`: the `source` cause is auxiliary metadata (and
// `dyn Error` is not `PartialEq`), so equality is decided by the data fields.
impl PartialEq for ValidationError {
    fn eq(&self, other: &Self) -> bool {
        self.rule == other.rule
            && self.message == other.message
            && self.field_path == other.field_path
            && self.reason == other.reason
            && self.kind == other.kind
    }
}

impl Eq for ValidationError {}

impl std::error::Error for ValidationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|s| &**s as &(dyn std::error::Error + 'static))
    }
}

/// Aggregate of CEL validation failures.
///
/// The `Err` payload of the Result-returning validation entry points (e.g.
/// [`Validator::validate`]), so callers can `?` instead of checking
/// `Vec::is_empty`. Derefs to `[ValidationError]` and is iterable, so every
/// individual failure stays inspectable; [`into_vec`](Self::into_vec) recovers
/// ownership of the full `Vec`.
///
/// A `ValidationErrors` is never empty: the entry points return `Ok(())` when no
/// rule fails and `Err(ValidationErrors)` only when at least one does.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ValidationErrors(Vec<ValidationError>);

impl ValidationErrors {
    /// Consume the aggregate and return the underlying `Vec`.
    #[must_use]
    pub fn into_vec(self) -> Vec<ValidationError> {
        self.0
    }

    /// The individual failures as a slice.
    #[must_use]
    pub fn as_slice(&self) -> &[ValidationError] {
        &self.0
    }

    /// Whether the aggregate holds no failures. Always `false` for a value
    /// produced by the validation entry points (which return `Ok` in that case),
    /// but meaningful for aggregates built directly via [`From`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Number of individual failures.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl std::fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{} validation error(s):", self.0.len())?;
        for (i, err) in self.0.iter().enumerate() {
            if i + 1 == self.0.len() {
                write!(f, "  - {err}")?;
            } else {
                writeln!(f, "  - {err}")?;
            }
        }
        Ok(())
    }
}

// Aggregate error: the constituent `ValidationError`s are the payload (reachable
// via `Deref`/`IntoIterator`), not a single underlying cause, so `source()` is
// `None`. Mirrors `ValidationError`'s own `Error` impl style.
impl std::error::Error for ValidationErrors {}

impl std::ops::Deref for ValidationErrors {
    type Target = [ValidationError];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<Vec<ValidationError>> for ValidationErrors {
    fn from(errors: Vec<ValidationError>) -> Self {
        Self(errors)
    }
}

impl IntoIterator for ValidationErrors {
    type IntoIter = std::vec::IntoIter<ValidationError>;
    type Item = ValidationError;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a ValidationErrors {
    type IntoIter = std::slice::Iter<'a, ValidationError>;
    type Item = &'a ValidationError;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Convert a collected error `Vec` into the Result shape the entry points return:
/// `Ok(())` when empty, `Err(ValidationErrors)` otherwise.
fn into_result(errors: Vec<ValidationError>) -> Result<(), ValidationErrors> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors(errors))
    }
}

/// Builds the fail-closed error emitted when schema nesting exceeds
/// [`MAX_SCHEMA_DEPTH`](crate::validation::compilation::MAX_SCHEMA_DEPTH). Surfacing this —
/// rather than silently skipping the over-deep subtree — is what keeps the
/// validator fail-closed: a too-deep schema cannot quietly report success.
/// A value the apiserver cannot bind for CEL (see `values::binding_error`).
fn binding_failure(message: String, path: &str) -> ValidationError {
    ValidationError {
        rule: String::new(),
        message,
        field_path: path.to_string(),
        reason: None,
        kind: ErrorKind::EvaluationError,
        source: None,
    }
}

/// A rule (or its `messageExpression`) that did not compile, as a validation
/// error at `path`.
fn compilation_error(err: &CompilationError, path: &str) -> ValidationError {
    match err {
        CompilationError::Parse { rule, source } => ValidationError {
            rule: rule.clone(),
            message: format!("failed to compile rule \"{rule}\": {source}"),
            field_path: path.to_string(),
            reason: None,
            kind: ErrorKind::CompilationFailure,
            // `cel::ParseErrors` is `!Clone` and only borrowed here,
            // so the typed cause cannot be owned; detail is in `message`.
            source: None,
        },
        CompilationError::MessageExpressionParse {
            rule,
            message_expression,
            source,
        } => ValidationError {
            rule: rule.clone(),
            message: format!(
                "failed to compile messageExpression \"{message_expression}\" for rule \"{rule}\": {source}"
            ),
            field_path: path.to_string(),
            reason: None,
            kind: ErrorKind::CompilationFailure,
            source: None,
        },
        CompilationError::InvalidRule(e) => ValidationError {
            rule: String::new(),
            message: format!("invalid rule definition: {e}"),
            field_path: path.to_string(),
            reason: None,
            kind: ErrorKind::InvalidRule,
            // `serde_json::Error` is borrowed and `!Clone`; detail is in `message`.
            source: None,
        },
        CompilationError::SchemaTooDeep { .. } => schema_too_deep_error(path),
    }
}

/// The path of a child schema, for an error found without an object: a
/// property by name, list items as `[*]`, map values as `*`.
fn schema_child_path(path: &str, child: ChildSchema<'_>) -> String {
    match child {
        ChildSchema::Property(name) => join_path(path, name),
        ChildSchema::Items => format!("{path}[*]"),
        ChildSchema::AdditionalProperties => join_path(path, "*"),
        ChildSchema::Branch => path.to_string(),
    }
}

/// The rules the apiserver would refuse when it registers the CRD: every rule
/// and `messageExpression` in the schema that does not compile, at any depth,
/// whatever nodes an object reaches (an omitted property, the items of an empty
/// list). The object walk only compiles the nodes it reaches, so these are
/// checked first. Iterative: the apiserver has no nesting limit, and a deep
/// schema must not overflow the stack.
fn registration_errors(schema: &serde_json::Value) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    registration_errors_from(schema, String::new(), None, &mut errors);
    errors
}

/// [`registration_errors`] from `schema` at `path`, inside the uncorrelatable
/// portion that starts at the list `uncorrelatable` names, if any.
fn registration_errors_from(
    schema: &serde_json::Value,
    path: String,
    uncorrelatable: Option<String>,
    errors: &mut Vec<ValidationError>,
) {
    let mut stack: Vec<(&serde_json::Value, String, Option<String>)> = vec![(schema, path, uncorrelatable)];
    while let Some((node, path, uncorrelatable)) = stack.pop() {
        for result in compile_schema_validations(node) {
            match result {
                Ok(cr) => errors.extend(old_self_error(&cr, uncorrelatable.as_deref(), &path)),
                Err(e) => errors.push(compilation_error(&e, &path)),
            }
        }
        let map_list = node.get("x-kubernetes-list-type").and_then(|t| t.as_str()) == Some("map");
        for (child, sub) in child_schemas(node).into_iter().rev() {
            let uncorrelatable = match child {
                ChildSchema::Items if !map_list => uncorrelatable.clone().or_else(|| Some(path.clone())),
                _ => uncorrelatable.clone(),
            };
            stack.push((sub, schema_child_path(&path, child), uncorrelatable));
        }
    }
}

/// The `oldSelf` rules the apiserver refuses at registration
/// (apiextensions validation.go): a rule reading `oldSelf` inside the items of a
/// list that is not `x-kubernetes-list-type: map` (at any depth below it, since
/// those items never correlate with old ones), and `optionalOldSelf` set on a
/// rule that does not read `oldSelf`. `uncorrelatable` is the path of the
/// outermost such list. (Every map type correlates, so properties never start
/// an uncorrelatable portion.)
fn old_self_error(
    cr: &CompilationResult,
    uncorrelatable: Option<&str>,
    path: &str,
) -> Option<ValidationError> {
    let message = match uncorrelatable {
        Some(list) if cr.is_transition_rule => {
            format!("oldSelf cannot be used on the uncorrelatable portion of the schema within {list:?}")
        }
        _ if !cr.is_transition_rule && cr.rule.optional_old_self.is_some() => {
            "optionalOldSelf may not be set if oldSelf is not used in rule".to_string()
        }
        _ => return None,
    };
    Some(ValidationError {
        rule: cr.rule.rule.clone(),
        message,
        field_path: path.to_string(),
        reason: None,
        kind: ErrorKind::CompilationFailure,
        source: None,
    })
}

/// [`registration_errors`] over a pre-compiled tree. A `SchemaTooDeep` marker
/// is not one: the apiserver has no nesting limit, so it only fails an object
/// that reaches it (in the walk). The rules below it were compiled into the
/// marker, and those that failed are reported here.
/// `uncorrelatable` as in [`registration_errors`]; a compiled list correlates
/// exactly when it kept `x-kubernetes-list-map-keys`, which only a map list
/// does (and a map list must declare them).
fn registration_errors_compiled(
    compiled: &CompiledSchema,
    path: &str,
    uncorrelatable: Option<&str>,
    errors: &mut Vec<ValidationError>,
) {
    // At the depth cap the subtree was not compiled; check it as a raw schema.
    if let Some(subtree) = &compiled.below_cap {
        registration_errors_from(
            subtree,
            path.to_string(),
            uncorrelatable.map(str::to_string),
            errors,
        );
        return;
    }
    for result in &compiled.validations {
        match result {
            Ok(cr) => errors.extend(old_self_error(cr, uncorrelatable, path)),
            Err(CompilationError::SchemaTooDeep { .. }) => {}
            Err(e) => errors.push(compilation_error(e, path)),
        }
    }
    // `properties` is a HashMap; walk it in name order, as `registration_errors`
    // walks the schema, so the errors come out in a stable order.
    let mut properties: Vec<_> = compiled.properties.iter().collect();
    properties.sort_by_key(|(name, _)| name.as_str());
    for (name, child) in properties {
        registration_errors_compiled(
            child,
            &schema_child_path(path, ChildSchema::Property(name)),
            uncorrelatable,
            errors,
        );
    }
    if let Some(items) = &compiled.items {
        let items_uncorrelatable = uncorrelatable.or(compiled.list_map_keys.is_empty().then_some(path));
        registration_errors_compiled(
            items,
            &schema_child_path(path, ChildSchema::Items),
            items_uncorrelatable,
            errors,
        );
    }
    if let Some(additional) = &compiled.additional_properties {
        registration_errors_compiled(
            additional,
            &schema_child_path(path, ChildSchema::AdditionalProperties),
            uncorrelatable,
            errors,
        );
    }
    for branch in compiled
        .all_of
        .iter()
        .chain(&compiled.one_of)
        .chain(&compiled.any_of)
    {
        registration_errors_compiled(branch, path, uncorrelatable, errors);
    }
}

fn schema_too_deep_error(path: &str) -> ValidationError {
    ValidationError {
        rule: String::new(),
        message: format!(
            "schema nesting exceeds the maximum depth of {}",
            crate::validation::compilation::MAX_SCHEMA_DEPTH
        ),
        field_path: path.to_string(),
        reason: None,
        kind: ErrorKind::SchemaTooDeep,
        source: None,
    }
}

/// Validates Kubernetes objects against CRD schema CEL validation rules.
///
/// Walks the OpenAPI schema tree, compiles `x-kubernetes-validations` rules at
/// each node, and evaluates them against the corresponding object values.
///
/// For repeated validation against the same schema, use [`compile_schema`](crate::compile_schema) +
/// [`validate_compiled`](Validator::validate_compiled) to avoid re-compilation.
///
/// # Thread Safety
///
/// `Validator` is `Send` and can be moved across threads.
pub struct Validator {
    base_ctx: Context<'static, 'static>,
}

impl std::fmt::Debug for Validator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Validator").finish()
    }
}

impl Validator {
    /// Create a new `Validator` with all K8s CEL functions pre-registered.
    pub fn new() -> Self {
        Self {
            base_ctx: base_context(),
        }
    }

    /// Validate an object against a CRD schema's CEL validation rules.
    ///
    /// Schema `default`s are applied to the object before the rules run, matching
    /// the apiserver, which defaults during admission before evaluating CEL. A CR
    /// that omits a defaulted field is therefore validated as the apiserver sees
    /// it (with the default in place), not as the raw object.
    ///
    /// Compiles rules on each call. For repeated validation against the same
    /// schema, prefer [`compile_schema`](crate::compile_schema) + [`validate_compiled`](Self::validate_compiled).
    pub fn validate(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
    ) -> Result<(), ValidationErrors> {
        self.validate_with_context(schema, object, old_object, None)
    }

    /// Validate an object against a CRD schema's CEL validation rules, with optional root context.
    ///
    /// Like [`validate`](Self::validate), but also binds `apiVersion`, `apiGroup`, and `kind`
    /// as root-level CEL variables when a [`RootContext`] is provided.
    ///
    /// Schema `default`s are applied to the object before evaluation, matching the
    /// apiserver's admission order (defaults run before CEL rules).
    pub fn validate_with_context(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Result<(), ValidationErrors> {
        let registration = registration_errors(schema);
        if !registration.is_empty() {
            return into_result(registration);
        }
        let defaulted = crate::validation::defaults::apply_defaults(schema, object);
        let defaulted_old = old_object.map(|o| crate::validation::defaults::apply_defaults(schema, o));
        let mut errors = Vec::new();
        self.walk_schema(
            schema,
            &defaulted,
            defaulted_old.as_ref(),
            String::new(),
            &mut errors,
            &self.base_ctx,
            root_ctx,
            0,
        );
        into_result(errors)
    }

    /// Validate an object using a pre-compiled schema tree.
    ///
    /// Use [`compile_schema`](crate::compile_schema) to build the [`CompiledSchema`], then call this
    /// method for each object to validate — rules are compiled only once.
    ///
    /// Schema `default`s (retained on the compiled tree) are applied to the object
    /// before evaluation, matching [`validate`](Self::validate) and the apiserver.
    pub fn validate_compiled(
        &self,
        compiled: &CompiledSchema,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
    ) -> Result<(), ValidationErrors> {
        self.validate_compiled_with_context(compiled, object, old_object, None)
    }

    /// Validate an object using a pre-compiled schema tree, with optional root context.
    ///
    /// Like [`validate_compiled`](Self::validate_compiled), but also binds `apiVersion`,
    /// `apiGroup`, and `kind` as root-level CEL variables when a [`RootContext`] is provided.
    pub fn validate_compiled_with_context(
        &self,
        compiled: &CompiledSchema,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Result<(), ValidationErrors> {
        let mut registration = Vec::new();
        registration_errors_compiled(compiled, "", None, &mut registration);
        if !registration.is_empty() {
            return into_result(registration);
        }
        let defaulted = crate::validation::defaults::apply_defaults_compiled(compiled, object);
        let defaulted_old =
            old_object.map(|o| crate::validation::defaults::apply_defaults_compiled(compiled, o));
        let mut errors = Vec::new();
        self.walk_compiled(
            compiled,
            &defaulted,
            defaulted_old.as_ref(),
            String::new(),
            &mut errors,
            &self.base_ctx,
            root_ctx,
            0,
        );
        into_result(errors)
    }

    /// Validate with schema defaults applied to the object first.
    ///
    /// Alias for [`validate`](Self::validate). As of 0.8, plain `validate` applies
    /// schema defaults itself, so this method is equivalent and retained only for
    /// backward compatibility.
    pub fn validate_with_defaults(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
    ) -> Result<(), ValidationErrors> {
        self.validate(schema, object, old_object)
    }

    /// Validate with schema defaults applied and root context variables bound.
    ///
    /// Alias for [`validate_with_context`](Self::validate_with_context). As of 0.8,
    /// that method applies schema defaults itself, so this is equivalent and
    /// retained only for backward compatibility.
    pub fn validate_with_defaults_and_context(
        &self,
        schema: &serde_json::Value,
        object: &serde_json::Value,
        old_object: Option<&serde_json::Value>,
        root_ctx: Option<&RootContext>,
    ) -> Result<(), ValidationErrors> {
        self.validate_with_context(schema, object, old_object, root_ctx)
    }

    // ── Schema-based walking (compiles on each call) ────────────────

    #[allow(clippy::too_many_arguments)]
    fn walk_schema(
        &self,
        schema: &serde_json::Value,
        value: &serde_json::Value,
        old_value: Option<&serde_json::Value>,
        path: String,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_, '_>,
        root_ctx: Option<&RootContext>,
        depth: usize,
    ) {
        if depth > crate::validation::compilation::MAX_SCHEMA_DEPTH {
            errors.push(schema_too_deep_error(&path));
            return;
        }

        // An old value that is JSON null is no old value: the apiserver's node
        // `oldObj` is nil, so transition rules skip and `optionalOldSelf` binds
        // `optional.none()` (schema/cel/validation.go validateExpressions).
        let old_value = old_value.filter(|o| !o.is_null());
        if let Some(message) = values::binding_error(value, &values::SchemaFormat::from_schema(schema)) {
            errors.push(binding_failure(message, &path));
        }
        let cel_value = json_to_cel_with_schema(value, schema);
        let cel_old = old_value.map(|o| json_to_cel_with_schema(o, schema));
        self.evaluate_validations(
            schema,
            &cel_value,
            cel_old.as_ref(),
            &path,
            errors,
            base_ctx,
            root_ctx,
        );

        if let (Some(properties), Some(obj)) = (
            schema.get("properties").and_then(|p| p.as_object()),
            value.as_object(),
        ) {
            for (prop_name, prop_schema) in properties {
                if let Some(child_value) = obj.get(prop_name) {
                    let child_old = old_value.and_then(|o| o.get(prop_name));
                    let child_path = join_path(&path, prop_name);
                    self.walk_schema(
                        prop_schema,
                        child_value,
                        child_old,
                        child_path,
                        errors,
                        base_ctx,
                        None,
                        depth + 1,
                    );
                }
            }
        }

        if let (Some(items_schema), Some(arr)) = (schema.get("items"), value.as_array()) {
            // Only `x-kubernetes-list-type: map` lists correlate old↔new by key.
            let list_map_keys: Vec<String> =
                if schema.get("x-kubernetes-list-type").and_then(|v| v.as_str()) == Some("map") {
                    schema
                        .get("x-kubernetes-list-map-keys")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
            let correlation = MapListCorrelation::build(&list_map_keys, old_value);
            for (i, item) in arr.iter().enumerate() {
                let old_item = correlation.get(item);
                let item_path = join_path_index(&path, i);
                self.walk_schema(
                    items_schema,
                    item,
                    old_item,
                    item_path,
                    errors,
                    base_ctx,
                    None,
                    depth + 1,
                );
            }
        }

        let preserve_unknown = schema
            .get("x-kubernetes-preserve-unknown-fields")
            .and_then(|v| v.as_bool())
            == Some(true);

        if !preserve_unknown
            && let (Some(additional_schema), Some(obj)) = (
                schema.get("additionalProperties").filter(|a| a.is_object()),
                value.as_object(),
            )
        {
            let known: std::collections::HashSet<&str> = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .map(|p| p.keys().map(|k| k.as_str()).collect())
                .unwrap_or_default();

            for (key, val) in obj {
                if known.contains(key.as_str()) {
                    continue;
                }
                let old_val = old_value.and_then(|o| o.get(key));
                let child_path = join_path(&path, key);
                self.walk_schema(
                    additional_schema,
                    val,
                    old_val,
                    child_path,
                    errors,
                    base_ctx,
                    None,
                    depth + 1,
                );
            }
        }

        // Walk allOf/oneOf/anyOf branches — all treated identically for CEL evaluation
        for keyword in &["allOf", "oneOf", "anyOf"] {
            if let Some(branches) = schema.get(keyword).and_then(|v| v.as_array()) {
                for branch in branches {
                    self.walk_schema(
                        branch,
                        value,
                        old_value,
                        path.clone(),
                        errors,
                        base_ctx,
                        root_ctx,
                        depth + 1,
                    );
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn evaluate_validations(
        &self,
        schema: &serde_json::Value,
        cel_value: &cel::Value,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_, '_>,
        root_ctx: Option<&RootContext>,
    ) {
        let compiled = compile_schema_validations(schema);
        self.evaluate_compiled_results(&compiled, cel_value, cel_old, path, errors, base_ctx, root_ctx);
    }

    // ── CompiledSchema-based walking ────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn walk_compiled(
        &self,
        compiled: &CompiledSchema,
        value: &serde_json::Value,
        old_value: Option<&serde_json::Value>,
        path: String,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_, '_>,
        root_ctx: Option<&RootContext>,
        depth: usize,
    ) {
        if depth > crate::validation::compilation::MAX_SCHEMA_DEPTH {
            errors.push(schema_too_deep_error(&path));
            return;
        }

        // Null old value = no old value, as in `walk_schema`.
        let old_value = old_value.filter(|o| !o.is_null());
        if let Some(message) = values::binding_error(value, &compiled.format) {
            errors.push(binding_failure(message, &path));
        }
        let cel_value = json_to_cel_with_compiled(value, compiled);
        let cel_old = old_value.map(|o| json_to_cel_with_compiled(o, compiled));
        self.evaluate_compiled_results(
            &compiled.validations,
            &cel_value,
            cel_old.as_ref(),
            &path,
            errors,
            base_ctx,
            root_ctx,
        );

        if let Some(obj) = value.as_object() {
            for (prop_name, child_compiled) in &compiled.properties {
                if let Some(child_value) = obj.get(prop_name) {
                    let child_old = old_value.and_then(|o| o.get(prop_name));
                    let child_path = join_path(&path, prop_name);
                    self.walk_compiled(
                        child_compiled,
                        child_value,
                        child_old,
                        child_path,
                        errors,
                        base_ctx,
                        None,
                        depth + 1,
                    );
                }
            }
        }

        if let (Some(items_compiled), Some(arr)) = (&compiled.items, value.as_array()) {
            let correlation = MapListCorrelation::build(&compiled.list_map_keys, old_value);
            for (i, item) in arr.iter().enumerate() {
                let old_item = correlation.get(item);
                let item_path = join_path_index(&path, i);
                self.walk_compiled(
                    items_compiled,
                    item,
                    old_item,
                    item_path,
                    errors,
                    base_ctx,
                    None,
                    depth + 1,
                );
            }
        }

        if !compiled.preserve_unknown_fields
            && let (Some(additional_compiled), Some(obj)) =
                (&compiled.additional_properties, value.as_object())
        {
            for (key, val) in obj {
                if compiled.properties.contains_key(key) {
                    continue;
                }
                let old_val = old_value.and_then(|o| o.get(key));
                let child_path = join_path(&path, key);
                self.walk_compiled(
                    additional_compiled,
                    val,
                    old_val,
                    child_path,
                    errors,
                    base_ctx,
                    None,
                    depth + 1,
                );
            }
        }

        for branch in compiled
            .all_of
            .iter()
            .chain(compiled.one_of.iter())
            .chain(compiled.any_of.iter())
        {
            self.walk_compiled(
                branch,
                value,
                old_value,
                path.clone(),
                errors,
                base_ctx,
                root_ctx,
                depth + 1,
            );
        }
    }

    // ── Shared evaluation logic ─────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn evaluate_compiled_results(
        &self,
        results: &[Result<CompilationResult, CompilationError>],
        cel_value: &cel::Value,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
        base_ctx: &Context<'_, '_>,
        root_ctx: Option<&RootContext>,
    ) {
        // Create a node-level scope once with self/oldSelf bound
        let mut node_ctx = base_ctx.new_inner_scope();
        node_ctx.add_variable_from_value("self", cel_value.clone());
        if let Some(old) = cel_old {
            node_ctx.add_variable_from_value("oldSelf", old.clone());
        }

        if path.is_empty()
            && let Some(rc) = root_ctx
        {
            node_ctx.add_variable_from_value(
                "apiVersion",
                cel::Value::String(std::sync::Arc::new(rc.api_version.clone())),
            );
            node_ctx.add_variable_from_value(
                "apiGroup",
                cel::Value::String(std::sync::Arc::new(rc.api_group.clone())),
            );
            node_ctx
                .add_variable_from_value("kind", cel::Value::String(std::sync::Arc::new(rc.kind.clone())));
        }

        for result in results {
            match result {
                Ok(cr) => {
                    self.evaluate_rule(cr, &node_ctx, cel_old, path, errors);
                }
                Err(e) => errors.push(compilation_error(e, path)),
            }
        }
    }

    fn evaluate_rule(
        &self,
        cr: &CompilationResult,
        node_ctx: &Context<'_, '_>,
        cel_old: Option<&cel::Value>,
        path: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        // Handle transition rules
        if cr.is_transition_rule && cel_old.is_none() && cr.rule.optional_old_self != Some(true) {
            return; // skip transition rule without old value
        }

        // optionalOldSelf: bind `oldSelf` as a CEL optional in a per-rule child
        // scope, overriding the node's raw-old binding. apiserver's
        // optionalOldSelfActivation resolves oldSelf to optional.none() on create
        // and optional.of(old) on update — NOT null/raw (schema/cel/validation.go
        // validationActivationWithOldSelf). The rule uses oldSelf.hasValue()/
        // .value() rather than an `oldSelf == null` check.
        let opt_scope;
        let effective_ctx: &Context<'_, '_> = if cr.rule.optional_old_self == Some(true) {
            opt_scope = {
                let mut s = node_ctx.new_inner_scope();
                let old_self = match cel_old {
                    Some(old) => {
                        cel::Value::Opaque(std::sync::Arc::new(cel::objects::OptionalValue::of(old.clone())))
                    }
                    None => cel::Value::Opaque(std::sync::Arc::new(cel::objects::OptionalValue::none())),
                };
                s.add_variable_from_value("oldSelf", old_self);
                s
            };
            &opt_scope
        } else {
            node_ctx
        };

        let result = cr.program.execute(effective_ctx);
        let error_path = effective_path(path, cr.rule.field_path.as_deref());

        match result {
            Ok(cel::Value::Bool(true)) => {
                // Validation passed
            }
            Ok(cel::Value::Bool(false)) => {
                let message = self.resolve_message(cr, effective_ctx);
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message,
                    field_path: error_path,
                    reason: cr.rule.reason.clone(),
                    kind: ErrorKind::ValidationFailure,
                    source: None,
                });
            }
            Ok(_) => {
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message: format!("rule \"{}\" did not evaluate to bool", cr.rule.rule),
                    field_path: error_path,
                    reason: None,
                    kind: ErrorKind::InvalidResult,
                    source: None,
                });
            }
            Err(e) => {
                // The rule compiled but evaluation failed. Calls to functions
                // the apiserver does not declare were refused at compile time
                // (see `references`); an `UndeclaredReference` here is an
                // undeclared identifier, a declared function kube-cel does not
                // implement, or a function called in a form none of its
                // overloads takes, classified distinctly so callers can tell a
                // coverage gap from a real runtime error. Either way the owned
                // `ExecutionError` is the cause and is preserved via `source()`.
                let (kind, message) = match &e {
                    cel::ExecutionError::UndeclaredReference(name) => (
                        ErrorKind::UnsupportedReference,
                        format!(
                            "rule references '{name}', which kube-cel cannot resolve here (undeclared, \
                             a function kube-cel does not implement, or not callable in this form); \
                             it cannot be evaluated client-side"
                        ),
                    ),
                    _ => (ErrorKind::EvaluationError, format!("rule evaluation error: {e}")),
                };
                errors.push(ValidationError {
                    rule: cr.rule.rule.clone(),
                    message,
                    field_path: error_path,
                    reason: None,
                    kind,
                    source: Some(std::sync::Arc::new(e)),
                });
            }
        }
    }

    /// Resolve the error message: try messageExpression first, fall back to
    /// static message, then default — applying apiserver's sanitisation rules
    /// (see [`resolve_failure_message`]).
    fn resolve_message(&self, cr: &CompilationResult, ctx: &Context<'_, '_>) -> String {
        let expr_result = cr
            .message_program
            .as_ref()
            .and_then(|prog| match prog.execute(ctx) {
                Ok(cel::Value::String(s)) => Some((*s).clone()),
                _ => None,
            });
        resolve_failure_message(expr_result.as_deref(), cr.rule.message.as_deref(), || {
            format!("failed rule: {}", cr.rule.rule.trim())
        })
    }
}

/// Maximum byte length of an evaluated `messageExpression` result, matching
/// apiserver `celconfig.MaxEvaluatedMessageExpressionSizeBytes` (5 KiB).
pub(crate) const MAX_MESSAGE_EXPRESSION_BYTES: usize = 5 * 1024;

/// Resolve a validation-failure message the way the apiserver does.
///
/// `expr_result` is the `messageExpression`'s evaluated value as a string (`None`
/// when there was no messageExpression, it errored, or it produced a non-string).
/// The apiserver trims that result and only uses it when it is non-empty,
/// single-line and within [`MAX_MESSAGE_EXPRESSION_BYTES`]; otherwise it falls
/// back to the trimmed static `message`, then to `default`. Both the CRD path
/// (`evalMessageExpression`) and the VAP validator apply these identical checks,
/// so both call sites share this guard rather than using the raw result.
pub(crate) fn resolve_failure_message(
    expr_result: Option<&str>,
    message: Option<&str>,
    default: impl FnOnce() -> String,
) -> String {
    if let Some(trimmed) = expr_result.map(str::trim)
        && !trimmed.is_empty()
        && trimmed.len() <= MAX_MESSAGE_EXPRESSION_BYTES
        && !trimmed.contains('\n')
    {
        return trimmed.to_string();
    }
    // apiserver's ruleMessageOrDefault checks the *untrimmed* message for
    // emptiness, then trims what it returns.
    match message {
        Some(m) if !m.is_empty() => m.trim().to_string(),
        _ => default(),
    }
}

impl Default for Validator {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    static THREAD_VALIDATOR: Validator = Validator::new();
}

/// Convenience function to validate without creating a [`Validator`] instance.
///
/// Uses a thread-local [`Validator`] to avoid re-registering CEL functions on each call.
///
/// See [`Validator::validate`] for details.
pub fn validate(
    schema: &serde_json::Value,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
) -> Result<(), ValidationErrors> {
    THREAD_VALIDATOR.with(|v| v.validate(schema, object, old_object))
}

/// Convenience function to validate using a pre-compiled schema.
///
/// Uses a thread-local [`Validator`] to avoid re-registering CEL functions on each call.
///
/// See [`Validator::validate_compiled`] for details.
pub fn validate_compiled(
    compiled: &CompiledSchema,
    object: &serde_json::Value,
    old_object: Option<&serde_json::Value>,
) -> Result<(), ValidationErrors> {
    THREAD_VALIDATOR.with(|v| v.validate_compiled(compiled, object, old_object))
}

// ── Path helpers ────────────────────────────────────────────────────

#[inline]
fn effective_path(base_path: &str, rule_field_path: Option<&str>) -> String {
    match rule_field_path {
        Some(fp) if fp.starts_with('.') => format!("{base_path}{fp}"),
        Some(fp) if !base_path.is_empty() => format!("{base_path}.{fp}"),
        Some(fp) => fp.to_string(),
        None => base_path.to_string(),
    }
}

#[inline]
fn join_path(base: &str, segment: &str) -> String {
    if base.is_empty() {
        segment.to_string()
    } else {
        format!("{base}.{segment}")
    }
}

#[inline]
fn join_path_index(base: &str, index: usize) -> String {
    if base.is_empty() {
        format!("[{index}]")
    } else {
        format!("{base}[{index}]")
    }
}

/// One component of an `x-kubernetes-list-map-keys` composite key. Only scalars
/// are keyable (apiserver's `keyStrategy`); a missing key or non-scalar value
/// makes the whole item unkeyable, so it never correlates.
#[derive(PartialEq, Eq, Hash)]
enum KeyPart {
    Bool(bool),
    Int(i64),
    /// A float under a single map key compares by value (`singleKeyStrategy`):
    /// the bits of the value with `-0.0` folded into `0.0`, which Go's `==` on
    /// `float64` map keys treats as equal.
    Float(u64),
    /// A float under several map keys compares by its Go `%f` rendering:
    /// `multiKeyStrategy` joins `fmt.Fprintf("%f")` strings, so floats that
    /// agree to six decimals are the same key there.
    Fixed(String),
    Str(String),
}

/// Composite key of `item` over `keys`, in schema key order. `None` if `item`
/// isn't an object, a key is absent, or any key value is non-scalar — matching
/// `CompositeKeyFor` in apiserver's maplist.go.
fn composite_key(keys: &[String], item: &serde_json::Value) -> Option<Vec<KeyPart>> {
    let obj = item.as_object()?;
    let multi_key = keys.len() > 1;
    let mut parts = Vec::with_capacity(keys.len());
    for key in keys {
        parts.push(match obj.get(key)? {
            serde_json::Value::Bool(b) => KeyPart::Bool(*b),
            serde_json::Value::String(s) => KeyPart::Str(s.clone()),
            // The apiserver decodes a JSON number as an int64 when it is an
            // integer that fits, else as a float64 (so 2^63 and 2^63 + 1 are
            // one key there), and keys on that Go value.
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => KeyPart::Int(i),
                // Rust's `{:.6}` and Go's `%f` round the same way.
                None if multi_key => KeyPart::Fixed(format!("{:.6}", n.as_f64()?)),
                None => KeyPart::Float((n.as_f64()? + 0.0).to_bits()),
            },
            _ => return None, // null / array / object
        });
    }
    Some(parts)
}

/// Correlates new list items to old list items by `x-kubernetes-list-map-keys`,
/// mirroring the apiserver's `MakeMapList` (schema/cel/validation.go). Only
/// `x-kubernetes-list-type: map` lists with declared keys correlate; for every
/// other list (set/atomic/unset) `map_keys` is empty, so [`get`](Self::get)
/// always returns `None`, each item's `oldSelf` is absent, and transition rules
/// are skipped — exactly as the apiserver's `emptyMapList` does.
struct MapListCorrelation<'a> {
    keys: &'a [String],
    old_by_key: std::collections::HashMap<Vec<KeyPart>, &'a serde_json::Value>,
}

impl<'a> MapListCorrelation<'a> {
    fn build(map_keys: &'a [String], old_value: Option<&'a serde_json::Value>) -> Self {
        let mut old_by_key = std::collections::HashMap::new();
        if !map_keys.is_empty()
            && let Some(arr) = old_value.and_then(|o| o.as_array())
        {
            for item in arr {
                if let Some(k) = composite_key(map_keys, item) {
                    old_by_key.entry(k).or_insert(item); // first occurrence wins
                }
            }
        }
        Self {
            keys: map_keys,
            old_by_key,
        }
    }

    /// The old item whose key tuple matches `new_item`, or `None`.
    fn get(&self, new_item: &serde_json::Value) -> Option<&'a serde_json::Value> {
        if self.old_by_key.is_empty() {
            return None;
        }
        let k = composite_key(self.keys, new_item)?;
        self.old_by_key.get(&k).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::compilation::compile_schema;
    use serde_json::json;

    fn make_schema(validations: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "replicas": {"type": "integer"},
                "name": {"type": "string"}
            },
            "x-kubernetes-validations": validations
        })
    }

    /// The validation profile drops kube-cel's `slice`/`flatten`/`distinct`/
    /// `reverse`/`sort`/`lists.range` because the upstream `lists` extension
    /// declares them. If upstream stopped declaring one, the validation env
    /// would silently lose it: pin that each is still declared.
    #[test]
    #[cfg(feature = "lists")]
    fn upstream_lists_declares_the_dropped_names() {
        let env = validation_env();
        for name in ["slice", "flatten", "distinct", "reverse", "sort", "lists.range"] {
            assert!(
                env.function(name).is_some(),
                "upstream lists no longer declares {name}"
            );
        }
    }

    /// Registering the general set onto the validation env must fail: the
    /// exclusions are what make the validation profile register at all.
    #[test]
    #[cfg(feature = "lists")]
    fn general_set_collides_with_the_validation_env() {
        let mut ctx = Context::with_env(Arc::clone(validation_env()));
        assert!(matches!(
            crate::register_all(&mut ctx),
            Err(crate::RegisterError::Rejected { .. })
        ));
    }

    #[test]
    fn validation_passes() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": 3, "name": "app"});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn validation_fails() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "must be non-negative");
        assert_eq!(errors[0].rule, "self.replicas >= 0");
    }

    #[test]
    fn default_message_when_none() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("self.replicas >= 0"));
    }

    #[test]
    fn reason_preserved() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "bad", "reason": "FieldValueInvalid"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors[0].reason.as_deref(), Some("FieldValueInvalid"));
    }

    #[test]
    fn transition_rule_skipped_without_old_object() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn transition_rule_evaluated_with_old_object() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, Some(&old)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "cannot scale down");
    }

    #[test]
    fn transition_rule_passes() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down"}
        ]));
        let obj = json!({"replicas": 5, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        assert!(validate(&schema, &obj, Some(&old)).is_ok());
    }

    // ── item 16: list-item transition correlation (apiserver MakeMapList) ──

    /// `{items: [{name, value}]}` with an item-level transition rule
    /// `self.value >= oldSelf.value`. `list_type` goes on the array node; `None`
    /// omits the marker (atomic default). Map lists add `list-map-keys: [name]`.
    fn list_item_transition_schema(list_type: Option<&str>) -> serde_json::Value {
        let mut arr = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "value": {"type": "integer"}
                },
                "x-kubernetes-validations": [
                    {"rule": "self.value >= oldSelf.value", "message": "value cannot decrease"}
                ]
            }
        });
        if let Some(lt) = list_type {
            arr["x-kubernetes-list-type"] = json!(lt);
            if lt == "map" {
                arr["x-kubernetes-list-map-keys"] = json!(["name"]);
            }
        }
        json!({"type": "object", "properties": {"items": arr}})
    }

    #[test]
    fn map_list_correlates_by_key_not_index() {
        let schema = list_item_transition_schema(Some("map"));
        // Same items, reordered. Index-correlation would pair b↔a and fail;
        // key-correlation pairs each name with itself → all pass.
        let old = json!({"items": [{"name":"a","value":1}, {"name":"b","value":10}]});
        let new = json!({"items": [{"name":"b","value":10}, {"name":"a","value":1}]});
        assert!(validate(&schema, &new, Some(&old)).is_ok());
    }

    #[test]
    fn map_list_transition_fails_on_matched_key() {
        let schema = list_item_transition_schema(Some("map"));
        let old = json!({"items": [{"name":"a","value":5}, {"name":"b","value":10}]});
        // "a" decreased 5→3, reordered so index-correlation would mis-pair.
        let new = json!({"items": [{"name":"b","value":10}, {"name":"a","value":3}]});
        let errors = validate(&schema, &new, Some(&old)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "value cannot decrease");
        assert_eq!(errors[0].field_path, "items[1]"); // "a" is at new index 1
    }

    #[test]
    fn map_list_added_item_skips_transition_rule() {
        let schema = list_item_transition_schema(Some("map"));
        let old = json!({"items": [{"name":"a","value":5}]});
        // "b" is new — no old correlate, so its transition rule is skipped (else
        // `oldSelf.value` on the absent binding would error and fail the object).
        let new = json!({"items": [{"name":"a","value":5}, {"name":"b","value":1}]});
        assert!(validate(&schema, &new, Some(&old)).is_ok());
    }

    #[test]
    fn old_self_in_uncorrelatable_list_items_is_refused() {
        // Items of an atomic (no marker) or set list never correlate with old
        // ones, so the apiserver refuses a rule reading `oldSelf` there when it
        // registers the CRD ("oldSelf cannot be used on the uncorrelatable
        // portion of the schema"), on create and update alike.
        for list_type in [None, Some("set")] {
            let schema = list_item_transition_schema(list_type);
            let old = json!({"items": [{"name":"a","value":5}]});
            let new = json!({"items": [{"name":"a","value":7}]});
            for result in [
                validate(&schema, &new, Some(&old)),
                validate(&schema, &new, None),
                validate_compiled(&compile_schema(&schema), &new, Some(&old)),
            ] {
                let errors = result.unwrap_err();
                assert_eq!(errors.len(), 1, "{list_type:?}: {errors:?}");
                assert_eq!(errors[0].kind, ErrorKind::CompilationFailure);
                assert_eq!(errors[0].field_path, "items[*]");
                assert!(
                    errors[0].message.contains("uncorrelatable"),
                    "{}",
                    errors[0].message
                );
            }
        }
    }

    #[test]
    fn comprehension_variable_named_old_self_makes_a_transition_rule() {
        // As the apiserver's UsesOldSelf: on create the rule is skipped, under
        // optionalOldSelf it is legal, in non-map list items it is refused.
        let root = |rule: serde_json::Value| json!({"type": "object", "x-kubernetes-validations": [rule]});
        assert!(
            validate(
                &root(json!({"rule": "[1].all(oldSelf, oldSelf > 5)"})),
                &json!({}),
                None
            )
            .is_ok()
        );
        let optional = root(json!({"rule": "[1].all(oldSelf, oldSelf > 0)", "optionalOldSelf": true}));
        assert!(validate(&optional, &json!({}), None).is_ok());
        let items = json!({
            "type": "object",
            "properties": {"l": {"type": "array", "items": {
                "type": "integer",
                "x-kubernetes-validations": [{"rule": "[1].all(oldSelf, oldSelf > 0)"}]
            }}}
        });
        assert!(validate(&items, &json!({"l": [1]}), None).is_err());
    }

    #[test]
    fn empty_message_expression_is_unset() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": "string", "x-kubernetes-validations": [
                {"rule": "self == 'x'", "message": "static", "messageExpression": ""}
            ]}}
        });
        assert!(validate(&schema, &json!({}), None).is_ok());
        let errors = validate(&schema, &json!({"a": "y"}), None).unwrap_err();
        assert_eq!(errors[0].message, "static");
    }

    #[test]
    fn optional_old_self_without_old_self_is_refused() {
        for flag in [true, false] {
            let schema = make_schema(json!([{"rule": "self.replicas >= 0", "optionalOldSelf": flag}]));
            let errors = validate(&schema, &json!({"replicas": 1, "name": "app"}), None).unwrap_err();
            assert_eq!(errors[0].kind, ErrorKind::CompilationFailure);
            assert!(
                errors[0].message.contains("optionalOldSelf"),
                "{}",
                errors[0].message
            );
        }
    }

    #[test]
    fn map_list_correlation_compiled_path() {
        // Same by-key correlation via the pre-compiled walk.
        let schema = list_item_transition_schema(Some("map"));
        let compiled = compile_schema(&schema);
        let old = json!({"items": [{"name":"a","value":5}, {"name":"b","value":10}]});
        let new = json!({"items": [{"name":"b","value":10}, {"name":"a","value":3}]});
        let errors = validate_compiled(&compiled, &new, Some(&old)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "items[1]");
    }

    #[test]
    fn map_list_multi_key_floats_compare_as_go_percent_f() {
        // Several map keys → floats compare by `%f` (six decimals), so
        // 1.0000001 and 1.0000002 are the same key and the rule runs.
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["g", "f"],
                    "items": {
                        "type": "object",
                        "properties": {
                            "g": {"type": "string"},
                            "f": {"type": "number"},
                            "v": {"type": "integer"}
                        },
                        "x-kubernetes-validations": [
                            {"rule": "self.v >= oldSelf.v", "message": "v cannot decrease"}
                        ]
                    }
                }
            }
        });
        let old = json!({"items": [{"g": "x", "f": 1.0000001, "v": 5}]});
        let new = json!({"items": [{"g": "x", "f": 1.0000002, "v": 1}]});
        assert!(validate(&schema, &new, Some(&old)).is_err());
        assert!(validate_compiled(&compile_schema(&schema), &new, Some(&old)).is_err());
        // Under a single key the same floats are different keys.
        let mut single = schema.clone();
        single["properties"]["items"]["x-kubernetes-list-map-keys"] = json!(["f"]);
        assert!(validate(&single, &new, Some(&old)).is_ok());
    }

    #[test]
    fn map_list_number_keys_compare_as_go_decodes_them() {
        // Past int64 a JSON number is a float64 in the apiserver, so 2^63 and
        // 2^63 + 1 are one key; and a single float key -0.0 equals 0.0.
        let schema = |keys: serde_json::Value| {
            json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "x-kubernetes-list-type": "map",
                        "x-kubernetes-list-map-keys": keys,
                        "items": {
                            "type": "object",
                            "properties": {"k": {"type": "number"}, "v": {"type": "integer"}},
                            "x-kubernetes-validations": [
                                {"rule": "self.v >= oldSelf.v", "message": "v cannot decrease"}
                            ]
                        }
                    }
                }
            })
        };
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        for (old_k, new_k) in [("9223372036854775808", "9223372036854775809"), ("-0.0", "0.0")] {
            let old = parse(&format!(r#"{{"items": [{{"k": {old_k}, "v": 5}}]}}"#));
            let new = parse(&format!(r#"{{"items": [{{"k": {new_k}, "v": 1}}]}}"#));
            let single = schema(json!(["k"]));
            assert!(validate(&single, &new, Some(&old)).is_err(), "{old_k} -> {new_k}");
            assert!(validate_compiled(&compile_schema(&single), &new, Some(&old)).is_err());
        }
    }

    #[test]
    fn map_list_multi_key_correlation() {
        // Two-key list: correlate by (group, name). A missing key on an item
        // makes it unkeyable (no correlation), so its transition rule is skipped.
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "x-kubernetes-list-type": "map",
                    "x-kubernetes-list-map-keys": ["group", "name"],
                    "items": {
                        "type": "object",
                        "properties": {
                            "group": {"type": "string"},
                            "name": {"type": "string"},
                            "value": {"type": "integer"}
                        },
                        "x-kubernetes-validations": [
                            {"rule": "self.value >= oldSelf.value", "message": "value cannot decrease"}
                        ]
                    }
                }
            }
        });
        let old = json!({"items": [
            {"group":"g","name":"a","value":5},
            {"group":"h","name":"a","value":1}
        ]});
        // Reordered; (h,a) decreased 1→0 must fail, (g,a) 5→5 holds. Index would
        // pair (g,a)↔(h,a) and (h,a)↔(g,a), giving the wrong verdicts.
        let new = json!({"items": [
            {"group":"h","name":"a","value":0},
            {"group":"g","name":"a","value":5}
        ]});
        let errors = validate(&schema, &new, Some(&old)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "items[0]"); // (h,a) at new index 0
    }

    #[test]
    fn nested_property_field_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {
                            "type": "integer",
                            "x-kubernetes-validations": [
                                {"rule": "self >= 0", "message": "must be non-negative"}
                            ]
                        }
                    }
                }
            }
        });
        let obj = json!({"spec": {"replicas": -1}});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.replicas");
        assert_eq!(errors[0].message, "must be non-negative");
    }

    #[test]
    fn array_items_validation() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"}
                        },
                        "x-kubernetes-validations": [
                            {"rule": "self.name.size() > 0", "message": "name required"}
                        ]
                    }
                }
            }
        });
        let obj = json!({
            "items": [
                {"name": "good"},
                {"name": ""},
                {"name": "also-good"}
            ]
        });
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "items[1]");
        assert_eq!(errors[0].message, "name required");
    }

    #[test]
    fn missing_field_not_validated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "optional_field": {
                    "type": "integer",
                    "x-kubernetes-validations": [
                        {"rule": "self >= 0", "message": "must be non-negative"}
                    ]
                }
            }
        });
        let obj = json!({});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn multiple_rules_partial_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "non-negative"},
            {"rule": "self.name.size() > 0", "message": "name required"}
        ]));
        let obj = json!({"replicas": -1, "name": ""});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn compilation_error_reported() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >="}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("failed to compile"));
    }

    #[test]
    fn no_validations_no_errors() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replicas": {"type": "integer"}
            }
        });
        let obj = json!({"replicas": -1});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn display_with_field_path() {
        let err = ValidationError {
            rule: "self >= 0".into(),
            message: "must be non-negative".into(),
            field_path: "spec.replicas".into(),
            reason: None,
            kind: ErrorKind::ValidationFailure,
            source: None,
        };
        assert_eq!(err.to_string(), "spec.replicas: must be non-negative");
    }

    #[test]
    fn display_without_field_path() {
        let err = ValidationError {
            rule: "self >= 0".into(),
            message: "must be non-negative".into(),
            field_path: String::new(),
            reason: None,
            kind: ErrorKind::ValidationFailure,
            source: None,
        };
        assert_eq!(err.to_string(), "must be non-negative");
    }

    #[test]
    fn validator_default() {
        let v = Validator::default();
        let schema = make_schema(json!([{"rule": "self.replicas >= 0"}]));
        let obj = json!({"replicas": 1, "name": "app"});
        assert!(v.validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn additional_properties_walking() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "integer",
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        });
        let obj = json!({"a": 1, "b": -1, "c": 5});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "b");
    }

    // ── Phase 5 tests ───────────────────────────────────────────────

    #[test]
    fn message_expression_produces_dynamic_message() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static fallback",
            "messageExpression": "'replicas is ' + string(self.replicas) + ', must be >= 0'"
        }]));
        let obj = json!({"replicas": -5, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "replicas is -5, must be >= 0");
    }

    #[test]
    fn message_expression_result_is_trimmed() {
        // apiserver TrimSpaces the messageExpression result before using it.
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static fallback",
            "messageExpression": "'  spaced out  '"
        }]));
        let obj = json!({"replicas": -5, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors[0].message, "spaced out");
    }

    #[test]
    fn empty_message_expression_falls_back_to_static() {
        // A messageExpression that evaluates to empty/whitespace-only is rejected
        // and the static message is used instead (apiserver: non-empty required).
        for me in ["''", "'   '"] {
            let schema = make_schema(json!([{
                "rule": "self.replicas >= 0",
                "message": "static fallback",
                "messageExpression": me
            }]));
            let obj = json!({"replicas": -5, "name": "app"});
            let errors = validate(&schema, &obj, None).unwrap_err();
            assert_eq!(errors[0].message, "static fallback", "messageExpression {me}");
        }
    }

    #[test]
    fn multiline_message_expression_falls_back_to_static() {
        // apiserver rejects a messageExpression result containing a line break.
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static fallback",
            "messageExpression": "'line one\\nline two'"
        }]));
        let obj = json!({"replicas": -5, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors[0].message, "static fallback");
    }

    #[test]
    fn invalid_message_expression_fails_closed() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= 0",
            "message": "static message",
            "messageExpression": "invalid >="
        }]));
        // The object *satisfies* the rule, yet a messageExpression that fails to
        // compile fails closed (CompilationFailure) — mirroring the apiserver,
        // which rejects such a CRD at registration — rather than being silently
        // dropped and the rule evaluated with the static message.
        let obj = json!({"replicas": 5, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::CompilationFailure);
    }

    #[test]
    fn optional_old_self_evaluated_on_create() {
        // apiserver binds oldSelf as a CEL optional under optionalOldSelf; on
        // create it is optional.none(), so the ratcheting rule guards with
        // hasValue()/value() (not `oldSelf == null`).
        let schema = make_schema(json!([{
            "rule": "!oldSelf.hasValue() || self.replicas >= oldSelf.value().replicas",
            "message": "cannot scale down",
            "optionalOldSelf": true
        }]));
        // Create (no old object): oldSelf = optional.none() → !hasValue() → passes.
        let obj = json!({"replicas": 1, "name": "app"});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn optional_old_self_with_old_object() {
        let schema = make_schema(json!([{
            "rule": "!oldSelf.hasValue() || self.replicas >= oldSelf.value().replicas",
            "message": "cannot scale down",
            "optionalOldSelf": true
        }]));
        // Update: oldSelf = optional.of(old); scaling 3 → 1 violates the rule.
        let obj = json!({"replicas": 1, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        let errors = validate(&schema, &obj, Some(&old)).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "cannot scale down");
    }

    #[test]
    fn optional_old_self_binds_optional_none_on_create() {
        // Directly pin the optional nature: on create hasValue() is false.
        let schema = make_schema(json!([{
            "rule": "!oldSelf.hasValue()",
            "message": "expected optional.none() on create",
            "optionalOldSelf": true
        }]));
        let obj = json!({"replicas": 1, "name": "app"});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn optional_old_self_binds_optional_of_on_update() {
        // On update oldSelf is optional.of(old) — value() yields the old object.
        let schema = make_schema(json!([{
            "rule": "oldSelf.hasValue() && oldSelf.value().replicas == 3",
            "message": "expected optional.of(old) on update",
            "optionalOldSelf": true
        }]));
        let obj = json!({"replicas": 1, "name": "app"});
        let old = json!({"replicas": 3, "name": "app"});
        assert!(validate(&schema, &obj, Some(&old)).is_ok());
    }

    #[test]
    fn optional_old_self_false_still_skips() {
        let schema = make_schema(json!([{
            "rule": "self.replicas >= oldSelf.replicas",
            "message": "cannot scale down",
            "optionalOldSelf": false
        }]));
        let obj = json!({"replicas": 1, "name": "app"});
        // optionalOldSelf: false → transition rule skipped on create
        assert!(validate(&schema, &obj, None).is_ok());
    }

    /// `{sub}` where `sub` is nullable and carries `rules`.
    fn nullable_sub_schema(rules: serde_json::Value) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "sub": {
                    "type": "object",
                    "nullable": true,
                    "properties": {"a": {"type": "string"}},
                    "x-kubernetes-validations": rules
                }
            }
        })
    }

    #[test]
    fn null_old_value_binds_optional_none() {
        // A JSON-null old value is no old value (apiserver `oldObj == nil`).
        let schema = nullable_sub_schema(json!([{
            "rule": "!oldSelf.hasValue()",
            "message": "oldSelf must be none",
            "optionalOldSelf": true
        }]));
        let obj = json!({"sub": {"a": "x"}});
        let old = json!({"sub": null});
        assert!(validate(&schema, &obj, Some(&old)).is_ok());
        assert!(validate_compiled(&compile_schema(&schema), &obj, Some(&old)).is_ok());
    }

    #[test]
    fn null_old_value_skips_transition_rule() {
        let schema =
            nullable_sub_schema(json!([{"rule": "self.a == oldSelf.a", "message": "a is immutable"}]));
        let obj = json!({"sub": {"a": "x"}});
        let old = json!({"sub": null});
        assert!(validate(&schema, &obj, Some(&old)).is_ok());
        assert!(validate_compiled(&compile_schema(&schema), &obj, Some(&old)).is_ok());
    }

    #[test]
    fn validate_compiled_matches_validate() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= 0", "message": "non-negative"}
                    ],
                    "properties": {
                        "replicas": {"type": "integer"}
                    }
                }
            }
        });
        let obj = json!({"spec": {"replicas": -1}});

        let errors_schema = validate(&schema, &obj, None).unwrap_err();
        let compiled = compile_schema(&schema);
        let errors_compiled = validate_compiled(&compiled, &obj, None).unwrap_err();

        assert_eq!(errors_schema.len(), errors_compiled.len());
        assert_eq!(errors_schema[0].message, errors_compiled[0].message);
        assert_eq!(errors_schema[0].field_path, errors_compiled[0].field_path);
    }

    #[test]
    fn validate_compiled_reuse() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.x > 0", "message": "x must be positive"}
            ],
            "properties": {"x": {"type": "integer"}}
        });
        let compiled = compile_schema(&schema);

        // Validate multiple objects with the same compiled schema
        assert!(validate_compiled(&compiled, &json!({"x": 1}), None).is_ok());
        assert_eq!(
            validate_compiled(&compiled, &json!({"x": -1}), None)
                .unwrap_err()
                .len(),
            1
        );
        assert!(validate_compiled(&compiled, &json!({"x": 5}), None).is_ok());
        assert_eq!(
            validate_compiled(&compiled, &json!({"x": 0}), None)
                .unwrap_err()
                .len(),
            1
        );
    }

    // ── fieldPath override tests ────────────────────────────────────

    #[test]
    fn fieldpath_overrides_auto_path() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.x >= 0", "message": "bad", "fieldPath": ".spec.x"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"x": -1}});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.spec.x");
    }

    #[test]
    fn fieldpath_without_dot() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.name.size() > 0", "message": "bad", "fieldPath": "name"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"name": ""}});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec.name");
    }

    #[test]
    fn fieldpath_at_root() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"type": "integer"}
            },
            "x-kubernetes-validations": [
                {"rule": "self.x >= 0", "message": "bad", "fieldPath": ".spec.x"}
            ]
        });
        let obj = json!({"x": -1});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, ".spec.x");
    }

    #[test]
    fn fieldpath_none_uses_auto() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "x": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.x >= 0", "message": "bad"}
                    ]
                }
            }
        });
        let obj = json!({"spec": {"x": -1}});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].field_path, "spec");
    }

    // ── ErrorKind tests ─────────────────────────────────────────────

    #[test]
    fn error_kind_compilation_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >="}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::CompilationFailure);
    }

    #[test]
    fn error_kind_validation_failure() {
        let schema = make_schema(json!([
            {"rule": "self.replicas >= 0", "message": "must be non-negative"}
        ]));
        let obj = json!({"replicas": -1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::ValidationFailure);
    }

    #[test]
    fn error_kind_evaluation_error() {
        let schema = make_schema(json!([
            {"rule": "self.missing_field > 0"}
        ]));
        let obj = json!({"replicas": 1, "name": "app"});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ErrorKind::EvaluationError);
    }

    // ── allOf/oneOf/anyOf tests ──────────────────────────────────────

    #[test]
    fn all_of_validations_evaluated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"type": "integer"},
                "y": {"type": "integer"}
            },
            "allOf": [
                {
                    "x-kubernetes-validations": [
                        {"rule": "self.x >= 0", "message": "x must be non-negative"}
                    ]
                },
                {
                    "x-kubernetes-validations": [
                        {"rule": "self.y >= 0", "message": "y must be non-negative"}
                    ]
                }
            ]
        });
        let obj = json!({"x": -1, "y": -1});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn one_of_validations_evaluated() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "oneOf": [{
                "x-kubernetes-validations": [
                    {"rule": "self.x != 0", "message": "x must not be zero"}
                ]
            }]
        });
        let obj = json!({"x": 0});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn nested_all_of_properties_walked() {
        let schema = json!({
            "type": "object",
            "allOf": [{
                "properties": {
                    "name": {
                        "type": "string",
                        "x-kubernetes-validations": [
                            {"rule": "self.size() > 0", "message": "name required"}
                        ]
                    }
                }
            }]
        });
        let obj = json!({"name": ""});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn all_of_compiled_matches_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "allOf": [{
                "x-kubernetes-validations": [
                    {"rule": "self.x >= 0", "message": "x must be non-negative"}
                ]
            }]
        });
        let obj = json!({"x": -1});
        let errors_schema = validate(&schema, &obj, None).unwrap_err();
        let compiled = compile_schema(&schema);
        let errors_compiled = validate_compiled(&compiled, &obj, None).unwrap_err();
        assert_eq!(errors_schema.len(), errors_compiled.len());
        assert_eq!(errors_schema[0].message, errors_compiled[0].message);
    }

    // ── x-kubernetes-preserve-unknown-fields tests ──────────────────

    #[test]
    fn preserve_unknown_fields_skips_additional_properties_walk() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-preserve-unknown-fields": true,
            "additionalProperties": {
                "type": "integer",
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        });
        let obj = json!({"unknown_field": -1});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn without_preserve_unknown_fields_additional_properties_still_walked() {
        let schema = json!({
            "type": "object",
            "additionalProperties": {
                "type": "integer",
                "x-kubernetes-validations": [
                    {"rule": "self >= 0", "message": "must be non-negative"}
                ]
            }
        });
        let obj = json!({"unknown_field": -1});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
    }

    // ── x-kubernetes-embedded-resource tests ────────────────────────

    #[test]
    fn embedded_resource_fields_accessible() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-embedded-resource": true,
            "properties": {
                "spec": {"type": "object"}
            },
            "x-kubernetes-validations": [{
                "rule": "self.apiVersion.size() >= 0",
                "message": "apiVersion must exist"
            }]
        });
        let obj = json!({"spec": {}});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn embedded_resource_preserves_existing_fields() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-embedded-resource": true,
            "x-kubernetes-validations": [{
                "rule": "self.apiVersion == 'v1'",
                "message": "wrong version"
            }]
        });
        let obj = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "test"}});
        assert!(validate(&schema, &obj, None).is_ok());
    }

    #[test]
    fn embedded_resource_compiled_path() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-embedded-resource": true,
            "x-kubernetes-validations": [{
                "rule": "self.kind.size() >= 0",
                "message": "kind must exist"
            }]
        });
        let obj = json!({"spec": {}});
        let compiled = compile_schema(&schema);
        assert!(validate_compiled(&compiled, &obj, None).is_ok());
    }

    // ── RootContext tests ────────────────────────────────────────────

    #[test]
    fn root_context_variables_bound() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "x-kubernetes-validations": [{
                "rule": "apiVersion == 'apps/v1'",
                "message": "wrong api version"
            }]
        });
        let obj = json!({"name": "test"});
        let root_ctx = RootContext {
            api_version: "apps/v1".into(),
            api_group: "apps".into(),
            kind: "Deployment".into(),
        };
        assert!(
            Validator::new()
                .validate_with_context(&schema, &obj, None, Some(&root_ctx))
                .is_ok()
        );
    }

    #[test]
    fn root_context_empty_api_group_for_core() {
        let schema = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "x-kubernetes-validations": [{
                "rule": "apiGroup == ''",
                "message": "not core"
            }]
        });
        let obj = json!({"name": "test"});
        let root_ctx = RootContext {
            api_version: "v1".into(),
            api_group: "".into(),
            kind: "Pod".into(),
        };
        assert!(
            Validator::new()
                .validate_with_context(&schema, &obj, None, Some(&root_ctx))
                .is_ok()
        );
    }

    #[test]
    fn validate_without_root_context_still_works() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "x-kubernetes-validations": [{"rule": "self.x >= 0", "message": "bad"}]
        });
        let obj = json!({"x": -1});
        let errors = validate(&schema, &obj, None).unwrap_err();
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn root_context_compiled_path() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}},
            "x-kubernetes-validations": [{
                "rule": "kind == 'MyResource'",
                "message": "wrong kind"
            }]
        });
        let obj = json!({"x": 1});
        let root_ctx = RootContext {
            api_version: "v1".into(),
            api_group: "example.com".into(),
            kind: "MyResource".into(),
        };
        let compiled = crate::validation::compilation::compile_schema(&schema);
        assert!(
            Validator::new()
                .validate_compiled_with_context(&compiled, &obj, None, Some(&root_ctx))
                .is_ok()
        );
    }

    #[test]
    fn validate_with_defaults_fills_missing_then_validates() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replicas": {
                    "type": "integer",
                    "default": 1,
                    "x-kubernetes-validations": [
                        {"rule": "self >= 0", "message": "must be non-negative"}
                    ]
                }
            }
        });
        // As of 0.8, plain validate injects replicas=1 and the rule passes.
        assert!(validate(&schema, &json!({}), None).is_ok());

        // validate_with_defaults is now an alias and behaves identically.
        assert!(
            Validator::new()
                .validate_with_defaults(&schema, &json!({}), None)
                .is_ok()
        );
    }

    #[test]
    fn plain_validate_applies_defaults_closing_fail_open() {
        // kube-rs/kube-cel#9: a defaulted field omitted by the object. The
        // apiserver defaults it in, so `!has(self.x)` is FALSE → REJECT. Plain
        // validate must now match (pre-0.8 it accepted, a fail-open divergence).
        let schema = json!({
            "type": "object",
            "properties": { "x": { "type": "string", "default": "d" } },
            "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
        });
        assert!(
            validate(&schema, &json!({}), None).is_err(),
            "default makes x present → !has(self.x) false → REJECT"
        );

        // Same closure through the compiled path and through nested map values.
        let compiled = crate::validation::compilation::compile_schema(&schema);
        assert!(
            Validator::new()
                .validate_compiled(&compiled, &json!({}), None)
                .is_err()
        );

        let nested = json!({
            "type": "object",
            "properties": {
                "m": {
                    "type": "object",
                    "additionalProperties": {
                        "type": "object",
                        "properties": { "x": { "type": "string", "default": "d" } },
                        "x-kubernetes-validations": [{ "rule": "!has(self.x)", "message": "x must be absent" }]
                    }
                }
            }
        });
        assert!(
            validate(&nested, &json!({ "m": { "a": {} } }), None).is_err(),
            "nested map value defaulted → REJECT (was fail-open even via validate_with_defaults)"
        );
    }

    #[test]
    fn validate_with_defaults_and_context_combined() {
        let schema = json!({
            "type": "object",
            "properties": {
                "replicas": {
                    "type": "integer",
                    "default": 1,
                    "x-kubernetes-validations": [
                        {"rule": "self >= 0", "message": "must be non-negative"}
                    ]
                }
            },
            "x-kubernetes-validations": [
                {"rule": "kind == 'Deployment'", "message": "wrong kind"}
            ]
        });
        let root_ctx = RootContext {
            api_version: "apps/v1".into(),
            api_group: "apps".into(),
            kind: "Deployment".into(),
        };
        // Empty object: defaults fill replicas=1, root context provides kind
        assert!(
            Validator::new()
                .validate_with_defaults_and_context(&schema, &json!({}), None, Some(&root_ctx))
                .is_ok()
        );
    }

    #[test]
    fn validation_error_serializable() {
        let err = ValidationError {
            rule: "self.x >= 0".into(),
            message: "must be non-negative".into(),
            field_path: "spec.x".into(),
            reason: Some("FieldValueInvalid".into()),
            kind: ErrorKind::ValidationFailure,
            source: None,
        };
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["rule"], "self.x >= 0");
        assert_eq!(json["field_path"], "spec.x");
        assert_eq!(json["kind"], "ValidationFailure");
    }
}
