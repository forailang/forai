//! Core builtins: I/O, collections, conversions, error type.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // I/O
    ins(b, "print", &[p("value", Type::Unknown)], &[Type::Void]);

    // (R0 clean slate, plan 113: `reclaim`/`markShared` removed — manual memory
    // management is gone; reference counting is rebuilt in R1.)
    // Deep-copy a value: returns a fresh, independently-owned duplicate of the
    // whole reachable graph. Use it to break aliasing — an independent copy that
    // follows normal value semantics.
    ins(
        b,
        "copy",
        &[p("value", type_parameter("T"))],
        &[type_parameter("T")],
    );
    // Debug/diagnostics: current heap bump pointer (`__heap_ptr`, the allocation
    // high-water mark in bytes). Freed blocks are reused from the free-list
    // without advancing this, so under correct reclamation it plateaus across
    // render/signal cycles; a monotonic climb signals a leak. Instrumentation
    // only — not part of the stable language surface.
    ins(b, "__heapPtr", &[], &[Type::Int]);
    // Debug/diagnostics: the live heap-object counter (`__live_objects`, ++ in
    // rt_alloc, -- in rt_free). Under correct reclamation an async task that
    // allocates locals returns this to its pre-spawn baseline at completion;
    // a per-task climb signals an async-reclamation leak (plan 115).
    // Instrumentation only — not part of the stable language surface.
    ins(b, "__liveObjects", &[], &[Type::Int]);
    // Reference-counting diagnostics (plan 113). `__refcount(x)` is the object's
    // current count (or -1 for a primitive). `__retain`/`__release` drive the
    // RC primitives directly — test scaffolding for validating the count
    // transitions before the codegen emits them at every reference site (P3).
    ins(
        b,
        "__refcount",
        &[p("value", type_parameter("T"))],
        &[Type::Int],
    );
    ins(
        b,
        "__retain",
        &[p("value", type_parameter("T"))],
        &[type_parameter("T")],
    );
    ins(
        b,
        "__release",
        &[p("value", type_parameter("T"))],
        &[Type::Void],
    );

    // Collection
    ins(b, "length", &[p("value", Type::Unknown)], &[Type::Int]);
    ins(b, "isEmpty", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(
        b,
        "append",
        &[
            p("items", array_of(type_parameter("T"))),
            p("item", type_parameter("T")),
        ],
        &[array_of(type_parameter("T"))],
    );

    // Conversion
    ins(b, "toString", &[p("value", Type::Unknown)], &[Type::String]);
    // Runtime type inspection: the kind name of any value ('int', 'float',
    // 'bool', 'null', 'void', 'string', 'array', 'dictionary', 'tuple',
    // 'closure', 'module', 'unknown'; records are dict-shaped and report
    // 'dictionary'). Lets Unknown-typed data
    // (parsed JSON, dynamic tool results) branch without try/catch cast
    // probes or stringify-and-inspect hacks.
    ins(b, "typeOf", &[p("value", Type::Unknown)], &[Type::String]);
    ins(b, "toInt", &[p("value", Type::Unknown)], &[Type::Int]);
    ins(b, "toFloat", &[p("value", Type::Unknown)], &[Type::Float]);
    ins(b, "toBool", &[p("value", Type::Unknown)], &[Type::Bool]);
    // parseInt/parseFloat return null at runtime for unparseable input,
    // so their type is optional — callers must `unwrap`/`?`-check.
    ins(
        b,
        "parseInt",
        &[p("text", Type::String)],
        &[optional_of(Type::Int)],
    );
    ins(
        b,
        "parseFloat",
        &[p("text", Type::String)],
        &[optional_of(Type::Float)],
    );

    // Error
    ins(b, "Error", &[p("message", Type::String)], &[Type::Error]);
    ins(b, "message", &[p("err", Type::Error)], &[Type::String]);
    ins(b, "kind", &[p("err", Type::Error)], &[Type::String]);
    ins(b, "isError", &[p("value", Type::Unknown)], &[Type::Bool]);
    ins(
        b,
        "unwrap",
        &[p("value", Type::Unknown), p("fallback", Type::Unknown)],
        &[Type::Unknown],
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
    fn test_append_signature_returns_array() {
        let b = fresh();
        match b.get("append").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.params[0].ty, Type::Array(_)));
                assert!(matches!(sig.returns[0], Type::Array(_)));
            }
            _ => panic!("expected Function"),
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
    fn test_unwrap_signature_is_expression_value() {
        let b = fresh();
        match b.get("unwrap").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(!matches!(sig.returns[0], Type::Void));
            }
            _ => panic!("expected Function"),
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
    fn test_parse_int_returns_optional_int() {
        let b = fresh();
        match b.get("parseInt").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.params[0].ty, Type::String));
                // parseInt returns null on unparseable input → Int?.
                match &sig.returns[0] {
                    Type::Optional(inner) => assert!(matches!(**inner, Type::Int)),
                    other => panic!("expected Optional(Int), got {:?}", other),
                }
            }
            _ => panic!("expected Function"),
        }
    }
}
