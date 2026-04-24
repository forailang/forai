//! Storage builtins — type signatures for `std.storage`.
//!
//! The wasm runtime implementation lives in
//! `fai-cli/src/wasm_runner/host/storage.rs` (native wasmtime-backed
//! thread-local HashMap) and `fai-cli/src/lib.rs` emits the browser
//! (`localStorage`) bridge when building for `wasm-html`.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "storageGet",
        &[p("key", Type::String)],
        &[optional_of(Type::String)],
    );
    ins(
        b,
        "storageSet",
        &[p("key", Type::String), p("value", Type::String)],
        &[Type::Void],
    );
    ins(b, "storageRemove", &[p("key", Type::String)], &[Type::Void]);
    ins(b, "storageClear", &[], &[Type::Void]);
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
    fn test_storage_builtins_registered() {
        let b = fresh();
        for name in &["storageGet", "storageSet", "storageRemove", "storageClear"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_storage_get_returns_optional_string() {
        let b = fresh();
        match b.get("storageGet").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Optional(inner) => assert!(matches!(**inner, Type::String)),
                other => panic!("expected Optional<String>, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }
}
