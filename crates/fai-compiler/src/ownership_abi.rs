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
/// Debug-only seeded-misclassification hook (plan 118 U6). With
/// `FAI_ABI_SEED=<name>` set, the named bare-call entry's return
/// convention is forced to `Borrowed` so CI can prove the FAI_ABI_CHECK
/// divergence detector actually fires — if a deliberately wrong entry
/// can hide, a real one can too. Compiled only under
/// `debug_assertions`: once plan-117 phase 3 makes this table
/// load-bearing, a seed in a release binary would change emitted code.
/// Seeding an already-`Borrowed` name (or `set`, whose `PassThrough`
/// is structurally exempt from parity) produces no divergence — seed
/// an `Owned`-returning name like `unwrap` or `map`.
#[cfg(debug_assertions)]
fn seeded(name: &str) -> bool {
    use std::sync::OnceLock;
    static SEED: OnceLock<Option<String>> = OnceLock::new();
    let seed = SEED.get_or_init(|| {
        let v = std::env::var("FAI_ABI_SEED").ok()?;
        let known = v == "set"
            || v == "unwrap"
            || is_borrowed_bare_global(&v)
            || is_fresh_builtin_call(&v);
        if !known {
            eprintln!(
                "[abi-check] FAI_ABI_SEED='{}' names no bare-call table entry — seed inactive",
                v
            );
        }
        Some(v)
    });
    seed.as_deref() == Some(name)
}

