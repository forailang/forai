//! Runtime helper WASM functions emitted into the module.
//!
//! These handle NaN-boxed value type dispatch (int vs float arithmetic,
//! comparisons, etc.) so that per-opcode translation stays simple.

use wasm_encoder::{Function, Instruction, MemArg, ValType};

// NaN-boxing constants (same bit patterns as fai-core/src/value.rs)
pub const QNAN: i64 = 0x7FFC_0000_0000_0000_u64 as i64;
pub const SIGN_BIT: i64 = 0x8000_0000_0000_0000_u64 as i64;
pub const TAG_NULL: i64 = 0x0001_0000_0000_0000_u64 as i64;
pub const TAG_VOID: i64 = 0x0002_0000_0000_0000_u64 as i64;
pub const TAG_BOOL: i64 = 0x0003_0000_0000_0000_u64 as i64;
pub const TAG_INT: i64 = 0x0004_0000_0000_0000_u64 as i64;

pub const VAL_NULL: i64 = QNAN | TAG_NULL;
pub const VAL_VOID: i64 = QNAN | TAG_VOID;
pub const VAL_TRUE: i64 = QNAN | TAG_BOOL | 1;
pub const VAL_FALSE: i64 = QNAN | TAG_BOOL;

// Mask for checking if a value is an int: QNAN | SIGN_BIT | tag_bits_mask
pub const INT_CHECK_MASK: i64 =
    (0x7FFC_0000_0000_0000_u64 | 0x8000_0000_0000_0000_u64 | 0x0007_0000_0000_0000_u64) as i64;
pub const INT_CHECK_EXPECT: i64 = (0x7FFC_0000_0000_0000_u64 | 0x0004_0000_0000_0000_u64) as i64;

/// Runtime function indices (offset from import_count).
/// Order must match `emit_all`.
pub const RT_IS_INT: u32 = 0;
pub const RT_IS_FLOAT: u32 = 1;
pub const RT_AS_NUMBER: u32 = 2;
pub const RT_MAKE_INT: u32 = 3;
pub const RT_MAKE_FLOAT: u32 = 4;
pub const RT_MAKE_BOOL: u32 = 5;
pub const RT_ADD: u32 = 6;
pub const RT_SUB: u32 = 7;
pub const RT_MUL: u32 = 8;
pub const RT_DIV: u32 = 9;
pub const RT_IDIV: u32 = 10;
pub const RT_MOD: u32 = 11;
pub const RT_POW: u32 = 12;
pub const RT_NEG: u32 = 13;
pub const RT_EQ: u32 = 14;
pub const RT_NE: u32 = 15;
pub const RT_LT: u32 = 16;
pub const RT_LE: u32 = 17;
pub const RT_GT: u32 = 18;
pub const RT_GE: u32 = 19;
pub const RT_ITOA: u32 = 21;
pub const RT_ALLOC: u32 = 22;
pub const RT_MAKE_OBJ: u32 = 23;
pub const RT_OBJ_ADDR: u32 = 24;
pub const RT_IS_OBJ: u32 = 25;
// Phase 2.2: WASM-native runtime functions (replace host imports)
pub const RT_STR_EQ: u32 = 26;
pub const RT_STR_CMP: u32 = 27;
pub const RT_ALLOC_STRING: u32 = 28;
pub const RT_CONCAT: u32 = 29;
pub const RT_GET_INDEX: u32 = 30;
pub const RT_GET_FIELD: u32 = 31;
pub const RT_SET_FIELD: u32 = 32;
pub const RT_PRINT_VAL_NEW: u32 = 33;
pub const RT_VALUE_TO_STR: u32 = 34;
pub const RT_CALL_NATIVE: u32 = 36;
pub const RT_PARSE_INT: u32 = 37;
pub const RT_PARSE_FLOAT: u32 = 38;

pub const RT_FREE: u32 = 39;
// Deep-COPY an object graph into fresh owned blocks. (i64)->i64. The `copy(x)`
// builtin: an independent duplicate that follows normal value semantics.
pub const RT_COPY_DEEP: u32 = 40;
// Reference counting (plan 113). RETAIN: increment an object's refcount, return
// it; no-op for primitives. (i64)->i64. RELEASE: decrement; at zero, recurse-
// release children then free. (i64)->(). The count lives in the 8-byte prefix at
// `obj_addr - 8` (see emit_alloc).
pub const RT_RETAIN: u32 = 41;
pub const RT_RELEASE: u32 = 42;
// Diagnostics (plan 115): read the live-object counter global. ()->i32. The
// counter's global index varies by module layout (sync vs async, module-var
// count), so it's baked into this helper at emit time; the `__liveObjects()`
// builtin calls it rather than hardcoding an index.
pub const RT_LIVE_OBJECTS: u32 = 43;
// Assignment-position string concat — emitted only for `s = s + x` where `s`
// is the same owned local being reassigned (see direct.rs
// try_compile_concat_move). Appends x's bytes in place when `s` is a uniquely
// owned string (rc == 1) with spare block capacity; grows with 2× capacity
// otherwise; falls back to RT_ADD semantics for non-string / shared values.
pub const RT_CONCAT_MOVE: u32 = 44;
pub const RT_COUNT: u32 = 45;

// Object type tags for heap objects
pub const OBJ_TAG_STRING: i32 = 0;
pub const OBJ_TAG_ARRAY: i32 = 1;
pub const OBJ_TAG_TUPLE: i32 = 2;
pub const OBJ_TAG_DICT: i32 = 3;
pub const OBJ_TAG_CLOSURE: i32 = 4;
pub const OBJ_TAG_MODULE: i32 = 5;
pub const OBJ_TAG_NATIVE_FN: i32 = 6;
pub const OBJ_TAG_INSTANCE: i32 = 7;
/// Shared mutable slot for a captured-and-mutated `var` (plan 114):
/// `[tag@0][pad@4][value@8]`, fixed 16 bytes, rc-prefixed like every
/// object. The enclosing scope and each capturing closure co-own it;
/// `RT_RELEASE` frees the held value and the block at rc 0.
pub const OBJ_TAG_CELL: i32 = 8;
/// Debug poison written into a freed object's tag slot under the RC checked-mode
/// (`FAI_RC_CHECK`, plan 113 R2). Not a valid tag (real tags are 0..=8), so any
/// RC op that observes it in a freed block traps loudly. Overwritten when the
/// block is reused by a later alloc.
pub const OBJ_TAG_POISON: i32 = 0x7E_DEAD;

// Native method IDs (for RT_CALL_NATIVE dispatch)
pub const METHOD_LENGTH: i32 = 0;
pub const METHOD_ABS: i32 = 1;
pub const METHOD_MIN: i32 = 2;
pub const METHOD_MAX: i32 = 3;
pub const METHOD_FLOOR: i32 = 4;
pub const METHOD_CEIL: i32 = 5;
pub const METHOD_APPEND: i32 = 6;
pub const METHOD_IS_EMPTY: i32 = 7;
pub const METHOD_FILE_READ: i32 = 8;
pub const METHOD_FILE_WRITE: i32 = 9;
pub const METHOD_FILE_EXISTS: i32 = 10;
pub const METHOD_TIME_NOW: i32 = 11;
pub const METHOD_TIME_UNIX: i32 = 12;
pub const METHOD_RANDOM: i32 = 13;
pub const METHOD_SLEEP: i32 = 14;
pub const METHOD_JSON_PARSE: i32 = 15;
pub const METHOD_JSON_STRINGIFY: i32 = 16;
pub const METHOD_ROUND: i32 = 17;
pub const METHOD_SQRT: i32 = 18;
pub const METHOD_CONTAINS: i32 = 19;
pub const METHOD_SPLIT: i32 = 20;
pub const METHOD_JOIN: i32 = 21;
pub const METHOD_SORT: i32 = 22;
pub const METHOD_GET_KEYS: i32 = 23;
/// `array.slice(arr, start, end)` — copy items `[start..end)` into a new array.
pub const METHOD_SLICE: i32 = 24;
/// `array.reverse(arr)` — new array with items in reverse order.
pub const METHOD_REVERSE: i32 = 25;
/// `string.toUpper(s)` — ASCII-only uppercase (a-z → A-Z). Non-ASCII bytes
/// pass through unchanged. See `native_to_upper` in fai-runtime for the
/// full-Unicode behaviour that the VM provides; the wasm codegen scopes
/// this to ASCII to avoid shipping a Unicode case-folding table.
pub const METHOD_TO_UPPER: i32 = 26;
/// `string.toLower(s)` — ASCII-only lowercase. Same Unicode caveat as
/// METHOD_TO_UPPER.
pub const METHOD_TO_LOWER: i32 = 27;
/// `string.trim(s)` — strip ASCII whitespace (0x09, 0x0A..0x0D, 0x20)
/// from both ends. Non-ASCII bytes are never treated as whitespace —
/// the VM's `str::trim` uses Unicode whitespace but the wasm codegen
/// stays ASCII.
pub const METHOD_TRIM: i32 = 28;
/// `string.trimStart(s)` — same ASCII whitespace set as METHOD_TRIM,
/// but only strips from the beginning.
pub const METHOD_TRIM_START: i32 = 53;
/// `string.trimEnd(s)` — ASCII whitespace stripped only from the end.
pub const METHOD_TRIM_END: i32 = 54;
/// `array.first(arr)` — first element, or `null` if empty.
pub const METHOD_FIRST: i32 = 55;
/// `array.last(arr)` — last element, or `null` if empty.
pub const METHOD_LAST: i32 = 56;
/// Assignment-position append — emitted only for `xs = append(xs, x)` /
/// `xs = array.append(xs, x)` where `xs` is the same owned local being
/// reassigned, so the pre-call value is dead after the call. Appends in
/// place when the array is uniquely owned (rc == 1) and its block has
/// spare capacity; otherwise copies like METHOD_APPEND but over-allocates
/// 2× so the following in-place appends are amortized O(1). Never emitted
/// for general-position `append`, which keeps copy semantics.
pub const METHOD_APPEND_MOVE: i32 = 57;
/// `string.startsWith(text, prefix)` → Bool. Byte-level compare.
pub const METHOD_STARTS_WITH: i32 = 29;
/// `string.endsWith(text, suffix)` → Bool. Byte-level compare.
pub const METHOD_ENDS_WITH: i32 = 30;
/// `string.indexOf(text, needle)` → Int. -1 if not found. Byte offset.
pub const METHOD_INDEX_OF: i32 = 31;
/// `string.substring(text, start, end)` → String. Byte indices clamped
/// to `[0, len]`. If `end < start`, returns empty.
pub const METHOD_SUBSTRING: i32 = 32;
/// `string.repeat(text, count)` → String. Negative count is treated as 0.
pub const METHOD_REPEAT: i32 = 33;
/// `string.replace(text, find, with)` → String. All occurrences. Empty
/// `find` returns `text` unchanged (minor divergence from the VM,
/// which inserts `with` between each byte; documented in the impl).
pub const METHOD_REPLACE: i32 = 34;
// NOTE: array.contains and array.indexOf share method IDs with their
// string counterparts (METHOD_CONTAINS / METHOD_INDEX_OF). Both methods
// branch on the tag of arg0 inside their body so the same dispatch
// entry serves String and Array containers. For primitive array
// elements (Int/Bool/Null/Float) the wasm impl uses i64 bit-equality,
// which matches the VM's stringify-then-compare because same-value
// primitives have the same bit pattern. Object-ref elements (Strings,
// structs) compare by identity — the VM compares by stringified
// content. Parity tests stay on primitive arrays.

/// `math.pow(base, exp)` → Float. **Integer-exponent only.** `exp` is
/// truncated to i32 via wasm's `f64 → i32` conversion; fractional
/// exponents produce the value for the floor, diverging from the VM's
/// `f64::powf`. Wasm has no built-in pow and a full-precision
/// implementation needs exp/ln from libm, which we don't embed.
/// Parity tests use integer exponents only.
pub const METHOD_POW: i32 = 35;

// ── std.http.server methods ──────────────────────────────────────
// Response helpers all share `IMPORT_HTTP_SERVER_RESPONSE` with
// different `kind` constants encoding the response flavour. `listen`
// is a standalone blocking import that runs the accept loop in the
// host and calls back into wasm via `__indirect_function_table`.
/// `server.text(status, body)` → Dict. Content-Type: text/plain.
pub const METHOD_SERVER_TEXT: i32 = 36;
/// `server.html(status, body)` → Dict. Content-Type: text/html.
pub const METHOD_SERVER_HTML: i32 = 37;
/// `server.json(status, body)` → Dict. Body is expected already
/// stringified (diverges from the VM which stringifies via `val_to_str`
/// — the wasm codegen doesn't have a universal value-to-string path
/// without a runtime helper, so we document the narrowed contract).
pub const METHOD_SERVER_JSON: i32 = 38;
/// `server.ok(body)` → Dict with status 200.
pub const METHOD_SERVER_OK: i32 = 39;
/// `server.redirect(status, url)` → Dict with empty body and `location`.
pub const METHOD_SERVER_REDIRECT: i32 = 40;
/// `server.listen(router, port)` → Void. Starts router accept loop in host.
pub const METHOD_SERVER_LISTEN: i32 = 41;
/// `server.router()` → Router (Int ID).
pub const METHOD_SERVER_ROUTER: i32 = 42;
/// `server.get(router, pattern, handler)` → Void.
pub const METHOD_SERVER_GET: i32 = 43;
/// `server.post(router, pattern, handler)` → Void.
pub const METHOD_SERVER_POST: i32 = 44;
/// `server.serveFiles(router, dir)` → Void.
pub const METHOD_SERVER_SERVE_FILES: i32 = 45;
/// `storage.storageGet(key)` → String? — key/value read from the host
/// platform's persistent store (localStorage in browser, etc.).
pub const METHOD_STORAGE_GET: i32 = 46;
/// `storage.storageSet(key, value)` → Void.
pub const METHOD_STORAGE_SET: i32 = 47;
/// `storage.storageRemove(key)` → Void.
pub const METHOD_STORAGE_REMOVE: i32 = 48;
/// `storage.storageClear()` → Void.
pub const METHOD_STORAGE_CLEAR: i32 = 49;

// Response `kind` discriminants passed to `IMPORT_HTTP_SERVER_RESPONSE`.
pub const RESPONSE_KIND_TEXT: i32 = 0;
pub const RESPONSE_KIND_HTML: i32 = 1;
pub const RESPONSE_KIND_JSON: i32 = 2;
pub const RESPONSE_KIND_OK: i32 = 3;
pub const RESPONSE_KIND_REDIRECT: i32 = 4;

/// `std.dictionary.getString(dict, key)` — same lookup as `dict_get` per VM
/// parity (no runtime type coercion). Returns the raw value under `key` or
/// VAL_NULL. `METHOD_GET_INT` / `METHOD_GET_BOOL` share the same body.
pub const METHOD_GET_STRING: i32 = 50;
pub const METHOD_GET_INT: i32 = 51;
pub const METHOD_GET_BOOL: i32 = 52;

pub const METHOD_UNKNOWN: i32 = 255;

/// Host imports — platform I/O operations.
pub const IMPORT_PRINT: u32 = 0;
pub const IMPORT_READ_FILE: u32 = 1;
pub const IMPORT_WRITE_FILE: u32 = 2;
pub const IMPORT_NOW_MS: u32 = 3;
pub const IMPORT_RANDOM: u32 = 4;
pub const IMPORT_SLEEP_MS: u32 = 5;
pub const IMPORT_CALL_FFI: u32 = 6;
pub const IMPORT_RUN_ALL: u32 = 7;
pub const IMPORT_SPAWN: u32 = 8;
pub const IMPORT_SET_HTML: u32 = 10;
pub const IMPORT_SET_HTML_AT: u32 = 11;
pub const IMPORT_JSON_PARSE: u32 = 12;
pub const IMPORT_JSON_STRINGIFY: u32 = 13;
pub const IMPORT_REMOTE_CALL: u32 = 14;
pub const IMPORT_FLOAT_TO_STR: u32 = 15;
/// `env.http_server_response(kind, status, body_ptr, body_len) -> i64 (Dict).
/// Host-side helper that builds the `{status, body, contentType?, location?}`
/// response dict on the guest heap and returns a NaN-boxed pointer.
pub const IMPORT_HTTP_SERVER_RESPONSE: u32 = 16;
/// Reserved import slot kept to preserve canonical import indices after
/// removing the legacy single-handler HTTP listener.
pub const IMPORT_RESERVED_17: u32 = 17;
/// `env.get_location_path() -> i64` — returns window.location.pathname as NaN-boxed String.
pub const IMPORT_GET_LOCATION_PATH: u32 = 18;
/// `env.push_history_state(ptr, len) -> void` — calls history.pushState with path string.
pub const IMPORT_PUSH_HISTORY_STATE: u32 = 19;
/// `env.http_server_router() -> i32` — creates a Router, returns its ID.
pub const IMPORT_HTTP_SERVER_ROUTER: u32 = 20;
/// `env.http_server_router_get(id, pat_ptr, pat_len, handler_val) -> void`
pub const IMPORT_HTTP_SERVER_ROUTER_GET: u32 = 21;
/// `env.http_server_router_post(id, pat_ptr, pat_len, handler_val) -> void`
pub const IMPORT_HTTP_SERVER_ROUTER_POST: u32 = 22;
/// `env.http_server_router_serve_files(id, dir_ptr, dir_len) -> void`
pub const IMPORT_HTTP_SERVER_ROUTER_SERVE_FILES: u32 = 23;
/// `env.http_server_router_listen(id, port) -> void` — starts the accept loop for a Router.
pub const IMPORT_HTTP_SERVER_ROUTER_LISTEN: u32 = 24;
/// `env.storage_get(key_ptr, key_len, buf_ptr) -> i32` — write the stored
/// value for `key` into the 64KB scratch buffer at `buf_ptr`, return the
/// byte length, or -1 when the key is absent.
pub const IMPORT_STORAGE_GET: u32 = 25;
/// `env.storage_set(key_ptr, key_len, val_ptr, val_len) -> void` — store
/// the value under `key` in the host platform's persistent store.
pub const IMPORT_STORAGE_SET: u32 = 26;
/// `env.storage_remove(key_ptr, key_len) -> void`.
pub const IMPORT_STORAGE_REMOVE: u32 = 27;
/// `env.storage_clear() -> void` — wipe the whole store. Rare; intended
/// for "sign out" or test teardown.
pub const IMPORT_STORAGE_CLEAR: u32 = 28;
/// `env.file_exists(path_ptr, path_len) -> i32` — returns 1 if the path
/// exists on the host filesystem, 0 otherwise. Stubbed in browser (always 0).
pub const IMPORT_FILE_EXISTS: u32 = 29;
/// `env.http_request_get(url_ptr, url_len, headers_val) -> i64` — NaN-boxed Dict.
/// Host issues an HTTP GET and returns `{status:Int, body:String,
/// headers:Dict}`, matching `native_http_get` (fai-runtime). Returns
/// VAL_NULL on transport failure. The host also handles `file://` URLs
/// for VM parity (reads the file and synthesises a 200 response).
pub const IMPORT_HTTP_REQUEST_GET: u32 = 30;
/// `env.http_request_post(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64`.
pub const IMPORT_HTTP_REQUEST_POST: u32 = 31;
/// `env.http_request_put(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64`.
pub const IMPORT_HTTP_REQUEST_PUT: u32 = 32;
/// `env.http_request_patch(url_ptr, url_len, body_ptr, body_len, headers_val) -> i64`.
pub const IMPORT_HTTP_REQUEST_PATCH: u32 = 33;
/// `env.http_request_delete(url_ptr, url_len, headers_val) -> i64`.
pub const IMPORT_HTTP_REQUEST_DELETE: u32 = 34;
/// `env.net_available() -> i32` — 1 if networking is reachable on the host
/// (always 1 on native wasmtime). Browser builds stub to 0.
pub const IMPORT_NET_AVAILABLE: u32 = 35;
/// `env.ffi_available(name_ptr, name_len) -> i32` — 1 if the C library
/// identified by `name` can be found via pkg-config or common system paths.
/// Browser stub always returns 0.
pub const IMPORT_FFI_AVAILABLE: u32 = 36;
/// `env.log_info(ptr, len)` — host writes `[INFO] <msg>` to stdout.
pub const IMPORT_LOG_INFO: u32 = 37;
/// `env.log_warn(ptr, len)` — host writes `[WARN] <msg>` to stdout.
pub const IMPORT_LOG_WARN: u32 = 38;
/// `env.log_error(ptr, len)` — host writes `[ERROR] <msg>` to stdout.
pub const IMPORT_LOG_ERROR: u32 = 39;
/// `env.path_join(l_ptr, l_len, r_ptr, r_len) -> i64` (String).
pub const IMPORT_PATH_JOIN: u32 = 40;
/// `env.path_basename(ptr, len) -> i64` (String).
pub const IMPORT_PATH_BASENAME: u32 = 41;
/// `env.path_dirname(ptr, len) -> i64` (String).
pub const IMPORT_PATH_DIRNAME: u32 = 42;
/// `env.path_extname(ptr, len) -> i64` (String).
pub const IMPORT_PATH_EXTNAME: u32 = 43;
/// `env.html_escape(ptr, len) -> i64` (String with HTML entities escaped).
pub const IMPORT_HTML_ESCAPE: u32 = 44;
/// `env.file_list(ptr, len) -> i64` — Array<String> of entry names in the
/// given directory, or VAL_NULL on I/O error.
pub const IMPORT_FILE_LIST: u32 = 45;
/// `env.json_require_string(dict_val, key_ptr, key_len) -> i64` — returns
/// the String at `key` in `dict` or VAL_NULL when the key is missing or the
/// value is not a string. Diverges from the VM which raises a typed error;
/// documented here as a known divergence.
pub const IMPORT_JSON_REQUIRE_STRING: u32 = 46;
/// Array higher-order ops. Each takes `(arr_val: i64, closure_val: i64)`
/// and invokes the closure per element via `__indirect_function_table`.
/// - `array_map` → new Array of closure results.
/// - `array_filter` → new Array of elements where closure returned truthy.
/// - `array_find` → first element where closure is truthy, or null.
/// - `array_is_any` → Bool: true if any closure result is truthy.
/// - `array_is_all` → Bool: true if all closure results are truthy.
pub const IMPORT_ARRAY_MAP: u32 = 47;
pub const IMPORT_ARRAY_FILTER: u32 = 48;
pub const IMPORT_ARRAY_FIND: u32 = 49;
pub const IMPORT_ARRAY_IS_ANY: u32 = 50;
pub const IMPORT_ARRAY_IS_ALL: u32 = 51;
/// TCP host imports. Handles are `i32` (really u32 values); -1 signals
/// failure. See fai-cli::wasm_runner::host::socket_registry for the
/// actual socket state and the matching `native_tcp_*` functions in
/// fai-runtime for VM parity.
pub const IMPORT_TCP_LISTEN: u32 = 52;
pub const IMPORT_TCP_ACCEPT: u32 = 53;
pub const IMPORT_TCP_CONNECT: u32 = 54;
pub const IMPORT_TCP_READ: u32 = 55;
pub const IMPORT_TCP_READ_LINE: u32 = 56;
pub const IMPORT_TCP_WRITE: u32 = 57;
pub const IMPORT_TCP_CLOSE: u32 = 58;
pub const IMPORT_TCP_ADDRESS: u32 = 59;
/// UDP host imports. Same handle convention as TCP.
pub const IMPORT_UDP_BIND: u32 = 60;
pub const IMPORT_UDP_SEND: u32 = 61;
pub const IMPORT_UDP_RECEIVE: u32 = 62;
pub const IMPORT_UDP_BROADCAST: u32 = 63;
/// CLI host imports. `read_line` returns a NaN-boxed String (prompt is
/// optional — pass len=0 to skip printing). Write/clear/move_to are all
/// void. Mirrors `native_cli_*` in fai-runtime.
pub const IMPORT_CLI_READ_LINE: u32 = 64;
pub const IMPORT_CLI_WRITE: u32 = 65;
pub const IMPORT_CLI_WRITE_LINE: u32 = 66;
pub const IMPORT_CLI_CLEAR: u32 = 67;
pub const IMPORT_CLI_MOVE_TO: u32 = 68;
/// `env.__fai_set_trap_msg(ptr, len) -> void` — store an assertion-failure
/// message in a host-side thread-local. The guest follows this with
/// `unreachable`; the CLI test runner catches the trap and surfaces the
/// stored message. Phase E plumbing for `test.equal` / `test.assert`.
pub const IMPORT_SET_TRAP_MSG: u32 = 69;
/// Spy/mock host imports for the `test` + `assert.*` framework.
///
/// `spy_set_mock(fn_id, value)` — replace every call to `fn_id` with a
/// return of `value`. `spy_set_mock_once` sets a one-shot mock that
/// fires for the next call then reverts. `spy_reset` clears both
/// mocks and the call record.
///
/// `spy_check_call(fn_id, args_ptr, arg_count, out_value_ptr) -> i32`:
/// called from the preamble of any top-level function that's been
/// referenced by `mock()` / `assert.*` at compile time. Records the
/// call; returns 1 and writes the mock value to `out_value_ptr` when
/// mocked, else returns 0 and the wrapper continues into the real
/// body.
///
/// `spy_assert_*` traps via `IMPORT_SET_TRAP_MSG` + `unreachable`.
pub const IMPORT_SPY_SET_MOCK: u32 = 70;
pub const IMPORT_SPY_SET_MOCK_ONCE: u32 = 71;
pub const IMPORT_SPY_RESET: u32 = 72;
pub const IMPORT_SPY_CHECK_CALL: u32 = 73;
pub const IMPORT_SPY_ASSERT_CALLED_WITH: u32 = 74;
pub const IMPORT_SPY_ASSERT_CALL_COUNT: u32 = 75;
pub const IMPORT_SPY_ASSERT_NOT_CALLED: u32 = 76;
/// `env.env_get(key_ptr, key_len) -> i64` — host process environment
/// lookup. Returns a NaN-boxed String allocated on the guest heap, or
/// VAL_NULL when the key is unset. Browser builds receive a stub that
/// always returns VAL_NULL.
pub const IMPORT_ENV_GET: u32 = 77;
/// `env.env_load(path_ptr, path_len) -> i32` — read a dotenv-style file
/// and merge `KEY=VALUE` lines into the host process environment.
/// Returns 1 on success, 0 if the file is missing or unreadable.
/// Malformed lines are skipped silently. Browser builds stub to 0.
pub const IMPORT_ENV_LOAD: u32 = 78;
/// `env.event_on(name_ptr, name_len, handler_val) -> i64` — register a
/// subscriber. Returns a NaN-boxed `Subscription { id, name }` Dict.
pub const IMPORT_EVENT_ON: u32 = 79;
/// `env.event_once(...)` — same as `event_on` but auto-removes after
/// the first delivery.
pub const IMPORT_EVENT_ONCE: u32 = 80;
/// `env.event_off(sub_val) -> i32` (Bool 0/1) — cancel a subscription.
/// Returns 1 if the subscription was active, 0 if already removed.
pub const IMPORT_EVENT_OFF: u32 = 81;
/// `env.event_emit(name_ptr, name_len, data_val) -> void` — deliver
/// an event synchronously to every subscriber registered under name,
/// in registration order, on a snapshot of the subscriber list.
pub const IMPORT_EVENT_EMIT: u32 = 82;
/// `env.event_subscribers(name_ptr, name_len) -> i32` — count.
pub const IMPORT_EVENT_SUBSCRIBERS: u32 = 83;
/// `env.event_clear(name_ptr, name_len) -> void` — drop every
/// subscriber for a single event name.
pub const IMPORT_EVENT_CLEAR: u32 = 84;
/// `env.event_clear_all() -> void` — drop every subscription.
pub const IMPORT_EVENT_CLEAR_ALL: u32 = 85;
/// `env.event_emit_deferred(name_ptr, name_len, data_val) -> void` —
/// queue an event for later dispatch. Each call appends one entry to
/// a single FIFO queue; entries are drained by `event_drain` in emit
/// order regardless of name. See Phase 5 of plans/event-system.md.
pub const IMPORT_EVENT_EMIT_DEFERRED: u32 = 86;
/// `env.event_drain() -> void` — dispatch every queued deferred event
/// in FIFO order. Subscribers can `emitDeferred` more events during
/// drain; those join the same drain pass. A subscriber that throws
/// becomes an `events:error` event with `{ name, message }` data and
/// drain continues — fire-and-forget by design.
pub const IMPORT_EVENT_DRAIN: u32 = 87;
/// `env.event_queue_len() -> i32` — current deferred queue length.
pub const IMPORT_EVENT_QUEUE_LEN: u32 = 88;
/// `env.process_run(command, cwd, env_json, timeout_ms, max_output_bytes) -> i64`
/// — run a bash command and return a JSON result string.
pub const IMPORT_PROCESS_RUN: u32 = 89;
/// `env.process_start(command, cwd, env_json, lifetime_ms) -> i64`.
pub const IMPORT_PROCESS_START: u32 = 90;
/// `env.process_write(session_id, input) -> i64`.
pub const IMPORT_PROCESS_WRITE: u32 = 91;
/// `env.process_read(session_id, max_output_bytes) -> i64`.
pub const IMPORT_PROCESS_READ: u32 = 92;
/// `env.process_stop(session_id) -> i64`.
pub const IMPORT_PROCESS_STOP: u32 = 93;
/// `env.crypto_available() -> i32` — 1 on native, 0 in browser stubs.
pub const IMPORT_CRYPTO_AVAILABLE: u32 = 94;
/// `env.crypto_hmac_sha256_hex(key_ptr, key_len, msg_ptr, msg_len) -> i64`.
pub const IMPORT_CRYPTO_HMAC_SHA256_HEX: u32 = 95;
/// `env.crypto_sha256_hex(ptr, len) -> i64`.
pub const IMPORT_CRYPTO_SHA256_HEX: u32 = 96;
/// `env.crypto_hex_encode(ptr, len) -> i64`.
pub const IMPORT_CRYPTO_HEX_ENCODE: u32 = 97;
/// `env.crypto_constant_time_equals(a_ptr, a_len, b_ptr, b_len) -> i32`.
pub const IMPORT_CRYPTO_CONSTANT_TIME_EQUALS: u32 = 98;
/// `env.crypto_base64_encode(ptr, len) -> i64`.
pub const IMPORT_CRYPTO_BASE64_ENCODE: u32 = 99;
/// `env.crypto_base64_decode(ptr, len) -> i64`.
pub const IMPORT_CRYPTO_BASE64_DECODE: u32 = 100;
/// `env.host_set_timer(task_id, ms) -> void`. Async scheduler ABI:
/// the guest owns task state; the host only arranges a later wakeup.
#[allow(dead_code)]
pub const IMPORT_HOST_SET_TIMER: u32 = 101;
/// `env.remote_begin(task_id, url_ptr,url_len, fn_ptr,fn_len, args_ptr,args_len,
/// hash_ptr,hash_len) -> ()` — start an RPC for `task_id` (browser: async
/// `fetch`; native: blocking) and arrange `__fai_resume_task(task_id)` when it
/// finishes. The suspending task reads the result with `remote_result`.
pub const IMPORT_REMOTE_BEGIN: u32 = 102;
/// `env.remote_result(task_id) -> i64` — the NaN-boxed value (or, on failure,
/// sets `__error_flag`/`__error_value` and returns null) stored by the
/// `remote_begin` for `task_id`.
pub const IMPORT_REMOTE_RESULT: u32 = 103;
/// `env.__fai_trap_report(code, a, b) -> void` — stash a structured trap
/// reason host-side immediately before an `unreachable`. The host decodes
/// `(code, a, b)` (see `TRAP_*` codes below; `a`/`b` carry a NaN-boxed
/// value, an address, or a count depending on the code) into a readable
/// message — e.g. `over-release (rc -1) of String "id" at 0x3fa38` — and
/// surfaces it when the trap unwinds. Browser hosts log the same message
/// via `console.error` before the trap kills the task. Plan 116 phase 1.
pub const IMPORT_TRAP_REPORT: u32 = 104;
/// `env.__fai_alloc_event(addr, size) -> void` — heap allocation ledger
/// (plan 116 phase 5, `--check-leaks`). Called at every `rt_alloc` return
/// with the logical object pointer and logical size, ONLY in builds where
/// [`check_leaks_enabled`] was set at codegen time. The host keeps an
/// addr→record map; whatever is never freed is the itemized live set, so
/// a leak names itself instead of being a scalar count.
pub const IMPORT_ALLOC_EVENT: u32 = 105;
/// `env.__fai_free_event(addr, size) -> void` — ledger twin of
/// [`IMPORT_ALLOC_EVENT`], called at `rt_free` entry (same gating). A free
/// with no matching live allocation surfaces double-free / heap corruption
/// that `--check-rc` (which only sees rc-prefixed objects) can miss.
pub const IMPORT_FREE_EVENT: u32 = 106;
/// `env.process_available() -> i32` — 1 on native, 0 in browser stubs.
/// Appended after the ledger imports to keep existing indices stable.
pub const IMPORT_PROCESS_AVAILABLE: u32 = 107;
/// `env.file_read_str(path_ptr, path_len) -> i64` — file contents as a
/// host-allocated NaN-boxed String, or VAL_NULL on failure. Replaces the
/// guest-scratch-buffer `read_file` ABI, whose fixed 64 KiB buffer the
/// host overflowed on larger files (silent heap corruption).
pub const IMPORT_FILE_READ_STR: u32 = 108;
/// `env.storage_get_str(key_ptr, key_len) -> i64` — stored value as a
/// host-allocated NaN-boxed String, or VAL_NULL when absent. Replaces
/// the guest-scratch-buffer `storage_get` ABI for the same reason.
pub const IMPORT_STORAGE_GET_STR: u32 = 109;
/// `env.__fai_rc_watch(obj_addr, rc_slot_addr, delta) -> void` — RC
/// watchpoint (FAI_RC_WATCH). rt_retain/rt_release call this on every RC
/// op when the watch codegen is on; the host logs a backtrace + the new
/// rc only for the watched address. The debugger-style "who touches this
/// refcount" primitive for tracking an over-release to its unmatched op.
pub const IMPORT_RC_WATCH: u32 = 110;
/// `env.__fai_mem_watch() -> void` — memory watchpoint (FAI_MEM_WATCH).
/// Called at every alloc/retain/release (and around FFI calls); the host
/// reads the word at FAI_MEM_WATCH=<addr> and logs a backtrace whenever it
/// changes. Generalizes the RC watchpoint to any address — for tracking a
/// stray write (e.g. a clobbered count field) to the op that produced it.
pub const IMPORT_MEM_WATCH: u32 = 111;
/// `env.__fai_ownership_event(op, site, value, aux) -> void` — phase-4
/// ownership-helper event stream. `op` is
/// [`fai_compiler::ownership_abi::OwnershipOp::id`], `site` is a compact
/// codegen site id, `value` carries the boxed value/address when relevant,
/// and `aux` carries the slot/convention discriminator for the operation.
pub const IMPORT_OWNERSHIP_EVENT: u32 = 112;
/// `env.replace_location(ptr, len) -> void` — replaces the current
/// browser location with a document navigation.
pub const IMPORT_REPLACE_LOCATION: u32 = 113;
/// `env.crypto_hmac_sha1_base64(key_ptr, key_len, msg_ptr, msg_len) -> i64`.
pub const IMPORT_CRYPTO_HMAC_SHA1_BASE64: u32 = 114;
/// `env.ffi_begin(task_id, ext_fn_idx, arg_count, args_ptr) -> void` — offload
/// a blocking extern call to the boundary worker pool and park the task (plan
/// 101 U7-U9). The driver loop resumes it when the worker finishes; the guest
/// then reads the value with `ffi_result`.
pub const IMPORT_FFI_BEGIN: u32 = 115;
/// `env.ffi_result(task_id) -> i64` — the NaN-boxed result of the offloaded
/// extern call started by `ffi_begin`.
pub const IMPORT_FFI_RESULT: u32 = 116;
/// `env.host_op_begin(task_id, op_kind, arg_count, args_ptr) -> void` — generic
/// async host-operation begin hook. The host copies NaN-boxed args out of guest
/// memory, submits blocking work to the boundary, and leaves the task parked.
pub const IMPORT_HOST_OP_BEGIN: u32 = 117;
/// `env.host_op_result(task_id) -> i64` — the NaN-boxed result of the generic
/// host operation started by `host_op_begin`.
pub const IMPORT_HOST_OP_RESULT: u32 = 118;
/// `env.crypto_rs256_sign_base64_url(key_ptr, key_len, msg_ptr, msg_len) -> i64`.
pub const IMPORT_CRYPTO_RS256_SIGN_BASE64_URL: u32 = 119;
/// `env.__fai_debug_function_call(name_ptr, name_len, event) -> void`.
/// Event 0 is START; event 1 is END. Declared only when
/// FAI_DEBUG_FUNCTION_CALLS is enabled.
pub const IMPORT_DEBUG_FUNCTION_CALL: u32 = 120;
pub const IMPORT_COUNT: u32 = 121;

