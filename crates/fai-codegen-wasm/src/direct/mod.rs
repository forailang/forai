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
    IMPORT_REMOTE_CALL, IMPORT_REPLACE_LOCATION, IMPORT_SET_HTML, IMPORT_SET_HTML_AT,
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
    RT_GET_INDEX, RT_GT, RT_IDIV, RT_IS_FLOAT, RT_IS_INT, RT_IS_OBJ, RT_LE, RT_LIVE_OBJECTS, RT_LT,
    RT_MAKE_BOOL, RT_MAKE_FLOAT, RT_MAKE_INT, RT_MAKE_OBJ, RT_MOD, RT_MUL, RT_NE, RT_NEG,
    RT_OBJ_ADDR, RT_PARSE_FLOAT, RT_PARSE_INT, RT_POW, RT_PRINT_VAL_NEW, RT_RELEASE, RT_RETAIN,
    RT_SET_FIELD, RT_STR_EQ, RT_SUB, RT_VALUE_TO_STR, TAG_BOOL, TAG_INT, VAL_FALSE, VAL_NULL,
    VAL_VOID,
};

mod assemble;
mod async_lower;
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
        IMPORT_FILE_READ_STR | IMPORT_WRITE_FILE | IMPORT_FILE_LIST
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

impl<'a, 'c> Builder<'a, 'c> {
    /// Compile an expression, leaving its value on the wasm stack and
    /// returning the shape of that value. This phase is conservative:
    /// expression emitters still leave boxed values, but callers now have
    /// an explicit place to handle future raw primitive shapes.
    fn compile_expr(&mut self, expr: &Expression) -> Result<ValueShape, BuildError> {
        match expr {
            Expression::NumberExpression(n) => {
                if n.is_float {
                    self.emit(Instruction::F64Const(n.value));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_FLOAT));
                } else {
                    self.emit(Instruction::I32Const(n.value as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                }
                Ok(ValueShape::Boxed)
            }
            Expression::BooleanExpression(b) => {
                self.emit(Instruction::I32Const(if b.value { 1 } else { 0 }));
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
                Ok(ValueShape::Boxed)
            }
            Expression::NullExpression(_) => {
                self.emit(Instruction::I64Const(VAL_NULL));
                Ok(ValueShape::Boxed)
            }
            Expression::StringExpression(s) => {
                // Intern the literal into the shared pool and emit a
                // `RT_ALLOC_STRING(offset, len)` that copies the bytes
                // out of the data section into a freshly-allocated
                // String object. Matches translate.rs's handling of
                // `Op::LoadConst` for String constants.
                let (off, len) = self.ctx.strings.borrow_mut().intern(&s.value);
                self.emit(Instruction::I32Const(off as i32));
                self.emit(Instruction::I32Const(len as i32));
                self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
                Ok(ValueShape::Boxed)
            }
            Expression::ArrayExpression(a) => {
                self.compile_array_literal(&a.items)?;
                Ok(ValueShape::Boxed)
            }
            Expression::TupleExpression(t) => {
                self.compile_tuple_literal(&t.items)?;
                Ok(ValueShape::Boxed)
            }
            Expression::DictionaryExpression(d) => {
                self.compile_dict_literal(d)?;
                Ok(ValueShape::Boxed)
            }
            Expression::IndexExpression(ix) => {
                self.compile_index_expr(ix)?;
                Ok(ValueShape::Boxed)
            }
            Expression::TemplateStringExpression(ts) => {
                self.compile_template_string(&ts.parts)?;
                Ok(ValueShape::Boxed)
            }
            Expression::OptionalCheckExpression(oc) => {
                self.compile_optional_check(&oc.expression)?;
                Ok(ValueShape::Boxed)
            }
            Expression::ForceUnwrapExpression(fu) => {
                self.compile_force_unwrap(&fu.expression)?;
                Ok(ValueShape::Boxed)
            }
            Expression::IdentifierExpression(id) => match self.resolve(&id.name) {
                Some(Resolve::Local(local)) => {
                    if local.is_cell {
                        // Cell-bound: local holds the cell address;
                        // deref the value slot (@8, plan 114).
                        self.emit(Instruction::LocalGet(local.local));
                        self.emit(Instruction::I64Load(mem_off(8)));
                    } else {
                        self.emit(Instruction::LocalGet(local.local));
                        self.emit_convert(local.shape, ValueShape::Boxed)?;
                    }
                    Ok(ValueShape::Boxed)
                }
                Some(Resolve::Upvalue(uv)) => {
                    self.emit_upvalue_read(uv);
                    Ok(ValueShape::Boxed)
                }
                Some(Resolve::ModuleVar(global_idx)) => {
                    self.emit(Instruction::GlobalGet(global_idx));
                    Ok(ValueShape::Boxed)
                }
                None => {
                    // Inline a module-level `let NAME = <literal>`
                    // constant (e.g. `SQLITE_OK = 0` from a dep)
                    // directly at the reference site. Only literal
                    // RHSes are eligible — non-literal top-level
                    // initialisers would need real globals with a
                    // program-entry initialiser.
                    if let Some(literal) = self.ctx.module_constants.get(&id.name).cloned() {
                        return self.compile_expr(&literal);
                    }
                    self.compile_function_reference(&id.name)?;
                    Ok(ValueShape::Boxed)
                }
            },
            Expression::BinaryExpression(be) => self.compile_binary(be),
            Expression::UnaryExpression(ue) => self.compile_unary(ue),
            Expression::CallExpression(ce) => {
                self.compile_call(ce)?;
                Ok(ValueShape::Boxed)
            }
            Expression::FunctionExpression(fd) => {
                self.compile_function_expression(fd)?;
                Ok(ValueShape::Boxed)
            }
            // Field access on a dict/instance/error, e.g. `err.message`
            // or `user.name`. Routes through `RT_GET_FIELD` with the
            // interned key name. If the object turns out to be a
            // module alias used as a value (which forai forbids
            // anyway — modules aren't first-class), surface the
            // existing refusal rather than emit broken code.
            Expression::MemberExpression(me) => {
                if let Expression::IdentifierExpression(obj_id) = &*me.object {
                    // Enum member reference: `Status.ready` → NaN-boxed
                    // Int of the member's declaration index. Equality
                    // between two enum values then reduces to integer
                    // equality so `case status when Status.ready` works
                    // via the existing `==` lowering.
                    if let Some(members) = self.ctx.enum_members.get(&obj_id.name) {
                        let idx =
                            members
                                .iter()
                                .position(|m| m == &me.property)
                                .ok_or_else(|| {
                                    BuildError::UnsupportedExpression("enum-member-not-found")
                                })?;
                        self.emit(Instruction::I32Const(idx as i32));
                        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                        return Ok(ValueShape::Boxed);
                    }
                    // A local/upvalue/module-var whose name collides
                    // with a module alias (e.g. a `signal` parameter
                    // inside the `signal` module) wins — the binding
                    // is the real receiver and `obj.property` is a
                    // plain field read. Without this shortcut the
                    // module-alias branch below would treat the
                    // parameter as a first-class module and refuse.
                    let shadowed_by_binding = self.resolve(&obj_id.name).is_some();
                    // Module-qualified function reference used as a
                    // value (e.g. `mock(ui.renderTitle, null)` or
                    // `apply(x, module.fn)`). If `obj.property`
                    // names a top-level function in the aliased
                    // module, synthesize a forwarding closure so
                    // the name can be passed around.
                    if !shadowed_by_binding {
                        if let Some(canonical) = self.ctx.module_aliases.get(&obj_id.name) {
                            let full_name = format!("{}.{}", canonical, me.property);
                            if self.function_by_name.contains_key(&full_name) {
                                self.compile_function_reference(&full_name)?;
                                return Ok(ValueShape::Boxed);
                            }
                            // Std module method as a value — e.g.
                            // `mock(cli.readLine, 'x')`. The only
                            // known consumer is the mock/spy no-op
                            // family which drops the value; emit
                            // `VAL_NULL` as a placeholder. If
                            // something later calls this placeholder,
                            // the wasm call-indirect will trap
                            // (acceptable; the checker will have
                            // flagged most such uses as type errors
                            // anyway).
                            if resolve_module_call(canonical, &me.property).is_some() {
                                self.emit(Instruction::I64Const(VAL_NULL));
                                return Ok(ValueShape::Boxed);
                            }
                        }
                        if self.ctx.module_aliases.contains_key(&obj_id.name)
                            || obj_id.name == "assert"
                        {
                            return Err(BuildError::ModuleAccessNotYetSupported(
                                me.property.clone(),
                            ));
                        }
                    }
                }
                if !self.try_compile_record_field_read(me)? {
                    self.compile_field_access(&me.object, &me.property)?;
                }
                Ok(ValueShape::Boxed)
            }
            _ => Err(BuildError::UnsupportedExpression(expr_variant_name(expr))),
        }
    }

