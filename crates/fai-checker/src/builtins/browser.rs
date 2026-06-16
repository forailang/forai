//! Browser, HTML, and RPC builtins.

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // Browser
    ins(b, "setHtml", &[p("html", Type::String)], &[Type::Void]);
    ins(
        b,
        "setHtmlAt",
        &[p("selector", Type::String), p("html", Type::String)],
        &[Type::Void],
    );
    // Browser router integration
    ins(b, "getLocationPath", &[], &[Type::String]);
    ins(
        b,
        "pushHistoryState",
        &[p("path", Type::String)],
        &[Type::Void],
    );
    ins(
        b,
        "replaceLocation",
        &[p("path", Type::String)],
        &[Type::Void],
    );

    // HTML
    ins(b, "htmlEscape", &[p("text", Type::String)], &[Type::String]);

    // RPC -- returns Unknown because the actual type depends on the remote function
    ins(
        b,
        "remoteCall",
        &[
            p("url", Type::String),
            p("fn", Type::String),
            p("argsJson", Type::String),
            p("hash", Type::String),
        ],
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
    fn test_browser_builtins() {
        let b = fresh();
        for name in &[
            "setHtml",
            "setHtmlAt",
            "htmlEscape",
            "remoteCall",
            "replaceLocation",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_remote_call_signature() {
        let b = fresh();
        match b.get("remoteCall").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 4);
                assert!(matches!(sig.returns[0], Type::Unknown));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_set_html_at_takes_selector_and_html() {
        let b = fresh();
        match b.get("setHtmlAt").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::Void));
            }
            _ => panic!("expected Function"),
        }
    }
}