/// Internal proof operation for the generic async host-op ABI. It echoes the
/// first boxed argument and is not exposed as a user-facing stdlib operation.
#[allow(dead_code)]
pub const HOST_OP_ECHO_BOXED: i32 = 0;
pub const HOST_OP_HTTP_GET: i32 = 1;
pub const HOST_OP_HTTP_POST: i32 = 2;
pub const HOST_OP_HTTP_PUT: i32 = 3;
pub const HOST_OP_HTTP_PATCH: i32 = 4;
pub const HOST_OP_HTTP_DELETE: i32 = 5;
pub const HOST_OP_PROCESS_RUN: i32 = 6;
pub const HOST_OP_FILE_READ: i32 = 7;
pub const HOST_OP_FILE_WRITE: i32 = 8;
pub const HOST_OP_FILE_LIST: i32 = 9;
pub const HOST_OP_ENV_LOAD: i32 = 10;
pub const HOST_OP_TCP_ACCEPT: i32 = 11;
pub const HOST_OP_TCP_CONNECT: i32 = 12;
pub const HOST_OP_TCP_READ: i32 = 13;
pub const HOST_OP_TCP_READ_LINE: i32 = 14;
pub const HOST_OP_UDP_RECEIVE: i32 = 15;
/// `process.write(session, input)` — child-paced (a full stdin pipe blocks
/// until the child reads), so it crosses the boundary as a Wait (plan 103 U2).
pub const HOST_OP_PROCESS_WRITE: i32 = 16;

// ── Trap-report codes (first arg of `__fai_trap_report`) ──────────
// The host renders these into human-readable trap reasons. Keep in
// sync with `wasm_runner/host/io.rs::format_trap_report` and the JS
// twins in `fai-cli/src/lib.rs`.
/// Retain of a freed (poisoned) object. `a` = boxed value, `b` = rc-slot addr.
pub const TRAP_RC_RETAIN_POISON: i32 = 1;
/// Release of a freed (poisoned) object. `a` = boxed value, `b` = obj addr.
pub const TRAP_RC_RELEASE_POISON: i32 = 2;
/// Refcount went negative (double-free / over-release). `a` = boxed value,
/// `b` = the new (negative) rc.
pub const TRAP_RC_OVER_RELEASE: i32 = 3;
/// `memory.grow` failed in `rt_alloc`. `a` = requested logical size,
/// `b` = heap pointer the allocation needed to reach.
pub const TRAP_OOM: i32 = 4;
/// Async task table full. `a` = task count, `b` = capacity.
pub const TRAP_TASK_OVERFLOW: i32 = 5;
/// `x!` force-unwrap saw null. `a`/`b` unused.
pub const TRAP_FORCE_UNWRAP_NULL: i32 = 6;
/// Uncaught `throw` reached the outermost frame. `a` = boxed error value.
pub const TRAP_UNCAUGHT_ERROR: i32 = 7;
/// Scheduler stall guard: the poll ready-loop ran an absurd number of
/// iterations without quiescing (livelock — e.g. a task re-readying
/// itself forever, the self-await bug class). `a` = iterations run,
/// `b` = the task id that was about to be resumed. Plan 116 phase 2:
/// converts a silent 100%-CPU hang into a reportable trap.
pub const TRAP_SCHED_STALL: i32 = 8;
/// Free-list integrity (FAI_RC_CHECK): a node about to be popped, or a
/// block being freed, is misaligned or outside the heap (below the
/// bucket region or at/above the bump pointer). Someone overwrote a
/// freed block's link word, or freed a garbage pointer. `a` = the bad
/// address, `b` = current heap_ptr.
pub const TRAP_FREELIST_CORRUPT: i32 = 9;
/// Free-list integrity (FAI_RC_CHECK): a block reached rt_alloc's pop
/// with its poisoned tag word overwritten — something wrote through a
/// stale pointer while the block sat on the free list. `a` = block
/// base, `b` = the tag word found (expected OBJ_TAG_POISON).
pub const TRAP_FREED_DIRTY: i32 = 10;
/// Free-list integrity (FAI_RC_CHECK): rt_free was handed a block whose
/// tag word is already poisoned — a double free. `a` = block base,
/// `b` = block size.
pub const TRAP_DOUBLE_FREE: i32 = 11;
/// Index store out of bounds (FAI_RC_CHECK): `xs[i] = v` with `i`
/// outside `0..count`. The unchecked store would land outside the
/// element region (i = -1 overwrites the array's own tag/count header —
/// the write-after-free signature TRAP_FREED_DIRTY catches downstream).
/// `a` = the index (signed), `b` = the array count.
pub const TRAP_INDEX_OOB: i32 = 12;
/// Dict grow saw an implausible capacity (always on): `RT_SET_FIELD`
/// derived `cap = (size_word - 8) / 16` from the rc-prefix size word and
/// got an absurd value, meaning `set()` was handed a non-dict, stale, or
/// mis-typed pointer. Trapping here prevents a multi-GB allocation.
/// `a` = computed capacity, `b` = the raw size word at addr-4.
pub const TRAP_DICT_CAP_INSANE: i32 = 13;
/// Single allocation exceeded the FAI_ALLOC_GUARD ceiling (256 MB) —
/// a runaway (concat loop, array/dict blowup) caught at the allocator
/// before it grows memory toward the 4 GB ceiling. `a` = requested
/// logical size, `b` = rounded block size.
pub const TRAP_ALLOC_TOO_BIG: i32 = 14;

// ── Check-leaks codegen gate (plan 116 phase 5) ───────────────────
// When enabled, `rt_alloc`/`rt_free` call the `__fai_alloc_event` /
// `__fai_free_event` host imports (and those imports are declared in
// the module). Release builds are unchanged: no imports, no calls.
//
// The flag is THREAD-LOCAL plus an env fallback (`FAI_CHECK_LEAKS`,
// mirroring `FAI_RC_CHECK`): codegen for one module runs on a single
// thread, and a thread-local toggle lets parallel test builds flip it
// without racing each other's import layout mid-build.
thread_local! {
    static CHECK_LEAKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable `--check-leaks` instrumentation for codegen on this
/// thread. The CLI sets this before compiling; tests use
/// [`CheckLeaksGuard`] for scoped enabling.
pub fn set_check_leaks(on: bool) {
    CHECK_LEAKS.with(|c| c.set(on));
}

/// Whether the current build should emit alloc/free ledger events.
pub fn check_leaks_enabled() -> bool {
    CHECK_LEAKS.with(|c| c.get()) || std::env::var_os("FAI_CHECK_LEAKS").is_some()
}

/// RAII guard enabling check-leaks codegen for the current thread.
pub struct CheckLeaksGuard;

impl CheckLeaksGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        set_check_leaks(true);
        CheckLeaksGuard
    }
}

impl Drop for CheckLeaksGuard {
    fn drop(&mut self) {
        set_check_leaks(false);
    }
}

// ── Ownership-check codegen gate (plan 117 phase 4) ─────────────────
//
// When enabled, storage/call helpers can emit `__fai_ownership_event`.
// The import is declared only for checked ownership builds so default
// modules instantiate against older/minimal hosts unchanged.
thread_local! {
    static OWNERSHIP_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable ownership-helper event codegen for this thread.
pub fn set_ownership_check(on: bool) {
    OWNERSHIP_CHECK.with(|c| c.set(on));
}

/// Whether this build should declare and emit ownership-helper events.
pub fn ownership_check_enabled() -> bool {
    OWNERSHIP_CHECK.with(|c| c.get()) || std::env::var_os("FAI_OWNERSHIP_CHECK").is_some()
}

/// RAII guard enabling ownership-helper event codegen for this thread.
pub struct OwnershipCheckGuard;

impl OwnershipCheckGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        set_ownership_check(true);
        OwnershipCheckGuard
    }
}

impl Drop for OwnershipCheckGuard {
    fn drop(&mut self) {
        set_ownership_check(false);
    }
}

// ── Function-call debug tracing ────────────────────────────────────────
//
// When enabled, user function bodies emit a tiny START/END host call with
// the compiled function name. Default builds declare no import and emit no
// calls so ordinary hosts keep their current import surface.
thread_local! {
    static DEBUG_FUNCTION_CALLS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable function-call debug tracing for this thread.
pub fn set_debug_function_calls(on: bool) {
    DEBUG_FUNCTION_CALLS.with(|c| c.set(on));
}

/// Whether this build should declare and emit function-call debug events.
pub fn debug_function_calls_enabled() -> bool {
    DEBUG_FUNCTION_CALLS.with(|c| c.get()) || std::env::var_os("FAI_DEBUG_FUNCTION_CALLS").is_some()
}

/// RAII guard enabling function-call debug tracing for the current thread.
pub struct DebugFunctionCallsGuard;

impl DebugFunctionCallsGuard {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        set_debug_function_calls(true);
        DebugFunctionCallsGuard
    }
}

impl Drop for DebugFunctionCallsGuard {
    fn drop(&mut self) {
        set_debug_function_calls(false);
    }
}

// --- Checked mode (plan 116) -------------------------------------------
//
// `--checked` bundles the cheap, always-safe corruption guards that have
// no measurable runtime cost: the alloc-guard (trap any single allocation
// past the 256 MB ceiling) and the index-store bounds check (trap an
// out-of-range `xs[i] = v` at the write site). It deliberately does NOT
// enable the heavy poison/free-list verification of `FAI_RC_CHECK` — those
// scan the free list on every alloc/release and are for deep debugging.
//
// Like check-leaks, the flag is thread-local plus an env fallback
// (`FAI_CHECKED`) so parallel test builds can flip it without racing each
// other's import layout mid-build.
thread_local! {
    static CHECKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Enable/disable `--checked` guard codegen for this thread. The CLI sets
/// this before compiling when `fai test --checked` is used.
pub fn set_checked(on: bool) {
    CHECKED.with(|c| c.set(on));
}

/// Whether the current build should emit the cheap checked-mode guards
/// (alloc-guard, index-store bounds check). `FAI_RC_CHECK` implies it,
/// since the heavy mode is a strict superset.
pub fn checked_enabled() -> bool {
    CHECKED.with(|c| c.get())
        || std::env::var_os("FAI_CHECKED").is_some()
        || std::env::var_os("FAI_RC_CHECK").is_some()
}

/// Return which imports are available for a given build target.
/// `None` means all imports available (native/test). The returned
/// vec has one bool per import index (0..IMPORT_COUNT).
///
/// Spy/mock imports (`spy_*`) are only declared in test-mode builds;
/// standalone native/browser builds strip them so the module can
/// instantiate against a host that doesn't provide the test framework.
pub fn available_imports_with_test_flag(target: Option<&str>, is_test: bool) -> Vec<bool> {
    let mut avail = vec![true; IMPORT_COUNT as usize];
    // Ledger imports exist only in `--check-leaks` builds, so release
    // modules carry no extra import entries (and instantiate against
    // hosts that predate them).
    if !check_leaks_enabled() {
        avail[IMPORT_ALLOC_EVENT as usize] = false;
        avail[IMPORT_FREE_EVENT as usize] = false;
    }
    // RC watchpoint import exists only when FAI_RC_WATCH is set (the same
    // gate under which rt_retain/rt_release emit the call), so normal
    // builds neither declare nor require it.
    if std::env::var_os("FAI_RC_WATCH").is_none() {
        avail[IMPORT_RC_WATCH as usize] = false;
    }
    // Same for the memory watchpoint import / FAI_MEM_WATCH.
    if std::env::var_os("FAI_MEM_WATCH").is_none() {
        avail[IMPORT_MEM_WATCH as usize] = false;
    }
    avail[IMPORT_RESERVED_17 as usize] = false;
    // Ownership helper events are opt-in, matching the helper-call emission
    // gate. Default/native/browser builds keep their old import surface.
    if !ownership_check_enabled() {
        avail[IMPORT_OWNERSHIP_EVENT as usize] = false;
    }
    if !debug_function_calls_enabled() {
        avail[IMPORT_DEBUG_FUNCTION_CALL as usize] = false;
    }
    match target {
        Some("wasm-html") | Some("wasm") => {
            // Browser async lowering owns wait/all through host_set_timer.
            // Do not require browser hosts to provide the old blocking/sync
            // imports; direct calls that reach them compile to `unreachable`.
            avail[IMPORT_SLEEP_MS as usize] = false;
            avail[IMPORT_RUN_ALL as usize] = false;
            avail[IMPORT_HTTP_SERVER_RESPONSE as usize] = false;
            avail[IMPORT_HTTP_SERVER_ROUTER as usize] = false;
            avail[IMPORT_HTTP_SERVER_ROUTER_GET as usize] = false;
            avail[IMPORT_HTTP_SERVER_ROUTER_POST as usize] = false;
            avail[IMPORT_HTTP_SERVER_ROUTER_SERVE_FILES as usize] = false;
            avail[IMPORT_HTTP_SERVER_ROUTER_LISTEN as usize] = false;
            avail[IMPORT_PROCESS_RUN as usize] = false;
            avail[IMPORT_PROCESS_START as usize] = false;
            avail[IMPORT_PROCESS_WRITE as usize] = false;
            avail[IMPORT_PROCESS_READ as usize] = false;
            avail[IMPORT_PROCESS_STOP as usize] = false;
            // std.crypto is native-only. `crypto_available` stays linked so
            // the availability probe can report false in the browser; the
            // compute functions are stripped (calling one traps).
            avail[IMPORT_CRYPTO_HMAC_SHA256_HEX as usize] = false;
            avail[IMPORT_CRYPTO_HMAC_SHA1_BASE64 as usize] = false;
            avail[IMPORT_CRYPTO_SHA256_HEX as usize] = false;
            avail[IMPORT_CRYPTO_HEX_ENCODE as usize] = false;
            avail[IMPORT_CRYPTO_CONSTANT_TIME_EQUALS as usize] = false;
            avail[IMPORT_CRYPTO_BASE64_ENCODE as usize] = false;
            avail[IMPORT_CRYPTO_BASE64_DECODE as usize] = false;
            avail[IMPORT_CRYPTO_RS256_SIGN_BASE64_URL as usize] = false;
            // FFI is native-only; an extern call reached on the browser
            // compiles to `unreachable` like the other stripped imports.
            avail[IMPORT_FFI_BEGIN as usize] = false;
            avail[IMPORT_FFI_RESULT as usize] = false;
        }
        _ => {}
    }
    if !is_test {
        avail[IMPORT_SPY_SET_MOCK as usize] = false;
        avail[IMPORT_SPY_SET_MOCK_ONCE as usize] = false;
        avail[IMPORT_SPY_RESET as usize] = false;
        avail[IMPORT_SPY_CHECK_CALL as usize] = false;
        avail[IMPORT_SPY_ASSERT_CALLED_WITH as usize] = false;
        avail[IMPORT_SPY_ASSERT_CALL_COUNT as usize] = false;
        avail[IMPORT_SPY_ASSERT_NOT_CALLED as usize] = false;
    }
    avail
}

/// Build a remap table: old import index → new import index (or None).
/// Also returns the count of available imports.
pub fn build_import_remap(available: &[bool]) -> (Vec<Option<u32>>, u32) {
    let mut remap = Vec::with_capacity(available.len());
    let mut new_idx = 0u32;
    for &avail in available {
        if avail {
            remap.push(Some(new_idx));
            new_idx += 1;
        } else {
            remap.push(None);
        }
    }
    (remap, new_idx)
}

/// Emit a `Call` to a host import, using the remap table.
/// If the import is unavailable for the target, emits `unreachable`.
pub fn emit_import_call(f: &mut Function, import_idx: u32, import_remap: &[Option<u32>]) {
    match import_remap.get(import_idx as usize).copied().flatten() {
        Some(new_idx) => {
            f.instruction(&Instruction::Call(new_idx));
        }
        None => {
            f.instruction(&Instruction::Unreachable);
        }
    }
}

/// Emit a `__fai_trap_report(code, a, b)` call followed by `unreachable`.
/// `push_a`/`push_b` must each leave exactly one i64 on the stack. The
/// host turns `(code, a, b)` into a readable trap reason (plan 116).
fn emit_trap_report_unreachable(
    f: &mut Function,
    import_remap: &[Option<u32>],
    code: i32,
    push_a: impl FnOnce(&mut Function),
    push_b: impl FnOnce(&mut Function),
) {
    f.instruction(&Instruction::I32Const(code));
    push_a(f);
    push_b(f);
    emit_import_call(f, IMPORT_TRAP_REPORT, import_remap);
    f.instruction(&Instruction::Unreachable);
}

/// Known constant string offsets in the data section (set by module.rs).
/// These are appended after the string pool data.
#[derive(Clone, Default)]
pub struct KnownStrings {
    pub length: (u32, u32),      // "length"
    pub abs: (u32, u32),         // "abs"
    pub min: (u32, u32),         // "min"
    pub max: (u32, u32),         // "max"
    pub floor: (u32, u32),       // "floor"
    pub ceil: (u32, u32),        // "ceil"
    pub append: (u32, u32),      // "append"
    pub is_empty: (u32, u32),    // "isEmpty"
    pub str_true: (u32, u32),    // "true"
    pub str_false: (u32, u32),   // "false"
    pub str_null: (u32, u32),    // "null"
    pub read: (u32, u32),        // "read"
    pub write: (u32, u32),       // "write"
    pub exists: (u32, u32),      // "exists"
    pub now: (u32, u32),         // "now"
    pub unix: (u32, u32),        // "unix"
    pub random: (u32, u32),      // "random"
    pub sleep: (u32, u32),       // "sleep"
    pub parse: (u32, u32),       // "parse"
    pub stringify: (u32, u32),   // "stringify"
    pub round: (u32, u32),       // "round"
    pub sqrt: (u32, u32),        // "sqrt"
    pub contains: (u32, u32),    // "contains"
    pub split: (u32, u32),       // "split"
    pub join: (u32, u32),        // "join"
    pub sort: (u32, u32),        // "sort"
    pub get_keys: (u32, u32),    // "getKeys"
    pub slice: (u32, u32),       // "slice"
    pub reverse: (u32, u32),     // "reverse"
    pub to_upper: (u32, u32),    // "toUpper"
    pub to_lower: (u32, u32),    // "toLower"
    pub trim: (u32, u32),        // "trim"
    pub starts_with: (u32, u32), // "startsWith"
    pub ends_with: (u32, u32),   // "endsWith"
    pub index_of: (u32, u32),    // "indexOf"
    pub substring: (u32, u32),   // "substring"
    pub repeat: (u32, u32),      // "repeat"
    pub replace: (u32, u32),     // "replace"
    pub pow: (u32, u32),         // "pow"
    // std.http.server method names
    pub listen: (u32, u32),      // "listen"
    pub text: (u32, u32),        // "text"
    pub html: (u32, u32),        // "html"
    pub json_fn: (u32, u32),     // "json" (used as a method name on std.http.server)
    pub ok: (u32, u32),          // "ok"
    pub redirect: (u32, u32),    // "redirect"
    pub router: (u32, u32),      // "router"
    pub get: (u32, u32),         // "get"
    pub post: (u32, u32),        // "post"
    pub serve_files: (u32, u32), // "serveFiles"
    // std.storage method names (verbose to avoid clashing with the
    // `get`/`set` shared method-id table used for `std.http.server`).
    pub storage_get: (u32, u32),    // "storageGet"
    pub storage_set: (u32, u32),    // "storageSet"
    pub storage_remove: (u32, u32), // "storageRemove"
    pub storage_clear: (u32, u32),  // "storageClear"
    // std.dictionary typed accessors
    pub get_string: (u32, u32), // "getString"
    pub get_int: (u32, u32),    // "getInt"
    pub get_bool: (u32, u32),   // "getBool"
    pub trim_start: (u32, u32), // "trimStart"
    pub trim_end: (u32, u32),   // "trimEnd"
    pub first: (u32, u32),      // "first"
    pub last: (u32, u32),       // "last"
}

/// Number of size-class buckets for the free list. The allocator keeps one
/// free-list head per block size (`block_size / 8`) for blocks up to
/// `NUM_FREE_BUCKETS * 8` bytes, so alloc/free are O(1) instead of an O(n)
/// linear exact-fit scan that degrades as a long-running server accumulates
/// freed blocks (see memory `allocator-freelist-on-degradation`). Larger blocks
/// fall back to a single linear list (rare, stays short). The bucket-head array
/// lives in a reserved guest-memory region at `bucket_base` (zero-initialised).
pub const NUM_FREE_BUCKETS: u32 = 1024;

/// Bytes the bucket-head array occupies (one i32 head per bucket).
pub const FREE_BUCKET_REGION_BYTES: u32 = NUM_FREE_BUCKETS * 4;

/// Emit all runtime helper function bodies.
/// Returns a Vec of Function in the order of RT_* constants.
pub fn emit_all(
    import_count: u32,
    import_remap: &[Option<u32>],
    ks: &KnownStrings,
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
) -> Vec<Function> {
    let base = import_count;
    vec![
        emit_is_int(),
        emit_is_float(),
        emit_as_number(base),
        emit_make_int(),
        emit_make_float(),
        emit_make_bool(),
        emit_add_with_concat(base),             // rt_add
        emit_binop_int_float(base, IntOp::Sub), // rt_sub
        emit_binop_int_float(base, IntOp::Mul), // rt_mul
        emit_div(base),                         // rt_div
        emit_idiv(base),                        // rt_idiv
        emit_mod_op(base),                      // rt_mod
        emit_pow(base),                         // rt_pow
        emit_neg(base),                         // rt_neg
        emit_cmp(base, CmpOp::Eq),              // rt_eq
        emit_cmp(base, CmpOp::Ne),              // rt_ne
        emit_cmp(base, CmpOp::Lt),              // rt_lt
        emit_cmp(base, CmpOp::Le),              // rt_le
        emit_cmp(base, CmpOp::Gt),              // rt_gt
        emit_cmp(base, CmpOp::Ge),              // rt_ge
        emit_print_val(base, import_remap),     // rt_print_val (legacy, primitives only)
        emit_itoa(),                            // rt_itoa
        emit_alloc(
            freelist_global,
            live_count_global,
            bucket_base,
            import_remap,
        ), // rt_alloc
        emit_make_obj(),                        // rt_make_obj
        emit_obj_addr(),                        // rt_obj_addr
        emit_is_obj(),                          // rt_is_obj
        // Phase 2.2: WASM-native runtime functions
        emit_str_eq(),                             // rt_str_eq
        emit_str_cmp(),                            // rt_str_cmp
        emit_alloc_string(base),                   // rt_alloc_string
        emit_concat_fn(base),                      // rt_concat
        emit_get_index(base),                      // rt_get_index
        emit_get_field(base, ks),                  // rt_get_field
        emit_set_field(base, import_remap),        // rt_set_field
        emit_print_val_new(base, import_remap),    // rt_print_val_new
        emit_value_to_str(base, ks, import_remap), // rt_value_to_str
        emit_import_module(base),                  // rt_import_module
        emit_call_native(base, import_remap),      // rt_call_native
        emit_parse_int(base),                      // rt_parse_int
        emit_parse_float(base),                    // rt_parse_float
        emit_free(
            freelist_global,
            live_count_global,
            bucket_base,
            import_remap,
        ), // rt_free
        emit_copy_deep(base),                      // rt_copy_deep
        emit_retain(base, bucket_base, import_remap), // rt_retain
        emit_release(base, bucket_base, import_remap), // rt_release
        emit_live_objects(live_count_global),      // rt_live_objects
        emit_concat_move(base),                    // rt_concat_move
    ]
}

// ── $rt_live_objects() -> i32 — read the live-object counter (plan 115) ──
// The counter's global index depends on the module layout, so it's captured
// here at emit time. The `__liveObjects()` debug builtin calls this and boxes
// the result as an Int.
fn emit_live_objects(live_count_global: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::End);
    f
}

