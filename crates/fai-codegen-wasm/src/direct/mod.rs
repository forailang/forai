//! Direct AST → wasm function builder.
//!
//! This builder takes forai AST function bodies and produces
//! `wasm_encoder::Function`s directly, with no bytecode intermediate.
//! It owns the NaN-box + runtime-helper contract used by the emitted
//! wasm module and is the production wasm codegen path.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use fai_compiler::ast::{
    AssignmentStatement, AssignmentTarget, BinaryExpression, CallExpression, Expression,
    ForStatement, FunctionDeclaration, IfStatement, LetStatement, Statement, ThrowStatement,
    TryStatement, TypeNode, UnaryExpression, VarStatement, WhileStatement,
};
use fai_compiler::ownership_abi::{ArgConvention, ExprOwnership, OwnershipAux, OwnershipOp};
use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};

use crate::program::FunctionInfo;
use crate::runtime::{
    IMPORT_ARRAY_FILTER, IMPORT_ARRAY_FIND, IMPORT_ARRAY_IS_ALL, IMPORT_ARRAY_IS_ANY,
    IMPORT_ARRAY_MAP, IMPORT_CALL_FFI, IMPORT_CLI_CLEAR, IMPORT_CLI_MOVE_TO, IMPORT_CLI_READ_LINE,
    IMPORT_CLI_WRITE, IMPORT_CLI_WRITE_LINE, IMPORT_CRYPTO_AVAILABLE, IMPORT_CRYPTO_BASE64_DECODE,
    IMPORT_CRYPTO_BASE64_ENCODE, IMPORT_CRYPTO_CONSTANT_TIME_EQUALS, IMPORT_CRYPTO_HEX_ENCODE,
    IMPORT_CRYPTO_HMAC_SHA1_BASE64, IMPORT_CRYPTO_HMAC_SHA256_HEX,
    IMPORT_CRYPTO_RS256_SIGN_BASE64_URL, IMPORT_CRYPTO_SHA256_HEX, IMPORT_ENV_GET, IMPORT_ENV_LOAD,
    IMPORT_EVENT_CLEAR, IMPORT_EVENT_CLEAR_ALL, IMPORT_EVENT_DRAIN, IMPORT_EVENT_EMIT,
    IMPORT_EVENT_EMIT_DEFERRED, IMPORT_EVENT_OFF, IMPORT_EVENT_ON, IMPORT_EVENT_ONCE,
    IMPORT_EVENT_QUEUE_LEN, IMPORT_EVENT_SUBSCRIBERS, IMPORT_FFI_AVAILABLE, IMPORT_FILE_EXISTS,
    IMPORT_FILE_LIST, IMPORT_FILE_READ_STR, IMPORT_GET_LOCATION_PATH, IMPORT_HTML_ESCAPE,
    IMPORT_HTTP_REQUEST_DELETE, IMPORT_HTTP_REQUEST_GET, IMPORT_HTTP_REQUEST_PATCH,
    IMPORT_HTTP_REQUEST_POST, IMPORT_HTTP_REQUEST_PUT, IMPORT_JSON_FORMAT, IMPORT_JSON_MINIFY,
    IMPORT_JSON_PARSE, IMPORT_JSON_QUERY, IMPORT_JSON_QUERY_PAGE, IMPORT_JSON_REQUIRE_STRING,
    IMPORT_JSON_STRINGIFY, IMPORT_JSON_STRINGIFY_PRETTY, IMPORT_JSON_VALID, IMPORT_LOG_ERROR, IMPORT_LOG_INFO,
    IMPORT_LOG_WARN, IMPORT_NET_AVAILABLE, IMPORT_NOW_MS, IMPORT_OWNERSHIP_EVENT,
    IMPORT_PATH_BASENAME, IMPORT_PATH_DIRNAME, IMPORT_PATH_EXTNAME, IMPORT_PATH_JOIN,
    IMPORT_PROCESS_AVAILABLE, IMPORT_PROCESS_READ, IMPORT_PROCESS_RUN, IMPORT_PROCESS_START,
    IMPORT_PROCESS_STOP, IMPORT_PROCESS_WRITE, IMPORT_PUSH_HISTORY_STATE, IMPORT_RANDOM,
    IMPORT_REMOTE_CALL, IMPORT_REPLACE_LOCATION, IMPORT_SECRETS_AVAILABLE, IMPORT_SECRETS_BASIC,
    IMPORT_SECRETS_BEARER, IMPORT_SECRETS_GET, IMPORT_SECRETS_HAS, IMPORT_SECRETS_HEADER,
    IMPORT_SECRETS_REFRESH, IMPORT_SECRETS_RESOLVE_TEMPLATE, IMPORT_SECRETS_REVEAL,
    IMPORT_SECRETS_REVEAL_OR, IMPORT_SET_HTML, IMPORT_SET_HTML_AT,
    IMPORT_SET_TRAP_MSG, IMPORT_SPAWN, IMPORT_STORAGE_CLEAR, IMPORT_STORAGE_GET_STR,
    IMPORT_STORAGE_REMOVE, IMPORT_STORAGE_SET, IMPORT_TCP_ACCEPT, IMPORT_TCP_ADDRESS,
    IMPORT_TCP_CLOSE, IMPORT_TCP_CONNECT, IMPORT_TCP_LISTEN, IMPORT_TCP_READ, IMPORT_TCP_READ_LINE,
    IMPORT_TCP_WRITE, IMPORT_TRAP_REPORT, IMPORT_UDP_BIND, IMPORT_UDP_BROADCAST,
    IMPORT_UDP_RECEIVE, IMPORT_UDP_SEND, IMPORT_WRITE_FILE, INT_CHECK_MASK, METHOD_APPEND,
    METHOD_APPEND_MOVE, METHOD_CONTAINS, METHOD_ENDS_WITH, METHOD_FIRST, METHOD_GET_KEYS,
    METHOD_INDEX_OF,
    METHOD_IS_EMPTY, METHOD_JOIN, METHOD_LAST, METHOD_LENGTH, METHOD_REPEAT, METHOD_REPLACE,
    METHOD_REVERSE, METHOD_SERVER_GET, METHOD_SERVER_HTML, METHOD_SERVER_JSON,
    METHOD_SERVER_LISTEN, METHOD_SERVER_OK, METHOD_SERVER_POST, METHOD_SERVER_REDIRECT,
    METHOD_SERVER_ROUTER, METHOD_SERVER_SERVE_FILES, METHOD_SERVER_TEXT, METHOD_SLICE, METHOD_SORT,
    METHOD_SPLIT, METHOD_STARTS_WITH, METHOD_SUBSTRING, METHOD_TO_LOWER, METHOD_TO_UPPER,
    METHOD_TRIM, METHOD_TRIM_END, METHOD_TRIM_START, OBJ_TAG_ARRAY, OBJ_TAG_CELL, OBJ_TAG_CLOSURE,
    OBJ_TAG_DICT, OBJ_TAG_NATIVE_FN, OBJ_TAG_STRING, OBJ_TAG_TUPLE, QNAN, RT_ADD, RT_ALLOC,
    RT_ALLOC_STRING, RT_AS_NUMBER, RT_CALL_NATIVE, RT_COUNT, RT_DIV, RT_EQ, RT_GE, RT_GET_FIELD,
    RT_GET_INDEX, RT_GT, RT_IDIV, RT_IS_FLOAT, RT_IS_INT, RT_IS_OBJ, RT_LE, RT_CURRENT_TASK, RT_LIVE_OBJECTS, RT_LT, RT_SET_TASK_CTX, RT_TASK_CTX, RT_TASK_WAITER,
    RT_MAKE_BOOL, RT_MAKE_FLOAT, RT_MAKE_INT, RT_MAKE_OBJ, RT_MOD, RT_MUL, RT_NE, RT_NEG,
    RT_OBJ_ADDR, RT_PARSE_FLOAT, RT_PARSE_INT, RT_POW, RT_PRINT_VAL_NEW, RT_RELEASE, RT_RETAIN,
    RT_SET_FIELD, RT_STR_EQ, RT_SUB, RT_VALUE_TO_STR, TAG_BOOL, TAG_INT, VAL_FALSE, VAL_NULL,
    VAL_VOID,
};

