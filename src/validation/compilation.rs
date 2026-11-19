//! Compilation of Kubernetes CRD `x-kubernetes-validations` rules into CEL programs.
//!
//! This module parses validation rules from CRD schemas and compiles them into
//! [`cel::Program`] instances that can be evaluated against resource data.

use std::collections::HashMap;

use cel::Program;

use crate::validation::{Scope, values::SchemaFormat};

/// A single CRD `x-kubernetes-validations` rule.
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    /// The CEL expression to evaluate.
    pub rule: String,
    /// Static error message returned when validation fails.
    #[serde(default)]
    pub message: Option<String>,
    /// CEL expression that produces a dynamic error message.
    #[serde(default)]
    pub message_expression: Option<String>,
    /// Machine-readable reason for the validation failure (e.g. "FieldValueForbidden").
    #[serde(default)]
    pub reason: Option<String>,
    /// JSONPath to the field that caused the failure.
    #[serde(default)]
    pub field_path: Option<String>,
    /// Whether `oldSelf` is optional. When `true`, the transition rule is
    /// evaluated even on create, with `oldSelf` bound as a CEL optional:
    /// `optional.none()` on create, `optional.of(old)` on update.
    #[serde(default)]
    pub optional_old_self: Option<bool>,
}

/// The result of successfully compiling a [`Rule`].
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change.
#[derive(Debug)]
#[non_exhaustive]
pub struct CompilationResult {
    /// The compiled CEL program.
    pub program: Program,
    /// The original rule that was compiled.
    pub rule: Rule,
    /// Whether the rule references `oldSelf` (transition rule).
    pub is_transition_rule: bool,
    /// Pre-compiled `messageExpression` program, or `None` if the rule had no
    /// `messageExpression`. A `messageExpression` that fails to compile yields a
    /// [`CompilationError::MessageExpressionParse`] instead of a `None` here.
    pub message_program: Option<Program>,
}

/// Errors that can occur during rule compilation.
#[derive(Debug)]
#[non_exhaustive]
pub enum CompilationError {
    /// CEL expression failed to compile: it does not parse, or it calls a
    /// function the apiserver does not declare (see
    /// [`ErrorKind::CompilationFailure`](crate::ErrorKind::CompilationFailure)).
    Parse {
        /// The original CEL expression that failed to compile.
        rule: String,
        /// The boxed error: the parse error reported by the CEL compiler, or
        /// the unsupported call. Boxed (rather than carrying the concrete
        /// `cel::ParseErrors`) so the pre-1.0 `cel` type is not frozen into
        /// this public enum variant; reach it via
        /// [`Error::source`](std::error::Error::source).
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A rule's `messageExpression` failed to compile, as for [`Self::Parse`].
    /// The `rule` itself is well-formed, but its dynamic error message cannot
    /// be compiled. Surfaced as a compilation error (rather than silently
    /// dropped) so the rule fails closed, mirroring the apiserver, which
    /// rejects such a CRD at registration.
    MessageExpressionParse {
        /// The rule whose `messageExpression` failed to compile.
        rule: String,
        /// The `messageExpression` that failed to compile.
        message_expression: String,
        /// The boxed error, as for [`Self::Parse`]. Reach it via
        /// [`Error::source`](std::error::Error::source).
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// JSON value could not be deserialized into a [`Rule`].
    InvalidRule(serde_json::Error),
    /// Schema nesting exceeded the maximum depth. The over-deep subtree was
    /// refused rather than silently truncated, so a too-deep schema cannot
    /// quietly drop the validation rules nested beneath the cap (fail-closed).
    SchemaTooDeep {
        /// The nesting depth at which the limit was exceeded.
        depth: usize,
    },
    /// `x-kubernetes-validations` under `allOf`, `anyOf`, `oneOf` or `not`,
    /// at any depth below one. The apiserver refuses such a CRD as not
    /// structural ("must be empty to be structural"); kube-cel reports it
    /// once, at the schema root.
    RuleInCombinator {
        /// Where the rules sit, as a schema path from the root:
        /// `properties[spec].allOf[0]`.
        location: String,
        /// The first rule there, as written.
        rule: String,
    },
    /// A rule field the apiserver refuses at registration, beside the
    /// expressions: a blank or multi-line `message`, a multi-line rule
    /// without a `message`, an unsupported `reason`, or a blank, multi-line
    /// or invalid `fieldPath`.
    InvalidRuleField {
        /// The rule, as written.
        rule: String,
        /// The refused field: `message`, `reason` or `fieldPath`.
        field: String,
        /// Why, in the apiserver's words: "must be non-empty if specified".
        detail: String,
    },
    /// Rules on a schema node the apiserver cannot build a CEL type for: no
    /// `type` (`x-kubernetes-preserve-unknown-fields` alone), or an array
    /// whose `items`, or a map whose `additionalProperties`, has none. The
    /// apiserver refuses the CRD ("failed to construct type information for
    /// x-kubernetes-validations rules"); kube-cel reports it once per node,
    /// at the schema root.
    NoTypeInformation {
        /// Where the rules sit, as a schema path from the root:
        /// `properties[spec].properties[config]`.
        location: String,
        /// The node's first rule, as written.
        rule: String,
    },
}

impl std::fmt::Display for CompilationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompilationError::Parse { rule, source } => {
                write!(f, "failed to compile CEL rule \"{rule}\": {source}")
            }
            CompilationError::MessageExpressionParse {
                rule,
                message_expression,
                source,
            } => {
                write!(
                    f,
                    "failed to compile messageExpression \"{message_expression}\" for rule \"{rule}\": {source}"
                )
            }
            CompilationError::InvalidRule(err) => {
                write!(f, "invalid rule definition: {err}")
            }
            CompilationError::SchemaTooDeep { depth } => {
                write!(
                    f,
                    "schema nesting depth {depth} exceeds the maximum of {MAX_SCHEMA_DEPTH}"
                )
            }
            CompilationError::InvalidRuleField { rule, field, detail } => {
                write!(f, "invalid {field} of rule \"{rule}\": {detail}")
            }
            CompilationError::RuleInCombinator { location, rule } => write!(
                f,
                "{location}.x-kubernetes-validations must be empty to be structural: the apiserver \
                 refuses CEL rules inside allOf, anyOf, oneOf and not (rule \"{rule}\")"
            ),
            CompilationError::NoTypeInformation { location, rule } => write!(
                f,
                "{location}.x-kubernetes-validations: failed to construct type information for \
                 x-kubernetes-validations rules: the apiserver refuses rules on a node without a \
                 type, or on an array or map whose items or values have none (rule \"{rule}\")"
            ),
        }
    }
}

