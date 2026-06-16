//! Ownership ABI for ARC (plan 117).
//!
//! Reference counting in forai works today, but ownership is encoded in
//! scattered codegen decisions: some expression shapes are treated as
//! fresh/owned, some std calls are whitelisted by name, some temporaries are
//! released at call sites. The failure mode is that each new feature finds a
//! new leak. This module is the single source of truth that replaces the
//! scattered whitelists: every callable the compiler knows by name has an
//! ownership signature here, and codegen reads it instead of re-deriving
//! freshness per expression kind.
//!
//! `fai-compiler` is the one crate both consumers already depend on:
//! `fai-codegen-wasm` reads the table to emit retains/releases, and
//! `fai-cli`'s wasm_runner reads it to install host imports. Keeping the
//! definitions here is what stops the conventions from drifting between the
//! two crates.
//!
//! Phase 1 (this commit) adds the vocabulary and the table, and has codegen
//! consult it for *logging only* — the existing heuristics still drive
//! behavior. Phase 3 swaps the heuristics for table lookups once the table is
//! proven equivalent over the test corpus.

/// Ownership of a value an expression leaves on the stack.
///
/// "Owned" does not mean unique — it means this code holds exactly one
/// reference-count credit and must eventually transfer or release it.
///
/// Phase-1 scaffolding: defined now as the shared vocabulary, but codegen
/// only begins annotating expression results with it in phase 3 (plan 117).
/// No consumer until then — do not prune as dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprOwnership {
    /// No heap retain credit: numbers, booleans, null, other immediates.
    /// No retain/release is ever emitted for these.
    Primitive,
    /// A heap handle the current code may read during the current operation
    /// but does not own. To store, return, capture, or otherwise outlive the
    /// operation, it must first be retained.
    Borrowed,
    /// One retain credit transferred to the current code. It must be
    /// stored/returned/passed to something that consumes the credit, or
    /// released.
    Owned,
}

/// Stable operation IDs emitted by phase-4 ownership helpers.
///
/// These IDs are part of the native/browser diagnostic ABI. Append new
/// operations; do not renumber existing ones.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwnershipOp {
    Retain = 1,
    Release = 2,
    Transfer = 3,
    Borrow = 4,
    Store = 5,
    Overwrite = 6,
    Cleanup = 7,
    Return = 8,
    Discard = 9,
    CallArgument = 10,
}

impl OwnershipOp {
    pub const ALL: [OwnershipOp; 10] = [
        OwnershipOp::Retain,
        OwnershipOp::Release,
        OwnershipOp::Transfer,
        OwnershipOp::Borrow,
        OwnershipOp::Store,
        OwnershipOp::Overwrite,
        OwnershipOp::Cleanup,
        OwnershipOp::Return,
        OwnershipOp::Discard,
        OwnershipOp::CallArgument,
    ];

    pub const fn id(self) -> u32 {
        self as u32
    }

    pub const fn name(self) -> &'static str {
        match self {
            OwnershipOp::Retain => "retain",
            OwnershipOp::Release => "release",
            OwnershipOp::Transfer => "transfer",
            OwnershipOp::Borrow => "borrow",
            OwnershipOp::Store => "store",
            OwnershipOp::Overwrite => "overwrite",
            OwnershipOp::Cleanup => "cleanup",
            OwnershipOp::Return => "return",
            OwnershipOp::Discard => "discard",
            OwnershipOp::CallArgument => "call_argument",
        }
    }

    pub const fn from_id(id: u32) -> Option<Self> {
        match id {
            1 => Some(OwnershipOp::Retain),
            2 => Some(OwnershipOp::Release),
            3 => Some(OwnershipOp::Transfer),
            4 => Some(OwnershipOp::Borrow),
            5 => Some(OwnershipOp::Store),
            6 => Some(OwnershipOp::Overwrite),
            7 => Some(OwnershipOp::Cleanup),
            8 => Some(OwnershipOp::Return),
            9 => Some(OwnershipOp::Discard),
            10 => Some(OwnershipOp::CallArgument),
            _ => None,
        }
    }
}

/// Compact auxiliary payload carried by `__fai_ownership_event`.
///
/// The host import remains `event(op, site, value, aux)`. `op` keeps the
/// append-only operation id, `site` resolves to debug metadata, and `aux`
/// carries small operation-specific detail. Most helper events have no extra
/// payload, but closure captures use the upvalue index and host-call argument
/// events use the argument index/convention family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipAux {
    None = 0,
    ClosureCapture = 1,
    HostArgument = 2,
    AsyncFrameSlot = 3,
}

impl OwnershipAux {
    pub const fn id(self) -> i32 {
        self as i32
    }

    pub const fn encode(self, detail: u16) -> i32 {
        (self.id() << 16) | detail as i32
    }

    pub const fn from_id(id: i32) -> Option<Self> {
        match id {
            0 => Some(OwnershipAux::None),
            1 => Some(OwnershipAux::ClosureCapture),
            2 => Some(OwnershipAux::HostArgument),
            3 => Some(OwnershipAux::AsyncFrameSlot),
            _ => None,
        }
    }

    pub const fn decode(encoded: i32) -> Option<(Self, u16)> {
        let kind = encoded >> 16;
        let detail = (encoded & 0xffff) as u16;
        match Self::from_id(kind) {
            Some(aux) => Some((aux, detail)),
            None => None,
        }
    }
}

/// How a callee treats one argument it is passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgConvention {
    /// Read-only during the call; the callee must not keep the handle after
    /// the call returns. The caller still owns and releases any owned temp it
    /// created for the argument.
    Borrowed,
    /// The callee takes one retain credit from the caller. Passing an `Owned`
    /// value transfers it; passing a `Borrowed` value requires the caller to
    /// retain first. After the call the caller must NOT release this credit.
    Consumed,
    /// The callee may store the handle beyond the call, but must create its
    /// own retain credit before storing. The caller still owns and later
    /// releases any owned temp it passed.
    RetainedByCallee,
    /// The host decodes/copies the value into host-native memory and does not
    /// keep a guest handle (e.g. a path string copied out of guest memory).
    /// The caller still handles any owned temp it passed.
    CopiedByHost,
}

impl ArgConvention {
    pub const ALL: [ArgConvention; 4] = [
        ArgConvention::Borrowed,
        ArgConvention::Consumed,
        ArgConvention::RetainedByCallee,
        ArgConvention::CopiedByHost,
    ];

    pub const fn id(self) -> u32 {
        match self {
            ArgConvention::Borrowed => 1,
            ArgConvention::Consumed => 2,
            ArgConvention::RetainedByCallee => 3,
            ArgConvention::CopiedByHost => 4,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            ArgConvention::Borrowed => "borrowed",
            ArgConvention::Consumed => "consumed",
            ArgConvention::RetainedByCallee => "retained_by_callee",
            ArgConvention::CopiedByHost => "copied_by_host",
        }
    }

