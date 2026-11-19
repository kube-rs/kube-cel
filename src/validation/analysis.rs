//! Static analysis for CEL validation rules.
//!
//! Provides compile-time checks beyond syntax validation:
//! variable scope validation and cost estimation.

use cel::common::ast::Expr;

use crate::validation::{Scope, compilation::CompiledSchema, references::undeclared_identifiers};

/// The context in which a CEL rule is evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScopeContext {
    /// CRD `x-kubernetes-validations` — only `self` and `oldSelf`.
    CrdValidation,
    /// ValidatingAdmissionPolicy — `object`, `oldObject`, `request`,
    /// `namespaceObject` and `authorizer`, and `params` and `variables`, which
    /// a policy has with a `paramKind` and `spec.variables`.
    AdmissionPolicy,
}

/// A warning produced by static analysis.
///
/// `#[non_exhaustive]`: an output type the crate constructs; new fields may be
/// added without a breaking change.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct AnalysisWarning {
    /// The CEL rule the warning applies to.
    pub rule: String,
    /// Human-readable description of the warning.
    pub message: String,
    /// Classification of the warning.
    pub kind: WarningKind,
}

/// The kind of warning produced by static analysis.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum WarningKind {
    /// Variable not available in the given scope.
    WrongScope,
    /// Estimated cost may exceed K8s budget.
    CostExceeded,
    /// Schema bounds missing (inflates cost estimate).
    MissingBounds,
}

fn valid_variables(scope: ScopeContext) -> &'static [&'static str] {
    match scope {
        // As the reference apiserver declares them. A policy's `params` and
        // `variables` depend on its spec, which this check does not see, so
        // both are taken.
        ScopeContext::CrdValidation => &["self", "oldSelf"],
        ScopeContext::AdmissionPolicy => &[
            "object",
            "oldObject",
            "request",
            "params",
            "namespaceObject",
            "authorizer",
            "variables",
        ],
    }
}

/// The compile-time check's scope for `context`: a policy's `params` and
/// `variables` depend on its spec, which static analysis does not see, so
/// both are taken.
fn scope_of(context: ScopeContext) -> Scope {
    match context {
        ScopeContext::CrdValidation => Scope::Crd,
        ScopeContext::AdmissionPolicy => Scope::Vap {
            params: true,
            authorizer: true,
            variables: true,
        },
    }
}

/// A [`WarningKind::WrongScope`] warning for each identifier `program` reads
/// that is not declared where it appears, by the compile-time check's name
/// resolution: a macro's variables are declared inside the macro, and type
/// names (`int`, `net.IP`) and function namespaces (`optional.of`,
/// `format.dns1123Label`) are no variables.
fn scope_warnings(rule: &str, program: &cel::Program, scope: ScopeContext) -> Vec<AnalysisWarning> {
    let valid = valid_variables(scope);
    undeclared_identifiers(program.expression(), scope_of(scope))
        .into_iter()
        .map(|var| AnalysisWarning {
            rule: rule.to_string(),
            message: format!(
                "variable '{}' is not available in {:?} context; valid variables: {:?}",
                var, scope, valid
            ),
            kind: WarningKind::WrongScope,
        })
        .collect()
}

/// Check a CEL expression for variable scope violations.
#[must_use]
pub fn check_rule_scope(rule: &str, scope: ScopeContext) -> Vec<AnalysisWarning> {
    match crate::validation::validation_env().compile(rule) {
        Ok(program) => scope_warnings(rule, &program, scope),
        Err(_) => vec![],
    }
}

const DEFAULT_MAX_ITEMS: u64 = 1000;
const DEFAULT_MAX_LENGTH: u64 = 1000;
const K8S_COST_BUDGET: u64 = 1_000_000;
const STRING_TRAVERSAL_FACTOR: f64 = 0.1;

/// Estimate cost of a CEL rule and warn if it may exceed K8s budget.
///
/// This is a coarse heuristic, not an accurate cost model. It catches the most
/// common issue: unbounded list comprehensions without maxItems.
#[must_use]
pub fn estimate_rule_cost(rule: &str, schema: &CompiledSchema) -> Vec<AnalysisWarning> {
    let program = match crate::validation::validation_env().compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let expr = program.expression();
    let mut warnings = Vec::new();
    let cost = estimate_expr_cost(&expr.expr, schema);

    if cost > K8S_COST_BUDGET {
        warnings.push(AnalysisWarning {
            rule: rule.to_string(),
            message: format!(
                "estimated cost {} exceeds K8s budget {}; consider adding maxItems/maxLength to schema bounds",
                cost, K8S_COST_BUDGET
            ),
            kind: WarningKind::CostExceeded,
        });
    }

    check_missing_bounds(&expr.expr, schema, rule, &mut warnings);
    warnings
}

