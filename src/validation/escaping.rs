//! Field name escaping for Kubernetes CEL.
//!
//! Kubernetes CEL requires escaping JSON field names that collide with CEL
//! reserved words or contain `__`, `.`, `-` or `/` (a single `_` stays as-is).
//! This module implements the same escaping rules as the Go apiserver
//! (`k8s.io/apiserver/pkg/cel/escaping.go`).

/// CEL reserved words that must be escaped as `__keyword__`.
const CEL_RESERVED_WORDS: &[&str] = &[
    "true",
    "false",
    "null",
    "in",
    "as",
    "break",
    "const",
    "continue",
    "else",
    "for",
    "function",
    "if",
    "import",
    "let",
    "loop",
    "package",
    "namespace",
    "return",
    "var",
    "void",
    "while",
];

/// Escape a JSON field name for use as a CEL map key.
///
/// Mirrors the apiserver `Escape` (`k8s.io/apiserver/pkg/cel/escaping.go`,
/// matcher `(__|[-./]|[^a-zA-Z0-9-./_])`). Rules, checked in order:
/// 1. If the name exactly matches a CEL reserved word → `__keyword__`
/// 2. Otherwise substitute each occurrence:
///    - `__` (double underscore) → `__underscores__`
///    - `.` → `__dot__`
///    - `-` → `__dash__`
///    - `/` → `__slash__`
///
///    A single `_` is a valid CEL identifier character and is left untouched —
///    only a `__` pair is escaped (matching the apiserver's leftmost-first pass).
/// 3. Names with none of those sequences are returned unchanged.
///
/// Note: the apiserver additionally makes names containing characters outside
/// `[a-zA-Z0-9-./_]` inaccessible (its `Escape` returns `ok=false`); this helper
/// passes such characters through unchanged. Declared structural properties do
/// not hit that case in practice.
#[must_use]
#[inline]
pub(crate) fn escape_field_name(name: &str) -> String {
    // Rule 1: exact match against reserved words
    if CEL_RESERVED_WORDS.contains(&name) {
        return format!("__{name}__");
    }

    // Rule 2: only `__`, `.`, `-`, `/` are escaped (a single `_` is left as-is).
    if name.contains("__") || name.contains('.') || name.contains('-') || name.contains('/') {
        let mut escaped = String::with_capacity(name.len() * 2);
        let mut chars = name.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '_' if chars.peek() == Some(&'_') => {
                    chars.next(); // consume the paired underscore
                    escaped.push_str("__underscores__");
                }
                '.' => escaped.push_str("__dot__"),
                '-' => escaped.push_str("__dash__"),
                '/' => escaped.push_str("__slash__"),
                other => escaped.push(other),
            }
        }
        return escaped;
    }

    // Rule 3: no escaping needed
    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_word_namespace() {
        assert_eq!(escape_field_name("namespace"), "__namespace__");
    }

    #[test]
    fn reserved_word_default_is_not_reserved() {
        // "default" is NOT in the CEL reserved word list
        assert_eq!(escape_field_name("default"), "default");
    }

    #[test]
    fn all_reserved_words() {
        for &word in CEL_RESERVED_WORDS {
            assert_eq!(escape_field_name(word), format!("__{word}__"));
        }
    }

    #[test]
    fn dash_escaping() {
        assert_eq!(escape_field_name("foo-bar"), "foo__dash__bar");
    }

    #[test]
    fn dot_escaping() {
        assert_eq!(escape_field_name("a.b"), "a__dot__b");
    }

    #[test]
    fn slash_escaping() {
        assert_eq!(escape_field_name("x/y"), "x__slash__y");
    }

    #[test]
    fn single_underscore_unchanged() {
        // apiserver escapes only `__`; a lone `_` is a valid CEL identifier char.
        assert_eq!(escape_field_name("my_field"), "my_field");
    }

    #[test]
    fn double_underscore_escaping() {
        assert_eq!(escape_field_name("a__b"), "a__underscores__b");
    }

    #[test]
    fn triple_underscore_leftmost_pair() {
        // Leftmost-first: the first `__` escapes, the trailing lone `_` stays.
        assert_eq!(escape_field_name("a___b"), "a__underscores___b");
    }

    #[test]
    fn mixed_special_characters() {
        assert_eq!(escape_field_name("a-b_c.d"), "a__dash__b_c__dot__d");
    }

    #[test]
    fn no_escaping_needed() {
        assert_eq!(escape_field_name("replicas"), "replicas");
        assert_eq!(escape_field_name("spec"), "spec");
        assert_eq!(escape_field_name("fooBar"), "fooBar");
    }

    #[test]
    fn empty_string() {
        assert_eq!(escape_field_name(""), "");
    }

    #[test]
    fn reserved_word_takes_priority_over_special_chars() {
        // "in" is a reserved word — should get keyword escaping, not char-level
        assert_eq!(escape_field_name("in"), "__in__");
    }

    #[test]
    fn multiple_dashes() {
        assert_eq!(escape_field_name("a-b-c"), "a__dash__b__dash__c");
    }

    #[test]
    fn leading_single_underscore_unchanged() {
        assert_eq!(escape_field_name("_private"), "_private");
    }

    #[test]
    fn slash_in_annotation_key() {
        assert_eq!(
            escape_field_name("app.kubernetes.io/name"),
            "app__dot__kubernetes__dot__io__slash__name"
        );
    }
}
