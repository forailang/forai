//! String builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "replace",
        &[
            p("text", Type::String),
            p("find", Type::String),
            p("with", Type::String),
        ],
        &[Type::String],
    );
    ins(
        b,
        "split",
        &[p("text", Type::String), p("on", Type::String)],
        &[array_of(Type::String)],
    );
    ins(b, "trim", &[p("text", Type::String)], &[Type::String]);
    ins(b, "toUpper", &[p("text", Type::String)], &[Type::String]);
    ins(b, "toLower", &[p("text", Type::String)], &[Type::String]);
    ins(
        b,
        "stringContains",
        &[p("text", Type::String), p("part", Type::String)],
        &[Type::Bool],
    );
    ins(
        b,
        "stringStartsWith",
        &[p("text", Type::String), p("prefix", Type::String)],
        &[Type::Bool],
    );
    ins(
        b,
        "stringEndsWith",
        &[p("text", Type::String), p("suffix", Type::String)],
        &[Type::Bool],
    );
    ins(
        b,
        "stringSubstring",
        &[
            p("text", Type::String),
            p("start", Type::Int),
            p("end", Type::Int),
        ],
        &[Type::String],
    );
    ins(
        b,
        "stringIndexOf",
        &[p("text", Type::String), p("search", Type::String)],
        &[Type::Int],
    );
    ins(
        b,
        "stringJoin",
        &[
            p("items", array_of(Type::String)),
            p("separator", Type::String),
        ],
        &[Type::String],
    );
    ins(
        b,
        "stringRepeat",
        &[p("text", Type::String), p("count", Type::Int)],
        &[Type::String],
    );
    ins(
        b,
        "stringTrimStart",
        &[p("text", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "stringTrimEnd",
        &[p("text", Type::String)],
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
    fn test_basic_string_builtins() {
        let b = fresh();
        for name in &["replace", "split", "trim", "toUpper", "toLower"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_string_query_builtins() {
        let b = fresh();
        for name in &[
            "stringContains",
            "stringStartsWith",
            "stringEndsWith",
            "stringIndexOf",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_string_transform_builtins() {
        let b = fresh();
        for name in &[
            "stringSubstring",
            "stringJoin",
            "stringRepeat",
            "stringTrimStart",
            "stringTrimEnd",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_replace_signature() {
        let b = fresh();
        match b.get("replace").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 3);
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_split_returns_array() {
        let b = fresh();
        match b.get("split").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.returns[0], Type::Array(_)));
            }
            _ => panic!("expected Function"),
        }
    }
}