impl std::error::Error for CompilationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CompilationError::Parse { source, .. } => Some(source.as_ref()),
            CompilationError::MessageExpressionParse { source, .. } => Some(source.as_ref()),
            CompilationError::InvalidRule(err) => Some(err),
            CompilationError::SchemaTooDeep { .. }
            | CompilationError::RuleInCombinator { .. }
            | CompilationError::InvalidRuleField { .. }
            | CompilationError::NoTypeInformation { .. } => None,
        }
    }
}

/// Compile a single [`Rule`] into a [`CompilationResult`].
///
/// Returns [`CompilationError::Parse`] if the rule expression is invalid, or
/// [`CompilationError::MessageExpressionParse`] if its `messageExpression` is.
pub(crate) fn compile_rule(rule: &Rule) -> Result<CompilationResult, CompilationError> {
    let program =
        crate::validation::compile(&rule.rule, Scope::Crd).map_err(|source| CompilationError::Parse {
            rule: rule.rule.clone(),
            source,
        })?;
    // Any `oldSelf` identifier, as the apiserver counts it (`UsesOldSelf`).
    let is_transition_rule = crate::validation::references::references_ident(program.expression(), "oldSelf");

    // Compile messageExpression if present. A failure fails closed (the apiserver
    // rejects such a CRD at registration), mirroring the `rule` path above rather
    // than silently dropping the dynamic message.
    // An empty one is unset: the apiserver compiles it only when non-empty.
    let message_program = match rule.message_expression.as_deref().filter(|e| !e.is_empty()) {
        Some(expr) => Some(crate::validation::compile(expr, Scope::Crd).map_err(|source| {
            CompilationError::MessageExpressionParse {
                rule: rule.rule.clone(),
                message_expression: expr.to_string(),
                source,
            }
        })?),
        None => None,
    };

    Ok(CompilationResult {
        program,
        rule: rule.clone(),
        is_transition_rule,
        message_program,
    })
}

/// Extract `x-kubernetes-validations` rules from a schema node and compile them.
///
/// If the schema has no `x-kubernetes-validations` key or it is not an array,
/// returns an empty `Vec`. Each rule is compiled independently — failures in one
/// rule do not prevent others from compiling.
pub(crate) fn compile_schema_validations(
    schema: &serde_json::Value,
) -> Vec<Result<CompilationResult, CompilationError>> {
    let rules = match schema.get("x-kubernetes-validations") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => return Vec::new(),
    };

    rules
        .iter()
        .map(|raw| {
            let rule: Rule = serde_json::from_value(raw.clone()).map_err(CompilationError::InvalidRule)?;
            let compiled = compile_rule(&rule)?;
            match rule_field_error(&rule, schema) {
                Some((field, detail)) => Err(CompilationError::InvalidRuleField {
                    rule: rule.rule.clone(),
                    field: field.to_owned(),
                    detail,
                }),
                None => Ok(compiled),
            }
        })
        .collect()
}

/// Whether the apiserver can build the CEL type of `self` for rules on
/// `schema` (apiserver `common.SchemaDeclType`): an `x-kubernetes-int-or-string`
/// node, a scalar `type`, an object whose `additionalProperties` (a schema, not
/// a boolean) has one, or an array whose `items` has one. A node without `type`
/// has none, `x-kubernetes-preserve-unknown-fields` or not; an object's
/// properties without one are left out of its type, not refused. Iterative, as
/// the apiserver has no nesting limit.
fn has_decl_type(schema: &serde_json::Value) -> bool {
    let mut node = schema;
    loop {
        if node.get("x-kubernetes-int-or-string").and_then(|v| v.as_bool()) == Some(true) {
            return true;
        }
        match node.get("type").and_then(|t| t.as_str()) {
            Some("array") => match node.get("items") {
                Some(items) if items.is_object() => node = items,
                _ => return false,
            },
            Some("object") => match node.get("additionalProperties") {
                Some(values) if values.is_object() => node = values,
                _ => return true,
            },
            Some("string" | "boolean" | "number" | "integer") => return true,
            _ => return false,
        }
    }
}

/// The `reason`s a rule may give (apiextensions `supportedValidationReason`).
const SUPPORTED_REASONS: &[&str] = &[
    "FieldValueDuplicate",
    "FieldValueForbidden",
    "FieldValueInvalid",
    "FieldValueRequired",
];

/// Whether `s` holds a line break, as the apiserver's `hasNewlines` reads one.
fn has_line_break(s: &str) -> bool {
    s.contains(['\n', '\r'])
}

