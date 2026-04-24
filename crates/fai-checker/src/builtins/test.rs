//! Test and mock builtins.

use super::{ins, ins_d, p, pd};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // Test assertions
    ins_d(
        b,
        "testAssert",
        &[p("condition", Type::Bool), pd("message", Type::String)],
        &[Type::Void],
    );
    ins_d(
        b,
        "testEqual",
        &[
            p("actual", Type::Unknown),
            p("expected", Type::Unknown),
            pd("message", Type::String),
        ],
        &[Type::Void],
    );
    ins_d(
        b,
        "testAssertFalse",
        &[p("condition", Type::Bool), pd("message", Type::String)],
        &[Type::Void],
    );
    ins_d(
        b,
        "testAssertNull",
        &[p("value", Type::Unknown), pd("message", Type::String)],
        &[Type::Void],
    );
    ins_d(
        b,
        "testAssertNotNull",
        &[p("value", Type::Unknown), pd("message", Type::String)],
        &[Type::Void],
    );

    // Mock/test utilities
    ins(
        b,
        "mock",
        &[p("target", Type::Unknown), p("value", Type::Unknown)],
        &[Type::Void],
    );
    ins(
        b,
        "mockOnce",
        &[p("target", Type::Unknown), p("value", Type::Unknown)],
        &[Type::Void],
    );
    ins(b, "mockReset", &[p("target", Type::Unknown)], &[Type::Void]);
    ins_d(
        b,
        "assertCalledWith",
        &[p("target", Type::Unknown), pd("value", Type::Unknown)],
        &[Type::Void],
    );
    ins(
        b,
        "assertCallCount",
        &[p("target", Type::Unknown), p("count", Type::Int)],
        &[Type::Void],
    );
    ins(
        b,
        "assertNotCalled",
        &[p("target", Type::Unknown)],
        &[Type::Void],
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
    fn test_assertion_builtins() {
        let b = fresh();
        for name in &["testAssert", "testEqual", "testAssertFalse"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_mock_builtins() {
        let b = fresh();
        for name in &["mock", "mockOnce", "mockReset"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_call_assertion_builtins() {
        let b = fresh();
        for name in &["assertCalledWith", "assertCallCount", "assertNotCalled"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_test_equal_has_optional_message() {
        let b = fresh();
        match b.get("testEqual").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 3);
                assert!(sig.params[2].has_default);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_assert_call_count_takes_int() {
        let b = fresh();
        match b.get("assertCallCount").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.params[1].ty, Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }
}