mod assemble;
mod async_lower;
mod builder_expr;
mod builder_stmt;
mod dispatch;
pub use assemble::*;
pub(crate) use async_lower::*;
pub(crate) use dispatch::*;

/// Global index for `__env_ptr`. Matches the bytecode translator's
/// layout: `__heap_ptr` at 0, `__env_ptr` at 1, `error_flag` at 2,
/// `error_value` at 3. A closure call sets this to `closure_addr + 16`
/// so the body can read upvalues at `GlobalGet(ENV_PTR) + N*8`.
const GLOBAL_ENV_PTR: u32 = 1;

/// `error_flag` (i32, 0/1). Set by a `throw` whose enclosing
/// function has no `try` frame for it; cleared by the post-call
/// propagation in a caller that does have a `try`. Initialized to
/// 0 in the global section.
const GLOBAL_ERROR_FLAG: u32 = 2;

/// `error_value` (i64, NaN-boxed). Holds the thrown value when
/// `error_flag` is set. Cleared at the same time as `error_flag`.
/// Initialized to 0 in the global section.
const GLOBAL_ERROR_VALUE: u32 = 3;
const OWNERSHIP_SITE_UNKNOWN: u32 = 0;

fn mem0() -> MemArg {
    MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}

fn mem_off(off: u64) -> MemArg {
    MemArg {
        offset: off,
        align: 0,
        memory_index: 0,
    }
}

/// FieldDeclarations for built-in named types — `Event`,
/// `HttpRequest`, `RpcCall`, etc. — registered in
/// `fai-checker/src/checker/program.rs::resolve_type_fields`. The
/// codegen merges these into its own `type_fields` map alongside
/// user-declared `type Foo ... end` entries so that
/// `let x T = from_dict(d)` expansion (see
/// `compile_from_dict_local_value`) finds the field list at codegen
/// time. The TypeNodes are intentionally minimal — `from_dict`'s
/// runtime path only reads field name / attributes / default_value
/// per-field, not the type. Drift risk between this list and
/// program.rs's checker entries surfaces as a loud
/// `UnknownIdentifier("from_dict")` at compile time.
fn builtin_type_fields() -> Vec<(String, Vec<fai_compiler::ast::FieldDeclaration>)> {
    use fai_compiler::ast::{FieldDeclaration, SourceLocation, TypeNode};

    let loc = SourceLocation { line: 0, column: 0 };
    let unknown_node = || TypeNode {
        kind: "type".to_string(),
        name: Some("Unknown".to_string()),
        is_type_parameter: None,
        function_params: None,
        function_returns: None,
        is_array: false,
        is_optional: false,
        location: loc.clone(),
    };
    let mk = |fields: &[&str]| -> Vec<FieldDeclaration> {
        fields
            .iter()
            .map(|n| FieldDeclaration {
                name: (*n).to_string(),
                type_node: unknown_node(),
                default_value: None,
                attributes: Vec::new(),
                location: loc.clone(),
            })
            .collect()
    };

    vec![
        // Lifecycle / messaging
        ("Event".to_string(), mk(&["name", "data"])),
        ("Subscription".to_string(), mk(&["id", "name"])),
        // HTTP wire shapes
        (
            "HttpRequest".to_string(),
            mk(&["method", "path", "body", "headers"]),
        ),
        (
            "HttpResponse".to_string(),
            mk(&[
                "status",
                "body",
                "contentType",
                "location",
                "cookies",
                "headers",
            ]),
        ),
        (
            "Cookie".to_string(),
            mk(&[
                "name", "value", "path", "maxAge", "httpOnly", "secure", "sameSite",
            ]),
        ),
        // Standard event payloads
        ("RequestResponse".to_string(), mk(&["request", "response"])),
        ("ServerStarted".to_string(), mk(&["port"])),
        ("HttpError".to_string(), mk(&["request", "message"])),
        ("RpcCall".to_string(), mk(&["fnName", "args"])),
        ("RpcResult".to_string(), mk(&["fnName", "value"])),
        ("RpcError".to_string(), mk(&["fnName", "message"])),
    ]
}

/// Collects string literals seen during compilation so the module
/// assembler can emit a single `DataSection` for them and know how
/// high to initialise `__heap_ptr` (heap allocations must not
/// overwrite string data at the start of linear memory).
#[derive(Default)]
pub struct StringInterner {
    /// Raw byte buffer — data-section contents, laid out at memory
    /// offset 0. Consumed by the module assembler.
    pub bytes: Vec<u8>,
    /// Dedup index: `s -> (offset, len)`. Interning the same string
    /// twice reuses the first slot.
    offsets: HashMap<String, (u32, u32)>,
}

