//! Cryptography builtins (std.crypto).
//!
//! Native-only hashing/HMAC/encoding primitives. The browser target
//! reports `crypto.available()` as false; the heavy functions are not
//! linked there (see `available_imports_with_test_flag` in fai-codegen-wasm).

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "cryptoAvailable", &[], &[Type::Bool]);
    ins(
        b,
        "cryptoHmacSha256Hex",
        &[p("key", Type::String), p("message", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoHmacSha1Base64",
        &[p("key", Type::String), p("message", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoSha256Hex",
        &[p("data", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoHexEncode",
        &[p("data", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoConstantTimeEquals",
        &[p("a", Type::String), p("b", Type::String)],
        &[Type::Bool],
    );
    ins(
        b,
        "cryptoBase64Encode",
        &[p("data", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoBase64Decode",
        &[p("data", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "cryptoRs256SignBase64Url",
        &[p("privateKeyPem", Type::String), p("message", Type::String)],
        &[Type::String],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> HashMap<String, Type> {
        let mut b = HashMap::new();
        install(&mut b);
        b
    }

    #[test]
    fn test_crypto_builtins_present() {
        let b = fresh();
        for name in &[
            "cryptoAvailable",
            "cryptoHmacSha256Hex",
            "cryptoHmacSha1Base64",
            "cryptoSha256Hex",
            "cryptoHexEncode",
            "cryptoConstantTimeEquals",
            "cryptoBase64Encode",
            "cryptoBase64Decode",
            "cryptoRs256SignBase64Url",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_available_returns_bool() {
        let b = fresh();
        match b.get("cryptoAvailable").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 0);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_hmac_takes_two_strings_returns_string() {
        let b = fresh();
        for name in &["cryptoHmacSha256Hex", "cryptoHmacSha1Base64"] {
            match b.get(*name).unwrap() {
                Type::Function(sig) => {
                    assert_eq!(sig.params.len(), 2);
                    assert!(matches!(sig.returns[0], Type::String));
                }
                _ => panic!("expected Function"),
            }
        }
    }

    #[test]
    fn test_constant_time_equals_returns_bool() {
        let b = fresh();
        match b.get("cryptoConstantTimeEquals").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_rs256_sign_takes_two_strings_returns_string() {
        let b = fresh();
        match b.get("cryptoRs256SignBase64Url").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }
}
