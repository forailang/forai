use super::*;

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
/// Opaque secret handle (plan 132): `[tag@0][name_len@4][name bytes@8..]`,
/// string-shaped so release/copy reuse the string size logic. The payload is
/// only the declared secret NAME — plaintext never enters guest memory; the
/// host resolves the handle at egress. Constructed exclusively by the
/// `secrets_get` host import, never by guest codegen.
pub const OBJ_TAG_SECRET: i32 = 9;
/// Debug poison written into a freed object's tag slot under the RC checked-mode
/// (`FAI_RC_CHECK`, plan 113 R2). Not a valid tag (real tags are 0..=9), so any
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
/// `env.json_query(json_ptr, json_len, path_ptr, path_len) -> i64` —
/// host-side JSON selection: parse natively, evaluate a jq-style
/// selection path (`.a.b[].c`, indexes `[0]`/`[-1]`, quoted keys, pipes,
/// `..` descent; empty path selects the root), and materialize ONLY the
/// matched values as a guest Array. Null on invalid JSON or a malformed
/// path. Large documents never build a full guest tree.
pub const IMPORT_JSON_QUERY: u32 = 121;
/// `env.json_query_page(json_ptr, json_len, path_ptr, path_len, offset,
/// limit) -> i64` — windowed variant returning a Dict
/// `{ total: Int, items: Array }` so callers can page a big match set
/// without materializing it. Null on invalid JSON; offset/limit are
/// clamped host-side.
pub const IMPORT_JSON_QUERY_PAGE: u32 = 122;
/// `env.json_format(ptr, len) -> i64` — reserialize a JSON string
/// pretty-printed (2-space indent, one attribute per line). Null on
/// invalid JSON. Native normalizes object key order (serde map);
/// the browser twin preserves insertion order.
pub const IMPORT_JSON_FORMAT: u32 = 123;
/// `env.json_minify(ptr, len) -> i64` — reserialize a JSON string
/// compact (no insignificant whitespace). Null on invalid JSON.
pub const IMPORT_JSON_MINIFY: u32 = 124;
/// `env.json_valid(ptr, len) -> i32` — 1 when the string parses as
/// JSON, 0 otherwise. No guest values are materialized.
pub const IMPORT_JSON_VALID: u32 = 125;
/// `env.json_stringify_pretty(val) -> i64` — like json_stringify but
/// pretty-printed with 2-space indent.
pub const IMPORT_JSON_STRINGIFY_PRETTY: u32 = 126;
/// `env.secrets_get(name_ptr, name_len) -> i64` — NaN-boxed opaque Secret
/// handle (OBJ_TAG_SECRET) carrying only the name (plan 132). The host
/// validates the name against the declared `[secrets]` manifest when one
/// exists; an undeclared name raises a catchable runtime error. Plaintext
/// is never returned — resolution happens host-side at egress.
pub const IMPORT_SECRETS_GET: u32 = 127;
/// `env.secrets_has(name_ptr, name_len) -> i32` — 1 when the active
/// backend can resolve the name, 0 otherwise. Never returns the value.
pub const IMPORT_SECRETS_HAS: u32 = 128;
/// `env.secrets_available() -> i32` — availability probe: 1 on the native
/// host, 0 in the browser (std.secrets is server-side only by design).
pub const IMPORT_SECRETS_AVAILABLE: u32 = 129;
pub const IMPORT_COUNT: u32 = 130;

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
        // IMPORT_JSON_QUERY: (json_ptr, json_len, path_ptr, path_len) -> i64.
        (
            "json_query",
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_JSON_QUERY_PAGE:
        // (json_ptr, json_len, path_ptr, path_len, offset, limit) -> i64.
        (
            "json_query_page",
            vec![
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
                ValType::I32,
            ],
            vec![ValType::I64],
        ),
        // IMPORT_JSON_FORMAT: (ptr, len) -> i64 (NaN-boxed String or null).
        (
            "json_format",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_JSON_MINIFY: (ptr, len) -> i64 (NaN-boxed String or null).
        (
            "json_minify",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_JSON_VALID: (ptr, len) -> i32 (0/1).
        (
            "json_valid",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_JSON_STRINGIFY_PRETTY: (val) -> i64 (NaN-boxed String).
        ("json_stringify_pretty", vec![ValType::I64], vec![ValType::I64]),
        // IMPORT_SECRETS_GET: (name_ptr, name_len) -> i64 (boxed Secret handle).
        (
            "secrets_get",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I64],
        ),
        // IMPORT_SECRETS_HAS: (name_ptr, name_len) -> i32 (0/1).
        (
            "secrets_has",
            vec![ValType::I32, ValType::I32],
            vec![ValType::I32],
        ),
        // IMPORT_SECRETS_AVAILABLE: () -> i32 (1 native, 0 browser).
        ("secrets_available", vec![], vec![ValType::I32]),
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