// ── $is_int(val: i64) -> i32 ──────────────────────────────────────

fn emit_is_int() -> Function {
    let mut f = Function::new([]); // param 0 = val (from type sig)
                                   // (val & INT_CHECK_MASK) == INT_CHECK_EXPECT
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(INT_CHECK_MASK));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const(INT_CHECK_EXPECT));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::End);
    f
}

// ── $is_float(val: i64) -> i32 ────────────────────────────────────

fn emit_is_float() -> Function {
    let mut f = Function::new([]);
    // A value is float if (val & QNAN) != QNAN
    // (simplified: doesn't handle canonical NaN, but sufficient for M1)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(QNAN));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const(QNAN));
    f.instruction(&Instruction::I64Ne);
    f.instruction(&Instruction::End);
    f
}

// ── $as_number(val: i64) -> f64 ───────────────────────────────────

fn emit_as_number(base: u32) -> Function {
    let mut f = Function::new([]);
    // if is_int(val): convert i32 payload to f64
    // else: reinterpret bits as f64
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::F64,
    )));
    {
        // Int path: extract low 32 bits as signed i32, convert to f64
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::F64ConvertI32S);
    }
    f.instruction(&Instruction::Else);
    {
        // Float path: reinterpret i64 bits as f64
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $make_int(x: i32) -> i64 ─────────────────────────────────────

fn emit_make_int() -> Function {
    let mut f = Function::new([]);
    // QNAN | TAG_INT | (x as u32 as u64)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | TAG_INT));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── $make_float(x: f64) -> i64 ───────────────────────────────────

fn emit_make_float() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ReinterpretF64);
    f.instruction(&Instruction::End);
    f
}

// ── $make_bool(x: i32) -> i64 ────────────────────────────────────

fn emit_make_bool() -> Function {
    let mut f = Function::new([]);
    // QNAN | TAG_BOOL | (x as u64)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | TAG_BOOL));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── Binary arithmetic with int/float dispatch ─────────────────────

#[derive(Clone, Copy)]
enum IntOp {
    Sub,
    Mul,
}

fn emit_binop_int_float(base: u32, op: IntOp) -> Function {
    let mut f = Function::new([]); // params a, b from type sig
                                   // if both_int(a, b): int_op, make_int
                                   // else: as_number(a) op as_number(b), make_float
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Int path
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        match op {
            IntOp::Sub => f.instruction(&Instruction::I32Sub),
            IntOp::Mul => f.instruction(&Instruction::I32Mul),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // Float path
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        match op {
            IntOp::Sub => f.instruction(&Instruction::F64Sub),
            IntOp::Mul => f.instruction(&Instruction::F64Mul),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_add with string concat support ────────────────────────────
// If both int: int add. If either is object: call IMPORT_CONCAT. Else: float add.

fn emit_add_with_concat(base: u32) -> Function {
    let mut f = Function::new([]);
    // Check both int first
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Both int: add as i32
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // Check if either is an object (string concat)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // String concat via host — first convert both to strings, then concat
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::Call(base + RT_CONCAT));
        }
        f.instruction(&Instruction::Else);
        {
            // Float path
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
            f.instruction(&Instruction::F64Add);
            f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_div (always returns float) ────────────────────────────────

fn emit_div(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_idiv (floor division → int) ──────────────────────────────

fn emit_idiv(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::F64Floor);
    f.instruction(&Instruction::I32TruncF64S);
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_mod ───────────────────────────────────────────────────────

fn emit_mod_op(base: u32) -> Function {
    let mut f = Function::new([]);
    // Both int? use i32 remainder. Else float.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32RemS);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        // float mod: a - floor(a/b) * b
        // Use local variables for the computation
        // Actually, there's no f64.rem in WASM, so we compute manually:
        // a - trunc(a/b) * b
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        // Stack: [a_f64, b_f64]
        // We need: a - trunc(a/b) * b
        // But we can't easily dup on WASM stack without locals.
        // Simpler: convert to ints via floor, compute int mod, box as float.
        // Actually for M1 float mod is rare. Let's do: convert both to f64,
        // compute a - floor(a/b) * b using extra locals.
        // We need 2 extra locals. Let's use a simpler approach:
        // just convert to int, mod, and return as int.
        f.instruction(&Instruction::F64Div);
        f.instruction(&Instruction::F64Floor);
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        // This is wrong for float mod, but M1 only handles int mod properly.
        // TODO: proper float mod in M2
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_pow ───────────────────────────────────────────────────────

fn emit_pow(base: u32) -> Function {
    // Params: local 0=a(i64), local 1=b(i64) from type sig
    // Extra locals: 2=result(f64), 3=base_val(f64), 4=exponent(i32), 5=counter(i32)
    let locals = vec![
        (1, ValType::F64), // local 2: result
        (1, ValType::F64), // local 3: base_val
        (1, ValType::I32), // local 4: exponent
        (1, ValType::I32), // local 5: counter
    ];
    let mut f = Function::new(locals);

    // base_val = as_number(a)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::LocalSet(3));

    // exponent = as_number(b) as i32 (truncate)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
    f.instruction(&Instruction::I32TruncF64S);
    f.instruction(&Instruction::LocalSet(4));

    // result = 1.0
    f.instruction(&Instruction::F64Const(1.0));
    f.instruction(&Instruction::LocalSet(2));

    // counter = 0
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    // loop: while counter < exponent
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1)); // break

        // result = result * base_val
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalSet(2));

        // counter++
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // If both were int and result fits, return int; else float
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::End);
    f
}

// ── $rt_neg ───────────────────────────────────────────────────────

fn emit_neg(base: u32) -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    }
    f.instruction(&Instruction::Else);
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::F64Neg);
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── Comparison operators ──────────────────────────────────────────

#[derive(Clone, Copy)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn emit_cmp(base: u32, op: CmpOp) -> Function {
    // locals: 2=addr_a(i32), 3=addr_b(i32)
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I32)]);

    // Check if both are int
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Int comparison
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32WrapI64);
        match op {
            CmpOp::Eq => f.instruction(&Instruction::I32Eq),
            CmpOp::Ne => f.instruction(&Instruction::I32Ne),
            CmpOp::Lt => f.instruction(&Instruction::I32LtS),
            CmpOp::Le => f.instruction(&Instruction::I32LeS),
            CmpOp::Gt => f.instruction(&Instruction::I32GtS),
            CmpOp::Ge => f.instruction(&Instruction::I32GeS),
        };
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    }
    f.instruction(&Instruction::Else);
    {
        // Check if both are float
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // Float comparison
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::F64ReinterpretI64);
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::F64ReinterpretI64);
            match op {
                CmpOp::Eq => f.instruction(&Instruction::F64Eq),
                CmpOp::Ne => f.instruction(&Instruction::F64Ne),
                CmpOp::Lt => f.instruction(&Instruction::F64Lt),
                CmpOp::Le => f.instruction(&Instruction::F64Le),
                CmpOp::Gt => f.instruction(&Instruction::F64Gt),
                CmpOp::Ge => f.instruction(&Instruction::F64Ge),
            };
            f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        }
        f.instruction(&Instruction::Else);
        {
            match op {
                CmpOp::Eq | CmpOp::Ne => {
                    // Check if both are objects (strings) — compare content
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::LocalGet(1));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::I32And);
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        // Both objects: extract string data and compare via RT_STR_EQ
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(2)); // addr_a
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(3)); // addr_b
                                                                  // RT_STR_EQ(ptr_a, len_a, ptr_b, len_b) -> i32
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_a
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_a
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_b
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_b
                        f.instruction(&Instruction::Call(base + RT_STR_EQ));
                        if let CmpOp::Ne = op {
                            f.instruction(&Instruction::I32Eqz); // invert for Ne
                        }
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        // Non-object, non-numeric: compare raw i64 bits
                        // (null==null, bool==bool)
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::LocalGet(1));
                        match op {
                            CmpOp::Eq => f.instruction(&Instruction::I64Eq),
                            _ => f.instruction(&Instruction::I64Ne),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::End);
                }
                _ => {
                    // Ordering: strings compare lexicographically by
                    // byte sequence; everything else falls back to
                    // numeric coercion.
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::LocalGet(1));
                    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                    f.instruction(&Instruction::I32And);
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(2)); // addr_a
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
                        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                        f.instruction(&Instruction::LocalSet(3)); // addr_b
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_a
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_a
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Const(8));
                        f.instruction(&Instruction::I32Add); // ptr_b
                        f.instruction(&Instruction::LocalGet(3));
                        f.instruction(&Instruction::I32Load(MemArg {
                            offset: 4,
                            align: 0,
                            memory_index: 0,
                        })); // len_b
                        f.instruction(&Instruction::Call(base + RT_STR_CMP));
                        f.instruction(&Instruction::I32Const(0));
                        match op {
                            CmpOp::Lt => f.instruction(&Instruction::I32LtS),
                            CmpOp::Le => f.instruction(&Instruction::I32LeS),
                            CmpOp::Gt => f.instruction(&Instruction::I32GtS),
                            CmpOp::Ge => f.instruction(&Instruction::I32GeS),
                            _ => unreachable!(),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
                        match op {
                            CmpOp::Lt => f.instruction(&Instruction::F64Lt),
                            CmpOp::Le => f.instruction(&Instruction::F64Le),
                            CmpOp::Gt => f.instruction(&Instruction::F64Gt),
                            CmpOp::Ge => f.instruction(&Instruction::F64Ge),
                            _ => unreachable!(),
                        };
                        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
                    }
                    f.instruction(&Instruction::End);
                }
            }
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_print_val(val: i64) -> void ──────────────────────────────
// Writes value as string to linear memory at heap_ptr, calls env.print

fn emit_print_val(base: u32, import_remap: &[Option<u32>]) -> Function {
    // Param 0: val (i64) from type sig
    // Extra locals: 1=ptr(i32), 2=len(i32)
    let locals = vec![
        (1, ValType::I32), // local 1: ptr
        (1, ValType::I32), // local 2: len
    ];
    let mut f = Function::new(locals);

    // ptr = heap_ptr (global 0)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalSet(1));

    // Check type and write appropriate string
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Int: call itoa, get length
        f.instruction(&Instruction::LocalGet(1)); // ptr (dest)
        f.instruction(&Instruction::LocalGet(0)); // val
        f.instruction(&Instruction::I32WrapI64); // extract i32
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2)); // len = itoa result

        // Call env.print(ptr, len)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(0)); // import 0 = env.print
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for bool true
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_TRUE));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "true" to memory
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x65757274)); // "true" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for bool false
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_FALSE));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "false" to memory - 5 bytes
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x736C6166)); // "fals" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x65)); // "e"
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(5));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for null
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Write "null"
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0x6C6C756E)); // "null" in little-endian
        f.instruction(&Instruction::I32Store(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::Call(0));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Check for float (not NaN-boxed tagged)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // For M1: print float as int (truncated).
        // TODO: proper float-to-string in M2
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::I32TruncSatF64S);
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        emit_import_call(&mut f, IMPORT_PRINT, import_remap);
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Default: print nothing (void, unknown types)
    f.instruction(&Instruction::End);
    f
}

// ── $rt_itoa(ptr: i32, val: i32) -> i32 (length) ────────────────
// Writes decimal digits of val into memory at ptr. Returns length.

fn emit_itoa() -> Function {
    // Params: 0=ptr(i32), 1=val(i32) from type sig
    // Extra locals: 2=len, 3=is_neg, 4=digit, 5=start, 6=end, 7=tmp
    let locals = vec![
        (1, ValType::I32), // local 2: len
        (1, ValType::I32), // local 3: is_neg
        (1, ValType::I32), // local 4: digit
        (1, ValType::I32), // local 5: start (for reversal)
        (1, ValType::I32), // local 6: end (for reversal)
        (1, ValType::I32), // local 7: tmp
    ];
    let mut f = Function::new(locals);

    // Handle 0 specially
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x30)); // '0'
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // Handle negative
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3)); // is_neg = 1
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(1)); // val = -val
                                                  // Write '-'
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x2D)); // '-'
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(2)); // len = 1
    }
    f.instruction(&Instruction::End);

    // Write digits in reverse order starting at ptr+len
    // start = len (position of first digit)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalSet(5));

    // Loop: while val > 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1)); // break if val == 0

        // digit = val % 10
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32RemU);
        f.instruction(&Instruction::LocalSet(4));

        // val = val / 10
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32DivU);
        f.instruction(&Instruction::LocalSet(1));

        // mem[ptr + len] = digit + '0'
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0x30));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // len++
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // Reverse the digits in place: swap mem[ptr+start..] with mem[ptr+len-1..]
    // start index, end = len - 1
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(6)); // end = len - 1

    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if start >= end, break
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // tmp = mem[ptr + start]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(7));

        // mem[ptr + start] = mem[ptr + end]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // mem[ptr + end] = tmp
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Store8(wasm_encoder::MemArg {
            offset: 0,
            align: 0,
            memory_index: 0,
        }));

        // start++, end--
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));

        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block

    // Return len
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::End);
    f
}

/// FAI_HEAP_VERIFY: emit a loop scanning every free-bucket head for an
/// implausible pointer (TRAP_FREELIST_CORRUPT) or an overwritten poison
/// tag (TRAP_FREED_DIRTY). `idx_local`/`node_local` are caller-provided
/// scratch i32 locals. Emitted into rt_alloc/rt_retain/rt_release under
/// the env flag so a stale-pointer write is caught within a statement
/// or two of the writer, with the writer's backtrace.
fn emit_heads_scan(
    f: &mut Function,
    bucket_base: u32,
    import_remap: &[Option<u32>],
    idx_local: u32,
    node_local: u32,
) {
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1)); // idx >= buckets → done
                                          // node = mem[bucket_base + idx*4]
    f.instruction(&Instruction::I32Const(bucket_base as i32));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(node_local));
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    // corrupt = (node & 7) | node < heap_start | node >= heap_ptr
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Const(
        (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
    ));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        f,
        import_remap,
        TRAP_FREELIST_CORRUPT,
        |f| {
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            f.instruction(&Instruction::GlobalGet(0));
            f.instruction(&Instruction::I64ExtendI32U);
        },
    );
    f.instruction(&Instruction::End);
    // dirty = mem[node+8] != OBJ_TAG_POISON (frees poison under
    // FAI_RC_CHECK — FAI_HEAP_VERIFY implies users set both).
    // `b` packs (bucket_idx << 32 | tag_word) so the report can name
    // the block's size class alongside the overwriting value.
    f.instruction(&Instruction::LocalGet(node_local));
    f.instruction(&Instruction::I32Load(off8));
    f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        f,
        import_remap,
        TRAP_FREED_DIRTY,
        |f| {
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            f.instruction(&Instruction::LocalGet(idx_local));
            f.instruction(&Instruction::I64ExtendI32U);
            f.instruction(&Instruction::I64Const(32));
            f.instruction(&Instruction::I64Shl);
            f.instruction(&Instruction::LocalGet(node_local));
            f.instruction(&Instruction::I32Load(off8));
            f.instruction(&Instruction::I64ExtendI32U);
            f.instruction(&Instruction::I64Or);
        },
    );
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End); // node != 0
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End); // loop
    f.instruction(&Instruction::End); // block
}

// ── $rt_alloc(size: i32) -> i32 ───────────────────────────────────
// Bump allocate `size` bytes (aligned to 8). Returns address.

fn emit_alloc(
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
    import_remap: &[Option<u32>],
) -> Function {
    // locals: 1=addr, 2=new_ptr, 3=mem_bytes, 4=prev/bucket_addr, 5=cur/head,
    // 6=orig_size, 7=bucket_idx, 8/9=verify scan idx/node (all i32)
    let check_leaks = check_leaks_enabled();
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let mut f = Function::new([(9, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    // Checked-mode free-list validation (plan 116): a node in local 5 is
    // about to be reused. Trap with a named reason if its address is
    // implausible (link word overwritten → TRAP_FREELIST_CORRUPT) or if
    // its poisoned tag word was overwritten while it sat on the free
    // list (write-after-free → TRAP_FREED_DIRTY). rt_free poisons every
    // freed block's tag under FAI_RC_CHECK, so a clean node always
    // reads OBJ_TAG_POISON here.
    let validate_node = |f: &mut Function, bucket_base: u32| {
        // corrupt = (node & 7) != 0  |  node < heap_start  |  node >= heap_ptr
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(
            (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
        ));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::GlobalGet(0));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            f,
            import_remap,
            TRAP_FREELIST_CORRUPT,
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::GlobalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        // dirty = mem[node+8] != OBJ_TAG_POISON
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Load(off8));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            f,
            import_remap,
            TRAP_FREED_DIRTY,
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Load(off8));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    };
    // `--check-leaks` ledger event: __fai_alloc_event(base+8, logical_size)
    // right before each return path hands out the logical pointer.
    let alloc_event = |f: &mut Function, base_local: u32| {
        f.instruction(&Instruction::LocalGet(base_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        emit_import_call(f, IMPORT_ALLOC_EVENT, import_remap);
    };
    // FAI_HEAP_VERIFY (plan 116): scan every free-bucket HEAD on every
    // allocation and trap at the first implausible or dirtied node. This
    // narrows "something wrote through a stale pointer while the block
    // sat on the free list" from detection-at-reuse (whenever that bucket
    // is next popped — possibly thousands of allocs later) down to the
    // first allocation after the bad write, so the trap backtrace lands
    // next to the writer. Heads-only keeps it O(NUM_FREE_BUCKETS) per
    // alloc; a mid-chain dirty node surfaces once it becomes head.
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 8, 9);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }
    // Stash the LOGICAL size requested (before the rc-prefix inflation below) so
    // each return path can stamp it into the prefix's spare word at obj_addr-4.
    // RT_RELEASE reads it back to free the block at its true allocated size —
    // load-bearing for objects whose logical size differs from a count-derived
    // formula (dicts over-allocate spare capacity for in-place `set` growth;
    // plan 115). The slot is the same word `rt_free` later reuses as the
    // free-list `next` link, so every alloc must re-stamp it.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalSet(6));
    // Live-object counter (plan 113 oracle): every alloc produces exactly one
    // object (or traps on grow failure), so bump it once up front.
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::GlobalSet(live_count_global));
    // ── refcount prefix (plan 113) ──
    // Reserve 8 extra bytes in front of the object for its reference count.
    // The block base holds `rc`; the logical object pointer we hand back is
    // base+8, so `tag@0`, `count@4` and all payload offsets are unchanged. We
    // inflate the request here so the free-list search and bump below operate on
    // the real block size; each return path writes rc=1 and yields base+8.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // Round the block size UP to a multiple of 8. The bump path aligns the next
    // pointer to 8 anyway, so this doesn't change footprint — but it makes every
    // block size an exact multiple of 8, which the size-bucketing below relies on:
    // `block_size / 8` must round-trip exactly, or a bucket would mix sizes in
    // `[idx*8, idx*8+7]` and a larger request could reuse a too-small freed block.
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(0));
    // FAI_ALLOC_GUARD: trap on any single allocation past 256 MB. No
    // forai value legitimately needs that in one block, so a request
    // this large is a runaway (a concat loop building an ever-bigger
    // string, an array/dict blowup) — trapping here names the size and
    // the backtrace instead of letting the bump path grow memory toward
    // the 4 GB ceiling and thrash. Diagnostic-only (off unless the env
    // is set at codegen or `--checked` is on) so production allocs pay
    // nothing.
    if std::env::var_os("FAI_ALLOC_GUARD").is_some() || checked_enabled() {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0x1000_0000)); // 256 MiB
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_ALLOC_TOO_BIG,
            |f| {
                f.instruction(&Instruction::LocalGet(6)); // requested logical size
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // rounded block size
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // ── reuse from the free list first ──
    // Free lists are SIZE-BUCKETED: one head per block size (`block_size/8`) in
    // the reserved region at `bucket_base`, so reuse is O(1) — no linear scan
    // that degrades as a long-running server accumulates freed blocks (memory
    // `allocator-freelist-on-degradation`). Blocks larger than the bucketed range
    // fall back to a single linear list (`freelist_global`); those are rare so it
    // stays short. A freed block stores [size@0, next@4].
    // bucket_idx = block_size / 8
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(3));
    f.instruction(&Instruction::I32ShrU);
    f.instruction(&Instruction::LocalSet(7));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    // ── small: O(1) bucket pop ──
    // bucket_addr = bucket_base + idx*4  (local 4)
    f.instruction(&Instruction::I32Const(bucket_base as i32));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(4));
    // head = mem[bucket_addr]  (local 5)
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(5));
    // if head != 0: pop + return
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    if rc_check {
        validate_node(&mut f, bucket_base);
    }
    // mem[bucket_addr] = head.next
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::I32Store(mem0()));
    // rc=1 at base, logical size at base+4, return base+8
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 5);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End); // head != 0
                                      // else: empty bucket → fall through to bump.
    f.instruction(&Instruction::Else);
    // ── large: linear exact-fit scan of the fallback list ──
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // prev = 0 (null: cur is head)
    f.instruction(&Instruction::GlobalGet(freelist_global));
    f.instruction(&Instruction::LocalSet(5)); // cur = head
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::BrIf(1)); // cur == 0 → not found, break to bump
    if rc_check {
        validate_node(&mut f, bucket_base);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::GlobalSet(freelist_global));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::I32Store(off4));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 5);
    }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::Br(0)); // continue
    f.instruction(&Instruction::End); // loop
    f.instruction(&Instruction::End); // block
    f.instruction(&Instruction::End); // idx < NUM_FREE_BUCKETS
                                      // addr = heap_ptr (global 0)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalSet(1));
    // new_ptr = (heap_ptr + size + 7) & ~7  (align to 8)
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7)); // ~7 = 0xFFFFFFF8
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    // mem_bytes = memory.size * 65536
    f.instruction(&Instruction::MemorySize(0));
    f.instruction(&Instruction::I32Const(65536));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::LocalSet(3));
    // Grow memory while new_ptr would exceed current mem_bytes.
    // memory.grow takes pages as argument, returns prev-page-count or
    // -1 on failure. We grow in 16-page chunks (1 MiB) until new_ptr
    // fits, trapping if grow returns -1.
    //
    // Without this, the Rust wasmtime host (tests, `forai run`)
    // silently resizes memory on mem.data_mut() access — tests pass.
    // The browser's JS host does not: jsToWasm writes past the end
    // hit a detached buffer, later reads return garbage, and users
    // see heap addresses rendered as ints instead of the expected
    // values. See plan 99 Phase 1 notes.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1)); // break: new_ptr fits
                                              // memory.grow(16)
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::MemoryGrow(0));
        // If -1, trap.
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_OOM,
            |f| {
                f.instruction(&Instruction::LocalGet(6)); // requested size
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(2)); // needed heap ptr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        // mem_bytes += 16 * 65536 = 1 MiB
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(16 * 65536));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block
                                      // heap_ptr = new_ptr
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::GlobalSet(0));
    // bumped block: rc=1 at base, logical size at base+4, return base+8
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Store(off4));
    if check_leaks {
        alloc_event(&mut f, 1);
    }
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_free(ptr: i32, size: i32) ────────────────────────────────
// Return a heap block to the free list: store [size@0, next@4] in the
// block and make it the new list head. `size` is the block's original
// alloc size so a later same-size `alloc` reuses it. Blocks are always
// >= 8 bytes (every heap object is), so the [size,next] header fits.
fn emit_free(
    freelist_global: u32,
    live_count_global: u32,
    bucket_base: u32,
    import_remap: &[Option<u32>],
) -> Function {
    // params: 0 = ptr (i32, logical obj ptr), 1 = size (i32, logical obj size)
    // locals: 2 = bucket_idx, 3 = bucket_addr (i32)
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    // FAI_NO_REUSE (UAF hunt): orphan every freed block instead of putting
    // it back on the free list. Combined with FAI_RC_CHECK's poison, a
    // freed block then stays poisoned forever (never reused/un-poisoned),
    // so a stale retain/release/access of a dangling reference traps AT
    // THE ACT (TRAP_RC_*_POISON) with the offending backtrace — catching
    // the corruptor, not just the downstream dirtied block. Leaks heavily;
    // diagnostic-only.
    let no_reuse = std::env::var_os("FAI_NO_REUSE").is_some();
    let mut f = Function::new([(2, ValType::I32)]);
    // `--check-leaks` ledger event, with the logical ptr/size before the
    // rc-prefix conversion below rewrites them to block base/size.
    if check_leaks_enabled() {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        emit_import_call(&mut f, IMPORT_FREE_EVENT, import_remap);
    }
    // Live-object counter (plan 113 oracle): one object reclaimed per free.
    f.instruction(&Instruction::GlobalGet(live_count_global));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::GlobalSet(live_count_global));
    // Refcount prefix (plan 113): the real block starts 8 bytes before the
    // logical pointer and is 8 bytes larger. Convert to the real base/size so
    // the free-list node covers the whole block (including the rc prefix) and a
    // later same-size alloc reuses it exactly.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(0));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // Round UP to a multiple of 8 to match emit_alloc's block size exactly, so a
    // freed block lands in the same bucket a same-size alloc will look in.
    f.instruction(&Instruction::I32Const(7));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Const(!7));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::LocalSet(1));
    // Checked-mode (plan 116): catch bad frees AT the free site, before
    // they poison the free list. A misaligned/out-of-heap base is a
    // garbage pointer (TRAP_FREELIST_CORRUPT); a base whose tag word is
    // already OBJ_TAG_POISON was freed before (TRAP_DOUBLE_FREE).
    if rc_check {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(
            (bucket_base + FREE_BUCKET_REGION_BYTES) as i32,
        ));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalGet(0));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_FREELIST_CORRUPT,
            |f| {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::GlobalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_DOUBLE_FREE,
            |f| {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64ExtendI32U);
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1));
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // Push onto the SIZE-BUCKETED free list (O(1)); blocks too large for the
    // bucketed range go on the single linear fallback list. Mirrors emit_alloc.
    // Skipped entirely under FAI_NO_REUSE (the block is orphaned).
    if !no_reuse {
        // bucket_idx = block_size / 8
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(NUM_FREE_BUCKETS as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        // ── small: push to bucket[idx] ──
        // bucket_addr = bucket_base + idx*4  (local 3)
        f.instruction(&Instruction::I32Const(bucket_base as i32));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        // block.next (base+4) = mem[bucket_addr]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        // mem[bucket_addr] = base
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::Else);
        // ── large: [size@0, next@4] on the linear fallback list ──
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalGet(freelist_global));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalSet(freelist_global));
        f.instruction(&Instruction::End);
    } // !no_reuse
      // Checked-mode: poison the object tag slot (base+8 = the logical obj_addr,
      // untouched by the free-list node at base/base+4) so a stale reference that
      // reaches an RC op before the block is reused traps. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
    }
    f.instruction(&Instruction::End);
    f
}