pub fn lookup_bare_call(name: &str) -> Option<Signature> {
    #[cfg(debug_assertions)]
    if seeded(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Borrowed,
            "SEEDED-BUG: return convention flipped by FAI_ABI_SEED (plan 118 U6)",
        ));
    }
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
    // 3. Borrowed bare-globals: element / field / argument reads, no fresh
    //    allocation. (`set`/`unwrap` already handled above.)
    if is_borrowed_bare_global(name) {
        return Some(Signature::ret_only(
            ReturnConvention::Borrowed,
            "borrowed bare-global: returns an element/field/arg view, not a fresh allocation",
        ));
    }
    // 4. Fresh-allocating builtins: always a sole-owned +1.
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
    HostImportRow { canon: "std.file", method: "read", import: "file_read_str", ret: ROwn, doc: "fresh string via wasm_alloc_str; null on error" },
    HostImportRow { canon: "std.file", method: "list", import: "file_list", ret: ROwn, doc: "fresh Array<String> graph via heap::build_value" },
    // std.process — every method returns a fresh status/output string.
    HostImportRow { canon: "std.process", method: "run", import: "process_run", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.process", method: "start", import: "process_start", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.process", method: "write", import: "process_write", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.process", method: "read", import: "process_read", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.process", method: "stop", import: "process_stop", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // std.path
    HostImportRow { canon: "std.path", method: "join", import: "path_join", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.path", method: "basename", import: "path_basename", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.path", method: "dirname", import: "path_dirname", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.path", method: "extname", import: "path_extname", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // std.env
    HostImportRow { canon: "std.env", method: "get", import: "env_get", ret: ROwn, doc: "fresh string via wasm_alloc_str; null when unset" },
    // std.events — the subscription dict is host-built fresh (heap::reserve
    // in build_subscription). Handler retention is the separate, pinned
    // phase-6 issue and does not change the RESULT's ownership.
    HostImportRow { canon: "std.events", method: "on", import: "event_on", ret: ROwn, doc: "fresh subscription dict via heap::reserve" },
    HostImportRow { canon: "std.events", method: "once", import: "event_once", ret: ROwn, doc: "fresh subscription dict via heap::reserve" },
    // std.html
    HostImportRow { canon: "std.html", method: "escape", import: "html_escape", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // std.json
    HostImportRow { canon: "std.json", method: "parse", import: "json_parse", ret: ROwn, doc: "fresh graph via heap::build_value; null on parse error" },
    HostImportRow { canon: "std.json", method: "stringify", import: "json_stringify", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // VERIFIED ALIAS — exhibit A for the verification rule: the host
    // returns the dict entry's own string pointer (host/json.rs, no
    // reserve, no retain). An Owned entry here is a use-after-free.
    HostImportRow { canon: "std.json", method: "requireString", import: "json_require_string", ret: RBor, doc: "returns the dict entry's own string pointer — alias, never Owned" },
    // std.crypto
    HostImportRow { canon: "std.crypto", method: "hmacSha256Hex", import: "crypto_hmac_sha256_hex", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.crypto", method: "sha256Hex", import: "crypto_sha256_hex", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.crypto", method: "hexEncode", import: "crypto_hex_encode", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.crypto", method: "base64Encode", import: "crypto_base64_encode", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.crypto", method: "base64Decode", import: "crypto_base64_decode", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // std.net
    HostImportRow { canon: "std.net.tcp", method: "accept", import: "tcp_accept", ret: ROwn, doc: "fresh conn dict via heap::build_value; null on error" },
    HostImportRow { canon: "std.net.tcp", method: "read", import: "tcp_read", ret: ROwn, doc: "fresh string via wasm_alloc_str; null on error" },
    HostImportRow { canon: "std.net.tcp", method: "readLine", import: "tcp_read_line", ret: ROwn, doc: "fresh string via wasm_alloc_str; null on error" },
    HostImportRow { canon: "std.net.tcp", method: "address", import: "tcp_address", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    HostImportRow { canon: "std.net.udp", method: "receive", import: "udp_receive", ret: ROwn, doc: "fresh value via heap::build_value/wasm_alloc_str; null on error" },
    // std.storage
    HostImportRow { canon: "std.storage", method: "storageGet", import: "storage_get_str", ret: ROwn, doc: "fresh string via wasm_alloc_str; null on miss" },
    // std.array — results verified one by one; find is an ALIAS.
    HostImportRow { canon: "std.array", method: "map", import: "array_map", ret: ROwn, doc: "fresh array via alloc_array_of" },
    HostImportRow { canon: "std.array", method: "filter", import: "array_filter", ret: ROwn, doc: "fresh array via alloc_array_of" },
    HostImportRow { canon: "std.array", method: "find", import: "array_find", ret: RBor, doc: "returns the matching ELEMENT — an alias of the array's slot, never Owned" },
    HostImportRow { canon: "std.array", method: "isAny", import: "array_is_any", ret: RPrim, doc: "encoded bool (boxed wire shape, no heap object)" },
    HostImportRow { canon: "std.array", method: "isAll", import: "array_is_all", ret: RPrim, doc: "encoded bool (boxed wire shape, no heap object)" },
    // std.http.request — every verb returns a fresh response dict.
    HostImportRow { canon: "std.http.request", method: "get", import: "http_request_get", ret: ROwn, doc: "fresh response dict via build_http_response_dict; null on error" },
    HostImportRow { canon: "std.http.request", method: "post", import: "http_request_post", ret: ROwn, doc: "fresh response dict via build_http_response_dict; null on error" },
    HostImportRow { canon: "std.http.request", method: "put", import: "http_request_put", ret: ROwn, doc: "fresh response dict via build_http_response_dict; null on error" },
    HostImportRow { canon: "std.http.request", method: "patch", import: "http_request_patch", ret: ROwn, doc: "fresh response dict via build_http_response_dict; null on error" },
    HostImportRow { canon: "std.http.request", method: "delete", import: "http_request_delete", ret: ROwn, doc: "fresh response dict via build_http_response_dict; null on error" },
    // std.http.server response builders — single backing import.
    HostImportRow { canon: "std.http.server", method: "ok", import: "http_server_response", ret: ROwn, doc: "fresh response dict (host reserve)" },
    HostImportRow { canon: "std.http.server", method: "text", import: "http_server_response", ret: ROwn, doc: "fresh response dict (host reserve)" },
    HostImportRow { canon: "std.http.server", method: "html", import: "http_server_response", ret: ROwn, doc: "fresh response dict (host reserve)" },
    HostImportRow { canon: "std.http.server", method: "json", import: "http_server_response", ret: ROwn, doc: "fresh response dict (host reserve)" },
    HostImportRow { canon: "std.http.server", method: "redirect", import: "http_server_response", ret: ROwn, doc: "fresh response dict (host reserve)" },
    // std.cli
    HostImportRow { canon: "std.cli", method: "readLine", import: "cli_read_line", ret: ROwn, doc: "fresh string via wasm_alloc_str" },
    // Imports with no module-call form (bare globals / machinery).
    HostImportRow { canon: "", method: "", import: "get_location_path", ret: ROwn, doc: "browser JS writeStrToWasm writes the rc=1 prefix (fresh); native host stubs to null" },
    HostImportRow { canon: "", method: "", import: "run_all", ret: ROwn, doc: "reserve'd tuple, rc=1, co-owns each result (plan 113)" },
    HostImportRow { canon: "", method: "", import: "call_ffi", ret: ROwn, doc: "encode_return_for_guest: primitives or fresh host-allocated strings" },
    // TODO(plan-117 phase 4/5): async RPC plumbing — ownership across task
    // segments unverified; conservatively Borrowed until the async engine's
    // handling is read end to end.
    HostImportRow { canon: "", method: "", import: "remote_call", ret: RBor, doc: "TODO unverified async RPC plumbing — conservatively borrowed" },
    HostImportRow { canon: "", method: "", import: "remote_result", ret: RBor, doc: "TODO unverified async RPC plumbing — conservatively borrowed" },
];

/// Look up the ownership signature of a std-module call (`canon.method`),
/// e.g. `std.json` / `parse`. Backed by [`HOST_IMPORTS`] — the verified
/// boxed-import surface (plan 119 U1).
pub fn lookup_std_module_call(canon: &str, method: &str) -> Option<Signature> {
    HOST_IMPORTS
        .iter()
        .find(|r| !r.canon.is_empty() && r.canon == canon && r.method == method)
        .map(|r| Signature::ret_only(r.ret, r.doc))
}

/// Look up ownership by wasm `env` import name — the form the emission
/// coverage check and the import round-trip test use.
pub fn lookup_host_import(import: &str) -> Option<Signature> {
    HOST_IMPORTS
        .iter()
        .find(|r| r.import == import)
        .map(|r| Signature::ret_only(r.ret, r.doc))
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
    matches!(
        name,
        "unwrap" | "get" | "getString" | "getInt" | "getBool" | "set" | "message" | "kind"
    )
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
    matches!(
        name,
        "append"
            | "getKeys"
            | "map"
            | "filter"
            | "slice"
            | "reverse"
            | "sort"
            | "split"
            | "copy"
            | "Error"
            | "all"
            | "trim"
            | "trimStart"
            | "trimEnd"
            | "toUpper"
            | "toLower"
            | "substring"
            | "repeat"
            | "join"
            | "replace"
            | "toString"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for name in ["get", "getString", "getInt", "getBool", "message", "kind"] {
            let sig = lookup_bare_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Borrowed, "{name}");
        }
    }

    /// Every fresh builtin, exhaustively — a partial sample would let a
    /// list edit slip through untested.
    const ALL_FRESH_BUILTINS: [&str; 21] = [
        "append", "getKeys", "map", "filter", "slice", "reverse", "sort", "split", "copy",
        "Error", "all", "trim", "trimStart", "trimEnd", "toUpper", "toLower", "substring",
        "repeat", "join", "replace", "toString",
    ];

    #[test]
    fn fresh_builtins_return_owned() {
        for name in ALL_FRESH_BUILTINS {
            assert!(is_fresh_builtin_call(name), "{name} fell out of the list");
            let sig = lookup_bare_call(name).unwrap_or_else(|| panic!("{name} unclassified"));
            assert_eq!(sig.ret, ReturnConvention::Owned, "{name}");
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
        for name in ALL_FRESH_BUILTINS {
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
        // Plan 119 U1: the verified boxed-import surface. The count pin
        // fails when a new boxed import lands without a row — extend the
        // table (after reading the host code), don't bump blindly.
        assert_eq!(HOST_IMPORTS.len(), 50, "boxed-import surface changed");
        for row in HOST_IMPORTS {
            assert!(!row.import.is_empty());
            assert!(!row.doc.is_empty(), "{} needs a verification doc", row.import);
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
    fn verified_aliases_stay_borrowed() {
        // The two proven aliasing returns: a silent upgrade to Owned is a
        // use-after-free and must fail here first.
        assert_eq!(
            lookup_std_module_call("std.json", "requireString").map(|s| s.ret),
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
    }

    #[test]
    fn std_module_calls_match_codegen_whitelist() {
        // All nine verified-owned entries, exhaustively.
        for (canon, method) in [
            ("std.json", "parse"),
            ("std.json", "stringify"),
            ("std.env", "get"),
            ("std.file", "read"),
            ("std.http.server", "ok"),
            ("std.http.server", "text"),
            ("std.http.server", "html"),
            ("std.http.server", "json"),
            ("std.http.server", "redirect"),
        ] {
            assert_eq!(
                lookup_std_module_call(canon, method).map(|s| s.ret),
                Some(ReturnConvention::Owned),
                "{canon}.{method}"
            );
        }
        // Borrowed-view std methods must stay out of the table.
        assert!(lookup_std_module_call("std.json", "someOtherMethod").is_none());
        assert!(lookup_std_module_call("std.array", "first").is_none());
    }
}