    pub const fn from_id(id: u32) -> Option<Self> {
        match id {
            1 => Some(ArgConvention::Borrowed),
            2 => Some(ArgConvention::Consumed),
            3 => Some(ArgConvention::RetainedByCallee),
            4 => Some(ArgConvention::CopiedByHost),
            _ => None,
        }
    }
}

/// How a callable hands its result back to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnConvention {
    /// A primitive (or null) — no retain credit. RC is a no-op on the result.
    Primitive,
    /// A heap handle tied to an existing owner the caller already has a
    /// lifetime for. Rare and internal-only until the compiler can enforce
    /// the lifetime; public heap returns should be `Owned`.
    Borrowed,
    /// The normal heap-return convention: the caller takes one retain credit.
    Owned,
    /// The result IS the n-th argument (0-based), returned in place — so the
    /// call's result ownership equals that argument expression's ownership.
    ///
    /// `set(dict, key, value)` returns its first arg mutated in place;
    /// `set({}, ..)` is owned (fresh dict), `set(d, ..)` on a borrowed var is
    /// borrowed. forui builds every component's props via chained `set`, so
    /// this must stay free of added retain/release traffic. Valid only for
    /// runtime intrinsics/builtins whose implementation provably returns that
    /// argument (currently only `set`); user functions and host imports may
    /// not declare it.
    PassThrough(usize),
}

/// A callable's full ownership signature: one convention per declared
/// argument plus the return convention. A `None` `args` means the argument
/// conventions are not yet modeled (return-only entry); callers must treat a
/// missing arg list conservatively, never as "all Borrowed".
#[derive(Debug, Clone)]
pub struct Signature {
    /// Per-argument conventions, positional. `None` until modeled.
    pub args: Option<Vec<ArgConvention>>,
    /// How the result is returned.
    pub ret: ReturnConvention,
    /// One-line human note replacing the inline ownership comments. Shown in
    /// instrumentation output.
    pub doc: &'static str,
}

impl Signature {
    const fn ret_only(ret: ReturnConvention, doc: &'static str) -> Self {
        Signature {
            args: None,
            ret,
            doc,
        }
    }
}

pub struct BareCallRow {
    pub name: &'static str,
    pub ret: ReturnConvention,
    pub doc: &'static str,
}

pub struct StdCallRow {
    pub canon: &'static str,
    pub method: &'static str,
    pub ret: ReturnConvention,
    pub doc: &'static str,
}

pub const BORROWED_BARE_CALLS: &[&str] = &[
    "unwrap",
    "get",
    "getString",
    "getInt",
    "getBool",
    "set",
    "message",
    "kind",
];

pub const FRESH_BARE_CALLS: &[&str] = &[
    "append",
    "getKeys",
    "map",
    "filter",
    "slice",
    "reverse",
    "sort",
    "split",
    "copy",
    "Error",
    "all",
    "trim",
    "trimStart",
    "trimEnd",
    "toUpper",
    "toLower",
    "substring",
    "repeat",
    "join",
    "replace",
    "toString",
    "jsonParse",
    "parse",
    "jsonStringify",
    "stringify",
    "getLocationPath",
    "__retain",
];

pub const PRIMITIVE_BARE_CALLS: &[&str] = &[
    "print",
    "__heapPtr",
    "__liveObjects",
    "__refcount",
    "__release",
    "isError",
    "is_int",
    "is_float",
    "is_null",
    "is_bool",
    "is_string",
    "is_array",
    "is_dict",
    "toInt",
    "toFloat",
    "parseInt",
    "parseFloat",
    "length",
    "isEmpty",
    "setHtml",
    "setHtmlAt",
    "pushHistoryState",
    "replaceLocation",
    "hasKey",
    "sleep",
    "mock",
    "mockOnce",
    "mockReset",
];

pub const BARE_CALL_SURFACE: &[BareCallRow] = &[
    BareCallRow {
        name: "set",
        ret: ReturnConvention::PassThrough(0),
        doc:
            "set(dict,key,value): returns arg0 mutated in place; result ownership follows the dict",
    },
    BareCallRow {
        name: "unwrap",
        ret: ReturnConvention::Owned,
        doc: "unwrap: compile_unwrap normalizes both paths to a uniform +1",
    },
    BareCallRow {
        name: "remoteCall",
        ret: ReturnConvention::Borrowed,
        doc: "TODO unverified async RPC plumbing — conservatively borrowed",
    },
];

pub const INLINE_STD_CALLS: &[StdCallRow] = &[
    StdCallRow {
        canon: "std.time",
        method: "unix",
        ret: ReturnConvention::Primitive,
        doc: "inline conversion from now_ms to boxed Int",
    },
    StdCallRow {
        canon: "std.convert",
        method: "toInt",
        ret: ReturnConvention::Primitive,
        doc: "type-aware conversion returns Int/null primitive",
    },
    StdCallRow {
        canon: "std.convert",
        method: "toFloat",
        ret: ReturnConvention::Primitive,
        doc: "type-aware conversion returns Float/null primitive",
    },
    StdCallRow {
        canon: "std.convert",
        method: "toString",
        ret: ReturnConvention::Owned,
        doc: "codegen normalizes RT_VALUE_TO_STR to a uniform +1 string",
    },
    StdCallRow {
        canon: "std.convert",
        method: "toBool",
        ret: ReturnConvention::Primitive,
        doc: "truthiness conversion returns Bool primitive",
    },
    StdCallRow {
        canon: "std.convert",
        method: "parseInt",
        ret: ReturnConvention::Primitive,
        doc: "parse helper returns Int/null primitive",
    },
    StdCallRow {
        canon: "std.convert",
        method: "parseFloat",
        ret: ReturnConvention::Primitive,
        doc: "parse helper returns Float/null primitive",
    },
    StdCallRow {
        canon: "std.error",
        method: "Error",
        ret: ReturnConvention::Owned,
        doc: "fresh error dict",
    },
    StdCallRow {
        canon: "std.error",
        method: "unwrap",
        ret: ReturnConvention::Owned,
        doc: "compile_unwrap normalizes both paths to a uniform +1",
    },
    StdCallRow {
        canon: "std.json",
        method: "requireString",
        ret: ReturnConvention::Owned,
        doc: "codegen retains the aliased dict field before releasing owned dict temps",
    },
    StdCallRow {
        canon: "std.math",
        method: "floor",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Int primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "ceil",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Int primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "round",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Int primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "abs",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Float primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "sqrt",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Float primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "min",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Float primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "max",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Float primitive",
    },
    StdCallRow {
        canon: "std.math",
        method: "pow",
        ret: ReturnConvention::Primitive,
        doc: "inline numeric helper returns Float primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "length",
        ret: ReturnConvention::Primitive,
        doc: "native method reads string length and returns Int primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "isEmpty",
        ret: ReturnConvention::Primitive,
        doc: "native method reads string length and returns Bool primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "replace",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "split",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh array",
    },
    StdCallRow {
        canon: "std.string",
        method: "trim",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "trimStart",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "trimEnd",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "toUpper",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "toLower",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "contains",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Bool primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "startsWith",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Bool primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "endsWith",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Bool primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "substring",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh/normalized +1 string",
    },
    StdCallRow {
        canon: "std.string",
        method: "indexOf",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Int primitive",
    },
    StdCallRow {
        canon: "std.string",
        method: "join",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh string",
    },
    StdCallRow {
        canon: "std.string",
        method: "repeat",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh string",
    },
    StdCallRow {
        canon: "std.array",
        method: "append",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh array",
    },
    StdCallRow {
        canon: "std.array",
        method: "length",
        ret: ReturnConvention::Primitive,
        doc: "native method reads array length and returns Int primitive",
    },
    StdCallRow {
        canon: "std.array",
        method: "isEmpty",
        ret: ReturnConvention::Primitive,
        doc: "native method reads array length and returns Bool primitive",
    },
    StdCallRow {
        canon: "std.array",
        method: "contains",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Bool primitive",
    },
    StdCallRow {
        canon: "std.array",
        method: "indexOf",
        ret: ReturnConvention::Primitive,
        doc: "native method returns Int primitive",
    },
    StdCallRow {
        canon: "std.array",
        method: "join",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh string",
    },
    StdCallRow {
        canon: "std.array",
        method: "sort",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh sorted array",
    },
    StdCallRow {
        canon: "std.array",
        method: "reverse",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh reversed array",
    },
    StdCallRow {
        canon: "std.array",
        method: "slice",
        ret: ReturnConvention::Owned,
        doc: "native method returns fresh array",
    },
    StdCallRow {
        canon: "std.array",
        method: "first",
        ret: ReturnConvention::Borrowed,
        doc: "native method returns borrowed element alias",
    },
    StdCallRow {
        canon: "std.array",
        method: "last",
        ret: ReturnConvention::Borrowed,
        doc: "native method returns borrowed element alias",
    },
];

