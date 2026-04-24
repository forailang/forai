//! Dictionary builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "get",
        &[p("dict", Type::Dictionary), p("key", Type::String)],
        &[optional_of(Type::Unknown)],
    );
    ins(
        b,
        "getString",
        &[p("dict", Type::Dictionary), p("key", Type::String)],
        &[optional_of(Type::String)],
    );
    ins(
        b,
        "getInt",
        &[p("dict", Type::Dictionary), p("key", Type::String)],
        &[optional_of(Type::Int)],
    );
    ins(
        b,
        "getBool",
        &[p("dict", Type::Dictionary), p("key", Type::String)],
        &[optional_of(Type::Bool)],
    );
    ins(
        b,
        "set",
        &[
            p("dict", Type::Dictionary),
            p("key", Type::String),
            p("value", Type::Unknown),
        ],
        &[Type::Dictionary],
    );
    ins(
        b,
        "getKeys",
        &[p("dict", Type::Dictionary)],
        &[array_of(Type::String)],
    );
    ins(
        b,
        "hasKey",
        &[p("dict", Type::Dictionary), p("key", Type::String)],
        &[Type::Bool],
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
    fn test_dict_builtins() {
        let b = fresh();
        for name in &[
            "get",
            "getString",
            "getInt",
            "getBool",
            "set",
            "getKeys",
            "hasKey",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_set_returns_dictionary() {
        let b = fresh();
        match b.get("set").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 3);
                assert!(matches!(sig.returns[0], Type::Dictionary));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_get_keys_returns_string_array() {
        let b = fresh();
        match b.get("getKeys").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Array(elem) => assert!(matches!(**elem, Type::String)),
                _ => panic!("expected Array"),
            },
            _ => panic!("expected Function"),
        }
    }
}