// ── $rt_copy_deep(v: i64) -> i64 ──────────────────────────────────
// Deep-copy an object graph into FRESH OWNED blocks: allocate a new block the
// same size/tag, copy the header + string bytes, and recursively copy each
// pointer child into the new block. The result has no SHARED_BIT — it's a fully
// independent value that follows normal scope/ownership rules (the `copy(x)`
// builtin). Primitives are immediate (returned as-is). Unsizeable tags
// (closure/module/native) can't be copied — returned as-is (shared). Acyclic
// under single ownership, so the recursion terminates.
fn emit_copy_deep(base: u32) -> Function {
    // param 0: v. locals 1=src,2=tag,3=count,4=i,5=size,6=dst,7=srcE,8=dstE.
    let mut f = Function::new([(8, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let b8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    let empty = wasm_encoder::BlockType::Empty;

    // if !is_obj(v) { return v }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // src = obj_addr(v); tag = mem[src]; count = mem[src+4]; size = 0
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(3));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    // size by tag (mirrors rt_drop_deep)
    let set_size = |f: &mut Function, hdr: i32, per: i32| {
        f.instruction(&Instruction::I32Const(hdr));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(per));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
    };
    // STRING → 8 + count*1
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 1);
    f.instruction(&Instruction::End);
    // ARRAY||TUPLE → 8 + count*8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 8);
    f.instruction(&Instruction::End);
    // DICT → 8 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 8, 16);
    f.instruction(&Instruction::End);
    // INSTANCE → 16 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    set_size(&mut f, 16, 16);
    f.instruction(&Instruction::End);

    // if size == 0 { return v }  — unsizeable tag (closure/module/native)
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // dst = rt_alloc(size); mem[dst]=tag; mem[dst+4]=count
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Store(off4));

    // STRING → byte-copy `count` bytes (src+8 → dst+8)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::Block(empty));
    f.instruction(&Instruction::Loop(empty));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(b8));
    f.instruction(&Instruction::I32Store8(b8));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // recursively copy each pointer child from src entry to dst entry.
    let emit_copy_children =
        |f: &mut Function, entry_base: i32, stride: i32, child_offsets: &[u64]| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(4)); // i = 0
            f.instruction(&Instruction::Block(empty));
            f.instruction(&Instruction::Loop(empty));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            // srcE = src + entry_base + i*stride ; dstE = dst + entry_base + i*stride
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(8));
            for &co in child_offsets {
                let ma = MemArg {
                    offset: co,
                    align: 0,
                    memory_index: 0,
                };
                // mem[dstE+co] = copy_deep(mem[srcE+co])
                f.instruction(&Instruction::LocalGet(8));
                f.instruction(&Instruction::LocalGet(7));
                f.instruction(&Instruction::I64Load(ma));
                f.instruction(&Instruction::Call(base + RT_COPY_DEEP));
                f.instruction(&Instruction::I64Store(ma));
            }
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(0));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);
        };

    // ARRAY||TUPLE → child i64 @ +8, stride 8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    emit_copy_children(&mut f, 8, 8, &[0]);
    f.instruction(&Instruction::End);
    // DICT → (key,val) @ +8, stride 16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_copy_children(&mut f, 8, 16, &[0, 8]);
    f.instruction(&Instruction::End);
    // INSTANCE → copy header slot @ +8 (type metadata, shallow), entries @ +16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Load(b8));
    f.instruction(&Instruction::I64Store(b8));
    emit_copy_children(&mut f, 16, 16, &[0, 8]);
    f.instruction(&Instruction::End);

    // return make_obj(dst)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_make_obj(addr: i32) -> i64 ───────────────────────────────
// NaN-box an address as an object pointer: QNAN | SIGN_BIT | addr

fn emit_make_obj() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64ExtendI32U);
    f.instruction(&Instruction::I64Const(QNAN | SIGN_BIT));
    f.instruction(&Instruction::I64Or);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_obj_addr(val: i64) -> i32 ────────────────────────────────
// Extract the 32-bit address from a NaN-boxed object pointer.

fn emit_obj_addr() -> Function {
    let mut f = Function::new([]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const(0x0000_FFFF_FFFF_FFFF_u64 as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_is_obj(val: i64) -> i32 ──────────────────────────────────
// Check if value is an object pointer (QNAN | SIGN_BIT set, not other tags).

fn emit_is_obj() -> Function {
    let mut f = Function::new([]);
    // An object has QNAN | SIGN_BIT in the high bits
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Const((QNAN | SIGN_BIT) as i64));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Const((QNAN | SIGN_BIT) as i64));
    f.instruction(&Instruction::I64Eq);
    f.instruction(&Instruction::End);
    f
}

// ══════════════════════════════════════════════════════════════════
// Phase 2.2: WASM-native runtime functions
// ══════════════════════════════════════════════════════════════════

// ── $rt_str_eq(a_ptr: i32, a_len: i32, b_ptr: i32, b_len: i32) -> i32 ──
fn emit_str_eq() -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 4: i (loop counter)
                                                    // Check lengths first
    f.instruction(&Instruction::LocalGet(1)); // a_len
    f.instruction(&Instruction::LocalGet(3)); // b_len
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // Compare byte-by-byte
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // i = 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= a_len: return 1 (equal)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // if mem[a_ptr+i] != mem[b_ptr+i]: return 0
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // i++
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0)); // continue
    }
    f.instruction(&Instruction::End); // end loop
    f.instruction(&Instruction::End); // end block
    f.instruction(&Instruction::I32Const(1)); // unreachable but needed for validation
    f.instruction(&Instruction::End);
    f
}

// ── $rt_str_cmp(a_ptr: i32, a_len: i32, b_ptr: i32, b_len: i32) -> i32 ──
// Returns -1 when a < b, 0 when equal, 1 when a > b.
fn emit_str_cmp() -> Function {
    let mut f = Function::new([(3, ValType::I32)]); // 4=i, 5=min_len, 6=byte_diff
                                                    // min_len = min(a_len, b_len)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I32,
    )));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalSet(5));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(4)); // i = 0
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= min_len break
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // byte_diff = a[i] - b[i]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));

        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(1));
        }
        f.instruction(&Instruction::End);

        // non-zero byte diff decides ordering
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Shared prefix: shorter string sorts first.
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_alloc_string(src_ptr: i32, len: i32) -> i64 ──
fn emit_alloc_string(base: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 2: addr
                                                    // size = 8 + len
    f.instruction(&Instruction::LocalGet(1)); // len
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(2)); // addr
                                              // write tag=0
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    // write len
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // copy data: memory.copy(dst=addr+8, src=src_ptr, len=len)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(0)); // src_ptr
    f.instruction(&Instruction::LocalGet(1)); // len
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // box as object
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_concat(a: i64, b: i64) -> i64 ──
fn emit_concat_fn(base: u32) -> Function {
    // locals: 2=addr_a(i32), 3=len_a(i32), 4=addr_b(i32), 5=len_b(i32), 6=dst(i32)
    let mut f = Function::new([
        (1, ValType::I32), // 2: addr_a
        (1, ValType::I32), // 3: len_a
        (1, ValType::I32), // 4: addr_b
        (1, ValType::I32), // 5: len_b
        (1, ValType::I32), // 6: dst
    ]);
    // Extract string a
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(2)); // addr_a
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3)); // len_a
                                              // Extract string b
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(4)); // addr_b
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5)); // len_b
                                              // Allocate result: 8 + len_a + len_b
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(6)); // dst
                                              // Write tag=0
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    // Write total len
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // Copy a's data: memory.copy(dst+8, addr_a+8, len_a)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // Copy b's data: memory.copy(dst+8+len_a, addr_b+8, len_b)
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });
    // Box as object
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_concat_move(a: i64, b: i64) -> i64 ──
// Assignment-position concat: the codegen emits this only for `s = s + x`
// where `s` is the same owned (or cell-bound) local being reassigned, so the
// pre-call value of `s` is dead once the call returns.
//
// Fast path — `a` is a uniquely owned string (rc == 1) whose block has spare
// capacity (logical-size stamp at obj-4, the same discipline dict growth and
// METHOD_APPEND_MOVE use): stringify `b`, memcpy its bytes onto the end of
// `a`, bump the length, and return `a` retained (+1 owned result; the
// caller's release of the old binding drops it back to 1).
//
// Grow path — unique string but full: allocate max(needed, 2 × stamp, 32),
// copy both halves, return the fresh block. RT_ALLOC stamps the
// over-allocated logical size, so subsequent in-place appends are amortized
// O(1) and RT_FREE returns the block at its true size.
//
// Fallback — `a` is not a string or is shared (rc > 1): defer to RT_ADD,
// which preserves the exact legacy semantics (numeric add, copy concat).
// A stringified `b` temp (when `b` wasn't already a string) is released
// after its bytes are copied on the fast/grow paths; the fallback keeps
// RT_ADD's existing temp behavior.
fn emit_concat_move(base: u32) -> Function {
    // params: 0 = a (i64), 1 = b (i64)
    // locals: 2=addr_a(i32), 3=len_a(i32), 4=addr_bs(i32), 5=len_b(i32),
    //         6=cap-then-dst(i32), 7=needed(i32), 8=bs(i64)
    let mut f = Function::new([(6, ValType::I32), (1, ValType::I64)]);
    let empty = wasm_encoder::BlockType::Empty;
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let bs_local: u32 = 8;

    // a must be an object …
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(2)); // addr_a
        // … tagged String, uniquely owned (rc at obj-8 == 1).
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(empty));
        {
            // bs = value_to_str(b) — b as-is when it is already a string,
            // a fresh owned string otherwise.
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::LocalSet(bs_local));
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(4)); // addr_bs
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Load(off4));
            f.instruction(&Instruction::LocalSet(3)); // len_a
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Load(off4));
            f.instruction(&Instruction::LocalSet(5)); // len_b
            // needed = 8 + len_a + len_b
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7));

            // Fast: needed fits the stamped logical size at obj-4.
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::If(empty));
            {
                // memcpy(addr_a + 8 + len_a, addr_bs + 8, len_b). memory.copy
                // has memmove semantics, so `s = s + s` (addr_bs == addr_a,
                // adjacent ranges) is safe; len_b was read before any write.
                f.instruction(&Instruction::LocalGet(2));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(4));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::MemoryCopy {
                    src_mem: 0,
                    dst_mem: 0,
                });
                f.instruction(&Instruction::LocalGet(2));
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::I32Store(off4));
                // Release a freshly stringified temp; keep a borrowed b.
                f.instruction(&Instruction::LocalGet(bs_local));
                f.instruction(&Instruction::LocalGet(1));
                f.instruction(&Instruction::I64Ne);
                f.instruction(&Instruction::If(empty));
                f.instruction(&Instruction::LocalGet(bs_local));
                f.instruction(&Instruction::Call(base + RT_RELEASE));
                f.instruction(&Instruction::End);
                // Owned return: RT_RETAIN passes the value through.
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::Call(base + RT_RETAIN));
                f.instruction(&Instruction::Return);
            }
            f.instruction(&Instruction::End);

            // Grow: cap = max(needed, 2 × stamp, 32).
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32Const(2));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::LocalSet(6)); // cap (reuse dst slot)
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::End);

            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_ALLOC));
            f.instruction(&Instruction::LocalSet(6)); // dst
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
            f.instruction(&Instruction::I32Store(mem0()));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Store(off4));
            // copy a's bytes, then b's.
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I64Ne);
            f.instruction(&Instruction::If(empty));
            f.instruction(&Instruction::LocalGet(bs_local));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Fallback: exact legacy `+` semantics.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::Call(base + RT_ADD));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_retain(v: i64) -> i64 — reference-count increment (plan 113) ──
// Bump the count in the 8-byte prefix at obj_addr-8; no-op + passthrough for
// primitives. Returns `v` so call sites can retain inline.
fn emit_retain(base: u32, bucket_base: u32, import_remap: &[Option<u32>]) -> Function {
    // param 0: v (i64). local 1 = rc slot address; 2/3 = scan scratch (i32).
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let rc_watch = std::env::var_os("FAI_RC_WATCH").is_some();
    let mut f = Function::new([(3, ValType::I32)]);
    let empty = wasm_encoder::BlockType::Empty;
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 2, 3);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(empty));
    // rc_slot = obj_addr(v) - 8
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(1));
    // RC watchpoint: __fai_rc_watch(obj_addr=rc_slot+8, rc_slot, +1).
    if rc_watch {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(1));
        emit_import_call(&mut f, IMPORT_RC_WATCH, import_remap);
    }
    // Checked-mode: trap on retaining a freed object (tag at rc_slot+8 poisoned).
    if rc_check {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_RETAIN_POISON,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1)); // rc-slot addr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // mem[rc_slot] = mem[rc_slot] + 1
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::End);
    // return v
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_release(v: i64) -> () — reference-count decrement; free at zero ──
// Decrement the count at obj_addr-8. At zero, release each child (so their
// counts drop too) and free the block via the per-tag child traversal. No-op on
// primitives (the `is_obj` guard). The acyclic owned graph guarantees the
// recursion terminates.
fn emit_release(base: u32, bucket_base: u32, import_remap: &[Option<u32>]) -> Function {
    // param 0: v. locals: 1=addr, 2=tag, 3=count, 4=i, 5=size, 6=entry, 7=rc,
    // 8/9 = FAI_HEAP_VERIFY scan scratch.
    let rc_check = std::env::var_os("FAI_RC_CHECK").is_some();
    let heap_verify = std::env::var_os("FAI_HEAP_VERIFY").is_some();
    let mem_watch = std::env::var_os("FAI_MEM_WATCH").is_some();
    let rc_watch = std::env::var_os("FAI_RC_WATCH").is_some();
    let mut f = Function::new([(9, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let empty = wasm_encoder::BlockType::Empty;
    if heap_verify {
        emit_heads_scan(&mut f, bucket_base, import_remap, 8, 9);
    }
    if mem_watch {
        emit_import_call(&mut f, IMPORT_MEM_WATCH, import_remap);
    }

    // if !is_obj(v) { return }
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // addr = obj_addr(v)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // RC watchpoint: __fai_rc_watch(obj_addr, rc_slot=addr-8, -1).
    if rc_watch {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(-1));
        emit_import_call(&mut f, IMPORT_RC_WATCH, import_remap);
    }
    // Checked-mode: trap on releasing a freed object (tag poisoned). Catches a
    // stale reference being released a second time. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_POISON));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_RELEASE_POISON,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(1)); // obj addr
                f.instruction(&Instruction::I64ExtendI32U);
            },
        );
        f.instruction(&Instruction::End);
    }
    // rc = mem[addr-8] - 1 ; mem[addr-8] = rc ; if rc != 0 { return }
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub); // store address (rc slot)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalTee(7)); // rc = old - 1 (kept on stack for store)
    f.instruction(&Instruction::I32Store(mem0()));
    // Checked-mode: a negative count means this object was released more times
    // than retained (double-free / over-release) — the canonical symptom of a
    // mis-classified transfer. Trap. (plan 113 R2)
    if rc_check {
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(empty));
        emit_trap_report_unreachable(
            &mut f,
            import_remap,
            TRAP_RC_OVER_RELEASE,
            |f| {
                f.instruction(&Instruction::LocalGet(0)); // boxed value
            },
            |f| {
                f.instruction(&Instruction::LocalGet(7)); // new (negative) rc
                f.instruction(&Instruction::I64ExtendI32S);
            },
        );
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // rc hit zero → free children, then self. tag = mem[addr]; count = mem[addr+4]
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(off4));
    f.instruction(&Instruction::LocalSet(3));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));

    let emit_entry_loop =
        |f: &mut Function, entry_base: i32, stride: i32, child_offsets: &[u64]| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(4)); // i = 0
            f.instruction(&Instruction::Block(empty));
            f.instruction(&Instruction::Loop(empty));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1)); // i >= count → break
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(entry_base));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(stride));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(6));
            for &co in child_offsets {
                f.instruction(&Instruction::LocalGet(6));
                f.instruction(&Instruction::I64Load(MemArg {
                    offset: co,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::Call(base + RT_RELEASE));
            }
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Br(0)); // continue
            f.instruction(&Instruction::End); // loop
            f.instruction(&Instruction::End); // block
        };

    // STRING → no children; size = 8 + count(len)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // ARRAY or TUPLE → child @ +8; size = 8 + count*8
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 8, 8, &[0]);
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // DICT → release (key,val) @ +8, stride 16 for each of `count` live entries.
    // The block SIZE is NOT count-derived: a dict over-allocates spare capacity
    // (`cap = max(16, count+8)`) for in-place `set` growth, and `count` can grow
    // after alloc — so free by the LOGICAL alloc size stamped in the prefix word
    // at obj_addr-4 (plan 115). Using a count formula here under-frees the block,
    // stranding the spare-capacity tail and defeating free-list reuse.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 8, 16, &[0, 8]);
    // size = mem[addr - 4] (the logical alloc size stamped by rt_alloc)
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // INSTANCE → (key,val) @ +16, stride 16; size = 16 + count*16
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    emit_entry_loop(&mut f, 16, 16, &[0, 8]);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // CLOSURE → upvalues @ +16, stride 8; size = 16 + uv_count*8 (plan 113 R2).
    // `uv_count` lives at addr+8 (addr+4 is the table index), so reload local 3
    // before reusing the entry loop. Releasing each upvalue balances the
    // capture-time retain: a captured-object upvalue drops its ref, and a
    // captured CELL (a NaN-boxed OBJ_TAG_CELL since plan 114) drops the
    // closure's co-ownership of the shared slot — the cell frees when its
    // last owner (enclosing frame or sibling closure) lets go.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_CLOSURE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));
    emit_entry_loop(&mut f, 16, 8, &[0]);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // CELL → shared mutable slot for a captured-mutated `var` (plan 114):
    // [tag@0][pad@4][value@8], fixed 16 bytes. Release the owned value,
    // then free the block.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_CELL));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Load(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::Call(base + RT_RELEASE));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);

    // if size != 0 { rt_free(addr, size) }
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_FREE));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_get_index(obj: i64, idx: i64) -> i64 ──
fn emit_get_index(base: u32) -> Function {
    // locals: 2=addr(i32), 3=len(i32), 4=i(i32),
    //         5=key_addr(i32), 6=key_len(i32)
    //
    // Polymorphic indexing matches the VM:
    //   - Array/Tuple: positional access (negative-index supported).
    //   - String key on Dict/Instance/Module/String: delegate to
    //     RT_GET_FIELD with the unboxed name pointer/length. This is
    //     load-bearing for module field access when the field name's
    //     string-pool index overflows u8 and the compiler falls back
    //     from Op::GetField to LoadString + Op::GetIndex (see
    //     fai-compiler/src/compiler.rs::emit_get_field). Without this
    //     path, e.g. `string.split(...)` in a large program silently
    //     returned null and downstream calls produced garbage —
    //     plans/bug-wasm-diff-insert-crash.md bug A.
    let mut f = Function::new([
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
    ]);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(2));
    // len = mem[addr+4]
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));
    // i = idx as i32
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::LocalSet(4));
    // Check tag is array(1) or tuple(2)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(3)); // tag < 3 means string/array/tuple
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(0)); // tag > 0 means not string
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Handle negative index
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        // Bounds check
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Else);
        {
            // Return mem[addr + 8 + i*8]
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::Else);
    {
        // Not array/tuple. If idx is a string object, delegate to
        // RT_GET_FIELD so module/dict/instance/string lookups behave
        // the same as a literal-name field access.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // key_addr = obj_addr(idx); tag = mem[key_addr]
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(5));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            {
                // key_len = mem[key_addr+4]
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Load(MemArg {
                    offset: 4,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::LocalSet(6));
                // get_field(obj, key_addr+8, key_len)
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::LocalGet(5));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(6));
                f.instruction(&Instruction::Call(base + RT_GET_FIELD));
            }
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_import_module(name_ptr: i32, name_len: i32) -> i64 ──
fn emit_import_module(base: u32) -> Function {
    let mut f = Function::new([(1, ValType::I32)]); // local 2: addr
                                                    // Allocate [tag=5][name_ptr][name_len] = 12 bytes, padded to 16
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(OBJ_TAG_MODULE));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(0)); // name_ptr
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(1)); // name_len
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_get_field(obj: i64, name_ptr: i32, name_len: i32) -> i64 ──
fn emit_get_field(base: u32, ks: &KnownStrings) -> Function {
    // locals: 3=addr(i32), 4=tag(i32), 5=count(i32), 6=i(i32),
    //         7=entry_addr(i32), 8=key_addr(i32), 9=key_len(i32), 10=method_id(i32), 11=fn_addr(i32)
    let mut f = Function::new([
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I32),
    ]);
    // addr = obj_addr(obj)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(3));
    // tag = mem[addr]
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(4));

    // === Dict / Instance path ===
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // count = mem[addr+4]
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(5));
        // entry base = 8 for dict, 16 for instance
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6)); // i = 0
                                                  // Loop over entries
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= count: break with VAL_NULL
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::Br(2)); // break out of block with value
            f.instruction(&Instruction::End);
            // entry_addr = addr + entry_base + i*16
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7)); // entry_addr
                                                      // key_val = mem[entry_addr] as i64, extract string addr
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(8)); // key_addr
                                                      // key_len = mem[key_addr+4]
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Load(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::LocalSet(9));
            // Compare: str_eq(key_addr+8, key_len, name_ptr, name_len)
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(1)); // name_ptr
            f.instruction(&Instruction::LocalGet(2)); // name_len
            f.instruction(&Instruction::Call(base + RT_STR_EQ));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            {
                // Match! Return mem[entry_addr+8]
                f.instruction(&Instruction::LocalGet(7));
                f.instruction(&Instruction::I64Load(MemArg {
                    offset: 8,
                    align: 0,
                    memory_index: 0,
                }));
                f.instruction(&Instruction::Br(3)); // break out of block with value
            }
            f.instruction(&Instruction::End);
            // i++
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(6));
            f.instruction(&Instruction::Br(0)); // continue loop
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::I64Const(VAL_NULL)); // fallback
        f.instruction(&Instruction::End); // end block
    }
    f.instruction(&Instruction::Else);
    {
        // === Module path (tag=5) ===
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(OBJ_TAG_MODULE));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            // Resolve method_id from module_name + method_name
            // method_id defaults to UNKNOWN
            f.instruction(&Instruction::I32Const(METHOD_UNKNOWN));
            f.instruction(&Instruction::LocalSet(10));

            // Check "length" method (works for all modules)
            emit_method_check(&mut f, base, ks.length, 1, 2, METHOD_LENGTH, 10);
            // Check "abs"
            emit_method_check(&mut f, base, ks.abs, 1, 2, METHOD_ABS, 10);
            // Check "min"
            emit_method_check(&mut f, base, ks.min, 1, 2, METHOD_MIN, 10);
            // Check "max"
            emit_method_check(&mut f, base, ks.max, 1, 2, METHOD_MAX, 10);
            // Check "floor"
            emit_method_check(&mut f, base, ks.floor, 1, 2, METHOD_FLOOR, 10);
            // Check "ceil"
            emit_method_check(&mut f, base, ks.ceil, 1, 2, METHOD_CEIL, 10);
            // Check "round"
            emit_method_check(&mut f, base, ks.round, 1, 2, METHOD_ROUND, 10);
            // Check "sqrt"
            emit_method_check(&mut f, base, ks.sqrt, 1, 2, METHOD_SQRT, 10);
            // Check "contains"
            emit_method_check(&mut f, base, ks.contains, 1, 2, METHOD_CONTAINS, 10);
            // Check "split"
            emit_method_check(&mut f, base, ks.split, 1, 2, METHOD_SPLIT, 10);
            // Check "join"
            emit_method_check(&mut f, base, ks.join, 1, 2, METHOD_JOIN, 10);
            // Check "sort"
            emit_method_check(&mut f, base, ks.sort, 1, 2, METHOD_SORT, 10);
            // Check "getKeys"
            emit_method_check(&mut f, base, ks.get_keys, 1, 2, METHOD_GET_KEYS, 10);
            // Check "slice"
            emit_method_check(&mut f, base, ks.slice, 1, 2, METHOD_SLICE, 10);
            // Check "reverse"
            emit_method_check(&mut f, base, ks.reverse, 1, 2, METHOD_REVERSE, 10);
            // Check "toUpper"
            emit_method_check(&mut f, base, ks.to_upper, 1, 2, METHOD_TO_UPPER, 10);
            // Check "toLower"
            emit_method_check(&mut f, base, ks.to_lower, 1, 2, METHOD_TO_LOWER, 10);
            // Check "trim"
            emit_method_check(&mut f, base, ks.trim, 1, 2, METHOD_TRIM, 10);
            // Check "startsWith"
            emit_method_check(&mut f, base, ks.starts_with, 1, 2, METHOD_STARTS_WITH, 10);
            // Check "endsWith"
            emit_method_check(&mut f, base, ks.ends_with, 1, 2, METHOD_ENDS_WITH, 10);
            // Check "indexOf"
            emit_method_check(&mut f, base, ks.index_of, 1, 2, METHOD_INDEX_OF, 10);
            // Check "substring"
            emit_method_check(&mut f, base, ks.substring, 1, 2, METHOD_SUBSTRING, 10);
            // Check "repeat"
            emit_method_check(&mut f, base, ks.repeat, 1, 2, METHOD_REPEAT, 10);
            // Check "replace"
            emit_method_check(&mut f, base, ks.replace, 1, 2, METHOD_REPLACE, 10);
            // Check "pow"
            emit_method_check(&mut f, base, ks.pow, 1, 2, METHOD_POW, 10);
            // Check "append"
            emit_method_check(&mut f, base, ks.append, 1, 2, METHOD_APPEND, 10);
            // Check "isEmpty"
            emit_method_check(&mut f, base, ks.is_empty, 1, 2, METHOD_IS_EMPTY, 10);
            // Check "first" / "last"
            emit_method_check(&mut f, base, ks.first, 1, 2, METHOD_FIRST, 10);
            emit_method_check(&mut f, base, ks.last, 1, 2, METHOD_LAST, 10);
            // File/time/random/sleep methods
            emit_method_check(&mut f, base, ks.read, 1, 2, METHOD_FILE_READ, 10);
            emit_method_check(&mut f, base, ks.write, 1, 2, METHOD_FILE_WRITE, 10);
            emit_method_check(&mut f, base, ks.exists, 1, 2, METHOD_FILE_EXISTS, 10);
            emit_method_check(&mut f, base, ks.now, 1, 2, METHOD_TIME_NOW, 10);
            emit_method_check(&mut f, base, ks.unix, 1, 2, METHOD_TIME_UNIX, 10);
            emit_method_check(&mut f, base, ks.random, 1, 2, METHOD_RANDOM, 10);
            emit_method_check(&mut f, base, ks.sleep, 1, 2, METHOD_SLEEP, 10);
            // std.http.server methods. `listen` uses the router accept loop;
            // `text`/`html`/`json`/`ok`/`redirect` all build response dicts
            // via IMPORT_HTTP_SERVER_RESPONSE with different `kind`
            // discriminants (see RESPONSE_KIND_*).
            emit_method_check(&mut f, base, ks.listen, 1, 2, METHOD_SERVER_LISTEN, 10);
            emit_method_check(&mut f, base, ks.text, 1, 2, METHOD_SERVER_TEXT, 10);
            emit_method_check(&mut f, base, ks.html, 1, 2, METHOD_SERVER_HTML, 10);
            emit_method_check(&mut f, base, ks.json_fn, 1, 2, METHOD_SERVER_JSON, 10);
            emit_method_check(&mut f, base, ks.ok, 1, 2, METHOD_SERVER_OK, 10);
            emit_method_check(&mut f, base, ks.redirect, 1, 2, METHOD_SERVER_REDIRECT, 10);
            emit_method_check(&mut f, base, ks.router, 1, 2, METHOD_SERVER_ROUTER, 10);
            emit_method_check(&mut f, base, ks.get, 1, 2, METHOD_SERVER_GET, 10);
            emit_method_check(&mut f, base, ks.post, 1, 2, METHOD_SERVER_POST, 10);
            emit_method_check(
                &mut f,
                base,
                ks.serve_files,
                1,
                2,
                METHOD_SERVER_SERVE_FILES,
                10,
            );
            // std.dictionary typed accessors — all three share a body.
            emit_method_check(&mut f, base, ks.get_string, 1, 2, METHOD_GET_STRING, 10);
            emit_method_check(&mut f, base, ks.get_int, 1, 2, METHOD_GET_INT, 10);
            emit_method_check(&mut f, base, ks.get_bool, 1, 2, METHOD_GET_BOOL, 10);
            emit_method_check(&mut f, base, ks.trim_start, 1, 2, METHOD_TRIM_START, 10);
            emit_method_check(&mut f, base, ks.trim_end, 1, 2, METHOD_TRIM_END, 10);
            // std.json methods — the compile-time sentinel path in
            // emit_get_field_by_idx only triggers when the module reg's
            // origin is Global(...). For top-level files this is true,
            // but inside a library-compiled function (e.g. Forui.rpc's
            // parseFnName) the `json` reference is loaded from the
            // globals area via plain GetGlobal + GetField — falling
            // through to RT_GET_FIELD, which needs a real method_id.
            emit_method_check(&mut f, base, ks.parse, 1, 2, METHOD_JSON_PARSE, 10);
            emit_method_check(&mut f, base, ks.stringify, 1, 2, METHOD_JSON_STRINGIFY, 10);
            // std.storage methods
            emit_method_check(&mut f, base, ks.storage_get, 1, 2, METHOD_STORAGE_GET, 10);
            emit_method_check(&mut f, base, ks.storage_set, 1, 2, METHOD_STORAGE_SET, 10);
            emit_method_check(
                &mut f,
                base,
                ks.storage_remove,
                1,
                2,
                METHOD_STORAGE_REMOVE,
                10,
            );
            emit_method_check(
                &mut f,
                base,
                ks.storage_clear,
                1,
                2,
                METHOD_STORAGE_CLEAR,
                10,
            );

            // Allocate NativeFn: [tag=6][method_id] = 8 bytes padded to 16
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::Call(base + RT_ALLOC));
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(OBJ_TAG_NATIVE_FN));
            f.instruction(&Instruction::I32Store(mem0()));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(10)); // method_id
            f.instruction(&Instruction::I32Store(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        }
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

/// Emit a `server.<kind>(...)` body that bundles up a response `kind`,
/// status, and body string, then calls `IMPORT_HTTP_SERVER_RESPONSE`
/// and returns its result.
///
/// - `body_is_arg1 = true` matches `text/html/json/redirect(status, body)`
///   where arg0 is the status Int and arg1 is the body String.
/// - `body_is_arg1 = false` matches `ok(body)` where arg0 is the body
///   String and status is hardcoded to 200.
///
/// Scratch locals used: 7 (body_addr), 8 (body_len), 9 (body_ptr).
/// These are within the i32 temp block reserved by emit_call_native.
fn emit_server_response_call(
    f: &mut Function,
    _base: u32,
    kind: i32,
    body_is_arg1: bool,
    import_remap: &[Option<u32>],
) {
    let body_local = if body_is_arg1 { 6 } else { 5 };

    // body_addr = obj_addr(body_val)
    f.instruction(&Instruction::LocalGet(body_local));
    f.instruction(&Instruction::Call(_base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(7));
    // body_len = mem[body_addr + 4]
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));
    // body_ptr = body_addr + 8
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(9));

    // args: (kind, status, body_ptr, body_len)
    f.instruction(&Instruction::I32Const(kind));
    if body_is_arg1 {
        // status = (arg0 & 0xFFFFFFFF) as i32
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
    } else {
        f.instruction(&Instruction::I32Const(200));
    }
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::LocalGet(8));
    emit_import_call(f, IMPORT_HTTP_SERVER_RESPONSE, import_remap);
    f.instruction(&Instruction::Return);
}

