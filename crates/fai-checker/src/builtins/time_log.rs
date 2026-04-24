//! Time, log, and CLI builtins.

use super::{ins, ins_d, p, pd};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // Time.
    //
    // `timeNow` returns milliseconds since the Unix epoch as a
    // Float. The prior declaration said `String` (suggested an ISO
    // 8601 surface) but no codegen path produced that — both the
    // direct and bytecode paths emit Float. Aligning the checker
    // with the runtime. `timeUnix` returns whole seconds as Int.
    ins(b, "timeNow", &[], &[Type::Float]);
    ins(b, "timeUnix", &[], &[Type::Int]);

    // Log
    ins(b, "logInfo", &[p("value", Type::Unknown)], &[Type::Void]);
    ins(b, "logWarn", &[p("value", Type::Unknown)], &[Type::Void]);
    ins(b, "logError", &[p("value", Type::Unknown)], &[Type::Void]);

    // CLI
    ins_d(
        b,
        "cliReadLine",
        &[pd("prompt", Type::String)],
        &[Type::String],
    );
    ins(b, "cliWrite", &[p("value", Type::Unknown)], &[Type::Void]);
    ins(
        b,
        "cliWriteLine",
        &[p("value", Type::Unknown)],
        &[Type::Void],
    );
    ins(b, "cliClear", &[], &[Type::Void]);
    ins(
        b,
        "cliMoveTo",
        &[p("row", Type::Int), p("column", Type::Int)],
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
    fn test_time_builtins() {
        let b = fresh();
        for name in &["timeNow", "timeUnix"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_log_builtins() {
        let b = fresh();
        for name in &["logInfo", "logWarn", "logError"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_cli_builtins() {
        let b = fresh();
        for name in &[
            "cliReadLine",
            "cliWrite",
            "cliWriteLine",
            "cliClear",
            "cliMoveTo",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_cli_read_line_has_optional_param() {
        let b = fresh();
        match b.get("cliReadLine").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(sig.params[0].has_default);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_time_now_takes_no_args() {
        let b = fresh();
        match b.get("timeNow").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 0);
                // Returns Float (ms since epoch) — see doc at the
                // declaration site for the checker/runtime history.
                assert!(matches!(sig.returns[0], Type::Float));
            }
            _ => panic!("expected Function"),
        }
    }
}
