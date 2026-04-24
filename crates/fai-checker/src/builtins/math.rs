//! Math builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "mathRandom", &[], &[Type::Float]);
    ins(b, "mathFloor", &[p("value", Type::Float)], &[Type::Int]);
    ins(b, "mathCeil", &[p("value", Type::Float)], &[Type::Int]);
    ins(b, "mathRound", &[p("value", Type::Float)], &[Type::Int]);
    ins(b, "mathAbs", &[p("value", Type::Float)], &[Type::Float]);
    ins(
        b,
        "mathMin",
        &[p("a", Type::Float), p("b", Type::Float)],
        &[Type::Float],
    );
    ins(
        b,
        "mathMax",
        &[p("a", Type::Float), p("b", Type::Float)],
        &[Type::Float],
    );
    ins(b, "mathSqrt", &[p("value", Type::Float)], &[Type::Float]);
    ins(
        b,
        "mathPow",
        &[p("base", Type::Float), p("exp", Type::Float)],
        &[Type::Float],
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
    fn test_math_builtins() {
        let b = fresh();
        for name in &[
            "mathRandom",
            "mathFloor",
            "mathCeil",
            "mathRound",
            "mathAbs",
            "mathMin",
            "mathMax",
            "mathSqrt",
            "mathPow",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_math_random_returns_float() {
        let b = fresh();
        match b.get("mathRandom").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 0);
                assert!(matches!(sig.returns[0], Type::Float));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_math_floor_returns_int() {
        let b = fresh();
        match b.get("mathFloor").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.params[0].ty, Type::Float));
                assert!(matches!(sig.returns[0], Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_math_min_takes_two_args() {
        let b = fresh();
        match b.get("mathMin").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::Float));
            }
            _ => panic!("expected Function"),
        }
    }
}