/// Emit a method name check: if str_eq(name_ptr, name_len, known_ptr, known_len) then method_id = id
fn emit_method_check(
    f: &mut Function,
    base: u32,
    known: (u32, u32),
    name_ptr_local: u32,
    name_len_local: u32,
    method_id: i32,
    result_local: u32,
) {
    let (kptr, klen) = known;
    f.instruction(&Instruction::LocalGet(name_ptr_local));
    f.instruction(&Instruction::LocalGet(name_len_local));
    f.instruction(&Instruction::I32Const(kptr as i32));
    f.instruction(&Instruction::I32Const(klen as i32));
    f.instruction(&Instruction::Call(base + RT_STR_EQ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(method_id));
    f.instruction(&Instruction::LocalSet(result_local));
    f.instruction(&Instruction::End);
}

// ── $rt_set_field(obj: i64, name_ptr: i32, name_len: i32, val: i64) -> void ──
fn emit_set_field(base: u32, import_remap: &[Option<u32>]) -> Function {
    // locals: 4=addr(i32), 5=count(i32), 6=i(i32), 7=entry_addr(i32),
    //         8=key_addr(i32), 9=key_len(i32), 10=entry_base(i32),
    //         11=is_instance(i32), 12=cap(i32), 13=new_addr(i32),
    //         14=gi(i32 grow-copy index), 15=src_entry(i32),
    //         16=dst_entry(i32). Returns i64 (the dict pointer).
    let mut f = Function::new([(13, ValType::I32)]);
    let off4 = MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    };
    let off8 = MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    };
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(4));
    // Accept tag == DICT (3) or INSTANCE (7). Mirrors RT_GET_FIELD's
    // combined dict/instance path. Before this covered Instance,
    // writes to Instance-tagged objects (produced by the
    // RT_CALL_NATIVE tuple-constructor path when a typedef is called
    // via a variable) were silently dropped.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0)); // not a dict/instance → return v unchanged
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // is_instance = (tag == 7)
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::LocalSet(11));
    // entry_base = 16 for Instance (type_name at offset 8 occupies
    // 8 bytes, so entries start at 16), 8 for Dict.
    f.instruction(&Instruction::LocalGet(11));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::LocalSet(10));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalSet(10));
    f.instruction(&Instruction::End);
    // count = mem[addr+4]
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(6));
    // Loop
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1)); // break
                                              // entry_addr = addr + entry_base + i*16
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        // key string comparison
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(base + RT_STR_EQ));
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            // Match: release the value this entry currently holds (the dict
            // owned it — RC, plan 113 R1; RT_RELEASE's is_obj guard skips a
            // primitive), then write the new value. The caller has already
            // retained `val` when it was a borrowed source.
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I64Load(MemArg {
                offset: 8,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(3)); // val
            f.instruction(&Instruction::I64Store(off8));
            f.instruction(&Instruction::LocalGet(0)); // address unchanged → return v
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    // Key not found. Only Dict grows — Instance has a fixed shape
    // defined by its typedef, so an unknown-field write is silently
    // ignored (matching the VM which also skips unknown instance
    // fields since the checker rejects them at compile time).
    f.instruction(&Instruction::LocalGet(11));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(0)); // instance: no append → return v
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    // Dict append. The block was sized for `cap` entries
    // (`cap = (logical_size - 8) / 16`, logical size in the rc-prefix
    // word at addr-4). If it's full, grow: allocate a bigger block,
    // shallow-copy the header + entries, and RETAIN each moved key/value
    // so both blocks hold a ref. We do NOT free the old block here — the
    // caller's `var` reassignment releases the old dict (which recursively
    // releases its children, dropping them back to the count the new
    // block now owns). That leaves the new block's children correctly
    // owned; the only cost is the old header block leaking by one rc on
    // a grow (sound and bounded — far better than the silent heap
    // overflow this replaces).
    // cap = (mem[addr-4] - 8) / 16
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32DivU);
    f.instruction(&Instruction::LocalSet(12));
    // Sanity guard: a plausible dict capacity is small. A huge `cap`
    // means the size word at addr-4 was garbage (set() called on a
    // non-dict / stale / mis-typed pointer), and growing would request
    // gigabytes and exhaust memory. Trap with the bad capacity + size
    // word and a backtrace instead — names the caller passing the bad
    // value. (1<<24 = 16M entries — orders beyond any real dict.)
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Const(1 << 24));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    emit_trap_report_unreachable(
        &mut f,
        import_remap,
        TRAP_DICT_CAP_INSANE,
        |f| {
            f.instruction(&Instruction::LocalGet(12)); // computed capacity
            f.instruction(&Instruction::I64ExtendI32U);
        },
        |f| {
            // raw size word at addr-4
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(4));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Load(mem0()));
            f.instruction(&Instruction::I64ExtendI32U);
        },
    );
    f.instruction(&Instruction::End);
    // if count >= cap: grow
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // new_addr = alloc(8 + (cap*2)*16). cap is always >= 16 for dicts
        // (literal floor), so cap*2 stays comfortably bounded.
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(32)); // 2 * 16
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(13));
        // header: tag + count
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(OBJ_TAG_DICT));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Store(off4));
        // copy + retain each of `count` entries
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(14));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            // src_entry = addr + 8 + gi*16; dst_entry = new_addr + 8 + gi*16
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(16));
            // dst.key = src.key; retain it
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::I64Store(mem0()));
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Drop);
            // dst.val = src.val; retain it
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Load(off8));
            f.instruction(&Instruction::I64Store(off8));
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::I64Load(off8));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Drop);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(14));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
                                          // addr = new_addr (subsequent append + return use the grown block)
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalSet(4));
    }
    f.instruction(&Instruction::End); // grow
                                      // Append new entry at addr + 8 + count*16.
    f.instruction(&Instruction::LocalGet(4)); // addr
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(5)); // count
    f.instruction(&Instruction::I32Const(16));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(7)); // entry_addr = addr + 8 + count*16
                                              // Write key (allocate string object)
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::LocalGet(1)); // name_ptr
    f.instruction(&Instruction::LocalGet(2)); // name_len
    f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
    f.instruction(&Instruction::I64Store(mem0())); // store key at entry_addr
                                                   // Write value
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::LocalGet(3)); // val
    f.instruction(&Instruction::I64Store(off8)); // store val at entry_addr+8
                                                 // Increment count
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Store(off4));
    // Return the (possibly new) dict pointer, NaN-boxed.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::End); // function end
    f
}

// ── $rt_value_to_str(val: i64) -> i64 ──
fn emit_value_to_str(base: u32, ks: &KnownStrings, import_remap: &[Option<u32>]) -> Function {
    // locals: 1=addr(i32), 2=len(i32)
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I32)]);

    // If already a string object, return as-is
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Int: use itoa
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_INT));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
        ValType::I64,
    )));
    {
        // Reserve 32 bytes of scratch space by bumping heap_ptr
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(1)); // scratch addr
                                                  // Write digits to scratch
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::Call(base + RT_ITOA));
        f.instruction(&Instruction::LocalSet(2)); // len
                                                  // Allocate string object from scratch
        f.instruction(&Instruction::LocalGet(1)); // src_ptr
        f.instruction(&Instruction::LocalGet(2)); // len
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        // Free the itoa scratch: alloc_string copied the bytes into a fresh
        // String, so the 32-byte scratch is dead. Returning it to the free list
        // lets the next int→str conversion reuse it instead of bumping the heap
        // every time (otherwise each `toString(n)` / `"" + n` leaks 32 bytes).
        // The result String is on the stack; rt_free pushes nothing.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::Call(base + RT_FREE));
    }
    f.instruction(&Instruction::Else);
    {
        // Bool true
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Const(VAL_TRUE));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
            ValType::I64,
        )));
        {
            f.instruction(&Instruction::I32Const(ks.str_true.0 as i32));
            f.instruction(&Instruction::I32Const(ks.str_true.1 as i32));
            f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        }
        f.instruction(&Instruction::Else);
        {
            // Bool false
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::I64Const(VAL_FALSE));
            f.instruction(&Instruction::I64Eq);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                ValType::I64,
            )));
            {
                f.instruction(&Instruction::I32Const(ks.str_false.0 as i32));
                f.instruction(&Instruction::I32Const(ks.str_false.1 as i32));
                f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
            }
            f.instruction(&Instruction::Else);
            {
                // Null
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64Const(VAL_NULL));
                f.instruction(&Instruction::I64Eq);
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I64,
                )));
                {
                    f.instruction(&Instruction::I32Const(ks.str_null.0 as i32));
                    f.instruction(&Instruction::I32Const(ks.str_null.1 as i32));
                    f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                }
                f.instruction(&Instruction::Else);
                {
                    // Float: use host import for proper formatting
                    f.instruction(&Instruction::LocalGet(0));
                    f.instruction(&Instruction::Call(base + RT_IS_FLOAT));
                    f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                        ValType::I64,
                    )));
                    {
                        // Allocate scratch buffer for the string
                        f.instruction(&Instruction::I32Const(64));
                        f.instruction(&Instruction::Call(base + RT_ALLOC));
                        f.instruction(&Instruction::LocalSet(1)); // buf_ptr
                                                                  // Call host: float_to_str(f64_value, buf_ptr) -> len
                        f.instruction(&Instruction::LocalGet(0));
                        f.instruction(&Instruction::F64ReinterpretI64);
                        f.instruction(&Instruction::LocalGet(1));
                        emit_import_call(&mut f, IMPORT_FLOAT_TO_STR, import_remap);
                        f.instruction(&Instruction::LocalSet(2)); // len
                                                                  // Allocate string object from buffer
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::LocalGet(2));
                        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                        // Free the 64-byte float-format scratch (see int path
                        // above): alloc_string copied it out, so reclaim it for
                        // the next conversion instead of leaking it.
                        f.instruction(&Instruction::LocalGet(1));
                        f.instruction(&Instruction::I32Const(64));
                        f.instruction(&Instruction::Call(base + RT_FREE));
                    }
                    f.instruction(&Instruction::Else);
                    {
                        // Default: empty string
                        f.instruction(&Instruction::I32Const(0));
                        f.instruction(&Instruction::I32Const(0));
                        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                    }
                    f.instruction(&Instruction::End);
                }
                f.instruction(&Instruction::End);
            }
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_print_val_new(val: i64) -> void ──
fn emit_print_val_new(base: u32, import_remap: &[Option<u32>]) -> Function {
    let mut f = Function::new([(1, ValType::I32), (1, ValType::I64)]);

    // Existing String values are borrowed by print; do not release them.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_IS_OBJ));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(1));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Load(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            emit_import_call(&mut f, IMPORT_PRINT, import_remap);
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);

    // Non-string values stringify to a fresh String owned by this helper.
    // Release it after env.print copies the bytes.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
    f.instruction(&Instruction::LocalTee(2));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // ptr = addr + 8
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    // len = mem[addr+4]
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    // call env.print(ptr, len)  — import index 0
    emit_import_call(&mut f, IMPORT_PRINT, import_remap);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::Call(base + RT_RELEASE));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_call_native(callee: i64, args_ptr: i32, arg_count: i32) -> i64 ──
/// Emit a loop that retains every element of a freshly built array (RC, plan
/// 113 R1). A runtime array builder (`append`, `sort`, `slice`, `reverse`,
/// `getKeys`, …) shallow-copies element references out of its source(s); the
/// new array co-owns each, so it must retain them or releasing the source later
/// deep-frees elements this array still points at. `dst_local` holds the array
/// payload base (tag@0), `count_local` the element count, `idx_local` is a
/// scratch i32 the caller guarantees is free at this point. RT_RETAIN's is_obj
/// guard makes this a no-op for primitive elements.
fn emit_retain_array_elems(
    f: &mut Function,
    base: u32,
    dst_local: u32,
    count_local: u32,
    idx_local: u32,
) {
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::LocalGet(count_local));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(dst_local));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I64Load(mem0()));
    f.instruction(&Instruction::Call(base + RT_RETAIN));
    f.instruction(&Instruction::Drop);
    f.instruction(&Instruction::LocalGet(idx_local));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(idx_local));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

