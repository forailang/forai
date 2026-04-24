//! Resource-limit registry shared by the compiler and WASM codegen.
//!
//! The compiler and the WASM codegen allocate slots in countable tables:
//! registers (u16 per function), parameters (u16), upvalues (u16), call
//! arguments (u16), string/constant pools (u16), WASM native method-id
//! table (u8), runtime call depth, etc. Each of these has historically
//! had its own ad-hoc check, its own error message, and sometimes no
//! check at all — we've been bitten in production (partners explorer)
//! by silent truncation, confusing Rust panics, and error messages that
//! don't tell the user what to do.
//!
//! This module centralises every limit into one table so:
//!
//! 1. Every countable resource surfaces the same error shape
//!    (`LimitExceeded`) with a name, the attempted value, the cap, the
//!    call-site context, and an actionable fix hint.
//! 2. Adding a new countable resource is a single `ResourceLimit`
//!    constant + one row in `ALL_LIMITS`, and the boundary test suite
//!    automatically exercises it.
//! 3. Phase-2 cap widening (e.g. registers u8 → u16) flips one value
//!    here and the rest of the codebase, including the test suite,
//!    retargets automatically.

/// Where the limit applies — used by test harnesses to pick the right
/// compilation driver, and by error messages to scope the fix hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    /// Counts something inside a single compiled function proto.
    /// Registers, parameters, upvalues, per-function constants.
    PerFunction,
    /// Counts operands on a single call instruction (arg_count).
    PerCall,
    /// Counts something across the whole compiled program / module.
    /// String pool, WASM method-id table, import slots.
    PerModule,
    /// Counts something at runtime per call frame or stack depth.
    PerFrame,
    /// Counts something across the whole runtime heap (memory pressure).
    PerHeap,
}

/// A single countable resource. Constants for each live below.
///
/// The `backends` field is gone in Plan 93 Phase F — wasm is the only
/// runtime, so every limit applies unconditionally.
#[derive(Clone, Copy, Debug)]
pub struct ResourceLimit {
    /// Lower-case name used in error messages. Grep-friendly.
    pub name: &'static str,
    /// Where the limit counts.
    pub scope: LimitScope,
    /// Inclusive maximum. e.g. `255` for a u8 index.
    pub cap: usize,
    /// One-sentence, actionable fix hint shown in the error.
    pub fix_hint: &'static str,
}

// ── Limit inventory ──────────────────────────────────────────────

/// Registers per function proto. `FnProto::max_registers: u16` →
/// indices 0..=65534 (Phase-2 widened from u8). Trips when a single
/// function's peak live-register pressure exceeds 65535 — effectively
/// unreachable in hand-written code; kept only as a sanity stop on
/// runaway code generation.
pub const REGISTERS: ResourceLimit = ResourceLimit {
    name: "registers",
    scope: LimitScope::PerFunction,
    cap: 65535,
    fix_hint: "hoist subexpressions into `let` bindings so the compiler can reuse register slots, or split this function into smaller helpers",
};

/// Parameters per function declaration. `FnProto::param_count: u16`.
pub const PARAMETERS: ResourceLimit = ResourceLimit {
    name: "parameters",
    scope: LimitScope::PerFunction,
    cap: 65535,
    fix_hint:
        "collect related parameters into a typed record and pass one value, or split the function",
};

/// Upvalues per closure. `FnProto::upvalue_count: u16`.
pub const UPVALUES: ResourceLimit = ResourceLimit {
    name: "upvalues",
    scope: LimitScope::PerFunction,
    cap: 65535,
    fix_hint: "capture a single record of the needed values instead of many individual upvalues",
};

/// Positional + labelled arguments per call instruction. Phase-2: the
/// underlying opcode operand widened u8→u16, so the cap is now 65535.
pub const CALL_ARGS: ResourceLimit = ResourceLimit {
    name: "call arguments",
    scope: LimitScope::PerCall,
    cap: 65535,
    fix_hint: "collect the arguments into a Dictionary or Array and unpack inside the function",
};

/// String-pool slots per module. `intern_string` returns u16.
pub const STRING_POOL: ResourceLimit = ResourceLimit {
    name: "string pool",
    scope: LimitScope::PerModule,
    cap: 65535,
    fix_hint: "deduplicate or factor out repeated string literals; move static data into a single resource",
};

/// Constants per function. `add_constant` returns u16.
pub const CONSTANTS: ResourceLimit = ResourceLimit {
    name: "constants",
    scope: LimitScope::PerFunction,
    cap: 65535,
    fix_hint: "split the function or share constants across calls via module-level lets",
};

/// WASM native-method dispatch table. METHOD_* constants in
/// `fai-codegen-wasm/src/runtime.rs`. u8 method_id slot.
pub const METHOD_IDS: ResourceLimit = ResourceLimit {
    name: "wasm native method ids",
    scope: LimitScope::PerModule,
    cap: 255,
    fix_hint: "add the new method via a different dispatch path, or widen the method_id encoding",
};