/// Look up the ownership signature of a bare-name builtin / bare-global.
///
/// This mirrors the EFFECTIVE dispatch order of codegen's
/// `call_returns_owned`, NOT the membership of any single whitelist — `set`
/// and `unwrap` both appear in the borrowed-bare-global list today but are
/// overridden by earlier explicit checks. Building from list membership would
/// silently change their classification. The order below is the dispatch
/// order:
///
/// 1. `set` — `PassThrough(0)`.
/// 2. `unwrap` — `Owned` (codegen normalizes its two paths to a uniform +1).
/// 3. borrowed bare-globals — `Borrowed`.
/// 4. fresh-allocating builtins — `Owned`.
///
/// Returns `None` for names that are not statically classified here (user
/// functions, closures, type constructors, externs) — those are owned by the
/// `+1` return convention and resolved per-program by codegen, not by this
/// static table.
/// Debug-only seeded-absence hook (plan 119 U2, repurposed from the
/// plan-118 divergence seed). With `FAI_ABI_SEED=<import-name>` set, the
/// named host-import row is treated as ABSENT by [`lookup_host_import`]
/// — and only there — so CI can prove the missing-signature sentinel and
/// the checked-build error actually fire. It deliberately never affects
/// the classification lookups: post-swap those drive emitted code, and a
/// seed that changed codegen would invalidate the very build it tests.
/// Compiled only under `debug_assertions`; an unknown import name warns
/// and stays inactive.
#[cfg(debug_assertions)]
fn seeded_absent(import: &str) -> bool {
    use std::sync::OnceLock;
    static SEED: OnceLock<Option<String>> = OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let v = std::env::var("FAI_ABI_SEED").ok()?;
        if !HOST_IMPORTS.iter().any(|r| r.import == v) {
            eprintln!(
                "[abi-check] FAI_ABI_SEED='{}' names no host-import row — seed inactive",
                v
            );
        }
        Some(v)
    });
    seed.as_deref() == Some(import)
}

pub fn lookup_bare_call(name: &str) -> Option<Signature> {
    // 1. `set` returns its first arg, mutated in place.
    if name == "set" {
        return Some(Signature {
            // dict (mutated in place, borrowed view), key (copied), value
            // (the dict co-owns it — retained by the callee).
            args: Some(vec![
                ArgConvention::Borrowed,
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee,
            ]),
            ret: ReturnConvention::PassThrough(0),
            doc: "set(dict,key,value): returns arg0 mutated in place; result ownership follows the dict",
        });
    }
    // 2. `unwrap` returns one arg or the other, normalized to +1.
    if name == "unwrap" {
        return Some(Signature::ret_only(
            ReturnConvention::Owned,
            "unwrap: compile_unwrap normalizes both paths to a uniform +1",
        ));
    }
    if name == "remoteCall" {
        return Some(Signature::ret_only(
            ReturnConvention::Borrowed,
            "TODO unverified async RPC plumbing — conservatively borrowed",
        ));
    }
    if PRIMITIVE_BARE_CALLS.contains(&name) {
        return Some(Signature::ret_only(
            ReturnConvention::Primitive,
            "primitive bare-global: returns a primitive/null or void, not an owned object",
        ));
    }
    // 4. Borrowed bare-globals: element / field / argument reads, no fresh
    //    allocation. (`set`/`unwrap` already handled above.)
    if is_borrowed_bare_global(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Borrowed,
            "borrowed bare-global: returns an element/field/arg view, not a fresh allocation",
        ));
    }
    // 5. Fresh-allocating builtins: always a sole-owned +1.
    if is_fresh_builtin_call(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Owned,
            "fresh builtin: unconditionally allocates a new rc-prefixed object",
        ));
    }
    None
}

/// Look up the ownership signature of a method-position call — `recv.m(...)`,
/// whether UFCS or plain member access. Member dispatch differs from bare
/// dispatch: codegen's heuristic member arms check the borrowed bare-globals
/// FIRST, so member-position `set` / `unwrap` classify as `Borrowed` — not
/// the `PassThrough(0)` / `Owned` of their bare forms. Phase 1 mirrors that
/// effective behavior verbatim; reconciling the bare/member asymmetry is a
/// phase-3 decision to make at the heuristic-swap point, not a table edit.
pub fn lookup_member_call(name: &str) -> Option<Signature> {
    if is_borrowed_bare_global(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Borrowed,
            "member-position bare-global: member dispatch checks the borrowed list first",
        ));
    }
    if is_fresh_builtin_call(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Owned,
            "fresh builtin: unconditionally allocates a new rc-prefixed object",
        ));
    }
    None
}