fn emit_call_native(base: u32, import_remap: &[Option<u32>]) -> Function {
    // locals: 3=addr(i32), 4=method_id(i32), 5=arg0(i64), 6=arg1(i64),
    // 7..14=i32 temps, 15..16=i64 temps, 17..18=extra i32 temps,
    // 19=extra i64 temp.
    //
    // The last two i32 temps (17, 18) were added for METHOD_REPLACE
    // which needs more persistent state than 7..14 provide (6 arg-field
    // locals + output_addr + out_i + i + match_flag + inner-j = 11
    // i32s). Free to reuse in other methods that need extra scratch —
    // document your use at the method body.
    let mut f = Function::new([
        (1, ValType::I32),
        (1, ValType::I32),
        (1, ValType::I64),
        (1, ValType::I64),
        (8, ValType::I32),
        (2, ValType::I64),
        (2, ValType::I32),
        (1, ValType::I64),
    ]);
    // addr = obj_addr(callee)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(3));
    // tag = mem[addr]
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::LocalSet(7));

    // Tuple type constructor path.
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(OBJ_TAG_TUPLE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // tuple_count = mem[addr+4]
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));

        // Need at least (name, fields)
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // fields_arr_val = tuple[1]
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(15));
        // defaults_arr_val = tuple[2] or null
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::LocalSet(16));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 24,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(16));
        f.instruction(&Instruction::End);

        // field_arr_addr / field_count
        f.instruction(&Instruction::LocalGet(15));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10));

        // Allocate instance: [tag][count][type_name:i64][entries...]
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(11)); // instance addr

        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(OBJ_TAG_INSTANCE));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I64Store(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));

        // defaults_count / defaults_addr
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(12)); // defaults addr
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(13)); // defaults count
        f.instruction(&Instruction::LocalGet(16));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::I64Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(16));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::End);

        // for i in 0..field_count
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(14));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // entry addr = instance + 16 + i*16
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(7));

            // key = fields_arr[i]
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::I64Store(mem0()));

            // value = arg[i] else default[i] else null
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::I64Eq);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::I64Const(VAL_NULL));
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);

            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::Drop);
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Store(MemArg {
                offset: 8,
                align: 0,
                memory_index: 0,
            }));

            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(14));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // At this point the callee is not a Tuple (handled above) and
    // not a Closure (handled by the caller in emit_native_call
    // before reaching RT_CALL_NATIVE). The only valid remaining
    // shape is NativeFn; anything else — Dict, Array, String,
    // Instance, Module, or a non-object value whose unboxed address
    // happened to land on some arbitrary memory — is not callable.
    //
    // Before this trap, the fallthrough silently returned VAL_NULL,
    // which masked bugs: callers would continue with null and trap
    // or misbehave somewhere downstream instead of at the call site.
    // The VM errors with "not callable" in this same case, so
    // trapping matches VM semantics and gives a stack trace that
    // points at the real problem. See plans/98-wasm-codegen-hardening.md
    // step 3 for the sibling pattern on unimplemented natives.
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(mem0()));
    f.instruction(&Instruction::I32Const(OBJ_TAG_NATIVE_FN));
    f.instruction(&Instruction::I32Ne);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    // method_id = mem[addr+4]
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(4));
    // Read arg0 (if available)
    f.instruction(&Instruction::LocalGet(2)); // arg_count
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(1)); // args_ptr
    f.instruction(&Instruction::I64Load(mem0()));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::End);
    // Read arg1 (if available)
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32GtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I64Load(MemArg {
        offset: 8,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::End);

    // Dispatch on method_id
    // METHOD_LENGTH = 0
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Eqz); // method_id == 0
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // length: return mem[obj_addr(arg0) + 4] as int
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_ABS = 1
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_ABS));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(7));
        // if negative, negate
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_MIN = 2
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_MIN));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        // select: if a < b then a else b
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::Select);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_MAX = 3
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_MAX));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::Select);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_APPEND = 6
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_APPEND));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // append: allocate a new array with count + 1 and copy existing items + arg1
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // src array addr

        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // count

        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9)); // dest addr

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Store(mem0()));

        // RC (plan 113 R1): the new array co-owns every element it now holds —
        // the `count` references shallow-copied from the source plus the
        // appended one. Retain each (local 10 = loop index, n = count + 1), or
        // releasing the source array later deep-frees elements this array still
        // points at. RT_RETAIN's is_obj guard skips primitives.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::Call(base + RT_RETAIN));
        f.instruction(&Instruction::Drop);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_APPEND_MOVE = 57 — assignment-position append. The codegen
    // emits this only for `xs = append(xs, x)`-shaped reassignments of an
    // owned binding, so the source value is dead once the call returns.
    // Fast path (rc == 1, spare block capacity): write the element in
    // place, bump count, retain the element (the array owns it) and the
    // array itself (the owned return; the caller's release of the old
    // binding drops it back to 1). Slow path (shared or full): copy like
    // METHOD_APPEND but over-allocate the destination to
    // max(2 × count, 4) elements so subsequent in-place appends are
    // amortized O(1). RT_ALLOC stamps the over-allocated logical size at
    // obj-4, which RT_RELEASE/RT_FREE already honor (same discipline as
    // dict spare-capacity growth, plan 115).
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_APPEND_MOVE));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // src array addr

        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // count

        // needed = 8 (header) + (count + 1) * 8
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(12));

        // fast = tag == ARRAY  &  rc == 1  &  needed <= stamped logical size
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Load(mem0())); // rc word at obj-8
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Load(mem0())); // logical size at obj-4
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        {
            // mem[src + 8 + count*8] = elem
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I64Store(MemArg {
                offset: 8,
                align: 0,
                memory_index: 0,
            }));
            // count += 1
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Store(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            // The array owns the appended element (RT_RETAIN's is_obj
            // guard skips primitives).
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Drop);
            // Owned return: +1 on the array itself; RT_RETAIN returns
            // the value, which is exactly the call's result.
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::Call(base + RT_RETAIN));
            f.instruction(&Instruction::Return);
        }
        f.instruction(&Instruction::End);

        // Slow path: copy into a block sized max(2 × count, 4) entries.
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9)); // dest addr

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Store(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));

        // The new array co-owns every element it now holds — the `count`
        // references shallow-copied from the source plus the appended one
        // (same discipline as METHOD_APPEND; RC, plan 113 R1).
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::Call(base + RT_RETAIN));
        f.instruction(&Instruction::Drop);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_IS_EMPTY = 7
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(METHOD_IS_EMPTY));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
    }
    f.instruction(&Instruction::End);

    // METHOD_FIRST = 55: `array.first(arr)` → first element or
    // `null` if the array is empty. Layout assumes OBJ_TAG_ARRAY
    // so tag=1, count@4, items start at offset 8. Reading an empty
    // string or dict via `first` would read garbage — the checker
    // types the method as `Array<T> → T?`, so type safety prevents
    // the misuse.
    emit_native_method_dispatch(&mut f, base, METHOD_FIRST, |f, base| {
        // addr = obj_addr(arg0)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // count = mem[addr + 4]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Return items[0] — i64 at mem[addr + 8].
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::Return);
    });

    // METHOD_LAST = 56: `array.last(arr)` → last element or `null`
    // if empty. Same shape as METHOD_FIRST; just reads the tail.
    emit_native_method_dispatch(&mut f, base, METHOD_LAST, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // count = mem[addr + 4]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Load at addr + 8 + (count - 1) * 8.
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::Return);
    });

    // METHOD_FILE_READ = 8: file.read(path_str) -> string_obj
    // Extracts path string, calls host read_file, wraps result as string obj.
    emit_native_method_dispatch(&mut f, base, METHOD_FILE_READ, |f, base| {
        // Get path string address
        f.instruction(&Instruction::LocalGet(5)); // arg0 = path string obj
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // path_obj_addr
                                                  // Allocate 64KB scratch buffer for file content
        f.instruction(&Instruction::I32Const(65536));
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalTee(3)); // buf_ptr (reuse local 3)
                                                  // Call host: read_file(path_ptr, path_len, buf_ptr) -> content_len
        f.instruction(&Instruction::LocalGet(7)); // path_obj_addr
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add); // path data ptr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        })); // path len
        f.instruction(&Instruction::LocalGet(3)); // buf_ptr
        emit_import_call(f, IMPORT_READ_FILE, import_remap);
        f.instruction(&Instruction::LocalTee(4)); // content_len or -1
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Wrap buf content as string object
        f.instruction(&Instruction::LocalGet(3)); // buf_ptr (source data)
        f.instruction(&Instruction::LocalGet(4)); // content_len
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        f.instruction(&Instruction::Return);
    });

    // METHOD_FILE_WRITE = 9: file.write(path_str, content_str) -> void
    emit_native_method_dispatch(&mut f, base, METHOD_FILE_WRITE, |f, base| {
        // Extract path
        f.instruction(&Instruction::LocalGet(5)); // arg0 = path
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // Extract content
        f.instruction(&Instruction::LocalGet(6)); // arg1 = content
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(3));
        // Call host: write_file(path_ptr, path_len, content_ptr, content_len)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        emit_import_call(f, IMPORT_WRITE_FILE, import_remap);
        f.instruction(&Instruction::Drop);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_FILE_EXISTS = 10: file.exists(path_str) -> Bool.
    // Mirrors the path-ptr/path-len convention used by METHOD_FILE_READ.
    // IMPORT_FILE_EXISTS sits at index 29 and must be routed through the
    // remap table because server imports at 16-17 and 20-24 are disabled
    // on `wasm-html` / `wasm` targets, which shifts higher import indices.
    let ir_for_exists = import_remap;
    emit_native_method_dispatch(&mut f, base, METHOD_FILE_EXISTS, |f, base| {
        // path_obj_addr = obj_addr(arg0)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // Host: file_exists(path_ptr, path_len) -> i32 (1/0)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add); // path data ptr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        })); // path len
        emit_import_call(f, IMPORT_FILE_EXISTS, ir_for_exists);
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
    });

    // METHOD_TIME_NOW = 11
    emit_native_method_dispatch(&mut f, base, METHOD_TIME_NOW, |f, _base| {
        emit_import_call(f, IMPORT_NOW_MS, import_remap);
        f.instruction(&Instruction::Call(_base + RT_MAKE_FLOAT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_TIME_UNIX = 12: time.unix() -> Int (seconds since epoch).
    // VM parity: `native_time_unix` returns `Value::int`. We compute
    // `now_ms() / 1000.0`, truncate to i32, and NaN-box as an Int.
    emit_native_method_dispatch(&mut f, base, METHOD_TIME_UNIX, |f, _base| {
        emit_import_call(f, IMPORT_NOW_MS, import_remap);
        f.instruction(&Instruction::F64Const(1000.0));
        f.instruction(&Instruction::F64Div);
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::Call(_base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_RANDOM = 13
    emit_native_method_dispatch(&mut f, base, METHOD_RANDOM, |f, _base| {
        emit_import_call(f, IMPORT_RANDOM, import_remap);
        f.instruction(&Instruction::Call(_base + RT_MAKE_FLOAT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SLEEP = 14
    emit_native_method_dispatch(&mut f, base, METHOD_SLEEP, |f, _base| {
        // arg0 is NaN-boxed float (ms)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(_base + RT_AS_NUMBER));
        emit_import_call(f, IMPORT_SLEEP_MS, import_remap);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_FLOOR = 4: floor(x) -> Int
    emit_native_method_dispatch(&mut f, base, METHOD_FLOOR, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::F64Floor);
        f.instruction(&Instruction::I32TruncSatF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_CEIL = 5: ceil(x) -> Int
    emit_native_method_dispatch(&mut f, base, METHOD_CEIL, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::F64Ceil);
        f.instruction(&Instruction::I32TruncSatF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_ROUND = 17: round(x) -> Int
    emit_native_method_dispatch(&mut f, base, METHOD_ROUND, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::F64Nearest);
        f.instruction(&Instruction::I32TruncSatF64S);
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SQRT = 18: sqrt(x) -> Float
    emit_native_method_dispatch(&mut f, base, METHOD_SQRT, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::F64Sqrt);
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_CONTAINS = 19: contains(haystack_str, needle_str) -> Bool
    emit_native_method_dispatch(&mut f, base, METHOD_CONTAINS, |f, base| {
        // Dispatch on container tag first. Tag 0 = String → fall
        // through to the byte-scan below. Tag 1 = Array → scan items
        // by i64 bit-equality.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_array_contains_body(f, base);
        f.instruction(&Instruction::End);

        // Extract haystack: arg0 (local 5) is NaN-boxed string
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // haystack_addr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // haystack_len
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7)); // haystack_ptr (now points to data)

        // Extract needle: arg1 (local 6) is NaN-boxed string
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9)); // needle_addr
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10)); // needle_len
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(9)); // needle_ptr

        // If needle_len == 0, return true
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_TRUE));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // If needle_len > haystack_len, return false
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_FALSE));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // Loop i from 0 to haystack_len - needle_len (inclusive)
        // local 11 = i, local 12 = limit (haystack_len - needle_len + 1)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(11)); // i = 0
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(12)); // limit

        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= limit: break (not found)
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // Compare needle bytes at haystack[i..i+needle_len]
            f.instruction(&Instruction::LocalGet(7)); // haystack_ptr
            f.instruction(&Instruction::LocalGet(11)); // + i
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10)); // needle_len
            f.instruction(&Instruction::LocalGet(9)); // needle_ptr
            f.instruction(&Instruction::LocalGet(10)); // needle_len
            f.instruction(&Instruction::Call(base + RT_STR_EQ));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::I64Const(VAL_TRUE));
            f.instruction(&Instruction::Return);
            f.instruction(&Instruction::End);

            // i++
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::Br(0)); // continue
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::End); // end block

        f.instruction(&Instruction::I64Const(VAL_FALSE));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SPLIT = 20: split(text_str, sep_str) -> Array<String>
    // Uses locals 7-14 for working data
    emit_native_method_dispatch(&mut f, base, METHOD_SPLIT, |f, base| {
        // Extract text string
        f.instruction(&Instruction::LocalGet(5)); // arg0 = text
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // text_obj_addr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // text_len
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7)); // text_ptr

        // Extract separator string
        f.instruction(&Instruction::LocalGet(6)); // arg1 = sep
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9)); // sep_obj_addr
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10)); // sep_len
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(9)); // sep_ptr

        // First pass: count splits to know array size
        // local 11 = pos, local 12 = count
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(11)); // pos = 0
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(12)); // count = 1 (at least one segment)

        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if pos + sep_len > text_len: break. Written as an addition
            // rather than `pos > text_len - sep_len` because the subtraction
            // is unsigned and underflows when text_len < sep_len (e.g.
            // split("", "/")), making the guard never fire and the scan walk
            // off the end of memory.
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::BrIf(1)); // break

            // if str_eq(text_ptr+pos, sep_len, sep_ptr, sep_len)
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::Call(base + RT_STR_EQ));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            {
                // count++, pos += sep_len
                f.instruction(&Instruction::LocalGet(12));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(12));
                f.instruction(&Instruction::LocalGet(11));
                f.instruction(&Instruction::LocalGet(10));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(11));
                f.instruction(&Instruction::Br(1)); // continue
            }
            f.instruction(&Instruction::End);

            // pos++
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::Br(0)); // continue
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::End); // end block

        // Allocate array: [tag=1][count][items...] = 8 + count*8 bytes
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(13)); // arr_addr
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // Second pass: fill array with substrings
        // local 11 = pos (scan), local 14 = start (segment start), local 12 = arr_index
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(11)); // pos = 0
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(14)); // start = 0
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(12)); // arr_index = 0

        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if pos + sep_len > text_len: break (emit final segment after
            // loop). Addition form avoids the unsigned underflow that the
            // `pos > text_len - sep_len` form hits when text_len < sep_len.
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::BrIf(1));

            // if str_eq(text_ptr+pos, sep_len, sep_ptr, sep_len)
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::Call(base + RT_STR_EQ));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            {
                // Emit segment [start..pos] as string into array
                f.instruction(&Instruction::LocalGet(13)); // arr_addr
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(12)); // arr_index
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Mul);
                f.instruction(&Instruction::I32Add); // slot addr

                f.instruction(&Instruction::LocalGet(7)); // text_ptr + start
                f.instruction(&Instruction::LocalGet(14));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(11)); // len = pos - start
                f.instruction(&Instruction::LocalGet(14));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
                f.instruction(&Instruction::I64Store(mem0())); // store in slot

                // arr_index++
                f.instruction(&Instruction::LocalGet(12));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(12));

                // start = pos + sep_len, pos = start
                f.instruction(&Instruction::LocalGet(11));
                f.instruction(&Instruction::LocalGet(10));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalTee(14)); // start
                f.instruction(&Instruction::LocalSet(11)); // pos
                f.instruction(&Instruction::Br(1)); // continue
            }
            f.instruction(&Instruction::End);

            // pos++
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::Br(0)); // continue
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::End); // end block

        // Emit final segment [start..text_len]
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add); // slot addr

        f.instruction(&Instruction::LocalGet(7)); // text_ptr + start
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8)); // len = text_len - start
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        f.instruction(&Instruction::I64Store(mem0()));

        // Return NaN-boxed array
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_JOIN = 21: join(arr, sep_str) -> String
    emit_native_method_dispatch(&mut f, base, METHOD_JOIN, |f, base| {
        // Extract array: arg0 (local 5)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // arr_addr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // arr_count

        // Extract separator string
        f.instruction(&Instruction::LocalGet(6)); // arg1 = sep
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9)); // sep_obj_addr
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10)); // sep_len
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(9)); // sep_ptr

        // If empty array, return empty string
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // Start with first element as result. It is borrowed from the array
        // until the first concat creates a fresh accumulator.
        // local 15 = accumulator (i64 NaN-boxed string)
        // local 17 = accumulator-owned flag (i32)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 8,
            align: 0,
            memory_index: 0,
        })); // arr[0]
        f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
        f.instruction(&Instruction::LocalSet(15)); // result = toString(arr[0])
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(17));

        // local 11 = i (loop from 1)
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(11));

        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= count: break
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // temp = concat(result, sep_string). The separator copy and any
            // previously owned accumulator are internal temps and can be
            // released once temp supersedes them.
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::LocalGet(9)); // sep_ptr
            f.instruction(&Instruction::LocalGet(10)); // sep_len
            f.instruction(&Instruction::Call(base + RT_ALLOC_STRING)); // NaN-boxed sep
            f.instruction(&Instruction::LocalTee(16));
            f.instruction(&Instruction::Call(base + RT_CONCAT));
            f.instruction(&Instruction::LocalSet(19));
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::LocalGet(17));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::End);

            // result = concat(result, toString(arr[i]))
            f.instruction(&Instruction::LocalGet(19));
            f.instruction(&Instruction::LocalGet(7)); // arr_addr
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(11)); // i
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0())); // arr[i]
            f.instruction(&Instruction::Call(base + RT_VALUE_TO_STR));
            f.instruction(&Instruction::Call(base + RT_CONCAT));
            f.instruction(&Instruction::LocalSet(15));
            f.instruction(&Instruction::LocalGet(19));
            f.instruction(&Instruction::Call(base + RT_RELEASE));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::LocalSet(17));

            // i++
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::Br(0)); // continue
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::End); // end block

        // One-element arrays never enter the loop, so the accumulator is still
        // borrowed from arr[0]. Retain it here to keep join's Owned result
        // contract uniform.
        f.instruction(&Instruction::LocalGet(17));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(15));
        f.instruction(&Instruction::Call(base + RT_RETAIN));
        f.instruction(&Instruction::Drop);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(15));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SORT = 22: sort(arr) -> new sorted Array (bubble sort on i32 values)
    emit_native_method_dispatch(&mut f, base, METHOD_SORT, |f, base| {
        // Extract source array
        f.instruction(&Instruction::LocalGet(5)); // arg0 = array
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // src_addr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // count

        // Allocate new array: 8 + count*8
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9)); // dst_addr
                                                  // Write tag and count
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        // Copy items from source
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });

        // RC (plan 113 R1): the sorted array co-owns every element copied
        // from the source — the MemoryCopy above shallow-copies the boxed
        // refs, so without a retain both arrays "own" the same elements
        // with one count, and releasing the source later double-frees each
        // element this array still points at. `array.slice`/`reverse`
        // already do this; `sort` was the lone gap (it predates heap-
        // element support — the old i32 compare only handled primitives,
        // which need no retain). local 14 = scratch index; the bubble-sort
        // comparison below reuses it fresh per iteration.
        emit_retain_array_elems(f, base, /*dst*/ 9, /*count*/ 8, /*idx*/ 14);

        // Bubble sort: outer loop i from 0 to count-1
        // local 10 = i (outer), local 11 = j (inner), local 12 = addr_j, local 13 = addr_j1
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10)); // i = 0
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= count - 1: break. SIGNED compare — for an empty array
            // `count - 1` underflows to -1, and an unsigned `>=` would read 0xFFFFFFFF
            // and run the loop, reading past the zero-length array (OOB). Signed: 0 >= -1
            // is true, so it breaks immediately. count/i are small non-negative.
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32GeS);
            f.instruction(&Instruction::BrIf(1));

            // Inner loop: j from 0 to count - 1 - i
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(11)); // j = 0
            f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
            {
                // if j >= count - 1 - i: break inner (signed, same underflow guard)
                f.instruction(&Instruction::LocalGet(11));
                f.instruction(&Instruction::LocalGet(8));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::LocalGet(10));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::I32GeS);
                f.instruction(&Instruction::BrIf(1));

                // addr_j = dst + 8 + j*8
                f.instruction(&Instruction::LocalGet(9));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(11));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Mul);
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(12)); // addr_j

                // addr_j1 = addr_j + 8
                f.instruction(&Instruction::LocalGet(12));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(13)); // addr_j1

                // Decide whether to swap (arr[j] should sort AFTER arr[j+1]).
                // Strings compare lexically via RT_STR_CMP; everything else
                // keeps the raw i32 compare (ints, and — as before — boxed
                // pointers for mixed/other types). Without the string case,
                // `array.sort` on strings ordered by allocation address, not
                // content — which silently broke filename-ordered migration
                // runners and any string sort. locals: 14/17 = a/b obj addr,
                // 15/16 = a/b boxed value (i64 scratch, free in sort).
                let off4 = MemArg {
                    offset: 4,
                    align: 0,
                    memory_index: 0,
                };
                f.instruction(&Instruction::LocalGet(12));
                f.instruction(&Instruction::I64Load(mem0()));
                f.instruction(&Instruction::LocalSet(15)); // a
                f.instruction(&Instruction::LocalGet(13));
                f.instruction(&Instruction::I64Load(mem0()));
                f.instruction(&Instruction::LocalSet(16)); // b
                                                           // both_str = is_str(a) && is_str(b)
                f.instruction(&Instruction::LocalGet(15));
                f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I32,
                )));
                f.instruction(&Instruction::LocalGet(15));
                f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                f.instruction(&Instruction::I32Load(mem0()));
                f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
                f.instruction(&Instruction::I32Eq);
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::LocalGet(16));
                f.instruction(&Instruction::Call(base + RT_IS_OBJ));
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I32,
                )));
                f.instruction(&Instruction::LocalGet(16));
                f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                f.instruction(&Instruction::I32Load(mem0()));
                f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
                f.instruction(&Instruction::I32Eq);
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::I32And);
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    ValType::I32,
                )));
                {
                    // RT_STR_CMP(a_ptr, a_len, b_ptr, b_len) > 0
                    f.instruction(&Instruction::LocalGet(15));
                    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                    f.instruction(&Instruction::LocalTee(14)); // a_addr
                    f.instruction(&Instruction::I32Const(8));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalGet(14));
                    f.instruction(&Instruction::I32Load(off4)); // a_len
                    f.instruction(&Instruction::LocalGet(16));
                    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
                    f.instruction(&Instruction::LocalTee(17)); // b_addr
                    f.instruction(&Instruction::I32Const(8));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalGet(17));
                    f.instruction(&Instruction::I32Load(off4)); // b_len
                    f.instruction(&Instruction::Call(base + RT_STR_CMP));
                    f.instruction(&Instruction::I32Const(0));
                    f.instruction(&Instruction::I32GtS);
                }
                f.instruction(&Instruction::Else);
                {
                    f.instruction(&Instruction::LocalGet(15));
                    f.instruction(&Instruction::I32WrapI64);
                    f.instruction(&Instruction::LocalGet(16));
                    f.instruction(&Instruction::I32WrapI64);
                    f.instruction(&Instruction::I32GtS);
                }
                f.instruction(&Instruction::End);
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                {
                    // Swap: temp = arr[j]; arr[j] = arr[j+1]; arr[j+1] = temp
                    f.instruction(&Instruction::LocalGet(12));
                    f.instruction(&Instruction::I64Load(mem0()));
                    f.instruction(&Instruction::LocalSet(15)); // temp
                    f.instruction(&Instruction::LocalGet(12));
                    f.instruction(&Instruction::LocalGet(13));
                    f.instruction(&Instruction::I64Load(mem0()));
                    f.instruction(&Instruction::I64Store(mem0()));
                    f.instruction(&Instruction::LocalGet(13));
                    f.instruction(&Instruction::LocalGet(15));
                    f.instruction(&Instruction::I64Store(mem0()));
                }
                f.instruction(&Instruction::End);

                // j++
                f.instruction(&Instruction::LocalGet(11));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(11));
                f.instruction(&Instruction::Br(0));
            }
            f.instruction(&Instruction::End); // end inner loop
            f.instruction(&Instruction::End); // end inner block

            // i++
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End); // end outer loop
        f.instruction(&Instruction::End); // end outer block

        // Return NaN-boxed array
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SLICE = 24: array.slice(arr, start, end) -> new array of
    // items `[start..end)`. Mirrors native_array_slice in fai-runtime:
    // clamps start/end to [0, len], treats end<start as empty slice.
    //
    // The call_native wrapper only pre-loads arg0 (local 5) and arg1
    // (local 6). arg2 (end) must be read from args_ptr (local 1) at
    // offset 16.
    emit_native_method_dispatch(&mut f, base, METHOD_SLICE, |f, base| {
        // src_addr
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // src_len = mem[src_addr + 4]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));

        // start = unbox(arg1) as i32
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(9));
        // end = unbox(mem[args_ptr + 16]) as i32
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(10));

        // Clamp start/end to [0, src_len] and collapse end<start to an empty
        // range — shared with METHOD_SUBSTRING via emit_clamp_range_to_len
        // (signed compares, so a wrapped/negative index can't drive an OOB).
        emit_clamp_range_to_len(f, /*start*/ 9, /*end*/ 10, /*len*/ 8);

        // count = end - start
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(11));

        // Allocate destination array: 8 + count*8 bytes.
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(12));

        // Write header: tag + count.
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // memcpy items: dst = dst_addr + 8, src = src_addr + 8 + start*8, n = count*8
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });

        // RC: the slice co-owns the element refs copied from the source.
        emit_retain_array_elems(f, base, 12, 11, 14);

        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_REVERSE = 25: array.reverse(arr) -> new array with items
    // in reverse order.
    emit_native_method_dispatch(&mut f, base, METHOD_REVERSE, |f, base| {
        // src_addr
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // count
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));

        // Allocate dest: 8 + count*8
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9));

        // Write header.
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // Loop: for i in 0..count, dst[i] = src[count - 1 - i]
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10)); // i
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // Load src[count - 1 - i]
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));
            f.instruction(&Instruction::LocalSet(15)); // tmp i64

            // Store into dst[i]
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::I64Store(mem0()));

            // i++
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        // RC: the reversed array co-owns the element refs copied from the source.
        emit_retain_array_elems(f, base, 9, 8, 14);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_TO_UPPER = 26: string.toUpper(s) -> new string with ASCII
    // lowercase letters shifted to uppercase. Non-ASCII bytes pass
    // through — this intentionally diverges from the VM's
    // str::to_uppercase for non-ASCII input (see the METHOD_TO_UPPER
    // docs at the constant definition). A full-Unicode impl would
    // require a case-folding table.
    //
    // Shape: for i in 0..len, if data[i] in b'a'..=b'z', write data[i]-32
    // into dst; else write data[i] unchanged.
    emit_string_case_shift(&mut f, base, METHOD_TO_UPPER, /* to_upper */ true);

    // METHOD_TO_LOWER = 27: ASCII-only mirror of METHOD_TO_UPPER.
    emit_string_case_shift(&mut f, base, METHOD_TO_LOWER, /* to_upper */ false);

    // METHOD_TRIM / METHOD_TRIM_START / METHOD_TRIM_END — all share the
    // same body, gated by two flags passed into the helper.
    emit_native_method_dispatch(&mut f, base, METHOD_TRIM, |f, base| {
        emit_trim_body(
            f, base, /* strip_start */ true, /* strip_end */ true,
        );
    });
    emit_native_method_dispatch(&mut f, base, METHOD_TRIM_START, |f, base| {
        emit_trim_body(
            f, base, /* strip_start */ true, /* strip_end */ false,
        );
    });
    emit_native_method_dispatch(&mut f, base, METHOD_TRIM_END, |f, base| {
        emit_trim_body(
            f, base, /* strip_start */ false, /* strip_end */ true,
        );
    });

    // METHOD_STARTS_WITH = 29: string.startsWith(text, prefix) -> Bool.
    // Byte-compare `prefix_len` bytes starting at offset 0 of `text`.
    emit_native_method_dispatch(&mut f, base, METHOD_STARTS_WITH, |f, base| {
        // text_addr / text_len
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        // prefix_addr / prefix_len
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10));

        // If prefix_len > text_len → false.
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // emit_byte_compare_prefix needs the offset in an i32 local.
        // We can't pass literal `0` as the local index — index 0 is
        // the `callee: i64` param of rt_call_native. Use local 11 as
        // an i32 scratch holding 0.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(11));
        emit_byte_compare_prefix(
            f, base, /*text_addr*/ 7, /*prefix_addr*/ 9, /*prefix_len*/ 10,
            /*offset*/ 11,
        );
    });

    // METHOD_ENDS_WITH = 30: string.endsWith(text, suffix) -> Bool.
    // Byte-compare `suffix_len` bytes starting at offset
    // `text_len - suffix_len`.
    emit_native_method_dispatch(&mut f, base, METHOD_ENDS_WITH, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10));

        // If suffix_len > text_len → false.
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // offset = text_len - suffix_len, stored in local 11.
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(11));
        emit_byte_compare_prefix(f, base, 7, 9, 10, 11);
    });

    // METHOD_INDEX_OF = 31: string.indexOf(text, needle) -> Int. -1 if
    // not found. Naive byte scan — O(text_len * needle_len) worst case,
    // acceptable for the typical short-needle use case.
    emit_native_method_dispatch(&mut f, base, METHOD_INDEX_OF, |f, base| {
        // Dispatch on container tag — Array falls into a separate
        // body (i64 bit-equality scan), String falls through to the
        // byte-level substring scan below.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        emit_array_index_of_body(f, base);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10));

        // Empty needle matches at position 0.
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // last_start = text_len - needle_len. If < 0, return -1.
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(11)); // last_start

        // Outer loop: i in 0..=last_start
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::BrIf(1));

            // Inner: byte-compare needle vs text[i..i+needle_len].
            // local 14 is the i32 "match" flag (locals 15-16 are i64
            // per the function prelude, so we can't use them here).
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(13));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::LocalSet(14)); // match = 1
            f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
            {
                f.instruction(&Instruction::LocalGet(13));
                f.instruction(&Instruction::LocalGet(10));
                f.instruction(&Instruction::I32GeU);
                f.instruction(&Instruction::BrIf(1));

                // text byte at i + j
                f.instruction(&Instruction::LocalGet(7));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(12));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(13));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::I32Load8U(mem0()));
                // needle byte at j
                f.instruction(&Instruction::LocalGet(9));
                f.instruction(&Instruction::I32Const(8));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalGet(13));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::I32Load8U(mem0()));
                f.instruction(&Instruction::I32Ne);
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::LocalSet(14));
                f.instruction(&Instruction::Br(2));
                f.instruction(&Instruction::End);

                f.instruction(&Instruction::LocalGet(13));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::LocalSet(13));
                f.instruction(&Instruction::Br(0));
            }
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::End);

            // If match, return i.
            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::Call(base + RT_MAKE_INT));
            f.instruction(&Instruction::Return);
            f.instruction(&Instruction::End);

            // i++
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(12));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SUBSTRING = 32: string.substring(text, start, end) ->
    // String. arg0 = text (local 5), arg1 = start (local 6), arg2 =
    // end (must be read from args_ptr+16 because call_native pre-loads
    // only arg0/arg1).
    emit_native_method_dispatch(&mut f, base, METHOD_SUBSTRING, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        // start = unbox(arg1)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(9));
        // end = unbox(mem[args_ptr + 16])
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(10));

        emit_clamp_range_to_len(f, /*start*/ 9, /*end*/ 10, /*len*/ 8);

        // new_len = end - start
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::MemoryCopy {
            src_mem: 0,
            dst_mem: 0,
        });

        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_REPEAT = 33: string.repeat(text, count) -> String with
    // `text` concatenated `count` times. Negative count is treated as 0
    // (mirrors VM's `.max(0)` clamp).
    emit_native_method_dispatch(&mut f, base, METHOD_REPEAT, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        // count = max(0, unbox(arg1))
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::End);

        // total_len = text_len * count
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // for i in 0..count: memcpy(dst+8+i*text_len, src+8, text_len)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });

            f.instruction(&Instruction::LocalGet(12));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(12));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_REPLACE = 34: string.replace(text, find, with) -> String
    // with every non-overlapping occurrence of `find` in `text`
    // replaced by `with`. arg2 (`with`) is read from `args_ptr + 16`.
    //
    // Two-pass algorithm:
    //   Pass 1 — count occurrences, derive output length
    //   Pass 2 — allocate and copy segments, substituting at each match
    //
    // Edge: if `find` is empty, return `text` unchanged. The VM's Rust
    // `str::replace` inserts `with` between every byte in that case
    // ("abc".replace("", "-") => "-a-b-c-"); we skip that to avoid an
    // infinite-loop trap. Parity tests must avoid empty `find`.
    //
    // i32 locals reserved (3, 4, 7-14, 17-18 — 12 total):
    //   7  text_addr       13  count / out_i (phase 2)
    //   8  text_len        14  match_flag / output_len
    //   9  find_addr       17  inner j (compare scratch)
    //   10 find_len        18  unused
    //   11 with_addr
    //   12 with_len
    //   3  output_addr     4  outer i
    emit_native_method_dispatch(&mut f, base, METHOD_REPLACE, |f, base| {
        // Extract text / find / with.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(10));
        // with_val = mem[args_ptr + 16] (i64)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(12));

        // Empty-find early return: return arg0 unchanged, but RETAIN it first
        // (RC, plan 113 R2) so this path yields an owned +1 just like the
        // fresh-allocating path below. That uniformity is what lets callers
        // treat `replace` as ownership-transferring (`is_fresh_builtin_call`)
        // without a use-after-free when `find` is empty.
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_RETAIN));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // Also: if find_len > text_len, no matches possible — skip to
        // a fresh copy of text. But simpler: let phase 1 count 0, phase
        // 2 copies text byte-for-byte. The two-pass still terminates.

        // ── Phase 1: count matches.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(13)); // count
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4)); // i
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i + find_len > text_len break
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::BrIf(1));

            // compare at i
            emit_byte_compare_flag(
                f, /*text*/ 7, /*find*/ 9, /*find_len*/ 10, /*offset*/ 4,
                /*flag*/ 14, /*j*/ 17,
            );

            f.instruction(&Instruction::LocalGet(14));
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(13));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::End);

            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        // ── Compute output_len = text_len + count*(with_len - find_len)
        // Reuse local 14 for output_len (no longer needed as flag).
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(14));

        // Alloc output (local 3) and write header.
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // ── Phase 2: copy, substituting at each match.
        // i = 0, out_i = 0. Reuse 13 as out_i; 4 as i.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= text_len break
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // Try match at i if enough room
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            {
                emit_byte_compare_flag(f, 7, 9, 10, 4, 14, 17);
                f.instruction(&Instruction::LocalGet(14));
                f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                {
                    // memcpy(output + 8 + out_i, with + 8, with_len)
                    f.instruction(&Instruction::LocalGet(3));
                    f.instruction(&Instruction::I32Const(8));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalGet(13));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalGet(11));
                    f.instruction(&Instruction::I32Const(8));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalGet(12));
                    f.instruction(&Instruction::MemoryCopy {
                        src_mem: 0,
                        dst_mem: 0,
                    });
                    // out_i += with_len
                    f.instruction(&Instruction::LocalGet(13));
                    f.instruction(&Instruction::LocalGet(12));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalSet(13));
                    // i += find_len
                    f.instruction(&Instruction::LocalGet(4));
                    f.instruction(&Instruction::LocalGet(10));
                    f.instruction(&Instruction::I32Add);
                    f.instruction(&Instruction::LocalSet(4));
                    // Continue the outer Loop. Nesting from this point:
                    //   0 = inner If (match), 1 = outer If (room),
                    //   2 = Loop, 3 = Block. Br 2 reloops.
                    f.instruction(&Instruction::Br(2));
                }
                f.instruction(&Instruction::End);
            }
            f.instruction(&Instruction::End);

            // Fallthrough: no match at i — copy one byte.
            // out[out_i] = text[i]
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::I32Store8(mem0()));
            // out_i++, i++
            f.instruction(&Instruction::LocalGet(13));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(13));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(4));

            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // METHOD_POW = 35: math.pow(base, exp) -> Float. Integer-exponent
    // only (exp truncated to i32). See the METHOD_POW const doc.
    //
    // Locals: 15 (i64 temp) is used as the f64 scratch via
    // reinterpretation isn't necessary — f64 values can just live on
    // the stack or in 15/16 as i64 alias since wasm locals are typed.
    // We need f64 locals here, but the emit_call_native prelude only
    // provides i32 and i64 locals. We work around this by doing all
    // f64 math on the stack without intermediate locals, staying
    // within the values-on-stack stack discipline.
    emit_native_method_dispatch(&mut f, base, METHOD_POW, |f, base| {
        // result (f64) kicks off at 1.0, stored temporarily via
        // i64 reinterpretation in local 15.
        f.instruction(&Instruction::F64Const(1.0));
        f.instruction(&Instruction::I64ReinterpretF64);
        f.instruction(&Instruction::LocalSet(15));

        // base (f64) lives in local 16 (also via reinterpret).
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::I64ReinterpretF64);
        f.instruction(&Instruction::LocalSet(16));

        // n = i32(exp)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_AS_NUMBER));
        f.instruction(&Instruction::I32TruncF64S);
        f.instruction(&Instruction::LocalSet(7));

        // If n is negative: compute positive pow then take 1/result.
        // Track sign separately.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(8)); // invert = 0
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::End);

        // Loop: while n > 0: result *= base; n--.
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::BrIf(1));
            // result = result * base  (via reinterpret)
            f.instruction(&Instruction::LocalGet(15));
            f.instruction(&Instruction::F64ReinterpretI64);
            f.instruction(&Instruction::LocalGet(16));
            f.instruction(&Instruction::F64ReinterpretI64);
            f.instruction(&Instruction::F64Mul);
            f.instruction(&Instruction::I64ReinterpretF64);
            f.instruction(&Instruction::LocalSet(15));
            // n--
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalSet(7));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        // Apply inversion if exponent was negative.
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::F64Const(1.0));
        f.instruction(&Instruction::LocalGet(15));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::F64Div);
        f.instruction(&Instruction::I64ReinterpretF64);
        f.instruction(&Instruction::LocalSet(15));
        f.instruction(&Instruction::End);

        // NaN-box the f64 result.
        f.instruction(&Instruction::LocalGet(15));
        f.instruction(&Instruction::F64ReinterpretI64);
        f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
        f.instruction(&Instruction::Return);
    });

    // ── std.http.server response helpers ─────────────────────────
    //
    // Each of text/html/json/ok/redirect calls
    // `IMPORT_HTTP_SERVER_RESPONSE(kind, status, body_ptr, body_len)`.
    // The host allocates a Dict on the guest heap containing
    // `{status, body, contentType?, location?}` and returns a
    // NaN-boxed pointer, so we can `Return` its result directly.

    // METHOD_SERVER_TEXT: text(status, body) -> Dict (Content-Type: text/plain)
    let ir = import_remap;
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_TEXT, |f, base| {
        emit_server_response_call(
            f,
            base,
            RESPONSE_KIND_TEXT,
            /* body_is_arg1 */ true,
            ir,
        );
    });
    // METHOD_SERVER_HTML: html(status, body) -> Dict (Content-Type: text/html)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_HTML, |f, base| {
        emit_server_response_call(f, base, RESPONSE_KIND_HTML, true, ir);
    });
    // METHOD_SERVER_JSON: json(status, body) -> Dict
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_JSON, |f, base| {
        emit_server_response_call(f, base, RESPONSE_KIND_JSON, true, ir);
    });
    // METHOD_SERVER_OK: ok(body) -> Dict (status=200)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_OK, |f, base| {
        emit_server_response_call(f, base, RESPONSE_KIND_OK, false, ir);
    });
    // METHOD_SERVER_REDIRECT: redirect(status, url) -> Dict (location=url)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_REDIRECT, |f, base| {
        emit_server_response_call(f, base, RESPONSE_KIND_REDIRECT, true, ir);
    });

    // METHOD_SERVER_LISTEN: listen(router, port) -> Void (starts router accept loop)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_LISTEN, |f, _base| {
        // router (arg0) is a NaN-boxed Int: low 32 bits are the router ID.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        // port (arg1) is a NaN-boxed Int: low 32 bits are the port.
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32WrapI64);
        emit_import_call(f, IMPORT_HTTP_SERVER_ROUTER_LISTEN, ir);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SERVER_ROUTER: router() -> Router (Int ID, NaN-boxed)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_ROUTER, |f, _base| {
        emit_import_call(f, IMPORT_HTTP_SERVER_ROUTER, ir);
        // Wrap the returned i32 ID as a NaN-boxed Int.
        f.instruction(&Instruction::I64ExtendI32U);
        let tag = (0x7FF8_0000_0000_0000u64 | 0x0001_0000_0000_0000u64) as i64; // QNAN | TAG_INT
        f.instruction(&Instruction::I64Const(tag));
        f.instruction(&Instruction::I64Or);
        f.instruction(&Instruction::Return);
    });

    // METHOD_SERVER_GET: get(router, pattern, handler) -> Void
    // Call: http_server_router_get(router_id: i32, pat_ptr: i32, pat_len: i32, handler: i64)
    // arg0=router(local 5), arg1=pattern(local 6), arg2=handler(args_ptr+16)
    // METHOD_SERVER_GET: get(router, pattern, handler) -> Void
    // Call: http_server_router_get(router_id: i32, pat_ptr: i32, pat_len: i32, handler: i64)
    // arg0=router(local 5), arg1=pattern(local 6), arg2=handler(args_ptr+16)
    // local 15 (i64 temp) used to stash handler while computing string ptr+len.
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_GET, |f, base| {
        // Load arg2 (handler) from args_ptr[2] = *(args_ptr + 16) into local 15 (i64 temp)
        f.instruction(&Instruction::LocalGet(1)); // args_ptr (i32)
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(15)); // handler → local 15 (i64 temp)
                                                   // router_id: arg0 (local 5, NaN-boxed Int) → i32
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        // pattern string addr → local 8 (i32 temp)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(8));
        // pat_ptr = str_addr + 8
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        // pat_len = mem[str_addr + 4]
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        // handler (local 15, i64)
        f.instruction(&Instruction::LocalGet(15));
        emit_import_call(f, IMPORT_HTTP_SERVER_ROUTER_GET, ir);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SERVER_POST: post(router, pattern, handler) -> Void
    // Same 3-arg layout as GET; uses local 15 (i64 temp) for handler.
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_POST, |f, base| {
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Load(MemArg {
            offset: 16,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(15));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(15));
        emit_import_call(f, IMPORT_HTTP_SERVER_ROUTER_POST, ir);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_SERVER_SERVE_FILES: serveFiles(router, dir) -> Void
    // Call: http_server_router_serve_files(router_id: i32, dir_ptr: i32, dir_len: i32)
    emit_native_method_dispatch(&mut f, base, METHOD_SERVER_SERVE_FILES, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        emit_import_call(f, IMPORT_HTTP_SERVER_ROUTER_SERVE_FILES, ir);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // ── std.json methods (runtime fallback path) ─────────────────
    // METHOD_JSON_PARSE: json.parse(val) -> value via IMPORT_JSON_PARSE.
    //
    // Only reached when emit_get_field_by_idx couldn't statically
    // resolve the call (library-compiled code). See the method_check
    // comment above for the trigger conditions.
    //
    // Mirrors the VM's `native_json_parse` which calls `val_to_str`
    // before parsing — non-String inputs (e.g. an Array returned from
    // `getString(dict, 'args')`) are returned as-is rather than
    // round-tripped through JSON. Matches VM semantics for the case
    // that triggers this path in the todo-fullstack server
    // (Forui.rpc::parseArgs).
    emit_native_method_dispatch(&mut f, base, METHOD_JSON_PARSE, |f, base| {
        // If arg0 is not an object: pass-through.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_IS_OBJ));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Get object addr and tag.
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // If tag != String: pass-through.
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(mem0()));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // String: extract ptr+len, call host import.
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add); // ptr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        })); // len
        emit_import_call(f, IMPORT_JSON_PARSE, import_remap);
        f.instruction(&Instruction::Return);
    });
    // METHOD_JSON_STRINGIFY: json.stringify(val) -> string via IMPORT_JSON_STRINGIFY.
    emit_native_method_dispatch(&mut f, base, METHOD_JSON_STRINGIFY, |f, _base| {
        f.instruction(&Instruction::LocalGet(5));
        emit_import_call(f, IMPORT_JSON_STRINGIFY, import_remap);
        f.instruction(&Instruction::Return);
    });

    // METHOD_STORAGE_GET: storageGet(key) -> String? via IMPORT_STORAGE_GET.
    // Mirrors METHOD_FILE_READ — stage a 64KB scratch buffer, ask the
    // host to write the value in, wrap the returned byte-length as a
    // fai String (or VAL_NULL when the host returned -1 for "absent").
    emit_native_method_dispatch(&mut f, base, METHOD_STORAGE_GET, |f, base| {
        // key_addr = obj_addr(arg0)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // Allocate 64KB scratch buffer for the value
        f.instruction(&Instruction::I32Const(65536));
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalTee(3)); // buf_ptr
                                                  // storage_get(key_ptr, key_len, buf_ptr) -> value_len or -1
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add); // key data ptr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        })); // key len
        f.instruction(&Instruction::LocalGet(3)); // buf_ptr
        emit_import_call(f, IMPORT_STORAGE_GET, import_remap);
        f.instruction(&Instruction::LocalTee(4)); // result len
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Wrap the scratch bytes as a String.
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(base + RT_ALLOC_STRING));
        f.instruction(&Instruction::Return);
    });

    // METHOD_STORAGE_SET: storageSet(key, value) -> Void via IMPORT_STORAGE_SET.
    emit_native_method_dispatch(&mut f, base, METHOD_STORAGE_SET, |f, base| {
        // key_addr
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // val_addr
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(3));
        // storage_set(key_ptr, key_len, val_ptr, val_len)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        emit_import_call(f, IMPORT_STORAGE_SET, import_remap);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_STORAGE_REMOVE: storageRemove(key) -> Void via IMPORT_STORAGE_REMOVE.
    emit_native_method_dispatch(&mut f, base, METHOD_STORAGE_REMOVE, |f, base| {
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        emit_import_call(f, IMPORT_STORAGE_REMOVE, import_remap);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_STORAGE_CLEAR: storageClear() -> Void via IMPORT_STORAGE_CLEAR.
    emit_native_method_dispatch(&mut f, base, METHOD_STORAGE_CLEAR, |f, _base| {
        emit_import_call(f, IMPORT_STORAGE_CLEAR, import_remap);
        f.instruction(&Instruction::I64Const(VAL_VOID));
        f.instruction(&Instruction::Return);
    });

    // METHOD_GET_STRING / METHOD_GET_INT / METHOD_GET_BOOL — std.dictionary
    // typed accessors. VM parity: all three are aliases for dict_get — no
    // runtime type coercion, just key lookup. Returns VAL_NULL when the
    // key is missing. Shared body emitted three times via the helper.
    //
    // Stack effect: arg0 (dict, i64) + arg1 (key string, i64) →
    //   key_addr = RT_OBJ_ADDR(arg1)
    //   RT_GET_FIELD(arg0, key_addr+8, mem[key_addr+4]) → i64 value
    for method_id in [METHOD_GET_STRING, METHOD_GET_INT, METHOD_GET_BOOL] {
        emit_native_method_dispatch(&mut f, base, method_id, |f, base| {
            // key_addr = obj_addr(arg1)
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
            f.instruction(&Instruction::LocalSet(7));
            // RT_GET_FIELD(dict = arg0, key_ptr = key_addr+8, key_len = mem[key_addr+4])
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Load(MemArg {
                offset: 4,
                align: 0,
                memory_index: 0,
            }));
            f.instruction(&Instruction::Call(base + RT_GET_FIELD));
            f.instruction(&Instruction::Return);
        });
    }

    // METHOD_GET_KEYS = 23: getKeys(dict) -> Array of key strings
    emit_native_method_dispatch(&mut f, base, METHOD_GET_KEYS, |f, base| {
        // Extract dict
        f.instruction(&Instruction::LocalGet(5)); // arg0 = dict
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7)); // dict_addr
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8)); // count

        // Alloc array: 8 + count*8
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9)); // arr_addr
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_ARRAY));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // Loop: copy key_val from dict entry[i] to arr[i]
        // Dict entry layout: [key:i64][val:i64] at dict_addr+8+i*16
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10)); // i = 0
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // arr_slot = arr_addr + 8 + i*8
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);

            // key_val = mem[dict_addr + 8 + i*16]
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(16));
            f.instruction(&Instruction::I32Mul);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I64Load(mem0()));

            // store in arr slot
            f.instruction(&Instruction::I64Store(mem0()));

            // i++
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End); // end loop
        f.instruction(&Instruction::End); // end block

        // RC: the keys array co-owns the key strings it copied from the dict.
        emit_retain_array_elems(f, base, 9, 8, 14);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });

    // Default: no METHOD_* branch matched. This means either
    //   (a) rt_get_field returned a NativeFn with method_id=UNKNOWN
    //       because the method name didn't match any `ks.*` entry, or
    //   (b) the method name DID match a ks entry but rt_call_native
    //       has no implementation for that METHOD_* id yet.
    //
    // Either way, this is a wasm-codegen gap relative to the VM
    // (fai-runtime, which has ~110 natives while the wasm codegen
    // covers ~50 today). Previously this fallthrough silently returned
    // `null`, which meant calls to unimplemented natives produced
    // nonsense output — the `string.toUpper` parity test was a live
    // example before we implemented METHOD_TO_UPPER.
    //
    // Trap loudly instead so the gap is impossible to miss. The
    // wasmtime / browser error points at this function (rt_call_native)
    // and the method_id is in local 4 — map it back to the METHOD_*
    // constant in this file to identify which native is missing, then
    // add the implementation.
    //
    // See plans/98-wasm-codegen-hardening.md step 3.
    f.instruction(&Instruction::Unreachable);
    f.instruction(&Instruction::End);
    f
}

