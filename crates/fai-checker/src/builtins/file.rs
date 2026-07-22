//! File and path builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // File
    ins(b, "fileRead", &[p("path", Type::String)], &[Type::String]);
    ins(
        b,
        "fileWrite",
        &[p("path", Type::String), p("text", Type::String)],
        &[Type::Bool],
    );
    ins(b, "fileExists", &[p("path", Type::String)], &[Type::Bool]);
    ins(b, "fileDelete", &[p("path", Type::String)], &[Type::Bool]);
    ins(
        b,
        "fileList",
        &[p("path", Type::String)],
        &[array_of(Type::String)],
    );

    // Path
    ins(
        b,
        "pathJoin",
        &[p("left", Type::String), p("right", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "pathDirname",
        &[p("path", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "pathBasename",
        &[p("path", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "pathExtname",
        &[p("path", Type::String)],
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
    fn test_file_builtins() {
        let b = fresh();
        for name in &["fileRead", "fileWrite", "fileExists", "fileDelete", "fileList"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_path_builtins() {
        let b = fresh();
        for name in &["pathJoin", "pathDirname", "pathBasename", "pathExtname"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_file_list_returns_string_array() {
        let b = fresh();
        match b.get("fileList").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Array(elem) => assert!(matches!(**elem, Type::String)),
                _ => panic!("expected Array"),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_file_write_returns_bool() {
        let b = fresh();
        match b.get("fileWrite").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }
}
