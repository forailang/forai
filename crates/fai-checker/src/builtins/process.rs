//! Process builtins (std.process).
//!
//! Native-only command/session execution. The browser target reports
//! `process.available()` as false; the run/session functions are not
//! linked there (see `available_imports_with_test_flag` in fai-codegen-wasm).

use std::collections::HashMap;

use super::{ins, p};
use crate::types::*;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    ins(b, "processAvailable", &[], &[Type::Bool]);
    ins(
        b,
        "processRun",
        &[
            p("command", Type::String),
            p("cwd", Type::String),
            p("envJson", Type::String),
            p("timeoutMs", Type::Int),
            p("maxOutputBytes", Type::Int),
        ],
        &[Type::String],
    );
    ins(
        b,
        "processStart",
        &[
            p("command", Type::String),
            p("cwd", Type::String),
            p("envJson", Type::String),
            p("lifetimeMs", Type::Int),
        ],
        &[Type::String],
    );
    ins(
        b,
        "processWrite",
        &[p("sessionId", Type::String), p("input", Type::String)],
        &[Type::String],
    );
    ins(
        b,
        "processRead",
        &[p("sessionId", Type::String), p("maxOutputBytes", Type::Int)],
        &[Type::String],
    );
    ins(
        b,
        "processStop",
        &[p("sessionId", Type::String)],
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
    fn test_process_builtins_present() {
        let b = fresh();
        for name in &[
            "processAvailable",
            "processRun",
            "processStart",
            "processWrite",
            "processRead",
            "processStop",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_available_returns_bool() {
        let b = fresh();
        match b.get("processAvailable").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 0);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_run_takes_five_params_returns_string() {
        let b = fresh();
        match b.get("processRun").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 5);
                assert!(matches!(sig.returns[0], Type::String));
            }
            _ => panic!("expected Function"),
        }
    }
}