/// Run all available static analyses on a CEL rule in a single pass.
///
/// Compiles the rule once and performs both scope validation and cost estimation.
/// More efficient than calling [`check_rule_scope`] and [`estimate_rule_cost`] separately.
#[must_use]
pub fn analyze_rule(rule: &str, schema: &CompiledSchema, scope: ScopeContext) -> Vec<AnalysisWarning> {
    let program = match crate::validation::validation_env().compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    // Scope validation
    let mut warnings = scope_warnings(rule, &program, scope);

    // Cost estimation
    let expr = program.expression();
    let cost = estimate_expr_cost(&expr.expr, schema);
    if cost > K8S_COST_BUDGET {
        warnings.push(AnalysisWarning {
            rule: rule.to_string(),
            message: format!(
                "estimated cost {} exceeds K8s budget {}; consider adding maxItems/maxLength to schema bounds",
                cost, K8S_COST_BUDGET
            ),
            kind: WarningKind::CostExceeded,
        });
    }
    check_missing_bounds(&expr.expr, schema, rule, &mut warnings);

    warnings
}

fn estimate_expr_cost(expr: &Expr, schema: &CompiledSchema) -> u64 {
    match expr {
        Expr::Comprehension(comp) => {
            let list_size = find_max_items(schema);
            let body_cost = estimate_expr_cost(&comp.loop_step.expr, schema);
            list_size * body_cost.max(1)
        }
        Expr::Call(call) => {
            let base = 1u64;
            let target_cost = call
                .target
                .as_ref()
                .map(|t| estimate_expr_cost(&t.expr, schema))
                .unwrap_or(0);
            let arg_cost: u64 = call
                .args
                .iter()
                .map(|a| estimate_expr_cost(&a.expr, schema))
                .sum();
            if is_string_traversal(&call.func_name) {
                let str_len = find_max_length(schema);
                base + (str_len as f64 * STRING_TRAVERSAL_FACTOR) as u64 + target_cost + arg_cost
            } else {
                base + target_cost + arg_cost
            }
        }
        Expr::Select(sel) => 1 + estimate_expr_cost(&sel.operand.expr, schema),
        Expr::List(list) => list
            .elements
            .iter()
            .map(|e| estimate_expr_cost(&e.expr, schema))
            .sum::<u64>()
            .max(1),
        _ => 1,
    }
}

fn find_max_items(schema: &CompiledSchema) -> u64 {
    if let Some(max) = schema.max_items {
        return max;
    }
    for prop in schema.properties.values() {
        if prop.items.is_some() {
            return prop.max_items.unwrap_or(DEFAULT_MAX_ITEMS);
        }
    }
    DEFAULT_MAX_ITEMS
}

fn find_max_length(schema: &CompiledSchema) -> u64 {
    if let Some(max) = schema.max_length {
        return max;
    }
    for prop in schema.properties.values() {
        if let Some(max) = prop.max_length {
            return max;
        }
    }
    DEFAULT_MAX_LENGTH
}

fn is_string_traversal(func: &str) -> bool {
    matches!(
        func,
        "contains"
            | "startsWith"
            | "endsWith"
            | "matches"
            | "find"
            | "findAll"
            | "replace"
            | "split"
            | "indexOf"
            | "lastIndexOf"
    )
}

