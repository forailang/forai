//! JSON builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "jsonParse", &[p("text", Type::String)], &[Type::Unknown]);
    // Host-side selection: parse natively and materialize only the values a
    // jq-style path matches (`.a.b[].c`, indexes `[0]`/`[-1]`, quoted keys,
    // pipes, `..` descent; '' selects the root). `jsonQuery` returns every
    // match; `jsonQueryPage` returns one window plus the total match count
    // as `{ total: Int, items: [...] }`. Both return null for invalid JSON
    // (like `jsonParse`) and for malformed paths.
    ins(
        b,
        "jsonQuery",
        &[p("text", Type::String), p("path", Type::String)],
        &[optional_of(array_of(Type::Unknown))],
    );
    // Text-level helpers: `jsonFormat` pretty-prints (2-space indent, one
    // attribute per line), `jsonMinify` strips insignificant whitespace —
    // both return null for invalid JSON. `jsonValid` is a cheap probe that
    // materializes nothing. `jsonStringifyPretty` is `jsonStringify` with
    // pretty output.
    ins(
        b,
        "jsonFormat",
        &[p("text", Type::String)],
        &[optional_of(Type::String)],
    );
    ins(
        b,
        "jsonMinify",
        &[p("text", Type::String)],
        &[optional_of(Type::String)],
    );
    ins(b, "jsonValid", &[p("text", Type::String)], &[Type::Bool]);
    ins(
        b,
        "jsonStringifyPretty",
        &[p("value", Type::Unknown)],
        &[Type::String],
    );
    ins(
        b,
        "jsonQueryPage",
        &[
            p("text", Type::String),
            p("path", Type::String),
            p("offset", Type::Int),
            p("limit", Type::Int),
        ],
        &[optional_of(Type::Dictionary)],
    );
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
        &[optional_of(Type::String)],
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

    #[test]
    fn test_json_require_string_returns_optional_string() {
        let b = fresh();
        match b.get("jsonRequireString").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Optional(inner) => assert!(matches!(**inner, Type::String)),
                other => panic!("expected Optional<String>, got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }
}