/// The first field of `rule`, on the schema node `schema`, the apiserver
/// refuses at registration beside the expressions, with why (apiextensions
/// validation.go, `x-kubernetes-validations`): a `message` that is blank but
/// not empty, or holds a line break once trimmed; no `message` for a rule
/// that holds a line break once trimmed; a `reason` outside
/// [`SUPPORTED_REASONS`]; a `fieldPath` that is blank but not empty, holds a
/// line break, or names no field of `schema` ([`valid_field_path`]). A blank
/// rule fails to compile instead.
pub(crate) fn rule_field_error(rule: &Rule, schema: &serde_json::Value) -> Option<(&'static str, String)> {
    let message = rule.message.as_deref().unwrap_or_default();
    let trimmed = message.trim();
    if !message.is_empty() && trimmed.is_empty() {
        return Some(("message", "must be non-empty if specified".into()));
    }
    if has_line_break(trimmed) {
        return Some(("message", "must not contain line breaks".into()));
    }
    if has_line_break(rule.rule.trim()) && trimmed.is_empty() {
        return Some((
            "message",
            "message must be specified if rule contains line breaks".into(),
        ));
    }
    if let Some(reason) = rule.reason.as_deref()
        && !SUPPORTED_REASONS.contains(&reason)
    {
        return Some((
            "reason",
            format!("unsupported value {reason:?}: supported values: {SUPPORTED_REASONS:?}"),
        ));
    }
    let field_path = rule.field_path.as_deref().unwrap_or_default();
    if !field_path.is_empty() && field_path.trim().is_empty() {
        return Some(("fieldPath", "must be non-empty if specified".into()));
    }
    if has_line_break(field_path) {
        return Some(("fieldPath", "must not contain line breaks".into()));
    }
    if !field_path.is_empty() && !valid_field_path(field_path, schema) {
        return Some(("fieldPath", "must be a valid path".into()));
    }
    None
}

/// Whether `path` names a field below the schema node `schema`, as the
/// apiserver's `ValidFieldPath` reads it: steps of `.name` or `['name']` (a
/// single-quoted string with backslash escapes), each a declared property of
/// an object, or any key of a map. List items cannot be named.
fn valid_field_path(path: &str, schema: &serde_json::Value) -> bool {
    let tokens = field_path_tokens(path);
    let mut tokens = tokens.into_iter();
    let mut node = schema;
    while let Some(token) = tokens.next() {
        let name = match token {
            "[" => {
                let Some(quoted) = tokens.next() else { return false };
                if quoted.len() < 2 || !quoted.starts_with('\'') || !quoted.ends_with('\'') {
                    return false;
                }
                let Some(name) = unescape_single_quoted(&quoted[1..quoted.len() - 1]) else {
                    return false;
                };
                if tokens.next() != Some("]") {
                    return false;
                }
                name
            }
            "." => match tokens.next() {
                Some(name) => name.to_owned(),
                None => return false,
            },
            _ => return false,
        };
        // As the apiserver's structural schema has them: empty `properties`
        // are none, and a `null` `additionalProperties` is absent, while a
        // boolean one still names any key.
        node = if let Some(properties) = node
            .get("properties")
            .and_then(|p| p.as_object())
            .filter(|p| !p.is_empty())
        {
            match properties.get(&name) {
                Some(child) => child,
                None => return false,
            }
        } else if let Some(additional) = node.get("additionalProperties").filter(|a| !a.is_null()) {
            additional
        } else {
            return false;
        };
    }
    true
}

/// `path` split at `.`, `[` and `]`, each its own token, keeping a
/// single-quoted string whole (up to a quote not after a backslash, or the
/// end): the apiserver's `ValidFieldPath` scanner.
fn field_path_tokens(path: &str) -> Vec<&str> {
    let bytes = path.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        if bytes[i] == b'\'' {
            i += 1;
            while i < bytes.len() && !(bytes[i] == b'\'' && bytes[i - 1] != b'\\') {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
        } else if matches!(bytes[i], b'.' | b'[' | b']') {
            i += 1;
        } else {
            while i < bytes.len() && !matches!(bytes[i], b'.' | b'[' | b']') {
                i += 1;
            }
        }
        tokens.push(&path[start..i]);
    }
    tokens
}

/// The contents of a single-quoted `fieldPath` string, its backslash escapes
/// (`\a \b \f \n \r \t \v \' \\`) replaced; `None` for any other escape.
fn unescape_single_quoted(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('a') => out.push('\u{7}'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('v') => out.push('\u{b}'),
            Some('\'') => out.push('\''),
            Some('\\') => out.push('\\'),
            // `\\.` does not match a line break, nor the end: kept as is.
            Some('\n') => out.push_str("\\\n"),
            None => out.push('\\'),
            Some(_) => return None,
        }
    }
    Some(out)
}

