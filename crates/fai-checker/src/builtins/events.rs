//! Event registry builtins — type signatures for `std.events`.
//!
//! `emit(name, data)` synchronously delivers an `Event { name, data }`
//! to every subscriber registered under `name`. Subscribers run in
//! registration order on a snapshot taken at the start of `emit`, so
//! adding or removing subscribers mid-dispatch does not perturb the
//! current pass.
//!
//! `on(name, handler) -> Subscription` and `once(name, handler)
//! -> Subscription` register a closure. The returned handle is
//! self-describing — it carries the event name internally — so
//! `off(sub)` works without the caller knowing which name it was
//! registered against.
//!
//! The host-side registry lives in
//! `fai-cli/src/wasm_runner/host/events.rs`.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    let event_type = named_type("Event", NamedCategory::Type);
    let subscription_type = named_type("Subscription", NamedCategory::Type);
    let handler_type = function_type(
        "eventHandler",
        vec![param("event", event_type)],
        vec![Type::Void],
    );

    ins(
        b,
        "eventOn",
        &[p("name", Type::String), p("handler", handler_type.clone())],
        &[subscription_type.clone()],
    );
    ins(
        b,
        "eventOnce",
        &[p("name", Type::String), p("handler", handler_type)],
        &[subscription_type.clone()],
    );
    ins(
        b,
        "eventOff",
        &[p("subscription", subscription_type)],
        &[Type::Bool],
    );
    ins(
        b,
        "eventEmit",
        &[p("name", Type::String), p("data", Type::Unknown)],
        &[Type::Void],
    );
    ins(
        b,
        "eventSubscribers",
        &[p("name", Type::String)],
        &[Type::Int],
    );
    ins(b, "eventClear", &[p("name", Type::String)], &[Type::Void]);
    ins(b, "eventClearAll", &[], &[Type::Void]);
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
    fn registers_all_event_builtins() {
        let b = fresh();
        for name in &[
            "eventOn",
            "eventOnce",
            "eventOff",
            "eventEmit",
            "eventSubscribers",
            "eventClear",
            "eventClearAll",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn event_on_returns_subscription() {
        let b = fresh();
        match b.get("eventOn").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Named { name, .. } => assert_eq!(name, "Subscription"),
                other => panic!("expected Named('Subscription'), got {:?}", other),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn event_off_takes_subscription_returns_bool() {
        let b = fresh();
        match b.get("eventOff").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 1);
                match &sig.params[0].ty {
                    Type::Named { name, .. } => assert_eq!(name, "Subscription"),
                    other => panic!("expected Named, got {:?}", other),
                }
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn event_emit_takes_unknown_data() {
        let b = fresh();
        match b.get("eventEmit").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.params[1].ty, Type::Unknown));
                assert!(matches!(sig.returns[0], Type::Void));
            }
            _ => panic!("expected Function"),
        }
    }
}