// ── $rt_parse_int(str_val: i64) -> i64 (NaN-boxed Int or Null) ────────
//
// Parse an optionally-signed decimal integer from a String. Returns
// VAL_NULL for any invalid input (non-digit bytes, empty after trim,
// out-of-range). Mirrors the VM's `s.trim().parse::<i32>()` semantics
// except errors produce Null rather than a runtime FaiError — fai's
// typical idiom is `parseInt(x)` used as an expression, and the wasm
// codegen doesn't have a clean error-path model.
fn emit_parse_int(base: u32) -> Function {
    // locals 1..7: i32 scratch (str_addr, start, end, byte, negative, result, digit)
    let mut f = Function::new([(7, ValType::I32)]);

    // addr = obj_addr(arg0)
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    // end = addr + 8 + len  (the one-past-last byte position relative
    // to the data start, but we'll actually track start/end as byte
    // INDICES within the string data — simpler).
    // Let `start` (local 2) and `end` (local 3) be byte offsets into
    // the data region starting at addr+8.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));

    // Trim leading whitespace.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Trim trailing whitespace.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // If empty after trim, return Null.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Optional leading sign.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5)); // negative = 0
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);
    // Also skip a leading '+'.
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'+' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);

    // Must have at least one digit remaining.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Accumulate digits.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(6)); // result
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // byte = mem[addr+8+i]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        // validate '0'..='9'
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'9' as i32));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I64Const(VAL_NULL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // digit = byte - '0'
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        // result = result * 10 + digit
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        // i++
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Apply sign.
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(6));
    f.instruction(&Instruction::End);

    // Return NaN-boxed Int.
    f.instruction(&Instruction::LocalGet(6));
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::End);
    f
}

// ── $rt_parse_float(str_val: i64) -> i64 (NaN-boxed Float or Null) ────
//
// Parse a decimal float of the form `[sign] integer-part [. fraction]`
// (no exponent notation). Returns Null on invalid input. Mirrors the
// VM's `s.trim().parse::<f64>()` for the simple-decimal subset that
// Rust's FromStr handles — but we don't support exponents (`1e5`).
//
// Algorithm: parse the integer part as f64 by accumulating digit-by-
// digit via `result = result * 10 + d`. Then if a `.` follows, parse
// the fractional part similarly while also accumulating a divisor
// (10^n) and finally add `frac / divisor` to the result.
fn emit_parse_float(base: u32) -> Function {
    // locals: 1 str_addr(i32), 2 i(i32), 3 end(i32), 4 byte(i32),
    // 5 negative(i32), 6 digit(i32), 7 seen_digit(i32),
    // 8 int_part(f64), 9 frac_part(f64), 10 divisor(f64)
    let mut f = Function::new([(7, ValType::I32), (3, ValType::F64)]);

    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(1));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(3));

    // Trim leading ws.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Trim trailing ws.
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        emit_is_ascii_ws(&mut f, 4);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Empty after trim → null.
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32GeU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // Sign.
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::LocalSet(4));
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'-' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(5));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(4));
    f.instruction(&Instruction::I32Const(b'+' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(2));
    f.instruction(&Instruction::End);

    // Integer part: accumulate into local 8 (f64).
    f.instruction(&Instruction::F64Const(0.0));
    f.instruction(&Instruction::LocalSet(8));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(7)); // seen_digit
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalSet(4));
        // If not a digit, break.
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'9' as i32));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        // int_part = int_part * 10 + digit
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::F64Const(10.0));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::F64ConvertI32U);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // Optional fractional part.
    f.instruction(&Instruction::F64Const(0.0));
    f.instruction(&Instruction::LocalSet(9)); // frac_part
    f.instruction(&Instruction::F64Const(1.0));
    f.instruction(&Instruction::LocalSet(10)); // divisor

    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(1));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load8U(mem0()));
    f.instruction(&Instruction::I32Const(b'.' as i32));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    {
        // Skip the '.'
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));

        // Fraction loop.
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(4));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'0' as i32));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'9' as i32));
            f.instruction(&Instruction::I32GtU);
            f.instruction(&Instruction::I32Or);
            f.instruction(&Instruction::BrIf(1));
            // frac = frac * 10 + digit, divisor *= 10
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::F64Const(10.0));
            f.instruction(&Instruction::F64Mul);
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(b'0' as i32));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::F64ConvertI32U);
            f.instruction(&Instruction::F64Add);
            f.instruction(&Instruction::LocalSet(9));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::F64Const(10.0));
            f.instruction(&Instruction::F64Mul);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::LocalSet(7));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(2));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    // If no digits were seen OR we didn't consume all remaining
    // characters (leftover junk like "3.14abc"), return Null.
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Eqz);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I64Const(VAL_NULL));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);

    // result = int_part + frac_part / divisor → reuse local 8 for the
    // combined value so we can conditionally negate it without stack
    // gymnastics around the If's blocktype.
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::LocalGet(10));
    f.instruction(&Instruction::F64Div);
    f.instruction(&Instruction::F64Add);
    f.instruction(&Instruction::LocalSet(8));

    // Apply sign in place.
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::F64Neg);
    f.instruction(&Instruction::LocalSet(8));
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::Call(base + RT_MAKE_FLOAT));
    f.instruction(&Instruction::End);
    f
}

/// Helper to emit a method dispatch case: if method_id == id, run body and return.
fn emit_native_method_dispatch(
    f: &mut Function,
    base: u32,
    method_id: i32,
    body: impl FnOnce(&mut Function, u32),
) {
    f.instruction(&Instruction::LocalGet(4)); // method_id local
    f.instruction(&Instruction::I32Const(method_id));
    f.instruction(&Instruction::I32Eq);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    body(f, base);
    f.instruction(&Instruction::End);
}

/// Emit an ASCII case-shift method body (toUpper when `to_upper=true`,
/// toLower otherwise). Takes the source string as arg0 (local 5),
/// allocates a new string of the same length, and copies each byte
/// with a shift applied when it falls in the opposite ASCII letter
/// range.
///
/// Local allocation inside this method body:
///   local 7: src_addr (i32)
///   local 8: src_len  (i32)
///   local 9: dst_addr (i32)
///   local 10: i       (i32) — loop counter
///   local 11: byte    (i32) — current byte
fn emit_string_case_shift(f: &mut Function, base: u32, method_id: i32, to_upper: bool) {
    // Bytes shifted by +/- 32 (the distance between 'a' and 'A').
    let (range_lo, range_hi, shift): (i32, i32, i32) = if to_upper {
        (b'a' as i32, b'z' as i32, -32) // a-z -> A-Z
    } else {
        (b'A' as i32, b'Z' as i32, 32) // A-Z -> a-z
    };

    emit_native_method_dispatch(f, base, method_id, |f, base| {
        // src_addr = obj_addr(arg0)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
        f.instruction(&Instruction::LocalSet(7));
        // src_len = mem[src_addr + 4]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Load(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));
        f.instruction(&Instruction::LocalSet(8));

        // Allocate new string object: 8 header + len bytes, heap-aligned
        // by rt_alloc (which over-allocates to 8-byte multiples). We
        // need the raw layout: [tag=0:i32][len:i32][data...].
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::Call(base + RT_ALLOC));
        f.instruction(&Instruction::LocalSet(9));

        // Write header: tag=0 (OBJ_TAG_STRING), len=src_len.
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
        f.instruction(&Instruction::I32Store(mem0()));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Store(MemArg {
            offset: 4,
            align: 0,
            memory_index: 0,
        }));

        // Byte-by-byte copy with ASCII case shift.
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(10)); // i = 0
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            // if i >= len, break
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));

            // byte = mem8[src_addr + 8 + i]
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));

            // If byte in [range_lo, range_hi], add shift.
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(range_lo));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(range_hi));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Const(shift));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(11));
            f.instruction(&Instruction::End);

            // mem8[dst_addr + 8 + i] = byte
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(11));
            f.instruction(&Instruction::I32Store8(mem0()));

            // i++
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
        f.instruction(&Instruction::Return);
    });
}

fn mem0() -> MemArg {
    MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}

/// Shared body for METHOD_TRIM / METHOD_TRIM_START / METHOD_TRIM_END.
/// `strip_start` advances the start index past leading ASCII whitespace;
/// `strip_end` retreats the end index past trailing ASCII whitespace.
/// Setting both true is the classic `trim`. Non-ASCII bytes are never
/// treated as whitespace.
fn emit_trim_body(f: &mut Function, base: u32, strip_start: bool, strip_end: bool) {
    // src_addr / len
    f.instruction(&Instruction::LocalGet(5));
    f.instruction(&Instruction::Call(base + RT_OBJ_ADDR));
    f.instruction(&Instruction::LocalSet(7));
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    // start = 0 (local 9)
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(9));

    if strip_start {
        // Advance start past leading whitespace.
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::I32GeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));
            emit_is_ascii_ws(f, 11);
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(9));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }

    // end = len (local 10)
    f.instruction(&Instruction::LocalGet(8));
    f.instruction(&Instruction::LocalSet(10));

    if strip_end {
        // Retreat end past trailing whitespace (while end > start).
        f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
        {
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::I32LeU);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(8));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::I32Load8U(mem0()));
            f.instruction(&Instruction::LocalSet(11));
            emit_is_ascii_ws(f, 11);
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::BrIf(1));
            f.instruction(&Instruction::LocalGet(10));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalSet(10));
            f.instruction(&Instruction::Br(0));
        }
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }

    // Allocate dst string sized (end - start).
    f.instruction(&Instruction::LocalGet(10));
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(12));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::Call(base + RT_ALLOC));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::I32Const(OBJ_TAG_STRING));
    f.instruction(&Instruction::I32Store(mem0()));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::I32Store(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Const(8));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(9));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(12));
    f.instruction(&Instruction::MemoryCopy {
        src_mem: 0,
        dst_mem: 0,
    });

    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::Call(base + RT_MAKE_OBJ));
    f.instruction(&Instruction::Return);
}

/// Push `1` onto the stack if the byte in `byte_local` is an ASCII
/// whitespace (0x09, 0x0A, 0x0B, 0x0C, 0x0D, or 0x20). Leaves the
/// result as an i32 on the stack. Used by METHOD_TRIM.
///
/// The set matches Rust's `u8::is_ascii_whitespace` plus 0x0B (VT) so
/// that VM semantics for ASCII whitespace stay byte-compatible.
fn emit_is_ascii_ws(f: &mut Function, byte_local: u32) {
    // Start with `is_space = (byte == 0x20)`.
    f.instruction(&Instruction::LocalGet(byte_local));
    f.instruction(&Instruction::I32Const(0x20));
    f.instruction(&Instruction::I32Eq);
    // OR each of the control bytes 0x09..=0x0D.
    for b in [0x09, 0x0A, 0x0B, 0x0C, 0x0D] {
        f.instruction(&Instruction::LocalGet(byte_local));
        f.instruction(&Instruction::I32Const(b));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
    }
}

/// Compare `prefix_len` bytes from `text[offset..offset+prefix_len]`
/// against `prefix[0..prefix_len]`, emitting a boolean result onto
/// `local 11` via RT_MAKE_BOOL and returning.
///
/// Caller is expected to have already ruled out the length-too-short
/// case and ensured `offset + prefix_len <= text_len`.
///
/// Local usage: uses 11..=14 as scratch. Safe because callers pass
/// their main-body locals in 7..=10.
fn emit_byte_compare_prefix(
    f: &mut Function,
    base: u32,
    text_addr_local: u32,
    prefix_addr_local: u32,
    prefix_len_local: u32,
    offset_local: u32,
) {
    // i = 0
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(12));
    // match = 1
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        // if i >= prefix_len break
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(prefix_len_local));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // text[offset + i] vs prefix[i]
        f.instruction(&Instruction::LocalGet(text_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(offset_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::LocalGet(prefix_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        // i++
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::LocalGet(13));
    f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    f.instruction(&Instruction::Return);
}

/// Scan an Array for an item equal (by i64 bit pattern) to arg1.
/// Returns a VAL_BOOL via RT_MAKE_BOOL and `Return`s. Intended to run
/// inside an outer `if tag == OBJ_TAG_ARRAY` guard; the caller must
/// NOT fall through to subsequent String code after this helper.
///
/// Uses locals: 7 (arr_addr, already set by the guard), 8 (len),
/// 13 (i loop counter).
fn emit_array_contains_body(f: &mut Function, base: u32) {
    // len = mem[arr_addr + 4]
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(13)); // i
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        // item (i64) = mem[arr_addr + 8 + i*8]
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        // Compare to arg1 (local 6) via RT_EQ so heap-allocated values
        // (strings, dicts, arrays) match by content rather than by
        // pointer identity. Ints, floats, bools, and null still
        // resolve to the same bit-equality short-circuit RT_EQ uses
        // for non-object operands.
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(base + RT_EQ));
        // RT_EQ returns a NaN-boxed Bool (VAL_TRUE / VAL_FALSE).
        // Compare against VAL_TRUE to get the i32 condition the loop
        // branch needs.
        f.instruction(&Instruction::I64Const(VAL_TRUE));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::Call(base + RT_MAKE_BOOL));
    f.instruction(&Instruction::Return);
}

/// Scan an Array for an item equal (by i64 bit pattern) to arg1.
/// Returns the first matching index as Int, or -1. Shaped to match
/// `emit_array_contains_body` — same guard contract (returns inside
/// the enclosing `if tag == OBJ_TAG_ARRAY`).
fn emit_array_index_of_body(f: &mut Function, base: u32) {
    f.instruction(&Instruction::LocalGet(7));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: 4,
        align: 0,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalSet(8));

    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(13));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));

        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64Load(mem0()));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(base + RT_MAKE_INT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);

    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Call(base + RT_MAKE_INT));
    f.instruction(&Instruction::Return);
}

/// Compare `find_len` bytes at `text[offset..offset+find_len]` against
/// `find[0..find_len]`. Stores the result (0/1) in `flag_local`.
/// Does NOT emit a return — used inline by `METHOD_REPLACE` where the
/// outer logic continues based on the flag.
///
/// Caller must have ensured `offset + find_len <= text_len` before
/// calling (i.e. this helper is safe to read the bytes).
///
/// Uses only the provided locals + temporary `j_local` as a loop
/// counter. All locals must be i32.
fn emit_byte_compare_flag(
    f: &mut Function,
    text_addr_local: u32,
    find_addr_local: u32,
    find_len_local: u32,
    offset_local: u32,
    flag_local: u32,
    j_local: u32,
) {
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(flag_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(j_local));
    f.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
    {
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::LocalGet(find_len_local));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::BrIf(1));
        // text byte at offset + j
        f.instruction(&Instruction::LocalGet(text_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(offset_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        // find byte at j
        f.instruction(&Instruction::LocalGet(find_addr_local));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(flag_local));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        // j++
        f.instruction(&Instruction::LocalGet(j_local));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(j_local));
        f.instruction(&Instruction::Br(0));
    }
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

/// Clamp `start` and `end` to `[0, len]` (with signed comparisons so
/// negative inputs fold to 0) and then fold `end < start` up to `end = start`.
/// Mirrors the VM's `native_substring` and `native_array_slice` clamps.
fn emit_clamp_range_to_len(f: &mut Function, start_local: u32, end_local: u32, len_local: u32) {
    // start = max(0, start)
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(start_local));
    f.instruction(&Instruction::End);
    // start = min(len, start)
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::I32GtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::LocalSet(start_local));
    f.instruction(&Instruction::End);
    // end = max(0, end)
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
    // end = min(len, end)
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::I32GtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(len_local));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
    // if end < start, end = start
    f.instruction(&Instruction::LocalGet(end_local));
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::I32LtS);
    f.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
    f.instruction(&Instruction::LocalGet(start_local));
    f.instruction(&Instruction::LocalSet(end_local));
    f.instruction(&Instruction::End);
}