/// A pre-compiled schema tree. Compile once with [`compile_schema`], then
/// validate many objects via [`Validator::validate_compiled`](crate::Validator::validate_compiled).
///
/// # Note
///
/// `CompiledSchema` is not `Clone` because [`cel::Program`] is `!Clone`.
/// Wrap in [`Arc`](std::sync::Arc) for shared ownership across threads.
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change. Read its fields directly or via the
/// accessor methods below.
#[derive(Debug)]
#[non_exhaustive]
pub struct CompiledSchema {
    /// Compiled validation rules at this schema node.
    pub validations: Vec<Result<CompilationResult, CompilationError>>,
    /// Compiled child property schemas.
    pub properties: HashMap<String, CompiledSchema>,
    /// Compiled array items schema.
    pub items: Option<Box<CompiledSchema>>,
    /// Compiled additionalProperties schema.
    pub additional_properties: Option<Box<CompiledSchema>>,
    /// Whether this node is a map type — its `additionalProperties` is a schema
    /// or `true` (i.e. not absent/`false`). Map keys are user-supplied data, not
    /// declared field names, so they are NOT field-name escaped during value
    /// conversion, matching the apiserver's `MapValue` behavior (kube-rs/kube-cel#8).
    pub is_map: bool,
    /// The `format` hint from the schema (e.g., `date-time`, `duration`).
    pub format: SchemaFormat,
    /// Compiled `allOf` branch schemas.
    pub all_of: Vec<CompiledSchema>,
    /// Compiled `oneOf` branch schemas.
    pub one_of: Vec<CompiledSchema>,
    /// Compiled `anyOf` branch schemas.
    pub any_of: Vec<CompiledSchema>,
    /// `maxLength` from the schema (for cost estimation).
    pub max_length: Option<u64>,
    /// `maxItems` from the schema (for cost estimation).
    pub max_items: Option<u64>,
    /// `maxProperties` from the schema (for cost estimation).
    pub max_properties: Option<u64>,
    /// Whether `x-kubernetes-preserve-unknown-fields: true` is set on this node.
    /// It does not stop `additionalProperties` from being walked or defaulted.
    pub preserve_unknown_fields: bool,
    /// Whether `x-kubernetes-embedded-resource: true` is set on this node.
    /// When true, `apiVersion`, `kind`, and `metadata` keys are injected with
    /// defaults if absent during value conversion.
    pub embedded_resource: bool,
    /// The `default` value declared on this schema node, if any. Used by the
    /// defaulting pass to fill a missing field before CEL evaluation, mirroring
    /// the apiserver's admission order (defaults applied before CEL rules run).
    pub default: Option<serde_json::Value>,
    /// `x-kubernetes-list-map-keys`, but populated **only** when this node is an
    /// `x-kubernetes-list-type: map` array. Empty for set/atomic/unset lists.
    /// Drives old↔new item correlation for transition rules: a non-empty value
    /// means items are matched by key (not index), mirroring the apiserver's
    /// `MakeMapList` (schema/cel/validation.go). Empty means no correlation, so
    /// `oldSelf` is absent per item and transition rules are skipped.
    pub list_map_keys: Vec<String>,
    /// Whether `nullable: true` is set on this node. The defaulting pass injects
    /// a `default` into a present-but-`null` field only when the field is **not**
    /// nullable (apiserver `isNonNullableNull`, defaulting/algorithm.go).
    pub nullable: bool,
    /// At the depth cap, the uncompiled subtree below it. The registration
    /// check walks it like a raw schema (the apiserver has no nesting limit),
    /// so the rules there are checked exactly as `validate` checks them.
    pub(crate) below_cap: Option<serde_json::Value>,
    /// The node as the apiserver's `UnstructuredToVal` reads it, for binding
    /// a rule's `self` and `oldSelf` (see `structural`).
    pub(crate) shape: std::sync::Arc<crate::validation::structural::Shape>,
}

impl CompiledSchema {
    /// Returns references to all compilation errors in this node's validations.
    #[must_use]
    pub fn compilation_errors(&self) -> Vec<&CompilationError> {
        self.validations.iter().filter_map(|r| r.as_ref().err()).collect()
    }

    /// Returns `true` if any validation rule at this node failed to compile.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.validations.iter().any(|r| r.is_err())
    }
}

/// Maximum schema nesting depth to prevent unbounded recursion.
///
/// Shared across compile (`compilation`), validate (`validation`), and default
/// injection (`defaults`) so the three depth guards stay in lockstep.
pub(crate) const MAX_SCHEMA_DEPTH: usize = 64;

fn compile_schema_array(schema: &serde_json::Value, key: &str, depth: usize) -> Vec<CompiledSchema> {
    schema
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|s| compile_schema_inner(s, depth)).collect())
        .unwrap_or_default()
}

/// Where a child schema sits under its parent, for naming its path.
#[derive(Clone, Copy)]
pub(crate) enum ChildSchema<'a> {
    /// `properties.<name>`
    Property(&'a str),
    /// `items`
    Items,
    /// `additionalProperties`, when it is a schema
    AdditionalProperties,
    /// an `allOf` / `oneOf` / `anyOf` branch, which applies to the same value
    Branch,
}

/// The subschemas the validators descend into, in schema order.
pub(crate) fn child_schemas(schema: &serde_json::Value) -> Vec<(ChildSchema<'_>, &serde_json::Value)> {
    let mut children = Vec::new();
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        children.extend(
            props
                .iter()
                .map(|(name, s)| (ChildSchema::Property(name.as_str()), s)),
        );
    }
    if let Some(items) = schema.get("items") {
        children.push((ChildSchema::Items, items));
    }
    if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
        children.push((ChildSchema::AdditionalProperties, additional));
    }
    for keyword in ["allOf", "oneOf", "anyOf"] {
        if let Some(branches) = schema.get(keyword).and_then(|b| b.as_array()) {
            children.extend(branches.iter().map(|b| (ChildSchema::Branch, b)));
        }
    }
    children
}

/// Recursively compile all `x-kubernetes-validations` rules in a schema tree.
///
/// Returns a [`CompiledSchema`] that can be reused across multiple validation
/// calls, avoiding repeated compilation.
#[must_use]
pub fn compile_schema(schema: &serde_json::Value) -> CompiledSchema {
    let mut compiled = compile_schema_inner(schema, 0);
    compiled.validations.extend(
        untyped_rule_errors(schema)
            .into_iter()
            .chain(combinator_rule_errors(schema))
            .map(Err),
    );
    compiled
}

