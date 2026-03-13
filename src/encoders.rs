//! Base64 encode/decode functions for Kubernetes CEL.
//!
//! Provides `base64.decode` and `base64.encode` matching
//! `cel-go/ext/encoders.go`.

use base64::{
    Engine,
    engine::{
        DecodePaddingMode,
        general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD},
    },
};
use cel::{Context, ExecutionError, ResolveResult, objects::Value};
use std::sync::Arc;

/// Base64 decoder that accepts both padded and unpadded input (matching cel-go).
const STANDARD_INDIFFERENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Register all encoder extension functions.
pub fn register(ctx: &mut Context<'_>) {
    ctx.add_function("base64.decode", base64_decode);
    ctx.add_function("base64.encode", base64_encode);
}

/// `base64.decode(<string>) -> bytes`
///
/// Accepts both padded and unpadded input (matching cel-go behavior).
fn base64_decode(s: Arc<String>) -> ResolveResult {
    let bytes = STANDARD_INDIFFERENT
        .decode(s.as_bytes())
        .map_err(|e| ExecutionError::function_error("base64.decode", e.to_string()))?;
    Ok(Value::Bytes(Arc::new(bytes)))
}

/// `base64.encode(<bytes>) -> string`
fn base64_encode(b: Arc<Vec<u8>>) -> ResolveResult {
    Ok(Value::String(Arc::new(STANDARD.encode(b.as_ref()))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cel::Program;

    fn eval(expr: &str) -> Value {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap()
    }

    fn eval_err(expr: &str) -> ExecutionError {
        let mut ctx = Context::default();
        register(&mut ctx);
        Program::compile(expr).unwrap().execute(&ctx).unwrap_err()
    }

    #[test]
    fn test_base64_decode() {
        assert_eq!(
            eval("base64.decode('aGVsbG8=')"),
            Value::Bytes(Arc::new(b"hello".to_vec()))
        );
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(
            eval("base64.encode(b'hello')"),
            Value::String(Arc::new("aGVsbG8=".into()))
        );
    }

    #[test]
    fn test_base64_roundtrip() {
        assert_eq!(
            eval("base64.decode('dGVzdA==')"),
            Value::Bytes(Arc::new(b"test".to_vec()))
        );
    }

    #[test]
    fn test_base64_decode_invalid() {
        eval_err("base64.decode('!!!')");
    }

    #[test]
    fn test_base64_decode_empty() {
        assert_eq!(eval("base64.decode('')"), Value::Bytes(Arc::new(vec![])));
    }

    #[test]
    fn test_base64_decode_unpadded() {
        // cel-go accepts unpadded base64
        assert_eq!(
            eval("base64.decode('aGVsbG8')"),
            Value::Bytes(Arc::new(b"hello".to_vec()))
        );
    }
}