    /// Named-function call: `foo(a, b)` and UFCS `x.foo(a)` → `foo(x, a)`
    /// when the checker marked the call site as a UFCS rewrite.
    ///
    /// Labelled args without a reorder entry are accepted if they
    /// already appear in declaration order; otherwise the builder
    /// refuses with an actionable direct-codegen error.
    fn label_order_for_user_call(
        &self,
        ce: &CallExpression,
        param_names: &[String],
        real_param_count: usize,
        ordered_arg_count: usize,
        is_ufcs: bool,
    ) -> Result<Vec<Option<usize>>, BuildError> {
        if param_names.len() != real_param_count {
            return Err(BuildError::UnsupportedExpression(
                "CallExpression/label-param-shape-mismatch",
            ));
        }
        let mut order: Vec<Option<usize>> = vec![None; real_param_count];
        let mut positional_idx = 0usize;
        if is_ufcs {
            if real_param_count == 0 || ordered_arg_count == 0 {
                return Err(BuildError::UnsupportedExpression(
                    "CallExpression/ufcs-label-shape-mismatch",
                ));
            }
            order[0] = Some(0);
            positional_idx = 1;
        }
        let ordered_offset = if is_ufcs { 1usize } else { 0usize };
        let mut seen_named = false;
        for (source_idx, arg) in ce.args.iter().enumerate() {
            let ordered_idx = source_idx + ordered_offset;
            if ordered_idx >= ordered_arg_count {
                return Err(BuildError::UnsupportedExpression(
                    "CallExpression/label-arg-out-of-range",
                ));
            }
            if let Some(label) = &arg.label {
                seen_named = true;
                let Some(param_idx) = param_names.iter().position(|p| p == label) else {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/unknown-labelled-arg",
                    ));
                };
                if order[param_idx].is_some() {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/duplicate-labelled-arg",
                    ));
                }
                order[param_idx] = Some(ordered_idx);
            } else {
                if seen_named {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/positional-after-labelled",
                    ));
                }
                if positional_idx >= real_param_count {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/label-positional-out-of-range",
                    ));
                }
                if order[positional_idx].is_some() {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/duplicate-positional-arg",
                    ));
                }
                order[positional_idx] = Some(ordered_idx);
                positional_idx += 1;
            }
        }
        Ok(order)
    }

    fn compile_call(&mut self, ce: &CallExpression) -> Result<(), BuildError> {
        let ufcs_key = (
            self.module_key.clone(),
            ce.location.line,
            ce.location.column,
        );
        let is_ufcs = self.checker().ufcs_calls.contains(&ufcs_key);

        // Resolve target name + compute the ordered arg list. UFCS
        // prepends the member's object as the first positional arg.
        let (name, ordered_args): (String, Vec<&Expression>) = match &*ce.callee {
            Expression::IdentifierExpression(id) => {
                (id.name.clone(), ce.args.iter().map(|a| &a.value).collect())
            }
            Expression::MemberExpression(me) if is_ufcs => {
                let mut v: Vec<&Expression> = Vec::with_capacity(ce.args.len() + 1);
                v.push(&me.object);
                v.extend(ce.args.iter().map(|a| &a.value));
                (me.property.clone(), v)
            }
            Expression::MemberExpression(me) => {
                // Module-method call: `alias.method(args)` where
                // `alias` is a top-level `use` binding. Look up the
                // alias → canonical path, then consult:
                //   1. The std dispatch table.
                //   2. The unified function table (for user-module
                //      calls into a sibling .fai file).
                // If both miss, surface `ModuleAccessNotYetSupported`.
                let mut user_module_call: Option<(String, Vec<&Expression>)> = None;
                if let Expression::IdentifierExpression(obj_id) = &*me.object {
                    // `assert.{equals,isTrue,isFalse,...}` is
                    // auto-exposed in test blocks without a `use`
                    // statement, so the alias map won't carry it.
                    // Bypass the map and use the canonical name
                    // directly; the resolver handles it as its own
                    // module. Matches the bytecode translator's
                    // `mod_name == "assert"` special case.
                    //
                    // Local bindings shadow module aliases: when the
                    // receiver identifier is a parameter, let/var, or
                    // module-level `var` (common when a parameter is
                    // named `signal` inside the `signal` module), the
                    // call is a plain method on the binding's value,
                    // not a module dispatch.
                    let shadowed_by_binding = self.resolve(&obj_id.name).is_some();
                    let canonical: Option<String> = if shadowed_by_binding {
                        None
                    } else {
                        self.ctx
                            .module_aliases
                            .get(&obj_id.name)
                            .cloned()
                            .or_else(|| {
                                if obj_id.name == "assert" {
                                    Some("assert".to_string())
                                } else {
                                    None
                                }
                            })
                    };
                    if let Some(canonical) = canonical {
                        if let Some(call) = resolve_module_call(&canonical, &me.property) {
                            // If this std-method was referenced by a
                            // `mock()` or `assert.*` target at compile
                            // time, wrap the call in a spy check so
                            // mocks actually fire. The `std_method_fn_ids`
                            // map is empty in non-test builds so this
                            // path costs nothing in production.
                            if let Some(&fn_id) = self
                                .ctx
                                .std_method_fn_ids
                                .get(&(canonical.clone(), me.property.clone()))
                            {
                                if self.ctx.mocked_fn_ids.contains(&fn_id) {
                                    return self.compile_mocked_module_call(fn_id, &call, &ce.args);
                                }
                            }
                            return self.compile_module_call(&call, &ce.args);
                        }
                        // Not a std module method — try user-module
                        // function lookup. Functions defined in a
                        // discovered `.fai` module land in the unified
                        // function table with their name prefixed by
                        // the module's canonical path, e.g.,
                        // `"mypkg.helpers.doThing"`.
                        let full_name = format!("{}.{}", canonical, me.property);
                        if self.function_by_name.contains_key(&full_name) {
                            user_module_call =
                                Some((full_name, ce.args.iter().map(|a| &a.value).collect()));
                        }
                    }
                }
                match user_module_call {
                    Some(v) => v,
                    None => {
                        // Last resort: for std namespaces that only
                        // register some handlers as bare globals, route
                        // the module-prefix form to the same handler.
                        if let Expression::IdentifierExpression(obj_id) = &*me.object {
                            if let Some(canonical) = self.ctx.module_aliases.get(&obj_id.name) {
                                let route_as_bare = matches!(
                                    canonical.as_str(),
                                    "std.dictionary"
                                        | "std.array"
                                        | "std.convert"
                                        | "std.error"
                                        | "std.browser"
                                );
                                if route_as_bare {
                                    let positional: Vec<&Expression> =
                                        ce.args.iter().map(|a| &a.value).collect();
                                    if let Some(()) =
                                        self.try_compile_bare_global(&me.property, &positional)?
                                    {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        // A bare identifier receiver that doesn't
                        // resolve to any in-scope binding, named
                        // function, or std bare global is almost
                        // always a module-dispatch typo (or a case
                        // where the alias map wasn't populated in
                        // a test harness). Emit the targeted
                        // diagnostic instead of falling through into
                        // the generic closure-call path, which would
                        // just fail later with `UnknownIdentifier`.
                        if let Expression::IdentifierExpression(obj_id) = &*me.object {
                            let is_module_alias =
                                self.ctx.module_aliases.contains_key(&obj_id.name)
                                    || obj_id.name == "assert";
                            let has_binding = self.resolve(&obj_id.name).is_some();
                            let has_function = self.function_by_name.contains_key(&obj_id.name);
                            if is_module_alias || (!has_binding && !has_function) {
                                return Err(BuildError::ModuleAccessNotYetSupported(
                                    me.property.clone(),
                                ));
                            }
                        }
                        // Unqualified member call on a real value —
                        // `matched!.builder()`, `row.cb(x)`,
                        // `state.onUpdate()`. Treat the member
                        // expression as a closure-valued field
                        // access and dispatch through the closure
                        // header.
                        let positional: Vec<&Expression> =
                            ce.args.iter().map(|a| &a.value).collect();
                        return self.compile_indirect_call_from_expr(&ce.callee, &positional);
                    }
                }
            }
            // Any other callee expression — `foo!()`,
            // `eventHandlers[id]()`, `getCb()()`, or any
            // value-producing expression whose type is a closure.
            // Evaluate it to a boxed closure via the normal
            // expression path (which handles `ForceUnwrapExpression`
            // etc. including its null-trap) and dispatch through the
            // closure header. Non-closure values trap at runtime via
            // `RT_OBJ_ADDR`.
            _ => {
                let positional: Vec<&Expression> = ce.args.iter().map(|a| &a.value).collect();
                return self.compile_indirect_call_from_expr(&ce.callee, &positional);
            }
        };

        // Bare-global builtins — these don't require `use std.X`
        // and aren't user-defined. The checker registers them in
        // the global namespace; the direct path routes by name
        // before falling through to extern/user-function lookup.
        if !is_ufcs {
            if let Some(result) = self.try_compile_bare_global(&name, &ordered_args)? {
                let _ = result;
                return Ok(());
            }
        }

        // User-defined type constructor: `TypeName(field: value, ...)`.
        // Lower to the equivalent dict literal, filling unspecified
        // fields from their default expressions (or `null` for
        // optional types). The checker already verified required
        // fields are supplied — any field that reaches this arm
        // without a value and without a default is treated as
        // optional-null.
        if !is_ufcs {
            if self.ctx.type_fields.contains_key(name.as_str()) {
                return self.compile_type_constructor(&name, &ce.args);
            }
        }

        // Extern FFI call: `foo(args)` where `foo` was declared in
        // an `extern { }` block. The compiler pre-assigned an
        // index per declaration; we serialise args to the scratch
        // region at offset 65536 and call `IMPORT_CALL_FFI`.
        // Matches `translate.rs::Op::CallExtern`.
        if !is_ufcs {
            if let Some(&ext_idx) = self.ctx.extern_fn_indices.get(name.as_str()) {
                return self.compile_extern_call(&name, ext_idx, &ordered_args);
            }
        }

        // Closure call: the callee is an identifier not in the
        // top-level function table but bound in the current scope
        // (local or upvalue). Indirect-dispatch through the closure's
        // `table_idx` field.
        if !self.function_by_name.contains_key(name.as_str()) && !is_ufcs {
            if let Some(r) = self.resolve(&name) {
                return self.compile_indirect_call(r, &ordered_args);
            }
        }

        // When this function was compiled inside a user-module
        // context, peer functions (same module) win over a bare
        // entry-AST name of the same spelling. Otherwise an entry
        // file that happens to define a `testNode` helper would
        // shadow every module-local `testNode` everywhere, breaking
        // calls with the wrong arg count. Fall back to the bare
        // lookup when the peer form isn't defined.
        let mut proto_idx: Option<u32> = None;
        let mut resolved_name = name.clone();
        if let Some(ctx_mod) = &self.module_context {
            let qualified = format!("{}.{}", ctx_mod, name);
            if let Some(&p) = self.function_by_name.get(&qualified) {
                proto_idx = Some(p);
                resolved_name = qualified;
            }
        }
        if proto_idx.is_none() {
            if let Some(&p) = self.function_by_name.get(name.as_str()) {
                proto_idx = Some(p);
            }
        }
        // `use { X } from mod` — bare `X(...)` resolves to `mod.X`.
        if proto_idx.is_none() {
            if let Some(qualified) = self.ctx.named_imports.get(name.as_str()) {
                if let Some(&p) = self.function_by_name.get(qualified) {
                    proto_idx = Some(p);
                    resolved_name = qualified.clone();
                }
            }
        }
        // `use { X } from std.Y` — bare `X(...)` dispatches through
        // the std module the same way `Y.X(...)` does. Keeps named
        // imports from std usable without the `Y.` prefix at every
        // call site. UFCS calls (`recv.X(rest)`) synthesise a
        // CallArgument for the receiver and prepend.
        if proto_idx.is_none() {
            if let Some(qualified) = self.ctx.named_imports.get(name.as_str()) {
                if let Some((module, method)) = qualified.rsplit_once('.') {
                    if let Some(call) = resolve_module_call(module, method) {
                        if is_ufcs {
                            if let Expression::MemberExpression(me) = &*ce.callee {
                                let mut args: Vec<fai_compiler::ast::CallArgument> =
                                    Vec::with_capacity(ce.args.len() + 1);
                                args.push(fai_compiler::ast::CallArgument {
                                    label: None,
                                    value: (*me.object).clone(),
                                    location: me.location.clone(),
                                });
                                args.extend_from_slice(&ce.args);
                                return self.compile_module_call(&call, &args);
                            }
                        }
                        return self.compile_module_call(&call, &ce.args);
                    }
                }
            }
        }
        let Some(proto_idx) = proto_idx else {
            // `from_dict` has no expression-level lowering: tail/return
            // positions are desugared into the typed-binding form before
            // codegen (fai-compiler::desugar), so reaching here means an
            // unsupported position (argument, non-named return type, …).
            // A dedicated error keeps this off the UnknownIdentifier
            // path, whose best-effort location walk finds the *first*
            // `from_dict` anywhere in the program — historically an
            // unrelated module (the ISSUES.md misattribution).
            if name == "from_dict" {
                return Err(BuildError::UnsupportedExpression(
                    "from_dict-without-typed-binding",
                ));
            }
            return Err(BuildError::UnknownIdentifier(name));
        };
        let _ = resolved_name; // kept for future diagnostics
        let expected = self.functions()[proto_idx as usize].param_count as usize;
        let type_param_count = self.functions()[proto_idx as usize].type_param_count as usize;
        let real_param_count = expected - type_param_count;
        let defaults = self.functions()[proto_idx as usize].param_defaults.clone();
        let param_names = self.functions()[proto_idx as usize].param_names.clone();
        // Named-param reorder: if the checker recorded a reorder map
        // for this call site, use it to pull caller args into
        // declaration order. Missing slots are filled from default
        // parameter expressions when available.
        let reorder_key = ufcs_key.clone();
        let checker_order = self
            .checker()
            .named_param_reorder
            .get(&reorder_key)
            .cloned();
        let label_order = if ce.args.iter().any(|a| a.label.is_some()) {
            match self.label_order_for_user_call(
                ce,
                &param_names,
                real_param_count,
                ordered_args.len(),
                is_ufcs,
            ) {
                Ok(order) => Some(order),
                Err(e) => {
                    if checker_order.is_some() {
                        None
                    } else {
                        return Err(e);
                    }
                }
            }
        } else {
            None
        };
        if let Some(order) = label_order.or(checker_order) {
            if order.len() != real_param_count {
                return Err(BuildError::UnsupportedExpression(
                    "CallExpression/reorder-shape-mismatch",
                ));
            }
            // Owned argument temporaries to release after the call (RC, plan
            // 113 R2) — mirrors the positional path below. User calls borrow
            // their params; if a result aliases an arg, returning a borrowed
            // arg retains it and storing a borrowed arg in a field/array retains
            // it. That gives the result its own credit, so the caller must still
            // release the fresh source argument credit after the call.
            let mut owned_arg_stashes: Vec<u32> = Vec::new();
            if type_param_count > 0 {
                let type_args = self
                    .checker()
                    .generic_type_args
                    .get(&ufcs_key)
                    .cloned()
                    .unwrap_or_default();
                for i in 0..type_param_count {
                    let type_name = type_args.get(i).cloned().unwrap_or_default();
                    let (off, len) = self.ctx.strings.borrow_mut().intern(&type_name);
                    self.emit(Instruction::I32Const(off as i32));
                    self.emit(Instruction::I32Const(len as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
                    let t = self.alloc_local();
                    self.emit(Instruction::LocalTee(t));
                    owned_arg_stashes.push(t);
                }
            }
            for (param_idx, slot) in order.iter().enumerate() {
                let arg_expr = if let Some(arg_idx) = slot {
                    let Some(arg) = ordered_args.get(*arg_idx) else {
                        return Err(BuildError::UnsupportedExpression(
                            "CallExpression/reorder-out-of-range",
                        ));
                    };
                    self.compile_expr_as(arg, ValueShape::Boxed)?;
                    Some(*arg)
                } else if let Some(Some(default_expr)) = defaults.get(param_idx + type_param_count)
                {
                    self.compile_expr_as(default_expr, ValueShape::Boxed)?;
                    Some(default_expr)
                } else {
                    return Err(BuildError::UnsupportedExpression(
                        "CallExpression/reorder-with-default",
                    ));
                };
                if let Some(e) = arg_expr {
                    if self.expr_transfers_ownership(e) {
                        let t = self.alloc_local();
                        self.emit(Instruction::LocalTee(t));
                        owned_arg_stashes.push(t);
                    }
                }
            }
            let wasm_idx = self.rt().base + RT_COUNT + proto_idx;
            self.emit(Instruction::Call(wasm_idx));
            self.emit_post_call_propagation(&owned_arg_stashes);
            for t in owned_arg_stashes {
                self.emit(Instruction::LocalGet(t));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
            return Ok(());
        }

        // Positional call: hidden generic `@type` args are injected
        // first, then supplied source args fill the leading real-param
        // slots. Any trailing real-param slots with defaults fall
        // through to their default expression.
        if ordered_args.len() + type_param_count > expected {
            return Err(BuildError::UnsupportedExpression(
                "CallExpression/arg-count-mismatch",
            ));
        }
        // Owned argument temporaries to release after the call (RC, plan 113
        // R2): a literal / fresh-call / fresh-builtin argument is created here
        // and only BORROWED by the callee (params own nothing), so it leaks
        // unless freed once the call returns. `LocalTee` keeps each on the stack
        // for the call while saving a copy to release. A borrowed arg
        // (identifier / field — incl. `mutable` bindings) is owned by the
        // caller's binding, so it is left alone. Aliasing heap results are safe:
        // user-function returns retain borrowed return values, and any store of
        // a borrowed param into an owning field/array retains before returning.
        let mut owned_arg_stashes: Vec<u32> = Vec::new();
        if type_param_count > 0 {
            let type_args = self
                .checker()
                .generic_type_args
                .get(&ufcs_key)
                .cloned()
                .unwrap_or_default();
            for i in 0..type_param_count {
                let type_name = type_args.get(i).cloned().unwrap_or_default();
                let (off, len) = self.ctx.strings.borrow_mut().intern(&type_name);
                self.emit(Instruction::I32Const(off as i32));
                self.emit(Instruction::I32Const(len as i32));
                self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
                // Freshly interned type-name string — always an owned temp.
                let t = self.alloc_local();
                self.emit(Instruction::LocalTee(t));
                owned_arg_stashes.push(t);
            }
        }
        for i in 0..real_param_count {
            let arg_expr = if let Some(a) = ordered_args.get(i) {
                self.compile_expr_as(a, ValueShape::Boxed)?;
                Some(*a)
            } else if let Some(Some(default_expr)) = defaults.get(i + type_param_count) {
                self.compile_expr_as(default_expr, ValueShape::Boxed)?;
                Some(default_expr)
            } else {
                return Err(BuildError::UnsupportedExpression(
                    "CallExpression/arg-count-mismatch",
                ));
            };
            if let Some(e) = arg_expr {
                if self.expr_transfers_ownership(e) {
                    let t = self.alloc_local();
                    self.emit(Instruction::LocalTee(t));
                    owned_arg_stashes.push(t);
                }
            }
        }
        let wasm_idx = self.rt().base + RT_COUNT + proto_idx;
        self.emit(Instruction::Call(wasm_idx));
        self.emit_post_call_propagation(&owned_arg_stashes);
        // Result is on the stack (propagation re-pushed it); release the owned
        // argument temporaries beneath it. On a throw, propagation releases the
        // stashes before branching away.
        for t in owned_arg_stashes {
            self.emit(Instruction::LocalGet(t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(())
    }

    /// Emit one `String` arg onto the stack as `(ptr, len)`. Mirrors
    /// the bytecode translator's pattern: stringify with
    /// `RT_VALUE_TO_STR`, unbox to an i32 address with `RT_OBJ_ADDR`,
    /// then push data pointer (`addr + 8`) and length (`I32Load @ 4`).
    fn emit_string_arg_from_expr(&mut self, e: &Expression) -> Result<(), BuildError> {
        self.emit_string_arg_stashing(e)?;
        Ok(())
    }

    /// If `e` is an OWNED temp, `LocalTee` the value currently on the stack into
    /// a fresh local (leaving it on the stack) and return that local, so a later
    /// `release_stash` can `RT_RELEASE` it after a builtin/host call consumed it.
    /// No-op (returns None) for borrowed args — releasing those would free a
    /// value the caller still owns. Plan 115 arg-temp mop-up.
    fn stash_if_owned(&mut self, e: &Expression) -> Option<u32> {
        if self.expr_transfers_ownership(e) {
            let t = self.alloc_local();
            self.emit(Instruction::LocalTee(t));
            Some(t)
        } else {
            None
        }
    }

    /// Release a stash produced by `stash_if_owned`. Stack-neutral, so any value
    /// already on the stack (e.g. the call's result) is preserved.
    fn release_stash(&mut self, stash: Option<u32>) {
        if let Some(t) = stash {
            self.emit_ownership_event_for_local(OwnershipOp::Discard, OWNERSHIP_SITE_UNKNOWN, t, 0);
            self.emit(Instruction::LocalGet(t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
    }

    fn pass_argument(
        &mut self,
        e: &Expression,
        convention: ArgConvention,
    ) -> Result<Option<u32>, BuildError> {
        let result = self.compile_expr_result_as(e, ValueShape::Boxed)?;
        let mut release_after_call = None;
        let aux = OwnershipAux::HostArgument.encode(convention.id() as u16);
        match convention {
            ArgConvention::Borrowed
            | ArgConvention::RetainedByCallee
            | ArgConvention::CopiedByHost => {
                if result.ownership == ExprOwnership::Owned {
                    let t = self.alloc_local();
                    self.emit(Instruction::LocalTee(t));
                    release_after_call = Some(t);
                }
            }
            ArgConvention::Consumed => match result.ownership {
                ExprOwnership::Borrowed => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Retain,
                        OWNERSHIP_SITE_UNKNOWN,
                        aux,
                    );
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                ExprOwnership::Owned => {
                    self.emit_ownership_event_for_stack(
                        OwnershipOp::Transfer,
                        OWNERSHIP_SITE_UNKNOWN,
                        aux,
                    );
                }
                ExprOwnership::Primitive => {}
            },
        }
        self.emit_ownership_event_for_stack(OwnershipOp::CallArgument, OWNERSHIP_SITE_UNKNOWN, aux);
        Ok(release_after_call)
    }

    /// Like `emit_string_arg_from_expr`, but when the arg is an OWNED temp
    /// (`expr_transfers_ownership`), stash the coerced string value and return
    /// its local so the caller can `RT_RELEASE` it after the host call.
    ///
    /// Releasing an owned string-arg temp is safe for ANY host import: a string
    /// arg is handed over as `(ptr, len)`, and the host copies the bytes into its
    /// own buffer — it never keeps the guest string object. (The host-retain
    /// hazard that blocks blanket arg release is `Boxed`-only: there the host
    /// receives the object handle and may stash it.) Plan 115 arg-temp mop-up.
    fn emit_string_arg_stashing(&mut self, e: &Expression) -> Result<Option<u32>, BuildError> {
        // String literal: its bytes are already interned in the data
        // section, so push (ptr, len) straight from there instead of
        // allocating a String object, stringifying it (identity), reading
        // its data pointer, and releasing it afterward. The consumer
        // (e.g. RT_GET_FIELD's key, json.parse's input) only reads the
        // (ptr, len) as a borrowed slice, so nothing is allocated or
        // released. Hot in literal-keyed dict access — `getString(d,
        // 'name')` / `getInt(props, 'padding')` run ~20×/node in render.
        if let Expression::StringExpression(s) = e {
            let (off, len) = self.ctx.strings.borrow_mut().intern(&s.value);
            self.emit(Instruction::I32Const(off as i32));
            self.emit(Instruction::I32Const(len as i32));
            return Ok(None);
        }
        self.compile_expr_as(e, ValueShape::Boxed)?;
        self.emit(Instruction::Call(self.rt().base + RT_VALUE_TO_STR));
        let stash = if self.expr_transfers_ownership(e) {
            let t = self.alloc_local();
            self.emit(Instruction::LocalTee(t));
            Some(t)
        } else {
            None
        };
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        let scratch = self.alloc_i32_local();
        self.emit(Instruction::LocalTee(scratch));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(scratch));
        self.emit(Instruction::I32Load(mem_off(4)));
        Ok(stash)
    }

    /// Emit one `Int` arg: compile the NaN-boxed value, then
    /// `I32WrapI64` — the low 32 bits of an Int NaN-box is the raw
    /// value, so wrap drops the tag bits cleanly.
    fn emit_int_arg_from_expr(&mut self, e: &Expression) -> Result<(), BuildError> {
        self.compile_expr(e)?;
        self.emit(Instruction::I32WrapI64);
        Ok(())
    }

    /// Wrap a std-module call in a spy-check: record the args,
    /// and if the test framework has registered a mock for this
    /// `fn_id`, use the mocked return value in place of the real
    /// call. On cache miss the real module call runs as normal.
    ///
    /// Args are evaluated into locals once for the spy record,
    /// then the else branch re-evaluates them via the normal
    /// `compile_module_call` path. For forai-idiomatic mocks
    /// (arguments are almost always literals or simple variable
    /// reads) double-evaluation is benign; tests that need truly
    /// side-effectful args should pre-bind to a local.
    fn compile_mocked_module_call(
        &mut self,
        fn_id: u32,
        call: &ModuleCall,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        // Stash each arg in a local so the subsequent buffer
        // allocation doesn't clobber String pointers mid-build.
        let mut arg_locals: Vec<u32> = Vec::with_capacity(call_args.len());
        let mut arg_owned: Vec<bool> = Vec::with_capacity(call_args.len());
        for a in call_args {
            let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            arg_locals.push(local);
            arg_owned.push(result.ownership == ExprOwnership::Owned);
        }

        let arity = call_args.len() as u32;
        let buf = self.alloc_i32_local();
        self.emit(Instruction::I32Const((arity.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(buf));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem0()));
        for (i, &local) in arg_locals.iter().enumerate() {
            self.emit(Instruction::LocalGet(buf));
            self.emit(Instruction::LocalGet(local));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        let out_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(out_ptr));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem0()));

        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const(arity as i32));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit_import_call(crate::runtime::IMPORT_SPY_CHECK_CALL);
        let mocked = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(mocked));
        for (&local, owned) in arg_locals.iter().zip(arg_owned.iter()) {
            if *owned {
                self.emit(Instruction::LocalGet(local));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
        }

        // i32 on stack: 1 = mocked (use *out_ptr), 0 = run real call.
        self.emit(Instruction::LocalGet(mocked));
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        let mocked_value = self.alloc_local();
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::LocalSet(mocked_value));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const((arity.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(mocked_value));
        self.emit(Instruction::Else);
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const((arity.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        // Real path — compile_module_call re-evaluates args.
        self.compile_module_call(call, call_args)?;
        self.emit_close();
        Ok(())
    }

    /// Lower a `(module, method, args)` call recorded in the
    /// dispatch table. Dispatches on the `ModuleCall` variant so
    /// "flat" imports share one codepath and each special shape
    /// lives in its own method.
    fn compile_module_call(
        &mut self,
        call: &ModuleCall,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        match call {
            ModuleCall::Simple {
                import_idx,
                args,
                result,
            } => self.compile_simple_module_call(*import_idx, args, *result, call_args),
            ModuleCall::HttpRequest {
                import_idx,
                has_body,
            } => self.compile_http_request_call(*import_idx, *has_body, call_args),
            ModuleCall::TimeUnix => self.compile_time_unix(call_args),
            ModuleCall::MathUnaryFloatToInt(op) => {
                self.compile_math_unary_float_to_int(op.clone(), call_args)
            }
            ModuleCall::MathUnaryFloat(op) => self.compile_math_unary_float(op.clone(), call_args),
            ModuleCall::MathBinaryFloat(op) => {
                self.compile_math_binary_float(op.clone(), call_args)
            }
            ModuleCall::MathPow => self.compile_math_pow(call_args),
            ModuleCall::CliReadLine => self.compile_cli_read_line(call_args),
            ModuleCall::ConvertToInt => self.compile_convert_to_int_call(call_args),
            ModuleCall::ConvertToFloat => self.compile_convert_to_float_call(call_args),
            ModuleCall::ConvertToString => self.compile_convert_to_string(call_args),
            ModuleCall::ConvertParseInt => self.compile_convert_parse(RT_PARSE_INT, call_args),
            ModuleCall::ConvertParseFloat => self.compile_convert_parse(RT_PARSE_FLOAT, call_args),
            ModuleCall::JsonRequireString => self.compile_json_require_string(call_args),
            ModuleCall::ConvertToBool => self.compile_convert_to_bool(call_args),
            ModuleCall::SpyAssertCalledWith => self.compile_spy_assert_called_with(call_args),
            ModuleCall::SpyAssertCallCount => self.compile_spy_assert_call_count(call_args),
            ModuleCall::SpyAssertNotCalled => self.compile_spy_assert_not_called(call_args),
            ModuleCall::NativeMethod { method_id, arity } => {
                self.compile_native_method(*method_id, *arity, call_args)
            }
            ModuleCall::Assertion(kind) => self.compile_assertion(*kind, call_args),
            ModuleCall::ErrorConstruct => self.compile_error_construct(call_args),
            ModuleCall::Unwrap => self.compile_unwrap(call_args),
        }
    }

    /// `std.error.Error(msg) -> Error`. Allocates a 24-byte dict
    /// with one entry `{"message": msg}`. Layout:
    /// `[tag:i32=3][count:i32=1][key:i64][value:i64]`. The key is a
    /// NaN-boxed String built via `RT_ALLOC_STRING` from the
    /// interned `"message"` bytes. Downstream `e.message` access
    /// routes through the standard dict `RT_GET_FIELD` path.
    ///
    /// Mirrors `translate.rs`'s `name == "Error" && arg_count == 1`
    /// branch.
    fn compile_error_construct(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/Error-arg-count",
            ));
        }
        // Evaluate msg first — it may itself allocate (e.g., String
        // literal via `RT_ALLOC_STRING`), so park it before the
        // parent dict allocation bumps the heap.
        self.compile_expr(&call_args[0].value)?;
        let msg_local = self.alloc_local();
        self.emit(Instruction::LocalSet(msg_local));

        // Intern the "message" key string in the shared pool.
        let (key_off, key_len) = self.ctx.strings.borrow_mut().intern("message");

        let dict_addr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(24));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(dict_addr));

        // tag = 3 (Dict)
        self.emit(Instruction::LocalGet(dict_addr));
        self.emit(Instruction::I32Const(OBJ_TAG_DICT));
        self.emit(Instruction::I32Store(mem0()));
        // count = 1
        self.emit(Instruction::LocalGet(dict_addr));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Store(mem_off(4)));
        // key at offset 8: NaN-boxed String "message"
        self.emit(Instruction::LocalGet(dict_addr));
        self.emit(Instruction::I32Const(key_off as i32));
        self.emit(Instruction::I32Const(key_len as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
        self.emit(Instruction::I64Store(mem_off(8)));
        // value at offset 16: msg — the dict co-owns it (RC, plan 113 R1).
        self.emit(Instruction::LocalGet(dict_addr));
        self.emit(Instruction::LocalGet(msg_local));
        if !self.expr_transfers_ownership(&call_args[0].value) {
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        }
        self.emit(Instruction::I64Store(mem_off(16)));

        // Box as object.
        self.emit(Instruction::LocalGet(dict_addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// `std.error.unwrap(value, fallback) -> value if non-null,
    /// else fallback`. Both args are evaluated first, then the
    /// value's NaN-box is compared to `VAL_NULL`; on match the
    /// fallback is pushed, otherwise the value. Mirrors
    /// `translate.rs`'s `name == "unwrap"` branch.
    fn compile_unwrap(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 2 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/unwrap-arg-count",
            ));
        }
        // Park both args in locals so the `If(Result(I64))` branches
        // can each push the appropriate one onto the stack.
        self.compile_expr(&call_args[0].value)?;
        let val = self.alloc_local();
        self.emit(Instruction::LocalSet(val));
        self.compile_expr(&call_args[1].value)?;
        let fallback = self.alloc_local();
        self.emit(Instruction::LocalSet(fallback));

        // RC (plan 115): unwrap returns ONE arg and discards the other. Make the
        // result uniformly `+1` (so `call_returns_owned` can mark unwrap owned and
        // callers transfer instead of over-retaining — `propOr` does
        // `unwrap(getString(...), '')` per node), and release the discarded arg if
        // it was an owned temp (e.g. the `''` fallback literal). Retain the
        // returned arg if it's borrowed; release the other if it's owned.
        let val_owned = self.expr_transfers_ownership(&call_args[0].value);
        let fb_owned = self.expr_transfers_ownership(&call_args[1].value);
        let rel = self.rt().base + RT_RELEASE;
        let ret = self.rt().base + RT_RETAIN;
        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Eq);
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        // → return fallback (val is null here, a primitive — nothing to release).
        if !fb_owned {
            self.emit(Instruction::LocalGet(fallback));
            self.emit(Instruction::Call(ret));
            self.emit(Instruction::Drop);
        }
        self.emit(Instruction::LocalGet(fallback));
        self.emit(Instruction::Else);
        // → return val; discard fallback (release it if it was an owned temp).
        if fb_owned {
            self.emit(Instruction::LocalGet(fallback));
            self.emit(Instruction::Call(rel));
        }
        if !val_owned {
            self.emit(Instruction::LocalGet(val));
            self.emit(Instruction::Call(ret));
            self.emit(Instruction::Drop);
        }
        self.emit(Instruction::LocalGet(val));
        self.emit_close();
        Ok(())
    }

    fn compile_json_require_string(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 2 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/json-require-string-arg-count",
            ));
        }

        let dict_result = self.compile_expr_result_as(&call_args[0].value, ValueShape::Boxed)?;
        let dict = self.alloc_local();
        self.emit(Instruction::LocalSet(dict));

        self.emit(Instruction::LocalGet(dict));
        let key_stash = self.emit_string_arg_stashing(&call_args[1].value)?;
        self.emit_import_call(IMPORT_JSON_REQUIRE_STRING);

        let result = self.alloc_local();
        self.emit(Instruction::LocalSet(result));
        self.release_stash(key_stash);

        // json_require_string returns an alias into the dict. Retain that alias
        // before releasing an owned inline dict temp so callers can treat this
        // std call as returning an owned optional string.
        self.emit(Instruction::LocalGet(result));
        self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        self.emit(Instruction::Drop);

        if dict_result.ownership == ExprOwnership::Owned {
            self.emit(Instruction::LocalGet(dict));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }

        self.emit(Instruction::LocalGet(result));
        Ok(())
    }

    /// Lower an assertion: `test.assert`, `test.equal`,
    /// `assert.equals`, `assert.isTrue`, `assert.isFalse`. Each
    /// evaluates a condition and produces an i32 `ok` flag (1 = pass,
    /// 0 = fail). On success we leave `VAL_TRUE` on the stack; on
    /// failure we push the caller's message (or `(0, 0)` as a
    /// sentinel the host substitutes with a default) to
    /// `IMPORT_SET_TRAP_MSG`, then emit `unreachable` — the test
    /// runner catches the trap and reads the message.
    ///
    /// Truthiness matches the runtime's VM: `null`, `void`, and
    /// `false` are falsy; everything else (including `0`, `""`,
    /// empty arrays) is truthy. The guest evaluates that by checking
    /// `val != VAL_NULL && val != VAL_VOID && val != VAL_FALSE`.
    fn compile_assertion(
        &mut self,
        kind: AssertionKind,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        let (cond_arg_count, msg_arg_idx) = match kind {
            AssertionKind::Truthy
            | AssertionKind::IsTrue
            | AssertionKind::IsFalse
            | AssertionKind::IsNull
            | AssertionKind::IsNotNull => (1, 1),
            AssertionKind::StringEq => (2, 2),
        };
        if call_args.len() < cond_arg_count || call_args.len() > cond_arg_count + 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/assertion-arg-count",
            ));
        }

        // Evaluate the pass condition onto the stack as an i32.
        match kind {
            AssertionKind::Truthy | AssertionKind::IsTrue | AssertionKind::IsFalse => {
                self.compile_truthy_i32(&call_args[0].value)?;
                if matches!(kind, AssertionKind::IsFalse) {
                    self.emit(Instruction::I32Eqz);
                }
            }
            AssertionKind::IsNull | AssertionKind::IsNotNull => {
                // pass iff `val == VAL_NULL` (isNull) or `val != VAL_NULL`
                // (isNotNull). Both leave an i32 0/1 on the stack.
                self.compile_expr_as(&call_args[0].value, ValueShape::Boxed)?;
                self.emit(Instruction::I64Const(VAL_NULL));
                self.emit(Instruction::I64Eq);
                if matches!(kind, AssertionKind::IsNotNull) {
                    self.emit(Instruction::I32Eqz);
                }
            }
            AssertionKind::StringEq => {
                // stringify both, pass (ptr, len) pairs to RT_STR_EQ.
                let left_stash = self.emit_string_arg_stashing(&call_args[0].value)?;
                let right_stash = self.emit_string_arg_stashing(&call_args[1].value)?;
                self.emit(Instruction::Call(self.rt().base + RT_STR_EQ));
                let ok = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(ok));
                self.release_stash(left_stash);
                self.release_stash(right_stash);
                self.emit(Instruction::LocalGet(ok));
            }
        }

        // Now the stack has an i32 `ok` flag. Branch on it.
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        // Pass path: return VAL_TRUE so the expression has an i64
        // value like any other call.
        self.emit(Instruction::I64Const(QNAN | TAG_BOOL | 1));
        self.emit(Instruction::Else);
        // Fail path: stringify message (or push empty sentinel),
        // trap.
        if call_args.len() > msg_arg_idx {
            self.emit_string_arg_from_expr(&call_args[msg_arg_idx].value)?;
        } else {
            // No message — the host fills in a default.
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::I32Const(0));
        }
        self.emit_import_call(IMPORT_SET_TRAP_MSG);
        self.emit(Instruction::Unreachable);
        self.emit_close();
        Ok(())
    }

    /// Dispatch a string/array method through the runtime's
    /// `RT_CALL_NATIVE(obj, args_ptr, arg_count)` helper.
    ///
    /// Steps, ordered to avoid linear-memory aliasing between the
    /// NativeFn allocation and the args buffer:
    /// 1. Compile each arg expression, parking the NaN-boxed result
    ///    in a fresh local. We can't write to memory yet because
    ///    `compile_expr` may itself bump `__heap_ptr` (e.g., for
    ///    String literals via `RT_ALLOC_STRING`).
    /// 2. Allocate one block sized to fit both the NativeFn header
    ///    AND the args buffer in one shot: `8 + arity * 8` bytes. A
    ///    single `RT_ALLOC` call ensures the args region is part of
    ///    the same committed allocation, which is the load-bearing
    ///    detail — splitting it into "alloc 8 for the header, then
    ///    write args past it" traps when `__heap_ptr` lands close
    ///    enough to the linear-memory boundary that the 8-byte
    ///    header fits without growing memory but the args writes
    ///    spill past `mem_size`.
    /// 3. Stamp the NativeFn header at the block's start, then
    ///    `args_base = nfn_addr + 8`. Both regions are inside the
    ///    block we just allocated, so writes are guaranteed to land
    ///    in committed memory.
    /// 4. Write each arg local into `args_base + i*8`.
    /// 5. Push (NativeFn boxed, args_base, arg_count); call
    ///    `RT_CALL_NATIVE`. Its return lands on the stack as the
    ///    expression's value. RT_CALL_NATIVE reads the args into
    ///    locals at dispatch entry, so subsequent allocations
    ///    overwriting the buffer are safe.
    fn compile_native_method(
        &mut self,
        method_id: i32,
        arity: usize,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != arity {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/native-method-arg-count",
            ));
        }

        // Compile args into locals (keeps values safe across the
        // NativeFn allocation).
        let mut arg_locals: Vec<u32> = Vec::with_capacity(arity);
        for a in call_args {
            self.compile_expr(&a.value)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            arg_locals.push(local);
        }

        // Allocate the NativeFn header AND the args buffer in one
        // `RT_ALLOC` call so the underlying `memory.grow` covers both
        // regions. Splitting this into two — alloc 8 for the header,
        // then write args past it — traps when `__heap_ptr` lands
        // close enough to the memory boundary that the 8-byte alloc
        // fits without growing but the args writes don't.
        let nfn_addr = self.alloc_i32_local();
        let block_size = 8 + (arity as i32) * 8;
        self.emit(Instruction::I32Const(block_size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(nfn_addr));
        self.emit(Instruction::LocalGet(nfn_addr));
        self.emit(Instruction::I32Const(OBJ_TAG_NATIVE_FN));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(nfn_addr));
        self.emit(Instruction::I32Const(method_id));
        self.emit(Instruction::I32Store(mem_off(4)));

        // args_base = nfn_addr + 8 — sits inside the same allocated
        // block, so writes are guaranteed to land in committed memory.
        let args_base = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(nfn_addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(args_base));

        // Write args to memory.
        for (i, &local) in arg_locals.iter().enumerate() {
            self.emit(Instruction::LocalGet(args_base));
            self.emit(Instruction::LocalGet(local));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        // Push RT_CALL_NATIVE args: obj (NaN-boxed NativeFn),
        // args_ptr, arg_count.
        self.emit(Instruction::LocalGet(nfn_addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        self.emit(Instruction::LocalGet(args_base));
        self.emit(Instruction::I32Const(arity as i32));
        self.emit(Instruction::Call(self.rt().base + RT_CALL_NATIVE));
        // Free the transient NativeFn wrapper + args buffer (RC, plan 113 R2).
        // It is a per-call dispatch shim that nothing else references and that
        // RT_RELEASE doesn't reclaim (no OBJ_TAG_NATIVE_FN branch), so without
        // this every array/string/dict method call leaked one block. The result
        // is on the stack and is an independent object (methods copy out of the
        // args buffer, never alias it), so freeing the block is safe.
        self.emit(Instruction::LocalGet(nfn_addr));
        self.emit(Instruction::I32Const(block_size));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        // Release owned argument temporaries (RC; plan 115 arg-temp mop-up). A
        // literal or fresh-call argument is created here and only borrowed by the
        // callee, so it leaks unless freed after the call.
        //
        // Arg 0 (receiver) is safe to release when the result either cannot
        // share a heap pointer with it, or the builder retains every shared
        // child before returning. This includes primitive readers, fresh string
        // transforms, and array/dict rebuilders like slice/getKeys that retain
        // copied elements. Still skipped: element accessors (first/last), where
        // the result is the receiver's child alias.
        let result_cannot_alias_receiver = matches!(
            method_id,
            METHOD_LENGTH
                | METHOD_IS_EMPTY
                | METHOD_CONTAINS
                | METHOD_INDEX_OF
                | METHOD_STARTS_WITH
                | METHOD_ENDS_WITH
                | METHOD_GET_KEYS
                | METHOD_REPLACE
                | METHOD_JOIN
                | METHOD_SPLIT
                | METHOD_TRIM
                | METHOD_TRIM_START
                | METHOD_TRIM_END
                | METHOD_TO_UPPER
                | METHOD_TO_LOWER
                | METHOD_SUBSTRING
                | METHOD_REPEAT
                | METHOD_APPEND
                | METHOD_SORT
                | METHOD_SLICE
                | METHOD_REVERSE
        );
        for (i, a) in call_args.iter().enumerate() {
            if i == 0 && !result_cannot_alias_receiver {
                continue;
            }
            if self.expr_transfers_ownership(&a.value) {
                self.emit(Instruction::LocalGet(arg_locals[i]));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
        }
        Ok(())
    }

    /// Emit one numeric arg (Int or Float) unboxed to f64 on the
    /// stack. Uses `RT_AS_NUMBER` which handles both Int and Float
    /// NaN-box patterns — matches the bytecode runtime's pattern for
    /// math methods.
    fn emit_number_arg(&mut self, e: &Expression) -> Result<(), BuildError> {
        self.compile_expr_as(e, ValueShape::Boxed)?;
        self.emit(Instruction::Call(self.rt().base + RT_AS_NUMBER));
        Ok(())
    }

    fn compile_simple_module_call(
        &mut self,
        import_idx: u32,
        arg_shapes: &[ArgShape],
        result: ResultShape,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if arg_shapes.len() != call_args.len() {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/arg-count-mismatch",
            ));
        }
        // Plan 119 U2: a boxed host return without an ownership signature
        // is a bug — error under checked builds (or FAI_ABI_CHECK), a
        // once-per-import stderr sentinel otherwise. Classification reads
        // the same table, so an unsigned import would otherwise silently
        // fall through as borrowed and over-retain forever.
        let import_ownership =
            fai_compiler::ownership_abi::lookup_host_import(import_name(import_idx));
        if matches!(result, ResultShape::Boxed) {
            let name = import_name(import_idx);
            if import_ownership.is_none() {
                if crate::runtime::checked_enabled() || abi_check_enabled() {
                    return Err(BuildError::MissingOwnershipSignature(name.to_string()));
                }
                report_missing_signature_once(name);
            }
        }
        // Owned STRING arg temps are released after the call (plan 115 arg-temp
        // mop-up). This is safe for every import: a string arg is passed as
        // (ptr,len) and the host copies the bytes, so it never keeps the guest
        // string object. BOXED arg temps are still NOT released here — some host
        // imports STASH the object reference host-side without a guest retain
        // (`events.on` keeps the listener closure, http route handlers keep their
        // handler, storage retains values), so freeing one would be a UAF.
        // Reclaiming owned Boxed arg temps needs per-import "does it retain?"
        // metadata — left as a follow-up; those leak (soundly) for now.
        let arg_conventions = import_ownership.as_ref().and_then(|sig| sig.args.as_ref());
        let mut stashes: Vec<u32> = Vec::new();
        for (i, (shape, arg)) in arg_shapes.iter().zip(call_args.iter()).enumerate() {
            match shape {
                ArgShape::String => {
                    if let Some(t) = self.emit_string_arg_stashing(&arg.value)? {
                        stashes.push(t);
                    }
                }
                ArgShape::Int => self.emit_int_arg_from_expr(&arg.value)?,
                ArgShape::Boxed => {
                    if let Some(convention) = arg_conventions.and_then(|args| args.get(i)).copied()
                    {
                        if let Some(t) = self.pass_argument(&arg.value, convention)? {
                            stashes.push(t);
                        }
                    } else {
                        self.compile_expr(&arg.value)?;
                    }
                }
            }
        }
        self.emit_import_call(import_idx);
        // Release owned string-arg temps. The result/return is produced by the
        // import (independent of the guest string bytes), so this can't free it.
        for t in &stashes {
            self.release_stash(Some(*t));
        }
        match result {
            ResultShape::Boxed => {}
            ResultShape::MakeInt => {
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
            }
            ResultShape::MakeBool => {
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
            }
            ResultShape::MakeFloat => {
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_FLOAT));
            }
            ResultShape::Void => {
                self.emit(Instruction::I64Const(VAL_VOID));
            }
        }
        if import_signals_errors(import_idx) {
            self.emit_post_call_propagation(&[]);
        }
        Ok(())
    }

    fn compile_http_request_call(
        &mut self,
        import_idx: u32,
        has_body: bool,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        let min_args = if has_body { 2 } else { 1 };
        let max_args = min_args + 1;
        if call_args.len() < min_args || call_args.len() > max_args {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/http-request-arg-count",
            ));
        }
        self.emit_string_arg_from_expr(&call_args[0].value)?;
        if has_body {
            self.emit_string_arg_from_expr(&call_args[1].value)?;
        }
        if call_args.len() == max_args {
            self.compile_expr(&call_args[min_args].value)?;
        } else {
            self.emit(Instruction::I64Const(VAL_NULL));
        }
        self.emit_import_call(import_idx);
        Ok(())
    }

    /// `std.time.unix() -> Int`. `IMPORT_NOW_MS` returns f64 ms;
    /// we divide by 1000.0, truncate to a signed i32 (saturate not
    /// needed — the range fits), and box as Int. Mirrors
    /// `runtime.rs::METHOD_TIME_UNIX`.
    fn compile_time_unix(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if !call_args.is_empty() {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/time.unix-takes-no-args",
            ));
        }
        self.emit_import_call(IMPORT_NOW_MS);
        self.emit(Instruction::F64Const(1000.0));
        self.emit(Instruction::F64Div);
        self.emit(Instruction::I32TruncF64S);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
        Ok(())
    }

    /// `std.math.{floor,ceil,round}(x: Float) -> Int`. Unbox,
    /// apply `op`, saturate-truncate to i32, box as Int.
    fn compile_math_unary_float_to_int(
        &mut self,
        op: Instruction<'static>,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/math-unary-arg-count",
            ));
        }
        self.emit_number_arg(&call_args[0].value)?;
        self.emit(op);
        self.emit(Instruction::I32TruncSatF64S);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
        Ok(())
    }

    /// `std.math.{abs,sqrt}(x: Float) -> Float`.
    fn compile_math_unary_float(
        &mut self,
        op: Instruction<'static>,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/math-unary-arg-count",
            ));
        }
        self.emit_number_arg(&call_args[0].value)?;
        self.emit(op);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_FLOAT));
        Ok(())
    }

    /// `std.math.{min,max}(a: Float, b: Float) -> Float`.
    fn compile_math_binary_float(
        &mut self,
        op: Instruction<'static>,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 2 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/math-binary-arg-count",
            ));
        }
        self.emit_number_arg(&call_args[0].value)?;
        self.emit_number_arg(&call_args[1].value)?;
        self.emit(op);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_FLOAT));
        Ok(())
    }

    /// `std.math.pow(base: Float, exp: Float) -> Float` with
    /// integer exponent. Mirrors `runtime.rs::METHOD_POW` using
    /// structured control flow: compute `base^|exp|` via an
    /// iterative multiply loop, then invert on negative exponent.
    /// wasm has no native `f64.pow`, so we're stuck with the
    /// iterative form.
    fn compile_math_pow(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 2 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/math.pow-arg-count",
            ));
        }

        // f64 locals for result + base. We allocate as i64 and
        // reinterpret across loop boundaries — matches the bytecode
        // helper's approach without needing an f64-local allocator.
        let result_bits = self.alloc_local();
        let base_bits = self.alloc_local();
        let n = self.alloc_i32_local();
        let invert = self.alloc_i32_local();

        // result = 1.0
        self.emit(Instruction::F64Const(1.0));
        self.emit(Instruction::I64ReinterpretF64);
        self.emit(Instruction::LocalSet(result_bits));

        // base = AS_NUMBER(arg0)
        self.emit_number_arg(&call_args[0].value)?;
        self.emit(Instruction::I64ReinterpretF64);
        self.emit(Instruction::LocalSet(base_bits));

        // n = i32_trunc(AS_NUMBER(arg1))
        self.emit_number_arg(&call_args[1].value)?;
        self.emit(Instruction::I32TruncF64S);
        self.emit(Instruction::LocalSet(n));

        // invert = 0; if n < 0: invert = 1; n = -n
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(invert));
        self.emit(Instruction::LocalGet(n));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::I32LtS);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::LocalSet(invert));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalGet(n));
        self.emit(Instruction::I32Sub);
        self.emit(Instruction::LocalSet(n));
        self.emit_close();

        // while n > 0: result *= base; n--
        self.emit_open(Instruction::Block(BlockType::Empty));
        self.emit_open(Instruction::Loop(BlockType::Empty));
        self.emit(Instruction::LocalGet(n));
        self.emit(Instruction::I32Eqz);
        self.emit(Instruction::BrIf(1));
        self.emit(Instruction::LocalGet(result_bits));
        self.emit(Instruction::F64ReinterpretI64);
        self.emit(Instruction::LocalGet(base_bits));
        self.emit(Instruction::F64ReinterpretI64);
        self.emit(Instruction::F64Mul);
        self.emit(Instruction::I64ReinterpretF64);
        self.emit(Instruction::LocalSet(result_bits));
        self.emit(Instruction::LocalGet(n));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Sub);
        self.emit(Instruction::LocalSet(n));
        self.emit(Instruction::Br(0));
        self.emit_close(); // loop
        self.emit_close(); // block

        // if invert: result = 1.0 / result
        self.emit(Instruction::LocalGet(invert));
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::F64Const(1.0));
        self.emit(Instruction::LocalGet(result_bits));
        self.emit(Instruction::F64ReinterpretI64);
        self.emit(Instruction::F64Div);
        self.emit(Instruction::I64ReinterpretF64);
        self.emit(Instruction::LocalSet(result_bits));
        self.emit_close();

        // NaN-box the result.
        self.emit(Instruction::LocalGet(result_bits));
        self.emit(Instruction::F64ReinterpretI64);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_FLOAT));
        Ok(())
    }

    /// `std.cli.readLine(prompt?) -> String`. Optional arg — zero
    /// args pushes `(0, 0)`, one arg stringifies + pushes
    /// `(ptr, len)`. Result is a NaN-boxed String from the host.
    fn compile_cli_read_line(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        match call_args.len() {
            0 => {
                self.emit(Instruction::I32Const(0));
                self.emit(Instruction::I32Const(0));
            }
            1 => self.emit_string_arg_from_expr(&call_args[0].value)?,
            _ => {
                return Err(BuildError::UnsupportedExpression(
                    "ModuleCall/cli.readLine-arity",
                ))
            }
        }
        self.emit_import_call(IMPORT_CLI_READ_LINE);
        Ok(())
    }

    /// `std.convert.toInt(v) -> Int`. Module-call dispatch wrapper.
    fn compile_convert_to_int_call(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/convert.toInt-arg-count",
            ));
        }
        self.compile_convert_to_int(&call_args[0].value)
    }

    /// `std.convert.toFloat(v) -> Float`. Module-call dispatch wrapper.
    fn compile_convert_to_float_call(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/convert.toFloat-arg-count",
            ));
        }
        self.compile_convert_to_float(&call_args[0].value)
    }

    /// Type-aware `toInt(v)`. Dispatches on the static type the
    /// checker inferred for `v`:
    /// - `Int`  → passthrough.
    /// - `Float` → truncate to i32 (saturating) and re-box as Int.
    /// - `String` → parse via `RT_PARSE_INT`. Returns null on parse
    ///   failure (matches `convert.parseInt` semantics).
    /// - other (`Unknown`, etc.) → passthrough as a best effort. The
    ///   checker normally narrows the type before we get here; a true
    ///   `Unknown` falling through is a legacy passthrough match.
    fn compile_convert_to_int(&mut self, arg: &Expression) -> Result<(), BuildError> {
        let arg_ty = self.expression_type_at(arg).cloned();
        match arg_ty {
            Some(fai_checker::types::Type::Float) => {
                self.compile_expr_as(arg, ValueShape::RawFloat)?;
                self.emit(Instruction::I32TruncSatF64S);
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
            }
            Some(fai_checker::types::Type::String) => {
                self.compile_expr(arg)?;
                self.emit(Instruction::Call(self.rt().base + RT_PARSE_INT));
            }
            _ => {
                self.compile_expr(arg)?;
            }
        }
        Ok(())
    }

    /// Type-aware `toFloat(v)`. Dispatches on the static type:
    /// - `Float` → passthrough (already raw f64 bits).
    /// - `Int`   → unbox, convert i64→f64, reinterpret as Float bits.
    /// - `String` → parse via `RT_PARSE_FLOAT`.
    /// - other (`Unknown`, etc.) → dynamically coerce boxed Int/Float
    ///   values through `rt_as_number`, then emit canonical Float bits.
    fn compile_convert_to_float(&mut self, arg: &Expression) -> Result<(), BuildError> {
        let arg_ty = self.expression_type_at(arg).cloned();
        match arg_ty {
            Some(fai_checker::types::Type::Int) => {
                self.compile_expr_as(arg, ValueShape::RawInt)?;
                self.emit(Instruction::F64ConvertI64S);
                self.emit(Instruction::I64ReinterpretF64);
            }
            Some(fai_checker::types::Type::String) => {
                self.compile_expr(arg)?;
                self.emit(Instruction::Call(self.rt().base + RT_PARSE_FLOAT));
            }
            _ => {
                self.compile_expr(arg)?;
                self.emit(Instruction::Call(self.rt().base + RT_AS_NUMBER));
                self.emit(Instruction::I64ReinterpretF64);
            }
        }
        Ok(())
    }

    /// `std.convert.toString(v) -> String`. One-liner via the
    /// existing `RT_VALUE_TO_STR` helper.
    fn compile_convert_to_string(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/convert.toString-arg-count",
            ));
        }
        self.emit_to_string_owned(&call_args[0].value)
    }

    /// No-op stub for checker-registered Void-returning builtins
    /// `std.convert.toBool(v) -> Bool`. Pushes the boxed value,
    /// runs the same truthy test `if`/`while` use, then boxes the
    /// resulting i32 as a Bool. Matches forai's convention: `null`,
    /// `void`, and literal `false` are falsy; Int 0 and empty
    /// string are truthy.
    fn compile_convert_to_bool(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/convert.toBool-arg-count",
            ));
        }
        self.compile_truthy_i32(&call_args[0].value)?;
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    /// `TypeName(field: value, ...)` — user-defined type constructor.
    /// The checker guarantees every required, non-defaulted, non-
    /// optional field is supplied. Here we synthesize a
    /// `DictionaryExpression` in field-declaration order, preferring
    /// the caller's labeled arg and falling back to the field's
    /// default expression, then `null` for fields that are neither
    /// supplied nor defaulted nor required (e.g. optional types).
    ///
    /// The resulting dict is compiled via the existing dict-literal
    /// path, so field access (`todo.id`) works uniformly against
    /// both literal dicts and typed records.
    /// Expand `let name Type = from_dict(dict)` at compile time.
    ///
    /// Emits a dict Object directly (not via compile_type_constructor)
    /// so each field can carry per-entry null-fallback logic:
    ///   - `field.attributes["omit"]` → skip dict lookup, use the
    ///     field's default (or `null` for optional types).
    ///   - `field.attributes["alias"]` → read `dict.<alias>` instead
    ///     of `dict.<field.name>`.
    ///   - otherwise pull `dict.<field.name>`. If the lookup returns
    ///     `null` AND the field has a default, fall back to the
    ///     default. Required fields whose default is absent flow null
    ///     through untouched.
    ///
    /// The dict read path is `RT_GET_FIELD`.
    /// Materialize `from_dict(dict_expr)` for `type_name`, leaving the boxed
    /// owned record (+1) on the stack. Releases the source dict when it was an
    /// owned temp. Shared by the sync let-binding path and the async resume-fn
    /// segment compiler — they differ only in where the value is stored (a
    /// scope-bound local vs. an async frame slot), not in how it is built.
    fn compile_from_dict_value(
        &mut self,
        type_name: &str,
        dict_expr: &Expression,
    ) -> Result<(), BuildError> {
        self.compile_expr_as(dict_expr, ValueShape::Boxed)?;
        let dict_local = self.alloc_local();
        self.emit(Instruction::LocalSet(dict_local));
        self.compile_from_dict_local_value(type_name, dict_local)?;
        // An OWNED source temp (e.g. `from_dict(json.parse(s))`) is consumed
        // by the materialization — the record retained every field it kept,
        // so the source's ref can go. A borrowed source (`from_dict(e.data)`)
        // stays the caller's. Releasing `dict_local` leaves the record on the
        // stack untouched.
        if self.expr_transfers_ownership(dict_expr) {
            self.emit(Instruction::LocalGet(dict_local));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(())
    }

    fn compile_from_dict_binding(
        &mut self,
        binding_name: &str,
        type_name: &str,
        dict_expr: Expression,
    ) -> Result<(), BuildError> {
        self.compile_from_dict_value(type_name, &dict_expr)?;
        let local = self.alloc_local();
        self.emit(Instruction::LocalSet(local));
        self.bind(binding_name, local);
        // The materialized record is a fresh owned dict — release at scope
        // exit like any `let` (this binding path bypasses `compile_bindings`'
        // tail, which is where note_droppable normally happens; without it
        // every `let x T = from_dict(d)` leaked the record — one per request
        // in brain's beforeRequest listener, plan 116).
        self.note_droppable(local);
        Ok(())
    }

    fn compile_from_dict_local_value(
        &mut self,
        type_name: &str,
        dict_local: u32,
    ) -> Result<(), BuildError> {
        use fai_compiler::ast::{Expression as AstExpr, NullExpression, SourceLocation};

        let fields = self
            .ctx
            .type_fields
            .get(type_name)
            .cloned()
            .ok_or(BuildError::UnsupportedExpression("from_dict-unknown-type"))?;

        // Evaluate each declared field's Value into a local first —
        // same ordering trick compile_dict_literal uses so nested
        // allocations don't clobber the outer dict's payload region.
        let loc = SourceLocation { line: 0, column: 0 };
        let mut val_locals: Vec<u32> = Vec::with_capacity(fields.len());
        for field in &fields {
            let omit = field.attributes.iter().any(|a| a.key == "omit");
            let alias = field
                .attributes
                .iter()
                .find(|a| a.key == "alias")
                .and_then(|a| a.string_value.clone());
            let null_default: AstExpr = AstExpr::NullExpression(NullExpression {
                location: loc.clone(),
            });

            if omit {
                // Use default (or null for optional fields).
                let default_expr = field.default_value.clone().unwrap_or(null_default);
                self.compile_expr_as(&default_expr, ValueShape::Boxed)?;
            } else {
                // Load dict.<source_key>, unbox null, fall back to
                // default when present.
                let source_key = alias.as_deref().unwrap_or(&field.name);
                let (key_off, key_len) = self.ctx.strings.borrow_mut().intern(source_key);
                self.emit(Instruction::LocalGet(dict_local));
                self.emit(Instruction::I32Const(key_off as i32));
                self.emit(Instruction::I32Const(key_len as i32));
                self.emit(Instruction::Call(self.rt().base + RT_GET_FIELD));

                let has_default = field.default_value.is_some();
                if has_default {
                    // `if val == VAL_NULL then default else val end`
                    let tmp = self.alloc_local();
                    self.emit(Instruction::LocalTee(tmp));
                    self.emit(Instruction::I64Const(VAL_NULL));
                    self.emit(Instruction::I64Eq);
                    self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
                    let default_expr = field.default_value.clone().unwrap();
                    self.compile_expr_as(&default_expr, ValueShape::Boxed)?;
                    self.emit(Instruction::Else);
                    self.emit(Instruction::LocalGet(tmp));
                    self.emit_close();
                }
                // Required-no-default fields: leave the possibly-null
                // value on the stack. Downstream code gets `null`
                // where the source dict was silent.
            }

            let value_local = self.alloc_local();
            self.emit(Instruction::LocalSet(value_local));
            val_locals.push(value_local);
        }

        // Intern field names.
        let keys: Vec<(u32, u32)> = fields
            .iter()
            .map(|f| self.ctx.strings.borrow_mut().intern(&f.name))
            .collect();

        // Allocate the dict Object. Matches compile_dict_literal's
        // layout: [tag:i32=OBJ_TAG_DICT][count:i32][entries...],
        // each entry = [key:i64 (boxed String)][value:i64] (16 bytes).
        let count = fields.len() as i32;
        let capacity = if count < 16 { 16 } else { count + 8 };
        let size = 8 + capacity * 16;
        let addr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(addr));

        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(OBJ_TAG_DICT));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(count));
        self.emit(Instruction::I32Store(mem_off(4)));

        for (i, (&(key_off, key_len), &val_local)) in keys.iter().zip(val_locals.iter()).enumerate()
        {
            let base = 8 + (i as u64) * 16;
            // Key.
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::I32Const(key_off as i32));
            self.emit(Instruction::I32Const(key_len as i32));
            self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
            self.emit(Instruction::I64Store(mem_off(base)));
            // Value — the new struct dict co-owns it (RC, plan 113 R1). Values
            // here are borrowed field reads (`RT_GET_FIELD` into the source
            // dict) or default-expr temps, so always retain: the source still
            // owns the field reads, and a fresh default temp over-retaining only
            // leaks by one (never a UAF). Precise per-field classification can
            // come later.
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::LocalGet(val_local));
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            self.emit(Instruction::I64Store(mem_off(base + 8)));
        }

        // Box as an Object and leave it on the stack.
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    fn compile_query_typed_binding(
        &mut self,
        binding_name: &str,
        type_name: &str,
        ce: &fai_compiler::ast::CallExpression,
    ) -> Result<(), BuildError> {
        // `query_typed(db, sql, params)` is currently a compile-time
        // specialization over the concrete LHS element type. Lower the
        // data fetch to the existing `query_params` package function,
        // then materialize a typed record for each returned row.
        let proto_idx = self
            .function_by_name
            .get("query_params")
            .copied()
            .or_else(|| {
                self.ctx
                    .named_imports
                    .get("query_params")
                    .and_then(|qualified| self.function_by_name.get(qualified).copied())
            })
            .or_else(|| {
                self.ctx
                    .named_imports
                    .get("query_typed")
                    .and_then(|qualified| qualified.rsplit_once('.'))
                    .map(|(module, _)| format!("{}.query_params", module))
                    .and_then(|qualified| self.function_by_name.get(&qualified).copied())
            })
            .or_else(|| {
                // Last-resort fallback: scan all known functions for a
                // `<module>.query_params` entry. Covers `prepare_module_
                // directory_for_tests` where forsqlite tests itself: the
                // synthetic module name (`__module__`) doesn't appear in
                // `named_imports`, but `function_by_name` does carry the
                // qualified entry. Picking the first match is fine since
                // a project linking two `query_params`-defining modules
                // would already be ambiguous at the use site.
                self.function_by_name.iter().find_map(|(name, idx)| {
                    if name.ends_with(".query_params") {
                        Some(*idx)
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| BuildError::UnknownIdentifier("query_params".to_string()))?;
        let expected = self.functions()[proto_idx as usize].param_count as usize;
        let type_param_count = self.functions()[proto_idx as usize].type_param_count as usize;
        if type_param_count != 0 || ce.args.len() > expected {
            return Err(BuildError::UnsupportedExpression(
                "query_typed/query_params-shape",
            ));
        }
        let defaults = self.functions()[proto_idx as usize].param_defaults.clone();
        for i in 0..expected {
            if let Some(arg) = ce.args.get(i) {
                self.compile_expr_as(&arg.value, ValueShape::Boxed)?;
            } else if let Some(Some(default_expr)) = defaults.get(i) {
                self.compile_expr_as(default_expr, ValueShape::Boxed)?;
            } else {
                return Err(BuildError::UnsupportedExpression(
                    "query_typed/query_params-arg-count",
                ));
            }
        }
        let wasm_idx = self.rt().base + RT_COUNT + proto_idx;
        self.emit(Instruction::Call(wasm_idx));
        self.emit_post_call_propagation(&[]);
        let rows_local = self.alloc_local();
        self.emit(Instruction::LocalSet(rows_local));

        let rows_addr = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(rows_local));
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::LocalSet(rows_addr));

        let count_local = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(rows_addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::LocalSet(count_local));

        let arr_addr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::LocalGet(count_local));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Mul);
        self.emit(Instruction::I32Add);
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(arr_addr));

        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(OBJ_TAG_ARRAY));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::LocalGet(count_local));
        self.emit(Instruction::I32Store(mem_off(4)));

        let i_local = self.alloc_i32_local();
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(i_local));

        self.emit_open(Instruction::Block(BlockType::Empty));
        self.emit_open(Instruction::Loop(BlockType::Empty));

        self.emit(Instruction::LocalGet(i_local));
        self.emit(Instruction::LocalGet(count_local));
        self.emit(Instruction::I32GeU);
        self.emit(Instruction::BrIf(1));

        self.emit(Instruction::LocalGet(rows_local));
        self.emit(Instruction::LocalGet(i_local));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
        self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
        let row_local = self.alloc_local();
        self.emit(Instruction::LocalSet(row_local));

        self.compile_from_dict_local_value(type_name, row_local)?;
        let item_local = self.alloc_local();
        self.emit(Instruction::LocalSet(item_local));

        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(i_local));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Mul);
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(item_local));
        self.emit(Instruction::I64Store(mem0()));

        self.emit(Instruction::LocalGet(i_local));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(i_local));
        self.emit(Instruction::Br(0));
        self.emit_close();
        self.emit_close();

        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        let local = self.alloc_local();
        self.emit(Instruction::LocalSet(local));
        self.bind(binding_name, local);
        // The raw rows array (query_params' owned +1 result) was consumed by
        // the materialization above — every field a typed record kept was
        // retained, so the source rows can go. Without this every
        // `query_typed` leaked the row dicts per query (plan 116).
        self.emit(Instruction::LocalGet(rows_local));
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        // The typed array is a fresh owned binding — release at scope exit
        // (this path bypasses `compile_bindings`' note_droppable tail).
        self.note_droppable(local);
        Ok(())
    }

    fn compile_type_constructor(
        &mut self,
        type_name: &str,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        use fai_compiler::ast::{
            DictionaryEntry, DictionaryExpression, Expression as AstExpr, NullExpression,
            SourceLocation,
        };
        let fields =
            self.ctx.type_fields.get(type_name).ok_or_else(|| {
                BuildError::UnsupportedExpression("type-constructor-unknown-type")
            })?;

        let mut supplied: HashMap<&str, &Expression> = HashMap::new();
        for a in call_args {
            let Some(label) = a.label.as_deref() else {
                return Err(BuildError::UnsupportedExpression(
                    "type-constructor-positional-arg",
                ));
            };
            supplied.insert(label, &a.value);
        }

        let loc = SourceLocation { line: 0, column: 0 };
        let mut entries: Vec<DictionaryEntry> = Vec::with_capacity(fields.len());
        for field in fields {
            let value: Expression = if let Some(e) = supplied.get(field.name.as_str()) {
                (*e).clone()
            } else if let Some(def) = &field.default_value {
                def.clone()
            } else {
                // Unsupplied, no default — the checker let this through
                // only when the field type is optional, so the right
                // placeholder is `null`.
                AstExpr::NullExpression(NullExpression {
                    location: loc.clone(),
                })
            };
            entries.push(DictionaryEntry {
                key: field.name.clone(),
                value,
                location: field.location.clone(),
            });
        }

        let dict_expr = DictionaryExpression {
            entries,
            location: loc,
        };
        self.compile_dict_literal(&dict_expr)
    }

    /// Array literal `[e0, e1, ..., en]` — allocates a heap Array
    /// object with layout `[tag:i32=1][count:i32][items:i64 each]`
    /// and boxes the result as an object. Matches
    /// `translate.rs::Op::NewArray`.
    ///
    /// Items are compiled into locals first so that element
    /// expressions which themselves allocate (nested arrays, String
    /// literals) don't clobber the parent array's payload region
    /// before we write each element.
    fn compile_array_literal(&mut self, items: &[Expression]) -> Result<(), BuildError> {
        let mut item_locals: Vec<u32> = Vec::with_capacity(items.len());
        for it in items {
            self.compile_expr_as(it, ValueShape::Boxed)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            item_locals.push(local);
        }

        let count = items.len() as i32;
        let size = 8 + count * 8;
        let arr_addr = self.alloc_i32_local();

        self.emit(Instruction::I32Const(size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(arr_addr));

        // tag
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(OBJ_TAG_ARRAY));
        self.emit(Instruction::I32Store(mem0()));
        // count
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(count));
        self.emit(Instruction::I32Store(mem_off(4)));
        // items — the array co-owns each (RC, plan 113 R1): retain a borrowed
        // element; a fresh value transfers its single ref.
        for (i, (it, &local)) in items.iter().zip(item_locals.iter()).enumerate() {
            self.emit(Instruction::LocalGet(arr_addr));
            self.emit(Instruction::LocalGet(local));
            if !self.expr_transfers_ownership(it) {
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            self.emit(Instruction::I64Store(mem_off(8 + (i as u64) * 8)));
        }

        // NaN-box as object.
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// Tuple literal `#(e0, e1, ...)`. Same layout as an Array
    /// except the heap tag is `OBJ_TAG_TUPLE`. Used for multi-value
    /// returns and type-constructor tuples. Mirrors
    /// `translate.rs::Op::NewTuple`.
    fn compile_tuple_literal(&mut self, items: &[Expression]) -> Result<(), BuildError> {
        let mut item_locals: Vec<u32> = Vec::with_capacity(items.len());
        for it in items {
            self.compile_expr_as(it, ValueShape::Boxed)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            item_locals.push(local);
        }

        let count = items.len() as i32;
        let size = 8 + count * 8;
        let addr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(addr));

        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(OBJ_TAG_TUPLE));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(count));
        self.emit(Instruction::I32Store(mem_off(4)));
        // The tuple co-owns each element (RC, plan 113 R1).
        for (i, (it, &local)) in items.iter().zip(item_locals.iter()).enumerate() {
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::LocalGet(local));
            if !self.expr_transfers_ownership(it) {
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            self.emit(Instruction::I64Store(mem_off(8 + (i as u64) * 8)));
        }
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// Dictionary literal `{k1: v1, k2: v2, ...}`. Layout:
    /// `[tag=3][count][(key_str, val_i64)*]` where keys are
    /// NaN-boxed Strings interned from the source key names.
    /// Over-allocates capacity so downstream `set()` can append
    /// entries in place — matches `translate.rs::Op::NewDict`.
    ///
    /// Key strings are interned via the shared `StringInterner` so
    /// they sit in the data section and the runtime's dict lookup
    /// sees stable byte pointers.
    fn compile_dict_literal(
        &mut self,
        d: &fai_compiler::ast::DictionaryExpression,
    ) -> Result<(), BuildError> {
        // Evaluate all values into locals first — values may
        // allocate (String literals, nested dicts) and we want the
        // dict allocation to sit above them in the heap.
        let mut val_locals: Vec<u32> = Vec::with_capacity(d.entries.len());
        for e in &d.entries {
            self.compile_expr_as(&e.value, ValueShape::Boxed)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            val_locals.push(local);
        }
        // Intern each key into the data section.
        let keys: Vec<(u32, u32)> = d
            .entries
            .iter()
            .map(|e| self.ctx.strings.borrow_mut().intern(&e.key))
            .collect();

        let count = d.entries.len() as i32;
        // Capacity matches translate.rs: at least 16 slots or
        // count+8, whichever is bigger — leaves room for `set()` to
        // append new keys.
        let capacity = if count < 16 { 16 } else { count + 8 };
        let size = 8 + capacity * 16;
        let addr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(addr));

        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(OBJ_TAG_DICT));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(count));
        self.emit(Instruction::I32Store(mem_off(4)));

        for (i, (&(key_off, key_len), &val_local)) in keys.iter().zip(val_locals.iter()).enumerate()
        {
            let base = 8 + (i as u64) * 16;
            // Key at base+0: RT_ALLOC_STRING(ptr, len) → boxed String.
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::I32Const(key_off as i32));
            self.emit(Instruction::I32Const(key_len as i32));
            self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
            self.emit(Instruction::I64Store(mem_off(base)));
            // Value at base+8 — the dict co-owns it (RC, plan 113 R1).
            self.emit(Instruction::LocalGet(addr));
            self.emit(Instruction::LocalGet(val_local));
            if !self.expr_transfers_ownership(&d.entries[i].value) {
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            self.emit(Instruction::I64Store(mem_off(base + 8)));
        }

        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// Index expression `obj[i]`. Works for arrays and dicts (and
    /// strings — RT_GET_INDEX branches on the container tag). Both
    /// args are NaN-boxed; the result is the stored value or
    /// `VAL_NULL` on miss/out-of-bounds.
    fn compile_index_expr(
        &mut self,
        ix: &fai_compiler::ast::IndexExpression,
    ) -> Result<(), BuildError> {
        // Fast path: a statically-Array receiver indexed by a
        // statically-Int index. Inline the element fetch instead of
        // calling the fully-polymorphic `rt_get_index` (which resolves
        // the object address via a nested call, dispatches on the
        // container tag for array/tuple/string/dict/instance/module,
        // and takes a *boxed* index it immediately unboxes). The inline
        // form skips the index boxing, the tag dispatch, and the call,
        // and exactly replicates `rt_get_index`'s array branch: negative
        // indices wrap by length, and an out-of-range index yields
        // VAL_NULL. The element is read straight from the slot (still a
        // borrowed value, same ownership as before), so RC is unchanged.
        if self.index_is_array_int(ix) {
            return self.compile_array_index_fast(ix);
        }
        self.compile_expr(&ix.object)?;
        self.compile_expr(&ix.index)?;
        self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
        Ok(())
    }

    /// Eligibility for the inline array-index read fast path: the
    /// receiver's checker type is a (non-optional) `Array` and the index
    /// is a statically-Int expression. Conservative — anything the
    /// checker didn't prove an array/int falls back to `rt_get_index`.
    fn index_is_array_int(&self, ix: &fai_compiler::ast::IndexExpression) -> bool {
        // The checker recorded this IndexExpression as a proven
        // Array-receiver / Int-index site (membership encodes both
        // facts), keyed by the IndexExpression's own location.
        self.checker().array_int_index_sites.contains(&(
            self.module_key.clone(),
            ix.location.line,
            ix.location.column,
        ))
    }

    /// Inline `arr[i]` element read for a proven Array/Int pair. Mirrors
    /// the array branch of `rt_get_index` exactly:
    ///   addr = obj & 0x0000_FFFF_FFFF_FFFF   (inlined rt_obj_addr)
    ///   len  = mem[addr+4]
    ///   if i < 0 { i += len }                (negative-index wrap)
    ///   if i < 0 || i >= len { VAL_NULL }     (bounds → null)
    ///   else { mem[addr + 8 + i*8] }
    fn compile_array_index_fast(
        &mut self,
        ix: &fai_compiler::ast::IndexExpression,
    ) -> Result<(), BuildError> {
        // addr = obj_addr(object)  — inline the mask + wrap.
        self.compile_expr_as(&ix.object, ValueShape::Boxed)?;
        self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
        self.emit(Instruction::I64And);
        self.emit(Instruction::I32WrapI64);
        let addr = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(addr));
        // i = index as i32 (raw — no NaN-box round trip).
        self.compile_expr_as(&ix.index, ValueShape::RawInt)?;
        self.emit(Instruction::I32WrapI64);
        let i = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(i));
        // Negative-index wrap: if i < 0 { i += len }.
        self.emit(Instruction::LocalGet(i));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::I32LtS);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(i));
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(i));
        self.emit_close();
        // Bounds check → VAL_NULL on out-of-range, else the slot value.
        self.emit(Instruction::LocalGet(i));
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::I32LtS);
        self.emit(Instruction::LocalGet(i));
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::I32GeS);
        self.emit(Instruction::I32Or);
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::Else);
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(i));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Mul);
        self.emit(Instruction::I32Add);
        self.emit(Instruction::I64Load(mem0()));
        self.emit_close();
        Ok(())
    }

    /// Field access `obj.prop`. Compiles the object, then calls
    /// `RT_GET_FIELD(obj, key_ptr, key_len)` with an interned key.
    /// Works on dicts, instances, error objects, and even strings
    /// (the runtime handles tag-based dispatch). Returns
    /// `VAL_NULL` if the key is absent.
    /// Inline a record field read when the checker proved `me`'s
    /// receiver is a user-defined record type (recorded in
    /// `record_field_read_sites`). Records are dict-backed and the
    /// constructor lays fields out in declaration order, so field at
    /// declaration index `slot` has its value at `addr + 16 + slot*16`
    /// (dict header 8 + slot*16 for the entry, +8 past the key box).
    /// Reading the slot directly skips the string-keyed `rt_get_field`
    /// scan and the call. The stored value is returned borrowed —
    /// identical ownership to `rt_get_field`. Returns true if it emitted
    /// the fast path, false to fall back to the generic path.
    fn try_compile_record_field_read(
        &mut self,
        me: &fai_compiler::ast::MemberExpression,
    ) -> Result<bool, BuildError> {
        let key = (
            self.module_key.clone(),
            me.location.line,
            me.location.column,
            fai_checker::checker::member_chain_depth(me),
        );
        let Some(type_name) = self.checker().record_field_read_sites.get(&key).cloned() else {
            return Ok(false);
        };
        // Declaration-order field index = dict slot index. Looked up from
        // the same ordered table the constructor uses to lay out the dict.
        let slot = match self.ctx.type_fields.get(&type_name) {
            Some(fields) => match fields.iter().position(|f| f.name == me.property) {
                Some(s) => s,
                None => return Ok(false),
            },
            None => return Ok(false),
        };
        // addr = obj_addr(object)  — inline the NaN-box mask + wrap.
        self.compile_expr_as(&me.object, ValueShape::Boxed)?;
        self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
        self.emit(Instruction::I64And);
        self.emit(Instruction::I32WrapI64);
        // value = mem[addr + 16 + slot*16]
        let value_off = 16 + (slot as u64) * 16;
        self.emit(Instruction::I64Load(mem_off(value_off)));
        Ok(true)
    }

    fn compile_field_access(&mut self, obj: &Expression, field: &str) -> Result<(), BuildError> {
        let (key_off, key_len) = self.ctx.strings.borrow_mut().intern(field);
        self.compile_expr(obj)?;
        self.emit(Instruction::I32Const(key_off as i32));
        self.emit(Instruction::I32Const(key_len as i32));
        self.emit(Instruction::Call(self.rt().base + RT_GET_FIELD));
        Ok(())
    }

    /// Template string `"before {expr} after"`. Each `Text` part
    /// is a literal String; each `Expression` part is stringified
    /// via `RT_VALUE_TO_STR`. Concatenation proceeds left-to-right
    /// into one allocation. An empty template evaluates to the
    /// empty String.
    fn compile_template_string(
        &mut self,
        parts: &[fai_compiler::ast::TemplateStringPart],
    ) -> Result<(), BuildError> {
        use fai_compiler::ast::TemplateStringPart;

        // Helper: emit one owned (+1) boxed String on the stack.
        // Text parts are fresh allocations; expression parts go through
        // `emit_to_string_owned` so string aliases are retained and owned
        // expression temps are released.
        let emit_part = |this: &mut Self, part: &TemplateStringPart| -> Result<(), BuildError> {
            match part {
                TemplateStringPart::Text { value } => {
                    let (off, len) = this.ctx.strings.borrow_mut().intern(value);
                    this.emit(Instruction::I32Const(off as i32));
                    this.emit(Instruction::I32Const(len as i32));
                    this.emit(Instruction::Call(this.rt().base + RT_ALLOC_STRING));
                }
                TemplateStringPart::Expression { expression } => {
                    this.emit_to_string_owned(expression)?;
                }
            }
            Ok(())
        };

        if parts.is_empty() {
            let (off, len) = self.ctx.strings.borrow_mut().intern("");
            self.emit(Instruction::I32Const(off as i32));
            self.emit(Instruction::I32Const(len as i32));
            self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
            return Ok(());
        }

        // Single part: no concatenation needed — emit it directly. A
        // Text part is one alloc; an Expr part is its own owned string.
        if parts.len() == 1 {
            return emit_part(self, &parts[0]);
        }

        // Multi-part: build the result in ONE allocation instead of a
        // left-fold of `rt_concat`s (each of which allocated an
        // intermediate string and re-copied the growing prefix —
        // O(parts²) bytes). Materialize every part as a (data_ptr, len)
        // pair, sum the lengths, allocate the result once, then
        // `memory.copy` each part into place. Text parts copy straight
        // from the interned data section (no String object allocated);
        // Expr parts are stringified into owned temps that are released
        // after the copy. Hot in SSR / template-heavy rendering.
        //
        // `ptr`/`len` for each part are held in i32 locals; the owned
        // Expr temps are tracked so they can be released once copied.
        let mut part_ptrs: Vec<u32> = Vec::with_capacity(parts.len());
        let mut part_lens: Vec<u32> = Vec::with_capacity(parts.len());
        let mut owned_temps: Vec<u32> = Vec::new();
        for part in parts {
            let ptr_l = self.alloc_i32_local();
            let len_l = self.alloc_i32_local();
            match part {
                TemplateStringPart::Text { value } => {
                    let (off, len) = self.ctx.strings.borrow_mut().intern(value);
                    self.emit(Instruction::I32Const(off as i32));
                    self.emit(Instruction::LocalSet(ptr_l));
                    self.emit(Instruction::I32Const(len as i32));
                    self.emit(Instruction::LocalSet(len_l));
                }
                TemplateStringPart::Expression { expression } => {
                    // Owned boxed String; stash it, then derive its data
                    // ptr (addr+8) and length (mem[addr+4]).
                    self.emit_to_string_owned(expression)?;
                    let s = self.alloc_local();
                    self.emit(Instruction::LocalTee(s));
                    owned_temps.push(s);
                    // addr = obj_addr(s) inline (mask + wrap)
                    self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
                    self.emit(Instruction::I64And);
                    self.emit(Instruction::I32WrapI64);
                    let addr = self.alloc_i32_local();
                    self.emit(Instruction::LocalTee(addr));
                    self.emit(Instruction::I32Const(8));
                    self.emit(Instruction::I32Add);
                    self.emit(Instruction::LocalSet(ptr_l));
                    self.emit(Instruction::LocalGet(addr));
                    self.emit(Instruction::I32Load(mem_off(4)));
                    self.emit(Instruction::LocalSet(len_l));
                }
            }
            part_ptrs.push(ptr_l);
            part_lens.push(len_l);
        }

        // total_len = sum of part lengths.
        let total_len = self.alloc_i32_local();
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(total_len));
        for &len_l in &part_lens {
            self.emit(Instruction::LocalGet(total_len));
            self.emit(Instruction::LocalGet(len_l));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalSet(total_len));
        }

        // result = alloc(8 + total_len); tag = STRING; len = total_len.
        let result = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::LocalGet(total_len));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(result));
        self.emit(Instruction::LocalGet(result));
        self.emit(Instruction::I32Const(OBJ_TAG_STRING));
        self.emit(Instruction::I32Store(mem0()));
        self.emit(Instruction::LocalGet(result));
        self.emit(Instruction::LocalGet(total_len));
        self.emit(Instruction::I32Store(mem_off(4)));

        // Copy each part into result+8, advancing a running offset.
        let cursor = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(result));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(cursor));
        for (&ptr_l, &len_l) in part_ptrs.iter().zip(part_lens.iter()) {
            // memory.copy(dst = cursor, src = ptr_l, len = len_l)
            self.emit(Instruction::LocalGet(cursor));
            self.emit(Instruction::LocalGet(ptr_l));
            self.emit(Instruction::LocalGet(len_l));
            self.emit(Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            // cursor += len_l
            self.emit(Instruction::LocalGet(cursor));
            self.emit(Instruction::LocalGet(len_l));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalSet(cursor));
        }

        // Release the owned Expr-part temps now that their bytes are
        // copied. (Note: derive ptrs BEFORE this — done above.)
        for &s in &owned_temps {
            self.emit(Instruction::LocalGet(s));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }

        // Box the result as an object.
        self.emit(Instruction::LocalGet(result));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// `x?` — optional check. Returns `true` when `x` isn't
    /// `null`, `false` otherwise. The NaN-box's sentinel
    /// comparison is direct i64 equality.
    fn compile_optional_check(&mut self, inner: &Expression) -> Result<(), BuildError> {
        self.compile_expr(inner)?;
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Ne);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    /// `x!` — force-unwrap. On `null`, report a "force-unwrap of
    /// null" trap reason and trap via `unreachable`; any other value
    /// flows through unchanged. Callers wanting a recoverable path
    /// should use `unwrap(x, fallback)` instead.
    fn compile_force_unwrap(&mut self, inner: &Expression) -> Result<(), BuildError> {
        self.compile_expr(inner)?;
        let tmp = self.alloc_local();
        self.emit(Instruction::LocalSet(tmp));
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Eq);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::I32Const(
            crate::runtime::TRAP_FORCE_UNWRAP_NULL,
        ));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Const(0));
        self.emit_import_call(IMPORT_TRAP_REPORT);
        self.emit(Instruction::Unreachable);
        self.emit_close();
        self.emit(Instruction::LocalGet(tmp));
        Ok(())
    }

    /// `std.convert.parseInt(s)` / `parseFloat(s)`. Both delegate to
    /// an RT helper that takes a NaN-boxed String and returns the
    /// parsed value (Int/Float) or `VAL_NULL`.
    fn compile_convert_parse(
        &mut self,
        helper: u32,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "ModuleCall/convert.parse-arg-count",
            ));
        }
        self.compile_expr(&call_args[0].value)?;
        let stash = self.stash_if_owned(&call_args[0].value);
        self.emit(Instruction::Call(self.rt().base + helper));
        self.release_stash(stash);
        Ok(())
    }

    /// Bare-global `remoteCall(url, fn, argsJson, hash)` — RPC
    /// boundary. Each arg is a String; the guest passes them as
    /// `(ptr, len)` pairs so the host has 8 i32 params (4 ptrs +
    /// 4 lens) before `IMPORT_REMOTE_CALL` runs. The host parses
    /// the JSON response server-side and returns a NaN-boxed forai
    /// value directly. Mirrors `translate.rs`'s `name == "remoteCall"`
    /// branch.
    fn compile_remote_call(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 4 {
            return Err(BuildError::UnsupportedExpression("remoteCall-arg-count"));
        }
        let mut stashes: Vec<u32> = Vec::new();
        for a in args {
            if let Some(t) = self.emit_string_arg_stashing(a)? {
                stashes.push(t);
            }
        }
        self.emit_import_call(IMPORT_REMOTE_CALL);
        for t in stashes {
            self.release_stash(Some(t));
        }
        Ok(())
    }

    /// Bare-global builtin dispatch. Returns `Some(())` when `name`
    /// matched a known global and was compiled, `None` to let the
    /// caller continue with user-function / extern / closure
    /// resolution. Mirrors the long chain of `if name == "..."`
    /// branches in `translate.rs::emit_call`.
    fn try_compile_bare_global(
        &mut self,
        name: &str,
        args: &[&Expression],
    ) -> Result<Option<()>, BuildError> {
        match name {
            // ── stdout / print ────────────────────────────────
            "print" => self.compile_print(args).map(Some),

            // (R0 clean slate, plan 113: `reclaim`/`markShared` builtins removed.)
            // ── memory: deep-copy a value (returns a fresh owned duplicate) ──
            "copy" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("copy-arg-count"));
                }
                self.compile_expr_as(args[0], ValueShape::Boxed)?;
                self.emit(Instruction::Call(
                    self.rt().base + crate::runtime::RT_COPY_DEEP,
                ));
                Ok(Some(()))
            }
            // ── debug: current heap bump pointer (__heap_ptr, global 0) ──
            "__heapPtr" => {
                if !args.is_empty() {
                    return Err(BuildError::UnsupportedExpression("__heapPtr-arg-count"));
                }
                // global 0 (i32 bytes) → f64 → boxed number (NaN-box = raw f64 bits)
                self.emit(Instruction::GlobalGet(0));
                self.emit(Instruction::F64ConvertI32S);
                self.emit(Instruction::I64ReinterpretF64);
                Ok(Some(()))
            }
            // ── debug: live heap-object counter (plan 115) ──
            "__liveObjects" => {
                if !args.is_empty() {
                    return Err(BuildError::UnsupportedExpression("__liveObjects-arg-count"));
                }
                // The counter's global index varies by module layout, so go
                // through the runtime helper (which baked it in) → boxed Int.
                self.emit(Instruction::Call(self.rt().base + RT_LIVE_OBJECTS));
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                Ok(Some(()))
            }
            // ── refcounting diagnostics / primitives (plan 113) ──
            "__refcount" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("__refcount-arg-count"));
                }
                self.compile_expr_as(args[0], ValueShape::Boxed)?;
                let v = self.alloc_local();
                self.emit(Instruction::LocalSet(v));
                let base = self.rt().base;
                self.emit(Instruction::LocalGet(v));
                self.emit(Instruction::Call(base + RT_IS_OBJ));
                self.emit(Instruction::If(BlockType::Result(ValType::I64)));
                // object → mem[obj_addr(v) - 8] (the rc prefix) as Int
                self.emit(Instruction::LocalGet(v));
                self.emit(Instruction::Call(base + RT_OBJ_ADDR));
                self.emit(Instruction::I32Const(8));
                self.emit(Instruction::I32Sub);
                self.emit(Instruction::I32Load(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
                self.emit(Instruction::Call(base + RT_MAKE_INT));
                self.emit(Instruction::Else);
                // primitive → -1 (not a heap object)
                self.emit(Instruction::I32Const(-1));
                self.emit(Instruction::Call(base + RT_MAKE_INT));
                self.emit(Instruction::End);
                Ok(Some(()))
            }
            "__retain" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("__retain-arg-count"));
                }
                self.compile_expr_as(args[0], ValueShape::Boxed)?;
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                Ok(Some(()))
            }
            "__release" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("__release-arg-count"));
                }
                self.compile_expr_as(args[0], ValueShape::Boxed)?;
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
                self.emit(Instruction::I64Const(VAL_VOID));
                Ok(Some(()))
            }

            // ── error / RPC ──────────────────────────────────
            "remoteCall" => {
                if args.len() != 4 {
                    return Err(BuildError::UnsupportedExpression("remoteCall-arg-count"));
                }
                self.compile_remote_call(args).map(Some)
            }
            "message" | "kind" => self.compile_error_field(name, args).map(Some),
            "isError" => self.compile_is_error(args).map(Some),
            "Error" => self.compile_bare_error_ctor(args).map(Some),
            "unwrap" => self.compile_bare_unwrap(args).map(Some),

            // ── type introspections ──────────────────────────
            "is_int" => self.compile_type_check_rt(args, RT_IS_INT).map(Some),
            "is_float" => self.compile_type_check_rt(args, RT_IS_FLOAT).map(Some),
            "is_null" => self.compile_is_null(args).map(Some),
            "is_bool" => self.compile_is_bool(args).map(Some),
            "is_string" => self.compile_is_tagged_obj(args, OBJ_TAG_STRING).map(Some),
            "is_array" => self.compile_is_tagged_obj(args, OBJ_TAG_ARRAY).map(Some),
            "is_dict" => self.compile_is_tagged_obj(args, OBJ_TAG_DICT).map(Some),

            // ── conversions ──────────────────────────────────
            "toString" => self.compile_to_string_bare(args).map(Some),
            "typeOf" => self.compile_type_of_bare(args).map(Some),
            "toInt" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("toInt-arg-count"));
                }
                self.compile_convert_to_int(args[0])?;
                Ok(Some(()))
            }
            "toFloat" => {
                if args.len() != 1 {
                    return Err(BuildError::UnsupportedExpression("toFloat-arg-count"));
                }
                self.compile_convert_to_float(args[0])?;
                Ok(Some(()))
            }
            "parseInt" => self.compile_parse_bare(args, RT_PARSE_INT).map(Some),
            "parseFloat" => self.compile_parse_bare(args, RT_PARSE_FLOAT).map(Some),

            // ── polymorphic length / isEmpty ──────────────────
            "length" => self.compile_bare_length(args).map(Some),
            "isEmpty" => self.compile_bare_is_empty(args).map(Some),

            // ── JSON shortcuts (bare forms of json.parse/stringify) ──
            "jsonParse" | "parse" => self.compile_bare_json_parse(args).map(Some),
            "jsonStringify" | "stringify" => self.compile_bare_json_stringify(args).map(Some),

            // ── browser HTML / router ─────────────────────────
            "setHtml" => self.compile_bare_set_html(args).map(Some),
            "setHtmlAt" => self.compile_bare_set_html_at(args).map(Some),
            "getLocationPath" => self.compile_bare_get_location_path(args).map(Some),
            "pushHistoryState" => self.compile_bare_push_history_state(args).map(Some),
            "replaceLocation" => self.compile_bare_replace_location(args).map(Some),

            // ── dict helpers ──────────────────────────────────
            "getString" | "getInt" | "getBool" | "get" => {
                self.compile_bare_dict_get(args).map(Some)
            }
            "hasKey" => self.compile_bare_has_key(args).map(Some),
            "set" => self.compile_bare_dict_set(args).map(Some),

            // ── array / dict UFCS-style bare calls ─────────────
            // These are usually written `arr.append(x)` or
            // `d.getKeys()`; the checker accepts the bare form too
            // (see `append` in `builtins/core.rs` and `getKeys` in
            // `builtins/dict.rs`). Route them through the same
            // native-method dispatch the UFCS path uses.
            "append" => self.compile_bare_native(args, METHOD_APPEND, 2).map(Some),
            "getKeys" => self.compile_bare_native(args, METHOD_GET_KEYS, 1).map(Some),

            // ── concurrency ──────────────────────────────────
            "sleep" => self.compile_bare_sleep(args).map(Some),
            "all" => self.compile_bare_all(args).map(Some),

            // ── mock / spy ─────────────────────────────────────
            // `mock` / `mockOnce` resolve their first arg (a
            // function reference) at compile time to a stable
            // `fn_id`; runtime lookups happen via the host spy
            // table. `mockReset` wipes both mocks and call history
            // for a target. If the target can't be resolved (e.g.
            // a pass-through from a higher-order function) we
            // compile it as a no-op — real interception only works
            // on compile-time-known function references, matching
            // how the test-block collector discovers targets.
            "mock" => self.compile_bare_spy_mock(args, false).map(Some),
            "mockOnce" => self.compile_bare_spy_mock(args, true).map(Some),
            "mockReset" => self.compile_bare_spy_reset(args).map(Some),

            _ => Ok(None),
        }
    }

    /// `sleep(ms)` — test-mode / legacy direct path via IMPORT_SLEEP_MS.
    ///
    /// Production routing sends async-effectful programs through
    /// `async_codegen`, where `sleep` lowers to scheduler-owned frame state
    /// plus `host_set_timer`. This direct fallback is reachable only in
    /// test-mode builds (where `try_codegen_async` declines) and old
    /// embedding paths that do not run async analysis; there the host
    /// `sleep_ms` import is a real (blocking) sleep so test assertions on
    /// async functions return correct values.
    /// The host function takes an f64 millisecond count; convert
    /// via RT_AS_NUMBER (handles both Int and Float). Returns
    /// Void (pushed as VAL_VOID).
    fn compile_bare_sleep(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("sleep-arg-count"));
        }
        self.compile_expr(args[0])?;
        self.emit(Instruction::Call(self.rt().base + RT_AS_NUMBER));
        self.emit_import_call(crate::runtime::IMPORT_SLEEP_MS);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `assert.calledWith(target, ...expected)` — serialise the
    /// expected arg values into a scratch buffer and compare
    /// against the host's recorded calls. Traps via
    /// `IMPORT_SET_TRAP_MSG` + `unreachable` on mismatch.
    fn compile_spy_assert_called_with(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.is_empty() {
            return Err(BuildError::UnsupportedExpression(
                "assert.calledWith-arg-count",
            ));
        }
        let target = &call_args[0].value;
        let fn_id = resolve_mock_target(
            target,
            &self.function_by_name,
            self.ctx.module_aliases,
            self.ctx.named_imports,
            self.ctx.std_method_fn_ids,
        );
        // Without a resolvable target we can't check anything; fall
        // through as a no-op (matches `mock` with the same target).
        let Some(fn_id) = fn_id else {
            for a in call_args {
                let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                self.discard_value(result);
            }
            self.emit(Instruction::I64Const(VAL_VOID));
            return Ok(());
        };
        let expected_count = call_args.len() - 1;

        // Stash each expected value in a local so the buffer
        // allocation doesn't clobber it during its own RT_ALLOC.
        let mut expected_locals = Vec::with_capacity(expected_count);
        let mut expected_owned = Vec::with_capacity(expected_count);
        for a in &call_args[1..] {
            let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            expected_locals.push(local);
            expected_owned.push(result.ownership == ExprOwnership::Owned);
        }

        let buf = self.alloc_i32_local();
        self.emit(Instruction::I32Const((expected_count.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(buf));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem0()));
        for (i, &local) in expected_locals.iter().enumerate() {
            self.emit(Instruction::LocalGet(buf));
            self.emit(Instruction::LocalGet(local));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const(expected_count as i32));
        self.emit_import_call(crate::runtime::IMPORT_SPY_ASSERT_CALLED_WITH);
        let assertion_failed = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(assertion_failed));
        for (&local, owned) in expected_locals.iter().zip(expected_owned.iter()) {
            if *owned {
                self.emit(Instruction::LocalGet(local));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
        }
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const((expected_count.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(assertion_failed));
        // i32 result on stack: 0 pass, 1 fail.
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::Unreachable);
        self.emit_close();
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `assert.callCount(target, n)` — compare the recorded count
    /// against `n`. Traps on mismatch.
    fn compile_spy_assert_call_count(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 2 {
            return Err(BuildError::UnsupportedExpression(
                "assert.callCount-arg-count",
            ));
        }
        let fn_id = resolve_mock_target(
            &call_args[0].value,
            &self.function_by_name,
            self.ctx.module_aliases,
            self.ctx.named_imports,
            self.ctx.std_method_fn_ids,
        );
        let Some(fn_id) = fn_id else {
            for a in call_args {
                let result = self.compile_expr_result_as(&a.value, ValueShape::Boxed)?;
                self.discard_value(result);
            }
            self.emit(Instruction::I64Const(VAL_VOID));
            return Ok(());
        };
        // Unbox the Int argument to i32. The checker guarantees
        // this is an Int; low 32 bits carry the value.
        self.emit(Instruction::I32Const(fn_id as i32));
        self.compile_expr(&call_args[1].value)?;
        self.emit(Instruction::I32WrapI64);
        self.emit_import_call(crate::runtime::IMPORT_SPY_ASSERT_CALL_COUNT);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::Unreachable);
        self.emit_close();
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `assert.notCalled(target)` — traps if any call was recorded.
    fn compile_spy_assert_not_called(
        &mut self,
        call_args: &[fai_compiler::ast::CallArgument],
    ) -> Result<(), BuildError> {
        if call_args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "assert.notCalled-arg-count",
            ));
        }
        let fn_id = resolve_mock_target(
            &call_args[0].value,
            &self.function_by_name,
            self.ctx.module_aliases,
            self.ctx.named_imports,
            self.ctx.std_method_fn_ids,
        );
        let Some(fn_id) = fn_id else {
            let result = self.compile_expr_result_as(&call_args[0].value, ValueShape::Boxed)?;
            self.discard_value(result);
            self.emit(Instruction::I64Const(VAL_VOID));
            return Ok(());
        };
        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit_import_call(crate::runtime::IMPORT_SPY_ASSERT_NOT_CALLED);
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::Unreachable);
        self.emit_close();
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// Bare-global no-op for checker-void builtins that don't have
    /// `mock(target, value)` / `mockOnce(target, value)` —
    /// register `value` as the return for `target` in the host
    /// spy table. Target must resolve at compile time (bare
    /// identifier, named-import alias, or `module.method` on a
    /// user module) so we can bake the `fn_id` into the wasm.
    /// An unresolvable target degrades to a no-op — the runtime
    /// has no way to know which function it was aiming at.
    fn compile_bare_spy_mock(
        &mut self,
        args: &[&Expression],
        once: bool,
    ) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("mock-arg-count"));
        }
        match resolve_mock_target(
            args[0],
            &self.function_by_name,
            self.ctx.module_aliases,
            self.ctx.named_imports,
            self.ctx.std_method_fn_ids,
        ) {
            Some(fn_id) => {
                self.emit(Instruction::I32Const(fn_id as i32));
                let value = self.compile_expr_result_as(args[1], ValueShape::Boxed)?;
                let release_after_call = if value.ownership == ExprOwnership::Owned {
                    let local = self.alloc_local();
                    self.emit(Instruction::LocalTee(local));
                    Some(local)
                } else {
                    None
                };
                let import = if once {
                    crate::runtime::IMPORT_SPY_SET_MOCK_ONCE
                } else {
                    crate::runtime::IMPORT_SPY_SET_MOCK
                };
                self.emit_import_call(import);
                self.release_stash(release_after_call);
            }
            None => {
                // Unresolvable target — preserve side effects and
                // emit no host call. The corresponding call sites
                // won't be instrumented either (they weren't in
                // the mocked set), so this is consistent.
                let target = self.compile_expr_result_as(args[0], ValueShape::Boxed)?;
                self.discard_value(target);
                let value = self.compile_expr_result_as(args[1], ValueShape::Boxed)?;
                self.discard_value(value);
            }
        }
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `mockReset(target)` — clear both the mock value and the
    /// accumulated call history for `target`. Same resolution
    /// rules as `mock()`; degrades to no-op on unresolvable
    /// targets.
    fn compile_bare_spy_reset(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("mockReset-arg-count"));
        }
        match resolve_mock_target(
            args[0],
            &self.function_by_name,
            self.ctx.module_aliases,
            self.ctx.named_imports,
            self.ctx.std_method_fn_ids,
        ) {
            Some(fn_id) => {
                self.emit(Instruction::I32Const(fn_id as i32));
                self.emit_import_call(crate::runtime::IMPORT_SPY_RESET);
            }
            None => {
                let result = self.compile_expr_result_as(args[0], ValueShape::Boxed)?;
                self.discard_value(result);
            }
        }
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// Bare-call dispatch for UFCS-style native methods — `append`,
    /// `getKeys`, etc. Wraps the `&Expression` args into
    /// `CallArgument`s and forwards to `compile_native_method`, which
    /// handles NativeFn allocation + args buffer + `RT_CALL_NATIVE`.
    fn compile_bare_native(
        &mut self,
        args: &[&Expression],
        method_id: i32,
        arity: usize,
    ) -> Result<(), BuildError> {
        if args.len() != arity {
            return Err(BuildError::UnsupportedExpression("bare-native-arg-count"));
        }
        use fai_compiler::ast::{CallArgument, SourceLocation};
        let loc = SourceLocation { line: 0, column: 0 };
        let call_args: Vec<CallArgument> = args
            .iter()
            .map(|e| CallArgument {
                label: None,
                value: (*e).clone(),
                location: loc.clone(),
            })
            .collect();
        self.compile_native_method(method_id, arity, &call_args)
    }

    /// Self-reassignment move forms: dispatch to the append or concat
    /// fast path when the RHS is `append(target, x)` / `target + x`.
    /// Returns `Some(result)` with the compiled value on the stack.
    fn try_compile_move_form(
        &mut self,
        target: &str,
        value: &Expression,
    ) -> Result<Option<ExprResult>, BuildError> {
        if let Some(r) = self.try_compile_append_move(target, value)? {
            return Ok(Some(r));
        }
        self.try_compile_concat_move(target, value)
    }

    /// Assignment-position append: `xs = append(xs, x)` or
    /// `xs = array.append(xs, x)` where the receiver is the same binding
    /// being reassigned. The pre-call value is dead after the call, so the
    /// runtime may append in place when the array is uniquely owned
    /// (METHOD_APPEND_MOVE) instead of copying every element — turning
    /// build-a-list-in-a-loop from O(n²) into amortized O(n). General-
    /// position `append` keeps its copy semantics and never routes here.
    ///
    /// Matches the exact dispatch precedence of `compile_call`: the bare
    /// `append` name is a global builtin (user functions don't shadow it),
    /// and the module form requires the receiver identifier to be an
    /// unshadowed alias of `std.array`. UFCS calls are left on the normal
    /// path. Returns `Some(result)` with the call's value on the stack
    /// when the pattern matched, `None` to let the caller compile the RHS
    /// normally.
    fn try_compile_append_move(
        &mut self,
        target: &str,
        value: &Expression,
    ) -> Result<Option<ExprResult>, BuildError> {
        let Expression::CallExpression(ce) = value else {
            return Ok(None);
        };
        if ce.args.len() != 2 || ce.args.iter().any(|a| a.label.is_some()) {
            return Ok(None);
        }
        let Expression::IdentifierExpression(first) = &ce.args[0].value else {
            return Ok(None);
        };
        if first.name != target {
            return Ok(None);
        }
        let ufcs_key = (
            self.module_key.clone(),
            ce.location.line,
            ce.location.column,
        );
        if self.checker().ufcs_calls.contains(&ufcs_key) {
            return Ok(None);
        }
        let is_native_append = match &*ce.callee {
            Expression::IdentifierExpression(id) => id.name == "append",
            Expression::MemberExpression(me) => {
                me.property == "append"
                    && matches!(&*me.object, Expression::IdentifierExpression(obj)
                        if self.resolve(&obj.name).is_none()
                            && self
                                .ctx
                                .module_aliases
                                .get(&obj.name)
                                .map(String::as_str)
                                == Some("std.array"))
            }
            _ => false,
        };
        if !is_native_append {
            return Ok(None);
        }
        let args: Vec<&Expression> = ce.args.iter().map(|a| &a.value).collect();
        self.compile_bare_native(&args, METHOD_APPEND_MOVE, 2)?;
        Ok(Some(ExprResult {
            shape: ValueShape::Boxed,
            ownership: ExprOwnership::Owned,
        }))
    }

    /// Assignment-position string concat: `s = s + x` where the left operand
    /// is the same binding being reassigned. The pre-call value is dead after
    /// the call, so RT_CONCAT_MOVE may append `x`'s bytes in place when the
    /// string is uniquely owned (rc == 1) with spare capacity — turning
    /// build-a-string-in-a-loop from O(n²) into amortized O(n). The helper
    /// falls back to RT_ADD for shared or non-string values, so this emits
    /// for any boxed target; provably numeric operands stay on
    /// compile_binary's native arithmetic paths. General-position `+` is
    /// untouched. Returns `Some(result)` with the call's value on the stack
    /// when the pattern matched.
    fn try_compile_concat_move(
        &mut self,
        target: &str,
        value: &Expression,
    ) -> Result<Option<ExprResult>, BuildError> {
        let Expression::BinaryExpression(be) = value else {
            return Ok(None);
        };
        if be.operator != "+" {
            return Ok(None);
        }
        let Expression::IdentifierExpression(lhs) = &*be.left else {
            return Ok(None);
        };
        if lhs.name != target {
            return Ok(None);
        }
        // Both operands provably numeric → leave the native int/float fast
        // paths in compile_binary alone.
        if self.numeric_shape_for_expr(&be.left).is_some()
            && self.numeric_shape_for_expr(&be.right).is_some()
        {
            return Ok(None);
        }
        // Left is the target identifier — always a borrowed load. Right may
        // be an owned temp (fresh literal / call result); stash and release
        // it after the call, mirroring compile_binary's boxed-operand
        // mop-up. RT_CONCAT_MOVE copies the bytes it needs, so the temp is
        // dead once the call returns.
        self.compile_expr_as(&be.left, ValueShape::Boxed)?;
        self.compile_expr_as(&be.right, ValueShape::Boxed)?;
        let right_stash = if self.expr_transfers_ownership(&be.right) {
            let t = self.alloc_local();
            self.emit(Instruction::LocalTee(t));
            Some(t)
        } else {
            None
        };
        self.emit(Instruction::Call(
            self.rt().base + crate::runtime::RT_CONCAT_MOVE,
        ));
        if let Some(stash) = right_stash {
            self.emit(Instruction::LocalGet(stash));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(Some(ExprResult {
            shape: ValueShape::Boxed,
            ownership: ExprOwnership::Owned,
        }))
    }

    /// `all(e1, e2, ...)` — legacy direct path via IMPORT_RUN_ALL.
    ///
    /// Production routing rejects or async-lowers `all` before this function is
    /// reachable. This fallback remains for direct builder tests and old
    /// embedding paths that do not run async analysis.
    ///
    /// Each argument expression is wrapped in a synthesized zero-arg
    /// closure (same construction `nowait` uses) so the host can
    /// dispatch them through `__indirect_function_table`. The N closure
    /// pointers are written to a scratch buffer on the heap, and
    /// `run_all(buf_ptr, count)` returns a NaN-boxed tuple of the N
    /// results — which the caller typically destructures via
    /// `let a, b = all(...)`.
    ///
    /// Mirrors the bytecode compiler's `<all-task>` implicit-closure
    /// pattern. Upvalue capture rides on `compile_function_expression`
    /// so arguments that reference outer locals work automatically.
    fn compile_bare_all(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        use fai_compiler::ast::{ExpressionStatement, FunctionDeclaration, SourceLocation};

        let count = args.len();
        let buf = self.alloc_i32_local();

        // Scratch buffer for the closure-pointer array. Each slot is
        // one i64 (NaN-boxed closure value). `run_all` reads `count * 8`
        // bytes starting at `buf`. RT_ALLOC returns an 8-byte aligned
        // address, so no padding is needed.
        let buf_size = (count.max(1) * 8) as i32;
        self.emit(Instruction::I32Const(buf_size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(buf));

        for (i, arg) in args.iter().enumerate() {
            let loc = SourceLocation { line: 0, column: 0 };
            let wrapper = FunctionDeclaration {
                name: format!("<all-task@{}>", i),
                type_params: Vec::new(),
                params: Vec::new(),
                return_types: Vec::new(),
                body: vec![fai_compiler::ast::Statement::ExpressionStatement(
                    ExpressionStatement {
                        expression: (*arg).clone(),
                        location: loc.clone(),
                    },
                )],
                doc: None,
                is_private: None,
                is_abstract: false,
                is_remote: false,
                location: loc,
                doc_comment: None,
            };
            // Closure value lands on the stack as i64.
            self.compile_function_expression(&wrapper)?;

            // Store at buf + i*8.
            let tmp = self.alloc_local();
            self.emit(Instruction::LocalSet(tmp));
            self.emit(Instruction::LocalGet(buf));
            self.emit(Instruction::LocalGet(tmp));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        // run_all(buf, count) -> i64 tuple pointer.
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const(count as i32));
        self.emit_import_call(crate::runtime::IMPORT_RUN_ALL);
        Ok(())
    }

    // Bare-global helpers below. Each mirrors a `translate.rs::name == "..."`
    // branch; together they give the direct path full bare-builtin
    // coverage.

    fn compile_type_check_rt(
        &mut self,
        args: &[&Expression],
        rt_fn: u32,
    ) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("type-check-arg-count"));
        }
        self.compile_expr(args[0])?;
        self.emit(Instruction::Call(self.rt().base + rt_fn));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    fn compile_is_null(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("is_null-arg-count"));
        }
        self.compile_expr(args[0])?;
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    fn compile_is_bool(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("is_bool-arg-count"));
        }
        // (val & INT_CHECK_MASK) == (QNAN | TAG_BOOL) — masks the
        // payload + sign bit so `true` (low bit = 1) and `false`
        // (low bit = 0) both match.
        self.compile_expr(args[0])?;
        self.emit(Instruction::I64Const(INT_CHECK_MASK));
        self.emit(Instruction::I64And);
        self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    /// Allocate a fresh guest String for an interned kind name and store
    /// it in `result_local`. Shared by the `typeOf` branches.
    fn emit_kind_string(&mut self, name: &str, result_local: u32) {
        let (off, len) = self.ctx.strings.borrow_mut().intern(name);
        self.emit(Instruction::I32Const(off as i32));
        self.emit(Instruction::I32Const(len as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC_STRING));
        self.emit(Instruction::LocalSet(result_local));
    }

    /// `typeOf(value)` — the runtime kind of any value as a fresh owned
    /// String: 'int', 'float', 'bool', 'null', 'void', or the heap tag name
    /// ('string', 'array', 'tuple', 'dictionary', 'closure', 'module',
    /// 'record'; anything else reports 'unknown'). A NaN-box/tag inspection —
    /// no stringify, no cast probes — so Unknown-typed data (parsed JSON,
    /// dynamic tool payloads) can branch on shape cheaply.
    fn compile_type_of_bare(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("typeOf-arg-count"));
        }
        // An owned argument temp (fresh call/literal) is only inspected, so
        // release it after the tag is read — compile_binary's operand mop-up.
        let owned = self.expr_transfers_ownership(args[0]);
        self.compile_expr_as(args[0], ValueShape::Boxed)?;
        let val = self.alloc_local();
        self.emit(Instruction::LocalSet(val));
        let result = self.alloc_local();

        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::Call(self.rt().base + RT_IS_INT));
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit_kind_string("int", result);
        self.emit(Instruction::Else);
        {
            self.emit(Instruction::LocalGet(val));
            self.emit(Instruction::Call(self.rt().base + RT_IS_OBJ));
            self.emit_open(Instruction::If(BlockType::Empty));
            {
                // Heap object: dispatch on the tag word at offset 0 via a
                // nested else-chain, so exactly ONE kind string is
                // allocated per call (a default-then-override would leak
                // the overwritten default).
                let tag = self.alloc_i32_local();
                self.emit(Instruction::LocalGet(val));
                self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
                self.emit(Instruction::I32Load(mem0()));
                self.emit(Instruction::LocalSet(tag));
                let tags = [
                    (OBJ_TAG_STRING, "string"),
                    (OBJ_TAG_ARRAY, "array"),
                    (OBJ_TAG_TUPLE, "tuple"),
                    (OBJ_TAG_DICT, "dictionary"),
                    (OBJ_TAG_CLOSURE, "closure"),
                    (crate::runtime::OBJ_TAG_MODULE, "module"),
                    (crate::runtime::OBJ_TAG_INSTANCE, "record"),
                ];
                for (t, name) in tags {
                    self.emit(Instruction::LocalGet(tag));
                    self.emit(Instruction::I32Const(t));
                    self.emit(Instruction::I32Eq);
                    self.emit_open(Instruction::If(BlockType::Empty));
                    self.emit_kind_string(name, result);
                    self.emit(Instruction::Else);
                }
                self.emit_kind_string("unknown", result);
                for _ in tags {
                    self.emit_close();
                }
            }
            self.emit(Instruction::Else);
            {
                self.emit(Instruction::LocalGet(val));
                self.emit(Instruction::I64Const(crate::runtime::VAL_NULL));
                self.emit(Instruction::I64Eq);
                self.emit_open(Instruction::If(BlockType::Empty));
                self.emit_kind_string("null", result);
                self.emit(Instruction::Else);
                {
                    self.emit(Instruction::LocalGet(val));
                    self.emit(Instruction::I64Const(VAL_VOID));
                    self.emit(Instruction::I64Eq);
                    self.emit_open(Instruction::If(BlockType::Empty));
                    self.emit_kind_string("void", result);
                    self.emit(Instruction::Else);
                    {
                        self.emit(Instruction::LocalGet(val));
                        self.emit(Instruction::I64Const(crate::runtime::VAL_TRUE));
                        self.emit(Instruction::I64Eq);
                        self.emit(Instruction::LocalGet(val));
                        self.emit(Instruction::I64Const(VAL_FALSE));
                        self.emit(Instruction::I64Eq);
                        self.emit(Instruction::I32Or);
                        self.emit_open(Instruction::If(BlockType::Empty));
                        self.emit_kind_string("bool", result);
                        self.emit(Instruction::Else);
                        // Everything left is a non-QNAN double (or the
                        // canonical NaN itself).
                        self.emit_kind_string("float", result);
                        self.emit_close();
                    }
                    self.emit_close();
                }
                self.emit_close();
            }
            self.emit_close();
        }
        self.emit_close();

        if owned {
            self.emit(Instruction::LocalGet(val));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        self.emit(Instruction::LocalGet(result));
        Ok(())
    }

    fn compile_is_tagged_obj(
        &mut self,
        args: &[&Expression],
        expected_tag: i32,
    ) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("is_tagged_obj-arg-count"));
        }
        // Default: VAL_FALSE. If the value is an object, load its
        // tag at offset 0 and compare to the expected tag.
        let result = self.alloc_local();
        self.emit(Instruction::I64Const(VAL_FALSE));
        self.emit(Instruction::LocalSet(result));

        self.compile_expr(args[0])?;
        let val = self.alloc_local();
        self.emit(Instruction::LocalSet(val));

        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::Call(self.rt().base + RT_IS_OBJ));
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::I32Load(mem0()));
        self.emit(Instruction::I32Const(expected_tag));
        self.emit(Instruction::I32Eq);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        self.emit(Instruction::LocalSet(result));
        self.emit_close();
        self.emit(Instruction::LocalGet(result));
        Ok(())
    }

    fn compile_to_string_bare(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("toString-arg-count"));
        }
        self.emit_to_string_owned(args[0])
    }

    /// Compile `toString(arg)` leaving a uniformly OWNED (+1) String on the
    /// stack. `RT_VALUE_TO_STR` returns a String argument AS-IS on its fast path
    /// (a borrowed alias, not a fresh +1); the non-String cases build a fresh
    /// string. To make the result uniformly +1 — so `toString` can join the
    /// owned-returning builtins and its result is released like any other temp
    /// instead of leaking when bound or used as a `+` operand (plan 115) — detect
    /// the alias (result `==` the arg value) and retain it. A fresh result is
    /// already +1 and left untouched.
    fn emit_to_string_owned(&mut self, arg: &Expression) -> Result<(), BuildError> {
        // Statically-String argument: `RT_VALUE_TO_STR` is the identity
        // on a String (returns it as-is), so skip the call entirely and
        // just make the value uniformly owned (+1): a borrowed string is
        // retained; an owned/fresh one is already +1. Hot in template
        // interpolation of string fields (`"<div>{{title}}</div>"`).
        if matches!(
            self.expression_type_at(arg),
            Some(fai_checker::types::Type::String)
        ) {
            let owned = self.expr_transfers_ownership(arg);
            self.compile_expr_as(arg, ValueShape::Boxed)?;
            if !owned {
                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
            }
            return Ok(());
        }

        let transfers = self.expr_transfers_ownership(arg);
        self.compile_expr_as(arg, ValueShape::Boxed)?;
        let argl = self.alloc_local();
        self.emit(Instruction::LocalTee(argl));
        self.emit(Instruction::Call(self.rt().base + RT_VALUE_TO_STR));
        let sl = self.alloc_local();
        self.emit(Instruction::LocalTee(sl));
        self.emit(Instruction::LocalGet(sl));
        self.emit(Instruction::LocalGet(argl));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(sl));
        self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        self.emit(Instruction::Drop);
        self.emit(Instruction::End);
        // Release an OWNED arg temp (plan 115 arg-temp mop-up): consuming
        // the arg's +1 is balanced on both paths — the alias case just
        // gave the result its own retained ref above, and a fresh result
        // is independent of the arg. Without this `toString(<owned call>)`
        // (e.g. `toString(signal.value())`, which returns a copy) leaked
        // the arg once per call. Stack-neutral above the result.
        if transfers {
            self.emit(Instruction::LocalGet(argl));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(())
    }

    fn compile_parse_bare(&mut self, args: &[&Expression], rt_fn: u32) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("parse-arg-count"));
        }
        self.compile_expr(args[0])?;
        let stash = self.stash_if_owned(args[0]);
        self.emit(Instruction::Call(self.rt().base + rt_fn));
        self.release_stash(stash);
        Ok(())
    }

    /// Polymorphic `length(v)` — reads the object header's count
    /// field at offset 4. Works for arrays, dicts, and strings
    /// (all share the `[tag, count, ...]` header). Returns Int.
    fn compile_bare_length(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("length-arg-count"));
        }
        self.compile_expr(args[0])?;
        // Release an owned arg temp after reading the header (plan 115 arg-temp
        // mop-up): the result is an Int, independent of the arg. RT_OBJ_ADDR only
        // reads the address, so stash before it consumes the value.
        let stash = self.stash_if_owned(args[0]);
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
        self.release_stash(stash);
        Ok(())
    }

    /// `isEmpty(v)` — count at offset 4 is zero. Matches `length(v) == 0`.
    fn compile_bare_is_empty(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("isEmpty-arg-count"));
        }
        self.compile_expr(args[0])?;
        let stash = self.stash_if_owned(args[0]);
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::I32Eqz);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        self.release_stash(stash);
        Ok(())
    }

    fn compile_bare_json_parse(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("jsonParse-arg-count"));
        }
        let stash = self.emit_string_arg_stashing(args[0])?;
        self.emit_import_call(crate::runtime::IMPORT_JSON_PARSE);
        self.release_stash(stash);
        Ok(())
    }

    fn compile_bare_json_stringify(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("jsonStringify-arg-count"));
        }
        self.compile_expr(args[0])?;
        // json.stringify READS the object to build a fresh string; it doesn't
        // retain it, so an owned arg temp is safe to release after the call
        // (plan 115 arg-temp mop-up). The result string is independent.
        let stash = self.stash_if_owned(args[0]);
        self.emit_import_call(crate::runtime::IMPORT_JSON_STRINGIFY);
        self.release_stash(stash);
        Ok(())
    }

    fn compile_bare_set_html(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("setHtml-arg-count"));
        }
        let stash = self.emit_string_arg_stashing(args[0])?;
        self.emit_import_call(IMPORT_SET_HTML);
        self.release_stash(stash);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    fn compile_bare_set_html_at(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("setHtmlAt-arg-count"));
        }
        let selector_stash = self.emit_string_arg_stashing(args[0])?;
        let html_stash = self.emit_string_arg_stashing(args[1])?;
        self.emit_import_call(IMPORT_SET_HTML_AT);
        self.release_stash(selector_stash);
        self.release_stash(html_stash);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    fn compile_bare_get_location_path(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if !args.is_empty() {
            return Err(BuildError::UnsupportedExpression(
                "getLocationPath-arg-count",
            ));
        }
        self.emit_import_call(IMPORT_GET_LOCATION_PATH);
        Ok(())
    }

    fn compile_bare_push_history_state(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "pushHistoryState-arg-count",
            ));
        }
        self.emit_string_arg_from_expr(args[0])?;
        self.emit_import_call(IMPORT_PUSH_HISTORY_STATE);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    fn compile_bare_replace_location(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression(
                "replaceLocation-arg-count",
            ));
        }
        self.emit_string_arg_from_expr(args[0])?;
        self.emit_import_call(IMPORT_REPLACE_LOCATION);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `Error(msg)` bare-global form of `error.Error(msg)` — same
    /// heap-allocated `{message: msg}` dict.
    fn compile_bare_error_ctor(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("Error-arg-count"));
        }
        // Forward to the existing module-form implementation —
        // same dict construction.
        use fai_compiler::ast::{CallArgument, SourceLocation};
        let fake_loc = SourceLocation { line: 0, column: 0 };
        let call_args: Vec<CallArgument> = args
            .iter()
            .map(|e| CallArgument {
                label: None,
                value: (*e).clone(),
                location: fake_loc.clone(),
            })
            .collect();
        self.compile_error_construct(&call_args)
    }

    /// `unwrap(v, fallback)` bare-global form of `error.unwrap`.
    fn compile_bare_unwrap(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("unwrap-arg-count"));
        }
        use fai_compiler::ast::{CallArgument, SourceLocation};
        let fake_loc = SourceLocation { line: 0, column: 0 };
        let call_args: Vec<CallArgument> = args
            .iter()
            .map(|e| CallArgument {
                label: None,
                value: (*e).clone(),
                location: fake_loc.clone(),
            })
            .collect();
        self.compile_unwrap(&call_args)
    }

    /// `getString(dict, key)` / `getInt` / `getBool` / `get` —
    /// typed dict accessors. All four have the same lookup shape;
    /// the checker enforces the return type. Runtime just does a
    /// `RT_GET_FIELD(dict, key_ptr, key_len)`.
    fn compile_bare_dict_get(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("dict-get-arg-count"));
        }
        self.compile_expr(args[0])?;
        // Extract key ptr/len from the boxed String arg, releasing an owned key
        // temp after the read (plan 115 arg-temp mop-up). The key (often a string
        // literal, e.g. `getInt(props, 'padding')`) is otherwise leaked once per
        // call — and dict accessors are called ~20× per node during render. The
        // result is a BORROW into the dict (arg0), so the dict is NOT released
        // here (that would free what the result points at).
        let key_stash = self.emit_string_arg_stashing(args[1])?;
        self.emit(Instruction::Call(self.rt().base + RT_GET_FIELD));
        self.release_stash(key_stash);
        Ok(())
    }

    /// `hasKey(dict, key) -> Bool` — looks up the key; returns
    /// true when the result isn't `null`.
    fn compile_bare_has_key(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("hasKey-arg-count"));
        }
        self.compile_bare_dict_get(args)?;
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Ne);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        Ok(())
    }

    /// `set(dict, key, value)` — mutates the dict in place via
    /// `RT_SET_FIELD`. Returns the (mutated) dict.
    fn compile_bare_dict_set(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 3 {
            return Err(BuildError::UnsupportedExpression("set-arg-count"));
        }
        // Evaluate dict into a local so we can return it after.
        self.compile_expr(args[0])?;
        let dict = self.alloc_local();
        self.emit(Instruction::LocalTee(dict));

        // Key ptr/len from the stringified arg. RT_SET_FIELD allocates its OWN
        // key string from the ptr/len, so an owned key temp here (a literal, e.g.
        // `set({}, 'text', text)` in every component) is only read and can be
        // released after the call — otherwise it leaks once per set (plan 115).
        let key_stash = self.emit_string_arg_stashing(args[1])?;

        self.compile_expr_as(args[2], ValueShape::Boxed)?;
        // The dict co-owns the stored value (RC, plan 113 R1): retain if
        // borrowed. RT_SET_FIELD releases any value it overwrites.
        if self.expr_transfers_ownership(args[2]) {
            self.emit_ownership_event_for_stack(OwnershipOp::Transfer, OWNERSHIP_SITE_UNKNOWN, 0);
        } else {
            self.emit_ownership_event_for_stack(OwnershipOp::Retain, OWNERSHIP_SITE_UNKNOWN, 0);
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        }
        self.emit_ownership_event_for_stack(OwnershipOp::Store, OWNERSHIP_SITE_UNKNOWN, 0);
        self.emit(Instruction::Call(self.rt().base + RT_SET_FIELD));
        // RT_SET_FIELD returns the dict pointer — identical to the input
        // unless an at-capacity dict was reallocated to fit the new key.
        // Capture it so the result is the live block, not a stale pointer
        // to a grown-away dict.
        self.emit(Instruction::LocalSet(dict));
        self.release_stash(key_stash);

        // Push the (possibly reallocated) dict as the result.
        self.emit(Instruction::LocalGet(dict));
        Ok(())
    }

    /// `print(...args)` — bare-global stdout writer. Each arg
    /// prints on a line via `RT_PRINT_VAL_NEW`, which stringifies
    /// the NaN-boxed value host-side and emits it. Mirrors
    /// `translate.rs`'s inline `name == "print"` handling.
    fn compile_print(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        for a in args {
            // Must be Boxed — `RT_PRINT_VAL_NEW` reads a NaN-boxed Value.
            // `compile_expr` may return a RawInt/RawFloat from arithmetic
            // fast-paths (e.g. `print(-7)`, `print(a + b)`), so force the
            // shape.
            self.compile_expr_as(a, ValueShape::Boxed)?;
            if self.expr_transfers_ownership(a) {
                // Owned argument temporary (a literal or fresh-call result):
                // `print` borrows it to stringify, then it's dead — release it
                // (RC, plan 113 R2) or it leaks per call. `LocalTee` keeps the
                // value on the stack for the print call and saves a copy to free
                // afterward (RT_RELEASE's is_obj guard skips boxed primitives).
                let t = self.alloc_local();
                self.emit(Instruction::LocalTee(t));
                self.emit(Instruction::Call(self.rt().base + RT_PRINT_VAL_NEW));
                self.emit(Instruction::LocalGet(t));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            } else {
                self.emit(Instruction::Call(self.rt().base + RT_PRINT_VAL_NEW));
            }
        }
        // print returns Void at the fai level; push VAL_VOID.
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    /// `message(err)` / `kind(err)` — read a named field from an
    /// Error dict. Errors are represented as dicts `{message: ...}`;
    /// `kind` isn't populated by the `Error(msg)` constructor and
    /// yields `VAL_NULL` in that case. Declared in the checker as
    /// returning `String`; the checker assumes a well-formed Error
    /// value so the direct path doesn't type-check further.
    fn compile_error_field(&mut self, field: &str, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("error-field-arg-count"));
        }
        self.compile_field_access(args[0], field)
    }

    /// `isError(v) -> Bool`. At runtime Errors are plain Dicts
    /// (`{message: ...}`), so we treat "is a Dict-tagged object"
    /// as the approximation — the checker's type rules prevent
    /// passing arbitrary dicts in practice. Matches the direct
    /// path's `is_dict`-style inline tag check: push
    /// `RT_IS_OBJ` and if true inspect the tag at offset 0.
    fn compile_is_error(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 1 {
            return Err(BuildError::UnsupportedExpression("isError-arg-count"));
        }
        // Default result: false. Stash in a local so we can
        // conditionally overwrite inside the object/tag check.
        let result = self.alloc_local();
        self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
        self.emit(Instruction::LocalSet(result));

        // Evaluate arg into a local so we can use it twice
        // (once for RT_IS_OBJ, once for the tag load if it IS an obj).
        self.compile_expr(args[0])?;
        let val = self.alloc_local();
        self.emit(Instruction::LocalSet(val));

        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::Call(self.rt().base + RT_IS_OBJ));
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(val));
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        self.emit(Instruction::I32Load(mem0()));
        self.emit(Instruction::I32Const(OBJ_TAG_DICT));
        self.emit(Instruction::I32Eq);
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
        self.emit(Instruction::LocalSet(result));
        self.emit_close();
        self.emit(Instruction::LocalGet(result));
        Ok(())
    }

    /// Extern FFI call. Serialises each arg as a NaN-boxed i64 at
    /// `mem[65536 + i*8]` — the scratch region the host reads
    /// during its marshalling pass — then calls
    /// `IMPORT_CALL_FFI(ext_fn_idx, arg_count, args_base=65536)`.
    /// The host uses the program's `ExternFnInfo` metadata to
    /// unbox args into the right C types via libloading, then
    /// re-boxes the return. Variadic externs work automatically
    /// since we pass the runtime arg count, not the declared one.
    /// Mirrors `translate.rs::Op::CallExtern`.
    fn compile_extern_call(
        &mut self,
        extern_name: &str,
        ext_fn_idx: u16,
        args: &[&Expression],
    ) -> Result<(), BuildError> {
        // Heap-allocate the args scratch buffer per call. A fixed
        // address here (the old `FFI_ARGS_BASE = 65536`) is unsound: the
        // heap base is `string_pool + bucket_region`, which in a large
        // program (big interned-string pool) climbs to and past 0x10000,
        // so the fixed buffer collides with live heap objects and every
        // FFI call scribbles them (a layout-dependent heap corruption —
        // it bit brain once its string pool crossed 64 KiB). A heap block
        // can't alias the heap. RT_ALLOC rounds up, so a 0-arg call still
        // gets a valid block; size it to hold every arg slot.
        let buf_bytes = ((args.len() as i32) * 8).max(8);
        let args_buf = self.alloc_i32_local();
        self.emit(Instruction::I32Const(buf_bytes));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(args_buf));

        for (i, a) in args.iter().enumerate() {
            self.emit(Instruction::LocalGet(args_buf));
            self.emit(Instruction::I32Const((i as i32) * 8));
            self.emit(Instruction::I32Add);
            // Must be Boxed — the scratch slot holds a NaN-boxed i64.
            // A RawFloat/RawInt from an arithmetic fast-path (e.g.
            // `fabs(-5.5)`) stored unconverted would trip wasm
            // validation with "expected i64, found f64".
            self.compile_expr_as(a, ValueShape::Boxed)?;
            self.emit(Instruction::I64Store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            }));
        }
        self.emit(Instruction::I32Const(ext_fn_idx as i32));
        self.emit(Instruction::I32Const(args.len() as i32));
        self.emit(Instruction::LocalGet(args_buf));
        self.emit_import_call(IMPORT_CALL_FFI);
        // ^ leaves the C return value on the stack as i64. Stash it so we
        // can read OUT params back and free the args buffer, then restore
        // it on top.
        let ret_local = self.alloc_local();
        self.emit(Instruction::LocalSet(ret_local));
        // For OUT params the host wrote the tracked-handle Value back into
        // `mem[args_buf + i*8]`; copy it into the source local so
        // `let db Ptr? = null; sqlite3_open(path, db)` sees `db` populated.
        if let Some(out_flags) = self.ctx.extern_out_params.get(extern_name).cloned() {
            for (i, &is_out) in out_flags.iter().enumerate() {
                if !is_out {
                    continue;
                }
                let Some(source_expr) = args.get(i) else {
                    continue;
                };
                if let Expression::IdentifierExpression(id) = source_expr {
                    let Some(binding) = self.lookup(&id.name) else {
                        continue;
                    };
                    self.emit(Instruction::LocalGet(args_buf));
                    self.emit(Instruction::I32Const((i as i32) * 8));
                    self.emit(Instruction::I32Add);
                    self.emit(Instruction::I64Load(MemArg {
                        offset: 0,
                        align: 3,
                        memory_index: 0,
                    }));
                    if binding.is_cell {
                        // Write through the heap cell (value-RC store @8;
                        // the host-written out value is a fresh handle —
                        // transfer).
                        self.emit_cell_store(binding.local, ExprResult::boxed(true));
                    } else {
                        self.emit_convert(ValueShape::Boxed, binding.shape)?;
                        self.emit(Instruction::LocalSet(binding.local));
                    }
                }
            }
        }
        // Free the scratch buffer (raw i64 slots, not an object graph — a
        // flat RT_FREE, no child release). Then restore the return value.
        self.emit(Instruction::LocalGet(args_buf));
        self.emit(Instruction::I32Const(buf_bytes));
        self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_FREE));
        self.emit(Instruction::LocalGet(ret_local));
        Ok(())
    }

    /// Indirect-dispatch a call whose callee is a value-typed binding
    /// (a closure). Matches the bytecode translator's pattern:
    ///
    /// 1. Unbox the closure value to its heap address.
    /// 2. Save the current `env_ptr` so the caller can resume reads
    ///    from its own upvalues after the callee returns.
    /// 3. Set `env_ptr = addr + 16` (skip past the header).
    /// 4. Push the caller's args (`i64` each).
    /// 5. Push the `table_idx` from the closure header at offset 4.
    /// 6. `call_indirect` with the `FaiFunc(N)` type for this arity.
    /// 7. Restore `env_ptr`.
    ///
    /// Any outer-scope save/restore isn't needed if we're not inside a
    /// closure ourselves, but emitting it unconditionally is cheap and
    /// keeps the pattern uniform; env_ptr at the top level is `0` and
    /// restoring it is a no-op.
    fn compile_indirect_call(
        &mut self,
        callee: Resolve,
        args: &[&Expression],
    ) -> Result<(), BuildError> {
        match callee {
            Resolve::Local(l) => {
                self.emit(Instruction::LocalGet(l.local));
                self.emit_convert(l.shape, ValueShape::Boxed)?;
            }
            Resolve::Upvalue(u) => self.emit_upvalue_read(u),
            Resolve::ModuleVar(global_idx) => {
                self.emit(Instruction::GlobalGet(global_idx));
            }
        }
        self.finish_indirect_call(args)
    }

    /// Indirect-call a callee expression that lowers to a boxed
    /// closure value — e.g. `foo!()`, `obj.cb()` where `cb` is a
    /// closure-typed field, `matched!.builder()`, or `arr[i]()`.
    /// `compile_expr_as` already handles the inner expression
    /// shapes (including `ForceUnwrapExpression`'s null-trap);
    /// `finish_indirect_call` then unboxes to a heap address, swaps
    /// `env_ptr`, pushes args, and dispatches through the closure's
    /// `table_idx`.
    fn compile_indirect_call_from_expr(
        &mut self,
        callee_expr: &Expression,
        args: &[&Expression],
    ) -> Result<(), BuildError> {
        self.compile_expr_as(callee_expr, ValueShape::Boxed)?;
        self.finish_indirect_call(args)
    }

    /// Finish the indirect-call sequence assuming the boxed closure
    /// value is already on the stack. Unboxes to a heap address,
    /// saves/restores `env_ptr`, pushes args, and dispatches.
    fn finish_indirect_call(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        let arity = args.len() as u16;
        let Some(&type_idx) = self.ctx.fai_func_type_indices.get(&arity) else {
            return Err(BuildError::UnsupportedExpression(
                "CallExpression/closure-arity-missing-type",
            ));
        };

        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        let addr_local = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(addr_local));

        // When the engine scheduler is in play, a *sync* function may still be
        // handed an *async* closure (e.g. `renderSSR(app)` doing `app()` where
        // `app` is an async component). Dispatch on the closure's `frame_size`
        // (header offset 12): 0 = a plain sync `FaiFunc` (`call_indirect`), > 0
        // = a resume fn that must be spawned and driven to completion. This is
        // the sync→async boundary mirror of the async path's `Term::AwaitClosure`
        // and the host's `__fai_drive_closure`.
        if let Some(actx) = self.ctx.async_ctx {
            let layout = *actx.layout;
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(12)));
            self.emit(Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            // ── async: spawn the closure as a task, drive `poll`, read result ──
            // Args (evaluated in the caller's env) → the new frame's param slots.
            let mut arg_locals = Vec::with_capacity(args.len());
            for a in args {
                self.compile_expr_as(a, ValueShape::Boxed)?;
                let l = self.alloc_local();
                self.emit(Instruction::LocalSet(l));
                arg_locals.push(l);
            }
            let frame_l = self.alloc_i32_local();
            let id_l = self.alloc_i32_local();
            let saved_cur = self.alloc_i32_local();
            // frame = alloc(frame_size)
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(12)));
            self.emit(Instruction::Call(layout.alloc));
            self.emit(Instruction::LocalSet(frame_l));
            // Fresh async frames must not inherit stale pending/local slots from
            // a recycled heap block. The awaited-closure CFG reads pending slots
            // after resume; stale ids there become invalid `task_result` reads.
            self.emit(Instruction::LocalGet(frame_l));
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(12)));
            self.emit(Instruction::MemoryFill(0));
            // frame[0] = env_ptr = addr + 16
            self.emit(Instruction::LocalGet(frame_l));
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Const(16));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Store(mem0()));
            // frame[8 + 8*i] = arg_i (params sit past the env slot)
            for (i, (arg, l)) in args.iter().zip(arg_locals.iter()).enumerate() {
                self.emit(Instruction::LocalGet(frame_l));
                let owned = self.expr_transfers_ownership(arg);
                self.emit(Instruction::LocalGet(*l));
                self.prepare_stack_for_owning_store(ExprResult {
                    shape: ValueShape::Boxed,
                    ownership: if owned {
                        ExprOwnership::Owned
                    } else {
                        ExprOwnership::Borrowed
                    },
                });
                self.emit(Instruction::I64Store(mem_off(8 + 8 * i as u64)));
            }
            // id = spawn(table_idx @ addr+4, frame)
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(4)));
            self.emit(Instruction::LocalGet(frame_l));
            self.emit(Instruction::Call(layout.spawn));
            self.emit(Instruction::LocalSet(id_l));
            // Record frame size so completion can reclaim the closure frame.
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(12)));
            self.emit(Instruction::I32Store(mem_off(
                crate::async_engine::O_FRAME_SIZE,
            )));
            // This caller is the result consumer while the drive loop runs. If
            // left detached (-1), a quickly completed child recycles itself and
            // releases O_RESULT before the caller reaches `task_result`.
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Const(-2));
            self.emit(Instruction::I32Store(mem_off(
                crate::async_engine::O_WAITER,
            )));
            // saved_cur = g_current (re-entrant: an outer task may be live)
            self.emit(Instruction::GlobalGet(layout.g_current));
            self.emit(Instruction::LocalSet(saved_cur));
            // drive: loop { poll(); if task[id] done break; if no ready work
            // left and we can't make progress, break too }. On the browser, the
            // task may park on a *host* op (a `remoteCall` fetch) that only
            // completes via the JS event loop — so when the ready queue empties
            // with the task still running, we must RETURN (yield) to the browser
            // rather than busy-spin (which would deadlock the fetch). The host
            // wakes the task later via `__fai_resume_task` + `__fai_poll`. On
            // native there is no event loop and `remote_begin` re-readies the
            // task synchronously, so we keep busy-polling (never stuck).
            let yield_when_stuck = layout.set_timer.is_some();
            self.emit(Instruction::Block(wasm_encoder::BlockType::Empty));
            self.emit(Instruction::Loop(wasm_encoder::BlockType::Empty));
            self.emit(Instruction::Call(layout.poll));
            self.emit(Instruction::Drop);
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Load(mem_off(crate::async_engine::O_STATUS)));
            self.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
            self.emit(Instruction::I32GeS);
            self.emit(Instruction::BrIf(1));
            if yield_when_stuck {
                // ready queue empty (g_head == -1) → parked on an external op.
                self.emit(Instruction::GlobalGet(layout.g_head));
                self.emit(Instruction::I32Const(-1));
                self.emit(Instruction::I32Eq);
                self.emit(Instruction::BrIf(1));
            }
            self.emit(Instruction::Br(0));
            self.emit(Instruction::End);
            self.emit(Instruction::End);
            // g_current = saved_cur
            self.emit(Instruction::LocalGet(saved_cur));
            self.emit(Instruction::GlobalSet(layout.g_current));
            // If the task finished, read and recycle its result. If browser
            // polling yielded because the child is parked on host I/O, detach
            // it again and return Void; the host wakeup will let it finish for
            // side effects, but this sync caller cannot suspend to await it.
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Load(mem_off(crate::async_engine::O_STATUS)));
            self.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
            self.emit(Instruction::I32GeS);
            self.emit(Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::Call(layout.task_result));
            let async_result_l = self.alloc_local();
            self.emit(Instruction::LocalSet(async_result_l));
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I64Load(mem_off(crate::async_engine::O_RESULT)));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::GlobalGet(layout.g_free_head));
            self.emit(Instruction::I32Store(mem_off(crate::async_engine::O_NEXT)));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::GlobalSet(layout.g_free_head));
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Const(crate::async_engine::ST_FREED));
            self.emit(Instruction::I32Store(mem_off(
                crate::async_engine::O_STATUS,
            )));
            self.emit(Instruction::LocalGet(async_result_l));
            self.emit(Instruction::Else);
            self.emit(Instruction::GlobalGet(layout.g_table_base));
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            self.emit(Instruction::I32Mul);
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Const(-1));
            self.emit(Instruction::I32Store(mem_off(
                crate::async_engine::O_WAITER,
            )));
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit(Instruction::End);
            self.emit(Instruction::Else);
            // ── sync: the original `call_indirect` (env-set + args inline) ──
            let saved_env = self.alloc_i32_local();
            self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
            self.emit(Instruction::LocalSet(saved_env));
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Const(16));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
            // Owned arg temps to release after this synchronous closure call
            // (RC, plan 113 R2). Same reasoning as the no-async sync path below.
            let mut owned_arg_stashes: Vec<u32> = Vec::new();
            for a in args {
                self.compile_expr_as(a, ValueShape::Boxed)?;
                if self.expr_transfers_ownership(a) {
                    let t = self.alloc_local();
                    self.emit(Instruction::LocalTee(t));
                    owned_arg_stashes.push(t);
                }
            }
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(4)));
            self.emit(Instruction::CallIndirect {
                type_index: type_idx,
                table_index: 0,
            });
            self.emit(Instruction::LocalGet(saved_env));
            self.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
            // This vector is only initialized on the sync branch of the emitted
            // wasm `if`; release it before closing that branch so the async
            // branch never sees unassigned stash locals during propagation.
            self.release_owned_arg_stashes(&owned_arg_stashes);
            self.emit(Instruction::End); // end if
            self.emit_post_call_propagation(&[]);
            return Ok(());
        }

        // Save env_ptr → scratch, then set env_ptr = addr + 16.
        let saved_env = self.alloc_i32_local();
        self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
        self.emit(Instruction::LocalSet(saved_env));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I32Const(16));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));

        // Push args, stashing owned temporaries to release after the call (RC,
        // plan 113 R2). A closure borrows its params and retains anything it
        // keeps (the owned-value discipline), so the caller-side temp is dead
        // once the synchronous call returns — release it or it leaks.
        let mut owned_arg_stashes: Vec<u32> = Vec::new();
        for a in args {
            self.compile_expr_as(a, ValueShape::Boxed)?;
            if self.expr_transfers_ownership(a) {
                let t = self.alloc_local();
                self.emit(Instruction::LocalTee(t));
                owned_arg_stashes.push(t);
            }
        }

        // table_idx from closure header (offset 4), then call_indirect.
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::CallIndirect {
            type_index: type_idx,
            table_index: 0,
        });

        // Restore env_ptr before the propagation check so an unwind
        // doesn't leak a closure's upvalue frame into outer scope.
        self.emit(Instruction::LocalGet(saved_env));
        self.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
        self.emit_post_call_propagation(&owned_arg_stashes);
        for t in owned_arg_stashes {
            self.emit(Instruction::LocalGet(t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(())
    }

    /// Compile an anonymous `def(...) ... end` — a closure literal.
    /// Recursively builds the closure body in a child `Builder`, then
    /// emits the heap-object layout at the current site and boxes the
    /// resulting address as a `Closure`-tagged object.
    ///
    /// Bare identifier that names a top-level function (not a local /
    /// upvalue) used in a value position — e.g. `apply(x, shout)`
    /// where `shout` is a `def`. Synthesize a forwarding closure
    /// `do with _a0 Unknown, ... end` whose body is `shout(_a0, ...)`,
    /// then hand it to the normal closure emitter. The result is a
    /// NaN-boxed Closure value callable through the indirect table,
    /// which is what the callee's `fn(value)` invocation expects.
    ///
    /// Falls through to `UnknownIdentifier` when the name isn't a
    /// top-level function either. Also consults `module_context` so
    /// peer-function references inside a user module resolve without
    /// requiring the `helpers.peer` alias.
    fn compile_function_reference(&mut self, name: &str) -> Result<(), BuildError> {
        let proto_idx = self
            .function_by_name
            .get(name)
            .copied()
            .or_else(|| {
                self.module_context.as_ref().and_then(|ctx_mod| {
                    let qualified = format!("{}.{}", ctx_mod, name);
                    self.function_by_name.get(&qualified).copied()
                })
            })
            .or_else(|| {
                self.ctx
                    .named_imports
                    .get(name)
                    .and_then(|qualified| self.function_by_name.get(qualified).copied())
            })
            .ok_or_else(|| BuildError::UnknownIdentifier(name.to_string()))?;
        let arity = self.functions()[proto_idx as usize].param_count as usize;

        use fai_compiler::ast::{
            CallArgument, CallExpression, ExpressionStatement, FunctionDeclaration,
            IdentifierExpression, Parameter, SourceLocation, Statement, TypeNode,
        };
        let loc = SourceLocation { line: 0, column: 0 };
        // Param names: `_fref0..._frefN`. They shadow nothing at the
        // call site (we're in a fresh closure body scope).
        let params: Vec<Parameter> = (0..arity)
            .map(|i| Parameter {
                name: format!("_fref{}", i),
                type_node: TypeNode {
                    kind: "TypeNode".to_string(),
                    name: Some("Unknown".to_string()),
                    is_type_parameter: Some(false),
                    function_params: None,
                    function_returns: None,
                    is_array: false,
                    is_optional: false,
                    location: loc.clone(),
                },
                default_value: None,
                is_out: false,
                is_mutable: false,
                location: loc.clone(),
                doc_comment: None,
            })
            .collect();
        let args: Vec<CallArgument> = (0..arity)
            .map(|i| CallArgument {
                label: None,
                value: fai_compiler::ast::Expression::IdentifierExpression(IdentifierExpression {
                    name: format!("_fref{}", i),
                    location: loc.clone(),
                }),
                location: loc.clone(),
            })
            .collect();
        let body_call = fai_compiler::ast::Expression::CallExpression(CallExpression {
            callee: Box::new(fai_compiler::ast::Expression::IdentifierExpression(
                IdentifierExpression {
                    name: name.to_string(),
                    location: loc.clone(),
                },
            )),
            args,
            location: loc.clone(),
        });
        let wrapper = FunctionDeclaration {
            name: format!("<fref:{}>", name),
            type_params: Vec::new(),
            params,
            return_types: Vec::new(),
            body: vec![Statement::ExpressionStatement(ExpressionStatement {
                expression: body_call,
                location: loc.clone(),
            })],
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            location: loc,
            doc_comment: None,
        };
        self.compile_function_expression(&wrapper)
    }

    /// The child builder's `outer_scope` points at the current scope
    /// stack; identifier lookups in the body that miss the local
    /// scopes fall through and get recorded as upvalues (captured by
    /// value — the enclosing local's value at closure-creation time).
    ///
    fn compile_function_expression(&mut self, fd: &FunctionDeclaration) -> Result<(), BuildError> {
        // A3.0: a closure that awaits/forks is compiled as a *resume* fn
        // (env-in-frame, scheduler-driven), not a sync `FaiFunc`. Generic async
        // closures are out of scope for now (fall back).
        let is_async = if let Some(actx) = self.ctx.async_ctx {
            let r = AsyncResolve {
                async_set: actx.async_set,
                all_fns: actx.all_fns,
                aliases: self.ctx.module_aliases,
                named_imports: self.ctx.named_imports,
                module_context: self.module_context.as_deref(),
                ufcs_calls: &self.ctx.checker.ufcs_calls,
                module_key: &self.module_key,
            };
            closure_is_async(fd, &r)
        } else {
            false
        };
        if is_async && !fd.type_params.is_empty() {
            return Err(BuildError::UnsupportedExpression("async-closure-generic"));
        }

        if self.outer_scope.is_some() {
            for name in collect_referenced_names(&fd.body) {
                if self.lookup(&name).is_none() {
                    let _ = self.resolve(&name);
                }
            }
        }

        let outer_view = OuterScopeView {
            scopes: &self.scopes,
            upvalues: &self.upvalues,
            upvalue_by_name: &self.upvalue_by_name,
        };
        let param_count = fd.params.len() as u16 + fd.type_params.len() as u16;
        let (inner_fn, upvalues, frame_size) = if is_async {
            // Resume-fn lowering: frame leads with `env_ptr`; the captured
            // upvalues come back so the header's env block is filled below.
            let actx = self.ctx.async_ctx.unwrap();
            let r = AsyncResolve {
                async_set: actx.async_set,
                all_fns: actx.all_fns,
                aliases: self.ctx.module_aliases,
                named_imports: self.ctx.named_imports,
                module_context: self.module_context.as_deref(),
                ufcs_calls: &self.ctx.checker.ufcs_calls,
                module_key: &self.module_key,
            };
            let frame = async_frame_layout(fd, &r, true);
            let fsize = frame.size;
            let file_key = self.module_key.clone();
            let (f, upvalues) = build_resume_fn(
                self.ctx,
                fd,
                &frame,
                actx.fn_table_idx,
                actx.frame_sizes,
                actx.layout,
                &r,
                self.module_context.as_deref(),
                Some(file_key.as_str()),
                Some(&outer_view),
            )?;
            (f, upvalues, fsize)
        } else {
            let mut inner = Builder::new(fd, self.ctx, Some(&outer_view));
            // Inherit the enclosing function's module context so
            // peer-function calls inside the closure resolve via the
            // `{module}.{name}` fallback, same as non-closure calls.
            inner.module_key = self.module_key.clone();
            inner.module_context = self.module_context.clone();
            inner.compile_body()?;
            let upvalues = std::mem::take(&mut inner.upvalues);
            (inner.finish(), upvalues, 0i32)
        };

        // Register the new closure proto. Its table_idx is the
        // GLOBAL slot this closure will occupy in the module's
        // element section — `closure_offset_base` covers closures
        // emitted by previous top-level functions in the same
        // program, and `self.ctx.closures.borrow().len()` is the
        // count within this function. Baking the global index
        // into the header is critical: `call_indirect` reads the
        // table slot directly from the header at runtime.
        let local_idx = self.ctx.closures.borrow().len() as u32;
        let table_idx = self.ctx.closure_offset_base + local_idx;
        self.ctx.closures.borrow_mut().push(BuiltClosure {
            info: FunctionInfo {
                name: format!("<closure@{}:{}>", fd.location.line, fd.location.column),
                param_count,
                type_param_count: fd.type_params.len() as u16,
                param_names: param_names_for(fd),
                include_in_coverage: false,
                param_defaults: param_defaults_for(fd),
                source_file: self.current_source_file(),
                source_line: fd.location.line,
                ..Default::default()
            },
            function: inner_fn,
            proto_index: table_idx,
            is_async,
        });

        // Allocate heap memory for the closure object.
        //   [0..4)   tag = OBJ_TAG_CLOSURE
        //   [4..8)   table_idx
        //   [8..12)  uv_count
        //   [12..16) frame_size — 0 = sync closure, >0 = async resume fn (A3.0)
        //   [16..)   upvalues (i64 each)
        let uv_count = upvalues.len() as i32;
        let size = 16 + uv_count * 8;
        let tmp = self.alloc_i32_local();

        self.emit(Instruction::I32Const(size));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(tmp));

        // tag
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I32Const(OBJ_TAG_CLOSURE));
        self.emit(Instruction::I32Store(mem0()));
        // table_idx
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I32Const(table_idx as i32));
        self.emit(Instruction::I32Store(mem_off(4)));
        // uv_count
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I32Const(uv_count));
        self.emit(Instruction::I32Store(mem_off(8)));
        // frame_size — 0 for a sync closure, the async resume-fn frame size
        // otherwise (the runtime async marker; written explicitly since the
        // allocator doesn't zero this slot).
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I32Const(frame_size));
        self.emit(Instruction::I32Store(mem_off(12)));
        // upvalues — non-cell locals snapshot their current Boxed value;
        // cell-bound locals store the cell as a NaN-boxed object (plan
        // 114), so outer and closure share one mutable slot.
        for (i, upvalue) in upvalues.iter().enumerate() {
            self.emit(Instruction::LocalGet(tmp));
            match upvalue.source {
                CaptureSource::Local(outer_local) => {
                    if upvalue.is_cell {
                        self.emit(Instruction::LocalGet(outer_local.local));
                        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
                    } else {
                        self.emit(Instruction::LocalGet(outer_local.local));
                        self.emit_convert(outer_local.shape, ValueShape::Boxed)?;
                    }
                }
                CaptureSource::Upvalue(uv_idx) => {
                    self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
                    self.emit(Instruction::I64Load(mem_off(uv_idx as u64 * 8)));
                }
            }
            // The closure co-owns each captured object (RC, plan 113 R1): a
            // captured object must outlive the binding it snapshots, so
            // retain it. Cells included (plan 114) — the closure's retained
            // ref is what keeps a shared cell alive after the enclosing
            // scope (or async frame) lets go. RT_RELEASE's closure-teardown
            // branch releases every upvalue, balancing these.
            self.capture_into_closure(i);
            self.emit(Instruction::I64Store(mem_off(16 + i as u64 * 8)));
        }

        // Box the address as a NaN-boxed closure value.
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
        Ok(())
    }

    /// Short-circuit `and` / `or` lowered to an if/else that returns
    /// an i64 (boxed) value.
    ///   `lhs and rhs`  ≡  `if truthy(lhs) then rhs else lhs`
    ///   `lhs or  rhs`  ≡  `if truthy(lhs) then lhs else rhs`
    /// The checker guarantees both sides are Bool, so the i64 is
    /// always a Bool value; we return `ValueShape::Boxed` to match.
    fn compile_short_circuit(&mut self, be: &BinaryExpression) -> Result<ValueShape, BuildError> {
        self.compile_expr_as(&be.left, ValueShape::Boxed)?;
        let lhs_local = self.alloc_local();
        self.emit(Instruction::LocalTee(lhs_local));
        self.emit_truthy_i32();
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        if be.operator == "and" {
            self.compile_expr_as(&be.right, ValueShape::Boxed)?;
            self.emit(Instruction::Else);
            self.emit(Instruction::LocalGet(lhs_local));
        } else {
            self.emit(Instruction::LocalGet(lhs_local));
            self.emit(Instruction::Else);
            self.compile_expr_as(&be.right, ValueShape::Boxed)?;
        }
        self.emit_close();
        Ok(ValueShape::Boxed)
    }

    /// RC ownership classifier (plan 113 R1). True if `expr` yields a FRESH,
    /// sole-owned value — a literal, or a `+` (concat String / numeric). Storing
    /// or binding such a value TRANSFERS its single owning reference (no retain).
    /// Everything else (identifier / field / index / call / unwrap reads, and
    /// `and`/`or` which return a borrowed OPERAND) is BORROWED — co-owning it
    /// requires a retain. Conservative: not-provably-fresh → retain (over-retain
    /// leaks; the opposite would be a use-after-free).
    fn is_fresh_value(expr: &Expression) -> bool {
        match expr {
            Expression::StringExpression(_)
            | Expression::NumberExpression(_)
            | Expression::NullExpression(_)
            | Expression::BooleanExpression(_)
            | Expression::ArrayExpression(_)
            | Expression::DictionaryExpression(_)
            | Expression::TupleExpression(_)
            | Expression::TemplateStringExpression(_)
            // A `do...end` / function expression allocates a fresh closure
            // object (RT_MAKE_OBJ) — sole-owned, so binding/storing it transfers
            // the single ref. Without this it would be treated as borrowed and
            // over-retained, leaking the closure (and everything it captured)
            // since nothing ever brings the count back to zero.
            | Expression::FunctionExpression(_) => true,
            Expression::BinaryExpression(be) => be.operator == "+",
            _ => false,
        }
    }

    /// Bare-global builtins that return a BORROWED reference (an element, a dict
    /// field, or a passed-in argument) rather than a fresh allocation. A call to
    /// one of these must never be treated as ownership-transferring — even if a
    /// user function shares the name, `compile_call` dispatches bare-globals
    /// before user functions, so the builtin (borrowed) body runs. Keep in sync
    /// with the borrowed-returning arms of `try_compile_bare_global`
    /// (`unwrap`, the `get*` dict accessors, `set`, the `message`/`kind` error
    /// field reads). Fresh-returning bare-globals are intentionally absent —
    /// transferring their (genuinely fresh) result is correct.

    /// Builtins that ALWAYS return a freshly `RT_ALLOC`-d (rc-prefixed) object
    /// distinct from their arguments (plan 113 R2). A call to one of these is
    /// ownership-transferring just like a user function: the result is a
    /// sole-owned +1 the caller takes without retaining, so binding it no longer
    /// leaks. Restricted to the structural collection builders whose bodies I've
    /// confirmed unconditionally allocate (append/getKeys/sort/slice/reverse are
    /// guest `RT_ALLOC`; map/filter/all route through the rc-prefixed host
    /// `reserve`; copy/Error/split always build a new object). String transforms
    /// (`trim`/`replace`/`toString`/…) can alias their input on a fast path
    /// (`RT_VALUE_TO_STR` returns a String arg as-is; `replace` returns the input
    /// when `find` is empty), but each is normalised to a uniform +1 — `replace`
    /// retains before returning, `toString`'s codegen (`emit_to_string_owned`)
    /// retains on the alias path — so transferring them is sound. Still EXCLUDES
    /// host-allocated results (json/env/file/path — unverified rc prefix), where
    /// transferring an aliasing result would be a use-after-free; the bar is
    /// "provably +1".

    /// Ownership inference (plan 113 R2). True if `ce` resolves to a call that
    /// yields an OWNED (+1) value the caller must take ownership of — a
    /// user-defined function or a closure value, both of which return +1 via
    /// `compile_return`'s always-retain-borrowed convention. Everything else
    /// (builtins, externs, type constructors, unresolved callees) is treated as
    /// borrowed: the caller retains, which is correct for a borrowed result and
    /// merely leaks a fresh one — never a use-after-free.
    ///
    /// CONSERVATIVE BY CONSTRUCTION: it returns true only when the callee is
    /// provably a user function or a bound closure value. Builtins are never in
    /// `function_by_name`, and the borrowed-returning bare-globals are excluded
    /// up front (mirroring `compile_call`'s bare-global-first dispatch), so a
    /// borrowed result can never be misclassified as transferable.
    /// Does `name` resolve to a user function, the way `compile_call`
    /// resolves a callee: bare name, module-peer fallback
    /// (`{module_context}.{name}` — peer calls inside a user module
    /// don't carry the module prefix), or a named import. Ownership
    /// classification must use the SAME resolution: a peer call
    /// classified "not a user fn" reads as borrowed, gets retained on
    /// bind / skipped by operand mop-up, and leaks one ref per call —
    /// this was the html-forui SSR per-node leak (plan 116).
    fn resolves_to_user_fn(&self, name: &str) -> bool {
        if self.function_by_name.contains_key(name) {
            return true;
        }
        if let Some(ctx_mod) = &self.module_context {
            if self
                .function_by_name
                .contains_key(&format!("{}.{}", ctx_mod, name))
            {
                return true;
            }
        }
        matches!(
            self.ctx.named_imports.get(name),
            Some(q) if self.function_by_name.contains_key(q)
        )
    }

    /// Ownership classifier (plan 117 phase 3): does `ce` yield an OWNED
    /// (+1) value the caller must take? Table-driven for everything the
    /// signature table classifies statically (the heuristic this replaced
    /// was proven equivalent by a zero-divergence FAI_ABI_CHECK run over
    /// the full 320-fixture corpus immediately before the swap); dynamic
    /// callables — externs, bound closures, user fns, type constructors —
    /// are per-program facts resolved in code, all returning +1.
    fn call_returns_owned(&mut self, ce: &CallExpression) -> bool {
        use fai_compiler::ownership_abi::{
            lookup_bare_call, lookup_member_call, lookup_std_module_call, ReturnConvention,
        };
        let ufcs_key = (
            self.module_key.clone(),
            ce.location.line,
            ce.location.column,
        );
        let is_ufcs = self.checker().ufcs_calls.contains(&ufcs_key);
        let sig = match &*ce.callee {
            Expression::IdentifierExpression(id) => lookup_bare_call(id.name.as_str()),
            // UFCS `recv.method(...)`: member dispatch checks the borrowed
            // list first (the bare/member set/unwrap asymmetry, preserved
            // by decision — see plans/119 KTD).
            Expression::MemberExpression(me) if is_ufcs => lookup_member_call(me.property.as_str()),
            Expression::MemberExpression(me) => {
                let m = me.property.as_str();
                // Module-alias path first, only when the object is an
                // unshadowed module alias. The mutating `resolve` is
                // deliberate: this is the real compilation path and the
                // heuristic it replaced had the same side-effect profile.
                let mut sig = None;
                if let Expression::IdentifierExpression(obj_id) = &*me.object {
                    if self.resolve(&obj_id.name).is_none() {
                        if let Some(canon) = self.ctx.module_aliases.get(&obj_id.name) {
                            // A user-module function (`alias.fn`) is dynamic:
                            // +1 via compile_return's convention.
                            if self
                                .function_by_name
                                .contains_key(&format!("{}.{}", canon, m))
                            {
                                return true;
                            }
                            sig = lookup_std_module_call(canon, m);
                        }
                    }
                }
                // A std-module miss falls back to the member table (e.g.
                // `alias.map(...)` where the method is a builtin), then to
                // the dynamic checks below.
                sig.or_else(|| lookup_member_call(m))
            }
            _ => None,
        };
        if let Some(sig) = sig {
            return match sig.ret {
                ReturnConvention::Owned => true,
                ReturnConvention::Borrowed | ReturnConvention::Primitive => false,
                // `set(dict, ..)` returns arg0 in place: result ownership
                // follows the argument expression's ownership.
                ReturnConvention::PassThrough(n) => ce
                    .args
                    .get(n)
                    .map(|a| self.expr_transfers_ownership(&a.value))
                    .unwrap_or(false),
            };
        }
        // Dynamic callables — not table material, all +1 when they hit.
        match &*ce.callee {
            Expression::IdentifierExpression(id) => {
                let name = id.name.as_str();
                // Extern FFI: primitives or fresh host-allocated strings.
                if self.ctx.extern_fn_indices.contains_key(name) {
                    return true;
                }
                // A callable bound name is a closure value (closures
                // return +1).
                if self.resolve(name).is_some() {
                    return true;
                }
                // Direct user function — bare, module-peer, or named import.
                if self.resolves_to_user_fn(name) {
                    return true;
                }
                // A type constructor lowers to a fresh dict literal.
                if self.ctx.type_fields.contains_key(name) {
                    return true;
                }
                false
            }
            // Only the UFCS member arm resolves user functions; a plain
            // member miss is a borrowed builtin method (first/last/get/...),
            // matching the replaced heuristic exactly.
            Expression::MemberExpression(me) if is_ufcs => {
                self.resolves_to_user_fn(me.property.as_str())
            }
            _ => false,
        }
    }

    /// Side-effect-free twin of `resolve` for diagnostics: answers "would
    /// `resolve` find a binding?" WITHOUT allocating an upvalue slot on first
    /// outer-scope reference. The parity logger must use this instead of
    /// `resolve` — an early upvalue capture from the logging path would
    /// reorder env-slot indices and make FAI_ABI_CHECK builds emit different
    /// wasm than unchecked builds, breaking the "logging only" contract.

    /// Log a divergence between the signature table and the heuristic for the
    /// statically-classified callables the table covers. Mirrors the
    /// heuristic's three dispatch arms exactly (bare identifier, UFCS member,
    /// plain member) — member-position names go through `lookup_member_call`,
    /// never the bare table, because the heuristic's member arms check the
    /// borrowed bare-globals first (so member `set`/`unwrap` are Borrowed
    /// there, unlike their bare forms). Dynamic callables (user fns, closures,
    /// type constructors, externs) are intentionally not in the static table;
    /// a table miss on those is expected and silent.

    /// Std-module host calls verified to return a FRESH owned (+1) object
    /// graph (each call allocates anew on the guest heap — host `reserve`
    /// or guest `RT_ALLOC_STRING`; a null result is a primitive no-op for
    /// RC). Classifying these as borrowed over-retains the result on bind
    /// and skips the operand mop-up — one leaked graph per call (the
    /// per-request `json.parse` / response-dict leak on servers, plan 116).
    /// Curated: only entries whose host/lowering code was checked. Methods
    /// returning borrowed views (array element reads, dict gets) must stay
    /// out.

    /// RC transfer test (plan 113 R2): does compiling `expr` leave an OWNED (+1)
    /// value on the stack that the consuming context should TRANSFER (take
    /// without retaining)? True for fresh allocations (`is_fresh_value`) and for
    /// calls that return owned (`call_returns_owned`). Borrowed reads
    /// (identifier / field / index / borrowed-builtin call) return false → the
    /// consumer retains to co-own. Used at every bind / store / reassign /
    /// return / discard site in place of the bare `is_fresh_value`.
    fn expr_transfers_ownership(&mut self, expr: &Expression) -> bool {
        if Self::is_fresh_value(expr) {
            return true;
        }
        if let Expression::CallExpression(ce) = expr {
            return self.call_returns_owned(ce);
        }
        // `x!` yields the same value as `x` — ownership classification
        // passes through to the unwrapped expression. Without this, an
        // owned-returning optional call (`json.queryPage(...)!`,
        // `json.requireString(...)!`) classified as borrowed gets an extra
        // retain on bind and its host-allocated +1 is never released.
        if let Expression::ForceUnwrapExpression(fe) = expr {
            return self.expr_transfers_ownership(&fe.expression);
        }
        // A function REFERENCE — an identifier that resolves to no binding
        // but names a top-level function — compiles to a FRESH closure
        // wrapper per use (`compile_function_reference`), an owned +1.
        // Classified borrowed it gets retained on bind / at spawn and the
        // wrapper leaks once per use (one per request in forui's
        // `renderToString(App, path)`, plan 114 tail).
        if let Expression::IdentifierExpression(id) = expr {
            if self.resolve(&id.name).is_none() && self.resolves_to_user_fn(&id.name) {
                return true;
            }
        }
        false
    }

    /// `s == 'literal'` / `s != 'literal'` (either operand order) where
    /// the non-literal side is statically a (non-optional) `String`.
    /// Emits a direct `rt_str_eq` of the dynamic operand's bytes against
    /// the literal's interned data-section bytes — no `rt_alloc_string`
    /// for the literal, no `rt_eq` dispatch, no `rt_release`. Returns
    /// `Some(RawBool)` when it handled the comparison, `None` to fall
    /// back. Requires the dynamic side to be exactly `String`: an
    /// optional `String?` could be null, whose NaN-box bits must not be
    /// masked into a heap address.
    fn try_compile_string_literal_eq(
        &mut self,
        be: &BinaryExpression,
    ) -> Result<Option<ValueShape>, BuildError> {
        use fai_checker::types::Type;
        if be.operator != "==" && be.operator != "!=" {
            return Ok(None);
        }
        // Identify exactly one literal operand and a non-literal,
        // statically-String dynamic operand.
        let is_str_lit = |e: &Expression| matches!(e, Expression::StringExpression(_));
        let (dynamic, literal) = match (&*be.left, &*be.right) {
            (l, r) if is_str_lit(r) && !is_str_lit(l) => (&be.left, r),
            (l, r) if is_str_lit(l) && !is_str_lit(r) => (&be.right, l),
            _ => return Ok(None),
        };
        if !matches!(self.expression_type_at(dynamic), Some(Type::String)) {
            return Ok(None);
        }
        let Expression::StringExpression(lit) = literal else {
            return Ok(None);
        };
        let (lit_off, lit_len) = self.ctx.strings.borrow_mut().intern(&lit.value);

        // Evaluate the dynamic operand; release it after the read if it
        // is an owned temp (rt_str_eq only reads — mirrors the boxed
        // operand mop-up in the generic path).
        self.compile_expr_as(dynamic, ValueShape::Boxed)?;
        let owned = self.expr_transfers_ownership(dynamic);
        let val_local = self.alloc_local();
        self.emit(Instruction::LocalTee(val_local));
        // addr = obj_addr(dynamic) inline (mask NaN-box tag bits, wrap)
        self.emit(Instruction::I64Const(0x0000_FFFF_FFFF_FFFF));
        self.emit(Instruction::I64And);
        self.emit(Instruction::I32WrapI64);
        let addr = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(addr));
        // rt_str_eq(ptr_a = addr+8, len_a = mem[addr+4], ptr_b = lit_off, len_b)
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::I32Const(lit_off as i32));
        self.emit(Instruction::I32Const(lit_len as i32));
        self.emit(Instruction::Call(self.rt().base + RT_STR_EQ));
        if be.operator == "!=" {
            self.emit(Instruction::I32Eqz);
        }
        if owned {
            // Result (i32) is independent of the operand; stash it,
            // release the operand temp, restore the result.
            let res = self.alloc_i32_local();
            self.emit(Instruction::LocalSet(res));
            self.emit(Instruction::LocalGet(val_local));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            self.emit(Instruction::LocalGet(res));
        }
        Ok(Some(ValueShape::RawBool))
    }

    fn compile_binary(&mut self, be: &BinaryExpression) -> Result<ValueShape, BuildError> {
        // Short-circuit Bool ops. The checker enforces Bool operands,
        // so the non-evaluated side is never touched at runtime —
        // patterns like `x? and x!.field == 42` rely on this.
        if be.operator == "and" || be.operator == "or" {
            return self.compile_short_circuit(be);
        }

        // String == / != against a string literal: compare bytes
        // directly against the interned data-section bytes instead of
        // allocating a fresh String object for the literal (and then
        // releasing it) on every evaluation. Hot in router-style
        // dispatch (`if path == '/users'`).
        if let Some(shape) = self.try_compile_string_literal_eq(be)? {
            return Ok(shape);
        }

        let left_numeric = self.numeric_shape_for_expr(&be.left);
        let right_numeric = self.numeric_shape_for_expr(&be.right);
        if left_numeric == Some(ValueShape::RawInt) && right_numeric == Some(ValueShape::RawInt) {
            if be.operator == "/" {
                self.compile_numeric_expr_as_float(&be.left)?;
                self.compile_numeric_expr_as_float(&be.right)?;
                self.emit(Instruction::F64Div);
                return Ok(ValueShape::RawFloat);
            }
            let native = match be.operator.as_str() {
                "+" => Some((Instruction::I64Add, ValueShape::RawInt)),
                "-" => Some((Instruction::I64Sub, ValueShape::RawInt)),
                "*" => Some((Instruction::I64Mul, ValueShape::RawInt)),
                "//" => Some((Instruction::I64DivS, ValueShape::RawInt)),
                "%" => Some((Instruction::I64RemS, ValueShape::RawInt)),
                "==" => Some((Instruction::I64Eq, ValueShape::RawBool)),
                "!=" => Some((Instruction::I64Ne, ValueShape::RawBool)),
                "<" => Some((Instruction::I64LtS, ValueShape::RawBool)),
                "<=" => Some((Instruction::I64LeS, ValueShape::RawBool)),
                ">" => Some((Instruction::I64GtS, ValueShape::RawBool)),
                ">=" => Some((Instruction::I64GeS, ValueShape::RawBool)),
                _ => None,
            };
            if let Some((instruction, result_shape)) = native {
                self.compile_expr_as(&be.left, ValueShape::RawInt)?;
                self.compile_expr_as(&be.right, ValueShape::RawInt)?;
                self.emit(instruction);
                return Ok(result_shape);
            }
        }
        if matches!(
            left_numeric,
            Some(ValueShape::RawInt | ValueShape::RawFloat)
        ) && matches!(
            right_numeric,
            Some(ValueShape::RawInt | ValueShape::RawFloat)
        ) && (left_numeric == Some(ValueShape::RawFloat)
            || right_numeric == Some(ValueShape::RawFloat))
        {
            let native = match be.operator.as_str() {
                "+" => Some((Instruction::F64Add, ValueShape::RawFloat)),
                "-" => Some((Instruction::F64Sub, ValueShape::RawFloat)),
                "*" => Some((Instruction::F64Mul, ValueShape::RawFloat)),
                "/" => Some((Instruction::F64Div, ValueShape::RawFloat)),
                "==" => Some((Instruction::F64Eq, ValueShape::RawBool)),
                "!=" => Some((Instruction::F64Ne, ValueShape::RawBool)),
                "<" => Some((Instruction::F64Lt, ValueShape::RawBool)),
                "<=" => Some((Instruction::F64Le, ValueShape::RawBool)),
                ">" => Some((Instruction::F64Gt, ValueShape::RawBool)),
                ">=" => Some((Instruction::F64Ge, ValueShape::RawBool)),
                _ => None,
            };
            if let Some((instruction, result_shape)) = native {
                self.compile_numeric_expr_as_float(&be.left)?;
                self.compile_numeric_expr_as_float(&be.right)?;
                self.emit(instruction);
                return Ok(result_shape);
            }
        }

        // Boxed operand path (string concat, generic +, comparisons on objects).
        // The op helpers READ their operands and return a fresh/primitive result
        // (concat builds a new string; comparisons return a bool) — they never
        // take ownership of an operand. So an OWNED operand temp (a fresh literal
        // or an owned call result, e.g. `'x' + toString(i)`) would leak after the
        // op unless released. Stash each owned operand (mirrors compile_call's
        // owned_arg_stashes) and release it after the call; the result is left on
        // the stack. Borrowed operands (identifiers/fields) own nothing, so they
        // are not stashed (releasing them would free something the caller still
        // owns). Plan 115 / arg-temp mop-up.
        self.compile_expr_as(&be.left, ValueShape::Boxed)?;
        let left_stash = if self.expr_transfers_ownership(&be.left) {
            let t = self.alloc_local();
            self.emit(Instruction::LocalTee(t));
            Some(t)
        } else {
            None
        };
        self.compile_expr_as(&be.right, ValueShape::Boxed)?;
        let right_stash = if self.expr_transfers_ownership(&be.right) {
            let t = self.alloc_local();
            self.emit(Instruction::LocalTee(t));
            Some(t)
        } else {
            None
        };
        let helper = match be.operator.as_str() {
            "+" => RT_ADD,
            "-" => RT_SUB,
            "*" => RT_MUL,
            "/" => RT_DIV,
            "//" => RT_IDIV,
            "%" => RT_MOD,
            "**" => RT_POW,
            "==" => RT_EQ,
            "!=" => RT_NE,
            "<" => RT_LT,
            "<=" => RT_LE,
            ">" => RT_GT,
            ">=" => RT_GE,
            other => return Err(BuildError::UnknownBinaryOp(other.to_string())),
        };
        self.emit(Instruction::Call(self.rt().base + helper));
        // Result is on the stack; release the owned operand temps beneath it.
        // RT_RELEASE is stack-neutral (pops only its own i64 arg).
        for stash in [left_stash, right_stash].into_iter().flatten() {
            self.emit(Instruction::LocalGet(stash));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
        Ok(ValueShape::Boxed)
    }

    fn compile_unary(&mut self, ue: &UnaryExpression) -> Result<ValueShape, BuildError> {
        match ue.operator.as_str() {
            "-" => {
                match self.numeric_shape_for_expr(&ue.expression) {
                    Some(ValueShape::RawInt) => {
                        self.emit(Instruction::I64Const(0));
                        self.compile_expr_as(&ue.expression, ValueShape::RawInt)?;
                        self.emit(Instruction::I64Sub);
                        return Ok(ValueShape::RawInt);
                    }
                    Some(ValueShape::RawFloat) => {
                        self.compile_numeric_expr_as_float(&ue.expression)?;
                        self.emit(Instruction::F64Neg);
                        return Ok(ValueShape::RawFloat);
                    }
                    _ => {}
                }
                self.compile_expr_as(&ue.expression, ValueShape::Boxed)?;
                self.emit(Instruction::Call(self.rt().base + RT_NEG));
                Ok(ValueShape::Boxed)
            }
            "!" | "not" => {
                // Invert truthiness: the helper leaves an i32 0/1 on
                // the stack, so flip it and re-box as Bool.
                self.compile_truthy_i32(&ue.expression)?;
                self.emit(Instruction::I32Eqz);
                self.emit(Instruction::Call(self.rt().base + RT_MAKE_BOOL));
                Ok(ValueShape::Boxed)
            }
            other => Err(BuildError::UnknownUnaryOp(other.to_string())),
        }
    }

    /// Assemble the final wasm function with all collected local
    /// declarations. Counts runs of consecutive same-type locals so
    /// the encoding stays compact.
    fn finish(self) -> Function {
        // Group into runs of consecutive same-type locals — wasm's
        // function header takes `(count, type)` pairs and the local
        // indices we already returned from `alloc_local` /
        // `alloc_i32_local` assume allocation order matches
        // declaration order.
        let mut locals: Vec<(u32, ValType)> = Vec::new();
        for &ty in &self.local_decls {
            if let Some(last) = locals.last_mut() {
                if last.1 == ty {
                    last.0 += 1;
                    continue;
                }
            }
            locals.push((1, ty));
        }
        let mut f = Function::new(locals);
        for i in &self.instrs {
            f.instruction(i);
        }
        f.instruction(&Instruction::End);
        f
    }
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
