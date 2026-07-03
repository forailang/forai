use super::*;

/// A single module-method entry. Most methods are `Simple` (push
/// args, call the import, wrap result); a small set have shapes that
/// don't fit the flat pattern and get explicit variants here.
pub(super) enum ModuleCall {
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
    /// `std.json.requireString(dict, key) -> String?`. The raw host import
    /// returns an alias to a string inside `dict`; this wrapper retains the
    /// result before releasing an owned inline dict temp, so the std call has
    /// an Owned result contract.
    JsonRequireString,
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
pub(super) fn resolve_module_call(module: &str, method: &str) -> Option<ModuleCall> {
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
    if (module, method) == ("std.json", "requireString") {
        return Some(ModuleCall::JsonRequireString);
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
        // Host-side JSON selection: parse natively, walk the dot-path, and
        // materialize only the matches (query) or one window of them with a
        // total count (queryPage). Large artifacts never build a full guest
        // tree the way `parse` does.
        ("std.json", "query") => (IMPORT_JSON_QUERY, &[AS::String, AS::String], RS::Boxed),
        ("std.json", "queryPage") => (
            IMPORT_JSON_QUERY_PAGE,
            &[AS::String, AS::String, AS::Int, AS::Int],
            RS::Boxed,
        ),
        // Text-level formatting helpers — reserialize host-side without
        // materializing guest values. `valid` is a cheap probe.
        ("std.json", "format") => (IMPORT_JSON_FORMAT, &[AS::String], RS::Boxed),
        ("std.json", "minify") => (IMPORT_JSON_MINIFY, &[AS::String], RS::Boxed),
        ("std.json", "valid") => (IMPORT_JSON_VALID, &[AS::String], RS::MakeBool),
        ("std.json", "stringifyPretty") => {
            (IMPORT_JSON_STRINGIFY_PRETTY, &[AS::Boxed], RS::Boxed)
        }
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
        ("std.crypto", "hmacSha1Base64") => (
            IMPORT_CRYPTO_HMAC_SHA1_BASE64,
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
        ("std.crypto", "rs256SignBase64Url") => (
            IMPORT_CRYPTO_RS256_SIGN_BASE64_URL,
            &[AS::String, AS::String],
            RS::Boxed,
        ),
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
    let (_, has_body) = http_request_host_op_kind(module, method)?;
    let import_idx = match method {
        "get" => IMPORT_HTTP_REQUEST_GET,
        "post" => IMPORT_HTTP_REQUEST_POST,
        "put" => IMPORT_HTTP_REQUEST_PUT,
        "patch" => IMPORT_HTTP_REQUEST_PATCH,
        "delete" => IMPORT_HTTP_REQUEST_DELETE,
        _ => unreachable!("http_request_host_op_kind accepted unknown method"),
    };
    Some(ModuleCall::HttpRequest {
        import_idx,
        has_body,
    })
}

pub(crate) fn http_request_host_op_kind(module: &str, method: &str) -> Option<(i32, bool)> {
    if module != "std.http.request" {
        return None;
    }
    match method {
        "get" => Some((crate::runtime::HOST_OP_HTTP_GET, false)),
        "post" => Some((crate::runtime::HOST_OP_HTTP_POST, true)),
        "put" => Some((crate::runtime::HOST_OP_HTTP_PUT, true)),
        "patch" => Some((crate::runtime::HOST_OP_HTTP_PATCH, true)),
        "delete" => Some((crate::runtime::HOST_OP_HTTP_DELETE, false)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AwaitHostOpKind {
    HttpRequest,
    BlockingIo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StdlibScheduling {
    /// The call can block on host I/O and must lower through
    /// `host_op_begin` / `host_op_result` so the scheduler can run other tasks.
    AwaitHostOp {
        await_kind: AwaitHostOpKind,
        op_kind: i32,
        arity: HostOpArity,
    },
    /// Host-backed but expected to complete promptly on the scheduler thread.
    DirectHostImport,
    /// Potentially CPU-bound work that intentionally remains direct until a
    /// separate fairness/preemption design exists.
    CpuBoundDirect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostOpArity {
    Exact(usize),
    Range { min: usize, max: usize },
}

impl HostOpArity {
    pub(super) fn accepts(self, len: usize) -> bool {
        match self {
            HostOpArity::Exact(expected) => len == expected,
            HostOpArity::Range { min, max } => len >= min && len <= max,
        }
    }
}

pub(super) fn stdlib_host_op_kind(module: &str, method: &str) -> Option<(i32, HostOpArity)> {
    match stdlib_scheduling(module, method)? {
        StdlibScheduling::AwaitHostOp { op_kind, arity, .. } => Some((op_kind, arity)),
        _ => None,
    }
}

pub(crate) fn stdlib_scheduling(module: &str, method: &str) -> Option<StdlibScheduling> {
    use StdlibScheduling::*;

    if let Some((op_kind, has_body)) = http_request_host_op_kind(module, method) {
        let min = if has_body { 2 } else { 1 };
        return Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::HttpRequest,
            op_kind,
            arity: HostOpArity::Range { min, max: min + 1 },
        });
    }

    match (module, method) {
        ("std.process", "run") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_PROCESS_RUN,
            arity: HostOpArity::Exact(5),
        }),
        // `write` blocks until the child drains its stdin pipe — child-paced,
        // so it must cross the boundary like `run` (plan 103 U2).
        ("std.process", "write") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_PROCESS_WRITE,
            arity: HostOpArity::Exact(2),
        }),
        ("std.file", "read") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_FILE_READ,
            arity: HostOpArity::Exact(1),
        }),
        ("std.file", "write") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_FILE_WRITE,
            arity: HostOpArity::Exact(2),
        }),
        ("std.file", "list") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_FILE_LIST,
            arity: HostOpArity::Exact(1),
        }),
        ("std.env", "load") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_ENV_LOAD,
            arity: HostOpArity::Exact(1),
        }),
        ("std.net.tcp", "accept") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_TCP_ACCEPT,
            arity: HostOpArity::Exact(1),
        }),
        ("std.net.tcp", "connect") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_TCP_CONNECT,
            arity: HostOpArity::Exact(2),
        }),
        ("std.net.tcp", "read") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_TCP_READ,
            arity: HostOpArity::Exact(1),
        }),
        ("std.net.tcp", "readLine") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_TCP_READ_LINE,
            arity: HostOpArity::Exact(1),
        }),
        ("std.net.udp", "receive") => Some(AwaitHostOp {
            await_kind: AwaitHostOpKind::BlockingIo,
            op_kind: crate::runtime::HOST_OP_UDP_RECEIVE,
            arity: HostOpArity::Exact(1),
        }),

        ("std.array", "map" | "filter" | "find" | "isAny" | "isAll")
        | ("std.json", "parse" | "stringify")
        | (
            "std.crypto",
            "hmacSha256Hex" | "hmacSha1Base64" | "sha256Hex" | "hexEncode" | "base64Encode"
            | "base64Decode" | "rs256SignBase64Url",
        ) => Some(CpuBoundDirect),

        ("std.file", "exists")
        | ("std.env", "get")
        | ("std.net", "available")
        | ("std.process", "available" | "start" | "read" | "stop")
        | ("std.net.tcp", "listen" | "write" | "close" | "address")
        | ("std.net.udp", "bind" | "send" | "broadcast")
        | ("std.crypto", "available" | "constantTimeEquals") => Some(DirectHostImport),
        _ => None,
    }
}
