//! Environment variable builtins — type signatures for `std.env`.
//!
//! The wasm runtime implementation lives in
//! `fai-cli/src/wasm_runner/host/env.rs`. `env.get` returns the host
//! process environment value for a key, or `null` when unset.
//! `env.load` parses a dotenv-style file and merges its entries into
//! the process environment so subsequent `env.get` calls can find
//! them. Browser builds receive stubs that return null / false.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "envGet",
        &[p("key", Type::String)],
        &[optional_of(Type::String)],
    );
    ins(b, "envLoad", &[p("path", Type::String)], &[Type::Bool]);
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
    fn test_env_builtins_registered() {
        let b = fresh();
        for name in &["envGet", "envLoad"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_env_get_returns_optional_string() {
        let b = fresh();
        match b.get("envGet").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Optional(inner) => assert!(matches!(**inner, Type::String)),
                other => panic!("expected Optional<String>, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_env_load_returns_bool() {
        let b = fresh();
        match b.get("envLoad").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }
}