/// Return the WASM type signatures needed for runtime functions.
/// Each entry is (params, results).
pub fn type_signatures() -> Vec<(Vec<ValType>, Vec<ValType>)> {
    vec![
        // RT_IS_INT: (i64) -> i32
        (vec![ValType::I64], vec![ValType::I32]),
        // RT_IS_FLOAT: (i64) -> i32
        (vec![ValType::I64], vec![ValType::I32]),
        // RT_AS_NUMBER: (i64) -> f64
        (vec![ValType::I64], vec![ValType::F64]),
        // RT_MAKE_INT: (i32) -> i64
        (vec![ValType::I32], vec![ValType::I64]),
        // RT_MAKE_FLOAT: (f64) -> i64
        (vec![ValType::F64], vec![ValType::I64]),
        // RT_MAKE_BOOL: (i32) -> i64
        (vec![ValType::I32], vec![ValType::I64]),
        // RT_ADD through RT_POW: (i64, i64) -> i64
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        // RT_NEG: (i64) -> i64
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_EQ through RT_GE: (i64, i64) -> i64
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        // RT_PRINT_VAL: (i64) -> void
        (vec![ValType::I64], vec![]),
        // RT_ITOA: (i32, i32) -> i32
        (vec![ValType::I32, ValType::I32], vec![ValType::I32]),
        // RT_ALLOC: (i32) -> i32
        (vec![ValType::I32], vec![ValType::I32]),
        // RT_MAKE_OBJ: (i32) -> i64
        (vec![ValType::I32], vec![ValType::I64]),
        // RT_OBJ_ADDR: (i64) -> i32
        (vec![ValType::I64], vec![ValType::I32]),
        // RT_IS_OBJ: (i64) -> i32
        (vec![ValType::I64], vec![ValType::I32]),
        // RT_STR_EQ: (i32, i32, i32, i32) -> i32
        (
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // RT_STR_CMP: (i32, i32, i32, i32) -> i32
        (
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // RT_ALLOC_STRING: (i32, i32) -> i64
        (vec![ValType::I32, ValType::I32], vec![ValType::I64]),
        // RT_CONCAT: (i64, i64) -> i64
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        // RT_GET_INDEX: (i64, i64) -> i64
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
        // RT_GET_FIELD: (i64, i32, i32) -> i64
        (
            vec![ValType::I64, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // RT_SET_FIELD: (i64, i32, i32, i64) -> i64 — returns the dict
        // pointer, which differs from the input only when an at-capacity
        // dict had to be reallocated to fit a new key (callers must use
        // the returned value).
        (
            vec![ValType::I64, ValType::I32, ValType::I32, ValType::I64],
            vec![ValType::I64],
        ),
        // RT_PRINT_VAL_NEW: (i64) -> void
        (vec![ValType::I64], vec![]),
        // RT_VALUE_TO_STR: (i64) -> i64
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_IMPORT_MODULE: (i32, i32) -> i64
        (vec![ValType::I32, ValType::I32], vec![ValType::I64]),
        // RT_CALL_NATIVE: (i64, i32, i32) -> i64
        (
            vec![ValType::I64, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // RT_PARSE_INT: (i64 string) -> i64 (Int or Null)
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_PARSE_FLOAT: (i64 string) -> i64 (Float or Null)
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_FREE: (i32 ptr, i32 size) -> void — return a heap block to the
        // free list so the allocator can reuse it (single-list, head in a
        // dedicated global; size is the block's original alloc size).
        (vec![ValType::I32, ValType::I32], vec![]),
        // RT_COPY_DEEP: (i64) -> i64 — deep-copy an object graph (fresh owned).
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_RETAIN: (i64) -> i64 — refcount increment, returns the value.
        (vec![ValType::I64], vec![ValType::I64]),
        // RT_RELEASE: (i64) -> void — refcount decrement; free at zero.
        (vec![ValType::I64], vec![]),
        // RT_LIVE_OBJECTS: () -> i32 — read the live-object counter global.
        (vec![], vec![ValType::I32]),
        // RT_CONCAT_MOVE: (i64, i64) -> i64
        (vec![ValType::I64, ValType::I64], vec![ValType::I64]),
    ]
}

/// Return the host import function signatures (platform I/O).
pub fn import_signatures() -> Vec<(&'static str, Vec<ValType>, Vec<ValType>)> {
    vec![
        // IMPORT_PRINT: (ptr, len) -> void
        ("print", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_READ_FILE: (path_ptr, path_len, buf_ptr) -> i32 (length, or -1)
        (
            "read_file",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_WRITE_FILE: (path_ptr, path_len, content_ptr, content_len) -> i32 (0=ok, -1=err)
        (
            "write_file",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_NOW_MS: () -> f64
        ("now_ms", vec![], vec![ValType::F64]),
        // IMPORT_RANDOM: () -> f64
        ("random", vec![], vec![ValType::F64]),
        // IMPORT_SLEEP_MS: (ms: f64) -> void
        ("sleep_ms", vec![ValType::F64], vec![]),
        // IMPORT_CALL_FFI: (ext_fn_idx: i32, arg_count: i32, args_ptr: i32) -> i64
        (
            "call_ffi",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_RUN_ALL: (args_ptr: i32, count: i32) -> i64 (NaN-boxed tuple pointer)
        (
            "run_all",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_SPAWN: (closure_val: i64) -> i64 (VAL_VOID)
        ("spawn", vec![ValType::I64], vec![ValType::I64]),
        // IMPORT_HTTP_POST: (url_ptr, url_len, body_ptr, body_len, result_buf_ptr) -> i32 (resp len, or -1)
        (
            "http_post",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            vec![ValType::I32],
        ),
        // IMPORT_SET_HTML: (html_ptr, html_len) -> void
        ("set_html", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_SET_HTML_AT: (selector_ptr, selector_len, html_ptr, html_len) -> void
        (
            "set_html_at",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_JSON_PARSE: (json_ptr, json_len) -> i64 (NaN-boxed value)
        (
            "json_parse",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_JSON_STRINGIFY: (val: i64) -> i64 (NaN-boxed string)
        ("json_stringify", vec![ValType::I64], vec![ValType::I64]),
        // IMPORT_REMOTE_CALL: (url_ptr, url_len, fn_ptr, fn_len, args_ptr, args_len, hash_ptr, hash_len) -> i64 (NaN-boxed value)
        (
            "remote_call",
            vec![
                ValType::I32,
                ValType::I32, // url
                ValType::I32,
                ValType::I32, // fn
                ValType::I32,
                ValType::I32, // args
                ValType::I32,
                ValType::I32, // hash
            ],
            vec![ValType::I64],
        ),
        // IMPORT_FLOAT_TO_STR: (value: f64, buf_ptr: i32) -> i32 (length written)
        (
            "float_to_str",
            vec![ValType::F64, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_HTTP_SERVER_RESPONSE: (kind, status, body_ptr, body_len) -> i64 (NaN-boxed Dict)
        (
            "http_server_response",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_RESERVED_17: removed legacy HTTP listener slot. It is always
        // marked unavailable, so compiled modules never declare it.
        ("reserved_17", vec![], vec![]),
        // IMPORT_GET_LOCATION_PATH: () -> i64 (NaN-boxed String with window.location.pathname)
        ("get_location_path", vec![], vec![ValType::I64]),
        // IMPORT_PUSH_HISTORY_STATE: (path_ptr: i32, path_len: i32) -> void
        (
            "push_history_state",
            vec![ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_HTTP_SERVER_ROUTER: () -> i32 (router ID)
        ("http_server_router", vec![], vec![ValType::I32]),
        // IMPORT_HTTP_SERVER_ROUTER_GET: (id, pat_ptr, pat_len, handler_val) -> void
        (
            "http_server_router_get",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I64],
            vec![],
        ),
        // IMPORT_HTTP_SERVER_ROUTER_POST: (id, pat_ptr, pat_len, handler_val) -> void
        (
            "http_server_router_post",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I64],
            vec![],
        ),
        // IMPORT_HTTP_SERVER_ROUTER_SERVE_FILES: (id, dir_ptr, dir_len) -> void
        (
            "http_server_router_serve_files",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_HTTP_SERVER_ROUTER_LISTEN: (id, port) -> void (blocks forever)
        (
            "http_server_router_listen",
            vec![ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_STORAGE_GET: (key_ptr, key_len, buf_ptr) -> value_len or -1
        (
            "storage_get",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_STORAGE_SET: (key_ptr, key_len, val_ptr, val_len) -> void
        (
            "storage_set",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_STORAGE_REMOVE: (key_ptr, key_len) -> void
        ("storage_remove", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_STORAGE_CLEAR: () -> void
        ("storage_clear", vec![], vec![]),
        // IMPORT_FILE_EXISTS: (path_ptr, path_len) -> i32 (1 = exists, 0 = no)
        (
            "file_exists",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_HTTP_REQUEST_GET: (url_ptr, url_len, headers_val) -> i64 (Dict | null)
        (
            "http_request_get",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![ValType::I64],
        ),
        // IMPORT_HTTP_REQUEST_POST: (url_ptr, url_len, body_ptr, body_len, headers_val) -> i64
        (
            "http_request_post",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I64,
            ],
            vec![ValType::I64],
        ),
        // IMPORT_HTTP_REQUEST_PUT
        (
            "http_request_put",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I64,
            ],
            vec![ValType::I64],
        ),
        // IMPORT_HTTP_REQUEST_PATCH
        (
            "http_request_patch",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I64,
            ],
            vec![ValType::I64],
        ),
        // IMPORT_HTTP_REQUEST_DELETE
        (
            "http_request_delete",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![ValType::I64],
        ),
        // IMPORT_NET_AVAILABLE: () -> i32 (0/1)
        ("net_available", vec![], vec![ValType::I32]),
        // IMPORT_FFI_AVAILABLE: (name_ptr, name_len) -> i32 (0/1)
        (
            "ffi_available",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_LOG_INFO / WARN / ERROR: (ptr, len) -> void. Each prints
        // the message prefixed by its level at the host.
        ("log_info", vec![ValType::I32, ValType::I32], vec![]),
        ("log_warn", vec![ValType::I32, ValType::I32], vec![]),
        ("log_error", vec![ValType::I32, ValType::I32], vec![]),
        (
            "path_join",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "path_basename",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "path_dirname",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "path_extname",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "html_escape",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "file_list",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "json_require_string",
            vec![ValType::I64, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "array_map",
            vec![ValType::I64, ValType::I64],
            vec![ValType::I64],
        ),
        (
            "array_filter",
            vec![ValType::I64, ValType::I64],
            vec![ValType::I64],
        ),
        (
            "array_find",
            vec![ValType::I64, ValType::I64],
            vec![ValType::I64],
        ),
        (
            "array_is_any",
            vec![ValType::I64, ValType::I64],
            vec![ValType::I64],
        ),
        (
            "array_is_all",
            vec![ValType::I64, ValType::I64],
            vec![ValType::I64],
        ),
        // TCP — handles as i32, bodies as strings of bytes.
        ("tcp_listen", vec![ValType::I32], vec![ValType::I32]),
        ("tcp_accept", vec![ValType::I32], vec![ValType::I64]),
        (
            "tcp_connect",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        ("tcp_read", vec![ValType::I32], vec![ValType::I64]),
        ("tcp_read_line", vec![ValType::I32], vec![ValType::I64]),
        (
            "tcp_write",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        ("tcp_close", vec![ValType::I32], vec![]),
        ("tcp_address", vec![ValType::I32], vec![ValType::I64]),
        // UDP.
        ("udp_bind", vec![ValType::I32], vec![ValType::I32]),
        (
            "udp_send",
            vec![
                ValType::I32, // handle
                ValType::I32,
                ValType::I32, // host_ptr, host_len
                ValType::I32, // port
                ValType::I32,
                ValType::I32, // data_ptr, data_len
            ],
            vec![ValType::I32],
        ),
        ("udp_receive", vec![ValType::I32], vec![ValType::I64]),
        ("udp_broadcast", vec![ValType::I32, ValType::I32], vec![]),
        // CLI.
        (
            "cli_read_line",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        ("cli_write", vec![ValType::I32, ValType::I32], vec![]),
        ("cli_write_line", vec![ValType::I32, ValType::I32], vec![]),
        ("cli_clear", vec![], vec![]),
        ("cli_move_to", vec![ValType::I32, ValType::I32], vec![]),
        (
            "__fai_set_trap_msg",
            vec![ValType::I32, ValType::I32],
            vec![],
        ),
        // spy_set_mock(fn_id: i32, value: i64) -> void
        ("spy_set_mock", vec![ValType::I32, ValType::I64], vec![]),
        // spy_set_mock_once(fn_id: i32, value: i64) -> void
        (
            "spy_set_mock_once",
            vec![ValType::I32, ValType::I64],
            vec![],
        ),
        // spy_reset(fn_id: i32) -> void
        ("spy_reset", vec![ValType::I32], vec![]),
        // spy_check_call(fn_id: i32, args_ptr: i32, arg_count: i32,
        //                out_value_ptr: i32) -> i32 (1=mocked, 0=continue)
        (
            "spy_check_call",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // spy_assert_called_with(fn_id: i32, expected_ptr: i32,
        //                        expected_count: i32) -> i32 (1 = fail)
        // On mismatch the host stashes a trap message via
        // `IMPORT_SET_TRAP_MSG` and returns 1; the guest then emits
        // `unreachable` to surface the trap. On success returns 0 —
        // the guest falls through.
        (
            "spy_assert_called_with",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // spy_assert_call_count(fn_id: i32, expected: i32) -> i32 (1 = fail)
        (
            "spy_assert_call_count",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // spy_assert_not_called(fn_id: i32) -> i32 (1 = fail)
        (
            "spy_assert_not_called",
            vec![ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_ENV_GET: (key_ptr, key_len) -> i64 (NaN-boxed String | VAL_NULL)
        (
            "env_get",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_ENV_LOAD: (path_ptr, path_len) -> i32 (1=ok, 0=err)
        (
            "env_load",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_EVENT_ON: (name_ptr, name_len, handler_val) -> i64 (Subscription Dict)
        (
            "event_on",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![ValType::I64],
        ),
        // IMPORT_EVENT_ONCE: (name_ptr, name_len, handler_val) -> i64 (Subscription Dict)
        (
            "event_once",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![ValType::I64],
        ),
        // IMPORT_EVENT_OFF: (sub_val) -> i32 (Bool)
        ("event_off", vec![ValType::I64], vec![ValType::I32]),
        // IMPORT_EVENT_EMIT: (name_ptr, name_len, data_val) -> void
        (
            "event_emit",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![],
        ),
        // IMPORT_EVENT_SUBSCRIBERS: (name_ptr, name_len) -> i32 (count)
        (
            "event_subscribers",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_EVENT_CLEAR: (name_ptr, name_len) -> void
        ("event_clear", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_EVENT_CLEAR_ALL: () -> void
        ("event_clear_all", vec![], vec![]),
        // IMPORT_EVENT_EMIT_DEFERRED: (name_ptr, name_len, data_val) -> void
        (
            "event_emit_deferred",
            vec![ValType::I32, ValType::I32, ValType::I64],
            vec![],
        ),
        // IMPORT_EVENT_DRAIN: () -> void
        ("event_drain", vec![], vec![]),
        // IMPORT_EVENT_QUEUE_LEN: () -> i32
        ("event_queue_len", vec![], vec![ValType::I32]),
        (
            "process_run",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            vec![ValType::I64],
        ),
        (
            "process_start",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            vec![ValType::I64],
        ),
        (
            "process_write",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "process_read",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "process_stop",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // std.crypto — native-only hashing/HMAC/encoding. String args are
        // (ptr, len) pairs; string results are NaN-boxed i64. `available`
        // and `constant_time_equals` return i32 (0/1) wrapped as Bool.
        ("crypto_available", vec![], vec![ValType::I32]),
        (
            "crypto_hmac_sha256_hex",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "crypto_sha256_hex",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "crypto_hex_encode",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "crypto_constant_time_equals",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        (
            "crypto_base64_encode",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        (
            "crypto_base64_decode",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // Async scheduler ABI. Host schedules an external wakeup for task_id.
        ("host_set_timer", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_REMOTE_BEGIN: (task_id, url_ptr,url_len, fn_ptr,fn_len,
        //   args_ptr,args_len, hash_ptr,hash_len) -> ()
        (
            "remote_begin",
            vec![
                ValType::I32, // task_id
                ValType::I32,
                ValType::I32, // url
                ValType::I32,
                ValType::I32, // fn
                ValType::I32,
                ValType::I32, // args
                ValType::I32,
                ValType::I32, // hash
            ],
            vec![],
        ),
        // IMPORT_REMOTE_RESULT: (task_id) -> i64 (NaN-boxed value)
        ("remote_result", vec![ValType::I32], vec![ValType::I64]),
        // IMPORT_TRAP_REPORT: (code, a, b) -> void — structured trap reason,
        // stashed host-side right before the guest executes `unreachable`.
        (
            "__fai_trap_report",
            vec![ValType::I32, ValType::I64, ValType::I64],
            vec![],
        ),
        // IMPORT_ALLOC_EVENT / IMPORT_FREE_EVENT: (addr, size) -> void —
        // heap allocation ledger (`--check-leaks`); only declared when
        // check-leaks codegen is enabled.
        (
            "__fai_alloc_event",
            vec![ValType::I32, ValType::I32],
            vec![],
        ),
        ("__fai_free_event", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_PROCESS_AVAILABLE: () -> i32 — stays linked on every
        // target so the availability probe can report false in the
        // browser (the std.process run/session imports are stripped).
        ("process_available", vec![], vec![ValType::I32]),
        // IMPORT_FILE_READ_STR: (path_ptr, path_len) -> i64 boxed String
        // or VAL_NULL. Host-allocated; no guest scratch buffer to overflow.
        (
            "file_read_str",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_STORAGE_GET_STR: (key_ptr, key_len) -> i64 boxed String
        // or VAL_NULL. Host-allocated; no guest scratch buffer to overflow.
        (
            "storage_get_str",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_RC_WATCH: (obj_addr, rc_slot_addr, delta) -> void.
        (
            "__fai_rc_watch",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_MEM_WATCH: () -> void. Host reads the watched address.
        ("__fai_mem_watch", vec![], vec![]),
        // IMPORT_OWNERSHIP_EVENT: (op, site, value, aux) -> void.
        (
            "__fai_ownership_event",
            vec![ValType::I32, ValType::I32, ValType::I64, ValType::I32],
            vec![],
        ),
        // IMPORT_REPLACE_LOCATION: (path_ptr, path_len) -> void.
        ("replace_location", vec![ValType::I32, ValType::I32], vec![]),
        // IMPORT_CRYPTO_HMAC_SHA1_BASE64: (key_ptr, key_len, msg_ptr, msg_len) -> i64.
        (
            "crypto_hmac_sha1_base64",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_FFI_BEGIN: (task_id, ext_fn_idx, arg_count, args_ptr) -> ()
        (
            "ffi_begin",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_FFI_RESULT: (task_id) -> i64 (NaN-boxed value)
        ("ffi_result", vec![ValType::I32], vec![ValType::I64]),
        // IMPORT_HOST_OP_BEGIN: (task_id, op_kind, arg_count, args_ptr) -> ()
        (
            "host_op_begin",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
        // IMPORT_HOST_OP_RESULT: (task_id) -> i64 (NaN-boxed value)
        ("host_op_result", vec![ValType::I32], vec![ValType::I64]),
        // IMPORT_CRYPTO_RS256_SIGN_BASE64_URL: (key_ptr, key_len, msg_ptr, msg_len) -> i64.
        (
            "crypto_rs256_sign_base64_url",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_DEBUG_FUNCTION_CALL: (name_ptr, name_len, event) -> void.
        (
            "__fai_debug_function_call",
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ),
    ]
}

/// Names of the runtime helper functions, in the order [`emit_all`]
/// emits them (index = `RT_*` constant). Used by the module assemblers
/// to emit the wasm `name` section so backtraces show `rt_alloc`
/// instead of `wasm-function[NNN]`.
pub fn rt_fn_names() -> [&'static str; RT_COUNT as usize] {
    [
        "rt_is_int",
        "rt_is_float",
        "rt_as_number",
        "rt_make_int",
        "rt_make_float",
        "rt_make_bool",
        "rt_add",
        "rt_sub",
        "rt_mul",
        "rt_div",
        "rt_idiv",
        "rt_mod",
        "rt_pow",
        "rt_neg",
        "rt_eq",
        "rt_ne",
        "rt_lt",
        "rt_le",
        "rt_gt",
        "rt_ge",
        "rt_print_val",
        "rt_itoa",
        "rt_alloc",
        "rt_make_obj",
        "rt_obj_addr",
        "rt_is_obj",
        "rt_str_eq",
        "rt_str_cmp",
        "rt_alloc_string",
        "rt_concat",
        "rt_get_index",
        "rt_get_field",
        "rt_set_field",
        "rt_print_val_new",
        "rt_value_to_str",
        "rt_import_module",
        "rt_call_native",
        "rt_parse_int",
        "rt_parse_float",
        "rt_free",
        "rt_copy_deep",
        "rt_retain",
        "rt_release",
        "rt_live_objects",
        "rt_concat_move",
    ]
}

#[cfg(test)]
mod alloc_free_tests {
    use super::{emit_alloc, emit_free};
    use wasm_encoder::{
        CodeSection, ConstExpr, ExportKind, ExportSection, FunctionSection, GlobalSection,
        GlobalType, MemorySection, MemoryType, Module, TypeSection, ValType,
    };
    use wasmtime::{Engine, Instance, Store};

    // Build a minimal module: global 0 = __heap_ptr (init 1024), global 1 =
    // free-list head (init 0), func 0 = rt_alloc, func 1 = rt_free. Drive them
    // directly to verify free + same-size reuse.
    fn build() -> Vec<u8> {
        let fl = 1u32; // free-list head global index
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], [ValType::I32]); // alloc: (size)->ptr
        types.ty().function([ValType::I32, ValType::I32], []); // free: (ptr,size)->()
        let mut funcs = FunctionSection::new();
        funcs.function(0);
        funcs.function(1);
        let mut mem = MemorySection::new();
        mem.memory(MemoryType {
            minimum: 1,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let i32mut = GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        };
        let live = 2u32; // live-object counter global index
        let bucket_base = 1024u32; // zero-init bucket-head region
        let heap_init = bucket_base + super::FREE_BUCKET_REGION_BYTES; // heap bump starts past it
        let mut globals = GlobalSection::new();
        globals.global(i32mut, &ConstExpr::i32_const(heap_init as i32)); // __heap_ptr
        globals.global(i32mut, &ConstExpr::i32_const(0)); // free-list head
        globals.global(i32mut, &ConstExpr::i32_const(0)); // live-object counter
        let mut exports = ExportSection::new();
        exports.export("alloc", ExportKind::Func, 0);
        exports.export("free", ExportKind::Func, 1);
        exports.export("heap", ExportKind::Global, 0);
        let mut code = CodeSection::new();
        // No imports in this fixture module — an empty remap makes the
        // OOM trap-report degrade to a bare `unreachable`, which is fine.
        code.function(&emit_alloc(fl, live, bucket_base, &[]));
        code.function(&emit_free(fl, live, bucket_base, &[]));
        let mut m = Module::new();
        m.section(&types);
        m.section(&funcs);
        m.section(&mem);
        m.section(&globals);
        m.section(&exports);
        m.section(&code);
        m.finish()
    }

    fn inst() -> (Store<()>, Instance) {
        let engine = Engine::default();
        let module = wasmtime::Module::new(&engine, build()).expect("module builds");
        let mut store = Store::new(&engine, ());
        let instance = Instance::new(&mut store, &module, &[]).expect("instantiates");
        (store, instance)
    }

    #[test]
    fn alloc_bumps_when_freelist_empty() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        // heap starts past the bucket region (1024 + FREE_BUCKET_REGION_BYTES);
        // the logical pointer is base+8 (rc prefix, plan 113)
        let heap_init = 1024 + super::FREE_BUCKET_REGION_BYTES as i32;
        assert_eq!(
            a,
            heap_init + 8,
            "first alloc is heap start + 8-byte rc prefix"
        );
        assert!(b > a, "second alloc bumps past the first (no free yet)");
    }

    #[test]
    fn free_then_alloc_reuses_same_size_block() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        assert_eq!(b, a, "freed block is reused by the next same-size alloc");
        // free list is now empty again → next alloc bumps a fresh block
        let c = alloc.call(&mut store, 16).unwrap();
        assert!(
            c > a,
            "after the freelist drains, alloc bumps a fresh block"
        );
    }

    #[test]
    fn two_frees_reuse_lifo() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        let a = alloc.call(&mut store, 16).unwrap();
        let b = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        free.call(&mut store, (b, 16)).unwrap();
        // LIFO: last freed (b) is reused first
        assert_eq!(alloc.call(&mut store, 16).unwrap(), b);
        assert_eq!(alloc.call(&mut store, 16).unwrap(), a);
    }

    #[test]
    fn alloc_reuses_only_exact_size_freed_block() {
        let (mut store, instance) = inst();
        let alloc = instance
            .get_typed_func::<i32, i32>(&mut store, "alloc")
            .unwrap();
        let free = instance
            .get_typed_func::<(i32, i32), ()>(&mut store, "free")
            .unwrap();
        // A freed 16-byte block is NOT reused for a larger (64) request…
        let a = alloc.call(&mut store, 16).unwrap();
        free.call(&mut store, (a, 16)).unwrap();
        let big = alloc.call(&mut store, 64).unwrap();
        assert!(big > a, "larger request bumps past a too-small freed block");
        // …nor for a smaller (8) request: exact-fit means a small request must
        // never grab a larger block (which would then be lost at its smaller
        // size). This is the property that keeps mixed-size loops tight.
        let b = alloc.call(&mut store, 32).unwrap();
        free.call(&mut store, (b, 32)).unwrap();
        let small = alloc.call(&mut store, 8).unwrap();
        assert!(
            small > b,
            "smaller request bumps rather than grabbing a larger freed block"
        );
    }
}

#[cfg(test)]
mod ownership_roundtrip_tests {
    use super::*;

    /// Plan 119 U4: the ownership table and the declared import surface
    /// cannot drift. Direction 1: every i64-returning env import either
    /// has an ownership row or is on the explicit allow-list below (with
    /// the reason it is not a user-visible boxed RESULT). Direction 2:
    /// every table row names a real import. `import_signatures()` is the
    /// full declared surface — a compiled module's import section is
    /// always a subset — so iterating it is the exhaustive check.
    #[test]
    fn import_surface_round_trips_against_ownership_table() {
        // i64-returning imports that are NOT user-visible boxed results.
        // Every entry carries its justification; an import in neither the
        // table nor this list fails the test.
        const ALLOWED_UNSIGNED: &[(&str, &str)] = &[(
            "spawn",
            "returns VAL_VOID, immediately dropped at the nowait call site \
             (compile_nowait) — never a user-visible value",
        )];

        let sigs = import_signatures();
        let names: std::collections::HashSet<&str> = sigs.iter().map(|(n, _, _)| *n).collect();

        // Direction 2: no dead table rows.
        for row in fai_compiler::ownership_abi::HOST_IMPORTS {
            assert!(
                names.contains(row.import),
                "table row '{}' names no declared import",
                row.import
            );
        }

        // Direction 1: no unsigned i64-returning imports.
        let mut unsigned: Vec<&str> = Vec::new();
        for (name, _params, results) in &sigs {
            if results.as_slice() == [wasm_encoder::ValType::I64]
                && fai_compiler::ownership_abi::lookup_host_import(name).is_none()
                && !ALLOWED_UNSIGNED.iter().any(|(n, _)| n == name)
            {
                unsigned.push(name);
            }
        }
        assert!(
            unsigned.is_empty(),
            "i64-returning imports with no ownership row and no allow-list entry:\n  {}",
            unsigned.join("\n  ")
        );
    }

    #[test]
    fn generic_host_op_imports_are_declared_at_expected_indices() {
        let sigs = import_signatures();
        assert_eq!(sigs.len(), IMPORT_COUNT as usize);
        assert_eq!(
            sigs[IMPORT_HOST_OP_BEGIN as usize],
            (
                "host_op_begin",
                vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
                vec![]
            )
        );
        assert_eq!(
            sigs[IMPORT_HOST_OP_RESULT as usize],
            ("host_op_result", vec![ValType::I32], vec![ValType::I64])
        );
    }
}
