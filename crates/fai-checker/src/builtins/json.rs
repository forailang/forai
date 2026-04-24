//! JSON builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "jsonParse", &[p("text", Type::String)], &[Type::Unknown]);
    ins(
        b,
        "jsonStringify",
        &[p("value", Type::Unknown)],
        &[Type::String],
    );
    ins(
        b,
        "jsonRequireString",
        &[p("value", Type::Dictionary), p("key", Type::String)],
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
    fn test_json_builtins() {
        let b = fresh();
        for name in &["jsonParse", "jsonStringify", "jsonRequireString"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_json_parse_returns_unknown() {
        let b = fresh();
        match b.get("jsonParse").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.returns[0], Type::Unknown));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_json_stringify_returns_string() {
        let b = fresh();
        match b.get("jsonStringify").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }
}
