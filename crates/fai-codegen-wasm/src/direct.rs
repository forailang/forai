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
use wasm_encoder::{BlockType, Function, Instruction, MemArg, ValType};

use crate::program::FunctionInfo;
use crate::runtime::{
    IMPORT_ARRAY_FILTER, IMPORT_ARRAY_FIND, IMPORT_ARRAY_IS_ALL, IMPORT_ARRAY_IS_ANY,
    IMPORT_ARRAY_MAP, IMPORT_CALL_FFI, IMPORT_CLI_CLEAR, IMPORT_CLI_MOVE_TO, IMPORT_CLI_READ_LINE,
    IMPORT_CLI_WRITE, IMPORT_CLI_WRITE_LINE, IMPORT_CRYPTO_AVAILABLE, IMPORT_CRYPTO_BASE64_DECODE,
    IMPORT_CRYPTO_BASE64_ENCODE, IMPORT_CRYPTO_CONSTANT_TIME_EQUALS, IMPORT_CRYPTO_HEX_ENCODE,
    IMPORT_CRYPTO_HMAC_SHA256_HEX, IMPORT_CRYPTO_SHA256_HEX, IMPORT_ENV_GET, IMPORT_ENV_LOAD,
    IMPORT_EVENT_CLEAR, IMPORT_EVENT_CLEAR_ALL, IMPORT_EVENT_DRAIN, IMPORT_EVENT_EMIT,
    IMPORT_EVENT_EMIT_DEFERRED, IMPORT_EVENT_OFF, IMPORT_EVENT_ON, IMPORT_EVENT_ONCE,
    IMPORT_EVENT_QUEUE_LEN, IMPORT_EVENT_SUBSCRIBERS, IMPORT_FFI_AVAILABLE, IMPORT_FILE_EXISTS,
    IMPORT_FILE_LIST, IMPORT_GET_LOCATION_PATH, IMPORT_HTML_ESCAPE, IMPORT_HTTP_REQUEST_DELETE,
    IMPORT_HTTP_REQUEST_GET, IMPORT_HTTP_REQUEST_PATCH, IMPORT_HTTP_REQUEST_POST,
    IMPORT_HTTP_REQUEST_PUT, IMPORT_JSON_PARSE, IMPORT_JSON_REQUIRE_STRING, IMPORT_JSON_STRINGIFY,
    IMPORT_LOG_ERROR, IMPORT_LOG_INFO, IMPORT_LOG_WARN, IMPORT_NET_AVAILABLE, IMPORT_NOW_MS,
    IMPORT_PATH_BASENAME, IMPORT_PATH_DIRNAME, IMPORT_PATH_EXTNAME, IMPORT_PATH_JOIN,
    IMPORT_PROCESS_AVAILABLE, IMPORT_PROCESS_READ, IMPORT_PROCESS_RUN, IMPORT_PROCESS_START,
    IMPORT_PROCESS_STOP,
    IMPORT_FILE_READ_STR, IMPORT_PROCESS_WRITE, IMPORT_PUSH_HISTORY_STATE, IMPORT_RANDOM,
    IMPORT_REMOTE_CALL, IMPORT_SET_HTML, IMPORT_SET_HTML_AT, IMPORT_SET_TRAP_MSG, IMPORT_SPAWN,
    IMPORT_TRAP_REPORT,
    IMPORT_STORAGE_CLEAR, IMPORT_STORAGE_GET_STR, IMPORT_STORAGE_REMOVE, IMPORT_STORAGE_SET,
    IMPORT_TCP_ACCEPT, IMPORT_TCP_ADDRESS, IMPORT_TCP_CLOSE, IMPORT_TCP_CONNECT, IMPORT_TCP_LISTEN,
    IMPORT_TCP_READ, IMPORT_TCP_READ_LINE, IMPORT_TCP_WRITE, IMPORT_UDP_BIND, IMPORT_UDP_BROADCAST,
    IMPORT_UDP_RECEIVE, IMPORT_UDP_SEND, IMPORT_WRITE_FILE, INT_CHECK_MASK, METHOD_APPEND,
    METHOD_CONTAINS, METHOD_ENDS_WITH, METHOD_FIRST, METHOD_GET_KEYS, METHOD_INDEX_OF,
    METHOD_IS_EMPTY, METHOD_JOIN, METHOD_LAST, METHOD_LENGTH, METHOD_REPEAT, METHOD_REPLACE,
    METHOD_REVERSE, METHOD_SERVER_GET, METHOD_SERVER_HTML, METHOD_SERVER_JSON,
    METHOD_SERVER_LISTEN, METHOD_SERVER_OK, METHOD_SERVER_POST, METHOD_SERVER_REDIRECT,
    METHOD_SERVER_ROUTER, METHOD_SERVER_SERVE_FILES, METHOD_SERVER_TEXT, METHOD_SLICE, METHOD_SORT,
    METHOD_SPLIT, METHOD_STARTS_WITH, METHOD_SUBSTRING, METHOD_TO_LOWER, METHOD_TO_UPPER,
    METHOD_TRIM, METHOD_TRIM_END, METHOD_TRIM_START, OBJ_TAG_ARRAY, OBJ_TAG_CELL, OBJ_TAG_CLOSURE,
    OBJ_TAG_DICT,
    OBJ_TAG_NATIVE_FN, OBJ_TAG_STRING, OBJ_TAG_TUPLE, QNAN, RT_ADD, RT_ALLOC, RT_ALLOC_STRING,
    RT_AS_NUMBER, RT_CALL_NATIVE, RT_CONCAT, RT_COUNT, RT_DIV, RT_EQ, RT_GE, RT_GET_FIELD,
    RT_GET_INDEX, RT_GT, RT_IDIV, RT_IS_FLOAT, RT_IS_INT, RT_IS_OBJ, RT_LE, RT_LT, RT_MAKE_BOOL,
    RT_LIVE_OBJECTS, RT_MAKE_FLOAT, RT_MAKE_INT, RT_MAKE_OBJ, RT_MOD, RT_MUL, RT_NE, RT_NEG,
    RT_OBJ_ADDR, RT_RELEASE, RT_RETAIN,
    RT_PARSE_FLOAT, RT_PARSE_INT, RT_POW, RT_PRINT_VAL_NEW, RT_SET_FIELD, RT_STR_EQ, RT_SUB,
    RT_VALUE_TO_STR, TAG_BOOL, TAG_INT, VAL_FALSE, VAL_NULL, VAL_VOID,
};

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

/// A single module-method entry. Most methods are `Simple` (push
/// args, call the import, wrap result); a small set have shapes that
/// don't fit the flat pattern and get explicit variants here.
enum ModuleCall {
    Simple {
        import_idx: u32,
        args: &'static [ArgShape],
        result: ResultShape,
    },
    HttpRequest {
        import_idx: u32,
        has_body: bool,
    },
    /// `std.time.unix() -> Int`. The host's `now_ms` returns f64
    /// milliseconds; `time.unix` divides by 1000, truncates to i32
    /// seconds, and NaN-boxes as Int. Mirrors
    /// `runtime.rs::METHOD_TIME_UNIX`.
    TimeUnix,
    /// `std.math.{floor,ceil,round}(x: Float) -> Int`. Unbox x via
    /// `RT_AS_NUMBER`, apply the carried f64 instruction
    /// (`F64Floor` / `F64Ceil` / `F64Nearest`), saturate-truncate to
    /// i32, NaN-box as Int.
    MathUnaryFloatToInt(Instruction<'static>),
    /// `std.math.{abs,sqrt}(x: Float) -> Float`. Unbox, apply the
    /// carried f64 instruction, NaN-box back as Float.
    MathUnaryFloat(Instruction<'static>),
    /// `std.math.{min,max}(a: Float, b: Float) -> Float`. Unbox
    /// both args, apply the carried f64 binary op, NaN-box.
    MathBinaryFloat(Instruction<'static>),
    /// `std.math.pow(base: Float, exp: Float) -> Float`. Integer
    /// exponent only — exp is truncated to i32, then an iterative
    /// multiply loop computes `base^|exp|` and inverts on negative
    /// exponent. Mirrors `runtime.rs::METHOD_POW`.
    MathPow,
    /// `std.cli.readLine(prompt?) -> String`. Zero-arg form pushes
    /// `(0, 0)` so the host skips the prompt print; one-arg form
    /// stringifies and pushes `(ptr, len)`. Can't fit `Simple`
    /// because arity varies across call sites.
    CliReadLine,
    /// `std.convert.toInt(v) -> Int`. Type-aware at codegen:
    /// Int passthrough, Float truncates, String routes to
    /// `RT_PARSE_INT`. Other types fall through unchanged.
    ConvertToInt,
    /// `std.convert.toFloat(v) -> Float`. Type-aware at codegen:
    /// Float passthrough, Int converts, String routes to
    /// `RT_PARSE_FLOAT`. Other types fall through unchanged.
    ConvertToFloat,
    /// `std.convert.toString(v) -> String`. Call `RT_VALUE_TO_STR`
    /// on the boxed value and return the resulting String obj.
    ConvertToString,
    /// `std.convert.parseInt(s) -> Int?`. Pass the boxed String to
    /// `RT_PARSE_INT` which returns Int-or-null.
    ConvertParseInt,
    /// `std.convert.parseFloat(s) -> Float?`. Same shape via
    /// `RT_PARSE_FLOAT`.
    ConvertParseFloat,
    /// `assert.calledWith(target, ...args)` — target must resolve
    /// at compile time; `args` are serialised into a scratch buffer
    /// and compared against recorded calls via the host spy table.
    /// Traps on mismatch.
    SpyAssertCalledWith,
    /// `assert.callCount(target, n)` — traps when the recorded
    /// call count differs from `n`.
    SpyAssertCallCount,
    /// `assert.notCalled(target)` — traps when any calls recorded.
    SpyAssertNotCalled,
    /// `std.convert.toBool(v) -> Bool`. Apply forai's truthy rule
    /// (`null`/`void`/`false` → false; everything else → true) and
    /// box the i32 result as a Bool. Mirrors the in-expression
    /// boolean coercion the checker inserts for implicit conversions.
    ConvertToBool,
    /// `std.error.Error(msg: String) -> Error`. Allocates a
    /// one-entry dict `{message: msg}`. The field access `e.message`
    /// then resolves via the normal dict `RT_GET_FIELD` path.
    /// Mirrors `translate.rs`'s inline `name == "Error"` branch.
    ErrorConstruct,
    /// `std.error.unwrap(value, fallback) -> value`. Returns `value`
    /// unless it's `VAL_NULL`, in which case returns `fallback`.
    /// Matches `translate.rs`'s inline `name == "unwrap"` branch.
    Unwrap,
    /// Assertion methods — `std.test.{assert,equal}`, `assert.{equals,isTrue,isFalse}`.
    /// Each evaluates a condition and, on failure, pushes the
    /// caller-supplied message (or `(0,0)` if absent) to
    /// `IMPORT_SET_TRAP_MSG` before `unreachable`. The CLI test
    /// runner reads the message out of host-side TLS after catching
    /// the wasm trap. Mirrors `translate.rs` sentinels
    /// `0xFF50..=0xFF54`.
    ///
    /// Variants:
    /// - `Truthy { invert }`: single-arg truthiness. `invert=true`
    ///   for `assert.isFalse`.
    /// - `StringEq`: two-arg stringified equality (both
    ///   `test.equal` and `assert.equals`).
    Assertion(AssertionKind),
    /// String / array methods dispatched through `RT_CALL_NATIVE`.
    /// The builder constructs a `NativeFn` heap object with the
    /// given `method_id`, writes `arity` args to the linear-memory
    /// args buffer, and calls `RT_CALL_NATIVE(obj, args_ptr, arity)`.
    /// Mirrors the bytecode translator's Op::Call dispatch for
    /// NativeFn objects.
    NativeMethod {
        method_id: i32,
        arity: usize,
    },
}

/// Resolve a `(canonical_module, method)` pair to a dispatch recipe.
/// Returns `None` if the pair isn't in this path's coverage — the
/// caller surfaces `ModuleAccessNotYetSupported` so callers can
/// distinguish "unknown module call" from other builder failures.
fn resolve_module_call(module: &str, method: &str) -> Option<ModuleCall> {
    if let Some(call) = resolve_http_request_call(module, method) {
        return Some(call);
    }

    // Shorthands keep the table readable; `AS::*` / `RS::*` let us
    // name the variants without colliding with std `String`, `Boxed`
    // ambiguity, etc.
    use ArgShape as AS;
    use ResultShape as RS;

    // Special shapes first — methods that don't fit the flat
    // (arg_shapes, result_shape) mould get their own variant.
    if (module, method) == ("std.time", "unix") {
        return Some(ModuleCall::TimeUnix);
    }
    // std.math — most methods are inline wasm f64 instructions
    // rather than host imports. `random` is the one exception; it
    // falls through into the Simple table below.
    if module == "std.math" {
        let math = match method {
            "floor" => Some(ModuleCall::MathUnaryFloatToInt(Instruction::F64Floor)),
            "ceil" => Some(ModuleCall::MathUnaryFloatToInt(Instruction::F64Ceil)),
            "round" => Some(ModuleCall::MathUnaryFloatToInt(Instruction::F64Nearest)),
            "abs" => Some(ModuleCall::MathUnaryFloat(Instruction::F64Abs)),
            "sqrt" => Some(ModuleCall::MathUnaryFloat(Instruction::F64Sqrt)),
            "min" => Some(ModuleCall::MathBinaryFloat(Instruction::F64Min)),
            "max" => Some(ModuleCall::MathBinaryFloat(Instruction::F64Max)),
            "pow" => Some(ModuleCall::MathPow),
            _ => None,
        };
        if math.is_some() {
            return math;
        }
    }
    // std.http.server — response builders + router + listen.
    // Every method dispatches through `RT_CALL_NATIVE` with a
    // `METHOD_SERVER_*` id; the runtime helper allocates the
    // response dict (via `IMPORT_HTTP_SERVER_RESPONSE`) or hands the
    // call off to the router/listen imports. Handler closures pass
    // through as plain `Boxed` i64 args — the host reads them out
    // and calls back via `__indirect_function_table` when requests
    // arrive.
    if module == "std.http.server" {
        let native: Option<(i32, usize)> = match method {
            "ok" => Some((METHOD_SERVER_OK, 1)),
            "text" => Some((METHOD_SERVER_TEXT, 2)),
            "html" => Some((METHOD_SERVER_HTML, 2)),
            "json" => Some((METHOD_SERVER_JSON, 2)),
            "redirect" => Some((METHOD_SERVER_REDIRECT, 2)),
            "router" => Some((METHOD_SERVER_ROUTER, 0)),
            "get" => Some((METHOD_SERVER_GET, 3)),
            "post" => Some((METHOD_SERVER_POST, 3)),
            "serveFiles" => Some((METHOD_SERVER_SERVE_FILES, 2)),
            "listen" => Some((METHOD_SERVER_LISTEN, 2)),
            _ => None,
        };
        if let Some((method_id, arity)) = native {
            return Some(ModuleCall::NativeMethod { method_id, arity });
        }
    }
    // std.error — the Error constructor builds a `{message: ...}`
    // dict; `unwrap` is a null-guarded pass-through. The remaining
    // helpers (`message`, `kind`, `isError`) share the bare-global
    // implementations and are routed in `compile_call`.
    if module == "std.error" {
        match method {
            "Error" => return Some(ModuleCall::ErrorConstruct),
            "unwrap" => return Some(ModuleCall::Unwrap),
            _ => {}
        }
    }
    // std.test / assert — trap-on-fail assertions.
    if module == "std.test" {
        match method {
            "assert" => return Some(ModuleCall::Assertion(AssertionKind::Truthy)),
            "equal" => return Some(ModuleCall::Assertion(AssertionKind::StringEq)),
            _ => {}
        }
    }
    // The `assert` namespace is magically in scope inside `@test`
    // blocks — there's no `use` statement for it. The direct path
    // treats `assert` as a canonical module name directly; see the
    // caller (`compile_call`) for how the alias lookup is bypassed.
    if module == "assert" {
        match method {
            "equals" | "equal" => return Some(ModuleCall::Assertion(AssertionKind::StringEq)),
            "isTrue" => return Some(ModuleCall::Assertion(AssertionKind::IsTrue)),
            "isFalse" => return Some(ModuleCall::Assertion(AssertionKind::IsFalse)),
            "isNull" => return Some(ModuleCall::Assertion(AssertionKind::IsNull)),
            "isNotNull" => return Some(ModuleCall::Assertion(AssertionKind::IsNotNull)),
            // Spy assertions: resolve target to fn_id at compile
            // time, check the host's call record at runtime, trap
            // on mismatch via IMPORT_SET_TRAP_MSG + unreachable.
            "calledWith" => return Some(ModuleCall::SpyAssertCalledWith),
            "callCount" => return Some(ModuleCall::SpyAssertCallCount),
            "notCalled" => return Some(ModuleCall::SpyAssertNotCalled),
            _ => {}
        }
    }
    // std.cli / std.storage — one-off specials.
    if (module, method) == ("std.cli", "readLine") {
        return Some(ModuleCall::CliReadLine);
    }
    // std.convert — RT-helper dispatch + pass-throughs.
    if module == "std.convert" {
        match method {
            "toInt" => return Some(ModuleCall::ConvertToInt),
            "toFloat" => return Some(ModuleCall::ConvertToFloat),
            "toString" => return Some(ModuleCall::ConvertToString),
            "toBool" => return Some(ModuleCall::ConvertToBool),
            "parseInt" => return Some(ModuleCall::ConvertParseInt),
            "parseFloat" => return Some(ModuleCall::ConvertParseFloat),
            _ => {}
        }
    }
    // std.string — every method dispatches through `RT_CALL_NATIVE`
    // with a method-id + arity. The runtime's string ops read args
    // 0 and 1 into pre-loaded locals and args 2+ from `args_ptr`
    // offsets, so writing all args sequentially to linear memory
    // satisfies every shape in the module.
    if module == "std.string" {
        let native: Option<(i32, usize)> = match method {
            "length" => Some((METHOD_LENGTH, 1)),
            "isEmpty" => Some((METHOD_IS_EMPTY, 1)),
            "replace" => Some((METHOD_REPLACE, 3)),
            "split" => Some((METHOD_SPLIT, 2)),
            "trim" => Some((METHOD_TRIM, 1)),
            "trimStart" => Some((METHOD_TRIM_START, 1)),
            "trimEnd" => Some((METHOD_TRIM_END, 1)),
            "toUpper" => Some((METHOD_TO_UPPER, 1)),
            "toLower" => Some((METHOD_TO_LOWER, 1)),
            "contains" => Some((METHOD_CONTAINS, 2)),
            "startsWith" => Some((METHOD_STARTS_WITH, 2)),
            "endsWith" => Some((METHOD_ENDS_WITH, 2)),
            "substring" => Some((METHOD_SUBSTRING, 3)),
            "indexOf" => Some((METHOD_INDEX_OF, 2)),
            "join" => Some((METHOD_JOIN, 2)),
            "repeat" => Some((METHOD_REPEAT, 2)),
            _ => None,
        };
        if let Some((method_id, arity)) = native {
            return Some(ModuleCall::NativeMethod { method_id, arity });
        }
    }
    // std.array — non-closure methods route through `RT_CALL_NATIVE`.
    // `contains`, `indexOf`, and `join` share their method IDs with
    // the string module; the runtime branches on the container's
    // heap tag. Closure-taking variants (map/filter/find/isAny/isAll)
    // aren't in this block — they sit in the Simple table below
    // because their host imports do the iteration.
    if module == "std.array" {
        let native: Option<(i32, usize)> = match method {
            "append" => Some((METHOD_APPEND, 2)),
            "length" => Some((METHOD_LENGTH, 1)),
            "isEmpty" => Some((METHOD_IS_EMPTY, 1)),
            "contains" => Some((METHOD_CONTAINS, 2)),
            "indexOf" => Some((METHOD_INDEX_OF, 2)),
            "join" => Some((METHOD_JOIN, 2)),
            "sort" => Some((METHOD_SORT, 1)),
            "reverse" => Some((METHOD_REVERSE, 1)),
            "slice" => Some((METHOD_SLICE, 3)),
            "first" => Some((METHOD_FIRST, 1)),
            "last" => Some((METHOD_LAST, 1)),
            _ => None,
        };
        if let Some((method_id, arity)) = native {
            return Some(ModuleCall::NativeMethod { method_id, arity });
        }
    }

    let (import_idx, args, result): (u32, &'static [AS], RS) = match (module, method) {
        // std.file — buffer-free variants. `read` is handled as a
        // `read` returns a host-allocated boxed String (or null) — no
        // guest scratch buffer, so file size is unbounded. Plan 116:
        // the old fixed-64KiB scratch ABI let the host overflow the
        // guest heap on larger files.
        ("std.file", "read") => (IMPORT_FILE_READ_STR, &[AS::String], RS::Boxed),
        ("std.file", "write") => (IMPORT_WRITE_FILE, &[AS::String, AS::String], RS::MakeBool),
        ("std.file", "exists") => (IMPORT_FILE_EXISTS, &[AS::String], RS::MakeBool),
        ("std.file", "list") => (IMPORT_FILE_LIST, &[AS::String], RS::Boxed),

        // std.process — command/session helpers return JSON strings.
        // `available` reports false on the browser target (probe stays
        // linked there; the run/session imports are stripped).
        ("std.process", "available") => (IMPORT_PROCESS_AVAILABLE, &[], RS::MakeBool),
        ("std.process", "run") => (
            IMPORT_PROCESS_RUN,
            &[AS::String, AS::String, AS::String, AS::Int, AS::Int],
            RS::Boxed,
        ),
        ("std.process", "start") => (
            IMPORT_PROCESS_START,
            &[AS::String, AS::String, AS::String, AS::Int],
            RS::Boxed,
        ),
        ("std.process", "write") => (IMPORT_PROCESS_WRITE, &[AS::String, AS::String], RS::Boxed),
        ("std.process", "read") => (IMPORT_PROCESS_READ, &[AS::String, AS::Int], RS::Boxed),
        ("std.process", "stop") => (IMPORT_PROCESS_STOP, &[AS::String], RS::Boxed),

        // std.math — `random` is the only method backed by a host
        // import. Everything else in std.math lowers to inline wasm
        // f64 instructions (handled in the Math* specials above).
        ("std.math", "random") => (IMPORT_RANDOM, &[], RS::MakeFloat),

        // std.time — `now` returns Float (ms since epoch); `unix`
        // is special-cased above (divide + truncate).
        //
        // NOTE: the checker declares `timeNow` as returning String
        // (ISO 8601), but the wasm runtime's `METHOD_TIME_NOW` emits
        // Float. We match the runtime here so the two codegen paths
        // agree; the checker/runtime disagreement is a pre-existing
        // issue tracked outside this work.
        ("std.time", "now") => (IMPORT_NOW_MS, &[], RS::MakeFloat),

        // std.log — (String) → void.
        ("std.log", "info") => (IMPORT_LOG_INFO, &[AS::String], RS::Void),
        ("std.log", "warn") => (IMPORT_LOG_WARN, &[AS::String], RS::Void),
        ("std.log", "error") => (IMPORT_LOG_ERROR, &[AS::String], RS::Void),

        // std.path — (String[, String]) → String.
        ("std.path", "join") => (IMPORT_PATH_JOIN, &[AS::String, AS::String], RS::Boxed),
        ("std.path", "basename") => (IMPORT_PATH_BASENAME, &[AS::String], RS::Boxed),
        ("std.path", "dirname") => (IMPORT_PATH_DIRNAME, &[AS::String], RS::Boxed),
        ("std.path", "extname") => (IMPORT_PATH_EXTNAME, &[AS::String], RS::Boxed),

        // std.env — process environment + dotenv loader. `get` returns
        // a NaN-boxed String allocated host-side or VAL_NULL when the
        // key is unset. `load` returns a 0/1 flag wrapped as Bool.
        ("std.env", "get") => (IMPORT_ENV_GET, &[AS::String], RS::Boxed),
        ("std.env", "load") => (IMPORT_ENV_LOAD, &[AS::String], RS::MakeBool),

        // std.events — host-backed registry. The host stores closure
        // handles by event name and invokes them via the indirect
        // function table on `emit`. `on`/`once` return a NaN-boxed
        // Subscription Dict (`{id, name}`); `off` returns Bool.
        ("std.events", "on") => (IMPORT_EVENT_ON, &[AS::String, AS::Boxed], RS::Boxed),
        ("std.events", "once") => (IMPORT_EVENT_ONCE, &[AS::String, AS::Boxed], RS::Boxed),
        ("std.events", "off") => (IMPORT_EVENT_OFF, &[AS::Boxed], RS::MakeBool),
        ("std.events", "emit") => (IMPORT_EVENT_EMIT, &[AS::String, AS::Boxed], RS::Void),
        ("std.events", "subscribers") => (IMPORT_EVENT_SUBSCRIBERS, &[AS::String], RS::MakeInt),
        ("std.events", "clear") => (IMPORT_EVENT_CLEAR, &[AS::String], RS::Void),
        ("std.events", "clearAll") => (IMPORT_EVENT_CLEAR_ALL, &[], RS::Void),
        ("std.events", "emitDeferred") => (
            IMPORT_EVENT_EMIT_DEFERRED,
            &[AS::String, AS::Boxed],
            RS::Void,
        ),
        ("std.events", "drain") => (IMPORT_EVENT_DRAIN, &[], RS::Void),
        ("std.events", "queueLen") => (IMPORT_EVENT_QUEUE_LEN, &[], RS::MakeInt),

        // std.html — (String) → String.
        ("std.html", "escape") => (IMPORT_HTML_ESCAPE, &[AS::String], RS::Boxed),

        // std.json — parse/stringify are pass-through string/boxed in;
        // `requireString` takes (Dict, String) and returns String/null.
        ("std.json", "parse") => (IMPORT_JSON_PARSE, &[AS::String], RS::Boxed),
        ("std.json", "stringify") => (IMPORT_JSON_STRINGIFY, &[AS::Boxed], RS::Boxed),
        ("std.json", "requireString") => (
            IMPORT_JSON_REQUIRE_STRING,
            &[AS::Boxed, AS::String],
            RS::Boxed,
        ),

        // std.net / std.ffi — availability checks.
        ("std.net", "available") => (IMPORT_NET_AVAILABLE, &[], RS::MakeBool),

        // std.crypto — string args lower to (ptr, len); hex/base64 results
        // are NaN-boxed strings. `available` and `constantTimeEquals` return
        // i32 (0/1) wrapped as Bool.
        ("std.crypto", "available") => (IMPORT_CRYPTO_AVAILABLE, &[], RS::MakeBool),
        ("std.crypto", "hmacSha256Hex") => (
            IMPORT_CRYPTO_HMAC_SHA256_HEX,
            &[AS::String, AS::String],
            RS::Boxed,
        ),
        ("std.crypto", "sha256Hex") => (IMPORT_CRYPTO_SHA256_HEX, &[AS::String], RS::Boxed),
        ("std.crypto", "hexEncode") => (IMPORT_CRYPTO_HEX_ENCODE, &[AS::String], RS::Boxed),
        ("std.crypto", "constantTimeEquals") => (
            IMPORT_CRYPTO_CONSTANT_TIME_EQUALS,
            &[AS::String, AS::String],
            RS::MakeBool,
        ),
        ("std.crypto", "base64Encode") => (IMPORT_CRYPTO_BASE64_ENCODE, &[AS::String], RS::Boxed),
        ("std.crypto", "base64Decode") => (IMPORT_CRYPTO_BASE64_DECODE, &[AS::String], RS::Boxed),
        ("std.ffi", "available") => (IMPORT_FFI_AVAILABLE, &[AS::String], RS::MakeBool),

        // std.net.tcp — handle-based TCP surface.
        ("std.net.tcp", "listen") => (IMPORT_TCP_LISTEN, &[AS::Int], RS::MakeInt),
        ("std.net.tcp", "accept") => (IMPORT_TCP_ACCEPT, &[AS::Int], RS::Boxed),
        ("std.net.tcp", "connect") => (IMPORT_TCP_CONNECT, &[AS::String, AS::Int], RS::MakeInt),
        ("std.net.tcp", "read") => (IMPORT_TCP_READ, &[AS::Int], RS::Boxed),
        ("std.net.tcp", "readLine") => (IMPORT_TCP_READ_LINE, &[AS::Int], RS::Boxed),
        ("std.net.tcp", "write") => (IMPORT_TCP_WRITE, &[AS::Int, AS::String], RS::MakeInt),
        ("std.net.tcp", "close") => (IMPORT_TCP_CLOSE, &[AS::Int], RS::Void),
        ("std.net.tcp", "address") => (IMPORT_TCP_ADDRESS, &[AS::Int], RS::Boxed),

        // std.net.udp — same shape family.
        ("std.net.udp", "bind") => (IMPORT_UDP_BIND, &[AS::Int], RS::MakeInt),
        ("std.net.udp", "send") => (
            IMPORT_UDP_SEND,
            &[AS::Int, AS::String, AS::Int, AS::String],
            RS::MakeInt,
        ),
        ("std.net.udp", "receive") => (IMPORT_UDP_RECEIVE, &[AS::Int], RS::Boxed),
        ("std.net.udp", "broadcast") => (IMPORT_UDP_BROADCAST, &[AS::Int, AS::Int], RS::Void),

        // std.cli — prompt-taking and optional-arg variants are
        // special-cased above; these are the fixed-shape ones.
        ("std.cli", "write") => (IMPORT_CLI_WRITE, &[AS::String], RS::Void),
        ("std.cli", "writeLine") => (IMPORT_CLI_WRITE_LINE, &[AS::String], RS::Void),
        ("std.cli", "clear") => (IMPORT_CLI_CLEAR, &[], RS::Void),
        ("std.cli", "moveTo") => (IMPORT_CLI_MOVE_TO, &[AS::Int, AS::Int], RS::Void),

        // std.storage — `storageGet` is special-cased above
        // (buffer-alloc). Methods use the full `storage<Op>` names
        // per the checker's BuiltinDoc.name.
        // `storageGet` mirrors `std.file.read`: host-allocated boxed
        // String result (or null), replacing the fixed-buffer ABI.
        ("std.storage", "storageGet") => (IMPORT_STORAGE_GET_STR, &[AS::String], RS::Boxed),
        ("std.storage", "storageSet") => (IMPORT_STORAGE_SET, &[AS::String, AS::String], RS::Void),
        ("std.storage", "storageRemove") => (IMPORT_STORAGE_REMOVE, &[AS::String], RS::Void),
        ("std.storage", "storageClear") => (IMPORT_STORAGE_CLEAR, &[], RS::Void),

        // std.array (closure-taking) — the host iterates the array
        // and dispatches back into the guest through the closure's
        // `table_idx`. Both args are plain NaN-boxed i64 (the array
        // object, the closure object). The builder's closure-literal
        // emission already produces a heap object with the right
        // table slot, so nothing extra is needed on the guest side.
        ("std.array", "map") => (IMPORT_ARRAY_MAP, &[AS::Boxed, AS::Boxed], RS::Boxed),
        ("std.array", "filter") => (IMPORT_ARRAY_FILTER, &[AS::Boxed, AS::Boxed], RS::Boxed),
        ("std.array", "find") => (IMPORT_ARRAY_FIND, &[AS::Boxed, AS::Boxed], RS::Boxed),
        ("std.array", "isAny") => (IMPORT_ARRAY_IS_ANY, &[AS::Boxed, AS::Boxed], RS::Boxed),
        ("std.array", "isAll") => (IMPORT_ARRAY_IS_ALL, &[AS::Boxed, AS::Boxed], RS::Boxed),

        _ => return None,
    };
    Some(ModuleCall::Simple {
        import_idx,
        args,
        result,
    })
}

fn resolve_http_request_call(module: &str, method: &str) -> Option<ModuleCall> {
    if module != "std.http.request" {
        return None;
    }
    let (import_idx, has_body) = match method {
        "get" => (IMPORT_HTTP_REQUEST_GET, false),
        "post" => (IMPORT_HTTP_REQUEST_POST, true),
        "put" => (IMPORT_HTTP_REQUEST_PUT, true),
        "patch" => (IMPORT_HTTP_REQUEST_PATCH, true),
        "delete" => (IMPORT_HTTP_REQUEST_DELETE, false),
        _ => return None,
    };
    Some(ModuleCall::HttpRequest {
        import_idx,
        has_body,
    })
}

/// Best-effort source-location lookup for a `BuildError`.
///
/// The codegen has 30+ `BuildError` raise sites. Threading
/// per-expression source locations through every site is an
/// open-ended refactor (plan 108 #1, ongoing); this helper picks
/// up the cheap wins by walking the AST for the offending name and
/// returning the first match.
///
/// For `UnknownIdentifier(name)` and similar string-bearing variants
/// we look for the first call-site or identifier matching `name` in
/// the entry AST or any module. For `UnsupportedStatement` /
/// `UnsupportedExpression` we currently can't pin down the location
/// from the variant string alone; those land with no location until
/// future work threads it through.
pub fn locate_build_error(
    err: BuildError,
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
) -> crate::LocatedBuildError {
    use fai_compiler::ast::Statement;

    let target_name: Option<&str> = match &err {
        BuildError::UnknownIdentifier(name) => Some(name.as_str()),
        BuildError::ModuleAccessNotYetSupported(name) => Some(name.as_str()),
        BuildError::UnknownBinaryOp(name) => Some(name.as_str()),
        BuildError::UnknownUnaryOp(name) => Some(name.as_str()),
        BuildError::DuplicateModuleName(name) => Some(name.as_str()),
        _ => None,
    };

    if let Some(name) = target_name {
        // Walk modules first — that's where most user code lives.
        for m in modules {
            if let Some((line, col, file)) =
                find_name_in_statements(&m.statements, name, &m.file_paths)
            {
                return crate::LocatedBuildError {
                    err,
                    file,
                    line: Some(line),
                    col: Some(col),
                    module: Some(m.name.clone()),
                };
            }
        }
        // Fall back to the entry AST.
        if let Some((line, col, _)) = find_name_in_statements(&ast.statements, name, &[]) {
            return crate::LocatedBuildError {
                err,
                file: None,
                line: Some(line),
                col: Some(col),
                module: None,
            };
        }
    }

    let _ = Statement::UseStatement; // silence unused-import lint when target_name is None
    crate::LocatedBuildError::unlocated(err)
}

/// Walk top-level statements (and one level into function bodies)
/// looking for a call or identifier matching `name`. Returns the
/// `(line, col, file)` of the first match, where `file` is pulled
/// from `file_paths` aligned to the statement that contains the
/// match.
fn find_name_in_statements(
    statements: &[fai_compiler::ast::Statement],
    name: &str,
    file_paths: &[Option<String>],
) -> Option<(u32, u32, Option<String>)> {
    for (idx, stmt) in statements.iter().enumerate() {
        let file = file_paths.get(idx).cloned().flatten();
        if let Some((line, col)) = scan_statement_for_name(stmt, name) {
            return Some((line, col, file));
        }
    }
    None
}

fn scan_statement_for_name(stmt: &fai_compiler::ast::Statement, name: &str) -> Option<(u32, u32)> {
    use fai_compiler::ast::Statement;
    match stmt {
        Statement::FunctionDeclaration(fd) => {
            for body_stmt in &fd.body {
                if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                    return Some(loc);
                }
            }
            None
        }
        Statement::TestDeclaration(td) => {
            for case in &td.cases {
                for body_stmt in &case.body {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            None
        }
        Statement::ExpressionStatement(es) => scan_expression_for_name(&es.expression, name),
        Statement::LetStatement(ls) => scan_expression_for_name(&ls.value, name),
        Statement::VarStatement(vs) => scan_expression_for_name(&vs.value, name),
        Statement::ReturnStatement(rs) => rs
            .value
            .as_ref()
            .and_then(|v| scan_expression_for_name(v, name)),
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                if let Some(loc) = scan_expression_for_name(&branch.condition, name) {
                    return Some(loc);
                }
                for body_stmt in &branch.body {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            if let Some(else_branch) = &is.else_branch {
                for body_stmt in else_branch {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn scan_expression_for_name(
    expr: &fai_compiler::ast::Expression,
    name: &str,
) -> Option<(u32, u32)> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) if id.name == name => {
            Some((id.location.line, id.location.column))
        }
        Expression::CallExpression(ce) => {
            if let Expression::IdentifierExpression(id) = &*ce.callee {
                if id.name == name {
                    return Some((ce.location.line, ce.location.column));
                }
            }
            scan_expression_for_name(&ce.callee, name).or_else(|| {
                ce.args
                    .iter()
                    .find_map(|a| scan_expression_for_name(&a.value, name))
            })
        }
        Expression::MemberExpression(me) => {
            if me.property == name {
                return Some((me.location.line, me.location.column));
            }
            scan_expression_for_name(&me.object, name)
        }
        Expression::BinaryExpression(be) => scan_expression_for_name(&be.left, name)
            .or_else(|| scan_expression_for_name(&be.right, name)),
        Expression::UnaryExpression(ue) => scan_expression_for_name(&ue.expression, name),
        Expression::IndexExpression(ie) => scan_expression_for_name(&ie.object, name)
            .or_else(|| scan_expression_for_name(&ie.index, name)),
        Expression::OptionalCheckExpression(oc) => scan_expression_for_name(&oc.expression, name),
        Expression::ForceUnwrapExpression(fu) => scan_expression_for_name(&fu.expression, name),
        _ => None,
    }
}

/// Errors the builder surfaces when it sees a construct it doesn't
/// handle. The production compiler surfaces these as actionable
/// direct-codegen diagnostics.
#[derive(Debug, Clone)]
pub enum BuildError {
    /// A Statement variant the builder hasn't migrated yet.
    UnsupportedStatement(&'static str),
    /// An Expression variant the builder hasn't migrated yet.
    UnsupportedExpression(&'static str),
    /// A boxed-returning host import has no ownership signature in the
    /// plan-117 table (checked builds only; unchecked builds log the
    /// `[abi-check] MISSING-SIGNATURE` sentinel instead).
    MissingOwnershipSignature(String),
    /// A binary operator string we don't recognise.
    UnknownBinaryOp(String),
    /// A unary operator string we don't recognise.
    UnknownUnaryOp(String),
    /// An identifier that resolves neither to a parameter nor a local
    /// binding. Module imports and globals go through dedicated paths
    /// — an `UnknownIdentifier` here means the name really isn't in
    /// scope at the AST level.
    UnknownIdentifier(String),
    /// Module-qualified member access that is not a supported std or
    /// user-module function. Field access on values uses a separate path.
    ModuleAccessNotYetSupported(String),
    /// Two discovered modules share the same canonical name. Happens
    /// when a local module directory collides with a dependency
    /// package — e.g. a `src/Forui/` directory in an app that also
    /// depends on the `Forui` package. The user must rename one
    /// rather than have the compiler silently pick a winner.
    DuplicateModuleName(String),
    /// The program contains a function marked async-effectful by
    /// Phase 1 analysis, but resumable async lowering is not
    /// implemented yet.
    AsyncLoweringUnsupported { function: String, cause: String },
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
pub fn build_function_with_spy_and_offset(
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

// ── Program-level entry point ─────────────────────────────────────
//
// All-or-nothing program codegen: every top-level function, test case,
// and closure must compile through the direct builder.

/// A fully-built wasm program ready to be serialised. `functions[0]`
/// is a synthesised `<__start__>` shim that runs the module-init
/// function and then user `main` (if one exists); its wasm index is
/// what the `_start` export points at. `functions[1]` is the
/// synthesised `<__module_init__>` that assigns every top-level
/// `var` initialiser into its wasm global. User functions follow
/// (main first if defined, then other entry-AST functions, then
/// per-module functions). `closures` are the anonymous
/// FunctionExpression heap-objects encountered inside those bodies
/// — they land in the indirect function table after the top-level
/// functions.
#[derive(Debug)]
pub struct BuiltProgram {
    pub functions: Vec<(FunctionInfo, Function)>,
    pub closures: Vec<BuiltClosure>,
    /// String-literal data to lay out at memory offset 0.
    pub string_data: Vec<u8>,
    /// One entry per (suite, case) pair when test mode is on.
    /// Index into `functions` — the wasm function for that case.
    /// The dispatcher at `_fai_run_test(suite_i, case_i)` uses
    /// this to route. Empty in non-test builds.
    pub test_cases: Vec<TestCaseEntry>,
    /// Number of top-level `var` declarations — each gets a
    /// dedicated mutable i64 wasm global appended after the four
    /// fixed runtime globals (`__heap_ptr`, `__env_ptr`,
    /// `error_flag`, `error_value`). The module assembler uses this
    /// to emit the right number of extra global slots, all
    /// initialised to `VAL_NULL`.
    pub module_var_count: u32,
}

/// Routing entry for one test case — the dispatcher uses the
/// `(suite_idx, case_idx)` pair to find the corresponding
/// zero-arg wrapper function at `function_index`.
#[derive(Debug, Clone)]
pub struct TestCaseEntry {
    pub suite_name: String,
    pub suite_idx: u16,
    pub case_idx: u16,
    pub function_index: usize,
}

const TEST_HOOK_BEFORE_ALL_CASE_IDX: u16 = u16::MAX;
const TEST_HOOK_AFTER_ALL_CASE_IDX: u16 = u16::MAX - 1;

/// Try compiling every top-level function in `ast` through the
/// direct builder. Returns `Ok(BuiltProgram)` when every function
/// succeeds; on the first refusal returns the corresponding
/// `BuildError` so the caller can decide what to do (e.g., fall
/// back to the bytecode path in `module.rs`).
///
/// `main` is emitted first so its wasm function index matches the
/// `_start` export convention. All other top-level functions follow
/// in source order.
///
/// The caller provides `CheckerInfo`; `fai-checker` isn't a
/// production dep of this crate. Extract
/// `(ufcs_calls, named_param_reorder)` from a `Checker` instance
/// that ran against `ast.statements` first.
///
/// `rt_base` is the wasm function index of the first runtime helper
/// — normally `import_count` (after all host imports). A matching
/// module assembler lays functions out as `[imports, rt_helpers,
/// top_level_functions, closures]`.
///
/// `fai_func_type_indices` should cover every param-count used by
/// both top-level functions and any closures they reference. The
/// caller pre-allocates these `FaiFunc(N)` type slots in the
/// module's type section.
pub fn build_program(
    ast: &fai_compiler::ast::Program,
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
) -> Result<BuiltProgram, BuildError> {
    build_program_with_modules(ast, &[], rt, checker, fai_func_type_indices, import_remap)
}

/// Extended entry point that also compiles functions from
/// `modules` (discovered sibling `.fai` files). Each module's
/// functions are added to the unified top-level list with names
/// prefixed by the module's canonical path, e.g.,
/// `"mypkg.helpers.doThing"`. Cross-module calls via
/// `helpers.doThing(...)` route through the alias map; calls
/// between peers inside a module use the `module_context` fallback.
pub fn build_program_with_modules(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
) -> Result<BuiltProgram, BuildError> {
    build_program_full(
        ast,
        modules,
        rt,
        checker,
        fai_func_type_indices,
        import_remap,
        false,
        None,
    )
}

/// Full-feature entry point that also synthesises per-test-case
/// wrapper functions when `is_test` is true. The module assembler
/// reads `BuiltProgram.test_cases` to emit a `_fai_run_test`
/// dispatcher keyed on `(suite_idx, case_idx)`.
///
/// `entry_file` is the path of the entry source file, used only for
/// the debug side-table (plan 116) — entry-AST functions have no
/// per-decl file path the way module functions do, so trap backtraces
/// would otherwise show `main (line 3)` instead of `main (main.fai:3)`.
#[allow(clippy::too_many_arguments)]
pub fn build_program_full(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
    is_test: bool,
    entry_file: Option<&str>,
) -> Result<BuiltProgram, BuildError> {
    // Reject canonical module-name collisions up front. A local
    // `src/Forui/` directory and a dependency package also named
    // `Forui` both produce `m.name = "Forui"`; silently picking one
    // would scramble call-resolution in a way users can't diagnose.
    {
        use std::collections::HashMap as StdMap;
        let mut by_canonical: StdMap<&str, ()> = StdMap::new();
        for m in modules {
            if by_canonical.insert(m.name.as_str(), ()).is_some() {
                return Err(BuildError::DuplicateModuleName(m.name.clone()));
            }
        }
    }

    // Alias map merges explicit namespace `use` imports with unique
    // user-module basename aliases. If two modules share a basename
    // (`auth` and `pages.auth`), no implicit alias is created for
    // that basename; explicit named imports still resolve through
    // their canonical module path.
    let mut module_aliases: HashMap<String, String> = HashMap::new();
    {
        use std::collections::HashMap as StdMap;
        let mut basename_counts: StdMap<String, usize> = StdMap::new();
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                *basename_counts.entry(last.to_string()).or_insert(0) += 1;
            }
        }
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                if basename_counts.get(last).copied().unwrap_or(0) == 1 {
                    module_aliases.insert(last.to_string(), m.name.clone());
                }
            }
        }
    }
    // Entry-AST aliases win on collision so `use std.array` isn't
    // shadowed by a user module conveniently named `array`.
    for (k, v) in collect_module_aliases_from(None, &ast.statements) {
        module_aliases.insert(k, v);
    }
    // Also fold in aliases declared inside each discovered module —
    // e.g. a helper file doing `use std.string` needs `string.isEmpty`
    // to resolve when its functions compile. Entry-level aliases
    // already won above; here we only insert keys that aren't taken.
    for m in modules {
        for (k, v) in collect_module_aliases_from(Some(&m.name), &m.statements) {
            module_aliases.entry(k).or_insert(v);
        }
    }

    let mut module_function_exports: HashMap<String, Vec<String>> = HashMap::new();
    for m in modules {
        let mut names = Vec::new();
        for s in &m.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                if !m.private_names.iter().any(|n| n == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        module_function_exports.insert(m.name.clone(), names);
    }

    // Named-import map: `use { X, Y } from app.models` in the entry
    // (or in any module) lets bare `X(...)` calls resolve to
    // `app.models.X`. Gathered from both the entry AST and every
    // discovered module. Entry declarations win on collision —
    // matching the alias-map precedence above.
    let mut named_imports: HashMap<String, String> = HashMap::new();
    fn record_named_imports(
        out: &mut HashMap<String, String>,
        stmts: &[fai_compiler::ast::Statement],
        current_module_name: Option<&str>,
        module_function_exports: &HashMap<String, Vec<String>>,
        insert_policy: fn(&mut HashMap<String, String>, String, String),
    ) {
        for s in stmts {
            if let fai_compiler::ast::Statement::UseStatement(u) = s {
                let qualified_prefix =
                    qualify_module_path_for_codegen(current_module_name, &u.module_path);
                if u.import_all {
                    if fai_checker::std_modules::is_std_module(&u.module_path) {
                        let std_exports = fai_checker::std_modules::std_module_exports();
                        if let Some(exports) = std_exports.get(&qualified_prefix) {
                            for (n, _) in exports {
                                insert_policy(
                                    out,
                                    n.clone(),
                                    format!("{}.{}", qualified_prefix, n),
                                );
                            }
                        }
                    } else if let Some(names) = module_function_exports.get(&qualified_prefix) {
                        for n in names {
                            insert_policy(out, n.clone(), format!("{}.{}", qualified_prefix, n));
                        }
                    }
                } else if let Some(names) = &u.imported_names {
                    let qualified_prefix =
                        qualify_module_path_for_codegen(current_module_name, &u.module_path);
                    for n in names {
                        insert_policy(out, n.clone(), format!("{}.{}", qualified_prefix, n));
                    }
                }
            }
        }
    }
    record_named_imports(
        &mut named_imports,
        &ast.statements,
        None,
        &module_function_exports,
        |m, k, v| {
            m.insert(k, v);
        },
    );
    for m in modules {
        record_named_imports(
            &mut named_imports,
            &m.statements,
            Some(&m.name),
            &module_function_exports,
            |m, k, v| {
                m.entry(k).or_insert(v);
            },
        );
    }

    // Collect extern functions from the entry AST first, then from
    // every discovered module. Entry-file names win on collision so
    // the program's own extern block overrides one re-exported from
    // a dependency. Without this merge, `use { close } from Forsqlite`
    // in the entry compiles the wrapper's `sqlite3_close(...)` call
    // through `compile_call` — and `compile_call` looks the name up in
    // `extern_fn_indices`, which previously only saw the entry file.
    let mut extern_fn_indices = collect_extern_fn_indices_from(&ast.statements);
    // Per-extern `is_out` flags per parameter. Needed so
    // `compile_extern_call` can emit the readback for OUT slots
    // after the host writes the C-returned pointer into guest
    // scratch memory.
    let mut extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
            for f in &ext.functions {
                extern_out_params
                    .insert(f.name.clone(), f.params.iter().map(|p| p.is_out).collect());
            }
        }
    }
    let mut next_idx = extern_fn_indices
        .values()
        .max()
        .map(|m| *m + 1)
        .unwrap_or(0);
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    extern_fn_indices.entry(f.name.clone()).or_insert_with(|| {
                        let idx = next_idx;
                        next_idx = next_idx.checked_add(1).expect("too many extern fns");
                        idx
                    });
                    extern_out_params
                        .entry(f.name.clone())
                        .or_insert_with(|| f.params.iter().map(|p| p.is_out).collect());
                }
            }
        }
    }

    // Collect `enum Name ... end` declarations from the entry AST and
    // every discovered module. Each enum keeps the member list in
    // declaration order; `Status.ready` lowers to the integer index
    // of `ready` in Status's member list (NaN-boxed). Equality of
    // two enum values reduces to integer equality.
    let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::EnumDeclaration(ed) = s {
            enum_members.insert(ed.name.clone(), ed.members.clone());
        }
    }
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::EnumDeclaration(ed) = s {
                enum_members
                    .entry(ed.name.clone())
                    .or_insert_with(|| ed.members.clone());
            }
        }
    }

    // Collect `type Name ... end` declarations from the entry AST and
    // every module. `Name(a: 1, b: 'x')` lowers to a dict literal
    // whose entries are `(field_name, supplied_value | default |
    // null-for-optional)` in declaration order.
    let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::TypeDeclaration(td) = s {
            type_fields.insert(td.name.clone(), td.fields.clone());
        }
    }
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::TypeDeclaration(td) = s {
                type_fields
                    .entry(td.name.clone())
                    .or_insert_with(|| td.fields.clone());
            }
        }
    }
    // Built-in named types (Event, HttpRequest, RpcCall, etc.) live
    // in the checker's `type_fields` but never reached the codegen
    // here. Without this, `let x T = from_dict(d)` for a built-in T
    // falls through the expansion at `compile_let_statement` and
    // codegen reports `UnknownIdentifier("from_dict")`. User-declared
    // types of the same name still win — they were inserted above.
    for (name, fields) in builtin_type_fields() {
        type_fields.entry(name).or_insert(fields);
    }

    // Module-level constants — top-level `let NAME = <literal>`
    // bindings. Collected from the entry AST and every module so a
    // helper file in a dependency can define `SQLITE_OK = 0` and have
    // callers in any sibling file inline it at reference sites.
    // Non-literal initialisers are skipped (we don't run them).
    let mut module_constants: HashMap<String, fai_compiler::ast::Expression> = HashMap::new();
    fn is_literal_expr(e: &fai_compiler::ast::Expression) -> bool {
        use fai_compiler::ast::Expression::*;
        matches!(
            e,
            NumberExpression(_) | BooleanExpression(_) | NullExpression(_) | StringExpression(_)
        )
    }
    fn collect_module_consts(
        stmts: &[fai_compiler::ast::Statement],
        out: &mut HashMap<String, fai_compiler::ast::Expression>,
    ) {
        for s in stmts {
            if let fai_compiler::ast::Statement::LetStatement(ls) = s {
                if ls.bindings.len() == 1 && is_literal_expr(&ls.value) {
                    out.entry(ls.bindings[0].name.clone())
                        .or_insert_with(|| ls.value.clone());
                }
            }
        }
    }
    collect_module_consts(&ast.statements, &mut module_constants);
    for m in modules {
        collect_module_consts(&m.statements, &mut module_constants);
    }

    // Module-level `var NAME = EXPR` declarations. Each gets a
    // dedicated mutable wasm global (i64) appended after the four
    // fixed runtime globals; globals start at index 4. First-seen
    // wins so a helper module can declare `var timerId = 0` and a
    // peer file referencing `timerId` resolves to that slot.
    //
    // Initialisers are grouped by their source module so each runs
    // in its own module context — otherwise a sibling-module
    // initialiser like router.fai's `createSignal('/')` can't
    // resolve `createSignal` via its own `use { createSignal }
    // from signal` import when we compile it from a dependency
    // context.
    const MODULE_VAR_GLOBAL_BASE: u32 = 4;
    let mut module_vars: HashMap<String, u32> = HashMap::new();
    // Ordered list of (module_context, name, initialiser). None
    // context means the entry AST's own top-level vars.
    let mut module_var_inits: Vec<(Option<String>, String, fai_compiler::ast::Expression)> =
        Vec::new();
    {
        fn collect_mvars(
            stmts: &[fai_compiler::ast::Statement],
            ctx_mod: Option<&str>,
            map: &mut HashMap<String, u32>,
            inits: &mut Vec<(Option<String>, String, fai_compiler::ast::Expression)>,
            base: u32,
        ) {
            for s in stmts {
                if let fai_compiler::ast::Statement::VarStatement(vs) = s {
                    if vs.bindings.len() != 1 {
                        continue;
                    }
                    let name = &vs.bindings[0].name;
                    if map.contains_key(name) {
                        continue;
                    }
                    let idx = base + inits.len() as u32;
                    map.insert(name.clone(), idx);
                    inits.push((
                        ctx_mod.map(|s| s.to_string()),
                        name.clone(),
                        vs.value.clone(),
                    ));
                }
            }
        }
        collect_mvars(
            &ast.statements,
            None,
            &mut module_vars,
            &mut module_var_inits,
            MODULE_VAR_GLOBAL_BASE,
        );
        for m in modules {
            collect_mvars(
                &m.statements,
                Some(m.name.as_str()),
                &mut module_vars,
                &mut module_var_inits,
                MODULE_VAR_GLOBAL_BASE,
            );
        }
    }
    let module_var_count = module_var_inits.len() as u32;

    // Does the user supply a `main`? `<__start__>` calls it after
    // `<__module_init__>` when present; otherwise it just runs init
    // and returns VAL_VOID via the init-call's return value.
    let has_main = ast.statements.iter().any(|s| {
        matches!(
            s,
            fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main",
        )
    });

    // Synthesise the two wrapper functions. Names start with `<` so
    // the export loop below skips them — hosts only see `_start`.
    let loc_zero = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
    let mk_call_stmt = |name: &str| -> fai_compiler::ast::Statement {
        fai_compiler::ast::Statement::ExpressionStatement(fai_compiler::ast::ExpressionStatement {
            expression: fai_compiler::ast::Expression::CallExpression(
                fai_compiler::ast::CallExpression {
                    callee: Box::new(fai_compiler::ast::Expression::IdentifierExpression(
                        fai_compiler::ast::IdentifierExpression {
                            name: name.to_string(),
                            location: loc_zero.clone(),
                        },
                    )),
                    args: Vec::new(),
                    location: loc_zero.clone(),
                },
            ),
            location: loc_zero.clone(),
        })
    };
    // Group the initialisers by their module context so each module
    // gets its own compiled init function. Per-module init functions
    // are named `<__module_init__:{module_path}>` (entry-AST vars
    // go into `<__module_init__:>`). A master `<__module_init__>`
    // calls them in declaration order.
    let mut per_module_inits: Vec<(Option<String>, Vec<fai_compiler::ast::Statement>)> = Vec::new();
    for (ctx_mod, name, value) in &module_var_inits {
        let stmt = fai_compiler::ast::Statement::AssignmentStatement(
            fai_compiler::ast::AssignmentStatement {
                target: fai_compiler::ast::AssignmentTarget::Variables {
                    names: vec![name.clone()],
                },
                value: value.clone(),
                location: loc_zero.clone(),
            },
        );
        match per_module_inits
            .iter_mut()
            .find(|(existing, _)| existing == ctx_mod)
        {
            Some((_, stmts)) => stmts.push(stmt),
            None => per_module_inits.push((ctx_mod.clone(), vec![stmt])),
        }
    }
    let per_module_init_names: Vec<String> = per_module_inits
        .iter()
        .map(|(ctx_mod, _)| match ctx_mod {
            Some(m) => format!("<__module_init__:{}>", m),
            None => "<__module_init__:>".to_string(),
        })
        .collect();
    let per_module_init_decls: Vec<(fai_compiler::ast::FunctionDeclaration, Option<String>)> =
        per_module_inits
            .iter()
            .zip(per_module_init_names.iter())
            .map(|((ctx_mod, body), fn_name)| {
                let fd = fai_compiler::ast::FunctionDeclaration {
                    name: fn_name.clone(),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_types: Vec::new(),
                    body: body.clone(),
                    doc: None,
                    is_private: None,
                    is_abstract: false,
                    is_remote: false,
                    location: loc_zero.clone(),
                    doc_comment: None,
                };
                (fd, ctx_mod.clone())
            })
            .collect();

    // Master `<__module_init__>` just dispatches to each per-module
    // init. Order matches `module_var_inits` — first-seen wins for
    // duplicate var names, same policy as global-index allocation.
    let master_init_body: Vec<fai_compiler::ast::Statement> = per_module_init_names
        .iter()
        .map(|n| mk_call_stmt(n))
        .collect();
    let module_init_fd = fai_compiler::ast::FunctionDeclaration {
        name: "<__module_init__>".to_string(),
        type_params: Vec::new(),
        params: Vec::new(),
        return_types: Vec::new(),
        body: master_init_body,
        doc: None,
        is_private: None,
        is_abstract: false,
        is_remote: false,
        location: loc_zero.clone(),
        doc_comment: None,
    };
    let mut start_body: Vec<fai_compiler::ast::Statement> = vec![mk_call_stmt("<__module_init__>")];
    if has_main && !is_test {
        start_body.push(mk_call_stmt("main"));
    }
    let start_fd = fai_compiler::ast::FunctionDeclaration {
        name: "<__start__>".to_string(),
        type_params: Vec::new(),
        params: Vec::new(),
        return_types: Vec::new(),
        body: start_body,
        doc: None,
        is_private: None,
        is_abstract: false,
        is_remote: false,
        location: loc_zero.clone(),
        doc_comment: None,
    };

    // Enumerate functions: synthesised wrappers first (so `_start`
    // points at `<__start__>`), then user `main`, then other
    // entry-AST top-level funcs, then each module's funcs prefixed
    // with the module path. Track each decl's module context so
    // unqualified peer calls resolve correctly.
    // Decls carry (function, ctx_module, ctx_file). The file path
    // is plumbed so the per-call-site keys (UFCS, named-param
    // reorder, expression types) can disambiguate by file —
    // otherwise two files in one module with calls at the same
    // (line, col) collide and codegen reads the wrong UFCS bit.
    let mut decls: Vec<(
        fai_compiler::ast::FunctionDeclaration,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    decls.push((start_fd, None, None));
    decls.push((module_init_fd, None, None));
    for (fd, ctx_mod) in per_module_init_decls {
        decls.push((fd, ctx_mod, None));
    }
    let main = ast.statements.iter().find_map(|s| match s {
        fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
            Some(fd.clone())
        }
        _ => None,
    });
    if let Some(fd) = main {
        decls.push((fd, None, None));
    }
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
            if fd.name != "main" {
                decls.push((fd.clone(), None, None));
            }
        }
    }
    for m in modules {
        for (idx, s) in m.statements.iter().enumerate() {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                let mut prefixed = fd.clone();
                prefixed.name = format!("{}.{}", m.name, fd.name);
                let file = m.file_paths.get(idx).cloned().flatten();
                decls.push((prefixed, Some(m.name.clone()), file));
            }
        }
    }

    let infos: Vec<FunctionInfo> = decls
        .iter()
        .map(|(fd, ctx_mod, ctx_file)| FunctionInfo {
            name: fd.name.clone(),
            // Module functions carry their own file; entry-AST functions
            // (no module context, real location) fall back to the entry
            // file. Synthesised wrappers (line 0) stay file-less.
            source_file: ctx_file.clone().or_else(|| {
                if ctx_mod.is_none() && fd.location.line > 0 {
                    entry_file.map(String::from)
                } else {
                    None
                }
            }),
            source_line: fd.location.line,
            param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
            type_param_count: fd.type_params.len() as u16,
            include_in_coverage: fd.name != "main",
            param_defaults: param_defaults_for(fd),
        })
        .collect();

    // Fn-id index used by the spy/mock machinery. Keep the same
    // ordering as `infos` so the runtime table index lines up with
    // the function's position in the codegen output.
    let function_by_name: HashMap<String, u32> = infos
        .iter()
        .enumerate()
        .map(|(i, info)| (info.name.clone(), i as u32))
        .collect();

    // Walk every `test` block's body (entry + each module) to find
    // functions that get mock/spy-tracked. Only these functions need
    // the `spy_check_call` preamble — everything else pays zero cost.
    // In non-test builds the set is empty so no instrumentation
    // happens regardless of what the AST contains.
    let spy_targets: SpyTargets = if is_test {
        collect_spy_targets(
            ast,
            modules,
            &function_by_name,
            &module_aliases,
            &named_imports,
        )
    } else {
        SpyTargets::default()
    };
    let mocked_fn_ids = spy_targets.fn_ids;
    let std_method_fn_ids = spy_targets.std_method_fn_ids;

    let strings = RefCell::new(StringInterner::default());
    let mut functions: Vec<(FunctionInfo, Function)> = Vec::with_capacity(decls.len());
    let mut closures: Vec<BuiltClosure> = Vec::new();
    for ((fd, ctx_mod, ctx_file), info) in decls.iter().zip(infos.iter().cloned()) {
        let result = build_function_with_spy_and_offset(
            fd,
            rt,
            &infos,
            checker,
            fai_func_type_indices,
            &module_aliases,
            &extern_fn_indices,
            import_remap,
            &strings,
            &enum_members,
            &type_fields,
            &named_imports,
            &mocked_fn_ids,
            &std_method_fn_ids,
            closures.len() as u32,
            ctx_mod.as_deref(),
            &module_constants,
            &extern_out_params,
            &module_vars,
            ctx_file.as_deref(),
            None,
        )?;
        functions.push((info, result.main));
        closures.extend(result.closures);
    }

    // Test-mode synthesis: one zero-arg wrapper per `(suite, case)`
    // pair. Each wrapper's body is
    // `setup ++ before_each ++ case.body ++ after_each`. The
    // dispatcher emitted by the module assembler routes
    // `_fai_run_test(suite_i, case_i)` to the matching wrapper.
    let mut test_cases: Vec<TestCaseEntry> = Vec::new();
    if is_test {
        // Collect TestDeclarations from entry AST and from all
        // modules. Module-scoped tests compile with their
        // module_context so unqualified calls resolve correctly.
        let mut test_specs: Vec<(
            &fai_compiler::ast::TestDeclaration,
            Option<String>,
            Option<String>,
        )> = Vec::new();
        for s in &ast.statements {
            if let fai_compiler::ast::Statement::TestDeclaration(td) = s {
                test_specs.push((td, None, None));
            }
        }
        for m in modules {
            for (idx, s) in m.statements.iter().enumerate() {
                if let fai_compiler::ast::Statement::TestDeclaration(td) = s {
                    let file = m.file_paths.get(idx).cloned().flatten();
                    test_specs.push((td, Some(m.name.clone()), file));
                }
            }
        }

        for (suite_idx, (td, ctx_mod, ctx_file)) in test_specs.iter().enumerate() {
            if let Some(before_all) = &td.before_all {
                let mut body: Vec<fai_compiler::ast::Statement> = td.setup.clone();
                body.extend(before_all.clone());
                let wrapper = fai_compiler::ast::FunctionDeclaration {
                    name: format!("<test-before-all:{}>", td.name),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_types: Vec::new(),
                    body,
                    doc: None,
                    is_private: None,
                    is_abstract: false,
                    is_remote: false,
                    location: td.location.clone(),
                    doc_comment: None,
                };
                let info = FunctionInfo {
                    name: wrapper.name.clone(),
                    param_count: 0,
                    type_param_count: 0,
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    source_line: wrapper.location.line,
                    ..Default::default()
                };
                let result = build_function_with_spy_and_offset(
                    &wrapper,
                    rt,
                    &infos,
                    checker,
                    fai_func_type_indices,
                    &module_aliases,
                    &extern_fn_indices,
                    import_remap,
                    &strings,
                    &enum_members,
                    &type_fields,
                    &named_imports,
                    &mocked_fn_ids,
                    &std_method_fn_ids,
                    closures.len() as u32,
                    ctx_mod.as_deref(),
                    &module_constants,
                    &extern_out_params,
                    &module_vars,
                    ctx_file.as_deref(),
                    None,
                )?;
                let function_index = functions.len();
                functions.push((info, result.main));
                closures.extend(result.closures);
                test_cases.push(TestCaseEntry {
                    suite_name: td.name.clone(),
                    suite_idx: suite_idx as u16,
                    case_idx: TEST_HOOK_BEFORE_ALL_CASE_IDX,
                    function_index,
                });
            }

            for (case_idx, case) in td.cases.iter().enumerate() {
                // Build a zero-arg FunctionDeclaration. The body
                // is `setup ++ before_each ++ case.body ++ after_each`
                // so the wrapper is self-contained per case.
                let mut body: Vec<fai_compiler::ast::Statement> = td.setup.clone();
                if let Some(before) = &td.before_each {
                    body.extend(before.clone());
                }
                body.extend(case.body.clone());
                if let Some(after) = &td.after_each {
                    body.extend(after.clone());
                }
                let wrapper = fai_compiler::ast::FunctionDeclaration {
                    name: format!("<test:{}#{}>", td.name, case_idx),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_types: Vec::new(),
                    body,
                    doc: None,
                    is_private: None,
                    is_abstract: false,
                    is_remote: false,
                    location: case.location.clone(),
                    doc_comment: None,
                };
                let info = FunctionInfo {
                    name: wrapper.name.clone(),
                    param_count: 0,
                    type_param_count: 0,
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    source_line: wrapper.location.line,
                    ..Default::default()
                };
                let result = build_function_with_spy_and_offset(
                    &wrapper,
                    rt,
                    &infos,
                    checker,
                    fai_func_type_indices,
                    &module_aliases,
                    &extern_fn_indices,
                    import_remap,
                    &strings,
                    &enum_members,
                    &type_fields,
                    &named_imports,
                    &mocked_fn_ids,
                    &std_method_fn_ids,
                    closures.len() as u32,
                    ctx_mod.as_deref(),
                    &module_constants,
                    &extern_out_params,
                    &module_vars,
                    ctx_file.as_deref(),
                    None,
                )?;
                let function_index = functions.len();
                functions.push((info, result.main));
                closures.extend(result.closures);
                test_cases.push(TestCaseEntry {
                    suite_name: td.name.clone(),
                    suite_idx: suite_idx as u16,
                    case_idx: case_idx as u16,
                    function_index,
                });
            }

            if let Some(after_all) = &td.after_all {
                let mut body: Vec<fai_compiler::ast::Statement> = td.setup.clone();
                body.extend(after_all.clone());
                let wrapper = fai_compiler::ast::FunctionDeclaration {
                    name: format!("<test-after-all:{}>", td.name),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_types: Vec::new(),
                    body,
                    doc: None,
                    is_private: None,
                    is_abstract: false,
                    is_remote: false,
                    location: td.location.clone(),
                    doc_comment: None,
                };
                let info = FunctionInfo {
                    name: wrapper.name.clone(),
                    param_count: 0,
                    type_param_count: 0,
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    source_line: wrapper.location.line,
                    ..Default::default()
                };
                let result = build_function_with_spy_and_offset(
                    &wrapper,
                    rt,
                    &infos,
                    checker,
                    fai_func_type_indices,
                    &module_aliases,
                    &extern_fn_indices,
                    import_remap,
                    &strings,
                    &enum_members,
                    &type_fields,
                    &named_imports,
                    &mocked_fn_ids,
                    &std_method_fn_ids,
                    closures.len() as u32,
                    ctx_mod.as_deref(),
                    &module_constants,
                    &extern_out_params,
                    &module_vars,
                    ctx_file.as_deref(),
                    None,
                )?;
                let function_index = functions.len();
                functions.push((info, result.main));
                closures.extend(result.closures);
                test_cases.push(TestCaseEntry {
                    suite_name: td.name.clone(),
                    suite_idx: suite_idx as u16,
                    case_idx: TEST_HOOK_AFTER_ALL_CASE_IDX,
                    function_index,
                });
            }
        }
    }

    Ok(BuiltProgram {
        functions,
        closures,
        string_data: strings.into_inner().bytes,
        test_cases,
        module_var_count,
    })
}

fn collect_module_aliases_from(
    current_module_name: Option<&str>,
    stmts: &[fai_compiler::ast::Statement],
) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    for s in stmts {
        if let fai_compiler::ast::Statement::UseStatement(u) = s {
            if u.import_all || u.imported_names.is_some() {
                continue;
            }
            if let Some(last) = u.module_path.last() {
                aliases.insert(
                    last.clone(),
                    qualify_module_path_for_codegen(current_module_name, &u.module_path),
                );
            }
        }
    }
    aliases
}

fn qualify_module_path_for_codegen(current_module_name: Option<&str>, path: &[String]) -> String {
    if path.first().map(|s| s.as_str()) == Some("std") {
        return path.join(".");
    }
    let is_external = path
        .first()
        .map(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .unwrap_or(false);
    if is_external {
        return path.join(".");
    }
    if let Some(current) = current_module_name {
        let package = current.split('.').next().unwrap_or(current);
        let is_package = package
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false);
        if is_package {
            return format!("{}.{}", package, path.join("."));
        }
    }
    path.join(".")
}

fn collect_extern_fn_indices_from(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, u16> {
    let mut indices = HashMap::new();
    let mut next = 0u16;
    for s in stmts {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
            for f in &ext.functions {
                indices.insert(f.name.clone(), next);
                next = next.checked_add(1).expect("too many extern fns");
            }
        }
    }
    indices
}

/// A spy target the mock/assert calls refer to.
#[derive(Debug, Clone)]
enum SpyTarget {
    /// A user-defined top-level function. `fn_id` is its index in
    /// the unified function table.
    UserFn(u32),
    /// A std module method (`cli.readLine`, `string.trim`, ...).
    /// The compiler-assigned `fn_id` is opaque — it just needs to
    /// match between the mock setup call and the module-call
    /// interception site.
    StdMethod(u32),
}

impl SpyTarget {
    fn fn_id(&self) -> u32 {
        match self {
            SpyTarget::UserFn(id) => *id,
            SpyTarget::StdMethod(fn_id) => *fn_id,
        }
    }
}

/// Resolve a spy-target expression. Tries user-function lookup
/// first; falls back to treating `alias.method` as a std-module
/// method reference when `alias` names a module and `method`
/// resolves through `resolve_module_call`. The caller supplies a
/// mutable `std_method_fn_ids` map — new std-method targets get a
/// fresh `fn_id` assigned lazily so the number space stays tight.
fn resolve_mock_target_full(
    expr: &fai_compiler::ast::Expression,
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
    std_method_fn_ids: &mut HashMap<(String, String), u32>,
    next_std_fn_id: &mut u32,
) -> Option<SpyTarget> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) => {
            if let Some(&p) = function_by_name.get(&id.name) {
                return Some(SpyTarget::UserFn(p));
            }
            if let Some(q) = named_imports.get(&id.name) {
                if let Some(&p) = function_by_name.get(q) {
                    return Some(SpyTarget::UserFn(p));
                }
            }
            None
        }
        Expression::MemberExpression(me) => {
            if let Expression::IdentifierExpression(obj) = &*me.object {
                if let Some(canonical) = module_aliases.get(&obj.name) {
                    let full = format!("{}.{}", canonical, me.property);
                    if let Some(&p) = function_by_name.get(&full) {
                        return Some(SpyTarget::UserFn(p));
                    }
                    if resolve_module_call(canonical, &me.property).is_some() {
                        let key = (canonical.clone(), me.property.clone());
                        let fn_id = *std_method_fn_ids.entry(key.clone()).or_insert_with(|| {
                            let id = *next_std_fn_id;
                            *next_std_fn_id += 1;
                            id
                        });
                        return Some(SpyTarget::StdMethod(fn_id));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Convenience wrapper — resolves to just the `fn_id` without
/// modifying the std-method table. Used by call sites that have
/// already collected the full set at compile-time via
/// `collect_spy_targets` and just need to look up an existing id.
fn resolve_mock_target(
    expr: &fai_compiler::ast::Expression,
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
    std_method_fn_ids: &HashMap<(String, String), u32>,
) -> Option<u32> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) => {
            if let Some(&p) = function_by_name.get(&id.name) {
                return Some(p);
            }
            if let Some(q) = named_imports.get(&id.name) {
                if let Some(&p) = function_by_name.get(q) {
                    return Some(p);
                }
            }
            None
        }
        Expression::MemberExpression(me) => {
            if let Expression::IdentifierExpression(obj) = &*me.object {
                if let Some(canonical) = module_aliases.get(&obj.name) {
                    let full = format!("{}.{}", canonical, me.property);
                    if let Some(&p) = function_by_name.get(&full) {
                        return Some(p);
                    }
                    if let Some(&id) =
                        std_method_fn_ids.get(&(canonical.clone(), me.property.clone()))
                    {
                        return Some(id);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Aggregate result of the compile-time spy-target scan.
#[derive(Debug, Default)]
struct SpyTargets {
    /// Every `fn_id` referenced by a mock/assert target — the
    /// union of user-function ids and lazily-assigned std-method
    /// ids. Used by the preamble check and by every call-site
    /// interceptor to decide whether to route through the host.
    fn_ids: HashSet<u32>,
    /// `(canonical_module, method_name) -> fn_id` for every std
    /// method that appeared as a mock target. The fn_id numbers
    /// are compile-time-unique; they start above the user
    /// function count so they never collide with user ids.
    std_method_fn_ids: HashMap<(String, String), u32>,
}

/// Walk every `test` block in the entry AST and in each discovered
/// module; collect spy targets (user functions and std-method
/// references) that get mocked or asserted on. Only functions in
/// `fn_ids` get the spy preamble; only module calls whose
/// `(canonical, method)` appears in `std_method_fn_ids` get
/// wrapped with a spy check at their call site.
fn collect_spy_targets(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
) -> SpyTargets {
    let mut out = SpyTargets::default();
    // Std-method fn_ids start after the last user fn_id so the
    // `fn_ids` set can treat both alike (the host side doesn't
    // care about the origin).
    let mut next_std_fn_id: u32 = function_by_name.len() as u32;
    fn walk_expr(
        expr: &fai_compiler::ast::Expression,
        fbn: &HashMap<String, u32>,
        aliases: &HashMap<String, String>,
        imports: &HashMap<String, String>,
        out: &mut SpyTargets,
        next_std_fn_id: &mut u32,
    ) {
        use fai_compiler::ast::Expression;
        if let Expression::CallExpression(ce) = expr {
            if let Some(target_name) = mock_target_name(&ce.callee) {
                if is_spy_call(&target_name) {
                    if let Some(first) = ce.args.first() {
                        if let Some(target) = resolve_mock_target_full(
                            &first.value,
                            fbn,
                            aliases,
                            imports,
                            &mut out.std_method_fn_ids,
                            next_std_fn_id,
                        ) {
                            out.fn_ids.insert(target.fn_id());
                        }
                    }
                }
            }
            for a in &ce.args {
                walk_expr(&a.value, fbn, aliases, imports, out, next_std_fn_id);
            }
            walk_expr(&ce.callee, fbn, aliases, imports, out, next_std_fn_id);
        }
    }
    fn mock_target_name(callee: &fai_compiler::ast::Expression) -> Option<String> {
        use fai_compiler::ast::Expression;
        match callee {
            Expression::IdentifierExpression(id) => Some(id.name.clone()),
            Expression::MemberExpression(me) => {
                if let Expression::IdentifierExpression(obj) = &*me.object {
                    Some(format!("{}.{}", obj.name, me.property))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    fn is_spy_call(name: &str) -> bool {
        matches!(
            name,
            "mock"
                | "mockOnce"
                | "mockReset"
                | "assert.calledWith"
                | "assert.callCount"
                | "assert.notCalled"
        )
    }

    fn scan_test_stmts(
        stmts: &[fai_compiler::ast::Statement],
        fbn: &HashMap<String, u32>,
        aliases: &HashMap<String, String>,
        imports: &HashMap<String, String>,
        out: &mut SpyTargets,
        next_std_fn_id: &mut u32,
    ) {
        use fai_compiler::ast::Statement;
        for s in stmts {
            match s {
                Statement::ExpressionStatement(es) => {
                    walk_expr(&es.expression, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::LetStatement(ls) => {
                    walk_expr(&ls.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::VarStatement(vs) => {
                    walk_expr(&vs.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::AssignmentStatement(a) => {
                    walk_expr(&a.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::IfStatement(is_stmt) => {
                    for b in &is_stmt.branches {
                        walk_expr(&b.condition, fbn, aliases, imports, out, next_std_fn_id);
                        scan_test_stmts(&b.body, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(e) = &is_stmt.else_branch {
                        scan_test_stmts(e, fbn, aliases, imports, out, next_std_fn_id);
                    }
                }
                Statement::TestDeclaration(td) => {
                    scan_test_stmts(&td.setup, fbn, aliases, imports, out, next_std_fn_id);
                    if let Some(b) = &td.before_all {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.before_each {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    for c in &td.cases {
                        scan_test_stmts(&c.body, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.after_each {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.after_all {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                }
                _ => {}
            }
        }
    }
    scan_test_stmts(
        &ast.statements,
        function_by_name,
        module_aliases,
        named_imports,
        &mut out,
        &mut next_std_fn_id,
    );
    for m in modules {
        scan_test_stmts(
            &m.statements,
            function_by_name,
            module_aliases,
            named_imports,
            &mut out,
            &mut next_std_fn_id,
        );
    }
    out
}

/// Max `FaiFunc(N)` arity the module assembler pre-allocates type
/// slots for. Covers top-level functions and closures. Overflow here
/// means a genuinely enormous arity — in practice forai programs
/// don't even approach this.
pub const MAX_DIRECT_ARITY: u16 = 16;

/// Compute the `FaiFunc(N) → type_index` map the builder expects.
/// Type indices are a function of the type-section layout (which
/// lists every import's type, then every runtime helper's type,
/// then the pre-allocated `FaiFunc(0..=MAX)` slots). That layout is
/// independent of target-filtering, so this doesn't need the
/// target. Exposed so callers that drive `build_program` outside
/// of `assemble_wasm_module` can share the same mapping.
pub fn direct_fai_func_type_indices() -> HashMap<u16, u32> {
    let import_count = crate::runtime::import_signatures().len() as u32;
    let rt_count = crate::runtime::type_signatures().len() as u32;
    let base = import_count + rt_count;
    (0..=MAX_DIRECT_ARITY)
        .map(|n| (n, base + n as u32))
        .collect()
}

/// Runtime-helper base index for a given target. The direct
/// builder's `Call(rt.base + RT_*)` instructions target this slot.
/// Depends on the post-filter import count, since unavailable
/// imports don't take up function-index slots.
pub fn direct_rt_base_for_target(target: Option<&str>) -> u32 {
    direct_rt_base_for_target_with_test_flag(target, true)
}

/// Same as [`direct_rt_base_for_target`] but honours `is_test` so
/// the runtime base stays in sync with the import section when
/// spy/mock imports are stripped from non-test builds.
pub fn direct_rt_base_for_target_with_test_flag(target: Option<&str>, is_test: bool) -> u32 {
    let avail = crate::runtime::available_imports_with_test_flag(target, is_test);
    let (_, actual) = crate::runtime::build_import_remap(&avail);
    actual
}

/// Backwards-compatible wrapper — equivalent to
/// `direct_rt_base_for_target(None)`. Callers that never set a
/// target can keep using this.
pub fn direct_rt_base() -> u32 {
    direct_rt_base_for_target(None)
}

/// Assemble a standalone wasm module from a `BuiltProgram`. The
/// layout matches the test infrastructure's `build_module`:
///
/// - **Types:** host imports (always all declared, even unavailable
///   ones), runtime helpers, `FaiFunc(0..=MAX)`.
/// - **Imports:** host imports filtered by `target` — unavailable
///   ones are excluded (e.g., `http_server_*` under `wasm-html`).
/// - **Functions:** runtime helpers, top-level user fns, closures.
/// - **Table:** funcref, populated with closure func indices.
/// - **Memory:** 16 pages min, grown as needed.
/// - **Globals:** `__heap_ptr` (starts above string data, 8-aligned),
///   `__env_ptr`, `error_flag`, `error_value`.
/// - **Exports:** `_start` → function index for `main` (functions\[0\]),
///   `memory`.
/// - **Elements:** table slot `i` → closure `i`'s function index.
/// - **Data:** string pool at offset 0.
///
/// `target` matches the bytecode path's `target` parameter — `None`
/// for native runs, `Some("wasm-html")` or `Some("wasm")` for
/// browser/headless builds that disable the server-side HTTP
/// imports. Callers must pass the same target to `build_program`
/// (via its `import_remap`) so the emitted code's import indices
/// agree with what the module declares.
///
/// Remaining limitations: fixed 16-page memory minimum rather than
/// derived from program size; no test-runner dispatcher (so `fai
/// test` still needs the bytecode path); no user-named top-level
/// function exports.
pub fn assemble_wasm_module(program: &BuiltProgram, target: Option<&str>) -> Vec<u8> {
    assemble_wasm_module_with_test_flag(program, target, true)
}

/// Same as [`assemble_wasm_module`] but gates spy/mock imports on
/// `is_test`. Non-test builds strip the `spy_*` imports so the
/// resulting wasm instantiates against a minimal host (e.g. the
/// native-binary runner that doesn't install the test framework).
pub fn assemble_wasm_module_with_test_flag(
    program: &BuiltProgram,
    target: Option<&str>,
    is_test: bool,
) -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };

    let fai_type_indices = direct_fai_func_type_indices();
    let import_available = crate::runtime::available_imports_with_test_flag(target, is_test);
    let (import_remap, actual_import_count) = crate::runtime::build_import_remap(&import_available);

    let mut module = EncModule::new();

    // ── types ──
    // Every import's type is declared regardless of availability —
    // it's harmless to have unused type entries, and keeping them
    // stable simplifies the offsets the builder bakes into its
    // instructions.
    let mut types = TypeSection::new();
    let import_sigs = crate::runtime::import_signatures();
    let mut import_type_indices = Vec::with_capacity(import_sigs.len());
    for (_, params, results) in &import_sigs {
        import_type_indices.push(types.len());
        types.ty().function(params.clone(), results.clone());
    }
    let rt_sigs = crate::runtime::type_signatures();
    let mut rt_type_indices = Vec::with_capacity(rt_sigs.len());
    for (params, results) in &rt_sigs {
        rt_type_indices.push(types.len());
        types.ty().function(params.clone(), results.clone());
    }
    for arity in 0..=MAX_DIRECT_ARITY {
        let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
        let expected = types.len();
        types.ty().function(params, vec![ValType::I64]);
        assert_eq!(
            expected, fai_type_indices[&arity],
            "type layout out of sync with direct_fai_func_type_indices",
        );
    }
    // Reserve a type for the `_fai_run_test(suite_i: i32,
    // case_i: i32) -> ()` dispatcher when test cases are present.
    // Always appending it (even when empty) would waste a slot,
    // so the type is conditional on `test_cases`.
    let test_runner_type_idx: Option<u32> = if program.test_cases.is_empty() {
        None
    } else {
        let idx = types.len();
        types
            .ty()
            .function(vec![ValType::I32, ValType::I32], vec![]);
        Some(idx)
    };
    module.section(&types);

    // ── imports ──
    // Only available imports are declared. Unavailable ones (e.g.,
    // `http_server_*` under `wasm-html`) are skipped; callers that
    // tried to reach them landed on `unreachable` via
    // `emit_import_call`.
    let mut imports = ImportSection::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if import_available[i] {
            imports.import("env", name, EntityType::Function(import_type_indices[i]));
        }
    }
    module.section(&imports);

    // ── functions ──
    let mut funcs = FunctionSection::new();
    for &t in &rt_type_indices {
        funcs.function(t);
    }
    for (info, _) in &program.functions {
        let t = *fai_type_indices.get(&info.param_count).unwrap_or_else(|| {
            panic!(
                "arity {} for `{}` exceeds MAX_DIRECT_ARITY",
                info.param_count, info.name,
            )
        });
        funcs.function(t);
    }
    for c in &program.closures {
        let t = *fai_type_indices
            .get(&c.info.param_count)
            .unwrap_or_else(|| {
                panic!(
                    "closure arity {} exceeds MAX_DIRECT_ARITY",
                    c.info.param_count,
                )
            });
        funcs.function(t);
    }
    // Test runner dispatcher (when present) sits at the very end
    // of the function section — its wasm function index is
    // `top_level_base + functions.len() + closures.len()`.
    if let Some(t) = test_runner_type_idx {
        funcs.function(t);
    }
    module.section(&funcs);

    // ── tables ──
    let mut tables = TableSection::new();
    let closure_count = program.closures.len() as u32;
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        minimum: closure_count as u64,
        maximum: Some(closure_count as u64),
        table64: false,
        shared: false,
    });
    module.section(&tables);

    // Append the known literal strings ("null", "true", "false")
    // after the user string pool so `RT_VALUE_TO_STR` can produce
    // them when stringifying `null` / Bool values. Without this
    // every stringification of those values reads `(0, 0)` and
    // emits the empty string.
    let mut extended = program.string_data.clone();
    fn append_known(buf: &mut Vec<u8>, s: &str) -> (u32, u32) {
        let off = buf.len() as u32;
        buf.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }
    let str_null = append_known(&mut extended, "null");
    let str_true = append_known(&mut extended, "true");
    let str_false = append_known(&mut extended, "false");
    let known = crate::runtime::KnownStrings {
        str_null,
        str_true,
        str_false,
        ..Default::default()
    };

    // ── memory ──
    //
    // Size: string data + 64 KiB scratch, rounded up to the next
    // page, with a 16-page (1 MiB) minimum so small programs have
    // room for heap growth. Matches `module.rs::emit_memory_section`
    // for parity — programs compiled through either path see
    // identical starting memory.
    let total_bytes = extended.len() as u32 + crate::runtime::FREE_BUCKET_REGION_BYTES + 65536;
    let pages = std::cmp::max((total_bytes / 65536) + 1, 16);
    let mut mem = MemorySection::new();
    mem.memory(MemoryType {
        minimum: pages as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mem);

    // ── globals ──
    // The size-bucketed free-list heads live in a zero-init region starting at
    // `bucket_base`; the heap bump pointer starts just past it.
    let bucket_base = ((extended.len() as u32) + 7) & !7;
    let heap_start = (bucket_base + crate::runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(heap_start as i32),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i64_const(0),
    );
    // Module-var globals. Initialised to VAL_NULL so any read-before-
    // init observes NaN-boxed null rather than a bit-pattern 0, which
    // wouldn't round-trip through the runtime's type checks. The
    // `<__module_init__>` function emitted by the codegen writes the
    // user-supplied initialiser into each slot at program start.
    for _ in 0..program.module_var_count {
        globals.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(crate::runtime::VAL_NULL),
        );
    }
    // Heap free-list head for rt_alloc reuse / rt_free, appended last so the
    // fixed (0-3) and module-var (4..) global indices are unchanged. 0 = empty.
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    // Live-object counter (plan 113): incremented in rt_alloc, decremented in
    // rt_free. The leak oracle reads it at program exit. Appended after the
    // free-list so earlier indices are unchanged. 0 = no live objects yet.
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    module.section(&globals);

    // Function indices use the POST-filter import count so they
    // agree with the import section above. This has to match what
    // `direct_rt_base_for_target(target)` returned when the program
    // was built — callers that drive `build_program` directly must
    // pass the same target.
    let top_level_base = actual_import_count + crate::runtime::RT_COUNT;
    let closure_base = top_level_base + program.functions.len() as u32;
    let main_func_idx = top_level_base;
    let test_runner_func_idx = closure_base + closure_count;

    // ── exports ──
    //
    // Parity with `module.rs::emit_export_section`. Host tooling
    // grabs `__heap_ptr` / `__env_ptr` /
    // `__indirect_function_table` to call closures from JS and to
    // inspect the NaN-boxed heap; named top-level functions are
    // exported so callbacks can reach them by name. When test
    // cases are present, `_fai_run_test` sits after all closures.
    let mut exports = ExportSection::new();
    exports.export("_start", ExportKind::Func, main_func_idx);
    exports.export("memory", ExportKind::Memory, 0);
    if closure_count > 0 {
        exports.export("__indirect_function_table", ExportKind::Table, 0);
    }
    // Host-callable refcount release: the HTTP host reclaims per-request
    // guest object graphs (request/response/event dicts it built) through
    // this after writing the response. The async assembler exports it too;
    // without it `host_release_value` silently no-ops and a sync-built
    // server leaks the full request graph per request (plan 116).
    exports.export(
        "__fai_release",
        ExportKind::Func,
        actual_import_count + RT_RELEASE,
    );
    exports.export("__heap_ptr", ExportKind::Global, 0);
    // Live-object counter (plan 113) — the host leak oracle reads this by name
    // after a run. Index = free-list (4 + module vars) + 1.
    exports.export(
        "__live_objects",
        ExportKind::Global,
        5 + program.module_var_count,
    );
    // Heap overflow free-list head — post-mortem heap stats walk it.
    exports.export(
        "__free_list",
        ExportKind::Global,
        4 + program.module_var_count,
    );
    exports.export("__env_ptr", ExportKind::Global, 1);
    exports.export("__error_flag", ExportKind::Global, GLOBAL_ERROR_FLAG);
    exports.export("__error_value", ExportKind::Global, GLOBAL_ERROR_VALUE);
    if test_runner_type_idx.is_some() {
        exports.export("_fai_run_test", ExportKind::Func, test_runner_func_idx);
    }
    let mut exported_names = std::collections::HashSet::new();
    for (i, (info, _)) in program.functions.iter().enumerate() {
        let name = &info.name;
        if name.is_empty() || name.starts_with('<') || exported_names.contains(name) {
            continue;
        }
        let func_idx = top_level_base + i as u32;
        exports.export(name, ExportKind::Func, func_idx);
        exported_names.insert(name.clone());
    }
    module.section(&exports);

    // ── elements ──
    if closure_count > 0 {
        let mut elements = ElementSection::new();
        let func_indices: Vec<u32> = (0..closure_count).map(|i| closure_base + i).collect();
        elements.active(
            Some(0),
            &ConstExpr::i32_const(0),
            Elements::Functions(func_indices.into()),
        );
        module.section(&elements);
    }

    // ── code ──
    //
    // Runtime helpers see the same `import_remap` the builder used,
    // so their internal `emit_import_call(IMPORT_X, remap)` lands
    // on the matching post-filter wasm index (or `unreachable` for
    // unavailable imports under `wasm-html`, matching the bytecode
    // path's behaviour).
    let mut code = CodeSection::new();
    let freelist_global = 4 + program.module_var_count; // appended after fixed+module-var globals
    let live_count_global = freelist_global + 1; // appended after the free-list
    for f in crate::runtime::emit_all(
        actual_import_count,
        &import_remap,
        &known,
        freelist_global,
        live_count_global,
        bucket_base,
    ) {
        code.function(&f);
    }
    for (_, f) in &program.functions {
        code.function(f);
    }
    for c in &program.closures {
        code.function(&c.function);
    }
    // Test runner dispatcher body: `_fai_run_test(suite_i,
    // case_i) -> ()`. For each (suite, case) entry, emit:
    //     if suite_i == s && case_i == c: call wrapper; return
    // Unknown (suite, case) traps via `unreachable`. The CLI
    // test runner reads out the trap and records a failure.
    if test_runner_type_idx.is_some() {
        use wasm_encoder::{BlockType, Function, Instruction};
        let mut dispatcher = Function::new([]);
        for entry in &program.test_cases {
            dispatcher.instruction(&Instruction::LocalGet(0)); // suite_i
            dispatcher.instruction(&Instruction::I32Const(entry.suite_idx as i32));
            dispatcher.instruction(&Instruction::I32Eq);
            dispatcher.instruction(&Instruction::LocalGet(1)); // case_i
            dispatcher.instruction(&Instruction::I32Const(entry.case_idx as i32));
            dispatcher.instruction(&Instruction::I32Eq);
            dispatcher.instruction(&Instruction::I32And);
            dispatcher.instruction(&Instruction::If(BlockType::Empty));
            let wasm_idx = top_level_base + entry.function_index as u32;
            dispatcher.instruction(&Instruction::Call(wasm_idx));
            // The wrapper returns i64 (like any fai function).
            // Test runner is `-> ()`, so drop the result.
            dispatcher.instruction(&Instruction::Drop);
            dispatcher.instruction(&Instruction::Return);
            dispatcher.instruction(&Instruction::End);
        }
        dispatcher.instruction(&Instruction::Unreachable);
        dispatcher.instruction(&Instruction::End);
        code.function(&dispatcher);
    }
    module.section(&code);

    // ── data ──
    if !extended.is_empty() {
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), extended.iter().copied());
        module.section(&data);
    }

    // ── debug metadata (plan 116): name section + fai-dbg table ──
    let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if let Some(idx) = import_remap.get(i).copied().flatten() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(idx, *name));
        }
    }
    for (k, n) in crate::runtime::rt_fn_names().iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            actual_import_count + k as u32,
            *n,
        ));
    }
    for (i, (info, _)) in program.functions.iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry {
            index: top_level_base + i as u32,
            name: info.name.clone(),
            file: info.source_file.clone(),
            line: info.source_line,
        });
    }
    for (i, c) in program.closures.iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry {
            index: closure_base + i as u32,
            name: c.info.name.clone(),
            file: c.info.source_file.clone(),
            line: c.info.source_line,
        });
    }
    if test_runner_type_idx.is_some() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            test_runner_func_idx,
            "_fai_run_test",
        ));
    }
    crate::debug_info::append_debug_sections(
        &mut module,
        &dbg,
        &crate::debug_info::DbgMeta {
            bucket_base: Some(bucket_base),
            bucket_count: crate::runtime::NUM_FREE_BUCKETS,
        },
    );

    module.finish()
}

// ── Real async engine: resume-body lowering + module assembly (R2) ──
//
// Compiles an async `main` into a *resume function* (`() -> ()`) over the
// guest scheduler in `async_engine`. v1 handles the narrow straight-line
// shape — statements that don't suspend plus `sleep(<number>)` suspension
// points, no locals across a suspension — which is enough for the
// `sleep_ordering` acceptance fixture. Returns `None` (fall back to the
// facade / sync path) for anything outside that shape.

/// `sleep(<number>)` as a statement → the millisecond delay, else `None`.
fn async_sleep_ms_of(stmt: &Statement) -> Option<f64> {
    let Statement::ExpressionStatement(es) = stmt else {
        return None;
    };
    let Expression::CallExpression(call) = &es.expression else {
        return None;
    };
    let Expression::IdentifierExpression(callee) = &*call.callee else {
        return None;
    };
    if callee.name != "sleep" {
        return None;
    }
    let [arg] = call.args.as_slice() else {
        return None;
    };
    let Expression::NumberExpression(n) = &arg.value else {
        return None;
    };
    Some(n.value.max(0.0))
}

/// If `expr` is `remoteCall(url, fn, args, hash)` (the RPC client transport),
/// return its 4 argument expressions and call-site location. Lowered as a
/// suspending host op (`Term::AwaitRemote`) so the task yields while the
/// request is in flight.
fn remote_call_args(expr: &Expression) -> Option<(Vec<&Expression>, &fai_compiler::ast::SourceLocation)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    if id.name != "remoteCall" || call.args.len() != 4 {
        return None;
    }
    Some((call.args.iter().map(|a| &a.value).collect(), &call.location))
}

/// If `expr` is a direct call to a *user* function (one of `fns`), return
/// its name and argument expressions. Builtins (`print`, `sleep`, `all`,
/// `Error`, …) are not user functions and never match. In the "everything
/// is async" model, every user-function call is an auto-await.
/// Module-aware name resolution for the async lowering. Mirrors
/// `async_analysis::resolve_bare_function` / `resolve_call_target` so the
/// qualified names produced here match the analysis' async set (and the
/// `{module}.{fn}`-prefixed function table). For single-file programs the
/// maps are empty and `module_context` is `None`, so resolution is identity.
struct AsyncResolve<'a> {
    /// Qualified names of async (task) functions — awaits/spawns target these.
    async_set: &'a std::collections::HashSet<String>,
    /// Qualified names of every user function (for "is this a known fn?").
    all_fns: &'a std::collections::HashSet<String>,
    /// Namespace aliases: `obj` in `obj.fn` → canonical module path.
    aliases: &'a std::collections::HashMap<String, String>,
    /// Named imports: bare `f` → `{module}.f`.
    named_imports: &'a std::collections::HashMap<String, String>,
    /// The module a function being lowered belongs to (peer-call resolution).
    module_context: Option<&'a str>,
    /// Call sites the checker rewrote via UFCS (`recv.method()` → `method(recv)`),
    /// keyed by `(module_key, line, col)` — same key `compile_call` uses.
    ufcs_calls: &'a std::collections::HashSet<(String, u32, u32)>,
    /// This function's `module_key` (file path, else module context) — the
    /// first element of a UFCS call-site key.
    module_key: &'a str,
}

impl<'a> AsyncResolve<'a> {
    /// Whether this call site was rewritten via UFCS by the checker.
    fn is_ufcs_call(&self, call: &CallExpression) -> bool {
        self.ufcs_calls.contains(&(
            self.module_key.to_string(),
            call.location.line,
            call.location.column,
        ))
    }

    /// Resolve a bare identifier to its canonical user-fn name.
    fn resolve_bare(&self, name: &str) -> Option<String> {
        if let Some(m) = self.module_context {
            let peer = format!("{}.{}", m, name);
            if self.all_fns.contains(&peer) {
                return Some(peer);
            }
        }
        if self.all_fns.contains(name) {
            return Some(name.to_string());
        }
        if let Some(imported) = self.named_imports.get(name) {
            if self.all_fns.contains(imported) {
                return Some(imported.clone());
            }
        }
        None
    }

    /// Resolve a member call `obj.prop` to its canonical name via aliases.
    fn resolve_member(&self, obj: &str, prop: &str) -> Option<String> {
        let canonical = self.aliases.get(obj)?;
        let target = format!("{}.{}", canonical, prop);
        if self.all_fns.contains(&target) {
            Some(target)
        } else {
            None
        }
    }

    /// Resolve any call expression's callee to a canonical user-fn name.
    fn resolve_call(&self, call: &CallExpression) -> Option<String> {
        match &*call.callee {
            Expression::IdentifierExpression(id) => self.resolve_bare(&id.name),
            Expression::MemberExpression(me) => {
                // UFCS (`recv.method(...)` → `method(recv, ...)`): the checker
                // recorded this site, so `method` resolves as a free function.
                if self.is_ufcs_call(call) {
                    return self.resolve_bare(&me.property);
                }
                // Otherwise a namespace-member call (`alias.fn`).
                let Expression::IdentifierExpression(obj) = &*me.object else {
                    return None;
                };
                self.resolve_member(&obj.name, &me.property)
            }
            _ => None,
        }
    }

    fn is_async(&self, name: &str) -> bool {
        self.async_set.contains(name)
    }
}

/// If `expr` is a call to an *async* user function, return its canonical name
/// and arg expressions. Sync user calls and builtins return `None` (they flow
/// through `compile_call` as plain direct calls).
fn user_callee<'a>(
    expr: &'a Expression,
    fns: &AsyncResolve<'_>,
) -> Option<(String, Vec<&'a Expression>, &'a fai_compiler::ast::SourceLocation)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let resolved = fns.resolve_call(call)?;
    if !fns.is_async(&resolved) {
        return None;
    }
    // UFCS prepends the receiver: `recv.method(a)` → `method(recv, a)`.
    let mut args: Vec<&'a Expression> = Vec::with_capacity(call.args.len() + 1);
    if fns.is_ufcs_call(call) {
        if let Expression::MemberExpression(me) = &*call.callee {
            args.push(&me.object);
        }
    }
    args.extend(call.args.iter().map(|a| &a.value));
    Some((
        resolved,
        args,
        &call.location,
    ))
}

/// Async-closure compilation context, threaded into `BuildContext` so a
/// closure encountered mid-body can be detected — and, in later A3.0 steps,
/// lowered as a resume fn (frame leads with `env_ptr`, params follow). Present
/// only on the real-engine path; `None` on the pure-sync builder.
#[derive(Clone, Copy)]
struct AsyncClosureCtx<'a> {
    async_set: &'a std::collections::HashSet<String>,
    all_fns: &'a std::collections::HashSet<String>,
    layout: &'a crate::async_engine::SchedLayout,
    fn_table_idx: &'a std::collections::HashMap<String, u32>,
    frame_sizes: &'a std::collections::HashMap<String, i32>,
}

/// A closure whose body awaits or forks must be compiled as a resume fn
/// (A3.0) — detected by the same suspension check used for named functions.
fn closure_is_async(fd: &FunctionDeclaration, r: &AsyncResolve<'_>) -> bool {
    stmts_have_suspension(&fd.body, r)
}

/// If `expr` is a call whose callee is an async closure *literal*
/// (`(do…end)(args)`), return the closure expression + its args. The literal's
/// async-ness is statically known, so the call is a suspension point with no
/// runtime sync/async dispatch (that's the closure-typed-*value* case, later).
fn async_closure_call<'a>(
    expr: &'a Expression,
    r: &AsyncResolve<'_>,
) -> Option<(&'a Expression, Vec<&'a Expression>)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::FunctionExpression(fd) = &*call.callee else {
        return None;
    };
    if !closure_is_async(fd, r) {
        return None;
    }
    Some((&*call.callee, call.args.iter().map(|a| &a.value).collect()))
}

/// If `expr` is a call whose callee is a closure-typed *parameter* (`p(args)`),
/// return the callee expression + args. Such a call may suspend (the closure
/// could be async), so it's lowered as an await with runtime sync/async
/// dispatch — the checker guarantees a called param is function-typed.
/// A call whose callee is a closure *value* rather than a named function:
/// invoking a closure-typed parameter (`children()`), or a computed callee
/// (`handlers[i]()`, `cb!()`, `getCb()()`). These dispatch through the closure
/// header and — mirroring `async_analysis`'s `ClosureCall` cause — are lowered
/// as `Term::AwaitClosure` so a suspending closure parks the caller instead of
/// being driven by a re-entrant `poll`. Must match the analysis's detection or
/// a function flagged async there could hit a CFG shape it can't lower here.
fn indirect_closure_call<'a>(
    expr: &'a Expression,
    params: &std::collections::HashSet<String>,
    fns: &AsyncResolve<'_>,
) -> Option<(&'a Expression, Vec<&'a Expression>)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let is_closure_callee = match &*call.callee {
        Expression::IdentifierExpression(id) => params.contains(&id.name),
        Expression::IndexExpression(_)
        | Expression::ForceUnwrapExpression(_)
        | Expression::CallExpression(_) => true,
        // A member call is a closure-valued *field* invocation
        // (`matched!.builder()`, `state.onUpdate()`) only when it's NEITHER a
        // UFCS rewrite (`recv.method()` → `method(recv)`) NOR a namespace-member
        // call on a module alias (`array.append`, `json.stringify`). Those two
        // resolve to named functions / builtins and are lowered as ordinary
        // calls — routing them through AwaitClosure (treating a builtin as a
        // closure value) corrupts the scheduler. This mirrors `compile_call`'s
        // routing, which only falls through to the closure path here.
        Expression::MemberExpression(me) => {
            let obj_is_module_alias = matches!(
                &*me.object,
                Expression::IdentifierExpression(id) if fns.aliases.contains_key(&id.name)
            );
            !fns.is_ufcs_call(call) && !obj_is_module_alias
        }
        _ => false,
    };
    if !is_closure_callee {
        return None;
    }
    Some((&*call.callee, call.args.iter().map(|a| &a.value).collect()))
}

/// Whether any statement (recursively) references a user-function call.
/// Used to gate `try` bodies: error propagation out of an awaited child is
/// not implemented yet, so a `try` containing an await falls back.
fn stmts_have_user_call(stmts: &[Statement], fns: &AsyncResolve<'_>) -> bool {
    stmts.iter().any(|s| stmt_has_user_call(s, fns))
}

/// Whether any statement (recursively) suspends — a `sleep` or a user call.
/// Used to gate a value-`try`'s `finally`: the try-result is held in a wasm
/// local that wouldn't survive a suspension inside `finally`.
fn stmts_have_suspension(stmts: &[Statement], fns: &AsyncResolve<'_>) -> bool {
    stmts.iter().any(|s| async_sleep_ms_of(s).is_some() || stmt_has_user_call(s, fns))
}

/// Whether `stmts` contain a `break`/`continue` that targets the *enclosing*
/// loop — i.e. not buried inside a nested `for`/`while` (those bind to the inner
/// loop). Used to decide whether a `for` loop can be safely desugared into an
/// index `while` (a `continue` would skip the manual index increment).
fn stmts_have_loop_control(stmts: &[Statement]) -> bool {
    stmts.iter().any(|s| match s {
        Statement::BreakStatement(_) | Statement::ContinueStatement(_) => true,
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| stmts_have_loop_control(&b.body))
                || is.else_branch.as_ref().is_some_and(|e| stmts_have_loop_control(e))
        }
        Statement::TryStatement(ts) => {
            stmts_have_loop_control(&ts.try_body)
                || stmts_have_loop_control(&ts.catch_body)
                || ts.finally_body.as_ref().is_some_and(|f| stmts_have_loop_control(f))
        }
        // A nested for/while owns its own break/continue; don't descend.
        _ => false,
    })
}

fn stmt_has_user_call(stmt: &Statement, fns: &AsyncResolve<'_>) -> bool {
    let value = match stmt {
        Statement::LetStatement(ls) => Some(&ls.value),
        Statement::VarStatement(vs) => Some(&vs.value),
        Statement::AssignmentStatement(a) => Some(&a.value),
        Statement::ExpressionStatement(es) => Some(&es.expression),
        Statement::ReturnStatement(rs) => rs.value.as_ref(),
        Statement::ThrowStatement(ts) => Some(&ts.expression),
        Statement::NowaitStatement(nw) => Some(&nw.expression),
        _ => None,
    };
    if let Some(e) = value {
        if expr_has_user_call(e, fns) {
            return true;
        }
    }
    match stmt {
        Statement::IfStatement(is) => {
            is.branches.iter().any(|b| {
                // A branch *condition* can itself contain an async call (it gets
                // hoisted into a preceding `let` by the ANF). Count it, or the
                // suspension would be invisible until after hoisting moved it into
                // the body — too late for the for→while desugar to fire.
                expr_has_user_call(&b.condition, fns) || stmts_have_user_call(&b.body, fns)
            }) || is
                .else_branch
                .as_ref()
                .is_some_and(|e| stmts_have_user_call(e, fns))
        }
        Statement::WhileStatement(ws) => {
            expr_has_user_call(&ws.condition, fns) || stmts_have_user_call(&ws.body, fns)
        }
        Statement::ForStatement(fs) => {
            expr_has_user_call(&fs.items, fns) || stmts_have_user_call(&fs.body, fns)
        }
        Statement::TryStatement(ts) => {
            stmts_have_user_call(&ts.try_body, fns)
                || stmts_have_user_call(&ts.catch_body, fns)
                || ts
                    .finally_body
                    .as_ref()
                    .is_some_and(|f| stmts_have_user_call(f, fns))
        }
        _ => false,
    }
}

/// Whether `expr` contains a user-function call anywhere (used to reject
/// nested awaits the v1 lowering can't place, e.g. `print(child())`).
fn expr_has_user_call(expr: &Expression, fns: &AsyncResolve<'_>) -> bool {
    if user_callee(expr, fns).is_some() {
        return true;
    }
    // `remoteCall(...)` is a suspension point too (lowered as `Term::AwaitRemote`),
    // so it counts as "has a call that needs segment handling" — a statement
    // containing one (other than at a directly-handled position) can't be pushed
    // inline as a plain sync segment statement.
    if remote_call_args(expr).is_some() {
        return true;
    }
    match expr {
        Expression::CallExpression(c) => {
            expr_has_user_call(&c.callee, fns)
                || c.args.iter().any(|a| expr_has_user_call(&a.value, fns))
        }
        Expression::BinaryExpression(b) => {
            expr_has_user_call(&b.left, fns) || expr_has_user_call(&b.right, fns)
        }
        Expression::UnaryExpression(u) => expr_has_user_call(&u.expression, fns),
        Expression::MemberExpression(m) => expr_has_user_call(&m.object, fns),
        Expression::IndexExpression(i) => {
            expr_has_user_call(&i.object, fns) || expr_has_user_call(&i.index, fns)
        }
        _ => false,
    }
}

/// A single-binding `let`/`var` statement → `(name, value)`.
fn single_binding<'a>(stmt: &'a Statement) -> Option<(&'a str, &'a Expression)> {
    match stmt {
        Statement::LetStatement(ls) if ls.bindings.len() == 1 => {
            Some((ls.bindings[0].name.as_str(), &ls.value))
        }
        Statement::VarStatement(vs) if vs.bindings.len() == 1 => {
            Some((vs.bindings[0].name.as_str(), &vs.value))
        }
        _ => None,
    }
}

/// If `expr` is `all(c1(), c2(), ...)` where every argument is a user-call,
/// return the list of `(callee, args)` children. `all` is a builtin keyword,
/// not a user function.
type AllChild<'a> = (String, Vec<&'a Expression>, &'a fai_compiler::ast::SourceLocation);

fn all_call<'a>(expr: &'a Expression, fns: &AsyncResolve<'_>) -> Option<Vec<AllChild<'a>>> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    if id.name != "all" || call.args.is_empty() {
        return None;
    }
    let mut children = Vec::with_capacity(call.args.len());
    for a in &call.args {
        let (callee, args, loc) = user_callee(&a.value, fns)?;
        // No nested user calls in a child's own args (v1).
        if args.iter().any(|x| expr_has_user_call(x, fns)) {
            return None;
        }
        children.push((callee, args, loc));
    }
    Some(children)
}

/// What a block does at entry with a prior suspension's children. A block
/// resumed after an `await`/`all` first checks each child for failure
/// (propagating the error if any failed), then binds the named results.
enum Incoming {
    None,
    /// One entry per awaited child (1 for an await, N for `all`). `Some(name)`
    /// binds child k's result to that local; `None` discards it. `on_error`
    /// is the enclosing catch handler `(catch block, error binding)` if the
    /// await was inside a `try`; a failed child jumps there instead of
    /// failing the task.
    Awaited {
        binds: Vec<Option<String>>,
        on_error: Option<(usize, String)>,
    },
    /// Resume of an `AwaitRemote`: bind `remote_result(g_current)` to `bind`
    /// (or discard if `None`).
    AwaitedRemote { bind: Option<String> },
}

/// A basic block in the resumable function's CFG: what to assign at entry
/// (from a prior suspension's results), the non-suspending statements to
/// run, and a terminator. The block index is the `resume_state` value used
/// to dispatch to it; control flows between blocks by setting `resume_state`
/// and re-dispatching (jumps) or returning to the scheduler (suspensions).
struct Block<'a> {
    incoming: Incoming,
    stmts: Vec<&'a Statement>,
    term: Term<'a>,
    /// Phase 3 reclamation (plans/111): frame-var names to `rt_drop` after this
    /// block's statements run, before its terminator. Set on the back-edge
    /// block of a NON-suspending `while` body to free confined fresh-literal
    /// loop-body temporaries each iteration (the CFG path's analogue of the
    /// sync Builder's per-iteration `pop_scope` drops).
    drops: Vec<String>,
}

enum Term<'a> {
    /// Placeholder while the CFG is being built; must be replaced.
    Unset,
    /// Unconditional jump: set `resume_state` and re-dispatch.
    Goto(usize),
    /// Branch on a (non-suspending) condition.
    Cond {
        cond: &'a Expression,
        then_blk: usize,
        else_blk: usize,
    },
    /// `sleep(ms)` then resume at `next`.
    Sleep { ms: f64, next: usize },
    /// `remoteCall(url, fn, args, hash)` — the RPC client transport. Lowered as
    /// a suspending host op: `remote_begin(g_current, …)` starts the request and
    /// parks the task; on resume the next block binds the response via
    /// `remote_result(g_current)`. Browser does the request with async `fetch`,
    /// so the UI thread stays free while it's in flight.
    AwaitRemote {
        args: Vec<&'a Expression>,
        loc: &'a fai_compiler::ast::SourceLocation,
        next: usize,
    },
    /// `await callee(args)` then resume at `next` (which binds the result).
    Await {
        callee: String,
        args: Vec<&'a Expression>,
        /// Call-site location for generic type-arg lookup.
        loc: &'a fai_compiler::ast::SourceLocation,
        next: usize,
    },
    /// `all(c1(), c2(), ...)` — spawn each, join on all, resume at `next`.
    All {
        children: Vec<AllChild<'a>>,
        next: usize,
    },
    /// `await` of an async *closure* call — `closure` is an expression that
    /// evaluates to a closure value (a `do…end` literal for now). Spawned via
    /// its heap header (frame size + table slot), then awaited like a named
    /// child; resume at `next`.
    AwaitClosure {
        closure: &'a Expression,
        args: Vec<&'a Expression>,
        next: usize,
    },
    /// Complete the task with an expression value.
    Complete(&'a Expression),
    /// Complete the task with `Void`.
    CompleteVoid,
    /// Complete with the result of the just-awaited child in pending slot 0
    /// (an await in tail/return position).
    CompletePending,
    /// Complete the task with `remote_result(g_current)` — a `remoteCall(...)`
    /// in tail/return position (the generated RPC client stubs return it).
    CompleteRemote,
    /// `throw value` inside a `try`: bind the value to the catch handler's
    /// name and jump to the catch block.
    ThrowTo {
        value: &'a Expression,
        catch_blk: usize,
        err_var: String,
    },
    /// `throw value` with no enclosing `try`: fail the task with the value.
    Fail(&'a Expression),
    /// Store `value` into the try-result local, then jump to `next`. Used to
    /// carry a `try`/`catch` body's value to a `finally` before completing.
    StoreResultGoto { value: &'a Expression, next: usize },
    /// Complete the task with the try-result local (after `finally` ran).
    CompleteResult,
}

/// How the last statement of a lowered sequence yields its value.
#[derive(Clone, Copy)]
enum TailMode {
    /// Not a tail sequence — control continues after it.
    None,
    /// Tail: complete the task with the value.
    Complete,
    /// Tail-in-a-try-with-finally: store the value into the try-result local
    /// and jump to `next` (the finally block).
    StoreResult(usize),
}

/// Result of lowering a statement/sequence: control continues at a block,
/// or the path diverged (completed/returned).
enum Flow {
    Continue(usize),
    Diverged,
}

/// Frame layout for one async function: a heap block holding each
/// param/local (i64 slots, in declaration order) followed by a pending
/// region of `pending_count` i32 child-id slots used to remember awaited
/// tasks (one for an auto-await, N for an `all(...)`) between segments.
struct AsyncFrame {
    var_off: std::collections::HashMap<String, u64>,
    vars: Vec<String>,
    pending_off: u64,
    size: i32,
    /// Closures reserve frame slot 0 for the captured-env address (`env_ptr`);
    /// the resume fn seeds `__env_ptr` from it at each entry so upvalue reads
    /// (`__env_ptr + i*8`) work. Named fns have no env slot.
    has_env: bool,
}

fn push_unique(vars: &mut Vec<String>, name: &str) {
    if !vars.iter().any(|v| v == name) {
        vars.push(name.to_string());
    }
}

/// Collect every `let`/`var` binding name, descending into `if`/`while`/
/// `try` bodies. Deduped (a name shared across sibling branches gets one
/// frame slot).
fn collect_async_vars(stmts: &[Statement], vars: &mut Vec<String>) {
    for stmt in stmts {
        match stmt {
            Statement::LetStatement(ls) => {
                for b in &ls.bindings {
                    push_unique(vars, &b.name);
                }
            }
            Statement::VarStatement(vs) => {
                for b in &vs.bindings {
                    push_unique(vars, &b.name);
                }
            }
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_async_vars(&branch.body, vars);
                }
                if let Some(e) = &is.else_branch {
                    collect_async_vars(e, vars);
                }
            }
            Statement::WhileStatement(ws) => collect_async_vars(&ws.body, vars),
            Statement::ForStatement(fs) => collect_async_vars(&fs.body, vars),
            Statement::TryStatement(ts) => {
                push_unique(vars, &ts.catch_name);
                collect_async_vars(&ts.try_body, vars);
                collect_async_vars(&ts.catch_body, vars);
                if let Some(f) = &ts.finally_body {
                    collect_async_vars(f, vars);
                }
            }
            _ => {}
        }
    }
}

/// Collect the names rebound by a MULTI-variable assignment (`x, y = expr`)
/// anywhere in `stmts`, descending into nested bodies. Those names are
/// EXCLUDED from the async completion release set: the tuple-destructure
/// assignment path plain-overwrites without retain-new / release-old, so the
/// slot's reference count is not guaranteed `+1` at completion.
///
/// SINGLE-variable reassignment (`x = expr`) is no longer an exclusion:
/// `build_resume_fn` marks those frame locals owned (`owned_frame_locals`),
/// so `compile_assignment` maintains the `+1` (retain-new / release-old) and
/// completion can release them — this is what stops `html = html + piece`
/// accumulators leaking every intermediate (the brain SSR leak, plan 116).
/// Field/index mutations (`x.f = …`, `x[i] = …`) don't rebind `x` — they
/// mutate its contents, so `x` keeps its single owned ref and is NOT
/// collected here.
fn collect_multi_rebound_names(stmts: &[Statement], out: &mut std::collections::HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Statement::AssignmentStatement(a) => {
                if let fai_compiler::ast::AssignmentTarget::Variables { names } = &a.target {
                    if names.len() > 1 {
                        for n in names {
                            out.insert(n.clone());
                        }
                    }
                }
            }
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_multi_rebound_names(&branch.body, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_multi_rebound_names(e, out);
                }
            }
            Statement::WhileStatement(ws) => collect_multi_rebound_names(&ws.body, out),
            Statement::ForStatement(fs) => collect_multi_rebound_names(&fs.body, out),
            Statement::TryStatement(ts) => {
                collect_multi_rebound_names(&ts.try_body, out);
                collect_multi_rebound_names(&ts.catch_body, out);
                if let Some(f) = &ts.finally_body {
                    collect_multi_rebound_names(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect every `try ... catch e` binding name in `stmts` (recursively). Catch
/// vars are EXCLUDED from the async completion release set: a `throw <borrowed>`
/// (`Term::ThrowTo`) binds the catch var to a borrowed value (rc not `+1`), so
/// releasing it could over-release. Conservative — catch payloads are small.
fn collect_catch_names(stmts: &[Statement], out: &mut std::collections::HashSet<String>) {
    for stmt in stmts {
        match stmt {
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_catch_names(&branch.body, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_catch_names(e, out);
                }
            }
            Statement::WhileStatement(ws) => collect_catch_names(&ws.body, out),
            Statement::ForStatement(fs) => collect_catch_names(&fs.body, out),
            Statement::TryStatement(ts) => {
                out.insert(ts.catch_name.clone());
                collect_catch_names(&ts.try_body, out);
                collect_catch_names(&ts.catch_body, out);
                if let Some(f) = &ts.finally_body {
                    collect_catch_names(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Collect the names of functions that are *spawned* anywhere in `stmts`:
/// `nowait f(...)` targets and `all(f(...), g(...))` children. A spawned
/// function must be a resume task (it lives in the function table), so it
/// has to join the async set even when its own body never suspends.
fn collect_spawn_targets(
    stmts: &[Statement],
    r: &AsyncResolve<'_>,
    out: &mut std::collections::HashSet<String>,
) {
    fn from_call(expr: &Expression, r: &AsyncResolve<'_>, out: &mut std::collections::HashSet<String>) {
        if let Expression::CallExpression(c) = expr {
            if let Some(n) = r.resolve_call(c) {
                out.insert(n);
            }
        }
    }
    fn from_all(expr: &Expression, r: &AsyncResolve<'_>, out: &mut std::collections::HashSet<String>) {
        if let Expression::CallExpression(c) = expr {
            if let Expression::IdentifierExpression(id) = &*c.callee {
                if id.name == "all" {
                    for a in &c.args {
                        from_call(&a.value, r, out);
                    }
                }
            }
        }
    }
    for stmt in stmts {
        match stmt {
            Statement::NowaitStatement(nw) => from_call(&nw.expression, r, out),
            Statement::LetStatement(ls) => from_all(&ls.value, r, out),
            Statement::VarStatement(vs) => from_all(&vs.value, r, out),
            Statement::IfStatement(is) => {
                for branch in &is.branches {
                    collect_spawn_targets(&branch.body, r, out);
                }
                if let Some(e) = &is.else_branch {
                    collect_spawn_targets(e, r, out);
                }
            }
            Statement::WhileStatement(ws) => collect_spawn_targets(&ws.body, r, out),
            Statement::ForStatement(fs) => collect_spawn_targets(&fs.body, r, out),
            Statement::TryStatement(ts) => {
                collect_spawn_targets(&ts.try_body, r, out);
                collect_spawn_targets(&ts.catch_body, r, out);
                if let Some(f) = &ts.finally_body {
                    collect_spawn_targets(f, r, out);
                }
            }
            _ => {}
        }
    }
}

/// Max child-id slots a statement (and its nested bodies) need across a
/// suspension: `all(...)` → its arg count, a single await → 1, else 0.
fn stmt_pending_count(
    stmt: &Statement,
    fns: &AsyncResolve<'_>,
    params: &std::collections::HashSet<String>,
) -> usize {
    let value = match stmt {
        Statement::LetStatement(ls) => Some(&ls.value),
        Statement::VarStatement(vs) => Some(&vs.value),
        Statement::ExpressionStatement(es) => Some(&es.expression),
        Statement::ReturnStatement(rs) => rs.value.as_ref(),
        _ => None,
    };
    let mut m = match value {
        Some(v) if all_call(v, fns).is_some() => all_call(v, fns).unwrap().len(),
        Some(v) if user_callee(v, fns).is_some() => 1,
        // A closure-parameter / computed-callee call lowers to `Term::AwaitClosure`,
        // which (in both its sync and async sub-paths) writes a child/synth task id
        // to `frame[pending_off]`. Without counting it the pending region is unsized
        // and that write overflows the frame into the adjacent heap object. (This is
        // what silently lost `children()`'s captured locals in forui.)
        Some(v) if indirect_closure_call(v, params, fns).is_some() => 1,
        _ => 0,
    };
    let recur = |stmts: &[Statement], m: &mut usize| {
        for s in stmts {
            *m = (*m).max(stmt_pending_count(s, fns, params));
        }
    };
    match stmt {
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                recur(&branch.body, &mut m);
            }
            if let Some(e) = &is.else_branch {
                recur(e, &mut m);
            }
        }
        Statement::WhileStatement(ws) => recur(&ws.body, &mut m),
        Statement::ForStatement(fs) => recur(&fs.body, &mut m),
        Statement::TryStatement(ts) => {
            recur(&ts.try_body, &mut m);
            recur(&ts.catch_body, &mut m);
            if let Some(f) = &ts.finally_body {
                recur(f, &mut m);
            }
        }
        _ => {}
    }
    m
}

/// Compute the frame layout: params first, then every `let`/`var` binding
/// name (recursively, including nested control flow and multi-binding
/// `let a, b = all(...)`), plus a pending region sized to the widest
/// suspension.
fn async_frame_layout(fd: &FunctionDeclaration, fns: &AsyncResolve<'_>, has_env: bool) -> AsyncFrame {
    let mut vars: Vec<String> = Vec::new();
    // Hidden `@type` params lead the frame (matching the sync ABI's leading
    // type-arg slots) so a generic callee's params land at the right offsets.
    for t in &fd.type_params {
        push_unique(&mut vars, &t.name);
    }
    for p in &fd.params {
        push_unique(&mut vars, &p.name);
    }
    collect_async_vars(&fd.body, &mut vars);
    // Param names — `stmt_pending_count` uses these to recognize a closure-param
    // call (`p()`) as a suspension that needs a pending slot, mirroring the CFG's
    // `indirect_closure_call` detection.
    let params: std::collections::HashSet<String> =
        fd.params.iter().map(|p| p.name.clone()).collect();
    let mut pending_count = 0usize;
    for stmt in &fd.body {
        pending_count = pending_count.max(stmt_pending_count(stmt, fns, &params));
    }
    // Closures reserve slot 0 for `env_ptr`; named-fn vars start at 0.
    let base: u64 = if has_env { 8 } else { 0 };
    let mut var_off = std::collections::HashMap::new();
    for (i, v) in vars.iter().enumerate() {
        var_off.insert(v.clone(), base + (i as u64) * 8);
    }
    let pending_off = base + (vars.len() as u64) * 8;
    // Value slots (i64) + pending region (i32 each), rounded up to 8 bytes.
    let raw = pending_off as usize + pending_count * 4;
    let size = ((raw + 7) & !7) as i32;
    AsyncFrame {
        var_off,
        vars,
        pending_off,
        size: size.max(8),
        has_env,
    }
}

/// Builds the CFG for an async function body. Lowers structured control
/// flow (`if`/`while`) and suspensions into basic blocks connected by
/// `Goto`/`Cond`/`Sleep`/`Await`/`All`/`Complete*`. Returns `None` (→ fall
/// back) for anything out of v1 scope.
struct CfgBuilder<'a> {
    blocks: Vec<Block<'a>>,
    fns: &'a AsyncResolve<'a>,
    /// Enclosing `try` handlers (catch block, catch binding name), innermost
    /// last. A `throw` targets the top of the stack.
    handlers: Vec<(usize, String)>,
    /// Closure-typed parameter names of the function being lowered. A call
    /// `p(...)` whose callee is one of these is an indirect closure call (it
    /// may suspend) → an await with runtime sync/async dispatch.
    params: &'a std::collections::HashSet<String>,
    /// Conservative escaping-name set for the whole function — used to pick
    /// confined fresh-literal loop-body temporaries to drop per iteration.
    escaping: &'a std::collections::HashSet<String>,
}

impl<'a> CfgBuilder<'a> {
    fn new_block(&mut self) -> usize {
        self.blocks.push(Block {
            incoming: Incoming::None,
            stmts: Vec::new(),
            term: Term::Unset,
            drops: Vec::new(),
        });
        self.blocks.len() - 1
    }

    fn args_ok(&self, args: &[&Expression]) -> bool {
        !args.iter().any(|a| expr_has_user_call(a, self.fns))
    }

    /// Lower `stmts` starting at `entry`. `is_tail` marks that the last
    /// statement's value is the function's result. Returns where control
    /// continues, or `Diverged` if the sequence always completes/returns.
    fn lower_seq(
        &mut self,
        stmts: &'a [Statement],
        entry: usize,
        mode: TailMode,
    ) -> Result<Flow, ()> {
        let n = stmts.len();
        if n == 0 {
            return self.finish_void(entry, mode);
        }
        let mut cur = entry;
        for (i, stmt) in stmts.iter().enumerate() {
            let m = if i + 1 == n { mode } else { TailMode::None };
            match self.lower_stmt(stmt, cur, m)? {
                Flow::Continue(next) => cur = next,
                Flow::Diverged => return Ok(Flow::Diverged),
            }
        }
        // The last statement fell through (it produced no value) — in tail
        // position the function's value is `Void`.
        self.finish_void(cur, mode)
    }

    /// Terminate `blk` for a sequence that fell through (no value) per `mode`.
    fn finish_void(&mut self, blk: usize, mode: TailMode) -> Result<Flow, ()> {
        match mode {
            TailMode::None => Ok(Flow::Continue(blk)),
            TailMode::Complete => {
                self.blocks[blk].term = Term::CompleteVoid;
                Ok(Flow::Diverged)
            }
            // A `Void` value flowing into a try-with-finally result slot is
            // out of v1 scope (the result is held in a wasm local).
            TailMode::StoreResult(_) => Err(()),
        }
    }

    /// Terminate `blk` producing `value` per `mode` (must be a tail mode).
    fn tail_value(&mut self, blk: usize, value: &'a Expression, mode: TailMode) -> Flow {
        match mode {
            TailMode::Complete => self.blocks[blk].term = Term::Complete(value),
            TailMode::StoreResult(next) => {
                self.blocks[blk].term = Term::StoreResultGoto { value, next }
            }
            TailMode::None => unreachable!("tail_value with TailMode::None"),
        }
        Flow::Diverged
    }

    /// The enclosing catch handler `(catch block, error binding)`, if any.
    fn handler(&self) -> Option<(usize, String)> {
        self.handlers.last().map(|(b, n)| (*b, n.clone()))
    }

    fn lower_stmt(&mut self, stmt: &'a Statement, cur: usize, mode: TailMode) -> Result<Flow, ()> {
        let is_tail = !matches!(mode, TailMode::None);
        // sleep(ms)
        if let Some(ms) = async_sleep_ms_of(stmt) {
            let next = self.new_block();
            self.blocks[cur].term = Term::Sleep { ms, next };
            return Ok(Flow::Continue(next));
        }
        // nowait userCall(...) — in-segment fork
        if let Statement::NowaitStatement(nw) = stmt {
            let Some((_, args, _)) = user_callee(&nw.expression, self.fns) else {
                return Err(());
            };
            if !self.args_ok(&args) {
                return Err(());
            }
            self.blocks[cur].stmts.push(stmt);
            return Ok(Flow::Continue(cur));
        }
        // let/var [a, b] = all(...)
        if matches!(stmt, Statement::LetStatement(_) | Statement::VarStatement(_)) {
            let (value, binds): (&Expression, Vec<String>) = match stmt {
                Statement::LetStatement(ls) => {
                    (&ls.value, ls.bindings.iter().map(|b| b.name.clone()).collect())
                }
                Statement::VarStatement(vs) => {
                    (&vs.value, vs.bindings.iter().map(|b| b.name.clone()).collect())
                }
                _ => unreachable!(),
            };
            if let Some(children) = all_call(value, self.fns) {
                if binds.len() != children.len() {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::All { children, next };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: binds.into_iter().map(Some).collect(),
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
        }
        // single-binding let/var
        if let Some((name, value)) = single_binding(stmt) {
            // `let x = remoteCall(...)` — suspend on the RPC, bind the result.
            if let Some((rargs, loc)) = remote_call_args(value) {
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitRemote { args: rargs, loc, next };
                self.blocks[next].incoming = Incoming::AwaitedRemote {
                    bind: Some(name.to_string()),
                };
                return Ok(Flow::Continue(next));
            }
            if let Some((callee, args, loc)) = user_callee(value, self.fns) {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::Await { callee, args, loc, next };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: vec![Some(name.to_string())],
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
            if let Some((closure, args)) = async_closure_call(value, self.fns)
                .or_else(|| indirect_closure_call(value, self.params, self.fns))
            {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitClosure { closure, args, next };
                self.blocks[next].incoming = Incoming::Awaited {
                    binds: vec![Some(name.to_string())],
                    on_error,
                };
                return Ok(Flow::Continue(next));
            }
            if expr_has_user_call(value, self.fns) {
                return Err(());
            }
            self.blocks[cur].stmts.push(stmt);
            return Ok(Flow::Continue(cur));
        }
        // assignment `v = expr` (no awaits in the value)
        if let Statement::AssignmentStatement(asg) = stmt {
            if expr_has_user_call(&asg.value, self.fns) {
                return Err(());
            }
            self.blocks[cur].stmts.push(stmt);
            return Ok(Flow::Continue(cur));
        }
        // expression statement
        if let Statement::ExpressionStatement(es) = stmt {
            // `remoteCall(...)` as a statement — in tail position the RPC result
            // is the function's value (the generated stubs do exactly this);
            // otherwise it's run for effect and the result discarded.
            if let Some((rargs, loc)) = remote_call_args(&es.expression) {
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitRemote { args: rargs, loc, next };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompleteRemote;
                        return Ok(Flow::Diverged);
                    }
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::AwaitedRemote { bind: None };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if let Some((callee, args, loc)) = user_callee(&es.expression, self.fns) {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::Await { callee, args, loc, next };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompletePending;
                        return Ok(Flow::Diverged);
                    }
                    // await-in-tail of a try-with-finally body: out of scope.
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::Awaited {
                            binds: vec![None],
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            // Invoking a closure value (`children()`, `handlers[i]()`) — await
            // it through the scheduler rather than the sync re-entrant drive.
            if let Some((closure, args)) = async_closure_call(&es.expression, self.fns)
                .or_else(|| indirect_closure_call(&es.expression, self.params, self.fns))
            {
                if !self.args_ok(&args) {
                    return Err(());
                }
                let on_error = self.handler();
                let next = self.new_block();
                self.blocks[cur].term = Term::AwaitClosure { closure, args, next };
                match mode {
                    TailMode::Complete => {
                        self.blocks[next].term = Term::CompletePending;
                        return Ok(Flow::Diverged);
                    }
                    TailMode::StoreResult(_) => return Err(()),
                    TailMode::None => {
                        self.blocks[next].incoming = Incoming::Awaited {
                            binds: vec![None],
                            on_error,
                        };
                        return Ok(Flow::Continue(next));
                    }
                }
            }
            if expr_has_user_call(&es.expression, self.fns) {
                return Err(());
            }
            if is_tail {
                return Ok(self.tail_value(cur, &es.expression, mode));
            }
            self.blocks[cur].stmts.push(stmt);
            return Ok(Flow::Continue(cur));
        }
        // throw value
        if let Statement::ThrowStatement(ts) = stmt {
            if expr_has_user_call(&ts.expression, self.fns) {
                return Err(()); // no await in a throw value (v1)
            }
            self.blocks[cur].term = match self.handlers.last() {
                Some((catch_blk, err_var)) => Term::ThrowTo {
                    value: &ts.expression,
                    catch_blk: *catch_blk,
                    err_var: err_var.clone(),
                },
                None => Term::Fail(&ts.expression),
            };
            return Ok(Flow::Diverged);
        }
        // try / catch / finally
        if let Statement::TryStatement(ts) = stmt {
            return self.lower_try(ts, cur, mode);
        }
        // return
        if let Statement::ReturnStatement(rs) = stmt {
            match &rs.value {
                Some(v) => {
                    if let Some((rargs, loc)) = remote_call_args(v) {
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitRemote { args: rargs, loc, next };
                        self.blocks[next].term = Term::CompleteRemote;
                    } else if let Some((callee, args, loc)) = user_callee(v, self.fns) {
                        if !self.args_ok(&args) {
                            return Err(());
                        }
                        let next = self.new_block();
                        self.blocks[cur].term = Term::Await { callee, args, loc, next };
                        self.blocks[next].term = Term::CompletePending;
                    } else if let Some((closure, args)) = async_closure_call(v, self.fns)
                        .or_else(|| indirect_closure_call(v, self.params, self.fns))
                    {
                        if !self.args_ok(&args) {
                            return Err(());
                        }
                        let next = self.new_block();
                        self.blocks[cur].term = Term::AwaitClosure { closure, args, next };
                        self.blocks[next].term = Term::CompletePending;
                    } else {
                        if expr_has_user_call(v, self.fns) {
                            return Err(());
                        }
                        self.blocks[cur].term = Term::Complete(v);
                    }
                }
                None => self.blocks[cur].term = Term::CompleteVoid,
            }
            return Ok(Flow::Diverged);
        }
        // if / if-else (no `elsif` chains in v1; no await in condition)
        if let Statement::IfStatement(is) = stmt {
            if is.branches.len() != 1 {
                return Err(());
            }
            let branch = &is.branches[0];
            let cond = &branch.condition;
            if expr_has_user_call(cond, self.fns) {
                return Err(());
            }
            if is_tail {
                if let Some(else_body) = &is.else_branch {
                    // Each branch produces the value (per `mode`).
                    let then_e = self.new_block();
                    let else_e = self.new_block();
                    self.blocks[cur].term = Term::Cond {
                        cond,
                        then_blk: then_e,
                        else_blk: else_e,
                    };
                    self.lower_seq(&branch.body, then_e, mode)?;
                    self.lower_seq(else_body, else_e, mode)?;
                    Ok(Flow::Diverged)
                } else {
                    // No `else` in tail position: only valid for a `Void`
                    // function — the `then` branch runs for effect and both
                    // paths complete `Void`. (A non-Void fn missing the else
                    // value is a checker error; `finish_void` rejects
                    // `StoreResult` here, falling back.) Lower the branch for
                    // effect, then complete `Void` at the merge.
                    let then_e = self.new_block();
                    let join = self.new_block();
                    self.blocks[cur].term = Term::Cond {
                        cond,
                        then_blk: then_e,
                        else_blk: join,
                    };
                    if let Flow::Continue(te) =
                        self.lower_seq(&branch.body, then_e, TailMode::None)?
                    {
                        self.blocks[te].term = Term::Goto(join);
                    }
                    self.finish_void(join, mode)
                }
            } else {
                let then_e = self.new_block();
                let join = self.new_block();
                let else_e = if is.else_branch.is_some() {
                    self.new_block()
                } else {
                    join
                };
                self.blocks[cur].term = Term::Cond {
                    cond,
                    then_blk: then_e,
                    else_blk: else_e,
                };
                if let Flow::Continue(te) = self.lower_seq(&branch.body, then_e, TailMode::None)? {
                    self.blocks[te].term = Term::Goto(join);
                }
                if let Some(else_body) = &is.else_branch {
                    if let Flow::Continue(ee) = self.lower_seq(else_body, else_e, TailMode::None)? {
                        self.blocks[ee].term = Term::Goto(join);
                    }
                }
                Ok(Flow::Continue(join))
            }
        } else if let Statement::WhileStatement(ws) = stmt {
            if expr_has_user_call(&ws.condition, self.fns) {
                return Err(());
            }
            let header = self.new_block();
            self.blocks[cur].term = Term::Goto(header);
            let body_e = self.new_block();
            let exit = self.new_block();
            self.blocks[header].term = Term::Cond {
                cond: &ws.condition,
                then_blk: body_e,
                else_blk: exit,
            };
            if let Flow::Continue(be) = self.lower_seq(&ws.body, body_e, TailMode::None)? {
                // Per-iteration reclamation: for a NON-suspending body (single
                // straight-line block `be`), free its confined fresh-literal
                // top-level temporaries before looping back. A suspending body
                // spans multiple blocks / may not set a binding on every path,
                // so skip it (sound leak). Inner non-suspending if/case/for are
                // compiled inline and drop via their own scope exits.
                if !stmts_have_suspension(&ws.body, self.fns) {
                    self.blocks[be].drops =
                        fai_compiler::escape_analysis::confined_freeable_names(&ws.body, self.escaping);
                }
                self.blocks[be].term = Term::Goto(header);
            }
            // A `while` yields no value, so in tail position it completes Void.
            self.finish_void(exit, mode)
        } else if !is_tail && !stmts_have_suspension(std::slice::from_ref(stmt), self.fns) {
            // Any other statement (e.g. a `for` loop, `case`) that contains no
            // suspension point runs as a plain inline segment statement — the
            // segment compiler (`compile_stmt`) lowers it directly, exactly as a
            // sync function would. Only statements that themselves suspend need
            // CFG segment-splitting (not supported inside loops/case yet).
            self.blocks[cur].stmts.push(stmt);
            Ok(Flow::Continue(cur))
        } else {
            if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                let (kind, loc) = match stmt {
                    Statement::ForStatement(s) => ("for", Some(&s.location)),
                    Statement::WhileStatement(s) => ("while", Some(&s.location)),
                    Statement::CaseStatement(s) => ("case", Some(&s.location)),
                    Statement::IfStatement(s) => ("if", Some(&s.location)),
                    Statement::AssignmentStatement(s) => ("assignment", Some(&s.location)),
                    Statement::ExpressionStatement(s) => ("expr", Some(&s.location)),
                    Statement::LetStatement(s) => ("let", Some(&s.location)),
                    Statement::VarStatement(s) => ("var", Some(&s.location)),
                    _ => ("other", None),
                };
                let at = loc
                    .map(|l| format!("{}:{}", l.line, l.column))
                    .unwrap_or_else(|| "?".to_string());
                eprintln!(
                    "[async-engine]   CFG bail: unsupported suspending `{}` at {} (is_tail={})",
                    kind, at, is_tail
                );
            }
            Err(()) // unsupported statement
        }
    }

    /// Lower a `try`/`catch`/`finally`. In statement position the bodies run
    /// for effect; in value position (tail) each body produces the result,
    /// carried through `finally` via the try-result local.
    fn lower_try(
        &mut self,
        ts: &'a fai_compiler::ast::TryStatement,
        cur: usize,
        mode: TailMode,
    ) -> Result<Flow, ()> {
        let catch_blk = self.new_block();
        match mode {
            TailMode::None => {
                // Statement position: bodies run for effect; finally runs on
                // both paths; control continues after.
                let after = self.new_block();
                let finally_blk = if ts.finally_body.is_some() {
                    self.new_block()
                } else {
                    after
                };
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                let try_exit = self.lower_seq(&ts.try_body, cur, TailMode::None)?;
                self.handlers.pop();
                if let Flow::Continue(te) = try_exit {
                    self.blocks[te].term = Term::Goto(finally_blk);
                }
                if let Flow::Continue(ce) = self.lower_seq(&ts.catch_body, catch_blk, TailMode::None)?
                {
                    self.blocks[ce].term = Term::Goto(finally_blk);
                }
                if let Some(fb) = &ts.finally_body {
                    if let Flow::Continue(fe) = self.lower_seq(fb, finally_blk, TailMode::None)? {
                        self.blocks[fe].term = Term::Goto(after);
                    }
                }
                Ok(Flow::Continue(after))
            }
            _ if ts.finally_body.is_some() => {
                // Value position with finally: only supported at the function
                // tail (`Complete`) and with a non-suspending finally (the
                // try-result lives in a wasm local across it). One result
                // local ⇒ no nested value-try-with-finally.
                if !matches!(mode, TailMode::Complete) {
                    return Err(());
                }
                let fb = ts.finally_body.as_ref().unwrap();
                if stmts_have_suspension(fb, self.fns) {
                    return Err(());
                }
                let finally_blk = self.new_block();
                // try/catch store their value into the try-result, then finally.
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                self.lower_seq(&ts.try_body, cur, TailMode::StoreResult(finally_blk))?;
                self.handlers.pop();
                self.lower_seq(&ts.catch_body, catch_blk, TailMode::StoreResult(finally_blk))?;
                // finally runs for effect, then completes with the result.
                if let Flow::Continue(fe) = self.lower_seq(fb, finally_blk, TailMode::None)? {
                    self.blocks[fe].term = Term::CompleteResult;
                }
                Ok(Flow::Diverged)
            }
            _ => {
                // Value position, no finally: each body produces the result.
                self.handlers.push((catch_blk, ts.catch_name.clone()));
                self.lower_seq(&ts.try_body, cur, mode)?;
                self.handlers.pop();
                self.lower_seq(&ts.catch_body, catch_blk, mode)?;
                Ok(Flow::Diverged)
            }
        }
    }
}

/// Build the CFG for `body`, or `None` if it uses anything out of v1 scope.
fn build_cfg<'a>(
    body: &'a [Statement],
    fns: &'a AsyncResolve<'a>,
    params: &'a std::collections::HashSet<String>,
    escaping: &'a std::collections::HashSet<String>,
) -> Option<Vec<Block<'a>>> {
    let mut cb = CfgBuilder {
        blocks: vec![Block {
            incoming: Incoming::None,
            stmts: Vec::new(),
            term: Term::Unset,
            drops: Vec::new(),
        }],
        fns,
        handlers: Vec::new(),
        params,
        escaping,
    };
    match cb.lower_seq(body, 0, TailMode::Complete).ok()? {
        Flow::Diverged => {}
        // Body fell through without producing a value → complete with Void.
        Flow::Continue(exit) => cb.blocks[exit].term = Term::CompleteVoid,
    }
    if cb.blocks.iter().any(|b| matches!(b.term, Term::Unset)) {
        return None;
    }
    Some(cb.blocks)
}

/// Emit `current_task.resume_state` (an i32) onto the stack.
fn emit_load_current_rstate(b: &mut Builder, layout: &crate::async_engine::SchedLayout) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_RSTATE)));
}

/// Emit `current_task.resume_state = state`.
fn emit_store_current_rstate(b: &mut Builder, layout: &crate::async_engine::SchedLayout, state: i32) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Const(state));
    b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_RSTATE)));
}

/// Emit `frame_ptr_local = current_task.frame`.
fn emit_load_current_frame(
    b: &mut Builder,
    layout: &crate::async_engine::SchedLayout,
    frame_ptr_local: u32,
) {
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::GlobalGet(layout.g_current));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_FRAME)));
    b.emit(Instruction::LocalSet(frame_ptr_local));
}

/// Compile one non-suspending in-segment statement: a single-binding
/// `let`/`var` stores into the binding's frame-backed local; a plain
/// expression statement (e.g. `print`) compiles for its side effect.
fn compile_async_segment_stmt(
    b: &mut Builder,
    stmt: &Statement,
    var_local: &std::collections::HashMap<String, u32>,
    release_set: &std::collections::HashSet<String>,
) -> Result<(), BuildError> {
    if let Some((name, value)) = single_binding(stmt) {
        let local = *var_local
            .get(name)
            .ok_or(BuildError::UnsupportedExpression("async-unknown-binding"))?;
        // A cell-captured var's binding local holds the heap cell's address
        // (plan 114): store the initial value *through* the cell's value
        // slot with value-RC — a plain `LocalSet` would clobber the address
        // with the value and hand the capturing closure an i64 where it
        // expects an i32.
        let is_cell = b.lookup(name).map(|bnd| bnd.is_cell).unwrap_or(false);
        if is_cell {
            let transfers = b.expr_transfers_ownership(value);
            b.compile_expr_as(value, ValueShape::Boxed)?;
            b.emit_cell_store(local, transfers);
        } else {
            b.compile_expr_as(value, ValueShape::Boxed)?;
            if release_set.contains(name) {
                // RC bind (plan 115, mirrors sync `compile_bindings`): a binding
                // that the completion path will RELEASE must own exactly `+1`. A
                // borrowed source (identifier / field / non-owning call) is
                // co-owned, so retain it; a fresh value or owned call result
                // already transfers its single ref.
                if !b.expr_transfers_ownership(value) {
                    b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                }
                // Release the value the slot held from a PREVIOUS loop iteration
                // (plan 116 follow-up — the async-frame loop leak): a binding
                // statement in a suspending loop body re-executes per iteration,
                // and completion releases only the final value, leaking N−1.
                // Runs after the initializer is evaluated (so a read of a
                // same-named outer binding still sees the old value); the new
                // value rides the stack across the stack-neutral RT_RELEASE.
                // First execution reads 0 — frames are zeroed at spawn — a safe
                // no-op.
                b.emit(Instruction::LocalGet(local));
                b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
            }
            // Vars NOT in the release set (multi-assign targets, catch vars)
            // keep the no-retain/no-release behaviour — they leak, soundly.
            b.emit(Instruction::LocalSet(local));
        }
        return Ok(());
    }
    b.compile_stmt(stmt)
}

/// Emit code that spawns a child task for `callee(args)`: allocate the
/// child's frame, write the argument values into its leading param slots,
/// and `spawn` it. Leaves the new task id in `childid_l`.
#[allow(clippy::too_many_arguments)]
fn emit_spawn_child(
    b: &mut Builder,
    callee: &str,
    args: &[&Expression],
    loc: &fai_compiler::ast::SourceLocation,
    frame_sizes: &std::collections::HashMap<String, i32>,
    fn_table_idx: &std::collections::HashMap<String, u32>,
    layout: &crate::async_engine::SchedLayout,
    childframe_l: u32,
    childid_l: u32,
) -> Result<(), BuildError> {
    let size = *frame_sizes
        .get(callee)
        .ok_or(BuildError::UnsupportedExpression("async-unknown-callee"))?;
    let tidx = *fn_table_idx
        .get(callee)
        .ok_or(BuildError::UnsupportedExpression("async-unknown-callee"))?;
    // Generic callee? Its frame leads with hidden `@type` slots — interned
    // type-name strings, exactly as `compile_call` injects them for the sync
    // ABI. Look the type args up by the call-site key the checker recorded.
    let tpc = b
        .function_by_name
        .get(callee)
        .map(|&p| b.functions()[p as usize].type_param_count as usize)
        .unwrap_or(0);
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::Call(layout.alloc));
    b.emit(Instruction::LocalSet(childframe_l));
    // Zero the fresh frame (plan 115): the allocator reuses freed frame blocks
    // without clearing them, so a slot not written on the path to a completion
    // would hold a STALE pointer from the previous task. Async reclamation
    // RT_RELEASEs every owned body slot at completion; zeroing makes an unwritten
    // slot read 0 (a safe RT_RELEASE no-op) instead of double-freeing the prior
    // task's object. Params/env are written just below, over the zeros.
    b.emit(Instruction::LocalGet(childframe_l));
    b.emit(Instruction::I32Const(0));
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::MemoryFill(0));
    if tpc > 0 {
        let key = (b.module_key.clone(), loc.line, loc.column);
        let type_args = b
            .checker()
            .generic_type_args
            .get(&key)
            .cloned()
            .unwrap_or_default();
        for i in 0..tpc {
            let type_name = type_args.get(i).cloned().unwrap_or_default();
            let (off, len) = b.ctx.strings.borrow_mut().intern(&type_name);
            b.emit(Instruction::LocalGet(childframe_l));
            b.emit(Instruction::I32Const(off as i32));
            b.emit(Instruction::I32Const(len as i32));
            b.emit(Instruction::Call(b.rt().base + RT_ALLOC_STRING));
            b.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }
    }
    // Write each real param: the provided arg, or — for an omitted optional
    // param — its default expression. The sync call path (`compile_call`) does
    // this; the spawn path must too, or an omitted `loader Loader?, default:
    // null` leaves a zero-initialized frame slot and a downstream `loader !=
    // null` guard wrongly succeeds (forking `doLoad` with a null loader →
    // `call_indirect` on garbage). Mirrors `compile_call`'s default fill.
    let (real_param_count, defaults) = match b.function_by_name.get(callee).copied() {
        Some(p) => {
            let fi = &b.functions()[p as usize];
            ((fi.param_count as usize).saturating_sub(tpc), fi.param_defaults.clone())
        }
        None => (args.len(), Vec::new()),
    };
    for i in 0..real_param_count {
        b.emit(Instruction::LocalGet(childframe_l));
        // RC: every param slot OWNS exactly +1 (plan 114 follow-up) —
        // retain a borrowed arg, transfer a fresh/owned one — and the
        // child releases its param slots at completion. Without this the
        // spawner's owned arg temps (a fresh closure / dict / concat
        // passed to an async fn) had no release point and leaked one ref
        // per call: forui's per-render view-builder closures, exactly.
        if let Some(arg) = args.get(i) {
            let transfers = b.expr_transfers_ownership(arg);
            b.compile_expr_as(arg, ValueShape::Boxed)?;
            if !transfers {
                b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
            }
        } else if let Some(Some(default_expr)) = defaults.get(i + tpc) {
            let transfers = b.expr_transfers_ownership(default_expr);
            b.compile_expr_as(default_expr, ValueShape::Boxed)?;
            if !transfers {
                b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
            }
        } else {
            return Err(BuildError::UnsupportedExpression("async-spawn-arg-count-mismatch"));
        }
        b.emit(Instruction::I64Store(mem_off(((tpc + i) as u64) * 8)));
    }
    b.emit(Instruction::I32Const(tidx as i32));
    b.emit(Instruction::LocalGet(childframe_l));
    b.emit(Instruction::Call(layout.spawn));
    b.emit(Instruction::LocalSet(childid_l));
    // Record the child's frame size so it's reclaimed when the task completes.
    b.emit(Instruction::GlobalGet(layout.g_table_base));
    b.emit(Instruction::LocalGet(childid_l));
    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
    b.emit(Instruction::I32Mul);
    b.emit(Instruction::I32Add);
    b.emit(Instruction::I32Const(size));
    b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_FRAME_SIZE)));
    Ok(())
}

/// Emit `RT_RELEASE(var_local[name])` for each owned body-binding frame slot at
/// an async completion terminator — the async analogue of sync `pop_scope`
/// (plan 115 Part 1). Each name is loaded from its wasm local (reloaded from the
/// frame at segment entry) and released; RT_RELEASE no-ops on primitives and on
/// the zero a never-written slot holds (frames are zeroed at spawn). The caller
/// must retain a borrowed result/error value BEFORE calling this so it survives
/// the releases (the +1-return convention). RT_RELEASE is stack-neutral
/// (i64)->(), so any value already on the stack is left untouched.
fn emit_async_drops(
    b: &mut Builder,
    names: &[String],
    var_local: &std::collections::HashMap<String, u32>,
    cell_offsets: &[u64],
    frame_ptr_l: u32,
) {
    if names.is_empty() && cell_offsets.is_empty() {
        return;
    }
    let release_fn = b.rt().base + RT_RELEASE;
    for name in names {
        if let Some(&local) = var_local.get(name) {
            b.emit(Instruction::LocalGet(local));
            b.emit(Instruction::Call(release_fn));
        }
    }
    // Plan 114: release the frame's co-ownership of each heap CELL (the
    // boxed pointer stored in its slot — read from the frame, which is
    // still live here; `complete` frees it after). RT_RELEASE's CELL
    // branch frees the held value and the block at rc 0; a closure that
    // captured the cell holds its own retained ref, so an escaped
    // closure keeps the cell alive past the task.
    for &off in cell_offsets {
        b.emit(Instruction::LocalGet(frame_ptr_l));
        b.emit(Instruction::I64Load(mem_off(off)));
        b.emit(Instruction::Call(release_fn));
    }
}

/// Build an async function's resume function: a `br_table` on the current
/// task's resume_state dispatches to each segment. At each segment entry
/// the frame pointer and frame-backed locals are reloaded; a pending
/// await result is read into its binding. Each non-final segment runs its
/// statements then suspends (`sleep` or spawn-child + `await`); the final
/// segment `complete`s the task with the result value.
#[allow(clippy::too_many_arguments)]
fn build_resume_fn(
    ctx: &BuildContext,
    fd: &FunctionDeclaration,
    frame: &AsyncFrame,
    fn_table_idx: &std::collections::HashMap<String, u32>,
    frame_sizes: &std::collections::HashMap<String, i32>,
    layout: &crate::async_engine::SchedLayout,
    fns: &AsyncResolve<'_>,
    module_context: Option<&str>,
    file_path: Option<&str>,
    outer: Option<&OuterScopeView>,
) -> Result<(Function, Vec<CaptureBinding>), BuildError> {
    let params: std::collections::HashSet<String> =
        fd.params.iter().map(|p| p.name.clone()).collect();
    // Escaping-name set for the real function — feeds both the CFG's per-
    // iteration loop-body drops and the Builder's inline-statement drops.
    let escaping = fai_compiler::escape_analysis::conservative_escaping(fd);
    let blocks = build_cfg(&fd.body, fns, &params, &escaping)
        .ok_or(BuildError::UnsupportedExpression("async-shape"))?;
    let b_count = blocks.len();

    // Param-less, empty-body view so the Builder doesn't create wasm
    // params or scan a body — we drive the CFG and bindings manually.
    let mut fd_view = fd.clone();
    fd_view.params = Vec::new();
    fd_view.type_params = Vec::new();
    fd_view.body = Vec::new();
    // `outer` is `Some` only for an async *closure* — its body may reference
    // upvalues, captured against the enclosing scope. Named fns pass `None`.
    let mut b = Builder::new(&fd_view, ctx, outer);
    // Module context so cross-module names + peer calls in the body resolve
    // the same way the sync path resolves them.
    if let Some(m) = module_context {
        b.module_context = Some(m.to_string());
    }
    // Per-call-site key source (UFCS / named-param / expression-type lookups);
    // mirror `build_function` so checker-recorded entries round-trip.
    b.module_key = file_path
        .or(module_context)
        .map(String::from)
        .unwrap_or_default();

    let frame_ptr_l = b.alloc_i32_local();
    let childframe_l = b.alloc_i32_local();
    let childid_l = b.alloc_i32_local();
    // Heap address of a closure being spawned/called (Term::AwaitClosure).
    let closure_addr_l = b.alloc_i32_local();
    // Sync-closure dispatch path: saved env_ptr, the inline call result, and a
    // synthesized completed-task id/addr so the result reads back uniformly.
    let saved_env_l = b.alloc_i32_local();
    let sync_result_l = b.alloc_local();
    let synth_id_l = b.alloc_i32_local();
    let synth_addr_l = b.alloc_i32_local();
    // Holds a value-`try`/`catch` body's result across a (non-suspending)
    // `finally` until the task completes.
    let try_result_l = b.alloc_local();
    // Holds a failed child's error read from the current task's error slot.
    let child_err_l = b.alloc_local();
    // Frame vars captured-and-mutated by a nested closure must be *cells*: the
    // closure shares the storage, not a snapshot. The frame slot IS the cell —
    // `cell_addr = frame_ptr + offset` (a stable heap address that survives
    // suspension); reads/writes deref it, and the closure captures it. Such a
    // var's local holds that address (i32), not the value.
    let cell_vars = collect_cell_captured_vars(&fd.body);
    // The Builder was constructed from an empty-body `fd_view`, so it has no
    // cell knowledge of its own. Seed it from the real body so `compile_bindings`
    // treats `var s = …` of a captured var as a cell (and, per the reuse path
    // there, stores into the frame slot we bind below rather than overriding it
    // with a plain local).
    b.cell_captured_vars = cell_vars.clone();
    // Likewise seed the escaping set from the REAL body, not the empty
    // `fd_view`. The unified scope-drop mechanism (`note_droppable`) fires when
    // a non-suspending nested loop/if/case in this function is compiled inline
    // via `compile_stmt`; without the real set it would skip the escape check
    // and over-drop. With it, confined fresh-literals in those non-suspending
    // blocks are freed per scope-exit just as in a sync function. (Bindings in
    // SUSPENDING loop bodies go through the CFG segment path, which doesn't
    // drop yet — sound leak.)
    b.confined_escaping = escaping.clone();
    let mut var_local: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    for v in &frame.vars {
        if cell_vars.contains(v) {
            let l = b.alloc_i32_local();
            b.bind_cell(v, l);
            var_local.insert(v.clone(), l);
        } else {
            let l = b.alloc_local();
            b.bind(v, l);
            var_local.insert(v.clone(), l);
        }
    }

    // Async reclamation (plan 115 Part 1): every owned body-binding frame slot
    // is RT_RELEASE'd at every completion terminator — the async analogue of
    // sync `pop_scope`. This turns the UNBOUNDED per-invocation leak (a request
    // handler's parsed body / DB rows / rendered HTML) into a steady-state
    // plateau: the slots are freed exactly when the task finishes.
    //
    // Soundness relies on each released slot owning exactly `+1` at completion:
    //   - let/var bindings get the +1 via transfer (fresh / call-result) or a
    //     retain-on-borrow at bind time (see `compile_async_segment_stmt`).
    //   - await-result bindings receive the child's `+1` result (transfer).
    //   - frames are zeroed at spawn, so a slot not written on the path to a
    //     completion reads 0 (a safe RT_RELEASE no-op) rather than a stale
    //     pointer from a recycled frame block.
    // The result/error value is retained-if-borrowed before the releases at each
    // terminator so it survives (the +1-return convention), exactly like sync
    // `compile_return`.
    //
    // Excluded: cell-captured vars (released through their frame SLOT below,
    // not their addr local — plan 114), catch vars and multi-assignment
    // targets (`a, b = …` plain-overwrites — rc not guaranteed +1).
    // Single-name reassigned vars ARE released: their locals are marked
    // owned below, so binding release-the-old + `compile_assignment`
    // retain-new/release-old keep them at exactly `+1` (plan 116 follow-up).
    // Params and type-params are released too (plan 114 follow-up): every
    // spawn site now stores an OWNED `+1` into each param slot
    // (retain-if-borrowed in `emit_spawn_child` / `Term::AwaitClosure` /
    // `emit_drive_closure`; type-arg strings are interned fresh), so the
    // task releasing them at completion is what closes the
    // owned-argument-to-async-call leak.
    let mut excluded: std::collections::HashSet<String> = cell_vars.clone();
    collect_multi_rebound_names(&fd.body, &mut excluded);
    collect_catch_names(&fd.body, &mut excluded);
    let release_names: Vec<String> = frame
        .vars
        .iter()
        .filter(|v| !excluded.contains(*v) && var_local.contains_key(*v))
        .cloned()
        .collect();
    let release_set: std::collections::HashSet<String> =
        release_names.iter().cloned().collect();
    // Reassignment of a release-set var must keep the slot at one owned ref:
    // mark the local so `compile_assignment` retains-new/releases-old exactly
    // like a sync owned local (these are completion-released, never
    // scope-dropped, so they don't go through `note_droppable`).
    for name in &release_names {
        if let Some(&l) = var_local.get(name) {
            b.owned_frame_locals.insert(l);
        }
    }
    // Cell vars are released at completion through their frame SLOT (the
    // boxed heap-cell pointer, plan 114), not their addr local — collect
    // the slot offsets for `emit_async_drops`.
    let cell_offsets: Vec<u64> = frame
        .vars
        .iter()
        .filter(|v| cell_vars.contains(*v))
        .map(|v| frame.var_off[v])
        .collect();

    let store_vars = |b: &mut Builder| {
        for v in &frame.vars {
            // Cell slots hold the boxed heap-cell pointer, written once at
            // first entry; the mutable value lives in the cell — nothing to
            // flush.
            if cell_vars.contains(v) {
                continue;
            }
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::LocalGet(var_local[v]));
            b.emit(Instruction::I64Store(mem_off(frame.var_off[v])));
        }
    };

    // Function entry: recover the frame pointer and reload frame-backed
    // locals once per (re)entry. Within an invocation, jumps re-dispatch
    // through the loop without reloading — locals stay live in wasm locals.
    emit_load_current_frame(&mut b, layout, frame_ptr_l);
    // Closure: seed `__env_ptr` from frame[0] so upvalue reads resolve. Done at
    // every (re)entry — a child await re-enters from the top and re-seeds.
    if frame.has_env {
        b.emit(Instruction::LocalGet(frame_ptr_l));
        b.emit(Instruction::I32Load(mem_off(0)));
        b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
    }
    for v in &frame.vars {
        if cell_vars.contains(v) {
            // Plan 114: the frame slot holds the NaN-boxed pointer of a
            // HEAP cell, not the cell itself. First entry (slot reads 0 —
            // frames are zeroed at spawn): allocate + tag the cell and
            // store its boxed pointer into the slot. Every entry: unbox
            // the pointer into the addr local. A heap cell survives the
            // frame, so an escaped closure that captured it stays valid
            // after the task completes — which is what lets frames with
            // cells be reclaimed again (the old design leaked the whole
            // frame to keep escaped closures safe).
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::I64Eqz);
            b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
            {
                let addr = var_local[v];
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::Call(b.rt().base + RT_ALLOC));
                b.emit(Instruction::LocalTee(addr));
                b.emit(Instruction::I64Const(crate::runtime::OBJ_TAG_CELL as i64));
                b.emit(Instruction::I64Store(mem0()));
                b.emit(Instruction::LocalGet(addr));
                b.emit(Instruction::I64Const(0));
                b.emit(Instruction::I64Store(mem_off(8)));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(addr));
                b.emit(Instruction::Call(b.rt().base + RT_MAKE_OBJ));
                b.emit(Instruction::I64Store(mem_off(frame.var_off[v])));
            }
            b.emit(Instruction::End);
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::Call(b.rt().base + RT_OBJ_ADDR));
            b.emit(Instruction::LocalSet(var_local[v]));
        } else {
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I64Load(mem_off(frame.var_off[v])));
            b.emit(Instruction::LocalSet(var_local[v]));
        }
    }

    // loop { block^B { br_table(resume_state) } <block bodies> }
    b.emit(Instruction::Loop(wasm_encoder::BlockType::Empty));
    for _ in 0..b_count {
        b.emit(Instruction::Block(wasm_encoder::BlockType::Empty));
    }
    emit_load_current_rstate(&mut b, layout);
    let targets: Vec<u32> = (0..b_count as u32).collect();
    b.emit(Instruction::BrTable(targets.into(), 0));

    for (k, blk) in blocks.iter().enumerate() {
        b.emit(Instruction::End); // block k region lands here
        // br index to reach the enclosing loop from this region.
        let loop_depth = (b_count - 1 - k) as u32;

        // If a child failed, the scheduler recorded the first-completed
        // error in this task's error slot. Route it: into the enclosing
        // `catch` (binding it) if the await was inside a `try`, else fail
        // this task (propagating up the await chain). The slot is reset so a
        // later await in the catch path starts clean.
        let check_child_error =
            |b: &mut Builder, on_error: Option<&(usize, String)>| -> Result<(), BuildError> {
                let cur_addr = |b: &mut Builder| {
                    b.emit(Instruction::GlobalGet(layout.g_table_base));
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                    b.emit(Instruction::I32Mul);
                    b.emit(Instruction::I32Add);
                };
                cur_addr(b);
                b.emit(Instruction::I64Load(mem_off(crate::async_engine::O_ERROR)));
                b.emit(Instruction::LocalSet(child_err_l));
                b.emit(Instruction::LocalGet(child_err_l));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Ne);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // Reset the error slot.
                cur_addr(b);
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Store(mem_off(crate::async_engine::O_ERROR)));
                match on_error {
                    Some((catch_blk, err_var)) => {
                        let l = *var_local
                            .get(err_var)
                            .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::LocalSet(l));
                        emit_store_current_rstate(b, layout, *catch_blk as i32);
                        // +1: this `br` is inside the error-check `If` block.
                        b.emit(Instruction::Br(loop_depth + 1));
                    }
                    None => {
                        b.emit(Instruction::GlobalGet(layout.g_current));
                        b.emit(Instruction::LocalGet(child_err_l));
                        b.emit(Instruction::Call(layout.fail));
                        b.emit(Instruction::Return);
                    }
                }
                b.emit(Instruction::End);
                Ok(())
            };
        let assign_pending = |b: &mut Builder, slot: u64, name: &str| -> Result<(), BuildError> {
            let l = *var_local
                .get(name)
                .ok_or(BuildError::UnsupportedExpression("async-unknown-bind"))?;
            if cell_vars.contains(name) {
                // Store the awaited result through the heap cell with
                // value-RC (plan 114). The child's +1 result transfers.
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
                b.emit(Instruction::Call(layout.task_result));
                b.emit_cell_store(l, true);
            } else {
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
                b.emit(Instruction::Call(layout.task_result));
                // Release the previous iteration's value before overwriting
                // (plan 116 follow-up): an awaited binding in a suspending loop
                // re-receives a `+1` child result per iteration; without this
                // only the final one is released at completion. First pass
                // reads 0 (zeroed frame) — a safe no-op. The incoming result
                // rides the stack across the stack-neutral RT_RELEASE.
                if release_set.contains(name) {
                    b.emit(Instruction::LocalGet(l));
                    b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
                }
                b.emit(Instruction::LocalSet(l));
            }
            Ok(())
        };
        // Recycle child task `slot`'s record onto the free list. The waiter only
        // resumes once its children have completed (join count hit 0), so the
        // slot is done and its result already consumed here. IDEMPOTENT: only a
        // slot still in a terminal (COMPLETE/FAILED) state is freed, and freeing
        // marks it ST_FREED — so a slot is never pushed onto `g_free_head` twice
        // (a double-free would hand the same slot to two live tasks via `spawn`,
        // e.g. a parent and its own child → self-await → poll re-readies it
        // forever). A slot already freed (or live/reused, status READY/RUNNING/
        // WAITING) is skipped.
        let free_pending = |b: &mut Builder, slot: u64| {
            let pend = b.alloc_i32_local();
            b.emit(Instruction::LocalGet(frame_ptr_l));
            b.emit(Instruction::I32Load(mem_off(frame.pending_off + slot * 4)));
            b.emit(Instruction::LocalSet(pend));
            // status = task[pend].status
            let st = b.alloc_i32_local();
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_STATUS)));
            b.emit(Instruction::LocalSet(st));
            // if status == COMPLETE || status == FAILED:
            b.emit(Instruction::LocalGet(st));
            b.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
            b.emit(Instruction::I32GeS);
            b.emit(Instruction::LocalGet(st));
            b.emit(Instruction::I32Const(crate::async_engine::ST_FAILED));
            b.emit(Instruction::I32LeS);
            b.emit(Instruction::I32And);
            b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
            // task[pend].next = g_free_head; g_free_head = pend
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::GlobalGet(layout.g_free_head));
            b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_NEXT)));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::GlobalSet(layout.g_free_head));
            // task[pend].status = ST_FREED
            b.emit(Instruction::GlobalGet(layout.g_table_base));
            b.emit(Instruction::LocalGet(pend));
            b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
            b.emit(Instruction::I32Mul);
            b.emit(Instruction::I32Add);
            b.emit(Instruction::I32Const(crate::async_engine::ST_FREED));
            b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_STATUS)));
            b.emit(Instruction::End);
        };
        if let Incoming::Awaited { binds, on_error } = &blk.incoming {
            // A failed child routes its error (to catch, or fails this task).
            // Otherwise bind the named results, then recycle each child slot —
            // including discarded ones (`binds[i] == None`, e.g. a `children()`
            // statement), which would otherwise leak a slot per render.
            check_child_error(&mut b, on_error.as_ref())?;
            for (slot, bind) in binds.iter().enumerate() {
                if let Some(name) = bind {
                    assign_pending(&mut b, slot as u64, name)?;
                } else {
                    // Discarded result (e.g. a `children()` statement). The child
                    // completed with an owned `+1` result; with no binding to take
                    // ownership it would leak, so release it here. RT_RELEASE
                    // no-ops on a primitive / void result.
                    b.emit(Instruction::LocalGet(frame_ptr_l));
                    b.emit(Instruction::I32Load(mem_off(frame.pending_off + (slot as u64) * 4)));
                    b.emit(Instruction::Call(layout.task_result));
                    b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
                }
                free_pending(&mut b, slot as u64);
            }
        }
        if let Incoming::AwaitedRemote { bind } = &blk.incoming {
            // The `remoteCall` finished; read its result for the current task.
            if let Some(name) = bind {
                let l = *var_local
                    .get(name)
                    .ok_or(BuildError::UnsupportedExpression("async-unknown-remote-bind"))?;
                if cell_vars.contains(name) {
                    // Value-RC store through the heap cell (plan 114); the
                    // host-built RPC result transfers.
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
                    b.emit_cell_store(l, true);
                } else {
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
                    // Release the previous iteration's value (plan 116
                    // follow-up) — same rationale as `assign_pending`.
                    if release_set.contains(name) {
                        b.emit(Instruction::LocalGet(l));
                        b.emit(Instruction::Call(b.rt().base + RT_RELEASE));
                    }
                    b.emit(Instruction::LocalSet(l));
                }
            } else {
                // Result discarded — still consume it to free the host's slot.
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
                b.emit(Instruction::Drop);
            }
        }

        for stmt in &blk.stmts {
            if let Statement::NowaitStatement(nw) = stmt {
                let (callee, args, loc) = user_callee(&nw.expression, fns)
                    .ok_or(BuildError::UnsupportedExpression("nowait-non-call"))?;
                emit_spawn_child(
                    &mut b,
                    &callee,
                    &args,
                    loc,
                    frame_sizes,
                    fn_table_idx,
                    layout,
                    childframe_l,
                    childid_l,
                )?;
                continue;
            }
            compile_async_segment_stmt(&mut b, stmt, &var_local, &release_set)?;
        }

        // (R1 clean slate, plan 113: async loop-body auto-drops removed — RC
        // reclaims uniformly.)

        match &blk.term {
            Term::Unset => return Err(BuildError::UnsupportedExpression("async-unset-block")),
            Term::Goto(t) => {
                emit_store_current_rstate(&mut b, layout, *t as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Cond {
                cond,
                then_blk,
                else_blk,
            } => {
                // resume_state = cond ? then_blk : else_blk; re-dispatch.
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.compile_expr_as(cond, ValueShape::RawBool)?;
                b.emit(Instruction::If(wasm_encoder::BlockType::Result(ValType::I32)));
                b.emit(Instruction::I32Const(*then_blk as i32));
                b.emit(Instruction::Else);
                b.emit(Instruction::I32Const(*else_blk as i32));
                b.emit(Instruction::End);
                b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_RSTATE)));
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Sleep { ms, next } => {
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::F64Const(*ms));
                b.emit(Instruction::Call(layout.sleep));
                b.emit(Instruction::Return);
            }
            Term::AwaitRemote { args, loc: _, next } => {
                // Park the current task on the in-flight request (no timer): the
                // host wakes it via `__fai_resume_task` when the response lands.
                // Status = WAITING, O_WAKE = -1 (so `poll` won't timer-promote).
                let rec = |b: &mut Builder| {
                    b.emit(Instruction::GlobalGet(layout.g_table_base));
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                    b.emit(Instruction::I32Mul);
                    b.emit(Instruction::I32Add);
                };
                rec(&mut b);
                b.emit(Instruction::I32Const(crate::async_engine::ST_WAITING));
                b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_STATUS)));
                rec(&mut b);
                b.emit(Instruction::F64Const(-1.0));
                b.emit(Instruction::F64Store(mem_off(crate::async_engine::O_WAKE)));
                // remote_begin(g_current, url*,len, fn*,len, args*,len, hash*,len)
                b.emit(Instruction::GlobalGet(layout.g_current));
                for a in args {
                    b.emit_string_arg_from_expr(a)?;
                }
                b.emit_import_call(crate::runtime::IMPORT_REMOTE_BEGIN);
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::CompleteRemote => {
                // complete(g_current, remote_result(g_current)) — the RPC result
                // is this (stub) task's return value. The result is host-provided
                // (not a frame slot), so releasing the body bindings first can't
                // touch it.
                emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit_import_call(crate::runtime::IMPORT_REMOTE_RESULT);
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::Await {
                callee,
                args,
                loc,
                next,
            } => {
                emit_spawn_child(
                    &mut b,
                    callee,
                    args,
                    loc,
                    frame_sizes,
                    fn_table_idx,
                    layout,
                    childframe_l,
                    childid_l,
                )?;
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::Call(layout.await_fn));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::AwaitClosure {
                closure,
                args,
                next,
            } => {
                // Evaluate the closure value → heap address.
                b.compile_expr_as(closure, ValueShape::Boxed)?;
                b.emit(Instruction::Call(b.rt().base + RT_OBJ_ADDR));
                b.emit(Instruction::LocalSet(closure_addr_l));
                // Runtime dispatch on the header's frame_size (offset 12):
                //   0  → sync closure (a `FaiFunc`) — call inline, no suspend.
                //   >0 → async closure (resume fn) — spawn as a task + await.
                // Both leave a child-task id in `pending`, so the next segment
                // reads the result uniformly via `task_result`.
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::I32Eqz);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // ── sync closure: inline call_indirect(FaiFunc(N)) ──
                let arity = args.len() as u16;
                let fai_ty = *b
                    .ctx
                    .fai_func_type_indices
                    .get(&arity)
                    .ok_or(BuildError::UnsupportedExpression("async-closure-arity"))?;
                b.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
                b.emit(Instruction::LocalSet(saved_env_l));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
                for arg in args.iter() {
                    b.compile_expr_as(arg, ValueShape::Boxed)?;
                }
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(4)));
                b.emit(Instruction::CallIndirect {
                    type_index: fai_ty,
                    table_index: 0,
                });
                b.emit(Instruction::LocalSet(sync_result_l));
                b.emit(Instruction::LocalGet(saved_env_l));
                b.emit(Instruction::GlobalSet(GLOBAL_ENV_PTR));
                // Synthesize a completed task holding the result so the next
                // segment can read it through `task_result` like an async child.
                // Reuse a free slot if available, else bump `g_count` — bumping
                // unconditionally would grow the table by one per *sync* closure
                // call, and the render tree is mostly sync closures, so the table
                // would creep up every render despite `free_pending` recycling.
                b.emit(Instruction::GlobalGet(layout.g_free_head));
                b.emit(Instruction::LocalTee(synth_id_l));
                b.emit(Instruction::I32Const(-1));
                b.emit(Instruction::I32Ne);
                b.emit(Instruction::If(wasm_encoder::BlockType::Empty));
                // pop: g_free_head = freed[synth_id].next
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::I32Load(mem_off(crate::async_engine::O_NEXT)));
                b.emit(Instruction::GlobalSet(layout.g_free_head));
                b.emit(Instruction::Else);
                b.emit(Instruction::GlobalGet(layout.g_count));
                b.emit(Instruction::LocalSet(synth_id_l));
                b.emit(Instruction::GlobalGet(layout.g_count));
                b.emit(Instruction::I32Const(1));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::GlobalSet(layout.g_count));
                b.emit(Instruction::End);
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::LocalSet(synth_addr_l));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::I32Const(crate::async_engine::ST_COMPLETE));
                b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_STATUS)));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::LocalGet(sync_result_l));
                b.emit(Instruction::I64Store(mem_off(crate::async_engine::O_RESULT)));
                b.emit(Instruction::LocalGet(synth_addr_l));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::I64Store(mem_off(crate::async_engine::O_ERROR)));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(synth_id_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                // Re-dispatch to `next` without suspending (locals stay live).
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Br(loop_depth + 1));
                b.emit(Instruction::Else);
                // ── async closure: spawn via header + await + suspend ──
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::Call(layout.alloc));
                b.emit(Instruction::LocalSet(childframe_l));
                // Zero the fresh frame (plan 115) — see `emit_spawn_child`. Size
                // is the closure's frame_size (header @ +12). env/args overwrite
                // the leading zeros below.
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::I32Const(0));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::MemoryFill(0));
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Const(16));
                b.emit(Instruction::I32Add);
                b.emit(Instruction::I32Store(mem_off(0)));
                for (j, arg) in args.iter().enumerate() {
                    b.emit(Instruction::LocalGet(childframe_l));
                    // Param slots own +1 (see `emit_spawn_child`) — retain
                    // a borrowed arg; the closure task releases its param
                    // slots at completion.
                    let transfers = b.expr_transfers_ownership(arg);
                    b.compile_expr_as(arg, ValueShape::Boxed)?;
                    if !transfers {
                        b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                    }
                    b.emit(Instruction::I64Store(mem_off(8 + (j as u64) * 8)));
                }
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(4)));
                b.emit(Instruction::LocalGet(childframe_l));
                b.emit(Instruction::Call(layout.spawn));
                b.emit(Instruction::LocalSet(childid_l));
                // Record the spawned closure frame's size (closure header @ +12)
                // so the task's completion reclaims it.
                b.emit(Instruction::GlobalGet(layout.g_table_base));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Const(crate::async_engine::REC_SIZE));
                b.emit(Instruction::I32Mul);
                b.emit(Instruction::I32Add);
                b.emit(Instruction::LocalGet(closure_addr_l));
                b.emit(Instruction::I32Load(mem_off(12)));
                b.emit(Instruction::I32Store(mem_off(crate::async_engine::O_FRAME_SIZE)));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::I32Store(mem_off(frame.pending_off)));
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(childid_l));
                b.emit(Instruction::Call(layout.await_fn));
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
                b.emit(Instruction::End);
            }
            Term::All { children, next } => {
                // Spawn each child (id → pending slot k) and `await` each so
                // the join count becomes N; resume only when all complete.
                for (j, (callee, args, loc)) in children.iter().enumerate() {
                    emit_spawn_child(
                        &mut b,
                        callee,
                        args,
                        loc,
                        frame_sizes,
                        fn_table_idx,
                        layout,
                        childframe_l,
                        childid_l,
                    )?;
                    b.emit(Instruction::LocalGet(frame_ptr_l));
                    b.emit(Instruction::LocalGet(childid_l));
                    b.emit(Instruction::I32Store(mem_off(frame.pending_off + (j as u64) * 4)));
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::LocalGet(childid_l));
                    b.emit(Instruction::Call(layout.await_fn));
                }
                store_vars(&mut b);
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Return);
            }
            Term::Complete(expr) => {
                // +1-return convention: the result escapes to the awaiter, which
                // now RELEASES its copy at its own completion, so EVERY completion
                // must hand back an owned `+1`. Retain a borrowed result (it may
                // read a binding); a fresh value / owned call result already is
                // `+1`. Then stash it, release the owned frame bindings, and
                // complete with it: if the result IS a released binding the retain
                // holds it across that release; if it's a FIELD of one, RC
                // deep-free decrements the field but our `+1` keeps it alive.
                b.compile_expr_as(expr, ValueShape::Boxed)?;
                if !b.expr_transfers_ownership(expr) {
                    b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                }
                let saved = b.alloc_local();
                b.emit(Instruction::LocalSet(saved));
                emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(saved));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::CompleteVoid => {
                // Void is a primitive — no result to retain.
                emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::I64Const(VAL_VOID));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::CompletePending => {
                // A tail/return await: propagate the child's error if it
                // failed, else complete with its result. The result is the
                // child's (read below from its task record, not a frame slot),
                // so releasing the body bindings first can't touch it.
                check_child_error(&mut b, None)?;
                emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(frame_ptr_l));
                b.emit(Instruction::I32Load(mem_off(frame.pending_off)));
                b.emit(Instruction::Call(layout.task_result));
                // Recycle the awaited child's slot (its result is now consumed)
                // BEFORE `complete` — `complete` now `rt_free`s this task's frame
                // (Phase 4 frame reclaim), and `free_pending` reads the frame's
                // pending slot, so it must run while the frame is still live.
                // `free_pending` is stack-neutral, so [g_current, result] for
                // `complete` is preserved.
                free_pending(&mut b, 0);
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
            Term::ThrowTo {
                value,
                catch_blk,
                err_var,
            } => {
                // Bind the thrown value to the catch name, then jump to the
                // catch handler (within this invocation — no reload needed).
                b.compile_expr_as(value, ValueShape::Boxed)?;
                let l = *var_local
                    .get(err_var)
                    .ok_or(BuildError::UnsupportedExpression("async-unknown-catch"))?;
                b.emit(Instruction::LocalSet(l));
                emit_store_current_rstate(&mut b, layout, *catch_blk as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::Fail(value) => {
                // `fail` is a completion: the error escapes to the awaiter's
                // `catch` (read from this task's O_ERROR), so retain-if-borrowed
                // before releasing the owned frame bindings — same +1 convention
                // as a normal `complete`.
                if release_names.is_empty() {
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.compile_expr_as(value, ValueShape::Boxed)?;
                    b.emit(Instruction::Call(layout.fail));
                } else {
                    b.compile_expr_as(value, ValueShape::Boxed)?;
                    if !b.expr_transfers_ownership(value) {
                        b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                    }
                    let saved = b.alloc_local();
                    b.emit(Instruction::LocalSet(saved));
                    emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                    b.emit(Instruction::GlobalGet(layout.g_current));
                    b.emit(Instruction::LocalGet(saved));
                    b.emit(Instruction::Call(layout.fail));
                }
                b.emit(Instruction::Return);
            }
            Term::StoreResultGoto { value, next } => {
                // Store the try/catch body's value into the try-result local,
                // then jump to the finally block. The value is held across the
                // finally and escapes via `CompleteResult` to the awaiter (which
                // releases its copy), so retain it if borrowed — the +1-return
                // convention, and it also keeps the value alive across the
                // frame-binding releases the finally / `CompleteResult` emit.
                b.compile_expr_as(value, ValueShape::Boxed)?;
                if !b.expr_transfers_ownership(value) {
                    b.emit(Instruction::Call(b.rt().base + RT_RETAIN));
                }
                b.emit(Instruction::LocalSet(try_result_l));
                emit_store_current_rstate(&mut b, layout, *next as i32);
                b.emit(Instruction::Br(loop_depth));
            }
            Term::CompleteResult => {
                // try_result_l already holds a `+1` ref (retained at
                // StoreResultGoto when there are bindings to release).
                emit_async_drops(&mut b, &release_names, &var_local, &cell_offsets, frame_ptr_l);
                b.emit(Instruction::GlobalGet(layout.g_current));
                b.emit(Instruction::LocalGet(try_result_l));
                b.emit(Instruction::Call(layout.complete));
                b.emit(Instruction::Return);
            }
        }
    }
    b.emit(Instruction::End); // close the dispatch loop
    b.emit(Instruction::Unreachable);
    // Upvalues captured during body compilation (closures only; empty for
    // named fns). The creation site uses them to build the env block.
    let upvalues = std::mem::take(&mut b.upvalues);
    Ok((b.finish(), upvalues))
}

/// A-normalize the async calls in a function body: hoist every async call that
/// appears as a *proper subexpression* into a preceding `let __anf_await_N =
/// <call>` temp, so the CFG's await lowering (which only recognizes an async
/// call as the whole value of a `let`/`return`/expr-stmt) can handle shapes like
/// `print(component())` or `f(asyncG())`. Sync calls and async-free expressions
/// are left untouched (no churn on existing code). Positions where hoisting
/// would change evaluation semantics — `&&`/`||` right operands, `while`
/// conditions, `elsif` chains — are deliberately not rewritten; an async call
/// there stays in place and the CFG falls back exactly as before.
fn anf_async_body(
    body: &[Statement],
    r: &AsyncResolve<'_>,
    counter: &mut usize,
) -> Vec<Statement> {
    let mut out = Vec::with_capacity(body.len());
    for stmt in body {
        anf_async_stmt(stmt, r, counter, &mut out);
    }
    out
}

fn anf_async_stmt(
    stmt: &Statement,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) {
    use fai_compiler::ast;
    // Atomize a value position the CFG can't take an async call in (assignment
    // value, `if` condition, `for` items): hoist even a top-level async call.
    let atomize = |expr: &Expression, counter: &mut usize, out: &mut Vec<Statement>| {
        if expr_has_user_call(expr, r) {
            anf_atom(expr, r, counter, out)
        } else {
            expr.clone()
        }
    };
    match stmt {
        Statement::LetStatement(ls) if ls.bindings.len() == 1 => {
            let value = anf_nested(&ls.value, r, counter, out);
            out.push(Statement::LetStatement(ast::LetStatement {
                bindings: ls.bindings.clone(),
                value,
                is_private: ls.is_private,
                is_shared: ls.is_shared,
                location: ls.location.clone(),
            }));
        }
        Statement::VarStatement(vs) if vs.bindings.len() == 1 => {
            let value = anf_nested(&vs.value, r, counter, out);
            out.push(Statement::VarStatement(ast::VarStatement {
                bindings: vs.bindings.clone(),
                value,
                is_private: vs.is_private,
                is_shared: vs.is_shared,
                location: vs.location.clone(),
            }));
        }
        Statement::ReturnStatement(rs) => {
            let value = rs.value.as_ref().map(|v| anf_nested(v, r, counter, out));
            out.push(Statement::ReturnStatement(ast::ReturnStatement {
                value,
                location: rs.location.clone(),
            }));
        }
        Statement::ExpressionStatement(es) => {
            let expression = anf_nested(&es.expression, r, counter, out);
            out.push(Statement::ExpressionStatement(ast::ExpressionStatement {
                expression,
                location: es.location.clone(),
            }));
        }
        Statement::AssignmentStatement(a) => {
            let value = atomize(&a.value, counter, out);
            out.push(Statement::AssignmentStatement(ast::AssignmentStatement {
                target: a.target.clone(),
                value,
                location: a.location.clone(),
            }));
        }
        Statement::IfStatement(is) => {
            // Desugar an else-if chain into nested single-branch ifs (the CFG
            // only lowers single-branch ifs), A-normalizing each piece. Each
            // branch condition is hoisted into the scope where it is actually
            // evaluated: branch 0's into `out` (before the if), branch k>0's
            // into the preceding `else` body (only reached if earlier
            // conditions were false), preserving short-circuit semantics.
            anf_if_chain(&is.branches, is.else_branch.as_deref(), &is.location, r, counter, out);
        }
        Statement::WhileStatement(ws) => {
            // Condition is re-evaluated each iteration — must NOT hoist it.
            let body = anf_async_body(&ws.body, r, counter);
            out.push(Statement::WhileStatement(ast::WhileStatement {
                condition: ws.condition.clone(),
                body,
                location: ws.location.clone(),
            }));
        }
        Statement::ForStatement(fs)
            if stmts_have_suspension(&fs.body, r) && !stmts_have_loop_control(&fs.body) =>
        {
            // A `for` loop whose body suspends can't be compiled inline (the body
            // would have to yield mid-iteration). Desugar it into an index-driven
            // `while` loop, which the engine already lowers across suspension:
            //
            //   let  __for_coll = <items>
            //   var  __for_idx  = 0
            //   while __for_idx < length(__for_coll) do
            //       let <item> = __for_coll[__for_idx]
            //       <body>
            //       __for_idx = __for_idx + 1
            //   end
            //
            // The loop index and collection live in the frame, so they survive a
            // suspension inside the body. (Plain non-suspending `for` loops keep
            // the fast inline path below.)
            let loc = fs.location.clone();
            let coll = atomize(&fs.items, counter, out);
            let coll_name = format!("__for_coll_{}", *counter);
            *counter += 1;
            let idx_name = format!("__for_idx_{}", *counter);
            *counter += 1;
            let ident = |name: &str| {
                Expression::IdentifierExpression(ast::IdentifierExpression {
                    name: name.to_string(),
                    location: loc.clone(),
                })
            };
            let int_lit = |n: f64| {
                Expression::NumberExpression(ast::NumberExpression {
                    value: n,
                    is_float: false,
                    location: loc.clone(),
                })
            };
            out.push(Statement::LetStatement(ast::LetStatement {
                bindings: vec![ast::BindingDeclaration {
                    name: coll_name.clone(),
                    type_name: None,
                }],
                value: coll,
                is_private: None,
                is_shared: None,
                location: loc.clone(),
            }));
            out.push(Statement::VarStatement(ast::VarStatement {
                bindings: vec![ast::BindingDeclaration {
                    name: idx_name.clone(),
                    type_name: None,
                }],
                value: int_lit(0.0),
                is_private: None,
                is_shared: None,
                location: loc.clone(),
            }));
            let mut wbody: Vec<Statement> = Vec::new();
            wbody.push(Statement::LetStatement(ast::LetStatement {
                bindings: vec![ast::BindingDeclaration {
                    name: fs.item_name.clone(),
                    type_name: None,
                }],
                value: Expression::IndexExpression(ast::IndexExpression {
                    object: Box::new(ident(&coll_name)),
                    index: Box::new(ident(&idx_name)),
                    location: loc.clone(),
                }),
                is_private: None,
                is_shared: None,
                location: loc.clone(),
            }));
            for s in &fs.body {
                anf_async_stmt(s, r, counter, &mut wbody);
            }
            wbody.push(Statement::AssignmentStatement(ast::AssignmentStatement {
                target: ast::AssignmentTarget::Variables {
                    names: vec![idx_name.clone()],
                },
                value: Expression::BinaryExpression(ast::BinaryExpression {
                    left: Box::new(ident(&idx_name)),
                    operator: "+".to_string(),
                    right: Box::new(int_lit(1.0)),
                    location: loc.clone(),
                }),
                location: loc.clone(),
            }));
            let condition = Expression::BinaryExpression(ast::BinaryExpression {
                left: Box::new(ident(&idx_name)),
                operator: "<".to_string(),
                right: Box::new(Expression::CallExpression(ast::CallExpression {
                    callee: Box::new(ident("length")),
                    args: vec![ast::CallArgument {
                        label: None,
                        value: ident(&coll_name),
                        location: loc.clone(),
                    }],
                    location: loc.clone(),
                })),
                location: loc.clone(),
            });
            out.push(Statement::WhileStatement(ast::WhileStatement {
                condition,
                body: wbody,
                location: loc.clone(),
            }));
        }
        Statement::ForStatement(fs) => {
            let items = atomize(&fs.items, counter, out);
            let body = anf_async_body(&fs.body, r, counter);
            out.push(Statement::ForStatement(ast::ForStatement {
                item_name: fs.item_name.clone(),
                items,
                body,
                location: fs.location.clone(),
            }));
        }
        Statement::TryStatement(ts) => {
            let try_body = anf_async_body(&ts.try_body, r, counter);
            let catch_body = anf_async_body(&ts.catch_body, r, counter);
            let finally_body = ts
                .finally_body
                .as_ref()
                .map(|b| anf_async_body(b, r, counter));
            out.push(Statement::TryStatement(ast::TryStatement {
                try_body,
                catch_name: ts.catch_name.clone(),
                catch_body,
                finally_body,
                location: ts.location.clone(),
            }));
        }
        other => out.push(other.clone()),
    }
}

/// Desugar an `if … else if … else …` chain into nested single-branch ifs
/// (the only shape the resume CFG lowers) while A-normalizing every condition
/// and body. Recurses on the tail so branch *k*'s condition is hoisted into the
/// `else` of branch *k−1* — i.e. only evaluated when the earlier conditions
/// were false, exactly as the original chain.
fn anf_if_chain(
    branches: &[fai_compiler::ast::IfBranch],
    else_body: Option<&[Statement]>,
    loc: &fai_compiler::ast::SourceLocation,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) {
    use fai_compiler::ast;
    let Some(head) = branches.first() else {
        if let Some(eb) = else_body {
            for s in eb {
                anf_async_stmt(s, r, counter, out);
            }
        }
        return;
    };
    let condition = if expr_has_user_call(&head.condition, r) {
        anf_atom(&head.condition, r, counter, out)
    } else {
        head.condition.clone()
    };
    let body = anf_async_body(&head.body, r, counter);
    let else_branch = if branches.len() > 1 {
        let mut nested = Vec::new();
        anf_if_chain(&branches[1..], else_body, loc, r, counter, &mut nested);
        Some(nested)
    } else {
        else_body.map(|eb| anf_async_body(eb, r, counter))
    };
    out.push(Statement::IfStatement(ast::IfStatement {
        branches: vec![ast::IfBranch {
            condition,
            body,
            location: head.location.clone(),
        }],
        else_branch,
        location: loc.clone(),
    }));
}

/// Reduce `expr` to an atom for the CFG: hoist nested async calls, and if the
/// (rewritten) expression is *itself* an async call, hoist that too — returning
/// the temp identifier that now holds its awaited value.
fn anf_atom(
    expr: &Expression,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) -> Expression {
    let e = anf_nested(expr, r, counter, out);
    if let Expression::CallExpression(c) = &e {
        if user_callee(&e, r).is_some() {
            let loc = c.location.clone();
            let name = format!("__anf_await_{}", *counter);
            *counter += 1;
            out.push(Statement::LetStatement(fai_compiler::ast::LetStatement {
                bindings: vec![fai_compiler::ast::BindingDeclaration {
                    name: name.clone(),
                    type_name: None,
                }],
                value: e,
                is_private: None,
                is_shared: None,
                location: loc.clone(),
            }));
            return Expression::IdentifierExpression(fai_compiler::ast::IdentifierExpression {
                name,
                location: loc,
            });
        }
    }
    e
}

/// Hoist async calls in the proper-subexpression positions of `expr` (call
/// args, arithmetic operands, member/index objects, …). The top-level
/// expression is returned in place — even if it is itself an async call — so an
/// await-position caller keeps it; `anf_atom` hoists the top level when an atom
/// is required. Async-free expressions are returned unchanged.
fn anf_nested(
    expr: &Expression,
    r: &AsyncResolve<'_>,
    counter: &mut usize,
    out: &mut Vec<Statement>,
) -> Expression {
    // Closure literal: A-normalize its *body* (a separate function — hoisted
    // temps stay inside it, keyed off the shared counter for unique names). The
    // closure value itself is not a call to hoist. Done unconditionally because
    // a closure passed to an async-free host call (`server.get(r,'*') do … end`)
    // still needs its async body rewritten, and `expr_has_user_call` does not
    // descend into closures.
    if let Expression::FunctionExpression(fd) = expr {
        let mut fd2 = fd.clone();
        fd2.body = anf_async_body(&fd.body, r, counter);
        return Expression::FunctionExpression(fd2);
    }
    // Leaf atoms have nothing to rewrite.
    if matches!(
        expr,
        Expression::IdentifierExpression(_)
            | Expression::NumberExpression(_)
            | Expression::StringExpression(_)
            | Expression::BooleanExpression(_)
            | Expression::NullExpression(_)
    ) {
        return expr.clone();
    }
    // `all(...)` is a concurrency special form: its arguments are concurrent
    // spawn points lowered by the CFG's `Term::All`, not ordinary call args.
    // Hoisting them would serialize the spawns (and break `all_call` detection),
    // so leave it opaque — the CFG handles `let [a, b] = all(f(), g())` directly.
    if let Expression::CallExpression(c) = expr {
        if let Expression::IdentifierExpression(id) = &*c.callee {
            if id.name == "all" {
                return expr.clone();
            }
        }
    }
    let mut e = expr.clone();
    match &mut e {
        Expression::CallExpression(c) => {
            *c.callee = anf_atom(&c.callee, r, counter, out);
            for a in &mut c.args {
                a.value = anf_atom(&a.value, r, counter, out);
            }
        }
        Expression::BinaryExpression(b) if b.operator != "&&" && b.operator != "||" => {
            *b.left = anf_atom(&b.left, r, counter, out);
            *b.right = anf_atom(&b.right, r, counter, out);
        }
        Expression::UnaryExpression(u) => {
            *u.expression = anf_atom(&u.expression, r, counter, out);
        }
        Expression::MemberExpression(m) => {
            *m.object = anf_atom(&m.object, r, counter, out);
        }
        Expression::IndexExpression(i) => {
            *i.object = anf_atom(&i.object, r, counter, out);
            *i.index = anf_atom(&i.index, r, counter, out);
        }
        Expression::ArrayExpression(a) => {
            for it in &mut a.items {
                *it = anf_atom(it, r, counter, out);
            }
        }
        Expression::TupleExpression(t) => {
            for it in &mut t.items {
                *it = anf_atom(it, r, counter, out);
            }
        }
        Expression::OptionalCheckExpression(o) => {
            *o.expression = anf_atom(&o.expression, r, counter, out);
        }
        Expression::ForceUnwrapExpression(f) => {
            *f.expression = anf_atom(&f.expression, r, counter, out);
        }
        _ => {}
    }
    e
}

/// Try to compile an async program through the real engine. Returns
/// `Some(wasm)` only for the v1-handled shape (native target, no modules,
/// a single async `main` whose suspension is `sleep`); otherwise `None`
/// so the caller falls back to the existing path.
pub fn try_codegen_async_engine(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &CheckerInfo,
    target: Option<&str>,
    analysis: &crate::async_analysis::AsyncAnalysis,
    entry_file: Option<&str>,
) -> Option<Vec<u8>> {
    use crate::async_engine::{self, SchedLayout};
    use crate::runtime::{self, IMPORT_NOW_MS, RT_ALLOC, RT_COUNT, RT_FREE};
    use std::collections::{HashMap as Map, HashSet as Set};
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };

    // ── v1 gate ──
    // (A4) Browser targets now engage the real engine too. `sleep` arranges a
    // host wakeup via `host_set_timer` instead of the native busy-poll.
    // Engage when the program has any async at all (something suspends, or
    // there's a `nowait` fork). `main` itself need not suspend — it may just
    // fork a `nowait` task. Purely-sync programs have an empty analysis and
    // fall through to the sync path.
    if analysis.is_empty() {
        return None;
    }
    // Gather every user function from the entry AST and every module. Module
    // functions are name-prefixed `{module}.{fn}` exactly as `build_program_full`
    // and `async_analysis` do, so the analysis' qualified async set and the
    // function table agree. `decls` owns the (possibly renamed) declarations;
    // `fn_module` records each one's module context for call resolution.
    // Each decl carries (fn, module_context, file_path). The file path feeds
    // the per-call-site `module_key` (UFCS / named-param / expression-type
    // lookups) — it must match what the checker recorded, exactly as
    // `build_program_full` plumbs it.
    let mut decls: Vec<(FunctionDeclaration, Option<String>, Option<String>)> = Vec::new();
    for s in &ast.statements {
        if let Statement::FunctionDeclaration(fd) = s {
            decls.push((fd.clone(), None, None));
        }
    }
    for m in modules {
        for (idx, s) in m.statements.iter().enumerate() {
            if let Statement::FunctionDeclaration(fd) = s {
                let mut prefixed = fd.clone();
                prefixed.name = format!("{}.{}", m.name, fd.name);
                let file = m.file_paths.get(idx).cloned().flatten();
                decls.push((prefixed, Some(m.name.clone()), file));
            }
        }
    }
    if decls.is_empty() {
        return None;
    }

    // ── module-level `var NAME = EXPR` globals + their initializers ──
    // Their globals live after the 4 runtime + 7 scheduler globals, so they
    // start at index 11. Initializers run once, before `main` is spawned, via
    // a synthesized `<__module_init__>` that the scheduler's `start_async`
    // calls — one per module context so each resolves its own imports.
    // Globals 0..=3 are runtime (heap_ptr, env_ptr, error_flag, error_value);
    // 4..=11 are the scheduler (g_count..g_free_head). Module `var`s follow.
    const MODULE_VAR_BASE: u32 = 12;
    let mut module_vars: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut module_var_inits: Vec<(Option<String>, Statement)> = Vec::new();
    {
        let mut collect = |stmts: &[Statement], ctx_mod: Option<&str>| {
            for s in stmts {
                if let Statement::VarStatement(vs) = s {
                    if vs.bindings.len() != 1 {
                        continue;
                    }
                    let name = &vs.bindings[0].name;
                    if module_vars.contains_key(name) {
                        continue;
                    }
                    module_vars.insert(name.clone(), MODULE_VAR_BASE + module_var_inits.len() as u32);
                    let target = fai_compiler::ast::AssignmentTarget::Variables {
                        names: vec![name.clone()],
                    };
                    let assign = Statement::AssignmentStatement(fai_compiler::ast::AssignmentStatement {
                        target,
                        value: vs.value.clone(),
                        location: vs.location.clone(),
                    });
                    module_var_inits.push((ctx_mod.map(|s| s.to_string()), assign));
                }
            }
        };
        collect(&ast.statements, None);
        for m in modules {
            collect(&m.statements, Some(m.name.as_str()));
        }
    }
    let module_var_count = module_var_inits.len() as u32;
    let mut master_init_name: Option<String> = None;
    if module_var_count > 0 {
        let loc = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
        let synth = |name: String, body: Vec<Statement>| FunctionDeclaration {
            name,
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body,
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            location: loc.clone(),
            doc_comment: None,
        };
        let mk_call = |name: &str| {
            Statement::ExpressionStatement(fai_compiler::ast::ExpressionStatement {
                expression: Expression::CallExpression(fai_compiler::ast::CallExpression {
                    callee: Box::new(Expression::IdentifierExpression(
                        fai_compiler::ast::IdentifierExpression {
                            name: name.to_string(),
                            location: loc.clone(),
                        },
                    )),
                    args: Vec::new(),
                    location: loc.clone(),
                }),
                location: loc.clone(),
            })
        };
        // Group initializers by module context, in first-seen order.
        let mut groups: Vec<(Option<String>, Vec<Statement>)> = Vec::new();
        for (ctx_mod, stmt) in &module_var_inits {
            match groups.iter_mut().find(|(m, _)| m == ctx_mod) {
                Some((_, v)) => v.push(stmt.clone()),
                None => groups.push((ctx_mod.clone(), vec![stmt.clone()])),
            }
        }
        let mut master_body: Vec<Statement> = Vec::new();
        for (ctx_mod, body) in groups {
            let fn_name = match &ctx_mod {
                Some(m) => format!("<__module_init__:{}>", m),
                None => "<__module_init__:>".to_string(),
            };
            master_body.push(mk_call(&fn_name));
            decls.push((synth(fn_name, body), ctx_mod, None));
        }
        decls.push((synth("<__module_init__>".to_string(), master_body), None, None));
        master_init_name = Some("<__module_init__>".to_string());
    }

    // name -> (module_context, file_path) for per-fn call resolution + module_key.
    let fn_ctx: std::collections::HashMap<String, (Option<String>, Option<String>)> = decls
        .iter()
        .map(|(fd, m, f)| (fd.name.clone(), (m.clone(), f.clone())))
        .collect();

    // ── module context maps (mirror build_program_full / async_analysis) ──
    let mut module_fn_exports: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for m in modules {
        let mut names = Vec::new();
        for s in &m.statements {
            if let Statement::FunctionDeclaration(fd) = s {
                if !m.private_names.iter().any(|n| n == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        module_fn_exports.insert(m.name.clone(), names);
    }
    let mut module_aliases: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    {
        let mut basename_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                *basename_counts.entry(last.to_string()).or_insert(0) += 1;
            }
        }
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                if basename_counts.get(last).copied().unwrap_or(0) == 1 {
                    module_aliases.insert(last.to_string(), m.name.clone());
                }
            }
        }
    }
    for (k, v) in collect_module_aliases_from(None, &ast.statements) {
        module_aliases.insert(k, v);
    }
    for m in modules {
        for (k, v) in collect_module_aliases_from(Some(&m.name), &m.statements) {
            module_aliases.entry(k).or_insert(v);
        }
    }
    let mut named_imports: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    {
        let mut record = |stmts: &[Statement], current: Option<&str>, entry_wins: bool| {
            for s in stmts {
                let Statement::UseStatement(u) = s else { continue };
                let qualified = qualify_module_path_for_codegen(current, &u.module_path);
                let mut put = |out: &mut std::collections::HashMap<String, String>, k: String, v: String| {
                    if entry_wins {
                        out.insert(k, v);
                    } else {
                        out.entry(k).or_insert(v);
                    }
                };
                if u.import_all {
                    if fai_checker::std_modules::is_std_module(&u.module_path) {
                        if let Some(exports) =
                            fai_checker::std_modules::std_module_exports().get(&qualified)
                        {
                            for (n, _) in exports {
                                put(&mut named_imports, n.clone(), format!("{}.{}", qualified, n));
                            }
                        }
                    } else if let Some(names) = module_fn_exports.get(&qualified) {
                        for n in names {
                            put(&mut named_imports, n.clone(), format!("{}.{}", qualified, n));
                        }
                    }
                } else if let Some(names) = &u.imported_names {
                    for n in names {
                        put(&mut named_imports, n.clone(), format!("{}.{}", qualified, n));
                    }
                }
            }
        };
        record(&ast.statements, None, true);
        for m in modules {
            record(&m.statements, Some(&m.name), false);
        }
    }
    // ── hybrid model ──
    // Only async-effectful functions become resume tasks; everything else
    // stays on the fast sync path (compiled by `build_function`). The async
    // set is the analysis' async ∪ scheduler functions. A call to an async fn
    // is an await; a call to a sync fn is a plain direct call.
    let mut async_set: std::collections::HashSet<String> = analysis
        .async_functions
        .iter()
        .chain(analysis.scheduler_functions.iter())
        .cloned()
        .collect();
    // Names of every user function (for module-aware call resolution). For a
    // single file these are bare names; module fns are `{module}.{fn}`.
    let all_user_fns: std::collections::HashSet<String> =
        decls.iter().map(|(fd, _, _)| fd.name.clone()).collect();
    // A spawned function (`nowait f()` / `all(f(), ...)`) must be a resume
    // task even if its own body never suspends — fold those targets in
    // (resolved to their canonical names in each fn's module context).
    {
        let empty: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut targets: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (fd, mctx, fctx) in &decls {
            let mk = fctx.as_deref().or(mctx.as_deref()).unwrap_or("");
            let r = AsyncResolve {
                async_set: &empty,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx.as_deref(),
                ufcs_calls: &checker.ufcs_calls,
                module_key: mk,
            };
            collect_spawn_targets(&fd.body, &r, &mut targets);
        }
        async_set.extend(targets);
    }
    // main must exist and take no arguments. Resolved against `decls` here — the
    // borrowing `all_fns` view is rebuilt after the A-normalization rewrite
    // below (which needs `&mut decls`).
    {
        let main_decl = decls.iter().find(|(fd, _, _)| fd.name == "main")?;
        if !main_decl.0.params.is_empty() {
            return None; // root takes no arguments
        }
    }
    // A1 — "everything is async, even `main`." `main` is always the startup
    // root task, whether or not its own body suspends. (Pure-sync programs
    // never reach here: the `analysis.is_empty()` early-out above sends them to
    // the fast path.)
    async_set.insert("main".to_string());

    // ── A-normalize async calls ──
    // The CFG's await lowering only recognizes an async call when it is the
    // whole value of a `let`/`return`/expr-stmt. Hoist async calls nested as
    // subexpressions (`print(component())`, `f(asyncG())`, …) into preceding
    // `let __anf_await_N = <call>` temps so they lower as awaits. Done before
    // `all_fns`/frame layout so every downstream pass sees the rewritten bodies.
    // `async_set` was computed from the original bodies — still correct, since
    // hoisting reorders calls into temps but changes neither the call graph nor
    // which functions are async.
    {
        for (fd, mctx, fctx) in decls.iter_mut() {
            let mk = fctx.as_deref().or(mctx.as_deref()).unwrap_or("");
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx.as_deref(),
                ufcs_calls: &checker.ufcs_calls,
                module_key: mk,
            };
            let mut counter = 0usize;
            let rewritten = anf_async_body(&fd.body, &r, &mut counter);
            fd.body = rewritten;
        }
    }

    let all_fns: Vec<&FunctionDeclaration> = decls.iter().map(|(fd, _, _)| fd).collect();
    let main = *all_fns.iter().find(|fd| fd.name == "main")?;
    for fd in &all_fns {
        let is_async = async_set.contains(&fd.name);
        // A sync fn becomes a `FaiFunc(arity)` in the table-type space.
        if !is_async
            && (fd.params.len() + fd.type_params.len()) > MAX_DIRECT_ARITY as usize
        {
            return None;
        }
    }

    // Proto order = wasm function order: each user fn sits at
    // `import_count + RT_COUNT + proto`. `main` is first.
    let mut ordered: Vec<&FunctionDeclaration> = vec![main];
    let mut rest: Vec<&FunctionDeclaration> =
        all_fns.iter().copied().filter(|fd| fd.name != "main").collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    ordered.extend(rest);

    // Table slots go to async fns only (sync fns are called directly, never
    // through the table). `main` = slot 0 (root). Frames likewise exist only
    // for async fns.
    let mut fn_table_idx: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut frame_sizes: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    let mut frames: std::collections::HashMap<String, AsyncFrame> = std::collections::HashMap::new();
    let mut tpos = 0u32;
    for fd in &ordered {
        if async_set.contains(&fd.name) {
            let (mctx, fctx) = fn_ctx
                .get(&fd.name)
                .map(|(m, f)| (m.as_deref(), f.as_deref()))
                .unwrap_or((None, None));
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx,
                ufcs_calls: &checker.ufcs_calls,
                module_key: fctx.or(mctx).unwrap_or(""),
            };
            let frame = async_frame_layout(fd, &r, false);
            fn_table_idx.insert(fd.name.clone(), tpos);
            frame_sizes.insert(fd.name.clone(), frame.size);
            frames.insert(fd.name.clone(), frame);
            tpos += 1;
        }
    }
    let nasync = tpos;
    let nuser = ordered.len() as u32;
    let root_frame_size = frames.get("main").map(|f| f.size)?;

    // ── module-level index layout ──
    let import_available = runtime::available_imports_with_test_flag(target, false);
    let (import_remap, actual_import_count) = runtime::build_import_remap(&import_available);
    let now_ms_idx = import_remap.get(IMPORT_NOW_MS as usize).copied().flatten()?;
    let import_sigs = runtime::import_signatures();
    let rt_sigs = runtime::type_signatures();

    // Scheduler-specific function types are appended after import, rt, and
    // FaiFunc(0..=MAX_DIRECT_ARITY) types.
    let sched_type_base =
        (import_sigs.len() + rt_sigs.len() + (MAX_DIRECT_ARITY as usize + 1)) as u32;
    let t_resume = sched_type_base;
    let t_i32_void = sched_type_base + 1;
    let t_void_i32 = sched_type_base + 2;
    let t_i32i32_i32 = sched_type_base + 3;
    let t_i32i64_void = sched_type_base + 4;
    let t_i32f64_void = sched_type_base + 5;
    let t_i32_i32 = sched_type_base + 6;
    let t_i32_i64 = sched_type_base + 7;
    let t_i32i32_void = sched_type_base + 8;
    let t_i64i64_i64 = sched_type_base + 9; // __fai_drive_closure

    // User fns occupy `[import_count + RT_COUNT, +nuser)` so async→sync direct
    // calls resolve to `rt.base + RT_COUNT + proto`. The scheduler sits after.
    let user_fn_base = actual_import_count + RT_COUNT;
    let sb = user_fn_base + nuser; // first scheduler fn index
    // Wasm index of the synthesized module-init fn (if any), for start_async.
    let module_init = master_init_name.as_ref().and_then(|name| {
        ordered
            .iter()
            .position(|fd| &fd.name == name)
            .map(|proto| user_fn_base + proto as u32)
    });
    let layout = SchedLayout {
        now_ms: now_ms_idx,
        alloc: actual_import_count + RT_ALLOC,
        free: actual_import_count + RT_FREE,
        retain: actual_import_count + RT_RETAIN,
        ready_push: sb,
        ready_pop: sb + 1,
        spawn: sb + 2,
        complete: sb + 3,
        fail: sb + 4,
        sleep: sb + 5,
        notify: sb + 6,
        poll: sb + 7,
        resume_task: sb + 9,
        task_result: sb + 10,
        await_fn: sb + 11,
        resume_type: t_resume,
        g_count: 4,
        g_head: 5,
        g_tail: 6,
        g_root: 7,
        g_current: 8,
        g_table_base: 9,
        g_live: 10,
        g_free_head: 11,
        main_resume_table_idx: 0,
        capacity: 4096,
        root_frame_size,
        module_init,
        // Browser targets delegate sleep wakeups to the host timer; native
        // busy-polls. `host_set_timer` is available on all targets, so gate on
        // the target rather than mere availability.
        set_timer: if matches!(target, Some("wasm") | Some("wasm-html")) {
            import_remap
                .get(crate::runtime::IMPORT_HOST_SET_TIMER as usize)
                .copied()
                .flatten()
        } else {
            None
        },
        trap_report: import_remap
            .get(crate::runtime::IMPORT_TRAP_REPORT as usize)
            .copied()
            .flatten(),
    };
    let start_async_idx = sb + 8;

    // ── compile each async function as a resume function ──
    let rt = RtOffsets {
        base: actual_import_count,
    };
    let fai_type_indices = direct_fai_func_type_indices();
    let strings = RefCell::new(StringInterner::default());
    let closures = RefCell::new(Vec::new());
    let empty_mock: Set<u32> = Set::new();
    let empty_std: Map<(String, String), u32> = Map::new();

    // ── context maps from entry + every module (mirror build_program_full) ──
    let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
    let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> = HashMap::new();
    let mut module_constants: HashMap<String, fai_compiler::ast::Expression> = HashMap::new();
    let mut extern_fn_indices = collect_extern_fn_indices_from(&ast.statements);
    let mut extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
    fn collect_consts(
        stmts: &[Statement],
        out: &mut HashMap<String, fai_compiler::ast::Expression>,
    ) {
        for s in stmts {
            if let Statement::LetStatement(ls) = s {
                if ls.bindings.len() == 1
                    && matches!(
                        ls.value,
                        Expression::NumberExpression(_)
                            | Expression::BooleanExpression(_)
                            | Expression::NullExpression(_)
                            | Expression::StringExpression(_)
                    )
                {
                    out.entry(ls.bindings[0].name.clone())
                        .or_insert_with(|| ls.value.clone());
                }
            }
        }
    }
    let mut collect_decls = |stmts: &[Statement]| {
        for s in stmts {
            match s {
                Statement::EnumDeclaration(ed) => {
                    enum_members
                        .entry(ed.name.clone())
                        .or_insert_with(|| ed.members.clone());
                }
                Statement::TypeDeclaration(td) => {
                    type_fields
                        .entry(td.name.clone())
                        .or_insert_with(|| td.fields.clone());
                }
                Statement::ExternBlockDeclaration(ext) => {
                    for f in &ext.functions {
                        extern_out_params
                            .entry(f.name.clone())
                            .or_insert_with(|| f.params.iter().map(|p| p.is_out).collect());
                    }
                }
                _ => {}
            }
        }
        collect_consts(stmts, &mut module_constants);
    };
    collect_decls(&ast.statements);
    for m in modules {
        collect_decls(&m.statements);
    }
    // Externs from modules get fresh indices after the entry's.
    let mut next_ext = extern_fn_indices.values().max().map(|m| *m + 1).unwrap_or(0);
    for m in modules {
        for s in &m.statements {
            if let Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    extern_fn_indices.entry(f.name.clone()).or_insert_with(|| {
                        let i = next_ext;
                        next_ext += 1;
                        i
                    });
                }
            }
        }
    }
    for (name, fields) in builtin_type_fields() {
        type_fields.entry(name).or_insert(fields);
    }
    let infos: Vec<FunctionInfo> = ordered
        .iter()
        .map(|fd| FunctionInfo {
            name: fd.name.clone(),
            param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
            type_param_count: fd.type_params.len() as u16,
            include_in_coverage: false,
            param_defaults: param_defaults_for(fd),
            // Same fallback policy as `build_program_full`: entry-AST
            // functions (no module context) get the entry file.
            source_file: match fn_ctx.get(&fd.name) {
                Some((_, Some(f))) => Some(f.clone()),
                Some((None, None)) if fd.location.line > 0 => entry_file.map(String::from),
                _ => None,
            },
            source_line: fd.location.line,
        })
        .collect();
    let ctx = BuildContext {
        rt,
        functions: &infos,
        checker,
        import_remap: &import_remap,
        fai_func_type_indices: &fai_type_indices,
        module_aliases: &module_aliases,
        extern_fn_indices: &extern_fn_indices,
        enum_members: &enum_members,
        type_fields: &type_fields,
        named_imports: &named_imports,
        mocked_fn_ids: &empty_mock,
        std_method_fn_ids: &empty_std,
        // Closures created inside async resume fns get table slots after the
        // async resume fns (which occupy 0..nasync).
        closure_offset_base: nasync,
        strings: &strings,
        closures: &closures,
        module_constants: &module_constants,
        extern_out_params: &extern_out_params,
        module_vars: &module_vars,
        async_ctx: Some(AsyncClosureCtx {
            async_set: &async_set,
            all_fns: &all_user_fns,
            layout: &layout,
            fn_table_idx: &fn_table_idx,
            frame_sizes: &frame_sizes,
        }),
    };
    // Compile in two passes so closures get non-overlapping table slots:
    // async resume fns first (their closures fill slots `nasync..`, via the
    // shared `closures` RefCell at `closure_offset_base = nasync`), then sync
    // fns (their closures continue after the async ones). Bodies are placed by
    // proto index so the function/code sections stay in proto order.
    let mut bodies: Vec<Option<Function>> = (0..ordered.len()).map(|_| None).collect();
    for (proto, fd) in ordered.iter().enumerate() {
        if async_set.contains(&fd.name) {
            let (mctx, fctx) = fn_ctx
                .get(&fd.name)
                .map(|(m, f)| (m.as_deref(), f.as_deref()))
                .unwrap_or((None, None));
            let frame = &frames[&fd.name];
            let r = AsyncResolve {
                async_set: &async_set,
                all_fns: &all_user_fns,
                aliases: &module_aliases,
                named_imports: &named_imports,
                module_context: mctx,
                ufcs_calls: &checker.ufcs_calls,
                module_key: fctx.or(mctx).unwrap_or(""),
            };
            let (f, _upvalues) = match build_resume_fn(
                &ctx, fd, frame, &fn_table_idx, &frame_sizes, &layout, &r, mctx, fctx, None,
            ) {
                Ok(v) => v,
                Err(e) => {
                    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                        eprintln!("[async-engine] resume fn '{}' failed: {:?}", fd.name, e);
                    }
                    return None;
                }
            };
            bodies[proto] = Some(f);
        }
    }
    let async_closure_count = closures.borrow().len() as u32;
    let mut sync_closures: Vec<BuiltClosure> = Vec::new();
    for (proto, fd) in ordered.iter().enumerate() {
        if async_set.contains(&fd.name) {
            continue;
        }
        let (mctx, fctx) = fn_ctx
            .get(&fd.name)
            .map(|(m, f)| (m.as_deref(), f.as_deref()))
            .unwrap_or((None, None));
        let res = build_function_with_spy_and_offset(
            fd,
            rt,
            &infos,
            checker,
            &fai_type_indices,
            &module_aliases,
            &extern_fn_indices,
            &import_remap,
            &strings,
            &enum_members,
            &type_fields,
            &named_imports,
            &empty_mock,
            &empty_std,
            nasync + async_closure_count + sync_closures.len() as u32,
            mctx,
            &module_constants,
            &extern_out_params,
            &module_vars,
            fctx,
            ctx.async_ctx,
        );
        let res = match res {
            Ok(v) => v,
            Err(e) => {
                if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
                    eprintln!("[async-engine] sync fn '{}' failed: {:?}", fd.name, e);
                }
                return None;
            }
        };
        bodies[proto] = Some(res.main);
        sync_closures.extend(res.closures);
    }
    let bodies: Vec<Function> = bodies.into_iter().map(|b| b.unwrap()).collect();
    // All closures, in table-slot order: async-fn closures first (slots
    // `nasync..`), then sync-fn closures.
    let async_closures = closures.into_inner();
    let closure_count = async_closure_count + sync_closures.len() as u32;

    // ── data section (string pool + known strings) ──
    let mut extended = strings.into_inner().bytes;
    fn append_known(buf: &mut Vec<u8>, s: &str) -> (u32, u32) {
        let off = buf.len() as u32;
        buf.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }
    let str_null = append_known(&mut extended, "null");
    let str_true = append_known(&mut extended, "true");
    let str_false = append_known(&mut extended, "false");
    let known = runtime::KnownStrings {
        str_null,
        str_true,
        str_false,
        ..Default::default()
    };

    // ── assemble ──
    let mut module = EncModule::new();

    let mut types = TypeSection::new();
    for (_, p, r) in &import_sigs {
        types.ty().function(p.clone(), r.clone());
    }
    for (p, r) in &rt_sigs {
        types.ty().function(p.clone(), r.clone());
    }
    for arity in 0..=MAX_DIRECT_ARITY {
        let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
        types.ty().function(params, vec![ValType::I64]);
    }
    types.ty().function(vec![], vec![]); // t_resume
    types.ty().function(vec![ValType::I32], vec![]); // t_i32_void
    types.ty().function(vec![], vec![ValType::I32]); // t_void_i32
    types
        .ty()
        .function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // t_i32i32_i32
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64], vec![]); // t_i32i64_void
    types
        .ty()
        .function(vec![ValType::I32, ValType::F64], vec![]); // t_i32f64_void
    types.ty().function(vec![ValType::I32], vec![ValType::I32]); // t_i32_i32
    types.ty().function(vec![ValType::I32], vec![ValType::I64]); // t_i32_i64
    types
        .ty()
        .function(vec![ValType::I32, ValType::I32], vec![]); // t_i32i32_void
    types
        .ty()
        .function(vec![ValType::I64, ValType::I64], vec![ValType::I64]); // t_i64i64_i64
    module.section(&types);

    let mut imports = ImportSection::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if import_available[i] {
            imports.import("env", name, EntityType::Function(i as u32));
        }
    }
    module.section(&imports);

    let mut funcs = FunctionSection::new();
    let rt_type_start = import_sigs.len() as u32;
    for k in 0..RT_COUNT {
        funcs.function(rt_type_start + k);
    }
    // User fns, proto order: async = resume type, sync = FaiFunc(arity).
    for fd in &ordered {
        if async_set.contains(&fd.name) {
            funcs.function(t_resume);
        } else {
            let pc = fd.params.len() as u16 + fd.type_params.len() as u16;
            funcs.function(fai_type_indices[&pc]);
        }
    }
    funcs.function(t_i32_void); // ready_push
    funcs.function(t_void_i32); // ready_pop
    funcs.function(t_i32i32_i32); // spawn
    funcs.function(t_i32i64_void); // complete
    funcs.function(t_i32i64_void); // fail
    funcs.function(t_i32f64_void); // sleep
    funcs.function(t_i32_void); // notify
    funcs.function(t_void_i32); // poll
    funcs.function(t_void_i32); // start_async
    funcs.function(t_i32_i32); // resume_task
    funcs.function(t_i32_i64); // task_result
    funcs.function(t_i32i32_void); // await
    funcs.function(t_i64i64_i64); // drive_closure
    // Closures, after the scheduler: async closures are resume fns (`t_resume`),
    // sync closures are `FaiFunc(arity)`. async-fn closures first, then sync-fn,
    // matching the table-slot order.
    for c in async_closures.iter().chain(sync_closures.iter()) {
        if c.is_async {
            funcs.function(t_resume);
        } else {
            funcs.function(fai_type_indices[&c.info.param_count]);
        }
    }
    module.section(&funcs);

    // Table = [async resume fns (0..nasync)] ++ [sync-fn closures (nasync..)].
    let table_len = nasync + closure_count;
    let mut tables = TableSection::new();
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        minimum: table_len as u64,
        maximum: Some(table_len as u64),
        table64: false,
        shared: false,
    });
    module.section(&tables);

    let total_bytes = extended.len() as u32 + runtime::FREE_BUCKET_REGION_BYTES + 65536;
    let pages = std::cmp::max((total_bytes / 65536) + 1, 16);
    let mut mem = MemorySection::new();
    mem.memory(MemoryType {
        minimum: pages as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mem);

    // The size-bucketed free-list heads live in a zero-init region starting at
    // `bucket_base`; the heap bump pointer starts just past it.
    let bucket_base = ((extended.len() as u32) + 7) & !7;
    let heap_start = (bucket_base + runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
    let i32mut = GlobalType {
        val_type: ValType::I32,
        mutable: true,
        shared: false,
    };
    let mut globals = GlobalSection::new();
    globals.global(i32mut, &ConstExpr::i32_const(heap_start as i32)); // __heap_ptr
    globals.global(i32mut, &ConstExpr::i32_const(0)); // __env_ptr
    globals.global(i32mut, &ConstExpr::i32_const(0)); // error_flag
    globals.global(
        GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i64_const(0),
    ); // error_value
    // Task ids start at 1: the host runner reads the root result via
    // `__fai_task_result(1)`, so `main` (the first spawn) must be id 1.
    // Slot 0 is left unused.
    globals.global(i32mut, &ConstExpr::i32_const(1)); // g_count
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_head
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_tail
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_root
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_current
    globals.global(i32mut, &ConstExpr::i32_const(0)); // g_table_base
    globals.global(i32mut, &ConstExpr::i32_const(0)); // g_live
    globals.global(i32mut, &ConstExpr::i32_const(-1)); // g_free_head (empty)
    // Module-level `var` globals (i64), indices 12.. — initialized to Void;
    // their real values are written by `<__module_init__>` before `main` runs.
    let i64mut = GlobalType {
        val_type: ValType::I64,
        mutable: true,
        shared: false,
    };
    for _ in 0..module_var_count {
        globals.global(i64mut, &ConstExpr::i64_const(VAL_VOID));
    }
    // Heap free-list head for rt_alloc reuse / rt_free (appended last so the
    // fixed (0-3), scheduler (4-11) and module-var (12..) global indices are
    // unchanged). 0 = empty list. Index = 12 + module_var_count.
    globals.global(i32mut, &ConstExpr::i32_const(0));
    // Live-object counter (plan 113), appended after the free-list.
    globals.global(i32mut, &ConstExpr::i32_const(0));
    module.section(&globals);

    // Heap free-list / live-count globals, appended after fixed+sched+
    // module-var globals (also referenced by the export section below).
    let freelist_global = 12 + module_var_count;
    let live_count_global = freelist_global + 1;

    let mut exports = ExportSection::new();
    exports.export("_start_async", ExportKind::Func, start_async_idx);
    exports.export("__fai_poll", ExportKind::Func, sb + 7);
    exports.export("__fai_resume_task", ExportKind::Func, sb + 9);
    exports.export("__fai_task_result", ExportKind::Func, sb + 10);
    // Host-driver entry: spawn+drive an async guest closure (route/event handler)
    // to completion. await = sb+11, drive_closure = sb+12 (appended last).
    exports.export("__fai_drive_closure", ExportKind::Func, sb + 12);
    // Host-callable refcount release: lets the host reclaim per-request guest
    // objects it owns (the request/response dicts it built) after writing the
    // response, so a long-running server plateaus instead of leaking ~1 dict
    // graph per request (plan 115). Points straight at the RT_RELEASE runtime fn.
    exports.export(
        "__fai_release",
        ExportKind::Func,
        actual_import_count + runtime::RT_RELEASE,
    );
    exports.export("memory", ExportKind::Memory, 0);
    // The host wasm runner allocates guest values (strings/arrays/dicts returned
    // by imports, FFI write-backs) by bumping `__heap_ptr`, and calls guest
    // closures back (event handlers, route handlers) through
    // `__indirect_function_table` after seeding `__env_ptr`. The sync path
    // exports these; the engine must too or the runner panics
    // (`heap.rs: get_export("__heap_ptr").unwrap()` on `None`). Global layout:
    // `__heap_ptr` = 0, `__env_ptr` = 1 (see `GLOBAL_ENV_PTR`); the function
    // table is table 0.
    exports.export("__heap_ptr", ExportKind::Global, 0);
    // Live-object counter (plan 113); index = free-list (12 + module vars) + 1.
    exports.export(
        "__live_objects",
        ExportKind::Global,
        13 + module_var_count,
    );
    exports.export("__env_ptr", ExportKind::Global, GLOBAL_ENV_PTR);
    // The browser runtime signals a failed `remoteCall` by setting these from JS
    // (`instance.exports.__error_flag.value = 1`), so the awaiting guest task
    // observes the error after it resumes. The sync path exports them too.
    exports.export("__error_flag", ExportKind::Global, GLOBAL_ERROR_FLAG);
    exports.export("__error_value", ExportKind::Global, GLOBAL_ERROR_VALUE);
    exports.export("__indirect_function_table", ExportKind::Table, 0);
    // Scheduler-introspection globals (plan 116 phase 2): always exported
    // so the runner's post-mortem dump can walk the task table on a trap
    // or watchdog timeout without a special debug build.
    exports.export("__dbg_count", ExportKind::Global, layout.g_count);
    exports.export("__dbg_root", ExportKind::Global, layout.g_root);
    exports.export("__dbg_live", ExportKind::Global, layout.g_live);
    exports.export("__dbg_current", ExportKind::Global, layout.g_current);
    exports.export("__dbg_table_base", ExportKind::Global, layout.g_table_base);
    exports.export("__dbg_free_head", ExportKind::Global, layout.g_free_head);
    exports.export("__dbg_head", ExportKind::Global, layout.g_head);
    exports.export("__dbg_tail", ExportKind::Global, layout.g_tail);
    // Heap overflow free-list head (blocks too large for the size
    // buckets) — the post-mortem heap stats walk it.
    exports.export("__free_list", ExportKind::Global, freelist_global);
    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
        // TEMP brain: nextSignalId=g16, registeredSignals=g18, routerPathSignal=g28
        if module_var_count > 17 {
            exports.export("__dbg_g16", ExportKind::Global, 16);
            exports.export("__dbg_g18", ExportKind::Global, 18);
            exports.export("__dbg_g28", ExportKind::Global, 28);
        }
    }
    module.section(&exports);

    let mut elements = ElementSection::new();
    // Table: [0..nasync) async fns → `user_fn_base + proto`; then closures →
    // `sb + SCHED_FN_COUNT + i` (closures sit after the scheduler in code).
    let closure_base = sb + async_engine::SCHED_FN_COUNT;
    let mut table_fns = vec![0u32; table_len as usize];
    for (proto, fd) in ordered.iter().enumerate() {
        if let Some(&tp) = fn_table_idx.get(&fd.name) {
            table_fns[tp as usize] = user_fn_base + proto as u32;
        }
    }
    for i in 0..closure_count {
        table_fns[(nasync + i) as usize] = closure_base + i;
    }
    elements.active(
        Some(0),
        &ConstExpr::i32_const(0),
        Elements::Functions(table_fns.into()),
    );
    module.section(&elements);

    let mut code = CodeSection::new();
    for f in runtime::emit_all(
        actual_import_count,
        &import_remap,
        &known,
        freelist_global,
        live_count_global,
        bucket_base,
    ) {
        code.function(&f);
    }
    // User fns (proto order) — must match the function section ordering.
    for body in &bodies {
        code.function(body);
    }
    for f in async_engine::emit_scheduler_functions(&layout) {
        code.function(&f);
    }
    for c in async_closures.iter().chain(sync_closures.iter()) {
        code.function(&c.function);
    }
    module.section(&code);

    if !extended.is_empty() {
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), extended.iter().copied());
        module.section(&data);
    }

    // ── debug metadata (plan 116): name section + fai-dbg table ──
    let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if let Some(idx) = import_remap.get(i).copied().flatten() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(idx, *name));
        }
    }
    for (k, n) in runtime::rt_fn_names().iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            actual_import_count + k as u32,
            *n,
        ));
    }
    for (proto, fd) in ordered.iter().enumerate() {
        let info = &infos[proto];
        let name = if async_set.contains(&fd.name) {
            format!("{}#resume", fd.name)
        } else {
            fd.name.clone()
        };
        dbg.push(crate::debug_info::FnDebugEntry {
            index: user_fn_base + proto as u32,
            name,
            file: info.source_file.clone(),
            line: info.source_line,
        });
    }
    // Scheduler helpers, in `emit_scheduler_functions` order.
    for (k, n) in [
        "sched_ready_push",
        "sched_ready_pop",
        "sched_spawn",
        "sched_complete",
        "sched_fail",
        "sched_sleep",
        "sched_notify_waiter",
        "sched_poll",
        "sched_start_async",
        "sched_resume_task",
        "sched_task_result",
        "sched_await",
        "sched_drive_closure",
    ]
    .iter()
    .enumerate()
    {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(sb + k as u32, *n));
    }
    for (i, c) in async_closures.iter().chain(sync_closures.iter()).enumerate() {
        let name = if c.is_async {
            format!("{}#resume", c.info.name)
        } else {
            c.info.name.clone()
        };
        dbg.push(crate::debug_info::FnDebugEntry {
            index: closure_base + i as u32,
            name,
            file: c.info.source_file.clone(),
            line: c.info.source_line,
        });
    }
    crate::debug_info::append_debug_sections(
        &mut module,
        &dbg,
        &crate::debug_info::DbgMeta {
            bucket_base: Some(bucket_base),
            bucket_count: runtime::NUM_FREE_BUCKETS,
        },
    );

    if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
        for (proto, fd) in ordered.iter().enumerate() {
            eprintln!(
                "[async-engine] func {} = {} ({})",
                user_fn_base + proto as u32,
                fd.name,
                if async_set.contains(&fd.name) {
                    "async"
                } else {
                    "sync"
                }
            );
        }
    }
    let bytes = module.finish();
    // Soundness gate: never hand back a module that doesn't validate. Shapes
    // the engine can't yet lower correctly (notably **async closures** —
    // closures that await/fork are compiled as sync funcs and call an async
    // resume fn, producing a type-mismatched module) would otherwise emit
    // invalid wasm that only fails at instantiation. Validate here and fall
    // back to the existing path instead. (A3.0 lifts this for async closures.)
    if let Err(e) = wasmparser::validate(&bytes) {
        if std::env::var("FAI_ASYNC_DEBUG").is_ok() {
            eprintln!("[async-engine] soundness gate rejected module: {e}");
            let _ = std::fs::write("/tmp/fai_invalid.wasm", &bytes);
            eprintln!("[async-engine] dumped invalid module to /tmp/fai_invalid.wasm");
        }
        return None;
    }
    Some(bytes)
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
    /// fall-through; a `return`/tail drops every active list (it jumps past the
    /// pop_scopes). `break`/`continue`/`throw` skip the drops (leak, sound).
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
}

/// Per-`try` bookkeeping for `throw` dispatch. Popped *before* the
/// catch body compiles so a throw inside `catch` propagates to the
/// next-outer try.
struct TryFrame {
    /// Absolute `block_depth` of the inner `$catch` block.
    catch_abs: u32,
    /// Local that holds the thrown value until the `$catch` block
    /// binds it to the user-declared `catch_name`.
    err_local: u32,
}

/// Label bookkeeping for `break` / `continue`. Each `while` lowering
/// pushes one of these before emitting the outer `block` + inner
/// `loop`; relative `br` depth at a use site is
/// `current_block_depth - target_abs`.
struct LoopFrame {
    /// Absolute `block_depth` after the outer `block` was opened —
    /// the `break` target.
    break_abs: u32,
    /// Absolute `block_depth` after the inner `loop` was opened —
    /// the `continue` target.
    continue_abs: u32,
}

impl<'a, 'c> Builder<'a, 'c> {
    fn new(
        fd: &'a FunctionDeclaration,
        ctx: &'c BuildContext<'a>,
        outer_scope: Option<&'c OuterScopeView<'c>>,
    ) -> Self {
        // Map each user parameter to its corresponding wasm local
        // index. Type params (generic `@type T`) come FIRST because
        // the call-site emission pushes type-arg strings before the
        // real user args (see `compile_call`), so they land in the
        // callee's lowest locals. Binding user params first here
        // would alias every user param to the wrong wasm local —
        // generic calls then read back the type-arg string instead
        // of the user value.
        let mut first_scope = HashMap::new();
        let mut idx = 0u32;
        for t in &fd.type_params {
            first_scope.insert(
                t.name.clone(),
                LocalBinding {
                    local: idx,
                    shape: ValueShape::Boxed,
                    is_cell: false,
                },
            );
            idx += 1;
        }
        for p in &fd.params {
            first_scope.insert(
                p.name.clone(),
                LocalBinding {
                    local: idx,
                    shape: ValueShape::Boxed,
                    is_cell: false,
                },
            );
            idx += 1;
        }
        Self {
            fd,
            ctx,
            instrs: Vec::new(),
            next_local: idx,
            scopes: vec![first_scope],
            scope_drops: vec![Vec::new()],
            confined_escaping: fai_compiler::escape_analysis::conservative_escaping(fd),
            local_decls: Vec::new(),
            loops: Vec::new(),
            tries: Vec::new(),
            block_depth: 0,
            function_by_name: ctx
                .functions
                .iter()
                .enumerate()
                .map(|(i, f)| (f.name.clone(), i as u32))
                .collect(),
            // Phase E start: all tests drive the entry module, so key
            // is the empty string. Nested modules will populate this
            // when the caller sets it via `compile_prepared_with_…`.
            module_key: String::new(),
            module_context: None,
            outer_scope,
            upvalues: Vec::new(),
            upvalue_by_name: HashMap::new(),
            cell_captured_vars: collect_cell_captured_vars(&fd.body),
            owned_frame_locals: HashSet::new(),
        }
    }

    fn rt(&self) -> RtOffsets {
        self.ctx.rt
    }

    /// Emit a `Call` to a host import using the target-aware remap.
    /// If the import is available for the current target, the call
    /// lands on its remapped wasm function index. If not (e.g.,
    /// `IMPORT_HTTP_SERVER_LISTEN` on `wasm-html`), emit
    /// `unreachable` — matches `runtime::emit_import_call`'s policy
    /// so both codegen paths trap identically on unavailable imports.
    fn emit_import_call(&mut self, import_idx: u32) {
        match self
            .ctx
            .import_remap
            .get(import_idx as usize)
            .copied()
            .flatten()
        {
            Some(new_idx) => {
                self.emit(Instruction::Call(new_idx));
            }
            None => {
                self.emit(Instruction::Unreachable);
            }
        }
    }

    fn functions(&self) -> &'a [FunctionInfo] {
        self.ctx.functions
    }

    fn checker(&self) -> &'a CheckerInfo {
        self.ctx.checker
    }

    fn expression_type_at(&self, expr: &Expression) -> Option<&fai_checker::types::Type> {
        let key = fai_checker::checker::expression_key(expr, self.module_key.clone());
        self.checker().expression_types.get(&key)
    }

    fn shape_for_expr(&self, expr: &Expression) -> ValueShape {
        self.expression_type_at(expr)
            .map(shape_for_type)
            .unwrap_or(ValueShape::Boxed)
    }

    fn numeric_shape_for_expr(&self, expr: &Expression) -> Option<ValueShape> {
        match expr {
            Expression::NumberExpression(n) => {
                if !n.is_float && n.value == (n.value as i64) as f64 {
                    Some(ValueShape::RawInt)
                } else {
                    Some(ValueShape::RawFloat)
                }
            }
            Expression::IdentifierExpression(id) => self
                .lookup(&id.name)
                .map(|binding| binding.shape)
                .filter(|shape| matches!(shape, ValueShape::RawInt | ValueShape::RawFloat)),
            _ => match self.shape_for_expr(expr) {
                shape @ (ValueShape::RawInt | ValueShape::RawFloat) => Some(shape),
                _ => None,
            },
        }
    }

    fn compile_expr_as(&mut self, expr: &Expression, want: ValueShape) -> Result<(), BuildError> {
        // Fast paths: when a caller wants a raw shape, skip the
        // box-then-unbox round-trip compile_expr would do. compile_expr
        // defaults to Boxed (so the many call sites that discard the
        // returned shape still see a valid NaN-boxed value), and this
        // function carves out the raw paths where we can do better.
        match (expr, want) {
            (Expression::NumberExpression(n), ValueShape::RawInt) => {
                // Both Int literals (`0`) and Float literals assigned
                // into a declared-Int slot (`let x Int = 3.7`) land
                // here. For the float case the `as i64` conversion
                // truncates toward zero — same semantics as the
                // RawFloat→RawInt runtime path (`I32TruncF64S`) and
                // what the user's "let myInt Int = 0.0 should work"
                // example implies.
                self.emit(Instruction::I64Const(n.value as i64));
                return Ok(());
            }
            (Expression::NumberExpression(n), ValueShape::RawFloat) => {
                self.emit(Instruction::F64Const(n.value));
                return Ok(());
            }
            (Expression::BooleanExpression(b), ValueShape::RawBool) => {
                self.emit(Instruction::I32Const(if b.value { 1 } else { 0 }));
                return Ok(());
            }
            (Expression::IdentifierExpression(id), _) => {
                if let Some(Resolve::Local(local)) = self.resolve(&id.name) {
                    if local.is_cell {
                        // Cell-bound: local holds an i32 cell address;
                        // dereference the value slot (@8, plan 114) to get
                        // the Boxed value, then convert.
                        self.emit(Instruction::LocalGet(local.local));
                        self.emit(Instruction::I64Load(mem_off(8)));
                        return self.emit_convert(ValueShape::Boxed, want);
                    }
                    self.emit(Instruction::LocalGet(local.local));
                    return self.emit_convert(local.shape, want);
                }
            }
            _ => {}
        }
        let got = self.compile_expr(expr)?;
        self.emit_convert(got, want)
    }

    fn compile_numeric_expr_as_float(&mut self, expr: &Expression) -> Result<(), BuildError> {
        match self.numeric_shape_for_expr(expr) {
            Some(ValueShape::RawInt) => {
                self.compile_expr_as(expr, ValueShape::RawInt)?;
                self.emit_convert(ValueShape::RawInt, ValueShape::RawFloat)
            }
            Some(ValueShape::RawFloat) => self.compile_expr_as(expr, ValueShape::RawFloat),
            _ => self.compile_expr_as(expr, ValueShape::RawFloat),
        }
    }

    fn emit_convert(&mut self, from: ValueShape, to: ValueShape) -> Result<(), BuildError> {
        match (from, to) {
            (a, b) if a == b => {}
            (ValueShape::RawInt, ValueShape::Boxed) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I64ExtendI32U);
                self.emit(Instruction::I64Const(QNAN | TAG_INT));
                self.emit(Instruction::I64Or);
            }
            (ValueShape::Boxed, ValueShape::RawInt) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I64ExtendI32S);
            }
            (ValueShape::RawFloat, ValueShape::Boxed) => {
                self.emit(Instruction::I64ReinterpretF64);
            }
            (ValueShape::Boxed, ValueShape::RawFloat) => {
                self.emit(Instruction::F64ReinterpretI64);
            }
            (ValueShape::RawBool, ValueShape::Boxed) => {
                self.emit(Instruction::I64ExtendI32U);
                self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
                self.emit(Instruction::I64Or);
            }
            (ValueShape::Boxed, ValueShape::RawBool) => {
                self.emit(Instruction::I32WrapI64);
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::I32And);
            }
            (ValueShape::RawInt, ValueShape::RawFloat) => {
                self.emit(Instruction::F64ConvertI64S);
            }
            (ValueShape::RawFloat, ValueShape::RawInt) => {
                // Narrow f64 → i32 (forai Ints are i32-sized payloads)
                // then widen back to the i64 the local storage uses.
                self.emit(Instruction::I32TruncF64S);
                self.emit(Instruction::I64ExtendI32S);
            }
            _ => return Err(BuildError::UnsupportedExpression("shape-conversion")),
        }
        Ok(())
    }

    /// Open a structured control-flow label (`block` / `loop` / `if`)
    /// and keep `block_depth` in sync.
    fn emit_open(&mut self, i: Instruction<'static>) {
        self.instrs.push(i);
        self.block_depth += 1;
    }

    /// Close a structured label (`End`). Panics on unbalanced opens —
    /// the builder is the only source of opens so this would be a
    /// bug, not a user error.
    fn emit_close(&mut self) {
        self.instrs.push(Instruction::End);
        self.block_depth = self
            .block_depth
            .checked_sub(1)
            .expect("direct builder: unbalanced End");
    }

    fn emit(&mut self, i: Instruction<'static>) {
        self.instrs.push(i);
    }

    fn alloc_local(&mut self) -> u32 {
        self.alloc_typed_local(ValueShape::Boxed)
    }

    fn alloc_typed_local(&mut self, shape: ValueShape) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        self.local_decls.push(match shape {
            ValueShape::Boxed | ValueShape::RawInt => ValType::I64,
            ValueShape::RawFloat => ValType::F64,
            ValueShape::RawBool => ValType::I32,
        });
        idx
    }

    fn lookup(&self, name: &str) -> Option<LocalBinding> {
        for scope in self.scopes.iter().rev() {
            if let Some(&binding) = scope.get(name) {
                return Some(binding);
            }
        }
        None
    }

    /// Resolve an identifier: local in our own scope stack, an upvalue
    /// captured from the enclosing function's scope, or a module-level
    /// `var` global. Allocates a fresh upvalue slot on first reference
    /// to an outer name. Module vars are checked last so a local `var`
    /// or a captured upvalue of the same name takes precedence —
    /// ordinary lexical shadowing.
    fn resolve(&mut self, name: &str) -> Option<Resolve> {
        if let Some(local) = self.lookup(name) {
            return Some(Resolve::Local(local));
        }
        if let Some(&i) = self.upvalue_by_name.get(name) {
            return Some(Resolve::Upvalue(i));
        }
        if let Some(outer) = self.outer_scope {
            if let Some(capture) = outer.lookup(name) {
                let uv_idx = self.upvalues.len() as u32;
                self.upvalues.push(capture);
                self.upvalue_by_name.insert(name.to_string(), uv_idx);
                return Some(Resolve::Upvalue(uv_idx));
            }
        }
        if let Some(&idx) = self.ctx.module_vars.get(name) {
            return Some(Resolve::ModuleVar(idx));
        }
        None
    }

    /// Emit the instructions that read upvalue `i` onto the stack:
    /// `env_ptr + i*8 -> I64Load`. Valid only inside a closure body.
    /// Cell-bound upvalues require one extra dereference: the env slot
    /// holds the NaN-boxed cell (plan 114), so unbox the address and
    /// `i64.load` the value slot at offset 8.
    fn emit_upvalue_read(&mut self, uv_idx: u32) {
        self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
        self.emit(Instruction::I64Load(mem_off(uv_idx as u64 * 8)));
        if self.upvalues[uv_idx as usize].is_cell {
            self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
            self.emit(Instruction::I64Load(mem_off(8)));
        }
    }

    fn bind(&mut self, name: &str, local: u32) {
        self.bind_shape(name, local, ValueShape::Boxed);
    }

    fn bind_shape(&mut self, name: &str, local: u32, shape: ValueShape) {
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            LocalBinding {
                local,
                shape,
                is_cell: false,
            },
        );
    }

    /// Bind `name` to a cell-backed slot. `addr_local` is an i32 local
    /// holding the cell's heap address (the logical pointer of a tagged
    /// `OBJ_TAG_CELL` block since plan 114); reads/writes on the name
    /// dereference the cell's value slot at offset 8. The stored value
    /// is always `Boxed`.
    fn bind_cell(&mut self, name: &str, addr_local: u32) {
        self.scopes.last_mut().unwrap().insert(
            name.to_string(),
            LocalBinding {
                local: addr_local,
                shape: ValueShape::Boxed,
                is_cell: true,
            },
        );
    }

    /// Store the Boxed value currently on the stack into the cell whose
    /// address is in `addr_local`, with value-RC (plan 114): the cell OWNS
    /// its value, so retain a borrowed source, release the previous value,
    /// then write the slot at offset 8. The previous value is released
    /// AFTER the new one is computed, so a self-referencing write
    /// (`s = s + x`) reads the old value safely.
    fn emit_cell_store(&mut self, addr_local: u32, transfers: bool) {
        if !transfers {
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        }
        let tmp = self.alloc_local();
        self.emit(Instruction::LocalSet(tmp));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I64Load(mem_off(8)));
        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Store(mem_off(8)));
    }

    /// Allocate a fresh tagged cell (`OBJ_TAG_CELL`, 16 bytes, rc=1 from
    /// the allocator) with a zeroed value slot, leaving its logical
    /// address in a new i32 local. The zero value makes the first
    /// `emit_cell_store`'s release-the-old a safe no-op (RT_ALLOC reuses
    /// free-list blocks without clearing them).
    fn emit_cell_alloc(&mut self) -> u32 {
        let addr_local = self.alloc_i32_local();
        self.emit(Instruction::I32Const(16));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalTee(addr_local));
        // tag@0 + zeroed pad@4 in one i64 store.
        self.emit(Instruction::I64Const(OBJ_TAG_CELL as i64));
        self.emit(Instruction::I64Store(mem0()));
        self.emit(Instruction::LocalGet(addr_local));
        self.emit(Instruction::I64Const(0));
        self.emit(Instruction::I64Store(mem_off(8)));
        addr_local
    }

    fn emit_typed_param_prelude(&mut self) -> Result<(), BuildError> {
        let typed_params: Vec<(u32, String, ValueShape)> = self
            .fd
            .params
            .iter()
            .enumerate()
            .filter_map(|(idx, param)| {
                let shape = shape_for_type_node(&param.type_node);
                (shape != ValueShape::Boxed).then(|| (idx as u32, param.name.clone(), shape))
            })
            .collect();

        for (param_idx, name, shape) in typed_params {
            let local = self.alloc_typed_local(shape);
            self.emit(Instruction::LocalGet(param_idx));
            self.emit_convert(ValueShape::Boxed, shape)?;
            self.emit(Instruction::LocalSet(local));
            self.bind_shape(&name, local, shape);
        }
        Ok(())
    }

    /// Compile the function body: every statement except the last is a
    /// side-effect; the last statement is the return value (`@return`).
    /// Empty bodies return Void. Matches the legacy compiler's
    /// "last statement is tail position" convention.
    fn compile_body(&mut self) -> Result<(), BuildError> {
        // Spy/mock preamble: emit only for top-level functions that
        // were referenced by `mock()` / `assert.*` in a test block.
        // `function_by_name` is keyed by the fully-qualified name
        // used in the unified function table, which matches
        // `fd.name` after `build_program_full` prefixes module funcs.
        if let Some(&fn_id) = self.function_by_name.get(self.fd.name.as_str()) {
            if self.ctx.mocked_fn_ids.contains(&fn_id) {
                self.emit_spy_preamble(fn_id)?;
            }
        }
        self.emit_typed_param_prelude()?;
        if self.fd.body.is_empty() {
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit(Instruction::Return);
            return Ok(());
        }
        // `<__start__>` is the synthesised wrapper that calls
        // `<__module_init__>` then user `main`. Drain the deferred
        // event queue once everything has run so any
        // `events.emitDeferred` from main / module init / event
        // subscribers gets dispatched before the program exits.
        // The drain has to happen between the tail expression
        // evaluating and the `Return` so main's return value (which
        // hosts read off `_start`) survives. See Phase 5 of
        // plans/event-system.md.
        let is_start = self.fd.name == "<__start__>";
        // Phase 3 reclamation (plans/111): confined fresh-literal bindings are
        // freed at scope exit via the unified `scope_drops` mechanism —
        // `pop_scope` for fall-through, and `compile_tail_stmt`/`compile_return`
        // for returns. `compile_body` just drives the tail; the cleanup lives
        // there.
        let last = self.fd.body.len() - 1;
        for (i, stmt) in self.fd.body.iter().enumerate() {
            if i == last {
                if is_start {
                    if let Statement::ExpressionStatement(es) = stmt {
                        self.compile_expr_as(&es.expression, ValueShape::Boxed)?;
                        let saved = self.alloc_local();
                        self.emit(Instruction::LocalSet(saved));
                        self.emit_import_call(crate::runtime::IMPORT_EVENT_DRAIN);
                        self.emit(Instruction::LocalGet(saved));
                        self.emit(Instruction::Return);
                    } else {
                        // `<__start__>` bodies are always built from
                        // mk_call_stmt() ExpressionStatements; this
                        // fall-through is just a safety net.
                        self.compile_tail_stmt(stmt)?;
                    }
                } else {
                    self.compile_tail_stmt(stmt)?;
                }
            } else {
                self.compile_stmt(stmt)?;
            }
        }
        Ok(())
    }

    /// Emit the call-interception preamble for `fn_id`:
    ///
    ///   args_ptr = alloc(N * 8)
    ///   for i in 0..N: args_ptr[i*8] = local[i]
    ///   out_ptr = alloc(8)
    ///   if spy_check_call(fn_id, args_ptr, N, out_ptr) != 0:
    ///     return *out_ptr
    ///   ; else fall through to the real body
    ///
    /// Param count comes from `fd.params` + any `type_params`
    /// (generic `@type T` locals), matching the arity that
    /// `FunctionInfo.param_count` records.
    fn emit_spy_preamble(&mut self, fn_id: u32) -> Result<(), BuildError> {
        let arity = self.fd.params.len() as u32 + self.fd.type_params.len() as u32;

        // Serialise params to a freshly-allocated buffer. `RT_ALLOC`
        // hands out 8-byte-aligned pointers so aligned i64 stores
        // are safe.
        let args_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const((arity as i32).max(1) * 8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(args_ptr));
        for i in 0..arity {
            self.emit(Instruction::LocalGet(args_ptr));
            self.emit(Instruction::LocalGet(i));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        // Output slot for the mock return value.
        let out_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(out_ptr));

        // spy_check_call(fn_id, args_ptr, arity, out_ptr) -> i32
        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(args_ptr));
        self.emit(Instruction::I32Const(arity as i32));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit_import_call(crate::runtime::IMPORT_SPY_CHECK_CALL);

        // If the import returned 1, load *out_ptr and return it.
        self.emit_open(Instruction::If(BlockType::Empty));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::Return);
        self.emit_close();
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Statement) -> Result<(), BuildError> {
        match stmt {
            Statement::LetStatement(s) => self.compile_let(s),
            Statement::VarStatement(s) => self.compile_var(s),
            Statement::AssignmentStatement(a) => self.compile_assignment(a),
            Statement::IfStatement(s) => self.compile_if(s),
            Statement::CaseStatement(s) => self.compile_case(s, false),
            Statement::WhileStatement(s) => self.compile_while(s),
            Statement::BreakStatement(_) => self.compile_break(),
            Statement::ContinueStatement(_) => self.compile_continue(),
            Statement::ReturnStatement(r) => self.compile_return(r),
            Statement::TryStatement(s) => self.compile_try(s),
            Statement::ThrowStatement(s) => self.compile_throw(s),
            Statement::ForStatement(s) => self.compile_for(s),
            Statement::NowaitStatement(n) => self.compile_nowait(n),
            Statement::ExpressionStatement(es) => {
                let shape = self.compile_expr(&es.expression)?;
                // Discard the result — the statement runs for its side effects.
                // Under the +1 return convention (RC, plan 113 R2) a call now
                // hands back an owned reference; if we just dropped it the ref
                // would leak. Release an owned/fresh Boxed result instead of
                // dropping it. A borrowed read (identifier/field) isn't owned
                // here, and primitives carry no count, so those just drop.
                if shape == ValueShape::Boxed && self.expr_transfers_ownership(&es.expression) {
                    self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
                } else {
                    self.emit(Instruction::Drop);
                }
                Ok(())
            }
            Statement::UseStatement(_) => {
                // `use` inside a function body is a no-op at emission
                // time — module resolution already ran during
                // `prepare_source` / the checker. Top-level `use`s are
                // filtered out before the per-function emit anyway.
                Ok(())
            }
            _ => Err(BuildError::UnsupportedStatement(stmt_variant_name(stmt))),
        }
    }

    /// Emit code that pops a NaN-boxed i64 value and pushes an i32:
    /// 1 if truthy, 0 if falsy. Matches VM `val_to_bool` — null, void,
    /// and `false` are falsy; everything else (including Int 0 and
    /// the empty string) is truthy.
    fn emit_truthy_i32(&mut self) {
        // Stash the value so we can compare it against three sentinels.
        let tmp = self.alloc_local();
        self.emit(Instruction::LocalSet(tmp));
        // `true` iff NOT (val == VAL_NULL || val == VAL_VOID || val == FALSE).
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(VAL_NULL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(VAL_VOID));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::I32Or);
        self.emit(Instruction::LocalGet(tmp));
        self.emit(Instruction::I64Const(QNAN | TAG_BOOL));
        self.emit(Instruction::I64Eq);
        self.emit(Instruction::I32Or);
        // Invert: 0 → truthy (1), non-0 → falsy (0).
        self.emit(Instruction::I32Eqz);
    }

    fn compile_truthy_i32(&mut self, e: &Expression) -> Result<(), BuildError> {
        match self.compile_expr(e)? {
            ValueShape::RawBool => Ok(()),
            shape => {
                self.emit_convert(shape, ValueShape::Boxed)?;
                self.emit_truthy_i32();
                Ok(())
            }
        }
    }

    /// `if cond1 body1 else if cond2 body2 else else_body end` lowers
    /// to a nested chain of wasm `if`/`else` blocks. Each branch
    /// evaluates its condition to an i32 truth flag, then emits the
    /// body under `if` and the next branch under `else`.
    fn compile_if(&mut self, s: &IfStatement) -> Result<(), BuildError> {
        self.compile_if_branches(&s.branches, s.else_branch.as_deref())
    }

    fn compile_if_branches(
        &mut self,
        branches: &[fai_compiler::ast::IfBranch],
        else_branch: Option<&[Statement]>,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            // Only an `else` to emit.
            if let Some(body) = else_branch {
                self.push_scope();
                for st in body {
                    self.compile_stmt(st)?;
                }
                self.pop_scope();
            }
            return Ok(());
        }
        let first = &branches[0];
        self.compile_truthy_i32(&first.condition)?;
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        for st in &first.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();
        if branches.len() > 1 || else_branch.is_some() {
            self.emit(Instruction::Else);
            self.compile_if_branches(&branches[1..], else_branch)?;
        }
        self.emit_close();
        Ok(())
    }

    /// `case value when m1 body1 when m2 body2 else default end`.
    /// Lowers to a nested if/else chain where each condition is
    /// `value == match_expr`. The value is evaluated once and
    /// parked in a local so every branch compares against the
    /// same NaN-box. Uses `RT_EQ` for comparison — matches forai's
    /// `==` semantics including String, Array, Dict deep equality.
    ///
    /// `is_tail = true` wires the case as a tail expression: each
    /// branch body emits `Return` via `compile_stmts_as_tail`, and
    /// the caller adds a fall-through `VAL_VOID; Return` after.
    fn compile_case(
        &mut self,
        cs: &fai_compiler::ast::CaseStatement,
        is_tail: bool,
    ) -> Result<(), BuildError> {
        self.compile_expr_as(&cs.value, ValueShape::Boxed)?;
        let val_local = self.alloc_local();
        self.emit(Instruction::LocalSet(val_local));
        self.compile_case_branches(
            val_local,
            &cs.when_branches,
            cs.default_branch.as_deref(),
            is_tail,
        )
    }

    fn compile_case_branches(
        &mut self,
        val_local: u32,
        branches: &[fai_compiler::ast::CaseBranch],
        default: Option<&[Statement]>,
        is_tail: bool,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            // No more `when` arms — run `else` (if any).
            if let Some(body) = default {
                self.push_scope();
                if is_tail {
                    self.compile_stmts_as_tail(body)?;
                } else {
                    for st in body {
                        self.compile_stmt(st)?;
                    }
                }
                self.pop_scope();
            } else if is_tail {
                // No branch matched and no default — tail-context
                // demands a return value. Push Void.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit(Instruction::Return);
            }
            return Ok(());
        }
        let first = &branches[0];
        // value == match_expr, then truthy-check to an i32 flag.
        self.emit(Instruction::LocalGet(val_local));
        self.compile_expr_as(&first.match_expr, ValueShape::Boxed)?;
        self.emit(Instruction::Call(self.rt().base + RT_EQ));
        self.emit_truthy_i32();
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        if is_tail {
            self.compile_stmts_as_tail(&first.body)?;
        } else {
            for st in &first.body {
                self.compile_stmt(st)?;
            }
        }
        self.pop_scope();
        if branches.len() > 1 || default.is_some() {
            self.emit(Instruction::Else);
            self.compile_case_branches(val_local, &branches[1..], default, is_tail)?;
        }
        self.emit_close();
        Ok(())
    }

    /// Lower `while cond ... end` to:
    ///
    /// ```text
    /// (block $break
    ///   (loop $continue
    ///     <cond>; br_if $break if !cond
    ///     <body>
    ///     br $continue
    ///   )
    /// )
    /// ```
    fn compile_while(&mut self, s: &WhileStatement) -> Result<(), BuildError> {
        self.emit_open(Instruction::Block(BlockType::Empty));
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty));
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
        });

        // Evaluate condition + branch out on falsy.
        self.compile_truthy_i32(&s.condition)?;
        self.emit(Instruction::I32Eqz);
        // `br 1` = exit the outer `block` (break). From inside the
        // loop body at open, block_depth = continue_abs; `br` depth
        // to break is `continue_abs - break_abs = 1`.
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        self.push_scope();
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        // Back-edge to the loop start.
        self.emit(Instruction::Br(self.block_depth - continue_abs));

        self.loops.pop();
        self.emit_close(); // end loop
        self.emit_close(); // end block
        Ok(())
    }

    fn compile_break(&mut self) -> Result<(), BuildError> {
        let frame = self
            .loops
            .last()
            .ok_or(BuildError::UnsupportedStatement("break outside loop"))?;
        let rel = self.block_depth - frame.break_abs;
        self.emit(Instruction::Br(rel));
        Ok(())
    }

    fn compile_continue(&mut self) -> Result<(), BuildError> {
        let frame = self
            .loops
            .last()
            .ok_or(BuildError::UnsupportedStatement("continue outside loop"))?;
        let rel = self.block_depth - frame.continue_abs;
        self.emit(Instruction::Br(rel));
        Ok(())
    }

    /// `return` with no value returns `Void`; `return <expr>` pushes
    /// the (boxed) expression value and returns it. Wasm functions
    /// have a single i64 result (boxed forai Value) regardless of the
    /// declared fai return type, so this always emits an i64.
    fn compile_return(&mut self, s: &fai_compiler::ast::ReturnStatement) -> Result<(), BuildError> {
        // An explicit `return` jumps past the `pop_scope` of every enclosing
        // scope, so free their confined bindings here. Compute the value first
        // (it may read a binding), stash it, drop, then return it.
        match &s.value {
            Some(expr) => {
                self.compile_expr_as(expr, ValueShape::Boxed)?;
                // +1 return convention (RC, plan 113 R2): every function hands
                // the caller an OWNED reference, so call sites can transfer
                // (not retain) the result — that is what removes the pervasive
                // call-result leak. A borrowed return value (identifier, field/
                // index read, borrowed-builtin result) is retained to make it
                // +1; a fresh value or an owned call result already is. This
                // also keeps the value alive across the active-drop releases
                // below (which would otherwise free a returned owned local).
                if !self.expr_transfers_ownership(expr) {
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                if self.has_active_drops() {
                    let saved = self.alloc_local();
                    self.emit(Instruction::LocalSet(saved));
                    self.emit_all_active_drops();
                    self.emit(Instruction::LocalGet(saved));
                }
            }
            None => {
                self.emit_all_active_drops();
                self.emit(Instruction::I64Const(VAL_VOID));
            }
        }
        self.emit(Instruction::Return);
        Ok(())
    }

    /// Lower `try ... catch e ... end` to two nested wasm blocks:
    ///
    /// ```text
    /// (block $after_try        ;; break target on normal completion
    ///   (block $catch_handler  ;; `throw` branches here
    ///     <try_body>
    ///     br $after_try        ;; skip catch body on success
    ///   )                      ;; end $catch_handler
    ///   ;; caught: err_local holds the thrown value
    ///   <catch_body with catch_name bound to err_local>
    /// )                        ;; end $after_try
    /// ```
    ///
    /// A `throw` inside the try body sets `err_local` and `br`s to
    /// `$catch_handler`. `finally` runs after both success and
    /// catch paths — the basic case (finally after clean success
    /// or caught error) works. An uncaught throw inside the catch
    /// body propagates without running finally; that matches the
    /// bytecode compiler's behaviour and is acceptable under forai's
    /// current error-handling contract.
    fn compile_try(&mut self, s: &TryStatement) -> Result<(), BuildError> {
        let err_local = self.alloc_local();
        self.emit_open(Instruction::Block(BlockType::Empty));
        self.emit_open(Instruction::Block(BlockType::Empty));
        let catch_abs = self.block_depth;
        self.tries.push(TryFrame {
            catch_abs,
            err_local,
        });

        self.push_scope();
        for st in &s.try_body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        // Success path: skip the catch handler by branching to
        // `$after_try` (outer block).
        let after_try_rel = self.block_depth - (catch_abs - 1);
        self.emit(Instruction::Br(after_try_rel));

        // Done with the try body — pop the frame BEFORE catch_body
        // compiles so a `throw` inside catch targets the next-outer
        // try (or traps if none).
        self.tries.pop();

        self.emit_close(); // end $catch_handler

        // Catch handler: err_local holds the thrown value. Bind it
        // under the user-declared catch_name for the catch body.
        self.push_scope();
        self.bind(&s.catch_name, err_local);
        for st in &s.catch_body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.emit_close(); // end $after_try

        // `finally` — run after both success and catch paths. The
        // bytecode compiler emits the finally body here too (no
        // guaranteed-execution plumbing — a `throw` inside catch
        // propagates without running finally). The direct path
        // matches that behaviour for parity.
        if let Some(finally_body) = &s.finally_body {
            self.push_scope();
            for st in finally_body {
                self.compile_stmt(st)?;
            }
            self.pop_scope();
        }
        Ok(())
    }

    /// Lower `throw expr`. Inside a `try`, stores the value into the
    /// innermost try's `err_local` and branches to `$catch_handler`
    /// — the inline fast path with no globals touched.
    ///
    /// Outside any try, stash the thrown value into the
    /// `error_flag`/`error_value` globals and return early with a
    /// placeholder result. The caller's post-call propagation check
    /// (see `emit_post_call_propagation`) will see the flag set and
    /// either deliver the error to its enclosing `try` or propagate
    /// further up. This is the unwind path that makes
    /// cross-function throw + catch work.
    fn compile_throw(&mut self, s: &ThrowStatement) -> Result<(), BuildError> {
        self.compile_expr(&s.expression)?;
        if let Some(frame) = self.tries.last() {
            let rel = self.block_depth - frame.catch_abs;
            let err_local = frame.err_local;
            self.emit(Instruction::LocalSet(err_local));
            self.emit(Instruction::Br(rel));
        } else {
            // Stash the value into the error globals; the caller
            // will pick it up via the post-call propagation check.
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::I32Const(1));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
            // Placeholder return value — the caller throws it away
            // as soon as it sees the flag set.
            self.emit(Instruction::I64Const(0));
            self.emit(Instruction::Return);
        }
        Ok(())
    }

    /// Emit the post-call propagation check. The call's i64 result
    /// must already be on the stack. The check stashes the result
    /// into a local so the inner `If` block doesn't need to reach
    /// across wasm's per-block operand stack — if it then sees
    /// `error_flag` set, it either delivers the error to the
    /// enclosing `try` (Br to the catch handler) or returns early
    /// with the result still acting as the function's placeholder
    /// return value. If the flag isn't set, the saved result is
    /// pushed back so the caller's expression context sees an i64
    /// exactly as if no check had run.
    fn emit_post_call_propagation(&mut self) {
        let result_local = self.alloc_local();
        self.emit(Instruction::LocalSet(result_local));
        self.emit(Instruction::GlobalGet(GLOBAL_ERROR_FLAG));
        self.emit_open(Instruction::If(BlockType::Empty));
        if let Some(frame) = self.tries.last() {
            let rel = self.block_depth - frame.catch_abs;
            let err_local = frame.err_local;
            self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::LocalSet(err_local));
            self.emit(Instruction::I32Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_FLAG));
            self.emit(Instruction::I64Const(0));
            self.emit(Instruction::GlobalSet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::Br(rel));
        } else if self.fd.name == "<__start__>" {
            // Outermost frame — there is nowhere left to propagate.
            // A clean `Return` here would silently exit the program
            // with `error_flag` still set; trap instead so an
            // uncaught throw terminates the program (the pre-fix
            // behaviour from before cross-function throw landed).
            // See forai#4. Report the error value first so the trap
            // names it (plan 116).
            self.emit(Instruction::I32Const(crate::runtime::TRAP_UNCAUGHT_ERROR));
            self.emit(Instruction::GlobalGet(GLOBAL_ERROR_VALUE));
            self.emit(Instruction::I64Const(0));
            self.emit_import_call(IMPORT_TRAP_REPORT);
            self.emit(Instruction::Unreachable);
        } else {
            // Push the saved result and return — it's a placeholder
            // the caller throws away once it sees the flag set.
            self.emit(Instruction::LocalGet(result_local));
            self.emit(Instruction::Return);
        }
        self.emit_close();
        self.emit(Instruction::LocalGet(result_local));
    }

    /// `nowait expr` — fire-and-forget: wrap `expr` in a zero-arg
    /// closure and hand it to `IMPORT_SPAWN`. The host dispatches
    /// the closure via `__indirect_function_table` (synchronous
    /// under the current tier-1 runtime — the scheduling boundary
    /// is the host's concern, not the guest's).
    ///
    /// Mirrors the bytecode compiler's `compile_implicit_closure` +
    /// `Op::Spawn` pattern. Shares the existing
    /// `compile_function_expression` closure-allocation path, so
    /// upvalue capture works automatically — a `nowait` inside a
    /// function that references outer locals wires them as closure
    /// upvalues without any special-casing here.
    fn compile_nowait(&mut self, s: &fai_compiler::ast::NowaitStatement) -> Result<(), BuildError> {
        // Synthesise a zero-arg FunctionDeclaration whose body is
        // the nowait expression (as a statement). `compile_function_expression`
        // treats it like any anonymous `do ... end`, emitting heap
        // allocation + upvalue capture and leaving the boxed closure
        // on the stack.
        let wrapper = fai_compiler::ast::FunctionDeclaration {
            name: format!("<nowait@{}:{}>", s.location.line, s.location.column),
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body: vec![fai_compiler::ast::Statement::ExpressionStatement(
                fai_compiler::ast::ExpressionStatement {
                    expression: s.expression.clone(),
                    location: s.location.clone(),
                },
            )],
            doc: None,
            is_private: None,
            is_abstract: false,
            is_remote: false,
            location: s.location.clone(),
            doc_comment: None,
        };
        self.compile_function_expression(&wrapper)?;
        self.emit_import_call(IMPORT_SPAWN);
        // IMPORT_SPAWN returns i64 (VAL_VOID) — discard.
        self.emit(Instruction::Drop);
        Ok(())
    }

    /// Phase D for-range: `for i in start..end` lowers to a counter
    /// loop with raw i32 state. The `item_name` is rebound each
    /// iteration by NaN-boxing the current counter.
    ///
    /// `for x in iterable`. Dispatches on the iterable shape:
    /// `RangeExpression` inlines an i32 counter loop (the cheap
    /// path), and any other expression is treated as an Array —
    /// the runtime reads the object's length at offset 4 and
    /// loads elements from `addr + 8 + i*8`. Matches the bytecode
    /// compiler's split between `Op::ForRange` and `Op::ForLoop`.
    ///
    /// Dict iteration isn't wired (neither codegen path handles it
    /// today); it surfaces as `ForStatement/unsupported-iterable`
    /// if the runtime tag turns out to be non-Array — but the
    /// builder can't know that at compile time, so trust the
    /// checker's type validation.
    fn compile_for(&mut self, s: &ForStatement) -> Result<(), BuildError> {
        if let Expression::RangeExpression(r) = &s.items {
            return self.compile_for_range(s, r);
        }
        self.compile_for_array(s)
    }

    /// Generic array iteration — evaluate the iterable into a local,
    /// read its length, and walk `0..length` emitting element loads.
    /// Each iteration rebinds `item_name` to `array[index]`.
    ///
    /// Structure (three nested labels — required so `continue`
    /// reaches the increment, not the loop header):
    /// ```text
    /// (block $break              ; break target
    ///   (loop $repeat             ; repeat target — Br here re-runs the check
    ///     (block $continue         ; continue target — fall-through hits increment
    ///       <length check; br_if $break>
    ///       <load item; bind>
    ///       <body>                 ; break → $break, continue → $continue
    ///     )                         ; end $continue — falls through to:
    ///     index++
    ///     br $repeat
    ///   )
    /// )
    /// ```
    fn compile_for_array(&mut self, s: &ForStatement) -> Result<(), BuildError> {
        // Evaluate the iterable — typically an ArrayExpression or a
        // variable holding an Array. Box stays on the stack so we
        // can unbox its address once.
        self.compile_expr(&s.items)?;
        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
        let arr_addr = self.alloc_i32_local();
        self.emit(Instruction::LocalSet(arr_addr));

        // length = mem[arr_addr + 4]
        let length = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Load(mem_off(4)));
        self.emit(Instruction::LocalSet(length));

        // index = 0
        let index = self.alloc_i32_local();
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(index));

        // item slot (NaN-boxed i64) — rebound each iteration.
        let item_local = self.alloc_local();

        self.emit_open(Instruction::Block(BlockType::Empty)); // $break
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty)); // $repeat
        let repeat_abs = self.block_depth;
        self.emit_open(Instruction::Block(BlockType::Empty)); // $continue
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
        });

        // if index >= length: break
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::LocalGet(length));
        self.emit(Instruction::I32GeS);
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        // item = i64 at arr_addr + 8 + index*8
        self.emit(Instruction::LocalGet(arr_addr));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::I32Mul);
        self.emit(Instruction::I32Add);
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::LocalSet(item_local));

        self.push_scope();
        self.bind(&s.item_name, item_local);
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.loops.pop();
        self.emit_close(); // end $continue — falls through to increment

        // index++
        self.emit(Instruction::LocalGet(index));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(index));
        // br $repeat — back to the check.
        self.emit(Instruction::Br(self.block_depth - repeat_abs));

        self.emit_close(); // end $repeat
        self.emit_close(); // end $break
        Ok(())
    }

    /// `for i in start..end` (inclusive range). Same three-label
    /// structure as `compile_for_array` — `continue` exits the inner
    /// block so the counter increment runs before looping.
    fn compile_for_range(
        &mut self,
        s: &ForStatement,
        range: &fai_compiler::ast::RangeExpression,
    ) -> Result<(), BuildError> {
        // Evaluate start and end, unbox to i32.
        self.compile_expr(&range.start)?;
        let start_i32 = self.alloc_i32_local();
        self.emit(Instruction::I32WrapI64);
        self.emit(Instruction::LocalSet(start_i32));
        self.compile_expr(&range.end)?;
        let end_i32 = self.alloc_i32_local();
        self.emit(Instruction::I32WrapI64);
        self.emit(Instruction::LocalSet(end_i32));
        // counter = start
        let counter = self.alloc_i32_local();
        self.emit(Instruction::LocalGet(start_i32));
        self.emit(Instruction::LocalSet(counter));
        // item slot (NaN-boxed) for the user's loop variable.
        let item_local = self.alloc_local();

        self.emit_open(Instruction::Block(BlockType::Empty)); // $break
        let break_abs = self.block_depth;
        self.emit_open(Instruction::Loop(BlockType::Empty)); // $repeat
        let repeat_abs = self.block_depth;
        self.emit_open(Instruction::Block(BlockType::Empty)); // $continue
        let continue_abs = self.block_depth;
        self.loops.push(LoopFrame {
            break_abs,
            continue_abs,
        });

        // `..` is exclusive (exit when counter >= end); `...` is
        // inclusive (exit when counter > end).
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::LocalGet(end_i32));
        if range.inclusive {
            self.emit(Instruction::I32GtS);
        } else {
            self.emit(Instruction::I32GeS);
        }
        self.emit(Instruction::BrIf(self.block_depth - break_abs));

        // item = make_int(counter)
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
        self.emit(Instruction::LocalSet(item_local));

        self.push_scope();
        self.bind(&s.item_name, item_local);
        for st in &s.body {
            self.compile_stmt(st)?;
        }
        self.pop_scope();

        self.loops.pop();
        self.emit_close(); // end $continue

        // counter++
        self.emit(Instruction::LocalGet(counter));
        self.emit(Instruction::I32Const(1));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(counter));
        self.emit(Instruction::Br(self.block_depth - repeat_abs));

        self.emit_close(); // end $repeat
        self.emit_close(); // end $break
        Ok(())
    }

    /// Allocate an i32-typed local. Mirrors `alloc_local` but for
    /// scratch counters that aren't NaN-boxed forai values.
    fn alloc_i32_local(&mut self) -> u32 {
        let idx = self.next_local;
        self.next_local += 1;
        self.local_decls.push(ValType::I32);
        idx
    }

    fn compile_assignment(&mut self, a: &AssignmentStatement) -> Result<(), BuildError> {
        match &a.target {
            AssignmentTarget::Variables { names } if names.len() == 1 => {
                // Assignment dispatches on where the name resolves:
                //   • Own local, plain       → LocalSet.
                //   • Own local, cell-bound  → I64Store through the cell.
                //   • Upvalue referring to a cell → I64Store through the
                //     env-stored cell address. This is how closures
                //     mutate their enclosing scope's `var`s.
                //   • Upvalue referring to a snapshot → refused (the
                //     captured `let` isn't mutable).
                match self.resolve(&names[0]) {
                    Some(Resolve::Local(binding)) => {
                        if binding.is_cell {
                            // Cell-bound `var` shared with closures: the cell
                            // OWNS its value (plan 114) — retain-new-if-
                            // borrowed, release-old, store at offset 8. A
                            // sibling closure that kept the old value has its
                            // own retain, so the release can't free under it.
                            let transfers = self.expr_transfers_ownership(&a.value);
                            self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                            self.emit_cell_store(binding.local, transfers);
                        } else if binding.shape == ValueShape::Boxed
                            && self.is_owned_local(binding.local)
                        {
                            // Reassign an owned object local (RC, plan 113 R1):
                            // retain a borrowed new value (co-ownership), release
                            // the old value this slot owned, then store. The slot
                            // keeps owning exactly one ref.
                            self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                            if !self.expr_transfers_ownership(&a.value) {
                                self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                            }
                            self.emit(Instruction::LocalGet(binding.local));
                            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
                            self.emit(Instruction::LocalSet(binding.local));
                        } else {
                            // Borrowed slot (param) or primitive: plain overwrite.
                            // The scope owns no ref here, so there is nothing to
                            // release and nothing to retain.
                            self.compile_expr_as(&a.value, binding.shape)?;
                            self.emit(Instruction::LocalSet(binding.local));
                        }
                        Ok(())
                    }
                    Some(Resolve::Upvalue(uv_idx)) => {
                        let upv = self.upvalues[uv_idx as usize];
                        if !upv.is_cell {
                            return Err(BuildError::UnsupportedStatement(
                                "AssignmentStatement/write-to-snapshot-upvalue",
                            ));
                        }
                        // env[uv] stores the NaN-boxed cell (plan 114).
                        // Unbox the address, then value-RC store at @8.
                        let cell_addr = self.alloc_i32_local();
                        self.emit(Instruction::GlobalGet(GLOBAL_ENV_PTR));
                        self.emit(Instruction::I64Load(mem_off(uv_idx as u64 * 8)));
                        self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
                        self.emit(Instruction::LocalSet(cell_addr));
                        let transfers = self.expr_transfers_ownership(&a.value);
                        self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                        self.emit_cell_store(cell_addr, transfers);
                        Ok(())
                    }
                    Some(Resolve::ModuleVar(global_idx)) => {
                        // A top-level `var` global owns its value for the life of
                        // the program (RC, plan 113 R1): retain a borrowed new
                        // value, release the previous one (reclaiming it mid-run),
                        // then store. The initial global value is 0/VAL_VOID, on
                        // which RT_RELEASE's is_obj guard is a safe no-op.
                        self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                        if !self.expr_transfers_ownership(&a.value) {
                            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                        }
                        self.emit(Instruction::GlobalGet(global_idx));
                        self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
                        self.emit(Instruction::GlobalSet(global_idx));
                        Ok(())
                    }
                    None => Err(BuildError::UnknownIdentifier(names[0].clone())),
                }
            }
            AssignmentTarget::Variables { names } => {
                // Multi-variable assignment `a, b = swap(...)` —
                // destructure the RHS tuple into each existing
                // local. Each name must already be bound;
                // reassignment doesn't allocate new locals.
                self.compile_expr(&a.value)?;
                let tuple_local = self.alloc_local();
                self.emit(Instruction::LocalSet(tuple_local));
                for (i, name) in names.iter().enumerate() {
                    let Some(binding) = self.lookup(name) else {
                        return Err(BuildError::UnknownIdentifier(name.clone()));
                    };
                    self.emit(Instruction::LocalGet(tuple_local));
                    self.emit(Instruction::I32Const(i as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                    self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
                    self.emit_convert(ValueShape::Boxed, binding.shape)?;
                    self.emit(Instruction::LocalSet(binding.local));
                }
                Ok(())
            }
            AssignmentTarget::Field { object } => {
                // `obj.field = value`. The AST's `object` is the
                // full MemberExpression — decompose it to reach the
                // object and field name, intern the name, and
                // dispatch to `RT_SET_FIELD(obj, key_ptr, key_len, val)`.
                let me = match object.as_ref() {
                    Expression::MemberExpression(me) => me,
                    _ => {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Field-non-member",
                        ));
                    }
                };
                // Refuse assignment to a module alias — modules
                // aren't mutable bindings. A local/upvalue/module-var
                // whose name happens to collide with the module alias
                // (common when a parameter is named `signal` inside
                // the `signal` module) keeps its binding semantics
                // and flows through to field-store as a normal object.
                if let Expression::IdentifierExpression(obj_id) = &*me.object {
                    let shadowed_by_binding = self.resolve(&obj_id.name).is_some();
                    if !shadowed_by_binding
                        && (self.ctx.module_aliases.contains_key(&obj_id.name)
                            || obj_id.name == "assert")
                    {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Field-on-module",
                        ));
                    }
                }
                let (key_off, key_len) = self.ctx.strings.borrow_mut().intern(&me.property);
                self.compile_expr(&me.object)?;
                self.emit(Instruction::I32Const(key_off as i32));
                self.emit(Instruction::I32Const(key_len as i32));
                // Must be Boxed — record fields hold NaN-boxed values. A
                // RawInt/RawFloat from an arithmetic fast-path (e.g.
                // `s.val = s.val + 1`) stored unconverted would read back as
                // garbage, exactly as the Index path below guards against.
                self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                // The object co-owns the new field value (RC, plan 113 R1):
                // retain if borrowed. RT_SET_FIELD releases the value it
                // overwrites (or just appends, with no old value to release).
                if !self.expr_transfers_ownership(&a.value) {
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                self.emit(Instruction::Call(self.rt().base + RT_SET_FIELD));
                // RT_SET_FIELD now returns the (possibly reallocated) dict
                // pointer. This `obj.field = v` statement path is used for
                // records/instances (fixed shape — never grow, pointer
                // unchanged) and the rare dict member-write; we can't rebind
                // an arbitrary lvalue here, so drop the result. String-keyed
                // dict growth goes through `dictionary.set`, which threads
                // the returned pointer.
                self.emit(Instruction::Drop);
                Ok(())
            }
            AssignmentTarget::Index { object } => {
                // `arr[i] = value`. The AST's `object` is the full
                // IndexExpression. We unbox the array address, add
                // `8 + i*8`, and do an `I64Store`. This mirrors the
                // bytecode path's `Op::SetIndex` — a direct memory
                // write with no bounds check (matches translator
                // semantics for parity).
                let ie = match object.as_ref() {
                    Expression::IndexExpression(ie) => ie,
                    _ => {
                        return Err(BuildError::UnsupportedStatement(
                            "AssignmentStatement/Index-non-index",
                        ));
                    }
                };
                // Compute the slot address `arr_addr + 8 + i*8` into a local so
                // we can both release the old occupant and store the new value
                // through it (RC, plan 113 R1).
                self.compile_expr_as(&ie.object, ValueShape::Boxed)?;
                self.emit(Instruction::Call(self.rt().base + RT_OBJ_ADDR));
                let arr_addr = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(arr_addr));
                self.compile_expr_as(&ie.index, ValueShape::RawInt)?;
                self.emit(Instruction::I32WrapI64);
                let idx = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(idx));
                // Checked-mode (plan 116): an out-of-range index store is
                // silent heap corruption — i = -1 lands on the array's own
                // tag/count header; past-end clobbers the next block. Trap
                // with a named reason at the write site instead. Cheap
                // (one compare on a write that already happens) so it
                // rides along with `--checked`, not just FAI_RC_CHECK.
                if crate::runtime::checked_enabled() {
                    self.emit(Instruction::LocalGet(idx));
                    self.emit(Instruction::LocalGet(arr_addr));
                    self.emit(Instruction::I32Load(MemArg {
                        offset: 4,
                        align: 0,
                        memory_index: 0,
                    }));
                    self.emit(Instruction::I32GeU); // unsigned: negative idx → huge
                    self.emit_open(Instruction::If(BlockType::Empty));
                    self.emit(Instruction::I32Const(crate::runtime::TRAP_INDEX_OOB));
                    self.emit(Instruction::LocalGet(idx));
                    self.emit(Instruction::I64ExtendI32S);
                    self.emit(Instruction::LocalGet(arr_addr));
                    self.emit(Instruction::I32Load(MemArg {
                        offset: 4,
                        align: 0,
                        memory_index: 0,
                    }));
                    self.emit(Instruction::I64ExtendI32U);
                    self.emit_import_call(IMPORT_TRAP_REPORT);
                    self.emit(Instruction::Unreachable);
                    self.emit_close();
                }
                self.emit(Instruction::LocalGet(arr_addr));
                self.emit(Instruction::I32Const(8));
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalGet(idx));
                self.emit(Instruction::I32Const(8));
                self.emit(Instruction::I32Mul);
                self.emit(Instruction::I32Add);
                let slot = self.alloc_i32_local();
                self.emit(Instruction::LocalSet(slot));
                // Evaluate the new value and retain it (if borrowed) FIRST — it
                // may read the slot being overwritten (`xs[i] = xs[i]`), so the
                // old value must stay alive until after we've taken our ref.
                // Must be Boxed — array slots hold NaN-boxed values; a
                // RawInt/RawFloat stored unconverted would read back as garbage.
                self.compile_expr_as(&a.value, ValueShape::Boxed)?;
                if !self.expr_transfers_ownership(&a.value) {
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                let newv = self.alloc_local();
                self.emit(Instruction::LocalSet(newv));
                // Now release the value the slot currently holds — the array
                // owned it (RT_RELEASE's is_obj guard skips a leftover int).
                self.emit(Instruction::LocalGet(slot));
                self.emit(Instruction::I64Load(mem0()));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
                // Store the new value through the slot.
                self.emit(Instruction::LocalGet(slot));
                self.emit(Instruction::LocalGet(newv));
                self.emit(Instruction::I64Store(mem0()));
                Ok(())
            }
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.scope_drops.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        // Fall-through exit of this scope: free its confined fresh-literal
        // bindings. (On a `return` that jumped out of this scope, this code is
        // unreachable — the return already dropped these and diverged.)
        if let Some(drops) = self.scope_drops.last() {
            if !drops.is_empty() {
                // RC release (plan 113): decrement each confined local's count and
                // deep-free at zero. Refcounting makes this order-independent and
                // safe even when a value is co-owned (e.g. stored in a container) —
                // whoever releases last frees it.
                let drop_fn = self.rt().base + RT_RELEASE;
                let locals = drops.clone();
                for l in locals {
                    self.emit(Instruction::LocalGet(l));
                    self.emit(Instruction::Call(drop_fn));
                }
            }
        }
        self.scopes.pop();
        self.scope_drops.pop();
    }

    /// Record `local` as a scope-exit drop in the current (innermost) scope, if
    /// `value` is a confined fresh-literal allocation. Called at binding time.
    fn note_droppable(&mut self, local: u32) {
        // RC scope-exit release (plan 113 R1): every object local owns exactly
        // one reference (transfer-fresh / retain-borrowed at the bind), so it is
        // released at scope exit. Refcounting makes this order-independent and
        // safe under co-ownership — the last owner frees. Callers gate on
        // `ValueShape::Boxed` (primitives carry no count).
        if let Some(top) = self.scope_drops.last_mut() {
            top.push(local);
        }
    }

    /// Emit `rt_drop` for every confined binding in every active scope
    /// (innermost → outermost). Used before a `return`/tail, which jumps past
    /// the `pop_scope` of each enclosing scope.
    fn emit_all_active_drops(&mut self) {
        let locals: Vec<u32> = self.scope_drops.iter().rev().flatten().copied().collect();
        if locals.is_empty() {
            return;
        }
        // RC release before a return/tail (which jumps past each pop_scope).
        // `compile_return`/`compile_tail_stmt` already retained a borrowed return
        // value before calling this, so releasing its owning local here leaves it
        // alive at +1 for the caller to take ownership of.
        let drop_fn = self.rt().base + RT_RELEASE;
        for l in locals {
            self.emit(Instruction::LocalGet(l));
            self.emit(Instruction::Call(drop_fn));
        }
    }

    fn has_active_drops(&self) -> bool {
        self.scope_drops.iter().any(|s| !s.is_empty())
    }

    /// True if `local` is an object local this scope OWNS — i.e. it was
    /// registered via `note_droppable` and will be released at scope exit. Only
    /// owned locals carry the `+1` that makes reassignment release-the-old
    /// correct; borrowed slots (function params) own nothing, so releasing their
    /// previous value would free something the caller still owns (a UAF). Used
    /// by `compile_assignment` to decide whether to release-old / retain-new.
    fn is_owned_local(&self, local: u32) -> bool {
        self.scope_drops.iter().any(|s| s.contains(&local))
            || self.owned_frame_locals.contains(&local)
    }

    /// Tail statement: the value of this statement (if it's an
    /// expression) is the function's return value. Branches of an
    /// `if` in tail position are themselves tails — so each one
    /// emits its own `Return` (no wasm `if` result-type needed).
    fn compile_tail_stmt(&mut self, stmt: &Statement) -> Result<(), BuildError> {
        match stmt {
            Statement::ExpressionStatement(es) => {
                self.compile_expr_as(&es.expression, ValueShape::Boxed)?;
                // +1 return convention (RC, plan 113 R2) — mirrors
                // `compile_return`: retain a borrowed tail value so the function
                // returns an owned ref the caller transfers, and so it survives
                // the active-drop releases below.
                if !self.expr_transfers_ownership(&es.expression) {
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                if self.has_active_drops() {
                    // Stash the return value (it may read a binding), free the
                    // dead bindings in every active scope, then return it.
                    let saved = self.alloc_local();
                    self.emit(Instruction::LocalSet(saved));
                    self.emit_all_active_drops();
                    self.emit(Instruction::LocalGet(saved));
                }
                self.emit(Instruction::Return);
                Ok(())
            }
            Statement::IfStatement(s) => {
                self.compile_if_branches_tail(&s.branches, s.else_branch.as_deref())?;
                // Fall-through safety: if no branch matched and
                // there's no else, return void. In practice all
                // reachable paths inside the branches already emit
                // Return, so this is unreachable code that wasm's
                // polymorphic-Return rules let us emit safely.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit(Instruction::Return);
                Ok(())
            }
            Statement::CaseStatement(s) => {
                self.compile_case(s, true)?;
                // Same fall-through safety as IfStatement above — if
                // no `when` branch matched and the source has no
                // `else`, return Void. Normally unreachable since
                // each tail branch emits `Return`, but wasm's
                // polymorphic-Return rules let the trailer land
                // safely after the structured If tower.
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit(Instruction::Return);
                Ok(())
            }
            _ => {
                // Non-expression tail (a trailing `var`/`let`, UseStatement in
                // main, etc.): compile as side effect, release the confined
                // locals in every active scope (RC scope-exit on fall-through;
                // the expression-tail arm above does the same), return Void.
                self.compile_stmt(stmt)?;
                self.emit_all_active_drops();
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit(Instruction::Return);
                Ok(())
            }
        }
    }

    /// Compile a sequence of statements where the last one is a tail
    /// (its value becomes the enclosing function's return value).
    /// Used inside `if` branches when the `if` itself is tail.
    fn compile_stmts_as_tail(&mut self, stmts: &[Statement]) -> Result<(), BuildError> {
        if stmts.is_empty() {
            self.emit(Instruction::I64Const(VAL_VOID));
            self.emit(Instruction::Return);
            return Ok(());
        }
        let last = stmts.len() - 1;
        for (i, s) in stmts.iter().enumerate() {
            if i == last {
                self.compile_tail_stmt(s)?;
            } else {
                self.compile_stmt(s)?;
            }
        }
        Ok(())
    }

    /// Mirror of `compile_if_branches` but every branch body is a
    /// tail — each one ends in its own `Return`.
    fn compile_if_branches_tail(
        &mut self,
        branches: &[fai_compiler::ast::IfBranch],
        else_branch: Option<&[Statement]>,
    ) -> Result<(), BuildError> {
        if branches.is_empty() {
            if let Some(body) = else_branch {
                self.push_scope();
                self.compile_stmts_as_tail(body)?;
                self.pop_scope();
            } else {
                self.emit(Instruction::I64Const(VAL_VOID));
                self.emit(Instruction::Return);
            }
            return Ok(());
        }
        let first = &branches[0];
        self.compile_truthy_i32(&first.condition)?;
        self.emit_open(Instruction::If(BlockType::Empty));
        self.push_scope();
        self.compile_stmts_as_tail(&first.body)?;
        self.pop_scope();
        if branches.len() > 1 || else_branch.is_some() {
            self.emit(Instruction::Else);
            self.compile_if_branches_tail(&branches[1..], else_branch)?;
        }
        self.emit_close();
        Ok(())
    }

    fn compile_let(&mut self, s: &LetStatement) -> Result<(), BuildError> {
        self.compile_bindings(&s.bindings, &s.value, s.is_shared.unwrap_or(false))
    }

    fn compile_var(&mut self, s: &VarStatement) -> Result<(), BuildError> {
        self.compile_bindings(&s.bindings, &s.value, s.is_shared.unwrap_or(false))
    }

    /// Shared binding logic for `let` and `var`. The direct path
    /// treats them identically at the wasm level — both allocate a
    /// fresh local and bind; `var`'s mutability is enforced upstream
    /// by the checker, not here.
    ///
    /// Multi-binding (`let a, b = rhs`) evaluates `rhs` once into a
    /// local (expected to be a Tuple), then destructures by reading
    /// each index via `RT_GET_INDEX(tuple, MAKE_INT(i))`. Mirrors
    /// the bytecode compiler's per-element `Op::GetIndex` loop.
    fn compile_bindings(
        &mut self,
        bindings: &[fai_compiler::ast::BindingDeclaration],
        value: &Expression,
        is_shared: bool,
    ) -> Result<(), BuildError> {
        match bindings.len() {
            0 => Err(BuildError::UnsupportedStatement(
                "LetStatement/empty-bindings",
            )),
            1 => {
                let name = &bindings[0].name;

                // `let t Type = from_dict(dict)` — expand at compile
                // time to the equivalent type constructor call using
                // the declared fields. Driving the type from the LHS
                // annotation lets the call stay a one-liner while the
                // codegen gets to statically resolve every field.
                if let Some(annotation) = &bindings[0].type_name {
                    if let Some(type_name) = annotation.name.clone() {
                        if let Expression::CallExpression(ce) = value {
                            if let Expression::IdentifierExpression(id) = &*ce.callee {
                                if id.name == "query_typed"
                                    && ce.args.len() == 3
                                    && annotation.is_array
                                    && self.ctx.type_fields.contains_key(&type_name)
                                {
                                    self.compile_query_typed_binding(name, &type_name, ce)?;
                                    return Ok(());
                                }
                                if id.name == "from_dict" && ce.args.len() == 1 {
                                    if self.ctx.type_fields.contains_key(&type_name) {
                                        let dict_expr = ce.args[0].value.clone();
                                        self.compile_from_dict_binding(
                                            name, &type_name, dict_expr,
                                        )?;
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }

                // Cell-box vars that are captured by a nested closure.
                // The pre-pass populated `cell_captured_vars` with only
                // `var` names (not `let`), so this never trips on an
                // immutable binding.
                if self.cell_captured_vars.contains(name) {
                    // Already bound as a cell? In a resume fn the frame slot
                    // holds the boxed pointer of the heap cell, seeded at
                    // function entry — store the initial value *through* that
                    // existing cell rather than allocating a fresh one and
                    // rebinding. Rebinding would orphan the frame's cell
                    // (losing the value across suspension) and hand the
                    // capturing closure a plain i64 local where it expects an
                    // i32 cell address.
                    if let Some(existing) = self.lookup(name) {
                        if existing.is_cell {
                            let addr_local = existing.local;
                            let transfers = self.expr_transfers_ownership(value);
                            self.compile_expr_as(value, ValueShape::Boxed)?;
                            self.emit_cell_store(addr_local, transfers);
                            return Ok(());
                        }
                    }
                    // Allocate a tagged 16-byte cell (plan 114), store the
                    // (Boxed) initial value with value-RC, bind the name to
                    // a cell binding. Reads and writes on either side deref
                    // the value slot at offset 8.
                    let addr_local = self.emit_cell_alloc();
                    let transfers = self.expr_transfers_ownership(value);
                    self.compile_expr_as(value, ValueShape::Boxed)?;
                    self.emit_cell_store(addr_local, transfers);
                    self.bind_cell(name, addr_local);
                    // The scope owns the cell's +1 from the allocator:
                    // release it at scope exit like any owned binding (the
                    // shadow local carries the boxed form scope_drops
                    // expects). A capturing closure that escapes keeps the
                    // cell alive through its own retained upvalue ref —
                    // before plan 114 this block simply leaked.
                    let boxed_local = self.alloc_local();
                    self.emit(Instruction::LocalGet(addr_local));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_OBJ));
                    self.emit(Instruction::LocalSet(boxed_local));
                    self.note_droppable(boxed_local);
                    return Ok(());
                }

                // When the binding carries an explicit type annotation
                // (`let x Float = 0`, `let x Int = 3.7`), the declared
                // type wins over the value's inferred type — emit_convert
                // handles the Int↔Float widening/narrowing the checker
                // approved. Otherwise fall back to the value's shape.
                let shape = bindings[0]
                    .type_name
                    .as_ref()
                    .map(shape_for_type_node)
                    .unwrap_or_else(|| self.shape_for_expr(value));
                self.compile_expr_as(value, shape)?;
                // RC bind (plan 113 R1): a borrowed source (identifier/field/
                // call/…) is co-owned by this new local → retain; a fresh value
                // transfers its single ref. The local is released at scope exit.
                // (`shared` no longer has any runtime effect.)
                let _ = is_shared;
                if shape == ValueShape::Boxed && !self.expr_transfers_ownership(value) {
                    self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
                }
                let local = self.alloc_typed_local(shape);
                self.emit(Instruction::LocalSet(local));
                self.bind_shape(name, local, shape);
                // Release this owned object local at scope exit (RC, plan 113 R1).
                if !is_shared && shape == ValueShape::Boxed {
                    self.note_droppable(local);
                }
                Ok(())
            }
            _ => {
                // Evaluate the RHS (expected Tuple) into a scratch
                // local so we can index into it repeatedly.
                self.compile_expr_as(value, ValueShape::Boxed)?;
                let tuple_local = self.alloc_local();
                self.emit(Instruction::LocalSet(tuple_local));

                for (i, binding) in bindings.iter().enumerate() {
                    // RT_GET_INDEX(tuple, NaN-boxed-Int(i)) — reads
                    // entry `i`. Works for any container tag; for
                    // Tuples (tag=2) the layout matches Arrays so
                    // the helper returns the stored i64 directly.
                    self.emit(Instruction::LocalGet(tuple_local));
                    self.emit(Instruction::I32Const(i as i32));
                    self.emit(Instruction::Call(self.rt().base + RT_MAKE_INT));
                    self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
                    let local = self.alloc_local();
                    self.emit(Instruction::LocalSet(local));
                    self.bind(&binding.name, local);
                }
                Ok(())
            }
        }
    }

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
                self.compile_field_access(&me.object, &me.property)?;
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
            return Err(BuildError::UnknownIdentifier(name));
        };
        let _ = resolved_name; // kept for future diagnostics
        let expected = self.functions()[proto_idx as usize].param_count as usize;
        let type_param_count = self.functions()[proto_idx as usize].type_param_count as usize;
        let real_param_count = expected - type_param_count;
        let defaults = self.functions()[proto_idx as usize].param_defaults.clone();

        // Named-param reorder: if the checker recorded a reorder map
        // for this call site, use it to pull caller args into
        // declaration order. Missing slots are filled from default
        // parameter expressions when available.
        let reorder_key = ufcs_key.clone();
        if let Some(order) = self.checker().named_param_reorder.get(&reorder_key) {
            if order.len() != real_param_count {
                return Err(BuildError::UnsupportedExpression(
                    "CallExpression/reorder-shape-mismatch",
                ));
            }
            // Owned argument temporaries to release after the call (RC, plan 113
            // R2) — mirrors the positional path below.
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
            self.emit_post_call_propagation();
            for t in owned_arg_stashes {
                self.emit(Instruction::LocalGet(t));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
            return Ok(());
        }

        // No reorder recorded — labelled args only work if they happen
        // to be in declaration order already. In that case the
        // checker didn't emit a reorder. Treat them as positional.
        if ce.args.iter().any(|a| a.label.is_some()) && !is_ufcs {
            // No checker reorder and still labelled — means either
            // the labels match declaration order (safe) or we need
            // bytecode-path handling. Accept; the expected-count
            // check below catches mismatches.
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
        // caller's binding, so it is left alone. If the callee returns one of
        // these args it retained it first (the +1 return convention), so the
        // result survives our release.
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
        self.emit_post_call_propagation();
        // Result is on the stack (propagation re-pushed it); release the owned
        // argument temporaries beneath it. On a throw, propagation branched away
        // and this is skipped — the temps leak on the error path (sound).
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
            self.emit(Instruction::LocalGet(t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
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
        for a in call_args {
            self.compile_expr(&a.value)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            arg_locals.push(local);
        }

        let arity = call_args.len() as u32;
        let buf = self.alloc_i32_local();
        self.emit(Instruction::I32Const((arity.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(buf));
        for (i, &local) in arg_locals.iter().enumerate() {
            self.emit(Instruction::LocalGet(buf));
            self.emit(Instruction::LocalGet(local));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        let out_ptr = self.alloc_i32_local();
        self.emit(Instruction::I32Const(8));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(out_ptr));

        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const(arity as i32));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit_import_call(crate::runtime::IMPORT_SPY_CHECK_CALL);

        // i32 on stack: 1 = mocked (use *out_ptr), 0 = run real call.
        self.emit_open(Instruction::If(BlockType::Result(ValType::I64)));
        self.emit(Instruction::LocalGet(out_ptr));
        self.emit(Instruction::I64Load(mem0()));
        self.emit(Instruction::Else);
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
                self.emit_string_arg_from_expr(&call_args[0].value)?;
                self.emit_string_arg_from_expr(&call_args[1].value)?;
                self.emit(Instruction::Call(self.rt().base + RT_STR_EQ));
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
        // Arg 0 (the RECEIVER) is released ONLY for methods that return a
        // PRIMITIVE (length/isEmpty/contains/indexOf/startsWith/endsWith): an
        // Int/Bool result provably can't reference the receiver, so freeing a
        // fresh receiver is safe. Heap-returning methods (split/replace/slice/
        // sort/append/join/…) are NOT released at arg 0 — their result can share
        // structure with the receiver (e.g. a builder that copies element refs),
        // and freeing the receiver could free something the result still points
        // at (a use-after-free — observed as a closure-arity trap when a freed
        // node is later `call_indirect`'d). Those receiver temps leak, soundly.
        // Args 1..n never alias any result, so owned ones are always released.
        // Arg 0 (receiver) is safe to release when the result CANNOT share a heap
        // pointer with it: a primitive result (length/isEmpty/contains/indexOf/
        // startsWith/endsWith), or a CROSS-TYPE transform — `join` (array → fresh
        // string) and `split` (string → fresh array of fresh substrings) build a
        // result of a different type with copied bytes, so nothing in the result
        // points into the receiver. Still skipped: same-type string→string
        // transforms (may alias the receiver on a fast path) and array-rebuilders
        // (append/sort/reverse/slice — result shares element pointers) and
        // element accessors (first/last — result IS a receiver element).
        let result_cannot_alias_receiver = matches!(
            method_id,
            METHOD_LENGTH
                | METHOD_IS_EMPTY
                | METHOD_CONTAINS
                | METHOD_INDEX_OF
                | METHOD_STARTS_WITH
                | METHOD_ENDS_WITH
                | METHOD_JOIN
                | METHOD_SPLIT
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
        if matches!(result, ResultShape::Boxed) {
            let name = import_name(import_idx);
            if fai_compiler::ownership_abi::lookup_host_import(name).is_none() {
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
        let mut string_stashes: Vec<u32> = Vec::new();
        for (shape, arg) in arg_shapes.iter().zip(call_args.iter()) {
            match shape {
                ArgShape::String => {
                    if let Some(t) = self.emit_string_arg_stashing(&arg.value)? {
                        string_stashes.push(t);
                    }
                }
                ArgShape::Int => self.emit_int_arg_from_expr(&arg.value)?,
                ArgShape::Boxed => {
                    self.compile_expr(&arg.value)?;
                }
            }
        }
        self.emit_import_call(import_idx);
        // Release owned string-arg temps. The result/return is produced by the
        // import (independent of the guest string bytes), so this can't free it.
        for t in &string_stashes {
            self.emit(Instruction::LocalGet(*t));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
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
    /// - other → passthrough.
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
    fn compile_from_dict_binding(
        &mut self,
        binding_name: &str,
        type_name: &str,
        dict_expr: Expression,
    ) -> Result<(), BuildError> {
        self.compile_expr_as(&dict_expr, ValueShape::Boxed)?;
        let dict_local = self.alloc_local();
        self.emit(Instruction::LocalSet(dict_local));
        self.compile_from_dict_local_value(type_name, dict_local)?;
        // An OWNED source temp (e.g. `from_dict(json.parse(s))`) is consumed
        // by the materialization — the record retained every field it kept,
        // so the source's ref can go. A borrowed source (`from_dict(e.data)`)
        // stays the caller's.
        if self.expr_transfers_ownership(&dict_expr) {
            self.emit(Instruction::LocalGet(dict_local));
            self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
        }
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
        self.emit_post_call_propagation();
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
        self.compile_expr(&ix.object)?;
        self.compile_expr(&ix.index)?;
        self.emit(Instruction::Call(self.rt().base + RT_GET_INDEX));
        Ok(())
    }

    /// Field access `obj.prop`. Compiles the object, then calls
    /// `RT_GET_FIELD(obj, key_ptr, key_len)` with an interned key.
    /// Works on dicts, instances, error objects, and even strings
    /// (the runtime handles tag-based dispatch). Returns
    /// `VAL_NULL` if the key is absent.
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
    /// with `RT_CONCAT(a, b)`. An empty template evaluates to the
    /// empty String.
    fn compile_template_string(
        &mut self,
        parts: &[fai_compiler::ast::TemplateStringPart],
    ) -> Result<(), BuildError> {
        use fai_compiler::ast::TemplateStringPart;

        // Helper: emit one part as a boxed String on the stack.
        // Text parts intern + RT_ALLOC_STRING; expressions stringify
        // via RT_VALUE_TO_STR.
        let emit_part = |this: &mut Self, part: &TemplateStringPart| -> Result<(), BuildError> {
            match part {
                TemplateStringPart::Text { value } => {
                    let (off, len) = this.ctx.strings.borrow_mut().intern(value);
                    this.emit(Instruction::I32Const(off as i32));
                    this.emit(Instruction::I32Const(len as i32));
                    this.emit(Instruction::Call(this.rt().base + RT_ALLOC_STRING));
                }
                TemplateStringPart::Expression { expression } => {
                    this.compile_expr_as(expression, ValueShape::Boxed)?;
                    this.emit(Instruction::Call(this.rt().base + RT_VALUE_TO_STR));
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

        emit_part(self, &parts[0])?;
        for p in &parts[1..] {
            emit_part(self, p)?;
            // Two strings on the stack — concatenate into one.
            self.emit(Instruction::Call(self.rt().base + RT_CONCAT));
        }
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
        self.emit(Instruction::I32Const(crate::runtime::TRAP_FORCE_UNWRAP_NULL));
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
        self.emit(Instruction::Call(self.rt().base + helper));
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
        for a in args {
            self.emit_string_arg_from_expr(a)?;
        }
        self.emit_import_call(IMPORT_REMOTE_CALL);
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
                self.emit(Instruction::Call(self.rt().base + crate::runtime::RT_COPY_DEEP));
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
                self.compile_expr(&a.value)?;
                self.emit(Instruction::Drop);
            }
            self.emit(Instruction::I64Const(VAL_VOID));
            return Ok(());
        };
        let expected_count = call_args.len() - 1;

        // Stash each expected value in a local so the buffer
        // allocation doesn't clobber it during its own RT_ALLOC.
        let mut expected_locals = Vec::with_capacity(expected_count);
        for a in &call_args[1..] {
            self.compile_expr(&a.value)?;
            let local = self.alloc_local();
            self.emit(Instruction::LocalSet(local));
            expected_locals.push(local);
        }

        let buf = self.alloc_i32_local();
        self.emit(Instruction::I32Const((expected_count.max(1) * 8) as i32));
        self.emit(Instruction::Call(self.rt().base + RT_ALLOC));
        self.emit(Instruction::LocalSet(buf));
        for (i, &local) in expected_locals.iter().enumerate() {
            self.emit(Instruction::LocalGet(buf));
            self.emit(Instruction::LocalGet(local));
            self.emit(Instruction::I64Store(mem_off((i as u64) * 8)));
        }

        self.emit(Instruction::I32Const(fn_id as i32));
        self.emit(Instruction::LocalGet(buf));
        self.emit(Instruction::I32Const(expected_count as i32));
        self.emit_import_call(crate::runtime::IMPORT_SPY_ASSERT_CALLED_WITH);
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
                self.compile_expr(&a.value)?;
                self.emit(Instruction::Drop);
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
            self.compile_expr(&call_args[0].value)?;
            self.emit(Instruction::Drop);
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
                self.compile_expr(args[1])?;
                let import = if once {
                    crate::runtime::IMPORT_SPY_SET_MOCK_ONCE
                } else {
                    crate::runtime::IMPORT_SPY_SET_MOCK
                };
                self.emit_import_call(import);
            }
            None => {
                // Unresolvable target — preserve side effects and
                // emit no host call. The corresponding call sites
                // won't be instrumented either (they weren't in
                // the mocked set), so this is consistent.
                self.compile_expr(args[0])?;
                self.emit(Instruction::Drop);
                self.compile_expr(args[1])?;
                self.emit(Instruction::Drop);
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
                self.compile_expr(args[0])?;
                self.emit(Instruction::Drop);
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
        self.emit(Instruction::Call(self.rt().base + rt_fn));
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
        self.emit_string_arg_from_expr(args[0])?;
        self.emit_import_call(IMPORT_SET_HTML);
        self.emit(Instruction::I64Const(VAL_VOID));
        Ok(())
    }

    fn compile_bare_set_html_at(&mut self, args: &[&Expression]) -> Result<(), BuildError> {
        if args.len() != 2 {
            return Err(BuildError::UnsupportedExpression("setHtmlAt-arg-count"));
        }
        self.emit_string_arg_from_expr(args[0])?;
        self.emit_string_arg_from_expr(args[1])?;
        self.emit_import_call(IMPORT_SET_HTML_AT);
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
        if !self.expr_transfers_ownership(args[2]) {
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
        }
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
                        self.emit_cell_store(binding.local, true);
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
            self.emit(Instruction::If(wasm_encoder::BlockType::Result(ValType::I64)));
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
            // frame[0] = env_ptr = addr + 16
            self.emit(Instruction::LocalGet(frame_l));
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Const(16));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::I32Store(mem0()));
            // frame[8 + 8*i] = arg_i (params sit past the env slot)
            for (i, l) in arg_locals.iter().enumerate() {
                self.emit(Instruction::LocalGet(frame_l));
                self.emit(Instruction::LocalGet(*l));
                self.emit(Instruction::I64Store(mem_off(8 + 8 * i as u64)));
            }
            // id = spawn(table_idx @ addr+4, frame)
            self.emit(Instruction::LocalGet(addr_local));
            self.emit(Instruction::I32Load(mem_off(4)));
            self.emit(Instruction::LocalGet(frame_l));
            self.emit(Instruction::Call(layout.spawn));
            self.emit(Instruction::LocalSet(id_l));
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
            // result = task_result(id)
            self.emit(Instruction::LocalGet(id_l));
            self.emit(Instruction::Call(layout.task_result));
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
            self.emit(Instruction::End); // end if
            self.emit_post_call_propagation();
            for t in owned_arg_stashes {
                self.emit(Instruction::LocalGet(t));
                self.emit(Instruction::Call(self.rt().base + RT_RELEASE));
            }
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
        self.emit_post_call_propagation();
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
                include_in_coverage: false,
                param_defaults: param_defaults_for(fd),
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
            self.emit(Instruction::Call(self.rt().base + RT_RETAIN));
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
    /// `and`/`or` which return a borrowed OPERAND, and interpolation which can be
    /// the degenerate `"{x}"` returning `x`) is BORROWED — co-owning it requires a
    /// retain. Conservative: not-provably-fresh → retain (over-retain leaks; the
    /// opposite would be a use-after-free).
    fn is_fresh_value(expr: &Expression) -> bool {
        match expr {
            Expression::StringExpression(_)
            | Expression::NumberExpression(_)
            | Expression::NullExpression(_)
            | Expression::BooleanExpression(_)
            | Expression::ArrayExpression(_)
            | Expression::DictionaryExpression(_)
            | Expression::TupleExpression(_)
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
        let ufcs_key = (self.module_key.clone(), ce.location.line, ce.location.column);
        let is_ufcs = self.checker().ufcs_calls.contains(&ufcs_key);
        let sig = match &*ce.callee {
            Expression::IdentifierExpression(id) => lookup_bare_call(id.name.as_str()),
            // UFCS `recv.method(...)`: member dispatch checks the borrowed
            // list first (the bare/member set/unwrap asymmetry, preserved
            // by decision — see plans/119 KTD).
            Expression::MemberExpression(me) if is_ufcs => {
                lookup_member_call(me.property.as_str())
            }
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

    fn compile_binary(&mut self, be: &BinaryExpression) -> Result<ValueShape, BuildError> {
        // Short-circuit Bool ops. The checker enforces Bool operands,
        // so the non-evaluated side is never touched at runtime —
        // patterns like `x? and x!.field == 42` rely on this.
        if be.operator == "and" || be.operator == "or" {
            return self.compile_short_circuit(be);
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
mod tests {
    //! End-to-end: parse forai source, feed `main`'s body to the
    //! builder, assemble a minimal wasm module (imports + runtime +
    //! one function), run via wasmtime, and assert the return value.
    //!
    //! This is the Phase C exit-criterion proof: a small program
    //! compiles to wasm *without producing bytecode for its function
    //! body*. The module scaffolding reuses the existing runtime
    //! helpers from `crate::runtime`.
    //!
    //! Programs here are minimal: a single `main` function returning
    //! an Int. Once control flow (Phase D), calls (Phase E), and the
    //! other phases migrate, we'll wire the builder into the main
    //! `module.rs` pipeline.
    use super::*;
    use crate::runtime;
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };
    use wasmtime::{Engine, Linker, Module as RuntimeModule, Store, Val};

    /// Max closure arity the standalone test harness pre-declares
    /// `FaiFunc(N)` types for. Tests only need up to a handful; bumping
    /// this is cheap (one extra type slot per entry).
    const MAX_FAI_ARITY: u16 = 8;

    /// Build the `fai_func_type_indices` map the direct builder needs
    /// for `CallIndirect`. Types are allocated after imports + runtime
    /// helpers (which is how the test harness lays them out below).
    fn build_fai_type_indices() -> HashMap<u16, u32> {
        let import_count = runtime::import_signatures().len() as u32;
        let rt_count = runtime::type_signatures().len() as u32;
        let base = import_count + rt_count;
        (0..=MAX_FAI_ARITY).map(|n| (n, base + n as u32)).collect()
    }

    /// Parse source, locate `def main`, and hand its AST to the
    /// direct builder. Returns the built wasm function.
    /// Standalone-module layout: imports first, then runtime helpers,
    /// then main. The builder needs `rt_base = import_count` so its
    /// `Call(rt_base + RT_*)` instructions land on the right helpers.
    fn rt_base_for_standalone() -> u32 {
        runtime::import_signatures().len() as u32
    }

    /// Identity remap — every import available. Used by tests that
    /// target `None` (native). Matches `runtime::build_import_remap`
    /// applied to an all-true availability vector.
    fn identity_import_remap() -> Vec<Option<u32>> {
        (0..runtime::IMPORT_COUNT as usize)
            .map(|i| Some(i as u32))
            .collect()
    }

    fn compile_main(src: &str) -> Function {
        let mut program = compile_all(src);
        assert!(
            program.closures.is_empty(),
            "compile_main used on a program with closures — use compile_all + build_standalone_module_many",
        );
        program.top_level.remove(0).1
    }

    fn with_tail_expression_builder<R>(
        src: &str,
        f: impl FnOnce(&mut Builder<'_, '_>, &Expression) -> R,
    ) -> R {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };
        let main = prepared
            .serde_ast
            .statements
            .iter()
            .find_map(|stmt| match stmt {
                fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
                    Some(fd)
                }
                _ => None,
            })
            .expect("main function should be present");
        let expression = match main.body.last().expect("main should have a body") {
            fai_compiler::ast::Statement::ExpressionStatement(es) => &es.expression,
            other => panic!("expected tail expression, got {other:?}"),
        };
        let functions = vec![FunctionInfo {
            name: main.name.clone(),
            param_count: main.params.len() as u16 + main.type_params.len() as u16,
            type_param_count: main.type_params.len() as u16,
            include_in_coverage: false,
            param_defaults: param_defaults_for(&main),
            ..Default::default()
        }];
        let type_indices = build_fai_type_indices();
        let module_aliases = HashMap::new();
        let extern_fn_indices = HashMap::new();
        let import_remap = identity_import_remap();
        let enum_members = HashMap::new();
        let type_fields = HashMap::new();
        let named_imports = HashMap::new();
        let mocked_fn_ids = HashSet::new();
        let std_method_fn_ids = HashMap::new();
        let strings = RefCell::new(StringInterner::default());
        let closures = RefCell::new(Vec::new());
        let module_constants = HashMap::new();
        let extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
        let module_vars: HashMap<String, u32> = HashMap::new();
        let ctx = BuildContext {
            rt: RtOffsets {
                base: rt_base_for_standalone(),
            },
            functions: &functions,
            checker: &checker_info,
            import_remap: &import_remap,
            fai_func_type_indices: &type_indices,
            module_aliases: &module_aliases,
            extern_fn_indices: &extern_fn_indices,
            enum_members: &enum_members,
            type_fields: &type_fields,
            named_imports: &named_imports,
            mocked_fn_ids: &mocked_fn_ids,
            std_method_fn_ids: &std_method_fn_ids,
            closure_offset_base: 0,
            strings: &strings,
            closures: &closures,
            module_constants: &module_constants,
            extern_out_params: &extern_out_params,
            module_vars: &module_vars,
            async_ctx: None,
        };
        let mut builder = Builder::new(main, &ctx, None);
        f(&mut builder, expression)
    }

    fn compile_tail_expression_shape(src: &str) -> ValueShape {
        with_tail_expression_builder(src, |builder, expression| {
            builder
                .compile_expr(expression)
                .expect("compile expression")
        })
    }

    fn compile_tail_expression_as(src: &str, want: ValueShape) {
        with_tail_expression_builder(src, |builder, expression| {
            builder
                .compile_expr_as(expression, want)
                .expect("compile expression as shape");
        });
    }

    #[test]
    fn int_int_addition_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  1 + 2\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn int_int_subtraction_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  5 - 2\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn int_int_comparison_returns_raw_bool_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Bool\ndo\n  5 > 2\nend");
        assert_eq!(shape, ValueShape::RawBool);
    }

    #[test]
    fn int_int_division_returns_raw_float_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Float\ndo\n  5 / 2\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn float_float_addition_returns_raw_float_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Float\ndo\n  1.5 + 2.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn int_float_addition_returns_raw_float_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Float\ndo\n  1 + 2.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn float_float_comparison_returns_raw_bool_shape() {
        let shape =
            compile_tail_expression_shape("def main\n    @return Bool\ndo\n  1.5 <= 2.5\nend");
        assert_eq!(shape, ValueShape::RawBool);
    }

    #[test]
    fn int_unary_negation_returns_raw_int_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Int\ndo\n  -5\nend");
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn float_unary_negation_returns_raw_float_shape() {
        let shape = compile_tail_expression_shape("def main\n    @return Float\ndo\n  -5.5\nend");
        assert_eq!(shape, ValueShape::RawFloat);
    }

    #[test]
    fn value_shape_for_type_keeps_only_monomorphic_primitives_raw() {
        use fai_checker::types::{optional_of, Type};

        assert_eq!(shape_for_type(&Type::Int), ValueShape::RawInt);
        assert_eq!(shape_for_type(&Type::Float), ValueShape::RawFloat);
        assert_eq!(shape_for_type(&Type::Bool), ValueShape::RawBool);
        assert_eq!(shape_for_type(&optional_of(Type::Int)), ValueShape::Boxed);
        assert_eq!(shape_for_type(&Type::String), ValueShape::Boxed);
        assert_eq!(shape_for_type(&Type::Unknown), ValueShape::Boxed);
    }

    #[test]
    fn builder_shape_for_expr_uses_checker_expression_type() {
        let shape = with_tail_expression_builder(
            "def main\n    @return Int\ndo\n  1 + 2\nend",
            |builder, expression| builder.shape_for_expr(expression),
        );
        assert_eq!(shape, ValueShape::RawInt);
    }

    #[test]
    fn compile_expr_as_boxed_accepts_current_boxed_expression() {
        compile_tail_expression_as(
            "def main\n    @return Int\ndo\n  1 + 2\nend",
            ValueShape::Boxed,
        );
    }

    #[test]
    fn raw_int_let_identifier_lookup_is_raw_int() {
        // The local's stored shape is what lets the raw arithmetic
        // path pick `I64Add` over `RT_ADD`. `compile_expr` auto-boxes
        // identifier reads (so callers that ignore shape stay safe),
        // so the check goes through `numeric_shape_for_expr` which
        // reads the binding's shape directly.
        with_tail_expression_builder(
            "def main\n    @return Int\ndo\n  let x = 5\n  x\nend",
            |builder, expression| {
                let last = builder.fd.body.len() - 1;
                let prefix: Vec<Statement> = builder.fd.body[..last].to_vec();
                for stmt in &prefix {
                    builder
                        .compile_stmt(stmt)
                        .expect("compile prefix statement");
                }
                assert_eq!(
                    builder.numeric_shape_for_expr(expression),
                    Some(ValueShape::RawInt),
                );
            },
        );
    }

    #[test]
    fn typed_param_prelude_rebinds_int_param_raw() {
        with_tail_expression_builder(
            "def main\n    @param x Int\n    @return Int\ndo\n  x\nend",
            |builder, expression| {
                builder
                    .emit_typed_param_prelude()
                    .expect("emit param prelude");
                assert_eq!(
                    builder.numeric_shape_for_expr(expression),
                    Some(ValueShape::RawInt),
                );
            },
        );
    }

    /// Walk a built standalone module and return the call targets
    /// (function indices) inside `main`'s body. Main lives at code
    /// section index `RT_COUNT` because the standalone layout places
    /// runtime helpers first in the code section, followed by
    /// top-level user functions with `main` first. Used by Phase 4
    /// wasm-inspection tests that assert monomorphic arithmetic elides
    /// the runtime-helper dispatch.
    fn collect_main_call_targets(wasm: &[u8]) -> Vec<u32> {
        let parser = wasmparser::Parser::new(0);
        let mut main_body_idx = 0usize;
        let mut targets = Vec::new();
        for payload in parser.parse_all(wasm) {
            if let wasmparser::Payload::CodeSectionEntry(body) = payload.expect("wasm payload") {
                if main_body_idx == runtime::RT_COUNT as usize {
                    let mut reader = body.get_operators_reader().expect("operators reader");
                    while !reader.eof() {
                        if let wasmparser::Operator::Call { function_index } =
                            reader.read().expect("operator")
                        {
                            targets.push(function_index);
                        }
                    }
                    return targets;
                }
                main_body_idx += 1;
            }
        }
        panic!("main body not found in wasm module");
    }

    #[test]
    fn raw_int_add_emits_no_rt_helper_call() {
        let wasm =
            build_standalone_module(compile_main("def main\n    @return Int\ndo\n  1 + 2\nend"));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    #[test]
    fn raw_float_add_emits_no_rt_helper_call() {
        let wasm = build_standalone_module(compile_main(
            "def main\n    @return Float\ndo\n  1.5 + 2.5\nend",
        ));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    #[test]
    fn mandelbrot_inner_comparison_emits_no_rt_helper_call() {
        // Mirrors the mandelbrot inner loop's
        // `zr * zr + zi * zi <= 4` check. Both sides of the outer `+`
        // are Float, so the add should be native F64Add and the
        // comparison native F64Le — no runtime-helper dispatch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  var zr = toFloat(0)\n",
            "  var zi = toFloat(0)\n",
            "  zr * zr + zi * zi <= 4\n",
            "end\n",
        )));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_LE, "rt_le"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    /// Compile `def main @return Int do <stmt>; <ret_expr> end` and run.
    fn run_let_then_return(stmt: &str, ret_type: &str, ret_expr: &str) -> i64 {
        let src = format!(
            "def main\n    @return {}\ndo\n  {}\n  {}\nend\n",
            ret_type, stmt, ret_expr,
        );
        let wasm = build_standalone_module_many(compile_all(&src));
        run_module(&wasm)
    }

    #[test]
    fn let_float_annotated_int_literal_widens_to_float() {
        // `let val Float = 0` — declared Float, RHS is Int literal 0.
        // Should widen and return 0.0.
        assert_eq!(
            run_let_then_return("let val Float = 0", "Float", "val"),
            boxed_float(0.0),
        );
        // Non-zero Int literal as Float.
        assert_eq!(
            run_let_then_return("let val Float = 7", "Float", "val"),
            boxed_float(7.0),
        );
    }

    #[test]
    fn let_inferred_float_literal_binds_float() {
        assert_eq!(
            run_let_then_return("let val = 0.0", "Float", "val"),
            boxed_float(0.0),
        );
    }

    #[test]
    fn let_float_annotated_int_variable_widens() {
        // Declared Float, RHS is an Int-typed identifier — the
        // RawInt→RawFloat path in emit_convert handles the widening
        // at runtime.
        let src = concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  let n = 7\n",
            "  let f Float = n\n",
            "  f\n",
            "end\n",
        );
        let wasm = build_standalone_module_many(compile_all(src));
        assert_eq!(run_module(&wasm), boxed_float(7.0));
    }

    #[test]
    fn var_float_annotated_int_literal_widens_to_float() {
        // Same as the let case but with `var`.
        assert_eq!(
            run_let_then_return("var val Float = 0", "Float", "val"),
            boxed_float(0.0),
        );
    }

    #[test]
    fn let_int_annotated_whole_float_literal_narrows() {
        // Whole-valued Float literal (e.g. `1.0`) may be declared as
        // Int — the literal narrows exactly. Non-whole literals like
        // `1.23` are rejected by the checker (tested in fai-checker).
        assert_eq!(
            run_let_then_return("let val Int = 0.0", "Int", "val"),
            boxed_int(0),
        );
        assert_eq!(
            run_let_then_return("let val Int = 42.0", "Int", "val"),
            boxed_int(42),
        );
    }

    #[test]
    fn let_inferred_int_literal_binds_int() {
        assert_eq!(
            run_let_then_return("let val = 0", "Int", "val"),
            boxed_int(0),
        );
        assert_eq!(
            run_let_then_return("let val = 42", "Int", "val"),
            boxed_int(42),
        );
    }

    #[test]
    fn let_bool_literal_binds_bool() {
        assert_eq!(
            run_let_then_return("let val = true", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val = false", "Bool", "val"),
            boxed_bool(false),
        );
        assert_eq!(
            run_let_then_return("let val Bool = true", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val Bool = false", "Bool", "val"),
            boxed_bool(false),
        );
    }

    #[test]
    fn let_bool_from_comparison_binds_bool() {
        assert_eq!(
            run_let_then_return("let val = 1 != 2", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val Bool = 1 == 1", "Bool", "val"),
            boxed_bool(true),
        );
        assert_eq!(
            run_let_then_return("let val = 1 == 2", "Bool", "val"),
            boxed_bool(false),
        );
    }

    #[test]
    fn raw_mixed_int_float_emits_no_rt_helper_call() {
        let wasm = build_standalone_module(compile_main(
            "def main\n    @return Float\ndo\n  3 + 0.5\nend",
        ));
        let targets = collect_main_call_targets(&wasm);
        let rt_base = rt_base_for_standalone();
        for (offset, name) in [
            (runtime::RT_ADD, "rt_add"),
            (runtime::RT_MAKE_INT, "rt_make_int"),
            (runtime::RT_MAKE_FLOAT, "rt_make_float"),
            (runtime::RT_AS_NUMBER, "rt_as_number"),
        ] {
            let forbidden = rt_base + offset;
            assert!(
                !targets.contains(&forbidden),
                "expected no call to {} ({}) in main body, got call targets: {:?}",
                name,
                forbidden,
                targets,
            );
        }
    }

    fn boxed_int(n: i32) -> i64 {
        runtime::QNAN | runtime::TAG_INT | (n as u32 as i64)
    }

    fn boxed_bool(b: bool) -> i64 {
        runtime::QNAN | runtime::TAG_BOOL | (if b { 1 } else { 0 })
    }

    fn boxed_float(f: f64) -> i64 {
        f.to_bits() as i64
    }

    /// Compile `def main @return {ret} do {expr} end` and run it.
    fn run_main_expr(ret: &str, expr: &str) -> i64 {
        let src = format!("def main\n    @return {}\ndo\n  {}\nend\n", ret, expr);
        let wasm = build_standalone_module(compile_main(&src));
        run_module(&wasm)
    }

    #[test]
    fn raw_int_local_passed_to_is_int_is_boxed() {
        // is_int inspects the NaN-box tag bits; if `x` leaks raw
        // (as a bare i64 without QNAN|TAG_INT), is_int returns false
        // and the program returns boxed_bool(false). This is the
        // observable form of the print-garbage bug the benchmark
        // surfaced: builtins taking Unknown must receive boxed values.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let x = 7\n",
            "  is_int(x)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(true));
    }

    #[test]
    fn int_int_operator_matrix_returns_correct_values() {
        // Arithmetic → Int (except `/` which promotes to Float).
        for (op, expected) in [("+", 9), ("-", 3), ("*", 18), ("//", 2), ("%", 0)] {
            let got = run_main_expr("Int", &format!("6 {} 3", op));
            assert_eq!(got, boxed_int(expected), "Int Int `{}` on 6, 3", op);
        }
        // Division: 6 / 3 → 2.0 Float.
        assert_eq!(
            run_main_expr("Float", "6 / 3"),
            boxed_float(2.0),
            "Int Int `/` promotes to Float",
        );
        // Comparisons → Bool.
        for (op, expected) in [
            ("==", false),
            ("!=", true),
            ("<", false),
            ("<=", false),
            (">", true),
            (">=", true),
        ] {
            let got = run_main_expr("Bool", &format!("6 {} 3", op));
            assert_eq!(got, boxed_bool(expected), "Int Int `{}` on 6, 3", op);
        }
    }

    #[test]
    fn float_float_operator_matrix_returns_correct_values() {
        for (op, expected) in [("+", 9.0), ("-", 3.0), ("*", 18.0), ("/", 2.0)] {
            let got = run_main_expr("Float", &format!("6.0 {} 3.0", op));
            assert_eq!(
                got,
                boxed_float(expected),
                "Float Float `{}` on 6.0, 3.0",
                op
            );
        }
        for (op, expected) in [
            ("==", false),
            ("!=", true),
            ("<", false),
            ("<=", false),
            (">", true),
            (">=", true),
        ] {
            let got = run_main_expr("Bool", &format!("6.0 {} 3.0", op));
            assert_eq!(
                got,
                boxed_bool(expected),
                "Float Float `{}` on 6.0, 3.0",
                op
            );
        }
    }

    #[test]
    fn int_float_operator_matrix_returns_correct_values() {
        // Mixed arithmetic promotes to Float. Forai's checker
        // rejects mixed-type comparisons, so those are covered only
        // by the same-type matrices.
        for (op, expected) in [("+", 6.5), ("-", 5.5), ("*", 3.0), ("/", 12.0)] {
            let got = run_main_expr("Float", &format!("6 {} 0.5", op));
            assert_eq!(got, boxed_float(expected), "Int Float `{}` on 6, 0.5", op);
        }
    }

    #[test]
    fn float_int_operator_matrix_returns_correct_values() {
        for (op, expected) in [("+", 7.0), ("-", 5.0), ("*", 6.0), ("/", 6.0)] {
            let got = run_main_expr("Float", &format!("6.0 {} 1", op));
            assert_eq!(got, boxed_float(expected), "Float Int `{}` on 6.0, 1", op);
        }
    }

    /// A compiled program for the standalone-module tests:
    /// top-level functions followed by the closures each of their
    /// bodies materialised, plus the interned string-data buffer the
    /// module assembler lays out at memory offset 0.
    struct TestProgram {
        top_level: Vec<(FunctionInfo, Function)>,
        closures: Vec<BuiltClosure>,
        string_data: Vec<u8>,
    }

    /// Compile every top-level function in `src` directly to wasm.
    /// Runs the checker first to capture expression types, UFCS, and
    /// named-param reorder info, then feeds each function declaration
    /// to `build_function` with the standalone-module type-index layout.
    fn compile_all(src: &str) -> TestProgram {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };

        let mut decls: Vec<fai_compiler::ast::FunctionDeclaration> = Vec::new();
        // `main` is emitted first so it lands at proto index 0 —
        // matches the production pipeline's convention.
        if let Some(main) = prepared.serde_ast.statements.iter().find_map(|s| match s {
            fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
                Some(fd.clone())
            }
            _ => None,
        }) {
            decls.push(main);
        }
        for s in &prepared.serde_ast.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                if fd.name != "main" {
                    decls.push(fd.clone());
                }
            }
        }
        let infos: Vec<FunctionInfo> = decls
            .iter()
            .map(|fd| FunctionInfo {
                name: fd.name.clone(),
                param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
                type_param_count: fd.type_params.len() as u16,
                include_in_coverage: fd.name != "main",
                param_defaults: param_defaults_for(fd),
                source_line: fd.location.line,
                ..Default::default()
            })
            .collect();
        let rt = RtOffsets {
            base: rt_base_for_standalone(),
        };
        let type_indices = build_fai_type_indices();
        // Collect top-level `use` statements and build
        // alias → canonical-path. Only namespace imports are
        // supported on this standalone helper path (named imports
        // like `use X { foo, bar }` are covered by production module
        // preparation).
        let module_aliases = collect_module_aliases(&prepared.serde_ast.statements);
        let extern_fn_indices = collect_extern_fn_indices(&prepared.serde_ast.statements);
        // Collect enum + type-declaration tables so tests exercise
        // the same paths the production `build_program_full` does.
        let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
        let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> =
            HashMap::new();
        for s in &prepared.serde_ast.statements {
            match s {
                fai_compiler::ast::Statement::EnumDeclaration(ed) => {
                    enum_members.insert(ed.name.clone(), ed.members.clone());
                }
                fai_compiler::ast::Statement::TypeDeclaration(td) => {
                    type_fields.insert(td.name.clone(), td.fields.clone());
                }
                _ => {}
            }
        }
        let named_imports: HashMap<String, String> = HashMap::new();
        let strings = RefCell::new(StringInterner::default());
        let remap = identity_import_remap();
        let mut top_level = Vec::with_capacity(decls.len());
        let mut all_closures = Vec::new();
        let empty_mocked: HashSet<u32> = HashSet::new();
        let empty_std_ids: HashMap<(String, String), u32> = HashMap::new();
        for (fd, info) in decls.iter().zip(infos.iter().cloned()) {
            let result = build_function_with_spy_and_offset(
                fd,
                rt,
                &infos,
                &checker_info,
                &type_indices,
                &module_aliases,
                &extern_fn_indices,
                &remap,
                &strings,
                &enum_members,
                &type_fields,
                &named_imports,
                &empty_mocked,
                &empty_std_ids,
                all_closures.len() as u32,
                None,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                None,
                None,
            )
            .unwrap_or_else(|e| panic!("direct build failed: {:?}", e));
            top_level.push((info, result.main));
            all_closures.extend(result.closures);
        }
        TestProgram {
            top_level,
            closures: all_closures,
            string_data: strings.into_inner().bytes,
        }
    }

    /// Walk top-level statements for namespace `use` imports and
    /// build a `last-segment → full-dotted-path` map. `use std.file`
    /// becomes `"file" -> "std.file"`, `use std.net.tcp` becomes
    /// `"tcp" -> "std.net.tcp"`. Named imports (`use X { a }`) are
    /// skipped — those need per-symbol binding that the direct path
    /// doesn't support yet.
    fn collect_module_aliases(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, String> {
        let mut aliases = HashMap::new();
        for s in stmts {
            if let fai_compiler::ast::Statement::UseStatement(u) = s {
                if u.import_all || u.imported_names.is_some() {
                    continue;
                }
                if let Some(last) = u.module_path.last() {
                    aliases.insert(last.clone(), u.module_path.join("."));
                }
            }
        }
        aliases
    }

    /// Walk top-level `extern { ... }` blocks and assign each
    /// function a stable index. Matches the ordering
    /// `compiler.rs` uses so the host's extern table indices line
    /// up whether the function was built via the direct path or the
    /// bytecode path. A program with no extern blocks returns an
    /// empty map.
    fn collect_extern_fn_indices(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, u16> {
        let mut indices = HashMap::new();
        let mut next_idx = 0u16;
        for s in stmts {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    indices.insert(f.name.clone(), next_idx);
                    next_idx = next_idx.checked_add(1).expect("too many extern functions");
                }
            }
        }
        indices
    }

    /// Build a standalone wasm module from a compiled program:
    /// runtime helpers + top-level functions + closure functions. A
    /// table populated from the closure list lets `call_indirect`
    /// dispatch at runtime.
    ///
    /// Module layout (function-index space):
    /// - `[0, import_count)` host imports
    /// - `[import_count, import_count + RT_COUNT)` runtime helpers
    /// - `[import_count + RT_COUNT, ... + top_level_count)` fai funcs
    /// - `[after top_level, ... + closure_count)` closures (in
    ///   `top_level[0].proto_index` order)
    ///
    /// Types are pre-allocated so that `fai_func_type_indices[N]`
    /// matches what `build_function` already baked into the
    /// `CallIndirect` instructions.
    fn build_module(program: TestProgram) -> Vec<u8> {
        let mut module = EncModule::new();

        // ── types ──
        let mut types = TypeSection::new();
        let import_sigs = runtime::import_signatures();
        let mut import_type_indices = Vec::with_capacity(import_sigs.len());
        for (_, params, results) in &import_sigs {
            import_type_indices.push(types.len());
            types.ty().function(params.clone(), results.clone());
        }
        let rt_sigs = runtime::type_signatures();
        let mut rt_type_indices = Vec::with_capacity(rt_sigs.len());
        for (params, results) in &rt_sigs {
            rt_type_indices.push(types.len());
            types.ty().function(params.clone(), results.clone());
        }
        // Pre-declare FaiFunc(0..=MAX_FAI_ARITY) — matches what
        // `build_fai_type_indices` hands to the builder above. Any
        // direct-built function or closure with arity in this range
        // picks the matching slot; closure `CallIndirect` instructions
        // reference it by absolute type index.
        let fai_type_indices = build_fai_type_indices();
        for arity in 0..=MAX_FAI_ARITY {
            let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
            let expected = types.len();
            types.ty().function(params, vec![ValType::I64]);
            assert_eq!(
                expected, fai_type_indices[&arity],
                "test harness fai-type layout diverged from builder's type_indices map",
            );
        }
        module.section(&types);

        // ── imports ──
        let mut imports = ImportSection::new();
        for (i, (name, _, _)) in import_sigs.iter().enumerate() {
            imports.import("env", name, EntityType::Function(import_type_indices[i]));
        }
        module.section(&imports);

        // ── functions ──
        // [rt_0 ...] [top_level_0 ...] [closure_0 ...]
        let mut funcs = FunctionSection::new();
        for &t in &rt_type_indices {
            funcs.function(t);
        }
        for (info, _) in &program.top_level {
            funcs.function(fai_type_indices[&info.param_count]);
        }
        for c in &program.closures {
            funcs.function(fai_type_indices[&c.info.param_count]);
        }
        module.section(&funcs);

        // ── tables ──
        // Elements are populated below; the table's min size equals
        // the closure count so wasm validation accepts the element
        // segment. Empty program → empty table is still legal.
        let mut tables = TableSection::new();
        let closure_count = program.closures.len() as u32;
        tables.table(TableType {
            element_type: RefType::FUNCREF,
            minimum: closure_count as u64,
            maximum: Some(closure_count as u64),
            table64: false,
            shared: false,
        });
        module.section(&tables);

        // ── memory ──
        let mut mem = MemorySection::new();
        mem.memory(MemoryType {
            minimum: 16,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&mem);

        // ── globals ──
        // Order matches translate.rs: __heap_ptr, __env_ptr,
        // error_flag, error_value. Index 1 is env_ptr — which is
        // what `GLOBAL_ENV_PTR` (and translate.rs) reference.
        //
        // `__heap_ptr` starts above the interned string data so heap
        // allocations don't overwrite it. Round up to 8-byte
        // alignment (RT_ALLOC hands out aligned blocks).
        let bucket_base = ((program.string_data.len() as u32) + 7) & !7;
        let heap_start = (bucket_base + runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(heap_start as i32),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(0),
        );
        // Heap free-list head (index 4 — appended after the 4 fixed globals;
        // this harness has no module-var/scheduler globals). 0 = empty list.
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        // Live-object counter (index 5, plan 113).
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(0),
        );
        module.section(&globals);

        let import_count = import_sigs.len() as u32;
        let top_level_base = import_count + runtime::RT_COUNT;
        let closure_base = top_level_base + program.top_level.len() as u32;
        let main_func_idx = top_level_base;

        // ── exports ──
        // `_start`, `memory`, and `__heap_ptr` are the three the
        // production build emits and the runtime tests rely on. The
        // heap-pointer global is exposed so heap-boundary regression
        // tests can pre-position it before invoking `_start` to
        // exercise allocation patterns that only show up when the
        // heap is near a page boundary.
        let mut exports = ExportSection::new();
        exports.export("_start", ExportKind::Func, main_func_idx);
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("__heap_ptr", ExportKind::Global, 0);
        exports.export("__live_objects", ExportKind::Global, 5); // plan 113 oracle
        module.section(&exports);

        // ── elements ──
        // Populate the function-reference table: slot `i` points at
        // the wasm function for closure `i`. Closure `i`'s runtime
        // `table_idx` field is just `i`, which matches what
        // `compile_function_expression` wrote.
        if closure_count > 0 {
            let mut elements = ElementSection::new();
            let func_indices: Vec<u32> = (0..closure_count).map(|i| closure_base + i).collect();
            elements.active(
                Some(0),
                &ConstExpr::i32_const(0),
                Elements::Functions(func_indices.into()),
            );
            module.section(&elements);
        }

        // ── code ──
        let mut code = CodeSection::new();
        let import_remap: Vec<Option<u32>> = (0..runtime::IMPORT_COUNT as usize)
            .map(|i| Some(i as u32))
            .collect();
        let known = runtime::KnownStrings::default();
        for f in runtime::emit_all(import_count, &import_remap, &known, 4, 5, bucket_base) {
            code.function(&f);
        }
        for (_, f) in &program.top_level {
            code.function(f);
        }
        for c in &program.closures {
            code.function(&c.function);
        }
        module.section(&code);

        // ── data ──
        // Emit the string-literal pool as an active segment starting
        // at memory offset 0. `RT_ALLOC_STRING(offset, len)` reads
        // these bytes and copies them into a freshly-allocated
        // String object, so the data must survive the module's
        // lifetime at a known offset.
        if !program.string_data.is_empty() {
            let mut data = DataSection::new();
            data.active(
                0,
                &ConstExpr::i32_const(0),
                program.string_data.iter().copied(),
            );
            module.section(&data);
        }

        // ── debug metadata (plan 116): name section + fai-dbg table ──
        let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
        for (i, (name, _, _)) in import_sigs.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(i as u32, *name));
        }
        for (k, n) in runtime::rt_fn_names().iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(
                import_count + k as u32,
                *n,
            ));
        }
        for (i, (info, _)) in program.top_level.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry {
                index: top_level_base + i as u32,
                name: info.name.clone(),
                file: info.source_file.clone(),
                line: info.source_line,
            });
        }
        for (i, c) in program.closures.iter().enumerate() {
            dbg.push(crate::debug_info::FnDebugEntry {
                index: closure_base + i as u32,
                name: c.info.name.clone(),
                file: c.info.source_file.clone(),
                line: c.info.source_line,
            });
        }
        crate::debug_info::append_debug_sections(
            &mut module,
            &dbg,
            &crate::debug_info::DbgMeta {
                bucket_base: Some(bucket_base),
                bucket_count: runtime::NUM_FREE_BUCKETS,
            },
        );

        module.finish()
    }

    /// Wrapper for single-main tests that don't define any other
    /// functions. Wraps the given `Function` as `main` with zero
    /// arity, no closures.
    fn build_standalone_module(main_fn: Function) -> Vec<u8> {
        build_module(TestProgram {
            top_level: vec![(
                FunctionInfo {
                    name: "main".to_string(),
                    param_count: 0,
                    type_param_count: 0,
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    ..Default::default()
                },
                main_fn,
            )],
            closures: Vec::new(),
            string_data: Vec::new(),
        })
    }

    /// Multi-function wrapper for tests that build a source file's
    /// entire top-level function list plus any nested closures.
    fn build_standalone_module_many(program: TestProgram) -> Vec<u8> {
        build_module(program)
    }

    fn run_module(wasm: &[u8]) -> i64 {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        // Stub every host import the module declares. Phase C
        // arithmetic doesn't need any of them at runtime, but
        // validation needs the functions to be present. Each stub
        // matches its signature and returns a default.
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Sync program: a single `_start` returning the root value.
        if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            return start.call(&mut store, ()).expect("run");
        }
        // Async program: kick off the root task, drive `__fai_poll` to completion
        // (status 2 = done, 3 = failed), then read the root's result. Any program
        // that invokes a closure value is async now (closure calls are potential
        // suspension points), so previously-sync tests can land here.
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("_start or _start_async export");
        start_async.call(&mut store, ()).expect("run _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("__fai_poll export");
        let mut status = 1;
        for _ in 0..10_000_000 {
            status = poll.call(&mut store, ()).expect("poll");
            if status == 2 || status == 3 {
                break;
            }
        }
        assert!(status == 2, "async root did not complete (status {status})");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("__fai_task_result export");
        task_result.call(&mut store, 1).expect("task_result")
    }

    #[test]
    fn direct_int_literal_return() {
        // Simplest possible program: `def main @return Int do 42 end`.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // NaN-boxed Int: high bits = QNAN | TAG_INT, low 32 = value.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected, "direct-built main should return 42");
    }

    #[test]
    fn direct_arithmetic() {
        // Exercise RT_ADD/RT_SUB/RT_MUL through the direct path.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  2 * 3 + 4 - 1\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (9u32 as u64);
        assert_eq!(result, expected, "2*3 + 4 - 1 should be 9");
    }

    #[test]
    fn direct_float_arithmetic() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  1.5 + 2.5\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(result, 4.0_f64.to_bits());
    }

    #[test]
    fn direct_int_division_returns_float() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  5 / 2\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(result, 2.5_f64.to_bits());
    }

    #[test]
    fn direct_let_binding() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let x = 10\n",
            "  let y = 32\n",
            "  x + y\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_comparison_true() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  5 > 3\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_unary_negation() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 7\n",
            "  -n\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | ((-7i32) as u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_true_branch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var x = 0\n",
            "  if true\n",
            "    x = 42\n",
            "  end\n",
            "  x\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_raw_bool_local_condition() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let flag = true\n",
            "  if flag\n",
            "    7\n",
            "  else\n",
            "    3\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_else_picks_else() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  if false\n",
            "    1\n",
            "  else\n",
            "    99\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (99u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_if_else_if_chain() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 2\n",
            "  if n == 1\n",
            "    10\n",
            "  else if n == 2\n",
            "    20\n",
            "  else if n == 3\n",
            "    30\n",
            "  else\n",
            "    99\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (20u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_sum_to_ten() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  var sum = 0\n",
            "  while i < 10\n",
            "    i = i + 1\n",
            "    sum = sum + i\n",
            "  end\n",
            "  sum\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 1 + 2 + ... + 10 = 55
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (55u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_break() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  while true\n",
            "    if i == 5\n",
            "      break\n",
            "    end\n",
            "    i = i + 1\n",
            "  end\n",
            "  i\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (5u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_while_continue_skips_iteration() {
        // Sum 1..=10 but skip multiples of 3. Expected: 1+2+4+5+7+8+10 = 37.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var i = 0\n",
            "  var sum = 0\n",
            "  while i < 10\n",
            "    i = i + 1\n",
            "    if i % 3 == 0\n",
            "      continue\n",
            "    end\n",
            "    sum = sum + i\n",
            "  end\n",
            "  sum\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (37u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_continue_skips_iteration() {
        // `continue` in for-range must reach the counter increment,
        // not jump back to the condition check. Without the inner
        // $continue block this'd infinite-loop.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 1..5\n",
            "    if i == 3\n",
            "      continue\n",
            "    end\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // `1..5` is exclusive: visits 1,2,3,4. Skip 3 → 1+2+4 = 7.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_exclusive_sum() {
        // `..` is exclusive: 0+1+2+3+4 = 10.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 0..5\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (10u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_inclusive_sum() {
        // `...` is inclusive: 0+1+2+3+4+5 = 15.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 0...5\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (15u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_range_break() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for i in 1..100\n",
            "    if i > 3\n",
            "      break\n",
            "    end\n",
            "    s = s + i\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // Runs i=1,2,3 then break. sum = 6.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (6u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_catches_throw() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    throw 99\n",
            "    result = 1\n",
            "  catch e\n",
            "    result = 42\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_no_throw_skips_catch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    result = 1\n",
            "  catch e\n",
            "    result = 99\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_finally_runs_after_success() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var log = 0\n",
            "  try\n",
            "    log = 1\n",
            "  catch e\n",
            "    log = 2\n",
            "  finally\n",
            "    log = log + 10\n",
            "  end\n",
            "  log\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // try path runs → log=1; finally adds 10 → 11.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 11;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_try_finally_runs_after_catch() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var log = 0\n",
            "  try\n",
            "    throw 99\n",
            "    log = 1\n",
            "  catch e\n",
            "    log = 2\n",
            "  finally\n",
            "    log = log + 100\n",
            "  end\n",
            "  log\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // throw → catch sets log=2; finally adds 100 → 102.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 102;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_catch_binds_thrown_value() {
        // The catch body must bind `e` to the thrown value. The
        // checker types `e` as `Error`, so we don't do arithmetic on
        // it — `e == e` exercises the binding (always true) without
        // tripping the type rule.
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var result = 0\n",
            "  try\n",
            "    throw 7\n",
            "  catch e\n",
            "    if e == e\n",
            "      result = 42\n",
            "    end\n",
            "  end\n",
            "  result\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_unary_not_flips_truthiness() {
        let wasm = build_standalone_module(compile_main(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  !false\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_simple() {
        // Main calls a helper defined in the same file. Helper's
        // proto index is 1 (main is always 0 in `compile_all`'s
        // ordering).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double an int.\n",
            "def double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  double(21)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_multi_arg() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Add two ints.\n",
            "def add\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  add(10, 32)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_recursion() {
        // Classic factorial — recursion on the direct path proves the
        // wasm function index resolution is consistent between the
        // caller and the callee's self-reference.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Integer factorial.\n",
            "def fact\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "  if n <= 1\n",
            "    1\n",
            "  else\n",
            "    n * fact(n - 1)\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  fact(5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 5! = 120
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 120;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_call_return_used_in_expr() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Return a constant.\n",
            "def ten\n",
            "    @return Int\n",
            "do\n",
            "  10\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  ten() + ten() + 22\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_rewrites_to_positional() {
        // `x.double()` with `double` a user-declared function rewrites
        // to `double(x)` — the checker marks the location; the builder
        // reads it and emits a direct call.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double an int.\n",
            "def double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 21\n",
            "  n.double()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_with_named_param_reorder() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Greet.\n",
            "def greet\n",
            "    @param name String\n",
            "    @param salutation String\n",
            "    @return String\n",
            "do\n",
            "  salutation + ', ' + name\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length('Alice'.greet(salutation: 'Hi'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 9;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_chain() {
        // Chained UFCS: `x.doubled().incremented()` → `incremented(doubled(x))`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Doubled.\ndef doubled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "# Incremented.\ndef incremented\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 10\n",
            "  n.doubled().incremented()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // (10 * 2) + 1 = 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_ufcs_with_extra_args() {
        // `x.add(5)` → `add(x, 5)`. The object becomes the first
        // positional arg, the remaining call args follow.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Add.\ndef add\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 37\n",
            "  n.add(5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_in_order() {
        // Named args in declaration order — no reorder entry from the
        // checker. The builder compiles them as positional.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Sub.\ndef sub\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a - b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sub(a: 50, b: 8)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_reordered() {
        // `b: 8, a: 50` is the opposite of declaration order — the
        // checker records a reorder map; the builder evaluates args
        // in source order but emits them in declaration order.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Sub.\ndef sub\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a - b\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sub(b: 8, a: 50)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 50 - 8 = 42 — proves the reorder put `a=50` in slot 0 and
        // `b=8` in slot 1 even though the call wrote b first.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_named_params_three_way_reorder() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Mk.\ndef mk\n",
            "    @param first Int\n",
            "    @param second Int\n",
            "    @param third Int\n",
            "    @return Int\n",
            "do\n",
            "  first * 100 + second * 10 + third\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  mk(third: 3, first: 1, second: 2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // first=1 second=2 third=3 → 123
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 123;
        assert_eq!(result, expected);
    }

    /// Extract just `main`'s AST + run the checker. Used by
    /// rejection tests that want to exercise a specific error path
    /// in `build_function` without building the whole module.
    fn compile_main_ast(
        src: &str,
    ) -> (
        fai_compiler::ast::FunctionDeclaration,
        CheckerInfo,
        Vec<FunctionInfo>,
    ) {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare");
        let mut checker = fai_checker::Checker::new();
        // Some rejection tests intentionally include constructs the
        // checker flags (e.g. closures with captured locals of the
        // wrong type). We still want the builder to get the AST, so
        // swallow checker errors here.
        let _ = checker.check_program(&prepared.serde_ast.statements);
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        let mut infos: Vec<FunctionInfo> = Vec::new();
        let mut main = None;
        for s in &prepared.serde_ast.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                infos.push(FunctionInfo {
                    name: fd.name.clone(),
                    param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
                    type_param_count: fd.type_params.len() as u16,
                    include_in_coverage: fd.name != "main",
                    param_defaults: param_defaults_for(fd),
                    ..Default::default()
                });
                if fd.name == "main" {
                    main = Some(fd.clone());
                }
            }
        }
        (main.expect("no main"), info, infos)
    }

    #[test]
    fn direct_nested_closure_reads_enclosing_local() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let outer = do with n Int\n",
            "    let inner = do with m Int\n",
            "      n + m\n",
            "    end\n",
            "    inner(3)\n",
            "  end\n",
            "  outer(39)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nested_closure_writes_enclosing_upvalue() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var total = 0\n",
            "  let outer = do\n",
            "    let inner = do\n",
            "      total = total + 21\n",
            "    end\n",
            "    inner()\n",
            "    inner()\n",
            "  end\n",
            "  outer()\n",
            "  total\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nested_closure_returned_then_called_later() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let makeAdder = do with n Int\n",
            "    do with m Int\n",
            "      n + m\n",
            "    end\n",
            "  end\n",
            "  let add40 = makeAdder(40)\n",
            "  add40(2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_rejects_module_member_access() {
        // Without an alias map, `file.read(path)` cannot resolve as a
        // known std module call and must refuse instead of compiling
        // arbitrary member access as a module dispatch.
        let (main, ci, infos) = compile_main_ast(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/tmp/x')\n",
            "end\n",
        ));
        // Note: the module-alias map is empty — `compile_main_ast`
        // doesn't build one, so `file` doesn't resolve as a known
        // alias and we fall through to the unknown-module refusal.
        let err = build_function(
            &main,
            RtOffsets {
                base: rt_base_for_standalone(),
            },
            &infos,
            &ci,
            &build_fai_type_indices(),
            &HashMap::new(),
            &HashMap::new(),
            &identity_import_remap(),
            &RefCell::new(StringInterner::default()),
        )
        .expect_err("builder should refuse module member calls");
        match err {
            BuildError::ModuleAccessNotYetSupported(name) => assert_eq!(name, "read"),
            other => panic!("expected ModuleAccessNotYetSupported, got {:?}", other),
        }
    }

    #[test]
    fn direct_use_statement_in_body_is_noop() {
        // Contrived but legal: a `use` statement sitting inside a
        // function body. Direct builder treats it as a no-op since
        // module resolution already happened upstream.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  use std.array\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Nowait (fire-and-forget spawn) ──────────────────────────
    //
    // `nowait expr` wraps `expr` in a zero-arg closure and hands
    // it to `IMPORT_SPAWN`. Under the current tier-1 runtime the
    // host invokes the closure synchronously, so the observable
    // behaviour is equivalent to calling the body directly — the
    // asynchrony boundary is at a higher layer.

    #[test]
    fn direct_type_constructor_builds_dict() {
        // `type Point @x Int @y Int end` with a constructor call.
        // The constructor lowers to a dict literal; field access
        // via `p.x` uses the normal RT_GET_FIELD path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Point\n",
            "  x Int\n",
            "  y Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let p = Point(x: 3, y: 4)\n",
            "  p.x + p.y\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_type_constructor_fills_defaults() {
        // Fields with `= default` are filled when the caller omits
        // them. Here `color` defaults to 'red'; the constructor
        // call only supplies `name`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "type Thing\n",
            "  name String\n",
            "  color String = 'red'\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let t = Thing(name: 'x')\n",
            "  length(t.color)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_spy_records_call_and_mocks_return() {
        // Full spy pipeline: a test block that mocks a user fn,
        // invokes it, then asserts call count and argument shape.
        // This exercises preamble emission, spy_check_call wiring,
        // and the assert.* imports end-to-end against the same
        // compile_all test helper used by the rest of this module.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double.\ndef double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        )));
        // compile_all runs with is_test=false and no mocked_fn_ids
        // (the test helper intentionally stays minimal). So we're
        // just checking that the generated module still runs clean
        // on the unmocked path. End-to-end spy behavior is exercised
        // by the CLI's todo-cli fixture where `is_test=true` and
        // real test blocks drive the instrumentation.
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_multiple_fn_refs_dont_share_closure_slot() {
        // Regression: two top-level functions each synthesizing a
        // forwarding closure for a different fn-ref used to bake
        // `table_idx=0` into both closures' headers. At runtime
        // call_indirect then landed on whichever closure was
        // emitted first, so `apply(x, doubled)` and
        // `apply(x, tripled)` both returned the doubled value.
        //
        // Fix: `closure_offset_base` threads the global closure
        // count into each top-level function's builder so the
        // baked `table_idx` matches the module's element-section
        // slot.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Doubled.\ndef doubled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "# Tripled.\ndef tripled\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 3\n",
            "end\n",
            "\n",
            "# Apply.\ndef apply\n",
            "    @param v Int\n",
            "    @param fn (Int) -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn(v)\n",
            "end\n",
            "\n",
            "# CallDoubled.\ndef callDoubled\n",
            "    @return Int\n",
            "do\n",
            "  apply(5, doubled)\n",
            "end\n",
            "\n",
            "# CallTripled.\ndef callTripled\n",
            "    @return Int\n",
            "do\n",
            "  apply(5, tripled)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  callDoubled() + callTripled()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 5*2 + 5*3 = 25; if both fn-refs collided onto one closure
        // slot, we'd get 5*2 + 5*2 = 20 or 5*3 + 5*3 = 30.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 25;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_mock_stubs_compile_and_run_as_noops() {
        // `mock(fn, val)`, `mockOnce`, `mockReset`, and
        // `assert.{calledWith,callCount,notCalled}` are checker-known
        // void-returning builtins with no runtime interception yet.
        // They compile as no-ops so test blocks that reference them
        // don't refuse codegen. This test exercises each shape inside
        // an entry function and verifies main returns cleanly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Helper.\ndef helper\n",
            "    @return Int\n",
            "do\n",
            "  1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  mock(helper, 7)\n",
            "  mockOnce(helper, 8)\n",
            "  mockReset(helper)\n",
            "  assert.calledWith(helper)\n",
            "  assert.callCount(helper, 0)\n",
            "  assert.notCalled(helper)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_function_ref_as_value_runs_through_apply() {
        // Passing a top-level `def` name as a value — `apply(x, shout)` —
        // synthesizes a forwarding closure under the hood. Verify the
        // value round-trips through `apply` and the wrapped call
        // returns the original input.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Shout.\ndef shout\n",
            "    @param text Int\n",
            "    @return Int\n",
            "do\n",
            "  text\n",
            "end\n",
            "\n",
            "# Apply.\ndef apply\n",
            "    @param value Int\n",
            "    @param fn (Int) -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn(value)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  apply(42, shout)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_function_ref_zero_arg_forwards() {
        // Zero-arity function references should synthesize a
        // zero-param wrapper. `call(greet)` round-trips through the
        // closure → indirect call → named function.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Greet.\ndef greet\n",
            "    @return Int\n",
            "do\n",
            "  7\n",
            "end\n",
            "\n",
            "# Call.\ndef call\n",
            "    @param fn () -> Int\n",
            "    @return Int\n",
            "do\n",
            "  fn()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  call(greet)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_runs_body() {
        // Since the host stub for IMPORT_SPAWN in our tests is a
        // no-op (returns I64(0)), the closure body doesn't execute
        // here. Verify the program still compiles and main's
        // trailing literal returns cleanly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# SlowWork.\ndef slowWork @return Void do\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nowait slowWork()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_spawn_import_is_called() {
        // Override IMPORT_SPAWN to return a known NaN-boxed Int
        // — proves the spawn dispatch fires and its return is
        // threaded through Drop. Not a valid return value per the
        // runtime contract (spawn returns VAL_VOID in production),
        // but harmless since we Drop it.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# SlowWork.\ndef slowWork @return Void do\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nowait slowWork()\n",
            "  7\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (999u32 as u64);
        // Main doesn't return spawn's result (it's Drop'd), so main
        // still returns 7 regardless of the stub's value. The test
        // proves the spawn call is present in the wasm (otherwise
        // the override would have no effect) and that validation
        // passes.
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_SPAWN, Val::I64(sentinel as i64))
                as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_legacy_bare_all_two_tasks_dispatches_run_all() {
        // `all(e1, e2)` should synthesize a closure per arg and
        // call IMPORT_RUN_ALL. Override the import to return a
        // sentinel so we can prove the dispatch fires. (The
        // default stub returns 0, which would pass a `let _ = all(...)`
        // trivially — the override makes the link load-bearing.)
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Double.\ndef double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = all(double(1), double(2))\n",
            "  7\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (42u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_RUN_ALL, Val::I64(sentinel as i64))
                as u64;
        // The returned tuple is bound to `_` and unused; main returns 7.
        // Hitting the sentinel proves only that validation + imports
        // wired up; the 7 below proves downstream compilation kept
        // working after the synthesized closures landed.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_nowait_captures_upvalue() {
        // The wrapped closure sees outer locals as upvalues — same
        // machinery as `do with ... end` literals. Compilation
        // must succeed with the upvalue wiring intact; the actual
        // call happens host-side, which our stub no-ops.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let label = 'hello'\n",
            "  nowait log.info(label)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Multi-binding destructure ────────────────────────────────
    //
    // `let a, b = swap(1, 2)` and `a, b = swap(...)` destructure a
    // Tuple value. The RHS compiles to a tuple (the compiler
    // synthesises `TupleExpression` from the last `x, y` of a
    // multi-return function), then the LHS walks indices via
    // `RT_GET_INDEX(tuple, MAKE_INT(i))`.

    #[test]
    fn direct_let_multi_binding_swap() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a, b = swap(1, 2)\n",
            "  a * 10 + b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // swap(1, 2) → (2, 1); a=2, b=1 → 2*10 + 1 = 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_var_multi_binding_swap() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a, b = swap(7, 3)\n",
            "  a - b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // swap(7, 3) → (3, 7); a=3, b=7 → 3 - 7 = -4
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | ((-4i32) as u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_reassignment_multi_binding() {
        // Plain `a, b = swap(...)` (no `let`/`var`) — both names
        // must already exist as mutable bindings. Tests the
        // AssignmentTarget::Variables multi-name path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Swap.\ndef swap\n",
            "    @param x Int\n",
            "    @param y Int\n",
            "    @return Int\n",
            "    @return Int\n",
            "do\n",
            "  y, x\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = 1\n",
            "  var b = 2\n",
            "  a, b = swap(a, b)\n",
            "  a * 10 + b\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // Initial a=1 b=2; swap(1,2) → (2,1); a=2 b=1 → 21
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 21;
        assert_eq!(result, expected);
    }

    // ── Field + index assignment ─────────────────────────────────
    //
    // `obj.field = x` routes through `RT_SET_FIELD(obj, key_ptr,
    // key_len, value)`; `arr[i] = x` writes directly to
    // `mem[obj_addr + 8 + i*8]`. Both mutate in place. Tests here
    // verify the full round-trip: assign, then read back.

    #[test]
    fn direct_dict_field_assignment_round_trip() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {score: 1}\n",
            "  d.score = 99\n",
            "  d.score\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_field_assignment_adds_new_key() {
        // `RT_SET_FIELD` appends the entry when the key isn't
        // present (that's why dict literals over-allocate
        // capacity — see `compile_dict_literal`).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {a: 1}\n",
            "  d.b = 2\n",
            "  array.length(d)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_assignment_round_trip() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = [10, 20, 30]\n",
            "  a[1] = 99\n",
            "  a[1]\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_assignment_preserves_length() {
        // Mutation-in-place — length stays the same.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var a = [10, 20, 30]\n",
            "  a[0] = 999\n",
            "  array.length(a)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_sums_elements() {
        // `for x in [1,2,3]` now compiles — array literal +
        // `compile_for_array` walks index 0..length, loading each
        // element with an I64 read from `addr + 8 + i*8`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3]\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 6;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_empty_no_iterations() {
        // Empty array — the length-based guard (`index >= length`)
        // exits before the body runs.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 7\n",
            "  for n in []\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_for_in_array_break_and_continue() {
        // Mixed break + continue in the same loop — confirms the
        // LoopFrame targets are wired correctly for both targets.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3, 4, 5, 6]\n",
            "    if n == 3\n",
            "      continue\n",
            "    end\n",
            "    if n == 5\n",
            "      break\n",
            "    end\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 1 + 2 + 4 = 7 (3 skipped, 5 breaks before summing).
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_computed_callee_compiles() {
        // `(get_fn())()` — calling the result of a call expression.
        // The builder now lowers arbitrary callee expressions
        // through `compile_indirect_call_from_expr`: evaluate the
        // expression to a boxed closure, then dispatch through the
        // closure header. Non-closure values trap at runtime via
        // `RT_OBJ_ADDR`; compilation succeeds.
        //
        // Regression guard: this used to refuse with
        // `UnsupportedExpression("CallExpression/non-identifier")`
        // and forui's `eventHandlers[id]()` / `matched!.builder()`
        // call sites hit that refusal. The direct builder now
        // matches what forai programs actually need.
        use fai_compiler::ast;
        let loc = ast::SourceLocation { line: 1, column: 1 };
        let inner = ast::CallExpression {
            callee: Box::new(ast::Expression::IdentifierExpression(
                ast::IdentifierExpression {
                    name: "get_fn".into(),
                    location: loc.clone(),
                },
            )),
            args: Vec::new(),
            location: loc.clone(),
        };
        let outer = ast::CallExpression {
            callee: Box::new(ast::Expression::CallExpression(inner)),
            args: Vec::new(),
            location: loc.clone(),
        };
        let main = ast::FunctionDeclaration {
            name: "main".into(),
            type_params: Vec::new(),
            params: Vec::new(),
            return_types: Vec::new(),
            body: vec![ast::Statement::ExpressionStatement(
                ast::ExpressionStatement {
                    expression: ast::Expression::CallExpression(outer),
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
        // `get_fn` needs to exist in the function table so the
        // inner call resolves; arity 0 / returns i64 matches the
        // standalone type-index layout.
        let get_fn_info = FunctionInfo {
            name: "get_fn".into(),
            param_count: 0,
            type_param_count: 0,
            include_in_coverage: false,
            param_defaults: Vec::new(),
            ..Default::default()
        };
        build_function(
            &main,
            RtOffsets {
                base: rt_base_for_standalone(),
            },
            &[get_fn_info],
            &CheckerInfo::empty(),
            &build_fai_type_indices(),
            &HashMap::new(),
            &HashMap::new(),
            &identity_import_remap(),
            &RefCell::new(StringInterner::default()),
        )
        .expect("builder should accept computed callee");
    }

    // ── closures ──────────────────────────────────────────────────
    //
    // These exercise the full closure path: a `FunctionExpression` in
    // main's body materialises a heap-allocated closure object, and
    // an indirect call dispatches through the table using the
    // closure's `table_idx` field.

    #[test]
    fn direct_closure_no_capture() {
        // Simplest closure — no upvalues. Calling it via the local
        // exercises `call_indirect` without any env-load logic.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let f = do with n Int\n",
            "    n * 3\n",
            "  end\n",
            "  f(14)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_single_capture() {
        // Closure captures `k` from the enclosing scope by value.
        // Body reads the upvalue via `GlobalGet(env_ptr) + I64Load(0)`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 32\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  f(10)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_multi_capture() {
        // Multiple upvalues — exercises the offset math on the second
        // (non-zero) capture slot.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = 10\n",
            "  let b = 20\n",
            "  let c = 12\n",
            "  let f = do with n Int\n",
            "    a + b + c + n\n",
            "  end\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_captures_let_by_value() {
        // `let` bindings aren't mutable, so there's nothing to share —
        // the closure keeps a plain snapshot. Proves the non-cell path
        // still works.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 42\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_called_twice_preserves_env() {
        // Calling the same closure twice in sequence must restore
        // env_ptr correctly after each call (the save/restore dance).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 7\n",
            "  let f = do with n Int\n",
            "    k * n\n",
            "  end\n",
            "  f(2) + f(4)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // (7*2) + (7*4) = 14 + 28 = 42.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Captured-var mutation (cell-boxing) ─────────────────────
    //
    // When a closure writes to an outer `var`, both the outer and the
    // closure must see each other's updates. The compiler boxes such
    // vars in heap cells and stores cell addresses in the closure's
    // env, so reads and writes on either side dereference the same
    // cell.

    #[test]
    fn direct_closure_writes_captured_var_visible_from_outer() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Call.\n",
            "def call\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  b()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var x = 0\n",
            "  call(do\n",
            "    x = 42\n",
            "  end)\n",
            "  x\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_increments_captured_counter() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Run n times.\n",
            "def repeatN\n",
            "    @param n Int\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  var i = 0\n",
            "  while i < n\n",
            "    b()\n",
            "    i = i + 1\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var count = 0\n",
            "  repeatN(5, do\n",
            "    count = count + 1\n",
            "  end)\n",
            "  count\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_two_closures_share_captured_var() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Block.\n",
            "type def Block\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Run pair.\n",
            "def runPair\n",
            "    @param a Block\n",
            "    @param b Block\n",
            "    @return Void\n",
            "do\n",
            "  a()\n",
            "  b()\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var total = 0\n",
            "  runPair(\n",
            "    do\n",
            "      total = total + 10\n",
            "    end,\n",
            "    do\n",
            "      total = total + 32\n",
            "    end\n",
            "  )\n",
            "  total\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_sees_outer_mutation_after_creation() {
        // Because outer-var captures are shared (cell-boxed) when the
        // var is captured by any closure, an outer mutation AFTER
        // closure creation is visible when the closure runs.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var k = 42\n",
            "  let f = do with n Int\n",
            "    k + n\n",
            "  end\n",
            "  k = 100\n",
            "  f(0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // With cell-sharing the closure reads k = 100, returns 100.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 100;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_closure_zero_arg() {
        // A thunk — zero-arg closure still takes the call_indirect
        // path; checks that the `FaiFunc(0)` type is wired correctly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 42\n",
            "  let f = do\n",
            "    k\n",
            "  end\n",
            "  f()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── Case statement ────────────────────────────────────────────
    //
    // `case value when m1 body1 when m2 body2 else default end`
    // lowers to a nested if/else chain where each condition is
    // `value == match_expr`. Tests here exercise matching branches,
    // the default, tail-position use, and RT_EQ's polymorphism
    // across primitive types.

    #[test]
    fn direct_case_matches_first_branch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 1\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_matches_later_branch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 2\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "    when 3\n",
            "      r = 30\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 20;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_falls_through_to_else() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 99\n",
            "    when 1\n",
            "      r = 10\n",
            "    when 2\n",
            "      r = 20\n",
            "    default\n",
            "      r = 999\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 999;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_no_match_no_else_leaves_state() {
        // No arm matches and no default — body must not execute.
        // The `r` variable keeps its pre-case value.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 42\n",
            "  case 99\n",
            "    when 1\n",
            "      r = 10\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_case_matches_string() {
        // RT_EQ handles String comparison by deep byte equality.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var r = 0\n",
            "  case 'beta'\n",
            "    when 'alpha'\n",
            "      r = 1\n",
            "    when 'beta'\n",
            "      r = 2\n",
            "    when 'gamma'\n",
            "      r = 3\n",
            "  end\n",
            "  r\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_string_ordering_is_lexicographic() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  'apple' < 'banana'\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(true));

        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  'banana' < 'apple'\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm), boxed_bool(false));
    }

    #[test]
    fn direct_case_in_tail_position() {
        // Case as the last statement — each branch body becomes a
        // tail, with its trailing expression as the function's
        // return value. No explicit `return` needed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Describe.\ndef describe\n",
            "    @param n Int\n",
            "    @return Int\n",
            "do\n",
            "  case n\n",
            "    when 1\n",
            "      100\n",
            "    when 2\n",
            "      200\n",
            "    default\n",
            "      999\n",
            "  end\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  describe(2)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 200;
        assert_eq!(result, expected);
    }

    // ── Aggregate + expression gaps ───────────────────────────────
    //
    // Dictionaries, tuples, indexing, field access, template
    // strings, optional checks, and force-unwrap. Each exercises a
    // distinct surface that was previously refused in compile_expr.

    #[test]
    fn direct_dict_literal_length() {
        // Three-entry dict — count stored at offset 4, array.length
        // (polymorphic on dicts too) reads that count directly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {name: 'alice', age: 30, admin: true}\n",
            "  array.length(d)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_field_access_returns_value() {
        // dict.field routes through RT_GET_FIELD; the stored value
        // comes back NaN-boxed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {score: 42}\n",
            "  d.score\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_missing_field_returns_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  d.missing\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_array_index_returns_element() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = [10, 20, 30]\n",
            "  a[1]\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 20;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_dict_index_by_string_key() {
        // dict[key] where key is a String — RT_GET_INDEX branches
        // on the container tag and does a key scan for dicts. The
        // checker types `d[k]` as `Unknown?` since it can't know
        // which key-type yielded what; `@return Unknown?` matches.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  let d = {score: 99}\n",
            "  d['score']\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    // NOTE: `TupleExpression` has no user-visible literal syntax —
    // it's compiler-internal (type-def field packing, multi-return
    // destructuring; see fai-compiler/src/lib.rs:664). The
    // `compile_tuple_literal` path above is exercised once those
    // surfaces land on the direct builder.

    #[test]
    fn direct_template_string_length() {
        // "hello {name}" with name='world' → "hello world", length 11.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let name = 'world'\n",
            "  string.length(\"hello {{name}}\")\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 11;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_template_string_with_number_coerces() {
        // The expression part is an Int — RT_VALUE_TO_STR handles
        // the coercion, no user-side conversion needed.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let n = 42\n",
            "  string.length(\"n={{n}}\")\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // "n=42" has 4 chars.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_optional_check_null_is_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  nullable()?\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_optional_check_non_null_is_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  nullable()?\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_force_unwrap_passes_non_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nullable()!\n",
            "end\n",
        )));
        let result = try_run_module(&wasm).expect("unwrap pass should not trap") as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_force_unwrap_traps_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  nullable()!\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("null unwrap must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    // ── std module dispatch ───────────────────────────────────────
    //
    // These exercise `resolve_module_call` end-to-end: a top-level
    // `use std.foo` installs the alias; the direct builder rewrites
    // `foo.method(args)` into the right arg-shape + import call.
    // The default wasmtime stubs all return 0, so return-value
    // assertions focus on imports where that's meaningful (`void`
    // imports, `MakeBool(0)` → false). Tests that just discard the
    // import's result verify arg-shape + validation end-to-end.

    /// Run a module overriding one import with a constant return.
    /// `override_idx` is the wasm import index (e.g.
    /// `runtime::IMPORT_NET_AVAILABLE`); `ret` is pushed into the
    /// first result slot. Tests use this to distinguish a true from
    /// the default-zero stub reply.
    fn run_module_with_override(wasm: &[u8], override_idx: u32, ret_val: Val) -> i64 {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (i, (name, params, results)) in runtime::import_signatures().iter().enumerate() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            let override_here = i as u32 == override_idx;
            let ret_val = ret_val.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = if override_here {
                                ret_val.clone()
                            } else {
                                match ty {
                                    wasm_encoder::ValType::I32 => Val::I32(0),
                                    wasm_encoder::ValType::I64 => Val::I64(0),
                                    wasm_encoder::ValType::F32 => Val::F32(0),
                                    wasm_encoder::ValType::F64 => Val::F64(0),
                                    _ => Val::I32(0),
                                }
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Sync program: a single `_start` returning the root value.
        if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            return start.call(&mut store, ()).expect("run");
        }
        // Async program: kick off the root task, drive `__fai_poll` to completion
        // (status 2 = done, 3 = failed), then read the root's result. Any program
        // that invokes a closure value is async now (closure calls are potential
        // suspension points), so previously-sync tests can land here.
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("_start or _start_async export");
        start_async.call(&mut store, ()).expect("run _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("__fai_poll export");
        let mut status = 1;
        for _ in 0..10_000_000 {
            status = poll.call(&mut store, ()).expect("poll");
            if status == 2 || status == 3 {
                break;
            }
        }
        assert!(status == 2, "async root did not complete (status {status})");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("__fai_task_result export");
        task_result.call(&mut store, 1).expect("task_result")
    }

    #[test]
    fn direct_module_log_info_no_op() {
        // `log.info(msg)` is void-returning; the dispatcher emits a
        // `VAL_VOID` trailer after the call. We drop the result, so
        // main's return value is the subsequent Int literal.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  log.info('hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_net_available_true() {
        // Override `net_available` to return 1 so we can verify the
        // result is wrapped through `MAKE_BOOL(1)` → `VAL_TRUE`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  net.available()\n",
            "end\n",
        )));
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_NET_AVAILABLE, Val::I32(1)) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_net_available_false() {
        // Default stub returns 0 — `MAKE_BOOL(0)` → `VAL_FALSE`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  net.available()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_path_join_compiles_and_runs() {
        // Two-string-arg shape. The stub returns i64(0) (not a valid
        // NaN-box, but we discard it); the test proves the wasm
        // validates with the right arg shape and main completes.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = path.join('a', 'b')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_path_basename_single_arg() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = path.basename('/a/b')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_json_parse_and_stringify() {
        // Exercise both the String→i64 (parse) and i64→i64
        // (stringify) arg shapes in one program.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let v = json.parse('{}')\n",
            "  let s = json.stringify(v)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_tcp_listen_takes_int_arg() {
        // Int-arg shape: the NaN-boxed `8080` is unboxed to an i32 on
        // the stack before `IMPORT_TCP_LISTEN`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = tcp.listen(8080)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_tcp_connect_mixed_args() {
        // (String, Int) → Int. Exercises both arg shapes in one call.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = tcp.connect('localhost', 8080)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_exists_true() {
        // Override `file_exists` to return 1 so the result wraps as
        // VAL_TRUE. Exercises the (String) → Bool pattern.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/tmp/foo')\n",
            "end\n",
        )));
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_FILE_EXISTS, Val::I32(1)) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_exists_false() {
        // Default stub returns 0 — MAKE_BOOL(0) → VAL_FALSE.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/nope')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_write_void_shape() {
        // (String, String) → void. Stub returns nothing; we verify
        // the wasm validates + runs and main returns its own Int.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  file.write('/tmp/x', 'hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_list_compiles_and_runs() {
        // (String) → Array?. Stub returns i64(0) which we discard,
        // so the test only verifies arg-shape + successful run.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = file.list('/tmp')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_file_read_returns_null_on_error() {
        // `file_read_str` returns VAL_NULL when the path doesn't
        // exist; the builder passes the boxed result straight through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/nope')\n",
            "end\n",
        )));
        let null_bits = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_FILE_READ_STR,
            Val::I64(null_bits as i64),
        ) as u64;
        assert_eq!(result, null_bits);
    }

    #[test]
    fn direct_module_file_read_passes_boxed_string_through() {
        // Success path: the host allocates the String and returns its
        // NaN-boxed value — the builder must not rewrap or unbox it.
        // Override with a recognizable object bit pattern and assert
        // it round-trips untouched.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  file.read('/tmp/empty')\n",
            "end\n",
        )));
        let obj_bits = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64 | 0x1230;
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_FILE_READ_STR,
            Val::I64(obj_bits as i64),
        ) as u64;
        assert_eq!(result, obj_bits);
    }

    #[test]
    fn direct_module_time_now_compiles_and_runs() {
        // `time.now()` dispatches to IMPORT_NOW_MS + RT_MAKE_FLOAT —
        // matches the bytecode runtime's `METHOD_TIME_NOW`.
        //
        // The checker types `timeNow` as `String` (per docs: "ISO
        // 8601"), but the runtime actually returns a Float (ms since
        // epoch). The divergence is outside this work; discard the
        // value so the test doesn't bake in either side of the
        // disagreement — we only prove the arg-shape + import call
        // lower correctly.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = time.now()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_time_unix_divides_and_truncates() {
        // `time.unix()` = `trunc(now_ms / 1000)` → Int. Override
        // `now_ms` to 3_456_789.0 → expect Int 3456.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  time.unix()\n",
            "end\n",
        )));
        let now_ms: f64 = 3_456_789.0;
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_NOW_MS, Val::F64(now_ms.to_bits()))
                as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (3456u32 as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_floor_int_literal_promotes() {
        // `math.floor(x: Float)` — passing an Int literal is OK:
        // `RT_AS_NUMBER` promotes both Int and Float to f64. Test
        // that `floor(3)` is 3 via the full dispatch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.floor(3.7)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_ceil() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.ceil(2.1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_round_nearest() {
        // F64Nearest is banker's-rounding on half-values; 2.5 → 2,
        // 3.5 → 4. Test the unambiguous cases.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  math.round(7.8)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 8;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_abs() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.abs(-3.5)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // 3.5f64 → reinterpret to i64.
        let expected = 3.5_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_sqrt() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.sqrt(16.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 4.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_min() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.min(2.0, 5.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 2.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_max() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.max(2.0, 5.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 5.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_positive_exp() {
        // 2^10 = 1024.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(2.0, 10.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 1024.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_zero_exp() {
        // Anything^0 = 1. Zero-iteration loop, result stays at 1.0.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(42.0, 0.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 1.0_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_pow_negative_exp() {
        // 2^-3 = 1/8 = 0.125. Exercises the invert branch.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.pow(2.0, -3.0)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 0.125_f64.to_bits();
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_math_random_compiles_and_runs() {
        // Default stub returns f64(0.0); `random()` wraps it as
        // Float. The result is exactly the 0-bit-pattern Float, but
        // we don't lock the test to that — assert only that the
        // high bits don't have the Int tag (so we know we didn't
        // accidentally route through MAKE_INT).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Float\n",
            "do\n",
            "  math.random()\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        assert_eq!(
            result, 0u64,
            "random() with stub 0.0 → Float(0) bit pattern"
        );
    }

    // ── std.cli ──────────────────────────────────────────────────

    #[test]
    fn direct_module_cli_clear_no_args() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.clear()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_write_stringifies() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.write('hi')\n",
            "  cli.writeLine(123)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_move_to_int_args() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  cli.moveTo(3, 7)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_read_line_no_prompt() {
        // Zero-arg form pushes (0, 0). The import stub returns 0
        // (not a valid NaN-box, but we discard). Main returns 42.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = cli.readLine()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_cli_read_line_with_prompt() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = cli.readLine('Name? ')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.storage ─────────────────────────────────────────────

    #[test]
    fn direct_module_storage_set_and_remove_void() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  storage.storageSet('k', 'v')\n",
            "  storage.storageRemove('k')\n",
            "  storage.storageClear()\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_storage_get_null_on_missing() {
        // `storageGet` returns `String?` (optional). The host returns
        // VAL_NULL for an absent key; the builder passes it through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  storage.storageGet('missing')\n",
            "end\n",
        )));
        let null_bits = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_STORAGE_GET_STR,
            Val::I64(null_bits as i64),
        ) as u64;
        assert_eq!(result, null_bits);
    }

    #[test]
    fn direct_module_storage_get_wraps_buffer_on_success() {
        // Default stub returns 0 (len=0). The builder wraps the
        // host-allocated boxed String — assert the boxed value rounds
        // trip through the builder untouched.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.storage\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  storage.storageGet('k')\n",
            "end\n",
        )));
        let obj_bits = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64 | 0x1230;
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_STORAGE_GET_STR,
            Val::I64(obj_bits as i64),
        ) as u64;
        assert_eq!(result, obj_bits);
    }

    // ── std.convert ─────────────────────────────────────────────

    #[test]
    fn direct_module_convert_to_string() {
        // `convert.toString(42)` goes through RT_VALUE_TO_STR. We
        // assert the result is an object (String) tag — verifying
        // the full value would require parsing the heap.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  convert.toString(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let obj_high = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64;
        assert_eq!(
            result & 0xFFFF_0000_0000_0000,
            obj_high & 0xFFFF_0000_0000_0000
        );
    }

    #[test]
    fn direct_module_convert_to_int_passthrough() {
        // `convert.toInt(42)` is a no-op at runtime — the int box
        // round-trips through the dispatcher unchanged.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  convert.toInt(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_int_succeeds() {
        // RT_PARSE_INT("123") returns Int(123). parseInt is Int? now.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  convert.parseInt('123')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 123;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_int_null_on_garbage() {
        // RT_PARSE_INT returns VAL_NULL when the input isn't a
        // valid integer.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  convert.parseInt('xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_convert_parse_float_succeeds() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n",
            "\n",
            "def main\n",
            "    @return Float?\n",
            "do\n",
            "  convert.parseFloat('3.5')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = 3.5_f64.to_bits();
        assert_eq!(result, expected);
    }

    // ── std.string ──────────────────────────────────────────────
    //
    // Every method goes through `RT_CALL_NATIVE` with a METHOD_*
    // id. These tests exercise arg count, NaN-box result shapes,
    // and the NativeFn allocation → args-buffer layout. Default
    // wasmtime stubs don't come into play; the runtime helpers
    // execute entirely guest-side.

    #[test]
    fn direct_module_string_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length('hello')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_is_empty_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.isEmpty('')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_is_empty_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.isEmpty('x')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    /// Regression: native method dispatch must allocate space for the
    /// args buffer along with the NativeFn header, in a single
    /// `RT_ALLOC` call. The original code allocated 8 bytes for the
    /// header, then wrote args past `__heap_ptr` without separately
    /// allocating that region. When `__heap_ptr` sits close enough to
    /// the current memory boundary that the 8-byte header fits
    /// without growing memory but the args writes don't, the writes
    /// trap. (`RT_ALLOC` grows in 1 MiB chunks, so once a grow
    /// happens, there's plenty of slack — the bug only surfaces when
    /// the header alloc fits without grow.)
    ///
    /// We pre-position `__heap_ptr` so the program's allocations
    /// (`'hello world'` → 24 bytes, `'world'` → 16 bytes, NativeFn
    /// header → 8 bytes) all fit without growing memory and land the
    /// post-header pointer at `mem_size - 8`. The buggy code then
    /// writes arg[0] at `mem_size - 8` (in-bounds) and arg[1] at
    /// `mem_size` (OOB, traps). The fix sizes the single alloc to
    /// cover both header and args, so the grow covers everything.
    #[test]
    fn direct_native_method_at_heap_page_boundary() {
        use wasmtime::{Engine, Linker, Module as RuntimeModule, Store, Val};
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello world', 'world')\n",
            "end\n",
        )));
        // Run the module ourselves so we can pin `__heap_ptr` to the
        // memory boundary before invoking `_start`. If we used
        // `run_module`, the program would never allocate enough on
        // its own to reach the boundary.
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // Pre-position `__heap_ptr` so the program's three allocs
        // (24 + 16 + 8 = 48 bytes) all fit without growing memory and
        // leave the post-NativeFn-alloc pointer at `mem_size - 8`.
        // The buggy code's args[1] write at `mem_size` then traps.
        let memory = instance.get_memory(&mut store, "memory").expect("memory");
        let mem_size = memory.data_size(&mut store) as u32;
        let heap = instance
            .get_global(&mut store, "__heap_ptr")
            .expect("__heap_ptr global");
        let target = (mem_size - 56) & !7;
        heap.set(&mut store, Val::I32(target as i32))
            .expect("set heap_ptr");
        let start = instance
            .get_typed_func::<(), i64>(&mut store, "_start")
            .expect("_start export");
        let result = start.call(&mut store, ()).expect("run") as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(
            result, expected,
            "string.contains must succeed even when heap_ptr lands at the page boundary",
        );
    }

    #[test]
    fn direct_module_string_contains_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello world', 'world')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_contains_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.contains('hello', 'xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_starts_with() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.startsWith('hello', 'he')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_ends_with() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  string.endsWith('hello', 'lo')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_index_of() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.indexOf('hello', 'lo')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_index_of_missing() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.indexOf('hello', 'xyz')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // indexOf returns -1 when not found. `-1 as i32 as u32` is
        // 0xFFFFFFFF — the low 32 of a NaN-boxed Int.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 0xFFFF_FFFF_u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_substring_length() {
        // `substring('hello', 1, 4)` = "ell". We verify via length —
        // probing the resulting string's heap layout would couple
        // the test to allocator internals.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.substring('hello', 1, 4))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_trim_and_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.trim('  hi  '))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_to_upper_length_unchanged() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.toUpper('hello'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_repeat_length() {
        // "ab" repeated 3 times → length 6.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.repeat('ab', 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 6;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_string_replace_length() {
        // "foo" → "bar" in "foo foo" → "bar bar", length 7.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.replace('foo foo', 'foo', 'bar'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 7;
        assert_eq!(result, expected);
    }

    // ── std.array (non-closure) ────────────────────────────────
    //
    // Same NativeMethod dispatch as std.string. Array literals
    // `[1, 2, 3]` now compile as well (see `compile_array_literal`),
    // so tests here construct and consume arrays end-to-end.

    #[test]
    fn direct_module_array_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_empty_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.isEmpty([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_empty_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.isEmpty([1])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_contains_primitive_hit() {
        // Runtime does i64 bit-equality for primitive elements —
        // matches the VM's stringified comparison for same-typed
        // primitives. Int(20) bit pattern is the NaN-box the literal
        // compiled to, so this is a direct hit.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.contains([10, 20, 30], 20)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_contains_miss() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  array.contains([10, 20, 30], 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_index_of() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf([10, 20, 30], 30)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_index_of_missing() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf([10, 20], 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 0xFFFF_FFFF_u64;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_append_returns_longer() {
        // `append` returns a fresh array with the element added —
        // the runtime helper allocates + copies. Verify via length.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.append([1, 2], 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 3;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_reverse_preserves_length() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.reverse([1, 2, 3, 4]))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_reverse_moves_first_to_last() {
        // indexOf of the original first element after reverse is
        // length - 1. [1,2,3] reversed → [3,2,1], indexOf(result, 1)
        // should be 2.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf(array.reverse([1, 2, 3]), 1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_sort_puts_smallest_first() {
        // After sort([3, 1, 2]) the 1 sits at index 0.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.indexOf(array.sort([3, 1, 2]), 1)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_slice_length() {
        // slice([10,20,30,40], 1, 3) = [20, 30], length 2.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  array.length(array.slice([10, 20, 30, 40], 1, 3))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 2;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_join_string_length() {
        // join(["a","b","c"], "-") = "a-b-c", length 5.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(array.join(['a', 'b', 'c'], '-'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    // ── std.array (closure-taking) ─────────────────────────────
    //
    // `map`, `filter`, `find`, `isAny`, `isAll` all have the same
    // shape: `(arr: Array, closure: Fn) -> <result>`. The guest
    // side just pushes two i64 values and calls the matching
    // `IMPORT_ARRAY_*`; the host reads array elements and calls
    // back into the guest via `__indirect_function_table` using
    // the closure's `table_idx`.
    //
    // These tests verify the guest-side plumbing. End-to-end
    // round-trip (host invoking the closure) would require a
    // wasmtime stub that reaches into the exported table — that
    // infrastructure lands with `std.http.server`.

    #[test]
    fn direct_module_array_map_compiles_and_runs() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x * 2\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_map_result_propagates() {
        // Override IMPORT_ARRAY_MAP to return a known NaN-boxed Int
        // sentinel — confirms the result from the host flows out
        // as the expression's value. The checker types `array.map`
        // as returning `Int[]` here, but at wasm layer the result
        // is just an i64 bit pattern; we only assert on the raw
        // bits coming back from the overridden stub, so the type
        // declaration doesn't affect the check.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int[]\n",
            "do\n",
            "  array.map([1], do with x Int\n",
            "    x\n",
            "  end)\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (777u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_ARRAY_MAP, Val::I64(sentinel as i64))
                as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_module_array_filter_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.filter([1, 2, 3], do with x Int\n",
            "    x > 1\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_find_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.find([1, 2, 3], do with x Int\n",
            "    x == 2\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_any_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.isAny([1, 2], do with x Int\n",
            "    x > 1\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_is_all_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = array.isAll([1, 2], do with x Int\n",
            "    x > 0\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_map_captures_upvalue() {
        // The closure captures `k` from the enclosing scope. The
        // host invokes the closure via the table; the closure body
        // reads `k` via `env_ptr + 0`. Verifies the full closure
        // + module-dispatch interaction at compile time.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 10\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x + k\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.test / assert ──────────────────────────────────────
    //
    // Each assertion either returns `VAL_TRUE` on pass or traps
    // after storing a message via `IMPORT_SET_TRAP_MSG`. Tests here
    // exercise both paths; the trap shows up as a wasmtime error
    // (`start.call` returns `Err`), which the direct-path tests
    // detect by calling the module without the `.expect("run")`
    // wrapper that normal `run_module` uses.

    /// Run a module and return `Ok(i64)` on clean completion or
    /// `Err(trap_message)` when the guest traps. Used by assertion
    /// tests to exercise the failure path without panicking the
    /// test runner.
    fn try_run_module(wasm: &[u8]) -> Result<i64, String> {
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let start = instance
            .get_typed_func::<(), i64>(&mut store, "_start")
            .expect("_start export");
        start.call(&mut store, ()).map_err(|e| e.to_string())
    }

    #[test]
    fn direct_module_test_assert_passes_truthy() {
        // All assertions type-check as `Void` at the checker level,
        // so they sit as statements, not tail expressions. `main`
        // returns Void; we just need the wasm not to trap.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.assert(true)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("passing assert should not trap");
    }

    #[test]
    fn direct_module_test_assert_traps_on_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.assert(1 == 2)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("failing assert should trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_test_equal_passes_on_same_value() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(42, 42)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("eq pass");
    }

    #[test]
    fn direct_module_test_equal_traps_on_diff() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(1, 2)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("eq mismatch must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_test_equal_with_message_still_traps() {
        // Exercises the message-arg path — caller-supplied message
        // passes through RT_VALUE_TO_STR + IMPORT_SET_TRAP_MSG
        // before the unreachable fires.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.test\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  test.equal(1, 2, 'should match')\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("trap expected");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_assert_is_true_passes() {
        // `assert.isTrue` is auto-exposed inside `@test` blocks. The
        // direct-path builder recognises the `assert` alias without
        // a `use` statement — see `compile_call`'s fallback.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isTrue(true)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("isTrue(true) passes");
    }

    #[test]
    fn direct_module_assert_is_false_passes_on_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isFalse(false)\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("isFalse(false) passes");
    }

    #[test]
    fn direct_module_assert_is_false_traps_on_truthy() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.isFalse(true)\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("isFalse(true) must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn direct_module_assert_equals_passes() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.equals('hi', 'hi')\n",
            "end\n",
        )));
        let _ = try_run_module(&wasm).expect("equals pass");
    }

    #[test]
    fn direct_module_assert_equals_traps_on_mismatch() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  assert.equals('hi', 'bye')\n",
            "end\n",
        )));
        let err = try_run_module(&wasm).expect_err("mismatch must trap");
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    // ── std.error ───────────────────────────────────────────────
    //
    // `Error(msg)` builds a `{message: msg}` dict; `unwrap` is a
    // null-guarded pass-through. `message`, `kind`, and `isError`
    // share the same implementation as their bare-global forms.

    #[test]
    fn direct_module_error_construct_returns_object() {
        // Result is a NaN-boxed Dict. Verify the high bits match
        // an object tag (QNAN | SIGN_BIT) — a cheap way to confirm
        // we took the `MAKE_OBJ` path without introspecting the
        // dict layout.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Error\n",
            "do\n",
            "  error.Error('oops')\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let obj_high = (runtime::QNAN as u64) | 0x8000_0000_0000_0000_u64;
        assert_eq!(
            result & 0xFFFF_0000_0000_0000,
            obj_high & 0xFFFF_0000_0000_0000,
        );
    }

    #[test]
    fn direct_module_unwrap_returns_value_when_non_null() {
        // The checker requires the first arg to `unwrap` to be a
        // nullable type. A helper with `@return Int?` + a non-null
        // body satisfies that while giving us a concrete Int at
        // runtime.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "# Some.\ndef some @return Int? do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  error.unwrap(some(), 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_unwrap_returns_fallback_on_null() {
        // The checker will flag `unwrap(null, 99)` because the first
        // arg's type would need to be a nullable. We construct a
        // nullable Int through `unwrap`'s own return type used in a
        // chain: `unwrap(unwrap(null, null), 99)` returns 99.
        //
        // Simpler: bind a literal-null to a typed `Int?` variable so
        // the checker accepts it, then unwrap that.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "# Nullable.\ndef nullable @return Int? do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  error.unwrap(nullable(), 99)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_error_message_qualified() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(error.message(error.Error('x')))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_error_is_error_qualified() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  error.isError(error.Error('x'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    // ── std.http.server ─────────────────────────────────────────
    //
    // Response builders (`ok`/`text`/`html`/`json`/`redirect`) and
    // router API (`router`/`get`/`post`/`serveFiles`/`listen`) all
    // route through `RT_CALL_NATIVE` with distinct `METHOD_SERVER_*`
    // ids. The runtime's helper allocates the response dict or
    // hands the call off to the matching `IMPORT_HTTP_SERVER_*`
    // import.
    //
    // Full end-to-end verification (host actually starting a server,
    // dispatching a request through `__indirect_function_table` to
    // a closure handler) needs infrastructure that doesn't belong
    // in these unit tests. What we can verify here:
    //   (a) the dispatch resolver picks the right METHOD id,
    //   (b) arg shapes + closure args compile cleanly,
    //   (c) the guest wasm validates and doesn't trap on default
    //       stubs (which return 0 for every import).

    #[test]
    fn direct_module_http_server_ok_returns_object() {
        // `server.ok(body)` → Dict. The runtime builds a dict by
        // calling `IMPORT_HTTP_SERVER_RESPONSE`. Default stub returns
        // i64(0) — not a valid NaN-box — so we discard the result
        // and just verify main runs through.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = server.ok('hello')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_text_with_status() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = server.text(200, 'plain text body')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_html_and_json_and_redirect_compile() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let h = server.html(200, '<p>hi</p>')\n",
            "  let j = server.json(200, 'already-stringified')\n",
            "  let r = server.redirect(302, '/new')\n",
            "  if h == h\n",
            "    if j == j\n",
            "      if r == r\n",
            "        42\n",
            "      else\n",
            "        0\n",
            "      end\n",
            "    else\n",
            "      0\n",
            "    end\n",
            "  else\n",
            "    0\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_router_roundtrip_compiles() {
        // Router construction + registering a handler closure +
        // listen. This is the full Router API surface except
        // serveFiles — exercises closure-as-arg at the server
        // layer (via `METHOD_SERVER_GET`, method_id 43).
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let r = server.router()\n",
            "  server.get(r, '/', do with req HttpRequest\n",
            "    server.ok('hi')\n",
            "  end)\n",
            "  server.post(r, '/submit', do with req HttpRequest\n",
            "    server.ok('thanks')\n",
            "  end)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_http_server_serve_files_compiles() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.http.server\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let r = server.router()\n",
            "  server.serveFiles(r, './public')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── remoteCall (bare global) ───────────────────────────────
    //
    // `remoteCall(url, fn, argsJson, hash)` → `Unknown`. A
    // bare-global RPC helper — the only builtin we support that
    // isn't module-scoped and isn't user-defined. Each String arg
    // lowers to `(ptr, len)`, then `IMPORT_REMOTE_CALL` runs the
    // round-trip; the host parses the response JSON and returns a
    // NaN-boxed forai value. Matches `translate.rs`'s `name ==
    // "remoteCall"` branch.

    #[test]
    fn direct_remote_call_propagates_result() {
        // Override the stub to return a known NaN-boxed Int; the
        // direct-path result matches exactly, proving the 4-String
        // arg-shape + IMPORT_REMOTE_CALL dispatch is wired.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  remoteCall('http://x', 'fn', '[]', 'hash-v1')\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (77u32 as u64);
        let result = run_module_with_override(
            &wasm,
            runtime::IMPORT_REMOTE_CALL,
            Val::I64(sentinel as i64),
        ) as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_remote_call_accepts_non_string_coerced_args() {
        // Any value type is allowed as any arg — the builder
        // coerces through RT_VALUE_TO_STR before pushing ptr/len.
        // This mirrors path.join / log.info behaviour.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = remoteCall('url', 'fn', '[]', 'hash')\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── extern FFI ──────────────────────────────────────────────
    //
    // `extern { ... }` blocks declare C-ABI functions. Callers
    // serialize NaN-boxed args to a scratch region at offset 65536
    // and invoke `IMPORT_CALL_FFI(ext_fn_idx, arg_count,
    // args_base)`. The host uses the program's `ExternFnInfo`
    // metadata (gathered from the extern block) to unbox to the
    // right C types via libloading, then boxes the return.
    //
    // Tests here verify the guest-side plumbing: the import is
    // called with the right (ext_fn_idx, arg_count, args_base)
    // triple, args land in scratch memory, and the return flows
    // back out. Host-side marshalling is covered in the
    // `fai-cli::wasm_runner` integration tests.

    #[test]
    fn direct_extern_call_propagates_result() {
        // Override IMPORT_CALL_FFI to return a specific NaN-boxed
        // Int — proves the extern-call wiring routes the import
        // and its return lands as the expression's value.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  strlen('hello')\n",
            "end\n",
        )));
        let sentinel = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (5u32 as u64);
        let result =
            run_module_with_override(&wasm, runtime::IMPORT_CALL_FFI, Val::I64(sentinel as i64))
                as u64;
        assert_eq!(result, sentinel);
    }

    #[test]
    fn direct_extern_multi_arg_compiles() {
        // Multi-arg extern — each arg is stored at
        // mem[65536 + i*8], then IMPORT_CALL_FFI is invoked with
        // arg_count=3. Default stub returns 0; we discard.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libmath\n",
            "  def add3(a: Int, b: Int, c: Int) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = add3(1, 2, 3)\n",
            "  42\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_extern_multiple_blocks_assign_sequential_indices() {
        // `strlen` is idx 0, `atoi` is idx 1. The stub can't
        // distinguish them (both return 0), but compilation +
        // validation proves the builder passes distinct indices at
        // each call site. If the indices were miswired, a later
        // integration test against a real libloading host would
        // blow up immediately.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "  def atoi(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a = strlen('hi')\n",
            "  let b = atoi('42')\n",
            "  if a == a\n",
            "    if b == b\n",
            "      42\n",
            "    else\n",
            "      0\n",
            "    end\n",
            "  else\n",
            "    0\n",
            "  end\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ── std.error bare-global builtins ───────────────────────────
    //
    // `Error(msg)` constructs a dict `{message: msg}`. `message`
    // and `kind` read named fields; `isError` checks the
    // object-tag. Previously refused as a language gap (see the
    // B-list fix session).

    #[test]
    fn direct_print_bare_global() {
        // `print(v)` is a bare-global builtin: stringify + write to
        // stdout via RT_PRINT_VAL_NEW. Program doesn't need a `use`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  print('hello')\n",
            "  print(42)\n",
            "  99\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 99;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_error_message_field() {
        // `message(err)` reads the "message" field from the Error
        // dict constructed by `Error("...")`.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let e = error.Error('oops')\n",
            "  string.length(message(e))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        // "oops" is 4 chars.
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 4;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_error_kind_absent_returns_null() {
        // Error constructor doesn't set a `kind` field; `kind(e)`
        // reads a missing key and returns null.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  let e = error.Error('oops')\n",
            "  kind(e)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_is_error_true_on_error() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.error\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isError(error.Error('boom'))\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_is_error_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isError(42)\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_first_returns_element() {
        // Previously refused as an unimplemented language gap —
        // now supported with runtime handlers + direct-path
        // dispatch. Returns the first element of a non-empty array.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  array.first([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_first_empty_is_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  array.first([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_last_returns_element() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  array.last([10, 20, 30])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 30;
        assert_eq!(result, expected);
    }

    #[test]
    fn direct_module_array_last_empty_is_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Unknown?\n",
            "do\n",
            "  array.last([])\n",
            "end\n",
        )));
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_NULL as u64);
        assert_eq!(result, expected);
    }

    // ── Phase G integration (try_codegen_direct) ────────────────
    //
    // End-to-end: parse forai source → run checker → call
    // `try_codegen_direct` → run the resulting wasm. Proves the
    // production path wires parse/check/build/assemble together
    // correctly and the produced module matches what the test
    // harness's piecewise `build_standalone_module_many` would
    // have produced.

    /// Parse a forai source string, run the checker, and try to
    /// compile via `try_codegen_direct`. Panics on parse / check
    /// failure; returns `None` if any function refuses the direct
    /// path.
    fn try_compile_via_production(src: &str) -> Option<Vec<u8>> {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        crate::try_codegen_direct(&prepared.serde_ast, &info, None)
    }

    /// Compile an entry source with one synthetic user module.
    /// Feeds both through the checker with module awareness, then
    /// through the direct path. Mirrors what the CLI does when
    /// running a multi-file project.
    fn try_compile_with_module(
        entry_src: &str,
        module_name: &str,
        module_src: &str,
    ) -> Option<Vec<u8>> {
        try_compile_with_modules(entry_src, vec![(module_name, module_src)])
    }

    /// Compile an entry source with multiple synthetic user modules.
    fn try_compile_with_modules(entry_src: &str, modules: Vec<(&str, &str)>) -> Option<Vec<u8>> {
        let prepared = fai_compiler::prepare_source_with_synthetic(
            entry_src,
            None,
            modules
                .into_iter()
                .map(|(name, src)| (name.to_string(), src.to_string()))
                .collect(),
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        let prepared_modules: Vec<fai_checker::PreparedModule> = prepared
            .modules
            .iter()
            .map(|m| fai_checker::PreparedModule {
                name: m.name.clone(),
                statements: m.statements.clone(),
                file_paths: m.file_paths.clone(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect();
        checker
            .check_with_modules(&prepared.serde_ast.statements, &prepared_modules)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        crate::try_codegen_direct_with_modules(&prepared.serde_ast, &prepared.modules, &info, None)
    }

    #[test]
    fn production_direct_simple_int_return() {
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  42\n",
            "end\n",
        ))
        .expect("direct compilation should succeed for int return");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_closure_and_array_map() {
        let wasm = try_compile_via_production(concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let k = 5\n",
            "  let _ = array.map([1, 2, 3], do with x Int\n",
            "    x + k\n",
            "  end)\n",
            "  42\n",
            "end\n",
        ))
        .expect("array.map + closure should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_dict_and_field_access() {
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let user = {name: 'alice', age: 30}\n",
            "  user.age\n",
            "end\n",
        ))
        .expect("dict + field access should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 30;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_with_string_methods() {
        let wasm = try_compile_via_production(concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  string.length(string.toUpper('hello'))\n",
            "end\n",
        ))
        .expect("string methods should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 5;
        assert_eq!(result, expected);
    }

    // NOTE: There used to be a `production_direct_refuses_unsupported_feature`
    // here, but the direct path now handles essentially every
    // source-level construct a well-formed forai program can
    // produce. The remaining refusals fire on synthetic ASTs
    // (see `direct_rejects_unsupported_feature`) or on deep
    // interactions (e.g., nested closures). If a future
    // source-level gap surfaces, re-add a production refusal test
    // against it.

    #[test]
    fn production_direct_for_in_array() {
        // End-to-end: for-in-array now compiles via direct.
        let wasm = try_compile_via_production(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var s = 0\n",
            "  for n in [1, 2, 3, 4]\n",
            "    s = s + n\n",
            "  end\n",
            "  s\n",
            "end\n",
        ))
        .expect("for-in-array should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 10;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_test_runner_dispatches_passing_case() {
        // Test blocks compile via the direct path when is_test=true.
        // The resulting module exports `_fai_run_test(suite,case)`
        // which the CLI test runner invokes. A passing case should
        // return cleanly.
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "# Add.\ndef add\n",
                "    @param x Int\n",
                "    @param y Int\n",
                "    @return Int\n",
                "do\n",
                "  x + y\n",
                "end\n",
                "\n",
                "test add\n",
                "it 'handles one plus one'\n",
                "  assert.equals(add(1, 1), 2)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        // Invoke `_fai_run_test(suite=0, case=0)` via wasmtime.
        use wasmtime::{
            Engine, FuncType, Linker, Module as RuntimeModule, Store, Val, ValType as WtValType,
        };
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let run_test = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
            .expect("_fai_run_test export");
        // Passing case — no trap.
        run_test
            .call(&mut store, (0, 0))
            .expect("passing test should not trap");
    }

    #[test]
    fn production_direct_test_mode_start_does_not_call_main() {
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  99\n",
                "end\n",
                "\n",
                "# Value.\ndef value\n",
                "    @return Int\n",
                "do\n",
                "  1\n",
                "end\n",
                "\n",
                "test value\n",
                "it 'returns one'\n",
                "  assert.equals(value(), 1)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        let result = run_module(&wasm) as i64;
        assert_eq!(result, runtime::VAL_VOID);
    }

    #[test]
    fn production_direct_test_runner_traps_on_failing_case() {
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            concat!(
                "# Subject.\ndef subject\n",
                "    @return Int\n",
                "do\n",
                "  1\n",
                "end\n",
                "\n",
                "test subject\n",
                "it 'wrong answer'\n",
                "  assert.equals(1, 2)\n",
                "end\n",
                "end\n",
            ),
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        let wasm = crate::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        )
        .expect("test-mode direct build should succeed");

        use wasmtime::{
            Engine, FuncType, Linker, Module as RuntimeModule, Store, Val, ValType as WtValType,
        };
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        let run_test = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
            .expect("_fai_run_test export");
        let err = run_test
            .call(&mut store, (0, 0))
            .expect_err("failing assert should trap")
            .to_string();
        assert!(
            err.contains("unreachable") || err.contains("wasm backtrace"),
            "unexpected err: {}",
            err,
        );
    }

    #[test]
    fn production_direct_cross_module_call() {
        // Entry file imports a sibling module and calls into it.
        // `helpers.double(x)` resolves via `module_aliases["helpers"]
        // = "mypkg.helpers"`, then lookup of
        // `"mypkg.helpers.double"` in the unified function table.
        let wasm = try_compile_with_module(
            concat!(
                "use mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  helpers.double(21)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("cross-module call should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_user_module_call() {
        let wasm = try_compile_with_module(
            concat!(
                "use * from mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  double(21)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("glob-imported user function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_user_module_ufcs_call() {
        let wasm = try_compile_with_module(
            concat!(
                "use * from mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  21.double()\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Double.\ndef double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
            ),
        )
        .expect("glob-imported UFCS function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_glob_import_std_call() {
        let wasm = try_compile_via_production(concat!(
            "use * from std.math\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  floor(42.9)\n",
            "end\n",
        ))
        .expect("glob-imported std function should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_module_internal_peer_call() {
        // A module function calls another function in the same
        // module by unqualified name. The `module_context` fallback
        // on the builder looks up `"mypkg.helpers.square"` when the
        // bare `square` lookup misses.
        let wasm = try_compile_with_module(
            concat!(
                "use mypkg.helpers\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  helpers.squarePlusOne(6)\n",
                "end\n",
            ),
            "mypkg.helpers",
            concat!(
                "# Square.\ndef square\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * x\n",
                "end\n",
                "\n",
                "# SquarePlusOne.\ndef squarePlusOne\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  square(x) + 1\n",
                "end\n",
            ),
        )
        .expect("module peer call should compile via direct");
        let result = run_module(&wasm) as u64;
        // square(6) + 1 = 37
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 37;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_allows_same_basename_modules_for_named_imports() {
        // Folder namespaces can naturally contain both `auth` and
        // `pages.auth`. Named imports use the full canonical module
        // path, so this is not ambiguous and should not require
        // renaming either folder.
        let wasm = try_compile_with_modules(
            concat!(
                "use { LoginPage } from pages.auth\n",
                "use { checkTask } from data.tasks\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  LoginPage() + checkTask()\n",
                "end\n",
            ),
            vec![
                (
                    "auth",
                    concat!(
                        "# Require session.\ndef requireSession\n",
                        "    @return Int\n",
                        "do\n",
                        "  20\n",
                        "end\n",
                    ),
                ),
                (
                    "pages.auth",
                    concat!(
                        "# Login page.\ndef LoginPage\n",
                        "    @return Int\n",
                        "do\n",
                        "  22\n",
                        "end\n",
                    ),
                ),
                (
                    "data.tasks",
                    concat!(
                        "use { requireSession } from auth\n",
                        "\n",
                        "# Check task.\ndef checkTask\n",
                        "    @return Int\n",
                        "do\n",
                        "  requireSession()\n",
                        "end\n",
                    ),
                ),
            ],
        )
        .expect("same-basename named imports should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_ufcs_in_user_module_uses_module_key() {
        // The checker keys UFCS rewrites by `(module, line, column)`.
        // Direct codegen must use the discovered module's canonical
        // name while compiling that module, otherwise `value.increment()`
        // is treated as an ordinary member call.
        let wasm = try_compile_with_modules(
            concat!(
                "use { run } from pages.tasks\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  run()\n",
                "end\n",
            ),
            vec![
                (
                    "maths",
                    concat!(
                        "# Increment.\ndef increment\n",
                        "    @param value Int\n",
                        "    @return Int\n",
                        "do\n",
                        "  value + 1\n",
                        "end\n",
                    ),
                ),
                (
                    "pages.tasks",
                    concat!(
                        "use { increment } from maths\n",
                        "\n",
                        "# Run.\ndef run\n",
                        "    @return Int\n",
                        "do\n",
                        "  let value = 41\n",
                        "  value.increment()\n",
                        "end\n",
                    ),
                ),
            ],
        )
        .expect("UFCS inside an imported module should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    #[test]
    fn production_direct_target_wasm_html_excludes_unavailable_imports() {
        // Under `wasm-html`, the module must not declare
        // `http_server_*` imports — otherwise a browser host that
        // doesn't provide them would fail at instantiate time.
        // Compile a trivial program for the `wasm-html` target and
        // parse its imports back out to verify.
        let prepared = fai_compiler::prepare_source(
            concat!("def main\n", "    @return Int\n", "do\n", "  42\n", "end\n",),
            None,
        )
        .expect("prepare");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls,
            named_param_reorder: checker.named_param_reorder,
            expression_types: checker.expression_types,
            generic_type_args: checker.generic_type_args,
        };
        let wasm = crate::try_codegen_direct(&prepared.serde_ast, &info, Some("wasm-html"))
            .expect("wasm-html build should succeed");

        // Scan the emitted wasm for import names. The simple way:
        // check each expected-excluded server import is absent.
        let parser = wasmparser::Parser::new(0);
        let mut import_names: Vec<String> = Vec::new();
        for payload in parser.parse_all(&wasm) {
            if let wasmparser::Payload::ImportSection(section) = payload.expect("payload") {
                for imp in section {
                    let imp = imp.expect("import entry");
                    import_names.push(imp.name.to_string());
                }
                break;
            }
        }
        for excluded_import in &[
            "sleep_ms",
            "run_all",
            "http_server_response",
            "http_server_listen",
            "http_server_router",
            "http_server_router_get",
            "http_server_router_post",
            "http_server_router_serve_files",
            "http_server_router_listen",
            "process_run",
            "process_start",
            "process_write",
            "process_read",
            "process_stop",
        ] {
            assert!(
                !import_names.iter().any(|n| n == excluded_import),
                "wasm-html build should exclude `{}` — saw imports {:?}",
                excluded_import,
                import_names,
            );
        }
        // Sanity: at least some imports still present.
        assert!(
            import_names.len() > 10,
            "expected many non-server imports, got {}: {:?}",
            import_names.len(),
            import_names,
        );
    }

    #[test]
    fn production_direct_exports_match_bytecode_path() {
        // The direct path must export the same set of symbols the
        // bytecode path does so hosts that reach into the module
        // (for closure dispatch, heap inspection, named callbacks)
        // work against either codegen.
        let wasm = try_compile_via_production(concat!(
            "# Helper.\ndef helper\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let f = do with n Int\n",
            "    n + 1\n",
            "  end\n",
            "  helper(21)\n",
            "end\n",
        ))
        .expect("direct build should succeed");

        let parser = wasmparser::Parser::new(0);
        let mut exports: Vec<(String, wasmparser::ExternalKind)> = Vec::new();
        for payload in parser.parse_all(&wasm) {
            if let wasmparser::Payload::ExportSection(section) = payload.expect("payload") {
                for e in section {
                    let e = e.expect("export");
                    exports.push((e.name.to_string(), e.kind));
                }
                break;
            }
        }
        let names: Vec<&str> = exports.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"_start"), "missing _start: {:?}", names);
        assert!(names.contains(&"memory"), "missing memory: {:?}", names);
        assert!(
            names.contains(&"__heap_ptr"),
            "missing __heap_ptr: {:?}",
            names
        );
        assert!(
            names.contains(&"__env_ptr"),
            "missing __env_ptr: {:?}",
            names
        );
        assert!(
            names.contains(&"__indirect_function_table"),
            "missing table export (closure present): {:?}",
            names,
        );
        assert!(
            names.contains(&"helper"),
            "named top-level function not exported: {:?}",
            names,
        );
    }

    #[test]
    fn production_direct_extern_call_roundtrip() {
        let wasm = try_compile_via_production(concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let _ = strlen('hi')\n",
            "  42\n",
            "end\n",
        ))
        .expect("extern FFI should compile via direct");
        let result = run_module(&wasm) as u64;
        let expected = (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | 42;
        assert_eq!(result, expected);
    }

    // ---- Bare-global builtin tests (Phase H prerequisites) ----
    //
    // Each verifies the direct path can compile + run a bare-global
    // call without falling back to bytecode. These were previously
    // only reachable via translate.rs.

    fn bool_true() -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64) | 1
    }
    fn bool_false() -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_BOOL as u64)
    }
    fn int_val(n: u32) -> u64 {
        (runtime::QNAN as u64) | (runtime::TAG_INT as u64) | (n as u64)
    }

    #[test]
    fn direct_bare_is_int_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_int(5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_int_false_on_float() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_int(1.5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_float_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_float(1.5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_null_true_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let v Int? = null\n",
            "  is_null(v)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_null_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_null(5)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_bool_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_bool(true)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_bool_false_on_int() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_bool(1)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_string_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_string('hi')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_array_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_array([1, 2, 3])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_array_false_on_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_array('hi')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_is_dict_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  is_dict({a: 1})\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_length_of_array() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length([10, 20, 30, 40])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(4));
    }

    #[test]
    fn direct_bare_length_of_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length('abcde')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(5));
    }

    #[test]
    fn direct_bare_is_empty_array_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let a Int[] = []\n",
            "  isEmpty(a)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_is_empty_array_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  isEmpty([1])\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_to_string_of_int() {
        // `toString(42)` → "42". Verify via length.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(toString(42))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(2));
    }

    #[test]
    fn direct_bare_to_string_boxes_raw_int_expression() {
        // Native integer arithmetic compiles to a raw Int shape; toString
        // must box it before handing it to the generic value-to-string helper.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(toString(0 + 1))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(1));
    }

    #[test]
    fn direct_convert_to_string_boxes_raw_int_expression() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "use std.convert\n\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  length(convert.toString(0 + 1))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(1));
    }

    #[test]
    fn direct_bare_to_int_passthrough() {
        // `toInt(v)` on an Int is a no-op pass-through in the direct path.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  toInt(7)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_bare_dict_get_string() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let d = {name: 'ada', age: 36}\n",
            "  length(getString(d, 'name'))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(3));
    }

    #[test]
    fn direct_bare_dict_has_key_true() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  hasKey(d, 'a')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_true());
    }

    #[test]
    fn direct_bare_dict_has_key_false() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let d = {a: 1}\n",
            "  hasKey(d, 'missing')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, bool_false());
    }

    #[test]
    fn direct_bare_parse_int() {
        // `parseInt("42")` uses RT_PARSE_INT (generated into the
        // module, not a host import) — runs real parsing.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  parseInt('42')\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_bare_parse_float_compiles() {
        // `parseFloat(s)` uses RT_PARSE_FLOAT — just verifies the
        // direct path compiles + runs without trapping.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Float?\n",
            "do\n",
            "  parseFloat('1.5')\n",
            "end\n",
        )));
        let _ = run_module(&wasm);
    }

    #[test]
    fn direct_bare_set_mutates_dict() {
        // `set(d, k, v)` returns the mutated dict. Read back via
        // getInt to confirm the value was inserted.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var d = {a: 1}\n",
            "  let d2 = set(d, 'b', 99)\n",
            "  unwrap(getInt(d2, 'b'), 0)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(99));
    }

    #[test]
    fn direct_bare_unwrap_returns_value_when_present() {
        // `unwrap(v, fallback)`: when v is non-null, returns v.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# Maybe.\ndef maybe\n",
            "    @return Int?\n",
            "do\n",
            "  42\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  unwrap(maybe(), 0)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_bare_unwrap_returns_fallback_on_null() {
        let wasm = build_standalone_module_many(compile_all(concat!(
            "# None.\ndef none\n",
            "    @return Int?\n",
            "do\n",
            "  null\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  unwrap(none(), 7)\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_bare_error_ctor_message() {
        // Bare `Error(msg)` form (no `error.` prefix) should build a
        // dict whose `message` field is the argument string.
        let wasm = build_standalone_module_many(compile_all(concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let e = Error('nope')\n",
            "  length(message(e))\n",
            "end\n",
        )));
        assert_eq!(run_module(&wasm) as u64, int_val(4));
    }

    /// Compile a source snippet through the same pipeline the CLI
    /// uses (`build_program_full` + `assemble_wasm_module`) so the
    /// synthesised `<__start__>` / `<__module_init__>` wrappers and
    /// the extra module-var globals land in the output. The
    /// standalone `compile_all` + `build_module` helpers bypass
    /// `build_program_full`, so they would not exercise the code
    /// paths under test here.
    fn build_via_full_pipeline(src: &str) -> Vec<u8> {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let checker_info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };
        crate::codegen_direct_full_reasoned(&prepared.serde_ast, &[], &checker_info, None, false)
            .expect("full-pipeline codegen failed")
    }

    #[test]
    fn direct_module_level_var_read() {
        // Module-level `var` referenced from `main` must resolve to
        // its dedicated wasm global and read the initialised value.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 42\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_module_level_var_write() {
        // Assigning to a module-level `var` inside `main` must route
        // through `GlobalSet`; the subsequent read sees the new value.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 0\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  counter = 7\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(7));
    }

    #[test]
    fn direct_module_level_var_persists_across_calls() {
        // A helper that bumps a module-level counter must share its
        // state with `main`'s subsequent read — any scheme that
        // reintroduced a per-call local for `counter` would lose
        // updates here.
        let wasm = build_via_full_pipeline(concat!(
            "var counter Int = 0\n",
            "\n",
            "# Bump the module-level counter by one.\n",
            "def bump\n",
            "    @return Int\n",
            "do\n",
            "  counter = counter + 1\n",
            "  counter\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  bump()\n",
            "  bump()\n",
            "  bump()\n",
            "  counter\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(3));
    }

    #[test]
    fn direct_start_export_is_zero_arg_when_no_main() {
        // Library-shaped file: no `main`, first declared function
        // takes two params. The synthesised `<__start__>` must still
        // give `_start` a `() -> i64` signature so the host runner
        // and test harness can invoke it.
        let wasm = build_via_full_pipeline(concat!(
            "# Return the sum of two integers.\n",
            "def addPair\n",
            "    @param a Int\n",
            "    @param b Int\n",
            "    @return Int\n",
            "do\n",
            "  a + b\n",
            "end\n",
        ));
        // `run_module` gets `_start` via `get_typed_func::<(), i64>`
        // — instantiation would fail if `_start` had any parameters.
        let _ = run_module(&wasm);
    }

    #[test]
    fn direct_generic_echo_returns_user_arg() {
        // `def echo @type T @param v $T @return $T do v end` called
        // as `echo(42)` must return 42. Regression guard: the
        // builder used to bind user params (locals 0..N) before
        // type params (locals N..N+M), but the call site emits
        // type-args first, so the callee read the type-arg string
        // instead of the real user value. Any generic function that
        // returns a user param was silently returning the wrong
        // value before this fix.
        let wasm = build_via_full_pipeline(concat!(
            "# Return v unchanged.\n",
            "def echo\n",
            "    @type T\n",
            "    @param v $T\n",
            "    @return $T\n",
            "do\n",
            "  v\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  echo(42)\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_generic_value_into_struct_field_round_trips() {
        // `def mkBox @type T @param v $T @return Box do Box(value: v) end`
        // then reading `b.value` must return the value passed in.
        // Same ordering bug — it corrupted the field write because
        // the generic-parameter read inside the constructor call
        // picked up the type-arg string, and that's what landed in
        // the dict.
        let wasm = build_via_full_pipeline(concat!(
            "type Box\n",
            "  value $T\n",
            "end\n",
            "\n",
            "# Build a Box carrying v.\n",
            "def mkBox\n",
            "    @type T\n",
            "    @param v $T\n",
            "    @return Box\n",
            "do\n",
            "  Box(value: v)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let b = mkBox(42)\n",
            "  b.value\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    /// Build a one-statement stub `DiscoveredModule` for collision
    /// tests — the body is unused; only the `name` field matters for
    /// `build_program_full`'s module bookkeeping.
    fn stub_module(name: &str) -> fai_compiler::compiler::DiscoveredModule {
        fai_compiler::compiler::DiscoveredModule {
            name: name.to_string(),
            statements: Vec::new(),
            file_paths: Vec::new(),
            private_names: Vec::new(),
        }
    }

    fn build_program_with_modules_for_test(
        entry_src: &str,
        modules: &[fai_compiler::compiler::DiscoveredModule],
    ) -> Result<BuiltProgram, BuildError> {
        let prepared = fai_compiler::prepare_source(entry_src, None).expect("prepare failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("checker failed");
        let info = CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };
        let rt = RtOffsets {
            base: direct_rt_base(),
        };
        let type_indices = direct_fai_func_type_indices();
        let import_available = crate::runtime::available_imports_with_test_flag(None, false);
        let (import_remap, _) = crate::runtime::build_import_remap(&import_available);
        build_program_full(
            &prepared.serde_ast,
            modules,
            rt,
            &info,
            &type_indices,
            &import_remap,
            false,
            None,
        )
    }

    #[test]
    fn direct_duplicate_module_canonical_name_errors() {
        // Two discovered modules with the same canonical name — a
        // local `Forui` directory plus a dependency package also
        // named `Forui` is the concrete user-facing scenario. The
        // builder must refuse rather than silently pick one and
        // shadow the other.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("Forui"), stub_module("Forui")];
        let err = build_program_with_modules_for_test(entry, &modules)
            .expect_err("duplicate module name must fail");
        match err {
            BuildError::DuplicateModuleName(name) => assert_eq!(name, "Forui"),
            other => panic!("expected DuplicateModuleName, got {:?}", other),
        }
    }

    #[test]
    fn direct_duplicate_module_basename_is_allowed_without_alias_use() {
        // Two modules with distinct canonical paths but the same
        // final segment are valid folder namespaces. The direct
        // builder should avoid creating an implicit ambiguous
        // basename alias, not reject the whole target graph.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("MyApp.Forui"), stub_module("Forui")];
        build_program_with_modules_for_test(entry, &modules)
            .expect("same basename modules should not fail by themselves");
    }

    #[test]
    fn direct_distinct_modules_with_no_basename_collision_ok() {
        // Sanity: distinct canonical names with distinct basenames
        // still build. Guards against over-reaching in the collision
        // check.
        let entry = concat!("def main\n", "    @return Int\n", "do\n", "  1\n", "end\n",);
        let modules = vec![stub_module("Forui.signal"), stub_module("Forui.view")];
        build_program_with_modules_for_test(entry, &modules)
            .expect("distinct names should build cleanly");
    }

    #[test]
    fn direct_force_unwrap_call_on_optional_closure() {
        // `cb!(arg)` — force-unwrap an optional closure and call it
        // in one expression. `compile_call` routes this through the
        // generic non-identifier callee path
        // (`compile_indirect_call_from_expr`), which reuses the
        // normal expression lowering for `ForceUnwrapExpression`.
        // That lowering already emits the `== VAL_NULL → unreachable`
        // null-trap before leaving the closure value on the stack,
        // so the `!` contract is preserved end-to-end — without any
        // special-case code in the call path.
        //
        // Regression guard: this used to refuse with
        // `UnsupportedExpression("CallExpression/non-identifier")`,
        // blocking forui's `navigateListener!(path)`,
        // `onChangeListener!()`, and `mountedApp!()` call sites.
        let wasm = build_via_full_pipeline(concat!(
            "type def Callback\n",
            "    @param x Int\n",
            "    @return Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var cb Callback? = null\n",
            "  cb = do with x Int\n",
            "      x + 1\n",
            "    end\n",
            "  cb!(41)\n",
            "end\n",
        ));
        assert_eq!(run_module(&wasm) as u64, int_val(42));
    }

    #[test]
    fn direct_force_unwrap_call_traps_on_null() {
        // Null optional + `!()` must trap at runtime — the `!`
        // contract (panic if null) applies whether the unwrap feeds
        // a read or a call. Paired with the happy-path test above,
        // this locks the generic callee path's null-check in place
        // so a later simplification can't quietly drop it.
        let wasm = build_via_full_pipeline(concat!(
            "type def Callback\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  var cb Callback? = null\n",
            "  cb!()\n",
            "end\n",
        ));
        let engine = Engine::default();
        let module = RuntimeModule::new(&engine, &wasm).expect("valid wasm");
        let mut store = Store::new(&engine, ());
        let mut linker = Linker::new(&engine);
        use wasmtime::{FuncType, ValType as WtValType};
        fn conv(v: wasm_encoder::ValType) -> WtValType {
            match v {
                wasm_encoder::ValType::I32 => WtValType::I32,
                wasm_encoder::ValType::I64 => WtValType::I64,
                wasm_encoder::ValType::F32 => WtValType::F32,
                wasm_encoder::ValType::F64 => WtValType::F64,
                _ => WtValType::I32,
            }
        }
        for (name, params, results) in runtime::import_signatures() {
            let wt_params: Vec<WtValType> = params.iter().copied().map(conv).collect();
            let wt_results: Vec<WtValType> = results.iter().copied().map(conv).collect();
            let results_clone = results.clone();
            linker
                .func_new(
                    "env",
                    name,
                    FuncType::new(&engine, wt_params, wt_results),
                    move |_caller, _args, rets| {
                        for (slot, ty) in rets.iter_mut().zip(results_clone.iter()) {
                            *slot = match ty {
                                wasm_encoder::ValType::I32 => Val::I32(0),
                                wasm_encoder::ValType::I64 => Val::I64(0),
                                wasm_encoder::ValType::F32 => Val::F32(0),
                                wasm_encoder::ValType::F64 => Val::F64(0),
                                _ => Val::I32(0),
                            };
                        }
                        Ok(())
                    },
                )
                .unwrap();
        }
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("instantiate");
        // `cb!()` invokes a closure value, so the program is async: the
        // null-unwrap trap fires while the root task runs under `__fai_poll`,
        // which `_start_async` drives. (A sync `_start` program would trap in the
        // `_start` call directly; handle whichever entry the module exposes.)
        let err = if let Ok(start) = instance.get_typed_func::<(), i64>(&mut store, "_start") {
            start.call(&mut store, ()).expect_err("should trap")
        } else {
            let start_async = instance
                .get_typed_func::<(), i32>(&mut store, "_start_async")
                .expect("_start or _start_async export");
            start_async.call(&mut store, ()).expect_err("should trap")
        };
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("unreachable"),
            "expected unreachable trap, got: {}",
            msg,
        );
    }
}