/// The nodes holding `x-kubernetes-validations` the apiserver cannot build a
/// CEL type for ([`has_decl_type`]), which it refuses at registration
/// (apiextensions `CELSchemaContext.TypeInfo`: "failed to construct type
/// information for x-kubernetes-validations rules"). An empty list is fine.
/// The apiserver skips the check where a node or one below it has a schema
/// error, so nodes under a combinator are left to [`combinator_rule_errors`].
/// One error per node, in schema order. Iterative, as the apiserver has no
/// nesting limit.
pub(crate) fn untyped_rule_errors(schema: &serde_json::Value) -> Vec<CompilationError> {
    let mut errors = Vec::new();
    let mut stack = vec![(schema, String::new())];
    while let Some((node, location)) = stack.pop() {
        if let Some(rules) = node
            .get("x-kubernetes-validations")
            .and_then(|r| r.as_array())
            .filter(|r| !r.is_empty())
            && !has_decl_type(node)
        {
            errors.push(CompilationError::NoTypeInformation {
                location: location.clone(),
                rule: rules[0]
                    .get("rule")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        let at = |step: String| {
            if location.is_empty() {
                step
            } else {
                format!("{location}.{step}")
            }
        };
        let mut children = Vec::new();
        if let Some(properties) = node.get("properties").and_then(|p| p.as_object()) {
            for (name, sub) in properties {
                children.push((sub, at(format!("properties[{name}]"))));
            }
        }
        if let Some(sub) = node.get("items").filter(|i| i.is_object()) {
            children.push((sub, at("items".to_owned())));
        }
        if let Some(sub) = node.get("additionalProperties").filter(|a| a.is_object()) {
            children.push((sub, at("additionalProperties".to_owned())));
        }
        // Reversed, so the stack pops them in schema order.
        stack.extend(children.into_iter().rev());
    }
    errors
}

/// The `x-kubernetes-validations` under `allOf`, `anyOf`, `oneOf` and `not`
/// anywhere in `schema`, which the apiserver refuses as not structural
/// (apiextensions `validateValueValidation`: "must be empty to be
/// structural"), at any depth below a combinator: in its `properties`,
/// `items`, `additionalProperties` and nested combinators too. An empty list
/// is fine. One error per schema node, in schema order. Iterative, as the
/// apiserver has no nesting limit.
pub(crate) fn combinator_rule_errors(schema: &serde_json::Value) -> Vec<CompilationError> {
    let mut errors = Vec::new();
    // (node, its schema path, whether it sits under a combinator)
    let mut stack = vec![(schema, String::new(), false)];
    while let Some((node, location, under_combinator)) = stack.pop() {
        if under_combinator
            && let Some(rules) = node
                .get("x-kubernetes-validations")
                .and_then(|r| r.as_array())
                .filter(|r| !r.is_empty())
        {
            errors.push(CompilationError::RuleInCombinator {
                location: location.clone(),
                rule: rules[0]
                    .get("rule")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_owned(),
            });
        }
        let at = |step: String| {
            if location.is_empty() {
                step
            } else {
                format!("{location}.{step}")
            }
        };
        let mut children = Vec::new();
        if let Some(properties) = node.get("properties").and_then(|p| p.as_object()) {
            for (name, sub) in properties {
                children.push((sub, at(format!("properties[{name}]")), under_combinator));
            }
        }
        match node.get("items") {
            Some(serde_json::Value::Array(items)) => {
                for (i, sub) in items.iter().enumerate() {
                    children.push((sub, at(format!("items[{i}]")), under_combinator));
                }
            }
            Some(sub) => children.push((sub, at("items".to_owned()), under_combinator)),
            None => {}
        }
        if let Some(sub) = node.get("additionalProperties").filter(|a| a.is_object()) {
            children.push((sub, at("additionalProperties".to_owned()), under_combinator));
        }
        for keyword in ["allOf", "anyOf", "oneOf"] {
            if let Some(branches) = node.get(keyword).and_then(|b| b.as_array()) {
                for (i, sub) in branches.iter().enumerate() {
                    children.push((sub, at(format!("{keyword}[{i}]")), true));
                }
            }
        }
        if let Some(sub) = node.get("not") {
            children.push((sub, at("not".to_owned()), true));
        }
        // Reversed, so the stack pops them in schema order.
        stack.extend(children.into_iter().rev());
    }
    errors
}

fn compile_schema_inner(schema: &serde_json::Value, depth: usize) -> CompiledSchema {
    if depth > MAX_SCHEMA_DEPTH {
        // Fail-closed: carry a SchemaTooDeep marker rather than returning a
        // silently-empty node, so `compilation_errors()` and the validators
        // surface the truncation instead of dropping the deep rules. The
        // subtree is kept, so the registration check still sees every rule
        // below the cap (the apiserver has no nesting limit of its own).
        return CompiledSchema {
            validations: vec![Err(CompilationError::SchemaTooDeep { depth })],
            below_cap: Some(schema.clone()),
            shape: crate::validation::structural::Shape::from_json(schema),
            properties: HashMap::new(),
            items: None,
            additional_properties: None,
            is_map: false,
            format: SchemaFormat::None,
            all_of: Vec::new(),
            one_of: Vec::new(),
            any_of: Vec::new(),
            max_length: None,
            max_items: None,
            max_properties: None,
            preserve_unknown_fields: false,
            embedded_resource: false,
            default: None,
            list_map_keys: Vec::new(),
            nullable: false,
        };
    }

    let validations = compile_schema_validations(schema);

    let mut properties: HashMap<String, CompiledSchema> = HashMap::new();
    if let Some(props) = schema.get("properties").and_then(|p| p.as_object()) {
        for (name, prop_schema) in props {
            properties.insert(name.clone(), compile_schema_inner(prop_schema, depth + 1));
        }
    }

    let items = schema
        .get("items")
        .map(|s| Box::new(compile_schema_inner(s, depth + 1)));

    let additional = schema.get("additionalProperties");
    let additional_properties = additional
        .filter(|a| a.is_object())
        .map(|s| Box::new(compile_schema_inner(s, depth + 1)));
    // A node whose `additionalProperties` is a schema or `true` (not absent/`false`)
    // is a map; its keys are user data and must not be field-name escaped (#8).
    let is_map = !matches!(additional, None | Some(serde_json::Value::Bool(false)));

    let format = SchemaFormat::from_schema(schema);

    let all_of = compile_schema_array(schema, "allOf", depth + 1);
    let one_of = compile_schema_array(schema, "oneOf", depth + 1);
    let any_of = compile_schema_array(schema, "anyOf", depth + 1);

    let max_length = schema.get("maxLength").and_then(|v| v.as_u64());
    let max_items = schema.get("maxItems").and_then(|v| v.as_u64());
    let max_properties = schema.get("maxProperties").and_then(|v| v.as_u64());

    let preserve_unknown_fields = schema
        .get("x-kubernetes-preserve-unknown-fields")
        .and_then(|v| v.as_bool())
        == Some(true);

    let embedded_resource = schema
        .get("x-kubernetes-embedded-resource")
        .and_then(|v| v.as_bool())
        == Some(true);

    // A `default: null` is no default (the apiserver skips nil defaults).
    let default = schema.get("default").filter(|d| !d.is_null()).cloned();

    let nullable = schema.get("nullable").and_then(serde_json::Value::as_bool) == Some(true);

    // Only `x-kubernetes-list-type: map` lists correlate old↔new items by key.
    let list_map_keys = if schema.get("x-kubernetes-list-type").and_then(|v| v.as_str()) == Some("map") {
        schema
            .get("x-kubernetes-list-map-keys")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|k| k.as_str().map(String::from)).collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    // Built from the children's shapes, so the tree is walked once.
    let shape = crate::validation::structural::Shape::node(
        schema,
        Some(
            properties
                .iter()
                .map(|(name, child)| (name.clone(), std::sync::Arc::clone(&child.shape)))
                .collect(),
        ),
        items
            .as_ref()
            .filter(|_| schema.get("items").is_some_and(serde_json::Value::is_object))
            .map(|c| std::sync::Arc::clone(&c.shape)),
        additional_properties
            .as_ref()
            .map(|c| std::sync::Arc::clone(&c.shape)),
    );

    CompiledSchema {
        validations,
        properties,
        items,
        additional_properties,
        is_map,
        format,
        all_of,
        one_of,
        any_of,
        max_length,
        max_items,
        max_properties,
        preserve_unknown_fields,
        embedded_resource,
        default,
        list_map_keys,
        nullable,
        below_cap: None,
        shape,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule fields the apiserver refuses at registration beside the
    /// expressions (checked on the reference apiserver; parity rows
    /// `PruleField*`).
    #[test]
    fn rule_field_errors_follow_the_apiserver() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "integer"},
                "m": {"type": "object", "additionalProperties": {"type": "object", "properties": {"x": {"type": "integer"}}}},
                "l": {"type": "array", "items": {"type": "integer"}},
                // Empty `properties` are none (`NewStructural`), so the map applies.
                "emptyProps": {"type": "object", "properties": {}, "additionalProperties": {"type": "object", "properties": {"x": {"type": "integer"}}}},
                // A `null` `additionalProperties` is absent; a boolean one names any key.
                "nullMap": {"type": "object", "additionalProperties": null},
                "closedMap": {"type": "object", "additionalProperties": false},
                "openMap": {"type": "object", "additionalProperties": true}
            }
        });
        let rule = |json: serde_json::Value| -> Rule { serde_json::from_value(json).unwrap() };
        let refused = [
            (serde_json::json!({"rule": "true", "message": "   "}), "message"),
            (serde_json::json!({"rule": "true", "message": "a\nb"}), "message"),
            (serde_json::json!({"rule": "true", "message": "a\rb"}), "message"),
            (serde_json::json!({"rule": "true &&\ntrue"}), "message"),
            (
                serde_json::json!({"rule": "true", "reason": "FieldValueTooLong"}),
                "reason",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": "  "}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".a\n"}),
                "fieldPath",
            ),
            (serde_json::json!({"rule": "true", "fieldPath": "a"}), "fieldPath"),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".nope"}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".l[0]"}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".m.k.nope"}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": "['a"}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": "['\\q']"}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".a."}),
                "fieldPath",
            ),
            (
                serde_json::json!({"rule": "true", "fieldPath": ".nullMap.k"}),
                "fieldPath",
            ),
        ];
        for (json, field) in refused {
            let got = rule_field_error(&rule(json.clone()), &schema);
            assert_eq!(got.map(|(f, _)| f), Some(field), "{json}");
        }
        for json in [
            serde_json::json!({"rule": "true", "message": ""}),
            serde_json::json!({"rule": "true", "message": " x "}),
            serde_json::json!({"rule": "true", "message": "a\n"}),
            serde_json::json!({"rule": "true &&\ntrue", "message": "m"}),
            serde_json::json!({"rule": "true", "reason": "FieldValueForbidden"}),
            serde_json::json!({"rule": "true", "fieldPath": ".a"}),
            serde_json::json!({"rule": "true", "fieldPath": "['a']"}),
            serde_json::json!({"rule": "true", "fieldPath": ".m.anykey.x"}),
            serde_json::json!({"rule": "true", "fieldPath": ".m['a.b'].x"}),
            serde_json::json!({"rule": "true", "fieldPath": ".l"}),
            serde_json::json!({"rule": "true", "fieldPath": ".emptyProps.k.x"}),
            serde_json::json!({"rule": "true", "fieldPath": ".emptyProps['k']"}),
            serde_json::json!({"rule": "true", "fieldPath": ".closedMap.k"}),
            serde_json::json!({"rule": "true", "fieldPath": ".openMap.k"}),
        ] {
            assert_eq!(rule_field_error(&rule(json.clone()), &schema), None, "{json}");
        }
    }
    use serde_json::json;

    #[test]
    fn compile_simple_rule() {
        let rule = Rule {
            rule: "self.replicas >= 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(!result.is_transition_rule);
    }

    #[test]
    fn detect_transition_rule() {
        let rule = Rule {
            rule: "self.replicas >= oldSelf.replicas".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.is_transition_rule);
    }

    #[test]
    fn detect_non_transition_rule() {
        let rule = Rule {
            rule: "self.replicas > 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(!result.is_transition_rule);
    }

    #[test]
    fn parse_error_on_invalid_cel() {
        let rule = Rule {
            rule: "self.replicas >=".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let err = compile_rule(&rule).unwrap_err();
        assert!(matches!(err, CompilationError::Parse { .. }));
        // Display should contain the rule text
        let msg = err.to_string();
        assert!(msg.contains("self.replicas >="));
    }

    #[test]
    fn deserialize_rule_all_fields() {
        let raw = json!({
            "rule": "self.x > 0",
            "message": "x must be positive",
            "messageExpression": "\"x is \" + string(self.x)",
            "reason": "FieldValueInvalid",
            "fieldPath": ".spec.x",
            "optionalOldSelf": true
        });
        let rule: Rule = serde_json::from_value(raw).unwrap();
        assert_eq!(rule.rule, "self.x > 0");
        assert_eq!(rule.message.as_deref(), Some("x must be positive"));
        assert_eq!(
            rule.message_expression.as_deref(),
            Some("\"x is \" + string(self.x)")
        );
        assert_eq!(rule.reason.as_deref(), Some("FieldValueInvalid"));
        assert_eq!(rule.field_path.as_deref(), Some(".spec.x"));
        assert_eq!(rule.optional_old_self, Some(true));
    }

    #[test]
    fn deserialize_rule_minimal() {
        let raw = json!({"rule": "self.x > 0"});
        let rule: Rule = serde_json::from_value(raw).unwrap();
        assert_eq!(rule.rule, "self.x > 0");
        assert!(rule.message.is_none());
        assert!(rule.message_expression.is_none());
        assert!(rule.reason.is_none());
        assert!(rule.field_path.is_none());
        assert!(rule.optional_old_self.is_none());
    }

    #[test]
    fn schema_validations_extracts_and_compiles() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.replicas >= 0", "message": "must be non-negative"},
                {"rule": "self.name.size() > 0"}
            ]
        });
        let results = compile_schema_validations(&schema);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_ok());
    }

    #[test]
    fn schema_validations_no_key() {
        let schema = json!({"type": "object"});
        let results = compile_schema_validations(&schema);
        assert!(results.is_empty());
    }

    #[test]
    fn schema_validations_empty_array() {
        let schema = json!({
            "x-kubernetes-validations": []
        });
        let results = compile_schema_validations(&schema);
        assert!(results.is_empty());
    }

    #[test]
    fn message_expression_compiled() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: Some("x must be positive".into()),
            message_expression: Some("'x is ' + string(self.x)".into()),
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.message_program.is_some());
    }

    #[test]
    fn message_expression_invalid_rejected() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: Some("fallback".into()),
            message_expression: Some("invalid >=".into()),
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        // A messageExpression that fails to compile must fail closed (mirroring
        // the rule path + the apiserver, which rejects such a CRD at
        // registration), not be silently dropped with a fall-back to the static
        // message.
        let err = compile_rule(&rule).unwrap_err();
        assert!(
            matches!(err, CompilationError::MessageExpressionParse { .. }),
            "expected MessageExpressionParse, got {err:?}"
        );
    }

    #[test]
    fn message_expression_none() {
        let rule = Rule {
            rule: "self.x > 0".into(),
            message: None,
            message_expression: None,
            reason: None,
            field_path: None,
            optional_old_self: None,
        };
        let result = compile_rule(&rule).unwrap();
        assert!(result.message_program.is_none());
    }

    #[test]
    fn compile_schema_tree() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [{"rule": "has(self.spec)"}],
            "properties": {
                "spec": {
                    "type": "object",
                    "x-kubernetes-validations": [{"rule": "self.replicas >= 0"}],
                    "properties": {
                        "replicas": {"type": "integer"}
                    }
                }
            }
        });
        let compiled = compile_schema(&schema);
        assert_eq!(compiled.validations.len(), 1);
        assert!(compiled.properties.contains_key("spec"));
        let spec = &compiled.properties["spec"];
        assert_eq!(spec.validations.len(), 1);
        assert!(spec.properties.contains_key("replicas"));
    }

    #[test]
    fn compile_schema_with_items() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "x-kubernetes-validations": [{"rule": "self.name.size() > 0"}]
            }
        });
        let compiled = compile_schema(&schema);
        assert!(compiled.items.is_some());
        assert_eq!(compiled.items.as_ref().unwrap().validations.len(), 1);
    }

    #[test]
    fn compile_schema_empty() {
        let schema = json!({"type": "object"});
        let compiled = compile_schema(&schema);
        assert!(compiled.validations.is_empty());
        assert!(compiled.properties.is_empty());
        assert!(compiled.items.is_none());
        assert!(compiled.additional_properties.is_none());
    }

    #[test]
    fn schema_validations_partial_errors() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.y >="},
                {"rule": "self.z == true"}
            ]
        });
        let results = compile_schema_validations(&schema);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
        assert!(results[2].is_ok());
    }

    #[test]
    fn compilation_errors_method() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.y >="},
                {"rule": "self.z == true"}
            ]
        });
        let compiled = compile_schema(&schema);
        let errors = compiled.compilation_errors();
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], CompilationError::Parse { .. }));
        assert!(compiled.has_errors());
    }

    #[test]
    fn compilation_errors_empty_when_all_valid() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.x > 0"},
                {"rule": "self.z == true"}
            ]
        });
        let compiled = compile_schema(&schema);
        assert!(compiled.compilation_errors().is_empty());
        assert!(!compiled.has_errors());
    }
}