impl StringInterner {
    /// Intern a string literal and return its `(byte_offset, len)` in
    /// the data buffer. First call appends, subsequent calls reuse.
    pub fn intern(&mut self, s: &str) -> (u32, u32) {
        if let Some(&r) = self.offsets.get(s) {
            return r;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        let r = (off, s.len() as u32);
        self.offsets.insert(s.to_string(), r);
        r
    }
}

// ── Module dispatch ──────────────────────────────────────────────
//
// Calls like `file.read(path)` and `tcp.listen(port)` go through a
// static table keyed on the canonical module path (`std.file`,
// `std.net.tcp`). Each entry describes the arg shapes the import
// expects on the wasm value stack, the import index to call, and
// how to translate the result back into a NaN-boxed forai value.
//
// Scope: imports that take primitives/strings and return primitives,
// strings, dicts, or void. Special cases such as buffer allocation
// (`std.file.read`), closures-as-args (`std.array.map`), and
// trap-message globals (`assert.*`) are handled by dedicated
// `ModuleCall` variants.

/// How a single argument is emitted onto the wasm value stack before
/// the host import call.
#[derive(Clone, Copy)]
enum ArgShape {
    /// NaN-boxed String → `(ptr, len)`: two i32 values. Non-string
    /// inputs are coerced through `RT_VALUE_TO_STR`, so numeric or
    /// null args fall through to their stringified form.
    String,
    /// NaN-boxed Int → i32. Uses `I32WrapI64` and masks off the tag
    /// bits (the low 32 of a NaN-boxed Int is the raw value).
    Int,
    /// i64 passthrough — the NaN-boxed value goes to the host
    /// untouched. Used for `i64` import params like
    /// `IMPORT_JSON_STRINGIFY(val: i64)`.
    Boxed,
}

thread_local! {
    /// Lazily-read FAI_ABI_CHECK gate (plan 117 phase 1). Read the env var
    /// once per thread instead of on every `call_returns_owned` invocation —
    /// that function runs at every bind/store/reassign/return/discard site,
    /// and `std::env::var` takes the process env lock each call. Mirrors the
    /// cached-flag pattern runtime.rs uses for FAI_RC_CHECK/CHECK_LEAKS.
    static ABI_CHECK: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Import index -> "env" import name, cached once per thread (the
/// signatures vec is rebuilt per call otherwise).
fn import_name(idx: u32) -> &'static str {
    thread_local! {
        static NAMES: std::cell::OnceCell<Vec<&'static str>> =
            const { std::cell::OnceCell::new() };
    }
    NAMES.with(|n| {
        n.get_or_init(|| {
            crate::runtime::import_signatures()
                .into_iter()
                .map(|(name, _, _)| name)
                .collect()
        })[idx as usize]
    })
}

/// Sync host imports that may raise through the `__error_flag` /
/// `__error_value` channel (`signal_host_error` on the fai-cli side).
/// Each gets the same post-call propagation check as a user function
/// call, so a host failure lands in the nearest `try`/`catch` (or
/// propagates to the caller) instead of flowing onward as a silent
/// null / false / empty value. Grown import-by-import as sync imports
/// migrate onto the channel; hosts that never set the flag (e.g. the
/// browser runtime's stubs) are unaffected — the check sees flag=0.
fn import_signals_errors(import_idx: u32) -> bool {
    matches!(
        import_idx,
        IMPORT_FILE_READ_STR
            | IMPORT_WRITE_FILE
            | IMPORT_FILE_LIST
            // json.parse raises a catchable error on malformed input rather
            // than returning null (json_stringify never fails, so it stays off).
            | IMPORT_JSON_PARSE
            | IMPORT_SECRETS_GET
            | IMPORT_SECRETS_REVEAL
            | IMPORT_SECRETS_BEARER
            | IMPORT_SECRETS_BASIC
            | IMPORT_SECRETS_HEADER
            // The sync http_request_* imports only raise for a Secret
            // header that cannot be resolved at egress (plan 132 phase 3);
            // ordinary HTTP failures still return response values.
            | IMPORT_HTTP_REQUEST_GET
            | IMPORT_HTTP_REQUEST_POST
            | IMPORT_HTTP_REQUEST_PUT
            | IMPORT_HTTP_REQUEST_PATCH
            | IMPORT_HTTP_REQUEST_DELETE
    )
}

/// `[abi-check] MISSING-SIGNATURE` sentinel, once per import name per
/// thread (plan 119 U2) — the unchecked-build form of the coverage error.
fn report_missing_signature_once(name: &'static str) {
    thread_local! {
        static SEEN: std::cell::RefCell<std::collections::HashSet<&'static str>> =
            std::cell::RefCell::new(std::collections::HashSet::new());
    }
    SEEN.with(|s| {
        if s.borrow_mut().insert(name) {
            eprintln!(
                "[abi-check] MISSING-SIGNATURE: boxed host import '{}' has no ownership                  table entry (fai-compiler ownership_abi) — classified borrowed, leaks on bind",
                name
            );
        }
    });
}

/// Whether the strict ownership-coverage error is enabled (plan 119; the
/// env var doubles as the missing-signature gate post-swap).
fn abi_check_enabled() -> bool {
    ABI_CHECK.with(|c| match c.get() {
        Some(v) => v,
        None => {
            let v = std::env::var_os("FAI_ABI_CHECK").is_some();
            c.set(Some(v));
            v
        }
    })
}

/// How to wrap the import's return value back into a forai value.
#[derive(Clone, Copy)]
enum ResultShape {
    /// Import returns i64 — already NaN-boxed. Leave on stack.
    Boxed,
    /// Import returns i32 — wrap with `RT_MAKE_INT`.
    MakeInt,
    /// Import returns i32 — wrap with `RT_MAKE_BOOL`.
    MakeBool,
    /// Import returns f64 — wrap with `RT_MAKE_FLOAT`. Used for
    /// `time.now`, where the host returns ms-since-epoch as f64.
    MakeFloat,
    /// Import returns no value (or returns `void`) — push `VAL_VOID`
    /// so the expression still leaves an i64 on the stack.
    Void,
}

/// Which assertion flavour to emit. Used by `ModuleCall::Assertion`.
#[derive(Clone, Copy)]
enum AssertionKind {
    /// `test.assert(val, msg?)` — passes when `val` is truthy.
    /// `msg_arg_idx` = 1.
    Truthy,
    /// `assert.isTrue(val, msg?)` — same truthiness semantics as
    /// `Truthy`; separate variant for clarity at the call site.
    IsTrue,
    /// `assert.isFalse(val, msg?)` — inverse of `IsTrue`.
    IsFalse,
    /// `assert.isNull(val, msg?)` — passes when `val == null`.
    IsNull,
    /// `assert.isNotNull(val, msg?)` — passes when `val != null`.
    IsNotNull,
    /// `test.equal(a, b, msg?)` and `assert.equals(a, b, msg?)` —
    /// stringify both sides and compare via `RT_STR_EQ`.
    /// `msg_arg_idx` = 2.
    StringEq,
}

/// Runtime-helper offset the function's `Call` instructions use.
/// Identical to the one `translate.rs` consumes: wasm function index
/// `rt_base + RT_*` selects the right helper.
#[derive(Clone, Copy)]
pub struct RtOffsets {
    pub base: u32,
}

/// Side-channel info the checker gathered during type-checking that
/// the direct builder needs. Callers extract the maps from a `Checker`
/// instance and hand them here.
///
/// Keys are `(module_name, line, column)`. Module name is `""` for the
/// entry module; for nested user modules it's the `std.foo.bar`-style
/// dotted path the compiler uses as a string-pool prefix.
#[derive(Debug, Default, Clone)]
pub struct CheckerInfo {
    /// Every call site that the checker rewrote from `x.foo(args)` to
    /// `foo(x, args)` because `foo` resolves as a global function (not
    /// a field on `x`). The builder performs the same rewrite at emit
    /// time.
    pub ufcs_calls: std::collections::HashSet<(String, u32, u32)>,
    /// Per-call-site reordering for named-parameter calls, keyed the
    /// same way. `map[(...)] = vec![Some(arg_i), ...]` means position
    /// `param_i` at the callee should be filled with the caller's
    /// argument at `arg_i`. `None` slots receive the param's default
    /// (defaults aren't wired yet in the direct path — this chunk
    /// only handles pure reorders).
    pub named_param_reorder: std::collections::HashMap<(String, u32, u32), Vec<Option<usize>>>,
    /// Static type for each expression that checked successfully. Plan 95 uses
    /// this to decide where a primitive can remain in a raw wasm value shape.
    /// Keyed like `fai_checker::checker::ExpressionKey` —
    /// `(module, line, column, right_line, right_column)` — so nested
    /// left-recursive `BinaryExpression`s sharing a leftmost position
    /// don't collide in the hash map.
    pub expression_types:
        std::collections::HashMap<fai_checker::checker::ExpressionKey, fai_checker::types::Type>,
    /// Concrete type constructor names inferred for generic `@type`
    /// call sites, keyed like UFCS/reorder maps.
    pub generic_type_args: std::collections::HashMap<(String, u32, u32), Vec<String>>,
    /// `arr[i]` sites proven to have an `Array` receiver and `Int`
    /// index (keyed by the IndexExpression's `(module, line, column)`).
    /// Lets the builder inline the element read instead of calling the
    /// polymorphic `rt_get_index`.
    pub array_int_index_sites: std::collections::HashSet<(String, u32, u32)>,
    /// `obj.field` reads proven to target a user record type, mapping
    /// the MemberExpression's `(module, line, column)` to the receiver's
    /// type name. Lets the builder read the field's fixed dict slot
    /// directly instead of the string-keyed `rt_get_field` scan.
    pub record_field_read_sites: std::collections::HashMap<(String, u32, u32, u32), String>,
}

impl CheckerInfo {
    /// An empty info — no UFCS, no reorders. Used by standalone test
    /// fixtures that don't rely on checker-driven rewrites.
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Wasm-level representation of a compiled forai expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueShape {
    /// NaN-boxed i64 value, the universal runtime representation.
    Boxed,
    /// Raw wasm i64 carrying an Int payload without NaN-box tag bits.
    RawInt,
    /// Raw wasm f64 carrying a Float directly.
    RawFloat,
    /// Raw wasm i32 carrying a Bool as 0/1.
    RawBool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotOwnership {
    NonOwning,
    Owning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExprResult {
    shape: ValueShape,
    ownership: ExprOwnership,
}

impl ExprResult {
    fn primitive(shape: ValueShape) -> Self {
        Self {
            shape,
            ownership: ExprOwnership::Primitive,
        }
    }

    fn boxed(owned: bool) -> Self {
        Self {
            shape: ValueShape::Boxed,
            ownership: if owned {
                ExprOwnership::Owned
            } else {
                ExprOwnership::Borrowed
            },
        }
    }
}

/// Build the `param_defaults` vector for a `FunctionInfo`. Type
/// parameters (prepended to wasm arg lists as hidden constructor
/// arguments) never have defaults; only real params do.
fn param_defaults_for(
    fd: &fai_compiler::ast::FunctionDeclaration,
) -> Vec<Option<fai_compiler::ast::Expression>> {
    let mut defaults = Vec::with_capacity(fd.type_params.len() + fd.params.len());
    for _ in &fd.type_params {
        defaults.push(None);
    }
    for p in &fd.params {
        defaults.push(p.default_value.clone());
    }
    defaults
}

/// Source-level parameter names for direct call lowering. Hidden generic
/// `@type` parameters are deliberately excluded; they are injected separately
/// before source arguments.
fn param_names_for(fd: &fai_compiler::ast::FunctionDeclaration) -> Vec<String> {
    fd.params.iter().map(|p| p.name.clone()).collect()
}

/// Pre-pass: identify `var` bindings in this function's body that
/// are referenced inside a nested closure (do...end block). Such vars
/// must share one heap cell between the outer scope and every
/// capturing closure, so writes on either side are visible everywhere
/// — that's the "inner function borrows the outer's scope" model.
///
/// The set returned is the intersection of:
///   1. Names declared by `VarStatement` in the outer body (recursing
///      into non-closure blocks: if/else, while, for, try, case).
///   2. Names referenced by an `IdentifierExpression` somewhere inside
///      a `FunctionExpression` body.
///
/// `let` bindings are excluded — they can't be reassigned, so sharing
/// a cell buys nothing. A false positive (outer var shadowed by a
/// closure-local of the same name) only costs extra heap alloc, never
/// correctness, so the walker is intentionally approximate here.
fn collect_cell_captured_vars(body: &[fai_compiler::ast::Statement]) -> HashSet<String> {
    use fai_compiler::ast::{Expression, Statement};

    fn walk_stmt(
        stmt: &Statement,
        declared: &mut HashSet<String>,
        closure_refs: &mut HashSet<String>,
        in_closure: bool,
    ) {
        match stmt {
            Statement::VarStatement(vs) => {
                if !in_closure {
                    for b in &vs.bindings {
                        declared.insert(b.name.clone());
                    }
                }
                walk_expr(&vs.value, declared, closure_refs, in_closure);
            }
            Statement::LetStatement(ls) => {
                walk_expr(&ls.value, declared, closure_refs, in_closure);
            }
            Statement::AssignmentStatement(a) => {
                match &a.target {
                    fai_compiler::ast::AssignmentTarget::Variables { names } => {
                        if in_closure {
                            for n in names {
                                closure_refs.insert(n.clone());
                            }
                        }
                    }
                    fai_compiler::ast::AssignmentTarget::Field { object }
                    | fai_compiler::ast::AssignmentTarget::Index { object } => {
                        walk_expr(object, declared, closure_refs, in_closure);
                    }
                }
                walk_expr(&a.value, declared, closure_refs, in_closure);
            }
            Statement::IfStatement(s) => {
                for branch in &s.branches {
                    walk_expr(&branch.condition, declared, closure_refs, in_closure);
                    for st in &branch.body {
                        walk_stmt(st, declared, closure_refs, in_closure);
                    }
                }
                if let Some(eb) = &s.else_branch {
                    for st in eb {
                        walk_stmt(st, declared, closure_refs, in_closure);
                    }
                }
            }
            Statement::WhileStatement(s) => {
                walk_expr(&s.condition, declared, closure_refs, in_closure);
                for st in &s.body {
                    walk_stmt(st, declared, closure_refs, in_closure);
                }
            }
            Statement::ForStatement(s) => {
                walk_expr(&s.items, declared, closure_refs, in_closure);
                for st in &s.body {
                    walk_stmt(st, declared, closure_refs, in_closure);
                }
            }
            Statement::TryStatement(s) => {
                for st in &s.try_body {
                    walk_stmt(st, declared, closure_refs, in_closure);
                }
                for st in &s.catch_body {
                    walk_stmt(st, declared, closure_refs, in_closure);
                }
                if let Some(fb) = &s.finally_body {
                    for st in fb {
                        walk_stmt(st, declared, closure_refs, in_closure);
                    }
                }
            }
            Statement::CaseStatement(s) => {
                walk_expr(&s.value, declared, closure_refs, in_closure);
                for arm in &s.when_branches {
                    walk_expr(&arm.match_expr, declared, closure_refs, in_closure);
                    for st in &arm.body {
                        walk_stmt(st, declared, closure_refs, in_closure);
                    }
                }
                if let Some(db) = &s.default_branch {
                    for st in db {
                        walk_stmt(st, declared, closure_refs, in_closure);
                    }
                }
            }
            Statement::ExpressionStatement(es) => {
                walk_expr(&es.expression, declared, closure_refs, in_closure);
            }
            Statement::ThrowStatement(t) => {
                walk_expr(&t.expression, declared, closure_refs, in_closure);
            }
            Statement::NowaitStatement(n) => {
                walk_expr(&n.expression, declared, closure_refs, in_closure);
            }
            Statement::ReturnStatement(r) => {
                if let Some(e) = &r.value {
                    walk_expr(e, declared, closure_refs, in_closure);
                }
            }
            _ => {}
        }
    }

    fn walk_expr(
        expr: &Expression,
        declared: &mut HashSet<String>,
        closure_refs: &mut HashSet<String>,
        in_closure: bool,
    ) {
        match expr {
            Expression::IdentifierExpression(id) => {
                if in_closure {
                    closure_refs.insert(id.name.clone());
                }
            }
            Expression::FunctionExpression(fd) => {
                // Descend into the closure body with in_closure=true.
                for st in &fd.body {
                    walk_stmt(st, declared, closure_refs, /* in_closure */ true);
                }
            }
            Expression::BinaryExpression(b) => {
                walk_expr(&b.left, declared, closure_refs, in_closure);
                walk_expr(&b.right, declared, closure_refs, in_closure);
            }
            Expression::UnaryExpression(u) => {
                walk_expr(&u.expression, declared, closure_refs, in_closure);
            }
            Expression::CallExpression(c) => {
                walk_expr(&c.callee, declared, closure_refs, in_closure);
                for a in &c.args {
                    walk_expr(&a.value, declared, closure_refs, in_closure);
                }
            }
            Expression::MemberExpression(m) => {
                walk_expr(&m.object, declared, closure_refs, in_closure);
            }
            Expression::IndexExpression(ix) => {
                walk_expr(&ix.object, declared, closure_refs, in_closure);
                walk_expr(&ix.index, declared, closure_refs, in_closure);
            }
            Expression::ArrayExpression(a) => {
                for it in &a.items {
                    walk_expr(it, declared, closure_refs, in_closure);
                }
            }
            Expression::DictionaryExpression(d) => {
                for e in &d.entries {
                    walk_expr(&e.value, declared, closure_refs, in_closure);
                }
            }
            Expression::TupleExpression(t) => {
                for it in &t.items {
                    walk_expr(it, declared, closure_refs, in_closure);
                }
            }
            Expression::RangeExpression(r) => {
                walk_expr(&r.start, declared, closure_refs, in_closure);
                walk_expr(&r.end, declared, closure_refs, in_closure);
            }
            Expression::OptionalCheckExpression(e) => {
                walk_expr(&e.expression, declared, closure_refs, in_closure);
            }
            Expression::ForceUnwrapExpression(e) => {
                walk_expr(&e.expression, declared, closure_refs, in_closure);
            }
            Expression::TemplateStringExpression(ts) => {
                for p in &ts.parts {
                    if let fai_compiler::ast::TemplateStringPart::Expression { expression } = p {
                        walk_expr(expression, declared, closure_refs, in_closure);
                    }
                }
            }
            _ => {}
        }
    }

    let mut declared: HashSet<String> = HashSet::new();
    let mut closure_refs: HashSet<String> = HashSet::new();
    for st in body {
        walk_stmt(st, &mut declared, &mut closure_refs, false);
    }
    declared.intersection(&closure_refs).cloned().collect()
}

/// Approximate pre-pass for nested closures: collect every identifier
/// name referenced somewhere in a closure body, including assignment
/// targets. The caller uses this to force the enclosing closure to
/// capture transitive outer names that are only mentioned by a child
/// closure, so they exist in the parent env when the child closure is
/// allocated.
fn collect_referenced_names(body: &[fai_compiler::ast::Statement]) -> HashSet<String> {
    use fai_compiler::ast::{Expression, Statement};

    fn walk_stmt(stmt: &Statement, names: &mut HashSet<String>) {
        match stmt {
            Statement::VarStatement(vs) => walk_expr(&vs.value, names),
            Statement::LetStatement(ls) => walk_expr(&ls.value, names),
            Statement::AssignmentStatement(a) => {
                match &a.target {
                    fai_compiler::ast::AssignmentTarget::Variables { names: vars } => {
                        for name in vars {
                            names.insert(name.clone());
                        }
                    }
                    fai_compiler::ast::AssignmentTarget::Field { object }
                    | fai_compiler::ast::AssignmentTarget::Index { object } => {
                        walk_expr(object, names);
                    }
                }
                walk_expr(&a.value, names);
            }
            Statement::IfStatement(s) => {
                for branch in &s.branches {
                    walk_expr(&branch.condition, names);
                    for st in &branch.body {
                        walk_stmt(st, names);
                    }
                }
                if let Some(else_body) = &s.else_branch {
                    for st in else_body {
                        walk_stmt(st, names);
                    }
                }
            }
            Statement::WhileStatement(s) => {
                walk_expr(&s.condition, names);
                for st in &s.body {
                    walk_stmt(st, names);
                }
            }
            Statement::ForStatement(s) => {
                walk_expr(&s.items, names);
                for st in &s.body {
                    walk_stmt(st, names);
                }
            }
            Statement::TryStatement(s) => {
                for st in &s.try_body {
                    walk_stmt(st, names);
                }
                for st in &s.catch_body {
                    walk_stmt(st, names);
                }
                if let Some(finally_body) = &s.finally_body {
                    for st in finally_body {
                        walk_stmt(st, names);
                    }
                }
            }
            Statement::CaseStatement(s) => {
                walk_expr(&s.value, names);
                for branch in &s.when_branches {
                    walk_expr(&branch.match_expr, names);
                    for st in &branch.body {
                        walk_stmt(st, names);
                    }
                }
                if let Some(default_body) = &s.default_branch {
                    for st in default_body {
                        walk_stmt(st, names);
                    }
                }
            }
            Statement::ExpressionStatement(es) => walk_expr(&es.expression, names),
            Statement::ThrowStatement(t) => walk_expr(&t.expression, names),
            Statement::NowaitStatement(n) => walk_expr(&n.expression, names),
            Statement::ReturnStatement(r) => {
                if let Some(expr) = &r.value {
                    walk_expr(expr, names);
                }
            }
            _ => {}
        }
    }

    fn walk_expr(expr: &Expression, names: &mut HashSet<String>) {
        match expr {
            Expression::IdentifierExpression(id) => {
                names.insert(id.name.clone());
            }
            Expression::FunctionExpression(fd) => {
                for st in &fd.body {
                    walk_stmt(st, names);
                }
            }
            Expression::BinaryExpression(b) => {
                walk_expr(&b.left, names);
                walk_expr(&b.right, names);
            }
            Expression::UnaryExpression(u) => walk_expr(&u.expression, names),
            Expression::CallExpression(c) => {
                walk_expr(&c.callee, names);
                for arg in &c.args {
                    walk_expr(&arg.value, names);
                }
            }
            Expression::MemberExpression(m) => walk_expr(&m.object, names),
            Expression::IndexExpression(ix) => {
                walk_expr(&ix.object, names);
                walk_expr(&ix.index, names);
            }
            Expression::ArrayExpression(a) => {
                for item in &a.items {
                    walk_expr(item, names);
                }
            }
            Expression::DictionaryExpression(d) => {
                for entry in &d.entries {
                    walk_expr(&entry.value, names);
                }
            }
            Expression::TupleExpression(t) => {
                for item in &t.items {
                    walk_expr(item, names);
                }
            }
            Expression::RangeExpression(r) => {
                walk_expr(&r.start, names);
                walk_expr(&r.end, names);
            }
            Expression::TemplateStringExpression(t) => {
                for part in &t.parts {
                    if let fai_compiler::ast::TemplateStringPart::Expression { expression } = part {
                        walk_expr(expression, names);
                    }
                }
            }
            Expression::OptionalCheckExpression(o) => walk_expr(&o.expression, names),
            Expression::ForceUnwrapExpression(f) => walk_expr(&f.expression, names),
            Expression::NumberExpression(_)
            | Expression::BooleanExpression(_)
            | Expression::NullExpression(_)
            | Expression::StringExpression(_) => {}
        }
    }

    let mut names = HashSet::new();
    for stmt in body {
        walk_stmt(stmt, &mut names);
    }
    names
}

fn shape_for_type(ty: &fai_checker::types::Type) -> ValueShape {
    match ty {
        fai_checker::types::Type::Int => ValueShape::RawInt,
        fai_checker::types::Type::Float => ValueShape::RawFloat,
        fai_checker::types::Type::Bool => ValueShape::RawBool,
        _ => ValueShape::Boxed,
    }
}

fn shape_for_type_node(ty: &TypeNode) -> ValueShape {
    if ty.is_array
        || ty.is_optional
        || ty.is_type_parameter == Some(true)
        || ty.function_params.is_some()
        || ty.function_returns.is_some()
    {
        return ValueShape::Boxed;
    }

    match ty.name.as_deref() {
        Some("Int") => ValueShape::RawInt,
        Some("Float") => ValueShape::RawFloat,
        Some("Bool") => ValueShape::RawBool,
        _ => ValueShape::Boxed,
    }
}

/// A closure proto the builder created while lowering a
/// `FunctionExpression`. The caller appends these to the wasm module
/// after the top-level fai functions; each slot lands at
/// `rt.base + RT_COUNT + functions.len() + proto_idx_within_closures`,
/// which is the same index stored in its closure heap object's
/// `table_idx` field.
#[derive(Debug)]
pub struct BuiltClosure {
    pub info: FunctionInfo,
    pub function: Function,
    /// Proto index relative to `functions.len()`. The closure's
    /// `table_idx` at runtime equals this value, and the indirect
    /// function table slot at that index points at the wasm function
    /// for this closure.
    pub proto_index: u32,
    /// Async closure (body awaits/forks): `function` is a resume fn (`()->()`)
    /// driven by the scheduler, not a `FaiFunc(N)`. The assembler emits the
    /// resume type for it, and its heap header carries a non-zero frame size.
    pub is_async: bool,
}

/// Output of `build_function`. `main` is the requested function;
/// `closures` are any anonymous `def` expressions encountered in its
/// body (or nested). Callers attach them to the wasm module alongside
/// the top-level fai functions.
#[derive(Debug)]
pub struct BuildResult {
    pub main: Function,
    pub closures: Vec<BuiltClosure>,
}

/// Build a wasm `Function` (plus any closure protos it referenced)
/// from a forai function AST.
///
/// `fd.params` allocate locals `[0..param_count)` (typed `i64` — the
/// NaN-boxed value). The builder then appends its own locals starting
/// at `param_count`. Local declarations are emitted at the function
/// header when `finish()` runs.
///
/// `functions` is the top-level function list — index into this slice
/// is the function's proto index, and `CallExpression` with an
/// `IdentifierExpression` callee resolves by name against this table.
/// Pass an empty slice for leaf functions that only use builtins.
///
/// `fai_func_type_indices` maps a param-count to the wasm `type` index
/// for the corresponding `FaiFunc(N)` signature — needed for
/// `CallIndirect` when a closure is invoked. Pass an empty map if
/// the body is known to contain no closure calls.
pub fn build_function(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
) -> Result<BuildResult, BuildError> {
    build_function_with_module(
        fd,
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        strings,
        None,
    )
}

/// Same as [`build_function`] but with a `module_context` — the
/// canonical name of the user module this function belongs to, e.g.,
/// `"mypkg.helpers"`. When set, unqualified identifier calls inside
/// the body fall back to `"{module_context}.{name}"` lookups before
/// refusing, so peer functions inside the module can call each
/// other without the `helpers.foo` prefix.
pub fn build_function_with_module(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
    module_context: Option<&str>,
) -> Result<BuildResult, BuildError> {
    let empty_enums: HashMap<String, Vec<String>> = HashMap::new();
    build_function_with_enums(
        fd,
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        strings,
        &empty_enums,
        module_context,
    )
}

/// Extended builder that also takes the program's `enum` declarations
/// so `Status.ready`-style member references can lower to the
/// member's integer index. `build_program_full` is the production
/// entry that collects enums from top-level `enum Name ... end`
/// statements and threads them through this constructor.
pub fn build_function_with_enums(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
    enum_members: &HashMap<String, Vec<String>>,
    module_context: Option<&str>,
) -> Result<BuildResult, BuildError> {
    let empty_types: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> = HashMap::new();
    let empty_imports: HashMap<String, String> = HashMap::new();
    build_function_with_types(
        fd,
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        strings,
        enum_members,
        &empty_types,
        &empty_imports,
        module_context,
    )
}

/// Full builder entry that also accepts user-defined `type`
/// declarations so `TypeName(field: value, ...)` calls can lower
/// to the equivalent dict literal. Production callers go through
/// `build_program_full`, which collects type decls from top-level
/// `type Name ... end` statements and forwards them here.
pub fn build_function_with_types(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
    enum_members: &HashMap<String, Vec<String>>,
    type_fields: &HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>>,
    named_imports: &HashMap<String, String>,
    module_context: Option<&str>,
) -> Result<BuildResult, BuildError> {
    let empty_mocked: HashSet<u32> = HashSet::new();
    let empty_std_ids: HashMap<(String, String), u32> = HashMap::new();
    build_function_with_spy(
        fd,
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        strings,
        enum_members,
        type_fields,
        named_imports,
        &empty_mocked,
        &empty_std_ids,
        module_context,
    )
}

/// Full builder entry that also takes the set of `fn_id`s that
/// need spy/mock instrumentation plus the std-method -> fn_id
/// lookup. Production callers go through `build_program_full`,
/// which collects both tables from every test block in `is_test`
/// builds (both empty otherwise).
pub fn build_function_with_spy(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
    enum_members: &HashMap<String, Vec<String>>,
    type_fields: &HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>>,
    named_imports: &HashMap<String, String>,
    mocked_fn_ids: &HashSet<u32>,
    std_method_fn_ids: &HashMap<(String, String), u32>,
    module_context: Option<&str>,
) -> Result<BuildResult, BuildError> {
    let ownership_sites = RefCell::new(Vec::new());
    build_function_with_spy_and_offset(
        fd,
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        strings,
        enum_members,
        type_fields,
        named_imports,
        mocked_fn_ids,
        std_method_fn_ids,
        0,
        module_context,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &ownership_sites,
        None,
        None,
    )
}

/// Same as [`build_function_with_spy`] but also takes the number
/// of closures already emitted by earlier top-level functions in
/// the same program. The builder uses this to bake the
/// globally-correct `table_idx` into each closure's heap header
/// so call-indirect lands on the right wasm function when a
/// program has multiple top-level functions each registering
/// their own closures.
pub(crate) fn build_function_with_spy_and_offset(
    fd: &FunctionDeclaration,
    rt: RtOffsets,
    functions: &[FunctionInfo],
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    module_aliases: &HashMap<String, String>,
    extern_fn_indices: &HashMap<String, u16>,
    import_remap: &[Option<u32>],
    strings: &RefCell<StringInterner>,
    enum_members: &HashMap<String, Vec<String>>,
    type_fields: &HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>>,
    named_imports: &HashMap<String, String>,
    mocked_fn_ids: &HashSet<u32>,
    std_method_fn_ids: &HashMap<(String, String), u32>,
    closure_offset_base: u32,
    module_context: Option<&str>,
    module_constants: &HashMap<String, fai_compiler::ast::Expression>,
    extern_out_params: &HashMap<String, Vec<bool>>,
    module_vars: &HashMap<String, u32>,
    ownership_sites: &RefCell<Vec<crate::debug_info::OwnershipSiteDebugEntry>>,
    file_path: Option<&str>,
    async_ctx: Option<AsyncClosureCtx<'_>>,
) -> Result<BuildResult, BuildError> {
    let closures = RefCell::new(Vec::new());
    let ctx = BuildContext {
        rt,
        functions,
        checker,
        fai_func_type_indices,
        module_aliases,
        extern_fn_indices,
        import_remap,
        enum_members,
        type_fields,
        named_imports,
        mocked_fn_ids,
        std_method_fn_ids,
        closure_offset_base,
        strings,
        closures: &closures,
        module_constants,
        extern_out_params,
        module_vars,
        ownership_sites,
        file_path,
        async_ctx,
    };
    let main = {
        let mut b = Builder::new(fd, &ctx, None);
        if let Some(module_context) = module_context {
            b.module_context = Some(module_context.to_string());
        }
        // Per-call-site keys (UFCS, named-param reorder, expression
        // types) need to disambiguate by file — two files in one
        // module can have a call at the same (line, col), and using
        // the module name alone collides them. Prefer file_path
        // when known; fall back to module_context. Mirrors
        // `Checker::location_key()` so checker-recorded entries
        // round-trip through codegen.
        b.module_key = file_path
            .or(module_context)
            .map(String::from)
            .unwrap_or_default();
        b.compile_body()?;
        b.finish()
    };
    Ok(BuildResult {
        main,
        closures: closures.into_inner(),
    })
}

/// Shared state across nested closure compilations so they agree on
/// proto indices + can append new protos to one list. The
/// `closures` RefCell lets inner builders append without each Builder
/// needing its own `&mut` to the list.
#[derive(Clone, Copy)]
struct BuildContext<'a> {
    rt: RtOffsets,
    functions: &'a [FunctionInfo],
    checker: &'a CheckerInfo,
    /// Host-import remap: `import_remap[canonical_idx]` is the new
    /// wasm function index after target-specific filtering (e.g.,
    /// `wasm-html` disables `http_server_*`), or `None` if the
    /// import is unavailable. Matches `runtime::build_import_remap`'s
    /// output so we share the same policy as the bytecode codegen.
    import_remap: &'a [Option<u32>],
    /// `param_count` → wasm type index for `FaiFunc(N)`. Direct builder
    /// needs this when emitting `CallIndirect` — the type index is
    /// assigned by the module assembler, not by the builder.
    fai_func_type_indices: &'a HashMap<u16, u32>,
    /// Top-level `use` bindings — maps the source-visible alias (the
    /// last path segment of `use std.X.Y`) to its canonical dotted
    /// path `"std.X.Y"`. The module dispatcher keys on the canonical
    /// form so rename-via-`use` doesn't multiply the dispatch table.
    module_aliases: &'a HashMap<String, String>,
    /// Extern fn name → index into the program's extern table.
    /// Populated from top-level `extern { ... }` blocks by the
    /// caller; read here to route bare identifier calls to
    /// `IMPORT_CALL_FFI` instead of the normal function lookup.
    extern_fn_indices: &'a HashMap<String, u16>,
    /// `enum Name` declarations keyed by enum name, value is the
    /// declared member list in order. Used to lower `Status.ready`
    /// member references to a NaN-boxed Int (the member's index).
    /// Equality between two enum values is just integer equality.
    enum_members: &'a HashMap<String, Vec<String>>,
    /// `type Name ... end` declarations by name → ordered field list
    /// (name, default expression if any, optional-ness). Used to
    /// lower `Name(a: 1, b: 'x')` into the equivalent dict literal,
    /// filling unsupplied fields from defaults or `null` when the
    /// field type is optional.
    type_fields: &'a HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>>,
    /// `use { X, Y } from mod` imports flattened to
    /// `X -> mod.X`, `Y -> mod.Y`. Used to resolve bare calls to
    /// functions whose canonical name is module-prefixed.
    named_imports: &'a HashMap<String, String>,
    /// Fn-ids that are referenced by `mock()` / `mockOnce()` /
    /// `mockReset()` / `assert.calledWith()` / `assert.callCount()` /
    /// `assert.notCalled()` somewhere in the test blocks. Each
    /// such function gets a preamble that calls `spy_check_call`
    /// before its real body runs. Covers both user functions and
    /// std-method targets (via `std_method_fn_ids`).
    mocked_fn_ids: &'a HashSet<u32>,
    /// `(canonical_module, method_name) -> fn_id` for every std
    /// method the program mocks or asserts on. The call-site
    /// interception code reads this to decide whether to wrap a
    /// module call with a spy-check.
    std_method_fn_ids: &'a HashMap<(String, String), u32>,
    /// Global closure-table offset for this top-level function.
    /// Each `compile_function_expression` records the closure at
    /// `closure_offset_base + self.ctx.closures.borrow().len()`
    /// so the `table_idx` baked into the closure header matches
    /// the closure's slot in the module-level element section.
    /// Without this, two top-level functions would each start
    /// numbering their closures at 0 and forwarding closures
    /// across functions would all resolve to the first slot.
    closure_offset_base: u32,
    /// Shared string interner — `StringExpression` compilation appends
    /// through this, and the module assembler consumes the final
    /// bytes to emit a `DataSection`. One pool across all the
    /// function bodies in a compilation unit.
    strings: &'a RefCell<StringInterner>,
    closures: &'a RefCell<Vec<BuiltClosure>>,
    /// Module-level `let NAME = <literal>` bindings — names that
    /// aren't locals or functions but expand inline to a constant
    /// literal expression wherever referenced. Used by forsqlite
    /// (`let SQLITE_OK = 0`) and any other library that declares
    /// constants at file scope. Only compile-time-known literals
    /// (Int/Float/Bool/String/null) are inlined; more complex
    /// initialisers are ignored and flow through as unknown names.
    module_constants: &'a HashMap<String, fai_compiler::ast::Expression>,
    /// Per-extern `is_out` flags per parameter. Used by
    /// `compile_extern_call` to emit a readback (store the
    /// host-written out-slot back into the calling local) for each
    /// OUT parameter, so C-style `out` arguments (`sqlite3_open(path,
    /// out db)`) propagate the written pointer back to the forai
    /// binding.
    extern_out_params: &'a HashMap<String, Vec<bool>>,
    /// Module-level `var NAME = EXPR` bindings — names that resolve
    /// to a dedicated mutable wasm global (i64 Boxed). Populated by
    /// `build_program_full` from top-level var statements in the
    /// entry AST and every discovered module; the indices line up
    /// with the global slots appended by `assemble_wasm_module`
    /// after the fixed runtime globals. Reads lower to `GlobalGet`,
    /// writes to `GlobalSet`; initialisers are compiled into a
    /// synthesised module-init function.
    module_vars: &'a HashMap<String, u32>,
    /// Shared ownership instrumentation site table. Each helper-level
    /// ownership event allocates a dense nonzero id here and emits it
    /// in `__fai_ownership_event`.
    ownership_sites: &'a RefCell<Vec<crate::debug_info::OwnershipSiteDebugEntry>>,
    /// Best known source file for the function currently being compiled.
    file_path: Option<&'a str>,
    /// Async-engine context for compiling closures that await/fork as resume
    /// fns (A3.0). `None` on the pure-sync build path.
    async_ctx: Option<AsyncClosureCtx<'a>>,
}

#[derive(Clone, Copy)]
struct LocalBinding {
    local: u32,
    shape: ValueShape,
    /// When true, `local` is an i32 local holding the address of an
    /// 8-byte heap cell containing the NaN-boxed value. Reads
    /// `i64.load` from the cell; writes `i64.store`. Cell-bound
    /// bindings are used for `var`s that escape into closures — this
    /// is how the outer scope and nested `do...end` blocks share one
    /// mutable slot. `shape` is always `Boxed` when `is_cell` is
    /// true.
    is_cell: bool,
}

#[derive(Clone, Copy)]
enum CaptureSource {
    Local(LocalBinding),
    Upvalue(u32),
}

#[derive(Clone, Copy)]
struct CaptureBinding {
    source: CaptureSource,
    shape: ValueShape,
    is_cell: bool,
}

/// View onto the outer builder's local bindings — used by an inner
/// closure builder to resolve identifiers that aren't in the closure's
/// own scopes. Lookups return the outer binding so capture can box raw
/// primitives into the closure heap.
struct OuterScopeView<'o> {
    scopes: &'o [HashMap<String, LocalBinding>],
    upvalues: &'o [CaptureBinding],
    upvalue_by_name: &'o HashMap<String, u32>,
}

impl<'o> OuterScopeView<'o> {
    fn lookup(&self, name: &str) -> Option<CaptureBinding> {
        for scope in self.scopes.iter().rev() {
            if let Some(&binding) = scope.get(name) {
                return Some(CaptureBinding {
                    source: CaptureSource::Local(binding),
                    shape: binding.shape,
                    is_cell: binding.is_cell,
                });
            }
        }
        if let Some(&uv_idx) = self.upvalue_by_name.get(name) {
            let binding = self.upvalues[uv_idx as usize];
            return Some(CaptureBinding {
                source: CaptureSource::Upvalue(uv_idx),
                shape: binding.shape,
                is_cell: binding.is_cell,
            });
        }
        None
    }
}

fn ownership_reason(op: OwnershipOp) -> &'static str {
    match op {
        OwnershipOp::Retain => "retain borrowed value",
        OwnershipOp::Release => "release owned value",
        OwnershipOp::Transfer => "transfer owned value",
        OwnershipOp::Borrow => "borrow value",
        OwnershipOp::Store => "store owned value",
        OwnershipOp::Overwrite => "overwrite owned slot",
        OwnershipOp::Cleanup => "cleanup owned value",
        OwnershipOp::Return => "return owned value",
        OwnershipOp::Discard => "discard owned value",
        OwnershipOp::CallArgument => "prepare call argument",
    }
}

fn ownership_seed_suppresses(op: OwnershipOp) -> bool {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        static SEED: OnceLock<Option<OwnershipOp>> = OnceLock::new();
        let seed = SEED.get_or_init(|| {
            let value = std::env::var("FAI_OWNERSHIP_SEED").ok()?;
            let Some(name) = value.strip_prefix("suppress-") else {
                eprintln!(
                    "[ownership-check] FAI_OWNERSHIP_SEED='{}' is not a known seed — seed inactive",
                    value
                );
                return None;
            };
            for op in OwnershipOp::ALL {
                if op.name() == name {
                    return Some(op);
                }
            }
            eprintln!(
                "[ownership-check] FAI_OWNERSHIP_SEED='{}' names no ownership op — seed inactive",
                value
            );
            None
        });
        return *seed == Some(op);
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = op;
        false
    }
}

/// How an identifier resolves in the current builder's scope stack.
/// `Local` is the ordinary case; `Upvalue` means the binding came from
/// the outer scope (only possible when the builder is compiling a
/// closure body) and needs to be read via `env_ptr + N*8`. `ModuleVar`
/// means the binding is a top-level `var` whose NaN-boxed value lives
/// in a dedicated wasm global — reads are `GlobalGet(idx)`, writes are
/// `GlobalSet(idx)`.
#[derive(Clone, Copy)]
enum Resolve {
    Local(LocalBinding),
    Upvalue(u32),
    ModuleVar(u32),
}

/// Internal builder state.
struct Builder<'a, 'c> {
    fd: &'a FunctionDeclaration,
    ctx: &'c BuildContext<'a>,
    /// Instructions for the function body. `finish()` wraps them in a
    /// `wasm_encoder::Function` with the right local declarations.
    instrs: Vec<Instruction<'static>>,
    /// Local-index counter. Starts after the params.
    next_local: u32,
    /// Per-scope name → local binding map. Innermost scope at the back.
    scopes: Vec<HashMap<String, LocalBinding>>,
    /// Phase 3 reclamation (plans/111): per-scope list of wasm locals holding
    /// confined fresh-literal (Array/Dict/Tuple) bindings — `rt_drop`'d when the
    /// scope exits. Parallel to `scopes`. `pop_scope` drops the top list on
    /// fall-through; non-trap early exits use targeted cleanup depths so they
    /// release the scopes they skip before branching.
    scope_drops: Vec<Vec<u32>>,
    /// Names that escape this function under a conservative intraprocedural
    /// analysis (every call assumed to retain). A binding is a drop candidate
    /// only if its name is NOT here. Computed once in `new`.
    confined_escaping: std::collections::HashSet<String>,
    /// Declared local types in emission order (i.e. what goes in the
    /// function header). The params themselves are NOT in this list
    /// because `wasm_encoder::Function::new` treats them as declared
    /// by the function signature.
    local_decls: Vec<ValType>,
    /// Stack of active loops. `break` targets the enclosing `block`;
    /// `continue` targets the `loop` itself. Depths in `LoopFrame`
    /// are absolute relative to the function root and compared
    /// against `block_depth` at use sites.
    loops: Vec<LoopFrame>,
    /// Stack of active `try` bodies. A `throw` inside the try body
    /// stores its value into `err_local` and branches to the inner
    /// `$catch` block at `catch_abs`.
    tries: Vec<TryFrame>,
    /// Running count of open structured control-flow labels — kept in
    /// sync by `emit_open`/`emit_close` wrappers so that `br` depth
    /// for `break`/`continue` is a single subtraction.
    block_depth: u32,
    /// Name → proto index. Built once from `ctx.functions` in `new()`.
    function_by_name: HashMap<String, u32>,
    /// Module name key used to look up per-call-site info in
    /// `checker.ufcs_calls` and friends. Derived from `fd.module`
    /// where available; empty string for the entry module.
    module_key: String,
    /// When this function was declared inside a user module (e.g.,
    /// `"mypkg.helpers"`), unqualified identifier calls fall back
    /// to `"{module_context}.{name}"` lookups so peer functions in
    /// the same module can call each other without the alias
    /// prefix. `None` for functions in the entry AST.
    module_context: Option<String>,
    /// Optional snapshot of the enclosing builder's scope stack.
    /// `Some` when this builder is compiling a closure body; `None`
    /// for a top-level function. Identifiers that don't resolve in
    /// `scopes` fall through to `outer_scope` and become upvalues.
    outer_scope: Option<&'c OuterScopeView<'c>>,
    /// Upvalues captured from the outer scope, in capture order.
    /// Value is the outer scope's binding — the closure-creation site
    /// reads it and boxes raw primitives to populate the i64 upvalue slot.
    upvalues: Vec<CaptureBinding>,
    /// Name → upvalue index. Separate from `scopes` because an upvalue
    /// reference emits `GlobalGet(env_ptr) + I64Load(off)`, not a
    /// `LocalGet`.
    upvalue_by_name: HashMap<String, u32>,
    /// Set of `var` names in this function's scope that are referenced
    /// inside a nested closure — they must be allocated as heap cells
    /// so the outer scope and the closure share one mutable slot.
    /// Populated once at `new()` via `collect_cell_captured_vars`.
    cell_captured_vars: HashSet<String>,
    /// Async-frame locals that OWN their value (`+1`) for the task's
    /// lifetime and are released at completion (`emit_async_drops`)
    /// rather than at a sync scope exit. `compile_assignment` treats
    /// them like `note_droppable`d locals (retain-new / release-old)
    /// so reassignment — `html = html + piece` accumulators — keeps
    /// the slot at exactly one owned ref instead of leaking the old
    /// value. Populated by `build_resume_fn` from the release set.
    owned_frame_locals: HashSet<u32>,
    /// Async resume segment error route. When set, sync helper calls that set
    /// `error_flag` fail the current scheduler task or jump to the async catch
    /// block instead of returning from the resume function.
    async_error_ctx: Option<AsyncErrorContext>,
}

/// Per-`try` bookkeeping for `throw` dispatch. Popped *before* the
/// catch body compiles so a throw inside `catch` propagates to the
/// next-outer try.
#[derive(Clone, Copy)]
struct TryFrame {
    /// Absolute `block_depth` of the inner `$catch` block.
    catch_abs: u32,
    /// `scope_drops` depth that remains live at the catch handler.
    cleanup_depth: usize,
    /// Local that holds the thrown value until the `$catch` block
    /// binds it to the user-declared `catch_name`.
    err_local: u32,
}

#[derive(Clone, Copy)]
struct AsyncErrorContext {
    layout: crate::async_engine::SchedLayout,
    loop_depth: u32,
    catch: Option<(usize, u32)>,
}

/// Label bookkeeping for `break` / `continue`. Each `while` lowering
/// pushes one of these before emitting the outer `block` + inner
/// `loop`; relative `br` depth at a use site is
/// `current_block_depth - target_abs`.
#[derive(Clone, Copy)]
struct LoopFrame {
    /// Absolute `block_depth` after the outer `block` was opened —
    /// the `break` target.
    break_abs: u32,
    /// Absolute `block_depth` after the inner `loop` was opened —
    /// the `continue` target.
    continue_abs: u32,
    /// `scope_drops` depth that remains live after `break`/`continue`.
    cleanup_depth: usize,
}

fn stmt_variant_name(s: &Statement) -> &'static str {
    match s {
        Statement::UseStatement(_) => "UseStatement",
        Statement::LetStatement(_) => "LetStatement",
        Statement::VarStatement(_) => "VarStatement",
        Statement::AssignmentStatement(_) => "AssignmentStatement",
        Statement::FunctionDeclaration(_) => "FunctionDeclaration",
        Statement::TypeDeclaration(_) => "TypeDeclaration",
        Statement::EnumDeclaration(_) => "EnumDeclaration",
        Statement::TestDeclaration(_) => "TestDeclaration",
        Statement::IfStatement(_) => "IfStatement",
        Statement::CaseStatement(_) => "CaseStatement",
        Statement::TryStatement(_) => "TryStatement",
        Statement::ThrowStatement(_) => "ThrowStatement",
        Statement::ForStatement(_) => "ForStatement",
        Statement::WhileStatement(_) => "WhileStatement",
        Statement::BreakStatement(_) => "BreakStatement",
        Statement::ContinueStatement(_) => "ContinueStatement",
        Statement::ReturnStatement(_) => "ReturnStatement",
        Statement::ExpressionStatement(_) => "ExpressionStatement",
        Statement::ExternBlockDeclaration(_) => "ExternBlockDeclaration",
        Statement::NowaitStatement(_) => "NowaitStatement",
        Statement::FunctionTypeDefDeclaration(_) => "FunctionTypeDefDeclaration",
    }
}

