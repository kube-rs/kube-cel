//! Static analysis for CEL validation rules.
//!
//! Provides compile-time checks beyond syntax validation:
//! variable scope validation and cost estimation.

use cel::Program;

/// The context in which a CEL rule is evaluated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeContext {
    /// CRD `x-kubernetes-validations` — only `self`, `oldSelf`, and root vars.
    CrdValidation,
    /// ValidatingAdmissionPolicy — `object`, `oldObject`, `request`, `params`, etc.
    AdmissionPolicy,
}

/// A warning produced by static analysis.
#[derive(Clone, Debug)]
pub struct AnalysisWarning {
    pub rule: String,
    pub message: String,
    pub kind: WarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_wrong_scope_variable() {
        let warnings =
            check_rule_scope("request.userInfo.username == 'admin'", ScopeContext::CrdValidation);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("request"));
        assert_eq!(warnings[0].kind, WarningKind::WrongScope);
    }

    #[test]
    fn self_and_old_self_are_valid() {
        let warnings =
            check_rule_scope("self.replicas >= oldSelf.replicas", ScopeContext::CrdValidation);
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
        let warnings =
            check_rule_scope("object.metadata.name == 'test'", ScopeContext::CrdValidation);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn invalid_syntax_returns_empty() {
        let warnings = check_rule_scope("self.x >=", ScopeContext::CrdValidation);
        assert!(warnings.is_empty());
    }
}