/// White-box end-to-end tests of `compile_schema` + `CompilationResult` that
/// bind `self` via the now-internal `json_to_cel`. Relocated from
/// `tests/compilation_integration.rs` when `json_to_cel` became `pub(crate)`.
#[cfg(test)]
mod end_to_end_tests {
    use cel::Value;
    use serde_json::json;

    use super::{CompilationError, compile_schema};
    use crate::validation::values::json_to_cel;

    /// Compile rules from a schema, bind `self`, evaluate the first program.
    fn compile_and_eval_first(schema: serde_json::Value, self_val: serde_json::Value) -> Value {
        let compiled = compile_schema(&schema);
        let cr = compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&self_val));
        cr.program.execute(&ctx).unwrap()
    }

    #[test]
    fn crd_schema_end_to_end() {
        let schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {"type": "integer"},
                        "minReplicas": {"type": "integer"}
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= self.minReplicas", "message": "replicas must be >= minReplicas"}
                    ]
                }
            }
        });

        let spec_schema = &schema["properties"]["spec"];
        let self_val = json!({"replicas": 5, "minReplicas": 2});

        let spec_compiled = compile_schema(spec_schema);
        assert_eq!(spec_compiled.validations.len(), 1);
        let compiled = spec_compiled.validations.into_iter().next().unwrap().unwrap();

        assert!(!compiled.is_transition_rule);
        assert_eq!(
            compiled.rule.message.as_deref(),
            Some("replicas must be >= minReplicas")
        );

        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&self_val));
        assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));
    }

    #[test]
    fn compile_and_eval_with_json_to_cel() {
        let schema = json!({
            "x-kubernetes-validations": [{"rule": "self.name.size() > 0", "message": "name required"}]
        });
        let result = compile_and_eval_first(schema, json!({"name": "my-app"}));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn transition_rule_compile_and_eval() {
        let schema = json!({
            "x-kubernetes-validations": [
                {"rule": "self.replicas >= oldSelf.replicas", "message": "cannot scale down", "reason": "FieldValueForbidden"}
            ]
        });

        let compiled_schema = compile_schema(&schema);
        let compiled = compiled_schema.validations.into_iter().next().unwrap().unwrap();

        assert!(compiled.is_transition_rule);
        assert_eq!(compiled.rule.message.as_deref(), Some("cannot scale down"));
        assert_eq!(compiled.rule.reason.as_deref(), Some("FieldValueForbidden"));

        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&json!({"replicas": 5})));
        ctx.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
        assert_eq!(compiled.program.execute(&ctx).unwrap(), Value::Bool(true));

        let mut ctx2 = crate::validation::base_context();
        ctx2.add_variable_from_value("self", json_to_cel(&json!({"replicas": 1})));
        ctx2.add_variable_from_value("oldSelf", json_to_cel(&json!({"replicas": 3})));
        assert_eq!(compiled.program.execute(&ctx2).unwrap(), Value::Bool(false));
    }

    #[test]
    fn message_and_reason_preserved() {
        let schema = json!({
            "type": "object",
            "properties": { "spec": { "type": "object", "properties": { "x": { "type": "integer" } } } },
            "x-kubernetes-validations": [{
                "rule": "self.x > 0",
                "message": "x must be positive",
                "messageExpression": "\"x is \" + string(self.x)",
                "reason": "FieldValueInvalid",
                "fieldPath": ".spec.x"
            }]
        });

        let compiled_schema = compile_schema(&schema);
        let compiled = compiled_schema.validations.into_iter().next().unwrap().unwrap();

        assert_eq!(compiled.rule.message.as_deref(), Some("x must be positive"));
        assert_eq!(
            compiled.rule.message_expression.as_deref(),
            Some("\"x is \" + string(self.x)")
        );
        assert_eq!(compiled.rule.reason.as_deref(), Some("FieldValueInvalid"));
        assert_eq!(compiled.rule.field_path.as_deref(), Some(".spec.x"));
    }

    #[test]
    fn multiple_rules_mixed_results() {
        let schema = json!({
            "type": "object",
            "x-kubernetes-validations": [
                {"rule": "self.a > 0"},
                {"rule": "invalid >="},
                {"rule": "self.b == true"}
            ]
        });

        let compiled = compile_schema(&schema);
        assert_eq!(compiled.validations.len(), 3);

        let cr = compiled.validations[0].as_ref().unwrap();
        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value("self", json_to_cel(&json!({"a": 5})));
        assert_eq!(cr.program.execute(&ctx).unwrap(), Value::Bool(true));

        assert!(matches!(
            compiled.validations[1].as_ref().unwrap_err(),
            CompilationError::Parse { .. }
        ));

        assert!(compiled.validations[2].is_ok());
    }

    #[test]
    fn realistic_crd_with_multiple_validation_levels() {
        let crd_schema = json!({
            "type": "object",
            "properties": {
                "spec": {
                    "type": "object",
                    "properties": {
                        "replicas": {"type": "integer"},
                        "template": {
                            "type": "object",
                            "properties": {"name": {"type": "string"}},
                            "x-kubernetes-validations": [
                                {"rule": "self.name.size() > 0", "message": "template name required"}
                            ]
                        }
                    },
                    "x-kubernetes-validations": [
                        {"rule": "self.replicas >= 1", "message": "at least one replica"}
                    ]
                }
            }
        });

        let spec_compiled = compile_schema(&crd_schema["properties"]["spec"]);
        assert_eq!(spec_compiled.validations.len(), 1);
        let spec_cr = spec_compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx = crate::validation::base_context();
        ctx.add_variable_from_value(
            "self",
            json_to_cel(&json!({"replicas": 3, "template": {"name": "web"}})),
        );
        assert_eq!(spec_cr.program.execute(&ctx).unwrap(), Value::Bool(true));

        let tmpl_compiled = compile_schema(&crd_schema["properties"]["spec"]["properties"]["template"]);
        assert_eq!(tmpl_compiled.validations.len(), 1);
        let tmpl_cr = tmpl_compiled.validations.into_iter().next().unwrap().unwrap();

        let mut ctx2 = crate::validation::base_context();
        ctx2.add_variable_from_value("self", json_to_cel(&json!({"name": "web"})));
        assert_eq!(tmpl_cr.program.execute(&ctx2).unwrap(), Value::Bool(true));

        let mut ctx3 = crate::validation::base_context();
        ctx3.add_variable_from_value("self", json_to_cel(&json!({"name": ""})));
        assert_eq!(tmpl_cr.program.execute(&ctx3).unwrap(), Value::Bool(false));
    }

    #[test]
    #[cfg(feature = "strings")]
    fn compiled_rule_with_extension_functions() {
        let schema = json!({
            "x-kubernetes-validations": [{"rule": "self.name.trim().lowerAscii().size() > 0"}]
        });
        let result = compile_and_eval_first(schema, json!({"name": "  Hello  "}));
        assert_eq!(result, Value::Bool(true));
    }
}
