//! FFI builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(
        b,
        "ffiAvailable",
        &[p("library", Type::String)],
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
    fn test_ffi_available_signature() {
        let b = fresh();
        assert!(b.contains_key("ffiAvailable"));
        match b.get("ffiAvailable").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.params[0].ty, Type::String));
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }
}