/// Runtime call-depth guard. Prevents recursion blowing the host
/// thread stack (Rust or wasmtime) and surfaces a clean error.
pub const CALL_DEPTH: ResourceLimit = ResourceLimit {
    name: "call depth",
    scope: LimitScope::PerFrame,
    cap: 1024,
    fix_hint: "reduce recursion depth or switch to an iterative form",
};

/// Every limit in one list so the boundary-parity test harness can
/// loop. Add new limits both as a `pub const` and in this slice.
pub const ALL_LIMITS: &[&'static ResourceLimit] = &[
    &REGISTERS,
    &PARAMETERS,
    &UPVALUES,
    &CALL_ARGS,
    &STRING_POOL,
    &CONSTANTS,
    &METHOD_IDS,
    &CALL_DEPTH,
];

// ── Source context ───────────────────────────────────────────────

/// Structured call-site description attached to a `LimitExceeded`.
/// Fields are individually optional so partial info (line but no
/// column, function name but no file) degrades gracefully. Editors
/// and agents can pattern-match the `Display` output (`path:line:col`)
/// to jump to the offending spot.
///
/// Build via the fluent helpers:
///
/// ```
/// use fai_core::limits::SourceContext;
/// let ctx = SourceContext::at(12, 3).in_function("parseGameObject");
/// ```
#[derive(Clone, Debug, Default)]
pub struct SourceContext {
    /// Name of the function being compiled (or `<script>` for top-
    /// level). Empty `None` when unknown.
    pub function_name: Option<String>,
    /// Absolute or project-relative file path. `None` when the
    /// compiler doesn't track it for this compilation unit (tests,
    /// synthesised sources).
    pub file: Option<String>,
    /// 1-based source line. `0` = unknown.
    pub line: u32,
    /// 1-based source column. `0` = unknown.
    pub column: u32,
    /// Free-form additional context (e.g. "48 arguments + callee
    /// won't fit in the register window"). Shown after the location
    /// in the error message.
    pub note: String,
}

impl SourceContext {
    /// No location known. Use when the overflow surfaces at a point
    /// that doesn't correspond to any specific source line (module
    /// registration, top-of-compile state).
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Line only — most common. Column defaults to `0` (unknown).
    pub fn at_line(line: u32) -> Self {
        Self {
            line,
            ..Self::default()
        }
    }

    /// Full (line, column). Both 1-based.
    pub fn at(line: u32, column: u32) -> Self {
        Self {
            line,
            column,
            ..Self::default()
        }
    }

    /// Attach the enclosing function name. Fluent builder — chain
    /// onto `at` / `at_line`.
    pub fn in_function(mut self, name: impl Into<String>) -> Self {
        self.function_name = Some(name.into());
        self
    }

    /// Attach the source file path. Keep it relative to the project
    /// root when possible so errors round-trip across machines.
    pub fn in_file(mut self, file: impl Into<String>) -> Self {
        self.file = Some(file.into());
        self
    }

    /// Attach free-form additional context. Printed after the location.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    /// True when no structured fields are set. Used by `Display` to
    /// pick the short error format.
    pub fn is_empty(&self) -> bool {
        self.function_name.is_none()
            && self.file.is_none()
            && self.line == 0
            && self.column == 0
            && self.note.is_empty()
    }

    /// Render the location as `"in function `foo` at file.fai:12:3"`
    /// or partial variants when fields are missing. Never returns an
    /// empty string when `is_empty` is false.
    fn render_location(&self) -> String {
        let mut parts = Vec::new();
        if let Some(name) = &self.function_name {
            parts.push(format!("in function `{name}`"));
        }
        let loc_suffix = match (&self.file, self.line, self.column) {
            (Some(f), line, col) if line > 0 && col > 0 => Some(format!("at {f}:{line}:{col}")),
            (Some(f), line, _) if line > 0 => Some(format!("at {f}:{line}")),
            (Some(f), _, _) => Some(format!("at {f}")),
            (None, line, col) if line > 0 && col > 0 => Some(format!("at line {line}:{col}")),
            (None, line, _) if line > 0 => Some(format!("at line {line}")),
            _ => None,
        };
        if let Some(s) = loc_suffix {
            parts.push(s);
        }
        parts.join(" ")
    }
}

// ── Check helper ─────────────────────────────────────────────────

/// The uniform overflow error shape. Compile-time and run-time paths
/// convert this into their own error types but preserve the `Display`
/// output so users (and agents) see the same fix hint.
///
/// Display format:
///   `"<limit>: limit exceeded (N > CAP) <in function `fn`> <at file:line:col> — <note>. <fix_hint>"`
///
/// Any missing location piece is omitted cleanly.
#[derive(Clone, Debug)]
pub struct LimitExceeded {
    pub limit: &'static ResourceLimit,
    /// What the caller tried to push the counter to. Always > cap.
    pub attempted: usize,
    /// Structured source context for editors/agents to locate the
    /// offending expression.
    pub source: SourceContext,
}

