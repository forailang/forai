//! Concurrency builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // all() is variadic — for type checking we accept Unknown args and return Unknown.
    // The compiler wraps each arg in an implicit closure.
    ins(b, "all", &[p("tasks", Type::Unknown)], &[Type::Unknown]);
    // `sleep(ms)` is the timed-suspend primitive. There is no `wait` —
    // calls auto-await by default, so a `wait` spelling would read like an
    // await keyword. See language.md "Concurrency".
    ins(b, "sleep", &[p("ms", Type::Int)], &[Type::Void]);
    // `taskId()` — the current scheduler task's id (plan 133), -1 in a
    // sync (non-scheduler) module. The key for request/task-scoped state
    // that must survive cooperative yields (forui's per-request RPC
    // context uses it; a module-global alone bleeds across tasks).
    ins(b, "taskId", &[], &[Type::Int]);
    // `taskWaiterId(id)` — the task currently awaiting `id` (its parent
    // for auto-awaited calls), or -1 when none/detached/sync. Together
    // with taskId() this lets request-scoped state walk the await chain
    // so child tasks inherit their request's context.
    ins(b, "taskWaiterId", &[p("id", Type::Int)], &[Type::Int]);
    // Inherited request-context id (plan 133): the scheduler copies it
    // parent -> child at spawn, so every descendant of a stamped task
    // reports the same id. `taskContextId()` reads the current task's;
    // `setTaskContextId(id)` stamps it (framework use — Forui.rpc marks
    // each request's route task; -1 = none).
    ins(b, "taskContextId", &[], &[Type::Int]);
    ins(b, "setTaskContextId", &[p("id", Type::Int)], &[Type::Void]);
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
    fn test_concurrency_builtins() {
        let b = fresh();
        for name in &["all", "sleep"] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_wait_is_not_a_builtin() {
        // `wait` was removed in favour of `sleep`; calls auto-await by
        // default so there is no await-like `wait` keyword.
        let b = fresh();
        assert!(
            !b.contains_key("wait"),
            "wait should no longer be a builtin"
        );
    }

    #[test]
    fn test_sleep_takes_int() {
        let b = fresh();
        match b.get("sleep").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                assert!(matches!(sig.params[0].ty, Type::Int));
                assert!(matches!(sig.returns[0], Type::Void));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_all_returns_unknown() {
        let b = fresh();
        match b.get("all").unwrap() {
            Type::Function(sig) => {
                assert!(matches!(sig.returns[0], Type::Unknown));
            }
            _ => panic!("expected Function"),
        }
    }
}
