//! Static analysis for CEL validation rules.
//!
//! Provides compile-time checks beyond syntax validation:
//! variable scope validation and cost estimation.

use cel::{Program, common::ast::Expr};

use crate::compilation::CompiledSchema;

/// The context in which a CEL rule is evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeContext {
    /// CRD `x-kubernetes-validations` — only `self`, `oldSelf`, and root vars.
    CrdValidation,
    /// ValidatingAdmissionPolicy — `object`, `oldObject`, `request`, `params`, etc.
    AdmissionPolicy,
}

/// A warning produced by static analysis.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AnalysisWarning {
    pub rule: String,
    pub message: String,
    pub kind: WarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
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
        ScopeContext::CrdValidation => &["self", "oldSelf", "apiVersion", "apiGroup", "kind"],
        ScopeContext::AdmissionPolicy => &[
            "self",
            "oldSelf",
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

/// Check a CEL expression for variable scope violations.
#[must_use]
pub fn check_rule_scope(rule: &str, scope: ScopeContext) -> Vec<AnalysisWarning> {
    let program = match Program::compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let valid = valid_variables(scope);
    let mut warnings = Vec::new();

    for var in program.references().variables() {
        if !valid.contains(&var) {
            warnings.push(AnalysisWarning {
                rule: rule.to_string(),
                message: format!(
                    "variable '{}' is not available in {:?} context; valid variables: {:?}",
                    var, scope, valid
                ),
                kind: WarningKind::WrongScope,
            });
        }
    }

    warnings
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
    let program = match Program::compile(rule) {
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
    let program = match Program::compile(rule) {
        Ok(p) => p,
        Err(_) => return vec![],
    };

    let mut warnings = Vec::new();

    // Scope validation
    let valid = valid_variables(scope);
    for var in program.references().variables() {
        if !valid.contains(&var) {
            warnings.push(AnalysisWarning {
                rule: rule.to_string(),
                message: format!(
                    "variable '{}' is not available in {:?} context; valid variables: {:?}",
                    var, scope, valid
                ),
                kind: WarningKind::WrongScope,
            });
        }
    }

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
    use crate::compilation::compile_schema;
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