impl std::fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: limit exceeded ({} > {})",
            self.limit.name, self.attempted, self.limit.cap,
        )?;
        let loc = self.source.render_location();
        if !loc.is_empty() {
            write!(f, " {loc}")?;
        }
        if !self.source.note.is_empty() {
            write!(f, " — {}", self.source.note)?;
        }
        write!(f, ". {}", self.limit.fix_hint)
    }
}

impl std::error::Error for LimitExceeded {}

/// Returns Ok(()) when `attempted <= limit.cap`, otherwise a populated
/// `LimitExceeded` that the caller surfaces through its own error
/// type. Pass `SourceContext::unknown()` when no location is known.
pub fn check(
    limit: &'static ResourceLimit,
    attempted: usize,
    source: SourceContext,
) -> Result<(), LimitExceeded> {
    if attempted > limit.cap {
        Err(LimitExceeded {
            limit,
            attempted,
            source,
        })
    } else {
        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_at_cap_is_ok() {
        for limit in ALL_LIMITS {
            assert!(
                check(limit, limit.cap, SourceContext::unknown()).is_ok(),
                "{}: cap ({}) should be the largest accepted value",
                limit.name,
                limit.cap,
            );
        }
    }

    #[test]
    fn check_over_cap_produces_limit_exceeded() {
        for limit in ALL_LIMITS {
            let err = check(
                limit,
                limit.cap + 1,
                SourceContext::at_line(42).with_note("fixture"),
            )
            .expect_err("over-cap must be a LimitExceeded");
            assert_eq!(err.limit.name, limit.name);
            assert_eq!(err.attempted, limit.cap + 1);
            assert_eq!(err.source.line, 42);
            assert_eq!(err.source.note, "fixture");
        }
    }

    #[test]
    fn display_without_source_includes_name_cap_and_hint() {
        let err = check(&REGISTERS, 65540, SourceContext::unknown()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("registers"), "should name the limit: {msg}");
        assert!(msg.contains("65540"), "should show attempted value: {msg}");
        assert!(msg.contains("65535"), "should show the cap: {msg}");
        assert!(
            msg.contains(REGISTERS.fix_hint),
            "should include fix hint: {msg}"
        );
        // No trailing location noise when nothing is known.
        assert!(!msg.contains("at line"), "no phantom location: {msg}");
        assert!(!msg.contains("in function"), "no phantom function: {msg}");
    }

    #[test]
    fn display_with_full_source_context_includes_all_pieces() {
        let src = SourceContext::at(12, 3)
            .in_function("parseGameObject")
            .in_file("src/server/main.fai")
            .with_note("10 fields + nested unwraps");
        let err = check(&REGISTERS, 65540, src).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("parseGameObject"), "function: {msg}");
        assert!(
            msg.contains("src/server/main.fai:12:3"),
            "file:line:col: {msg}"
        );
        assert!(msg.contains("10 fields"), "note: {msg}");
        assert!(msg.contains(REGISTERS.fix_hint), "fix hint: {msg}");
    }

    #[test]
    fn display_with_line_only_falls_back_to_at_line_format() {
        // Backwards-compatible: when only line is known, the error
        // reads `"at line N"` so editors that don't know the file can
        // still jump via the current buffer.
        let err = check(&CALL_DEPTH, 2000, SourceContext::at_line(42)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("at line 42"), "expected `at line 42`: {msg}",);
    }

    #[test]
    fn display_with_function_but_no_line_renders_function_only() {
        let err = check(
            &REGISTERS,
            65540,
            SourceContext::default().in_function("parseRow"),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("in function `parseRow`"), "{msg}");
        assert!(!msg.contains("at line"), "no phantom line: {msg}");
    }

    #[test]
    fn all_limits_list_is_in_sync_with_constants() {
        // Sanity: catch the case where someone adds a `pub const X:
        // ResourceLimit = ...` but forgets to add it to `ALL_LIMITS`.
        // We can't enumerate constants via reflection, so instead
        // assert the inventory's size + names stay as expected here.
        // Updating this list is intentional when a new limit lands.
        let names: Vec<&'static str> = ALL_LIMITS.iter().map(|l| l.name).collect();
        assert_eq!(
            names,
            vec![
                "registers",
                "parameters",
                "upvalues",
                "call arguments",
                "string pool",
                "constants",
                "wasm native method ids",
                "call depth",
            ],
            "ALL_LIMITS order / contents changed — update this test intentionally when adding a new limit",
        );
    }

    // `wasm_only_limits_are_tagged` was removed in Plan 93 Phase F:
    // the `Backend` enum and per-limit `backends` tag came from the
    // days of VM-vs-wasm parity testing; wasm is now the only runtime.
}
