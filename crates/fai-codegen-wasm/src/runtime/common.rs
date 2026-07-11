use super::*;

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
            avail[IMPORT_CRYPTO_RANDOM_HEX as usize] = false;
            avail[IMPORT_CRYPTO_PBKDF2_SHA256_HEX as usize] = false;
            avail[IMPORT_CRYPTO_SHA256_BASE64URL as usize] = false;
            avail[IMPORT_CRYPTO_BASE64URL_ENCODE as usize] = false;
            avail[IMPORT_CRYPTO_BASE64URL_DECODE as usize] = false;
            avail[IMPORT_CRYPTO_AES_GCM_ENCRYPT as usize] = false;
            avail[IMPORT_CRYPTO_AES_GCM_DECRYPT as usize] = false;
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
pub(super) fn emit_trap_report_unreachable(
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

pub(super) fn mem0() -> MemArg {
    MemArg {
        offset: 0,
        align: 0,
        memory_index: 0,
    }
}
