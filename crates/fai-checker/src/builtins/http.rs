//! HTTP request and server builtins.

use super::{ins, p, pd};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // HTTP request
    let response_type = named_type("Response", NamedCategory::Type);
    ins(
        b,
        "httpRequestGet",
        &[p("url", Type::String), pd("headers", Type::Dictionary)],
        &[response_type.clone()],
    );
    ins(
        b,
        "httpRequestPost",
        &[
            p("url", Type::String),
            p("body", Type::String),
            pd("headers", Type::Dictionary),
        ],
        &[response_type.clone()],
    );
    ins(
        b,
        "httpRequestPut",
        &[
            p("url", Type::String),
            p("body", Type::String),
            pd("headers", Type::Dictionary),
        ],
        &[response_type.clone()],
    );
    ins(
        b,
        "httpRequestPatch",
        &[
            p("url", Type::String),
            p("body", Type::String),
            pd("headers", Type::Dictionary),
        ],
        &[response_type.clone()],
    );
    ins(
        b,
        "httpRequestDelete",
        &[p("url", Type::String), pd("headers", Type::Dictionary)],
        &[response_type],
    );

    // HTTP server — response builders
    ins(
        b,
        "httpServerOk",
        &[p("body", Type::String)],
        &[Type::Dictionary],
    );
    ins(
        b,
        "httpServerText",
        &[p("status", Type::Int), p("body", Type::String)],
        &[Type::Dictionary],
    );
    ins(
        b,
        "httpServerHtml",
        &[p("status", Type::Int), p("body", Type::String)],
        &[Type::Dictionary],
    );
    ins(
        b,
        "httpServerJson",
        &[p("status", Type::Int), p("value", Type::Unknown)],
        &[Type::Dictionary],
    );
    ins(
        b,
        "httpServerRedirect",
        &[p("status", Type::Int), p("url", Type::String)],
        &[Type::Dictionary],
    );
    // Kept for backward compatibility (not exported from std.http.server module anymore)
    ins(
        b,
        "httpServerListen",
        &[
            p("port", Type::Int),
            p(
                "using",
                function_type(
                    "httpRequestHandler",
                    vec![param(
                        "request",
                        named_type("HttpRequest", NamedCategory::Type),
                    )],
                    vec![Type::Dictionary],
                ),
            ),
        ],
        &[Type::Void],
    );

    // HTTP server — Router API
    let router_type = named_type("Router", NamedCategory::Type);
    let http_request_type = named_type("HttpRequest", NamedCategory::Type);
    let route_handler = function_type(
        "routeHandler",
        vec![param("request", http_request_type)],
        vec![Type::Dictionary],
    );

    // server.router() -> Router
    ins(b, "httpServerRouter", &[], &[router_type.clone()]);

    // router.get(pattern, handler) -> Void
    ins(
        b,
        "httpServerRouterGet",
        &[
            p("router", router_type.clone()),
            p("pattern", Type::String),
            p("handler", route_handler.clone()),
        ],
        &[Type::Void],
    );

    // router.post(pattern, handler) -> Void
    ins(
        b,
        "httpServerRouterPost",
        &[
            p("router", router_type.clone()),
            p("pattern", Type::String),
            p("handler", route_handler),
        ],
        &[Type::Void],
    );

    // router.serveFiles(dir) -> Void
    ins(
        b,
        "httpServerRouterServeFiles",
        &[p("router", router_type.clone()), p("dir", Type::String)],
        &[Type::Void],
    );

    // router.listen(port) -> Void
    ins(
        b,
        "httpServerRouterListen",
        &[p("router", router_type), p("port", Type::Int)],
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
    fn test_http_request_builtins() {
        let b = fresh();
        for name in &[
            "httpRequestGet",
            "httpRequestPost",
            "httpRequestPut",
            "httpRequestPatch",
            "httpRequestDelete",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_http_server_builtins() {
        let b = fresh();
        for name in &[
            "httpServerOk",
            "httpServerText",
            "httpServerHtml",
            "httpServerJson",
            "httpServerRedirect",
            "httpServerListen",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_http_request_get_returns_response() {
        let b = fresh();
        match b.get("httpRequestGet").unwrap() {
            Type::Function(sig) => match &sig.returns[0] {
                Type::Named { name, .. } => assert_eq!(name, "Response"),
                _ => panic!("expected Named"),
            },
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_http_server_listen_takes_handler_function() {
        let b = fresh();
        match b.get("httpServerListen").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.params[1].ty, Type::Function(_)));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_http_request_post_takes_body() {
        let b = fresh();
        match b.get("httpRequestPost").unwrap() {
            Type::Function(sig) => {
                // url, body, headers (optional)
                assert_eq!(sig.params.len(), 3);
                assert_eq!(sig.params[0].name, "url");
                assert_eq!(sig.params[1].name, "body");
                assert_eq!(sig.params[2].name, "headers");
                assert!(sig.params[2].has_default);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_http_request_get_has_optional_headers() {
        let b = fresh();
        match b.get("httpRequestGet").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert_eq!(sig.params[1].name, "headers");
                assert!(sig.params[1].has_default);
                assert!(matches!(sig.params[1].ty, Type::Dictionary));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_all_request_methods_have_headers() {
        let b = fresh();
        for (name, expected_params) in &[
            ("httpRequestGet", 2usize),
            ("httpRequestPost", 3),
            ("httpRequestPut", 3),
            ("httpRequestPatch", 3),
            ("httpRequestDelete", 2),
        ] {
            match b.get(*name).unwrap() {
                Type::Function(sig) => {
                    assert_eq!(
                        sig.params.len(),
                        *expected_params,
                        "{} should have {} params",
                        name,
                        expected_params
                    );
                    let last = sig.params.last().unwrap();
                    assert_eq!(
                        last.name, "headers",
                        "{} last param should be headers",
                        name
                    );
                    assert!(last.has_default, "{} headers should have default", name);
                }
                _ => panic!("{} should be Function", name),
            }
        }
    }
}
