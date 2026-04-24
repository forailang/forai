//! Core builtins: I/O, collections, conversions, error type.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // I/O
    ins(b, "print", &[p("value", Type::Unknown)], &[Type::Void]);

    // Collection
    ins(b, "length", &[p("value", Type::Unknown)], &[Type::Int]);
    ins(b, "isEmpty", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(
        b,
        "append",
        &[p("items", Type::Unknown), p("item", Type::Unknown)],
        &[Type::Void],
    );

    // Conversion
    ins(b, "toString", &[p("value", Type::Unknown)], &[Type::String]);
    ins(b, "toInt", &[p("value", Type::Unknown)], &[Type::Int]);
    ins(b, "toFloat", &[p("value", Type::Unknown)], &[Type::Float]);
    ins(b, "toBool", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "parseInt", &[p("text", Type::String)], &[Type::Int]);
    ins(b, "parseFloat", &[p("text", Type::String)], &[Type::Float]);

    // Error
    ins(b, "Error", &[p("message", Type::String)], &[Type::Error]);
    ins(b, "message", &[p("err", Type::Error)], &[Type::String]);
    ins(b, "kind", &[p("err", Type::Error)], &[Type::String]);
    ins(b, "isError", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(
        b,
        "unwrap",
        &[p("value", Type::Unknown), p("fallback", Type::Unknown)],
        &[Type::Void],
    );

    // Type introspection
    ins(b, "is_int", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_float", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_string", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_bool", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_null", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_array", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(b, "is_dict", &[p("value", Type::Unknown)], &[Type::Bool]);

    // Row-to-type construction driven by the binding's type
    // annotation: `let p Person = from_dict(row)` expands at compile
    // time to a Person constructor call with one `get(dict, field)`
    // per declared field. Field attributes (`alias`, `omit`) control
    // the column → field mapping.
    ins(
        b,
        "from_dict",
        &[p("dict", Type::Unknown)],
        &[Type::Unknown],
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
    fn test_io_builtins() {
        let b = fresh();
        assert!(b.contains_key("print"));
    }

    #[test]
    fn test_collection_builtins() {
        let b = fresh();
        for name in &["length", "isEmpty", "append"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_conversion_builtins() {
        let b = fresh();
        for name in &[
            "toString",
            "toInt",
            "toFloat",
            "toBool",
            "parseInt",
            "parseFloat",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_error_builtins() {
        let b = fresh();
        for name in &["Error", "message", "kind", "isError", "unwrap"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_print_signature() {
        let b = fresh();
        match b.get("print").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.returns[0], Type::Void));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_type_introspection_builtins() {
        let b = fresh();
        for name in &[
            "is_int",
            "is_float",
            "is_string",
            "is_bool",
            "is_null",
            "is_array",
            "is_dict",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
            match b.get(*name).unwrap() {
                Type::Function(sig) => {
                    assert_eq!(sig.params.len(), 1, "{} should have 1 param", name);
                    assert!(
                        matches!(sig.returns[0], Type::Bool),
                        "{} should return Bool",
                        name
                    );
                }
                _ => panic!("{} should be Function", name),
            }
        }
    }

    #[test]
    fn test_from_dict_builtin() {
        let b = fresh();
        assert!(b.contains_key("from_dict"));
        match b.get("from_dict").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.returns[0], Type::Unknown));
            }
            _ => panic!("from_dict should be Function"),
        }
    }

    #[test]
    fn test_parse_int_returns_int() {
        let b = fresh();
        match b.get("parseInt").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.params[0].ty, Type::String));
                assert!(matches!(sig.returns[0], Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }
}
