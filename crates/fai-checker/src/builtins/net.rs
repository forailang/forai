//! Networking builtins (TCP, UDP, std.net).

use super::{ins, p};
use crate::types::*;
use std::collections::HashMap;

pub(super) fn install(b: &mut HashMap<String, Type>) {
    // std.net
    ins(b, "netAvailable", &[], &[Type::Bool]);

    // std.net.tcp
    ins(b, "netTcpListen", &[p("port", Type::Int)], &[Type::Int]);
    ins(
        b,
        "netTcpAccept",
        &[p("listener", Type::Int)],
        &[Type::Dictionary],
    );
    ins(
        b,
        "netTcpConnect",
        &[p("host", Type::String), p("port", Type::Int)],
        &[Type::Int],
    );
    ins(b, "netTcpRead", &[p("conn", Type::Int)], &[Type::String]);
    ins(
        b,
        "netTcpReadLine",
        &[p("conn", Type::Int)],
        &[Type::String],
    );
    ins(
        b,
        "netTcpWrite",
        &[p("conn", Type::Int), p("data", Type::String)],
        &[Type::Int],
    );
    ins(b, "netTcpClose", &[p("handle", Type::Int)], &[Type::Void]);
    ins(b, "netTcpAddress", &[p("conn", Type::Int)], &[Type::String]);

    // std.net.udp
    ins(b, "netUdpBind", &[p("port", Type::Int)], &[Type::Int]);
    ins(
        b,
        "netUdpSend",
        &[
            p("socket", Type::Int),
            p("host", Type::String),
            p("port", Type::Int),
            p("data", Type::String),
        ],
        &[Type::Int],
    );
    ins(
        b,
        "netUdpReceive",
        &[p("socket", Type::Int)],
        &[Type::Dictionary],
    );
    ins(b, "netUdpClose", &[p("socket", Type::Int)], &[Type::Void]);
    ins(
        b,
        "netUdpBroadcast",
        &[p("socket", Type::Int), p("enabled", Type::Bool)],
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
    fn test_net_available() {
        let b = fresh();
        assert!(b.contains_key("netAvailable"));
        match b.get("netAvailable").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 0);
                assert!(matches!(sig.returns[0], Type::Bool));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_tcp_builtins() {
        let b = fresh();
        for name in &[
            "netTcpListen",
            "netTcpAccept",
            "netTcpConnect",
            "netTcpRead",
            "netTcpReadLine",
            "netTcpWrite",
            "netTcpClose",
            "netTcpAddress",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_udp_builtins() {
        let b = fresh();
        for name in &[
            "netUdpBind",
            "netUdpSend",
            "netUdpReceive",
            "netUdpClose",
            "netUdpBroadcast",
        ] {
            assert!(b.contains_key(*name), "missing: {}", name);
        }
    }

    #[test]
    fn test_udp_send_takes_four_args() {
        let b = fresh();
        match b.get("netUdpSend").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 4);
                assert!(matches!(sig.returns[0], Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_tcp_connect_returns_int_handle() {
        let b = fresh();
        match b.get("netTcpConnect").unwrap() {
            Type::Function(sig) => {
                assert_eq!(sig.params.len(), 2);
                assert!(matches!(sig.returns[0], Type::Int));
            }
            _ => panic!("expected Function"),
        }
    }
}