fn check_missing_bounds(
    expr: &Expr,
    schema: &CompiledSchema,
    rule: &str,
    warnings: &mut Vec<AnalysisWarning>,
) {
    if let Expr::Comprehension(_) = expr {
        for prop in schema.properties.values() {
            if prop.items.is_some() && prop.max_items.is_none() {
                warnings.push(AnalysisWarning {
                    rule: rule.to_string(),
                    message: "list field has no maxItems bound; cost estimate uses worst-case default".into(),
                    kind: WarningKind::MissingBounds,
                });
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validation::compilation::compile_schema;
    use serde_json::json;

    #[test]
    fn detect_wrong_scope_variable() {
        let warnings = check_rule_scope(
            "request.userInfo.username == 'admin'",
            ScopeContext::CrdValidation,
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("request"));
        assert_eq!(warnings[0].kind, WarningKind::WrongScope);
    }

    #[test]
    fn self_and_old_self_are_valid() {
        let warnings = check_rule_scope("self.replicas >= oldSelf.replicas", ScopeContext::CrdValidation);
        assert!(warnings.is_empty());
    }

    #[test]
    fn admission_policy_scope_allows_request() {
        let warnings = check_rule_scope(
            "request.userInfo.username == 'admin'",
            ScopeContext::AdmissionPolicy,
        );
        assert!(warnings.is_empty());
    }

    /// Type names, function namespaces and a macro's variables are no
    /// variables (they used to be flagged), while a macro's variable outside
    /// the macro is.
    #[test]
    fn scope_check_resolves_names_as_the_compiler_does() {
        for rule in [
            "type(self.x) == int",
            "optional.of(1).hasValue() && self.x > 0",
            "format.dns1123Label().validate(self.n).hasValue()",
            "type(ip('1.2.3.4')) == net.IP",
            "self.items.all(item, item.size() > 0) && self.items.map(x, x).exists(y, y == '')",
        ] {
            let warnings = check_rule_scope(rule, ScopeContext::CrdValidation);
            assert!(warnings.is_empty(), "{rule}: {warnings:?}");
        }
        let warnings = check_rule_scope(
            "self.items.all(x, x != '') && x == ''",
            ScopeContext::CrdValidation,
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].message.contains("'x'"));
    }

    /// The receiver of an unknown member function is still read, as cel-go
    /// reports `missing` in `[missing].noSuchFunction()`.
    #[test]
    fn scope_check_reads_the_receiver_of_an_unknown_function() {
        for scope in [ScopeContext::CrdValidation, ScopeContext::AdmissionPolicy] {
            for rule in [
                "[missing].noSuchFunction() == true",
                "optional.of(missing).noSuchFunction() == true",
            ] {
                let warnings = check_rule_scope(rule, scope);
                assert_eq!(warnings.len(), 1, "{rule}: {warnings:?}");
                assert!(warnings[0].message.contains("'missing'"), "{rule}");
            }
            let warnings = check_rule_scope("missing.mystery(other) == true", scope);
            assert_eq!(warnings.len(), 2, "{warnings:?}");
        }
    }

    /// A policy's `params` and `variables` depend on its spec, so both are
    /// taken.
    #[test]
    fn admission_policy_scope_takes_params_and_variables() {
        let warnings = check_rule_scope(
            "params.max >= object.spec.replicas && variables.foo == 1 && type(1) == int",
            ScopeContext::AdmissionPolicy,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn crd_scope_flags_root_context_names() {
        let warnings = check_rule_scope(
            "kind == 'Deployment' && apiGroup == 'apps'",
            ScopeContext::CrdValidation,
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn admission_policy_scope_flags_self() {
        let warnings = check_rule_scope("self.x == oldSelf.x", ScopeContext::AdmissionPolicy);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
    }

    #[test]
    fn crd_scope_rejects_object_variable() {
        let warnings = check_rule_scope("object.metadata.name == 'test'", ScopeContext::CrdValidation);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn invalid_syntax_returns_empty() {
        let warnings = check_rule_scope("self.x >=", ScopeContext::CrdValidation);
        assert!(warnings.is_empty());
    }

    #[test]
    fn unbounded_list_comprehension_warns() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });
        let compiled = compile_schema(&schema);
        let warnings = estimate_rule_cost("self.items.all(item, item.size() > 0)", &compiled);
        assert!(
            warnings
                .iter()
                .any(|w| w.kind == WarningKind::CostExceeded || w.kind == WarningKind::MissingBounds)
        );
    }

    #[test]
    fn bounded_list_no_cost_warning() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "maxItems": 10,
                    "items": {"type": "string", "maxLength": 64}
                }
            }
        });
        let compiled = compile_schema(&schema);
        let warnings = estimate_rule_cost("self.items.all(item, item.size() > 0)", &compiled);
        // With bounded list (10 items), cost should be low
        assert!(warnings.iter().all(|w| w.kind != WarningKind::CostExceeded));
    }

    #[test]
    fn simple_comparison_low_cost() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}}
        });
        let compiled = compile_schema(&schema);
        let warnings = estimate_rule_cost("self.x >= 0", &compiled);
        assert!(warnings.is_empty());
    }

    #[test]
    fn analyze_rule_catches_scope_issue() {
        let schema = json!({"type": "object", "properties": {"x": {"type": "integer"}}});
        let compiled = compile_schema(&schema);
        let warnings = analyze_rule("request.name == 'test'", &compiled, ScopeContext::CrdValidation);
        assert!(warnings.iter().any(|w| w.kind == WarningKind::WrongScope));
    }

    #[test]
    fn analyze_rule_catches_cost_and_bounds() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {"type": "array", "items": {"type": "string"}}
            }
        });
        let compiled = compile_schema(&schema);
        let warnings = analyze_rule(
            "self.items.all(item, item.size() > 0)",
            &compiled,
            ScopeContext::CrdValidation,
        );
        // `self` should not be flagged as a scope violation
        assert!(
            !warnings
                .iter()
                .any(|w| w.kind == WarningKind::WrongScope && w.message.contains("'self'"))
        );
        // Missing maxItems bound should be reported
        assert!(warnings.iter().any(|w| w.kind == WarningKind::MissingBounds));
    }

    #[test]
    fn missing_bounds_warning() {
        let schema = json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {"type": "string"}
                }
            }
        });
        let compiled = compile_schema(&schema);
        let warnings = estimate_rule_cost("self.items.all(item, item.size() > 0)", &compiled);
        assert!(warnings.iter().any(|w| w.kind == WarningKind::MissingBounds));
    }
}
