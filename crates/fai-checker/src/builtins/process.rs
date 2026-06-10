//! Process builtins.

use std::collections::HashMap;

use super::{ins, p};
use crate::types::*;

pub(super) fn install(b: &mut HashMap<String, Type>) {
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