/// One row of the boxed host-import surface (plan 119 U1): the std-module
/// call form when one exists (`canon`/`method`; empty `canon` for imports
/// reached outside the module-call syntax), the wasm `env` import name, and
/// the VERIFIED return convention. Every row's `doc` names what was read in
/// the host implementation to justify the convention — `Owned` is only
/// recorded where the host provably allocates fresh (`wasm_alloc_str`,
/// `heap::reserve`/`build_value`, `alloc_array_of`); aliasing returns are
/// `Borrowed`; unverified plumbing stays `Borrowed` with a TODO.
pub struct HostImportRow {
    pub canon: &'static str,
    pub method: &'static str,
    pub import: &'static str,
    pub ret: ReturnConvention,
    pub doc: &'static str,
}

use ReturnConvention::{Borrowed as RBor, Owned as ROwn, Primitive as RPrim};

pub const HOST_IMPORTS: &[HostImportRow] = &[
    // std.file
    HostImportRow {
        canon: "std.file",
        method: "read",
        import: "file_read_str",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str; null on error",
    },
    HostImportRow {
        canon: "std.file",
        method: "list",
        import: "file_list",
        ret: ROwn,
        doc: "fresh Array<String> graph via heap::build_value",
    },
    HostImportRow {
        canon: "std.file",
        method: "write",
        import: "write_file",
        ret: RPrim,
        doc: "encoded bool status; host copies path/content strings",
    },
    HostImportRow {
        canon: "std.file",
        method: "exists",
        import: "file_exists",
        ret: RPrim,
        doc: "encoded bool; host copies path string",
    },
    // std.process — every method returns a fresh status/output string.
    HostImportRow {
        canon: "std.process",
        method: "available",
        import: "process_available",
        ret: RPrim,
        doc: "encoded bool availability probe",
    },
    HostImportRow {
        canon: "std.process",
        method: "run",
        import: "process_run",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.process",
        method: "start",
        import: "process_start",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.process",
        method: "write",
        import: "process_write",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.process",
        method: "read",
        import: "process_read",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.process",
        method: "stop",
        import: "process_stop",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    // std.path
    HostImportRow {
        canon: "std.path",
        method: "join",
        import: "path_join",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.path",
        method: "basename",
        import: "path_basename",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.path",
        method: "dirname",
        import: "path_dirname",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.path",
        method: "extname",
        import: "path_extname",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    // std.math / std.time / std.log
    HostImportRow {
        canon: "std.math",
        method: "random",
        import: "random",
        ret: RPrim,
        doc: "host f64 converted to boxed Float primitive",
    },
    HostImportRow {
        canon: "std.time",
        method: "now",
        import: "now_ms",
        ret: RPrim,
        doc: "host f64 milliseconds converted to boxed Float primitive",
    },
    HostImportRow {
        canon: "std.log",
        method: "info",
        import: "log_info",
        ret: RPrim,
        doc: "void log call; host copies message string",
    },
    HostImportRow {
        canon: "std.log",
        method: "warn",
        import: "log_warn",
        ret: RPrim,
        doc: "void log call; host copies message string",
    },
    HostImportRow {
        canon: "std.log",
        method: "error",
        import: "log_error",
        ret: RPrim,
        doc: "void log call; host copies message string",
    },
    // std.env
    HostImportRow {
        canon: "std.env",
        method: "get",
        import: "env_get",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str; null when unset",
    },
    HostImportRow {
        canon: "std.env",
        method: "load",
        import: "env_load",
        ret: RPrim,
        doc: "encoded bool; host copies dotenv path string",
    },
    // std.events — the subscription dict is host-built fresh (heap::reserve
    // in build_subscription). Handler retention is the separate, pinned
    // phase-6 issue and does not change the RESULT's ownership.
    HostImportRow {
        canon: "std.events",
        method: "on",
        import: "event_on",
        ret: ROwn,
        doc: "fresh subscription dict via heap::reserve",
    },
    HostImportRow {
        canon: "std.events",
        method: "once",
        import: "event_once",
        ret: ROwn,
        doc: "fresh subscription dict via heap::reserve",
    },
    HostImportRow {
        canon: "std.events",
        method: "off",
        import: "event_off",
        ret: RPrim,
        doc: "encoded bool; unregister releases host-retained handler",
    },
    HostImportRow {
        canon: "std.events",
        method: "emit",
        import: "event_emit",
        ret: RPrim,
        doc: "void host dispatch; data is borrowed for the dispatch",
    },
    HostImportRow {
        canon: "std.events",
        method: "subscribers",
        import: "event_subscribers",
        ret: RPrim,
        doc: "encoded subscriber count",
    },
    HostImportRow {
        canon: "std.events",
        method: "clear",
        import: "event_clear",
        ret: RPrim,
        doc: "void; releases host-retained handlers for one name",
    },
    HostImportRow {
        canon: "std.events",
        method: "clearAll",
        import: "event_clear_all",
        ret: RPrim,
        doc: "void; releases all host-retained handlers and deferred payloads",
    },
    HostImportRow {
        canon: "std.events",
        method: "emitDeferred",
        import: "event_emit_deferred",
        ret: RPrim,
        doc: "void; deferred queue retains payload until drain or clear",
    },
    HostImportRow {
        canon: "std.events",
        method: "drain",
        import: "event_drain",
        ret: RPrim,
        doc: "void; releases deferred queue payloads after dispatch",
    },
    HostImportRow {
        canon: "std.events",
        method: "queueLen",
        import: "event_queue_len",
        ret: RPrim,
        doc: "encoded deferred queue length",
    },
    // std.html
    HostImportRow {
        canon: "std.html",
        method: "escape",
        import: "html_escape",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    // std.json
    HostImportRow {
        canon: "std.json",
        method: "parse",
        import: "json_parse",
        ret: ROwn,
        doc: "fresh graph via heap::build_value; null on parse error",
    },
    HostImportRow {
        canon: "std.json",
        method: "stringify",
        import: "json_stringify",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    // VERIFIED ALIAS — exhibit A for the verification rule: the host
    // returns the dict entry's own string pointer (host/json.rs, no
    // reserve, no retain). An Owned entry here is a use-after-free.
    HostImportRow {
        canon: "std.json",
        method: "requireString",
        import: "json_require_string",
        ret: RBor,
        doc: "returns the dict entry's own string pointer — alias, never Owned",
    },
    // std.crypto
    HostImportRow {
        canon: "std.crypto",
        method: "available",
        import: "crypto_available",
        ret: RPrim,
        doc: "encoded bool availability probe",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "hmacSha256Hex",
        import: "crypto_hmac_sha256_hex",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "hmacSha1Base64",
        import: "crypto_hmac_sha1_base64",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "sha256Hex",
        import: "crypto_sha256_hex",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "hexEncode",
        import: "crypto_hex_encode",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "base64Encode",
        import: "crypto_base64_encode",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "base64Decode",
        import: "crypto_base64_decode",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.crypto",
        method: "constantTimeEquals",
        import: "crypto_constant_time_equals",
        ret: RPrim,
        doc: "encoded bool; host copies both string inputs",
    },
    // std.net
    HostImportRow {
        canon: "std.net",
        method: "available",
        import: "net_available",
        ret: RPrim,
        doc: "encoded bool availability probe",
    },
    HostImportRow {
        canon: "std.ffi",
        method: "available",
        import: "ffi_available",
        ret: RPrim,
        doc: "encoded bool availability probe",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "listen",
        import: "tcp_listen",
        ret: RPrim,
        doc: "encoded integer socket handle",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "accept",
        import: "tcp_accept",
        ret: ROwn,
        doc: "fresh conn dict via heap::build_value; null on error",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "connect",
        import: "tcp_connect",
        ret: RPrim,
        doc: "encoded integer socket handle; host copies host string",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "read",
        import: "tcp_read",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str; null on error",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "readLine",
        import: "tcp_read_line",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str; null on error",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "address",
        import: "tcp_address",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "write",
        import: "tcp_write",
        ret: RPrim,
        doc: "encoded byte count; host copies data string",
    },
    HostImportRow {
        canon: "std.net.tcp",
        method: "close",
        import: "tcp_close",
        ret: RPrim,
        doc: "void close by integer handle",
    },
    HostImportRow {
        canon: "std.net.udp",
        method: "bind",
        import: "udp_bind",
        ret: RPrim,
        doc: "encoded integer socket handle",
    },
    HostImportRow {
        canon: "std.net.udp",
        method: "receive",
        import: "udp_receive",
        ret: ROwn,
        doc: "fresh value via heap::build_value/wasm_alloc_str; null on error",
    },
    HostImportRow {
        canon: "std.net.udp",
        method: "send",
        import: "udp_send",
        ret: RPrim,
        doc: "encoded byte count; host copies address and body strings",
    },
    HostImportRow {
        canon: "std.net.udp",
        method: "broadcast",
        import: "udp_broadcast",
        ret: RPrim,
        doc: "void socket option update by integer handle",
    },
    // std.storage
    HostImportRow {
        canon: "std.storage",
        method: "storageGet",
        import: "storage_get_str",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str; null on miss",
    },
    HostImportRow {
        canon: "std.storage",
        method: "storageSet",
        import: "storage_set",
        ret: RPrim,
        doc: "void; host copies key/value strings into storage",
    },
    HostImportRow {
        canon: "std.storage",
        method: "storageRemove",
        import: "storage_remove",
        ret: RPrim,
        doc: "void; host copies key string",
    },
    HostImportRow {
        canon: "std.storage",
        method: "storageClear",
        import: "storage_clear",
        ret: RPrim,
        doc: "void storage clear",
    },
    // std.array — results verified one by one; find is an ALIAS.
    HostImportRow {
        canon: "std.array",
        method: "map",
        import: "array_map",
        ret: ROwn,
        doc: "fresh array via alloc_array_of",
    },
    HostImportRow {
        canon: "std.array",
        method: "filter",
        import: "array_filter",
        ret: ROwn,
        doc: "fresh array via alloc_array_of",
    },
    HostImportRow {
        canon: "std.array",
        method: "find",
        import: "array_find",
        ret: RBor,
        doc: "returns the matching ELEMENT — an alias of the array's slot, never Owned",
    },
    HostImportRow {
        canon: "std.array",
        method: "isAny",
        import: "array_is_any",
        ret: RPrim,
        doc: "encoded bool (boxed wire shape, no heap object)",
    },
    HostImportRow {
        canon: "std.array",
        method: "isAll",
        import: "array_is_all",
        ret: RPrim,
        doc: "encoded bool (boxed wire shape, no heap object)",
    },
    // std.http.request — every verb returns a fresh response dict.
    HostImportRow {
        canon: "std.http.request",
        method: "get",
        import: "http_request_get",
        ret: ROwn,
        doc: "fresh response dict via build_http_response_dict; null on error",
    },
    HostImportRow {
        canon: "std.http.request",
        method: "post",
        import: "http_request_post",
        ret: ROwn,
        doc: "fresh response dict via build_http_response_dict; null on error",
    },
    HostImportRow {
        canon: "std.http.request",
        method: "put",
        import: "http_request_put",
        ret: ROwn,
        doc: "fresh response dict via build_http_response_dict; null on error",
    },
    HostImportRow {
        canon: "std.http.request",
        method: "patch",
        import: "http_request_patch",
        ret: ROwn,
        doc: "fresh response dict via build_http_response_dict; null on error",
    },
    HostImportRow {
        canon: "std.http.request",
        method: "delete",
        import: "http_request_delete",
        ret: ROwn,
        doc: "fresh response dict via build_http_response_dict; null on error",
    },
    // std.http.server response builders — single backing import.
    HostImportRow {
        canon: "std.http.server",
        method: "ok",
        import: "http_server_response",
        ret: ROwn,
        doc: "fresh response dict (host reserve)",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "text",
        import: "http_server_response",
        ret: ROwn,
        doc: "fresh response dict (host reserve)",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "html",
        import: "http_server_response",
        ret: ROwn,
        doc: "fresh response dict (host reserve)",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "json",
        import: "http_server_response",
        ret: ROwn,
        doc: "fresh response dict (host reserve)",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "redirect",
        import: "http_server_response",
        ret: ROwn,
        doc: "fresh response dict (host reserve)",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "router",
        import: "http_server_router",
        ret: RPrim,
        doc: "encoded router id; native router store owns no guest handles",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "get",
        import: "http_server_router_get",
        ret: RPrim,
        doc: "void; stores and host-retains route handler closure",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "post",
        import: "http_server_router_post",
        ret: RPrim,
        doc: "void; stores and host-retains route handler closure",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "serveFiles",
        import: "http_server_router_serve_files",
        ret: RPrim,
        doc: "void; copies static directory string and stores no guest handle",
    },
    HostImportRow {
        canon: "std.http.server",
        method: "listen",
        import: "http_server_router_listen",
        ret: RPrim,
        doc: "void accept loop; borrows router id and dispatches retained route handlers",
    },
    // std.cli
    HostImportRow {
        canon: "std.cli",
        method: "readLine",
        import: "cli_read_line",
        ret: ROwn,
        doc: "fresh string via wasm_alloc_str",
    },
    HostImportRow {
        canon: "std.cli",
        method: "write",
        import: "cli_write",
        ret: RPrim,
        doc: "void; host copies output string",
    },
    HostImportRow {
        canon: "std.cli",
        method: "writeLine",
        import: "cli_write_line",
        ret: RPrim,
        doc: "void; host copies output string",
    },
    HostImportRow {
        canon: "std.cli",
        method: "clear",
        import: "cli_clear",
        ret: RPrim,
        doc: "void terminal control",
    },
    HostImportRow {
        canon: "std.cli",
        method: "moveTo",
        import: "cli_move_to",
        ret: RPrim,
        doc: "void terminal cursor move by primitive coordinates",
    },
    // Imports with no module-call form (bare globals / machinery).
    HostImportRow {
        canon: "",
        method: "",
        import: "get_location_path",
        ret: ROwn,
        doc: "browser JS writeStrToWasm writes the rc=1 prefix (fresh); native host stubs to null",
    },
    HostImportRow {
        canon: "std.browser",
        method: "replaceLocation",
        import: "replace_location",
        ret: RPrim,
        doc: "void navigation side effect; host copies path string",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "run_all",
        ret: ROwn,
        doc: "reserve'd tuple, rc=1, co-owns each result (plan 113)",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "call_ffi",
        ret: ROwn,
        doc: "encode_return_for_guest: primitives or fresh host-allocated strings",
    },
    // Test spy/mock imports. They are test-mode only, but still store guest
    // handles in native host state when mocks are active.
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_set_mock",
        ret: RPrim,
        doc: "void; host retains stored persistent mock value",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_set_mock_once",
        ret: RPrim,
        doc: "void; host retains stored once-mock value until first use/reset",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_reset",
        ret: RPrim,
        doc: "void; releases retained mock values and clears borrowed call history",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_check_call",
        ret: RPrim,
        doc: "primitive flag; borrowed call args are recorded for same-test assertions",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_assert_called_with",
        ret: RPrim,
        doc: "primitive assertion flag; expected args are borrowed for comparison",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_assert_call_count",
        ret: RPrim,
        doc: "primitive assertion flag",
    },
    HostImportRow {
        canon: "",
        method: "",
        import: "spy_assert_not_called",
        ret: RPrim,
        doc: "primitive assertion flag",
    },
    // TODO(plan-117 phase 4/5): async RPC plumbing — ownership across task
    // segments unverified; conservatively Borrowed until the async engine's
    // handling is read end to end.
    HostImportRow { canon: "", method: "", import: "remote_call", ret: RBor, doc: "TODO unverified async RPC plumbing — conservatively borrowed" },
    HostImportRow { canon: "", method: "", import: "remote_result", ret: RBor, doc: "TODO unverified async RPC plumbing — conservatively borrowed" },
    HostImportRow { canon: "", method: "", import: "ffi_result", ret: RBor, doc: "async FFI offload result; primitives or fresh host allocations (ownership-neutral) — conservatively borrowed" },
];

struct HostImportArgRow {
    import: &'static str,
    args: &'static [ArgConvention],
}

use ArgConvention::{Borrowed as ABor, CopiedByHost as ACopy, RetainedByCallee as ARet};

const ARRAY_HOF_ARGS: &[ArgConvention] = &[ABor, ABor];
const JSON_STRINGIFY_ARGS: &[ArgConvention] = &[ABor];
const EVENT_SUBSCRIBE_ARGS: &[ArgConvention] = &[ACopy, ARet];
const EVENT_EMIT_ARGS: &[ArgConvention] = &[ACopy, ABor];
const EVENT_DEFERRED_ARGS: &[ArgConvention] = &[ACopy, ARet];
const EVENT_OFF_ARGS: &[ArgConvention] = &[ABor];
const EVENT_CLEAR_ARGS: &[ArgConvention] = &[ACopy];
const SPY_SET_MOCK_ARGS: &[ArgConvention] = &[ACopy, ARet];
const SPY_RESET_ARGS: &[ArgConvention] = &[ACopy];
const SPY_CHECK_CALL_ARGS: &[ArgConvention] = &[ACopy, ACopy, ACopy, ACopy];
const SPY_ASSERT_CALLED_WITH_ARGS: &[ArgConvention] = &[ACopy, ACopy, ACopy];
const SPY_ASSERT_CALL_COUNT_ARGS: &[ArgConvention] = &[ACopy, ACopy];
const HTTP_ROUTER_HANDLER_ARGS: &[ArgConvention] = &[ACopy, ACopy, ARet];
const HTTP_ROUTER_SERVE_FILES_ARGS: &[ArgConvention] = &[ACopy, ACopy];
const HTTP_ROUTER_LISTEN_ARGS: &[ArgConvention] = &[ACopy, ACopy];

const HOST_IMPORT_ARGS: &[HostImportArgRow] = &[
    HostImportArgRow {
        import: "array_map",
        args: ARRAY_HOF_ARGS,
    },
    HostImportArgRow {
        import: "array_filter",
        args: ARRAY_HOF_ARGS,
    },
    HostImportArgRow {
        import: "array_find",
        args: ARRAY_HOF_ARGS,
    },
    HostImportArgRow {
        import: "array_is_any",
        args: ARRAY_HOF_ARGS,
    },
    HostImportArgRow {
        import: "array_is_all",
        args: ARRAY_HOF_ARGS,
    },
    HostImportArgRow {
        import: "json_stringify",
        args: JSON_STRINGIFY_ARGS,
    },
    HostImportArgRow {
        import: "event_on",
        args: EVENT_SUBSCRIBE_ARGS,
    },
    HostImportArgRow {
        import: "event_once",
        args: EVENT_SUBSCRIBE_ARGS,
    },
    HostImportArgRow {
        import: "event_off",
        args: EVENT_OFF_ARGS,
    },
    HostImportArgRow {
        import: "event_emit",
        args: EVENT_EMIT_ARGS,
    },
    HostImportArgRow {
        import: "event_clear",
        args: EVENT_CLEAR_ARGS,
    },
    HostImportArgRow {
        import: "event_emit_deferred",
        args: EVENT_DEFERRED_ARGS,
    },
    HostImportArgRow {
        import: "spy_set_mock",
        args: SPY_SET_MOCK_ARGS,
    },
    HostImportArgRow {
        import: "spy_set_mock_once",
        args: SPY_SET_MOCK_ARGS,
    },
    HostImportArgRow {
        import: "spy_reset",
        args: SPY_RESET_ARGS,
    },
    HostImportArgRow {
        import: "spy_check_call",
        args: SPY_CHECK_CALL_ARGS,
    },
    HostImportArgRow {
        import: "spy_assert_called_with",
        args: SPY_ASSERT_CALLED_WITH_ARGS,
    },
    HostImportArgRow {
        import: "spy_assert_call_count",
        args: SPY_ASSERT_CALL_COUNT_ARGS,
    },
    HostImportArgRow {
        import: "spy_assert_not_called",
        args: SPY_RESET_ARGS,
    },
    HostImportArgRow {
        import: "http_server_router_get",
        args: HTTP_ROUTER_HANDLER_ARGS,
    },
    HostImportArgRow {
        import: "http_server_router_post",
        args: HTTP_ROUTER_HANDLER_ARGS,
    },
    HostImportArgRow {
        import: "http_server_router_serve_files",
        args: HTTP_ROUTER_SERVE_FILES_ARGS,
    },
    HostImportArgRow {
        import: "http_server_router_listen",
        args: HTTP_ROUTER_LISTEN_ARGS,
    },
];

pub fn lookup_host_import_args(import: &str) -> Option<&'static [ArgConvention]> {
    HOST_IMPORT_ARGS
        .iter()
        .find(|r| r.import == import)
        .map(|r| r.args)
}

/// Look up the ownership signature of a std-module call (`canon.method`),
/// e.g. `std.json` / `parse`. Backed by [`HOST_IMPORTS`] — the verified
/// boxed-import surface (plan 119 U1).
pub fn lookup_std_module_call(canon: &str, method: &str) -> Option<Signature> {
    if let Some(row) = INLINE_STD_CALLS
        .iter()
        .find(|r| r.canon == canon && r.method == method)
    {
        return Some(Signature::ret_only(row.ret, row.doc));
    }
    HOST_IMPORTS
        .iter()
        .find(|r| !r.canon.is_empty() && r.canon == canon && r.method == method)
        .map(|r| Signature {
            args: lookup_host_import_args(r.import).map(|args| args.to_vec()),
            ret: r.ret,
            doc: r.doc,
        })
}

/// Look up ownership by wasm `env` import name — the form the emission
/// coverage check and the import round-trip test use.
pub fn lookup_host_import(import: &str) -> Option<Signature> {
    #[cfg(debug_assertions)]
    if seeded_absent(import) {
        return None;
    }
    HOST_IMPORTS
        .iter()
        .find(|r| r.import == import)
        .map(|r| Signature {
            args: lookup_host_import_args(r.import).map(|args| args.to_vec()),
            ret: r.ret,
            doc: r.doc,
        })
}

/// Borrowed-returning bare-globals: element reads, dict-field accessors, and
/// error-field reads that alias their input rather than allocating. Kept in
/// sync with codegen's `is_borrowed_bare_global`.
///
/// NOTE: `set` and `unwrap` are intentionally still listed here to match the
/// codegen predicate one-to-one, but `lookup_bare_call` resolves them BEFORE
/// consulting this function (dispatch order), so they never resolve to
/// `Borrowed`. Do not treat membership here as the final classification.
pub fn is_borrowed_bare_global(name: &str) -> bool {
    BORROWED_BARE_CALLS.contains(&name)
}

/// Builtins that ALWAYS return a freshly allocated, rc-prefixed object
/// distinct from their arguments. This is the single physical home of the
/// list — codegen's `is_fresh_builtin_call` delegates here. Collection
/// builders are guest `RT_ALLOC` or rc-prefixed host `reserve`; string
/// transforms that can alias their input on a fast path are included only
/// because codegen normalizes each to a uniform +1: `replace` retains before
/// returning on the empty-find path, and `toString` aliases a String arg via
/// `RT_VALUE_TO_STR` but its codegen (`emit_to_string_owned`) retains on that
/// alias path. Per-name verification is required before adding entries.
pub fn is_fresh_builtin_call(name: &str) -> bool {
    FRESH_BARE_CALLS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_op_ids_are_stable() {
        let ids: Vec<u32> = OwnershipOp::ALL.iter().map(|op| op.id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

        for op in OwnershipOp::ALL {
            assert_eq!(OwnershipOp::from_id(op.id()), Some(op));
            assert!(!op.name().is_empty());
        }
        assert_eq!(OwnershipOp::from_id(0), None);
        assert_eq!(OwnershipOp::from_id(11), None);
    }

    #[test]
    fn ownership_aux_ids_are_stable() {
        assert_eq!(OwnershipAux::None.id(), 0);
        assert_eq!(OwnershipAux::ClosureCapture.id(), 1);
        assert_eq!(OwnershipAux::HostArgument.id(), 2);
        assert_eq!(OwnershipAux::AsyncFrameSlot.id(), 3);

        for aux in [
            OwnershipAux::None,
            OwnershipAux::ClosureCapture,
            OwnershipAux::HostArgument,
            OwnershipAux::AsyncFrameSlot,
        ] {
            assert_eq!(OwnershipAux::from_id(aux.id()), Some(aux));
            assert_eq!(OwnershipAux::decode(aux.encode(37)), Some((aux, 37)));
        }
        assert_eq!(OwnershipAux::from_id(-1), None);
        assert_eq!(OwnershipAux::from_id(4), None);
        assert_eq!(OwnershipAux::decode(4 << 16), None);
    }

    #[test]
    fn arg_convention_ids_are_stable() {
        let ids: Vec<u32> = ArgConvention::ALL.iter().map(|c| c.id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);

        for convention in ArgConvention::ALL {
            assert_eq!(ArgConvention::from_id(convention.id()), Some(convention));
            assert!(!convention.name().is_empty());
        }
        assert_eq!(ArgConvention::from_id(0), None);
        assert_eq!(ArgConvention::from_id(5), None);
    }

    #[test]
    fn set_is_passthrough_arg0() {
        let sig = lookup_bare_call("set").expect("set must be classified");
        assert_eq!(sig.ret, ReturnConvention::PassThrough(0));
    }

    #[test]
    fn unwrap_resolves_owned_despite_being_a_borrowed_bare_global() {
        // The dispatch-order hazard: `unwrap` is in the borrowed list but the
        // explicit check wins. The table must return Owned, not Borrowed.
        assert!(is_borrowed_bare_global("unwrap"));
        let sig = lookup_bare_call("unwrap").expect("unwrap must be classified");
        assert_eq!(sig.ret, ReturnConvention::Owned);
    }

    #[test]
    fn set_resolves_passthrough_despite_being_a_borrowed_bare_global() {
        assert!(is_borrowed_bare_global("set"));
        let sig = lookup_bare_call("set").unwrap();
        assert_eq!(sig.ret, ReturnConvention::PassThrough(0));
    }

    #[test]
    fn borrowed_bare_globals_return_borrowed() {
        for name in BORROWED_BARE_CALLS {
            let sig = lookup_bare_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            let expected = match *name {
                "set" => ReturnConvention::PassThrough(0),
                "unwrap" => ReturnConvention::Owned,
                _ => ReturnConvention::Borrowed,
            };
            assert_eq!(sig.ret, expected, "{name}");
        }
    }

    #[test]
    fn fresh_builtins_return_owned() {
        for name in FRESH_BARE_CALLS {
            assert!(is_fresh_builtin_call(name), "{name} fell out of the list");
            let sig = lookup_bare_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Owned, "{name}");
        }
    }

    #[test]
    fn primitive_bare_calls_return_primitives() {
        for name in PRIMITIVE_BARE_CALLS {
            let sig = lookup_bare_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Primitive, "{name}");
        }
    }

    #[test]
    fn explicit_bare_call_surface_resolves() {
        for row in BARE_CALL_SURFACE {
            let sig = lookup_bare_call(row.name).unwrap_or_else(|| panic!("{} missing", row.name));
            assert_eq!(sig.ret, row.ret, "{}", row.name);
            assert!(!row.doc.is_empty(), "{} needs a doc", row.name);
        }
    }

    #[test]
    fn member_dispatch_checks_borrowed_list_first() {
        // The bare/member asymmetry: member-position set/unwrap classify
        // Borrowed (heuristic member arms hit is_borrowed_bare_global first),
        // unlike their bare forms (PassThrough(0) / Owned).
        for name in ["set", "unwrap", "get", "getString", "message", "kind"] {
            let sig = lookup_member_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Borrowed, "{name}");
        }
        for name in FRESH_BARE_CALLS {
            let sig = lookup_member_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Owned, "{name}");
        }
        assert!(lookup_member_call("someUserMethod").is_none());
    }

    #[test]
    fn unknown_names_are_unclassified() {
        // User functions / closures / type constructors are resolved by
        // codegen per-program, not by this static table.
        assert!(lookup_bare_call("myUserFunction").is_none());
        assert!(lookup_bare_call("Point").is_none());
    }

    #[test]
    fn host_import_surface_is_fully_classified() {
        // Plan 119 U1 started this as the verified boxed-import surface; plan
        // 117 phase 6 also records void/primitive imports whose arguments carry
        // ownership conventions; plan 101 adds ffi_result. The count pin fails
        // when a host import lands without a row — extend the table after
        // reading the host code.
        assert_eq!(HOST_IMPORTS.len(), 98, "host import surface changed");
        for row in HOST_IMPORTS {
            assert!(!row.import.is_empty());
            assert!(
                !row.doc.is_empty(),
                "{} needs a verification doc",
                row.import
            );
            // Module-call rows resolve through the std lookup; all rows
            // resolve through the import-name lookup.
            if !row.canon.is_empty() {
                assert!(
                    lookup_std_module_call(row.canon, row.method).is_some(),
                    "{}.{} unresolvable",
                    row.canon,
                    row.method
                );
            }
            assert!(lookup_host_import(row.import).is_some(), "{}", row.import);
        }
    }

    #[test]
    fn inline_std_call_surface_is_classified() {
        for row in INLINE_STD_CALLS {
            let sig = lookup_std_module_call(row.canon, row.method)
                .unwrap_or_else(|| panic!("{}.{} missing", row.canon, row.method));
            assert_eq!(sig.ret, row.ret, "{}.{}", row.canon, row.method);
            assert!(
                !row.doc.is_empty(),
                "{}.{} needs a doc",
                row.canon,
                row.method
            );
        }
    }

    #[test]
    fn phase4_host_arg_conventions_are_table_driven() {
        for import in [
            "array_map",
            "array_filter",
            "array_find",
            "array_is_any",
            "array_is_all",
        ] {
            let sig = lookup_host_import(import).unwrap_or_else(|| panic!("{import} missing"));
            assert_eq!(
                sig.args,
                Some(vec![ArgConvention::Borrowed, ArgConvention::Borrowed]),
                "{import}"
            );
        }

        let sig = lookup_std_module_call("std.json", "stringify").expect("json.stringify");
        assert_eq!(sig.args, Some(vec![ArgConvention::Borrowed]));
    }

    #[test]
    fn phase6_event_arg_conventions_are_table_driven() {
        let sig = lookup_host_import("event_on").expect("event_on");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee
            ])
        );

        let sig = lookup_host_import("event_once").expect("event_once");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee
            ])
        );

        let sig = lookup_host_import("event_emit_deferred").expect("event_emit_deferred");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee
            ])
        );

        let sig = lookup_host_import("event_emit").expect("event_emit");
        assert_eq!(
            sig.args,
            Some(vec![ArgConvention::CopiedByHost, ArgConvention::Borrowed])
        );
    }

    #[test]
    fn phase6_spy_arg_conventions_are_table_driven() {
        let sig = lookup_host_import("spy_set_mock").expect("spy_set_mock");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee
            ])
        );

        let sig = lookup_host_import("spy_set_mock_once").expect("spy_set_mock_once");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::RetainedByCallee
            ])
        );

        let sig = lookup_host_import("spy_reset").expect("spy_reset");
        assert_eq!(sig.args, Some(vec![ArgConvention::CopiedByHost]));
    }

    #[test]
    fn phase6_router_arg_conventions_are_table_driven() {
        for import in ["http_server_router_get", "http_server_router_post"] {
            let sig = lookup_host_import(import).unwrap_or_else(|| panic!("{import} missing"));
            assert_eq!(
                sig.args,
                Some(vec![
                    ArgConvention::CopiedByHost,
                    ArgConvention::CopiedByHost,
                    ArgConvention::RetainedByCallee
                ]),
                "{import}"
            );
        }

        let sig = lookup_host_import("http_server_router_serve_files").expect("serveFiles");
        assert_eq!(
            sig.args,
            Some(vec![
                ArgConvention::CopiedByHost,
                ArgConvention::CopiedByHost
            ])
        );
    }

    #[test]
    fn verified_aliases_stay_borrowed() {
        // Raw host aliases stay borrowed: std-call wrappers may promote an
        // alias by retaining before releasing owned args, but the import table
        // itself must continue to describe the host ABI truth.
        assert_eq!(
            lookup_host_import("json_require_string").map(|s| s.ret),
            Some(ReturnConvention::Borrowed)
        );
        assert_eq!(
            lookup_std_module_call("std.array", "find").map(|s| s.ret),
            Some(ReturnConvention::Borrowed)
        );
        // Unverified async plumbing stays conservatively borrowed.
        assert_eq!(
            lookup_host_import("remote_call").map(|s| s.ret),
            Some(ReturnConvention::Borrowed)
        );
        // Encoded bools are primitives despite the boxed wire shape.
        assert_eq!(
            lookup_std_module_call("std.array", "isAny").map(|s| s.ret),
            Some(ReturnConvention::Primitive)
        );
        assert_eq!(
            lookup_std_module_call("std.json", "requireString").map(|s| s.ret),
            Some(ReturnConvention::Owned)
        );
    }

    #[test]
    fn std_module_calls_match_inventory() {
        for row in HOST_IMPORTS.iter().filter(|row| !row.canon.is_empty()) {
            if INLINE_STD_CALLS
                .iter()
                .any(|inline| inline.canon == row.canon && inline.method == row.method)
            {
                continue;
            }
            assert_eq!(
                lookup_std_module_call(row.canon, row.method).map(|s| s.ret),
                Some(row.ret),
                "{}.{}",
                row.canon,
                row.method
            );
        }
        assert!(lookup_std_module_call("std.json", "someOtherMethod").is_none());
        assert_eq!(
            lookup_std_module_call("std.array", "first").map(|s| s.ret),
            Some(ReturnConvention::Borrowed)
        );
    }
}