fn expr_variant_name(e: &Expression) -> &'static str {
    match e {
        Expression::IdentifierExpression(_) => "IdentifierExpression",
        Expression::StringExpression(_) => "StringExpression",
        Expression::TemplateStringExpression(_) => "TemplateStringExpression",
        Expression::NumberExpression(_) => "NumberExpression",
        Expression::BooleanExpression(_) => "BooleanExpression",
        Expression::NullExpression(_) => "NullExpression",
        Expression::ArrayExpression(_) => "ArrayExpression",
        Expression::DictionaryExpression(_) => "DictionaryExpression",
        Expression::TupleExpression(_) => "TupleExpression",
        Expression::RangeExpression(_) => "RangeExpression",
        Expression::CallExpression(_) => "CallExpression",
        Expression::MemberExpression(_) => "MemberExpression",
        Expression::UnaryExpression(_) => "UnaryExpression",
        Expression::OptionalCheckExpression(_) => "OptionalCheckExpression",
        Expression::ForceUnwrapExpression(_) => "ForceUnwrapExpression",
        Expression::BinaryExpression(_) => "BinaryExpression",
        Expression::IndexExpression(_) => "IndexExpression",
        Expression::FunctionExpression(_) => "FunctionExpression",
    }
}

#[cfg(test)]
mod tests;
