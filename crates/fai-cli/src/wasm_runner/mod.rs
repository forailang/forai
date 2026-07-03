//! Run compiled WASM modules via embedded Wasmtime JIT.
//!
//! The module under test must export `_start() -> i64` and `memory`.
//! Host functions are grouped into submodules under `host/`.
//!
//! A single process-wide `wasmtime::Engine` is shared via `OnceLock`.
//! Creating an Engine compiles JIT dispatch tables and is expensive; reusing
//! one across runs cuts per-run cost to roughly module compilation time.

use std::sync::OnceLock;
use std::time::Duration;

use wasmtime::*;

mod debug_table;
pub(crate) mod externs_section;
mod heap;
mod host;
mod leak_ledger;
mod nan_box;
pub mod output;
mod ownership_balance;
mod post_mortem;
mod print;

pub use fai_ffi::FfiType;
pub use host::util::{ExternGuard, ExternInfo};
pub(crate) use host::parse_dotenv;
pub(crate) use host::secrets::{SecretsGuard, SecretsManifest};

/// Shared wasmtime engine. Cheap to clone but expensive to construct; build
/// once per process.
fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(Engine::default)
}

/// Engine with epoch interruption enabled, used only for watchdog runs
/// (plan 116 phase 2). Separate from [`shared_engine`] so normal runs
/// pay zero epoch-check overhead.
fn watchdog_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.epoch_interruption(true);
        Engine::new(&config).expect("watchdog engine config is valid")
    })
}

/// Per-run execution options.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunOptions {
    /// Kill the run with a post-mortem dump (task table + heap stats)
    /// if it hasn't completed after this many seconds. Converts the
    /// worst failure mode — a silent 100%-CPU hang — into a report.
    /// `None` = no watchdog (the default; servers legitimately run
    /// forever). Armed by `fai run --watchdog [secs]` / `--debug`.
    pub watchdog_secs: Option<u64>,
    /// Heap allocation ledger (plan 116 phase 5, `--check-leaks`).
    /// `Some(..)` arms it: every alloc/free event from a `--check-leaks`
    /// build is recorded, and an itemized live-set report (grouped by
    /// size + allocation site) prints at exit and after a trap.
    pub check_leaks: Option<CheckLeaksOptions>,
    /// Helper-level ownership event consumer (plan 117 phase 4). The
    /// codegen half declares `__fai_ownership_event`; the runner half
    /// records the event stream and prints a compact helper-balance summary.
    pub check_ownership: bool,
}

/// Options for the `--check-leaks` heap ledger.
#[derive(Debug, Default, Clone, Copy)]
pub struct CheckLeaksOptions {
    /// Print a compact live-set summary every this-many milliseconds
    /// (`--check-leaks=interval:1000`) — the server mode, where "exit"
    /// never comes and the leak shows as a non-plateauing curve.
    pub interval_ms: Option<u64>,
}

impl RunOptions {
    /// Read options from the environment (`FAI_WATCHDOG=<secs>`,
    /// `FAI_CHECK_LEAKS=1|interval:<ms>`), the channel the CLI flags
    /// use so options survive the call chain without threading
    /// parameters through every compile step.
    pub fn from_env() -> Self {
        Self {
            watchdog_secs: std::env::var("FAI_WATCHDOG")
                .ok()
                .and_then(|s| s.parse().ok()),
            check_leaks: std::env::var("FAI_CHECK_LEAKS")
                .ok()
                .map(|v| CheckLeaksOptions {
                    interval_ms: v.strip_prefix("interval:").and_then(|n| n.parse().ok()),
                }),
            check_ownership: std::env::var_os("FAI_OWNERSHIP_CHECK").is_some(),
        }
    }
}

/// Background thread bumping the watchdog engine's epoch every 100ms
/// while a watchdog run is in flight; stops (and joins) on drop. The
/// store's epoch deadline is `secs * 10` ticks, so a guest stuck in a
/// wasm loop traps ~`secs` seconds in.
struct EpochTicker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: &Engine) -> Self {
        let engine = engine.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop2.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                engine.increment_epoch();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Format a wasmtime error with its full cause chain. The alternate formatter
/// (`{:#}`) on `wasmtime::Error` walks the causes, which is what users want
/// for traps and failed host imports.
fn fmt_err(context: &str, e: wasmtime::Error) -> String {
    format!("{}: {:#}", context, e)
}

/// Run a compiled FAI WASM module. Output goes to the current host stdout sink
/// (real stdout by default; a capture buffer when a [`output::CaptureGuard`]
/// is active on this thread).
///
/// Auto-discovers FFI extern metadata from the wasm's `fai-externs`
/// custom section if present, so a prebuilt `.wasm` carrying `extern`
/// blocks (sqlite, libm, etc.) dispatches `call_ffi` correctly without
/// the original source. Use [`run_wasm_with_externs`] when you have
/// the externs in hand and want to override what's embedded.
pub fn run_wasm(wasm_bytes: &[u8]) -> Result<(), String> {
    let externs = externs_section::extract_externs(wasm_bytes);
    run_wasm_with_externs(wasm_bytes, externs)
}

/// [`run_wasm`] with explicit [`RunOptions`] (auto-discovered externs).
pub fn run_wasm_opts(wasm_bytes: &[u8], opts: RunOptions) -> Result<(), String> {
    let externs = externs_section::extract_externs(wasm_bytes);
    run_wasm_with_externs_opts(wasm_bytes, externs, opts)
}

/// Same as [`run_wasm`], but populates the extern-function table the
/// host's `call_ffi` import reads. Pass an empty `Vec` when the guest
/// has no `extern` blocks. Guard is scoped to this call — the next
/// run starts with a fresh table. Options come from the environment
/// (`FAI_WATCHDOG`); use [`run_wasm_with_externs_opts`] to pass them
/// explicitly.
pub fn run_wasm_with_externs(wasm_bytes: &[u8], externs: Vec<ExternInfo>) -> Result<(), String> {
    run_wasm_with_externs_opts(wasm_bytes, externs, RunOptions::from_env())
}

/// Full-control variant of [`run_wasm_with_externs`].
pub fn run_wasm_with_externs_opts(
    wasm_bytes: &[u8],
    externs: Vec<ExternInfo>,
    opts: RunOptions,
) -> Result<(), String> {
    let _extern_guard = ExternGuard::set(externs);
    let engine = if opts.watchdog_secs.is_some() {
        watchdog_engine()
    } else {
        shared_engine()
    };
    let module = Module::new(engine, wasm_bytes).map_err(|e| fmt_err("WASM load error", e))?;
    // Debug side-table (plan 116): function index → name/file/line,
    // used to decorate trap backtraces. Empty for pre-116 binaries.
    let dbg = std::rc::Rc::new(debug_table::DbgTable::from_wasm(wasm_bytes));
    host::util::set_bucket_base(dbg.heap_buckets.map(|(b, _)| b).unwrap_or(0));
    // Arm (or clear) the heap allocation ledger for this run. Always
    // reset: a previous `--check-leaks` run on this thread must not
    // bleed records into this one. The ledger gets its own handle on
    // the debug table so interval reports can attribute allocations.
    leak_ledger::reset(
        opts.check_leaks.is_some(),
        opts.check_leaks.and_then(|c| c.interval_ms),
        opts.check_leaks.is_some().then(|| dbg.clone()),
    );
    ownership_balance::reset(
        opts.check_ownership,
        opts.check_ownership.then(|| dbg.clone()),
    );
    let mut store = Store::new(engine, ());
    let mut linker = Linker::new(engine);

    host::install_all(&mut linker)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| fmt_err("WASM instantiation error", e))?;

    // Watchdog (plan 116 phase 2): a deadline of `secs` (in 100ms epoch
    // ticks) plus a ticker thread converts a guest stuck inside wasm —
    // the silent 100%-CPU hang — into an interrupt trap we can report.
    let _ticker = opts.watchdog_secs.map(|secs| {
        store.set_epoch_deadline(secs.saturating_mul(10));
        EpochTicker::start(engine)
    });

    // Failure rendering: decorated backtrace + trap reason, then the
    // post-mortem state dump (task table + heap stats).
    let fail = |context: &str, e: &wasmtime::Error, store: &mut Store<()>| -> String {
        let mut trap_msg = host::take_trap_msg();
        if trap_msg.is_none() && matches!(e.downcast_ref::<Trap>(), Some(Trap::Interrupt)) {
            trap_msg = Some(format!(
                "watchdog: still running after {}s — interrupted",
                opts.watchdog_secs.unwrap_or(0),
            ));
        }
        let mut msg = dbg.render_trap(context, e, trap_msg);
        if let Some(pm) = post_mortem::render(&instance, store, &dbg) {
            msg.push('\n');
            msg.push_str(&pm);
        }
        if let Some(leaks) = leak_ledger::render_report(&instance, store, &dbg) {
            msg.push('\n');
            msg.push_str(&leaks);
        }
        if let Some(ownership) = ownership_balance::render_report() {
            msg.push('\n');
            msg.push_str(&ownership);
        }
        msg
    };

    if let Some(start_async) = instance
        .get_typed_func::<(), i32>(&mut store, "_start_async")
        .ok()
    {
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .map_err(|e| fmt_err("missing __fai_poll export", e))?;
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .map_err(|e| fmt_err("missing __fai_task_result export", e))?;
        // Resume tasks parked on a boundary job (e.g. outbound RPC) once their
        // worker finishes; the next poll runs their continuation (plan 101).
        let resume_task = instance
            .get_typed_func::<i32, i32>(&mut store, "__fai_resume_task")
            .ok();

        let started = std::time::Instant::now();
        let mut status = match start_async.call(&mut store, ()) {
            Ok(s) => s,
            Err(e) => return Err(fail("WASM async start error", &e, &mut store)),
        };
        while status != 2 && status != 3 {
            // Watchdog: poll keeps returning "working" but the program
            // never completes (e.g. a task forever WAITING on a child
            // that never finishes). Dump state and bail.
            if let Some(secs) = opts.watchdog_secs {
                if started.elapsed() >= Duration::from_secs(secs) {
                    let mut msg = format!(
                        "WASM watchdog: no completion after {}s — killing the run",
                        secs,
                    );
                    if let Some(pm) = post_mortem::render(&instance, &mut store, &dbg) {
                        msg.push('\n');
                        msg.push_str(&pm);
                    }
                    if let Some(leaks) = leak_ledger::render_report(&instance, &mut store, &dbg) {
                        msg.push('\n');
                        msg.push_str(&leaks);
                    }
                    if let Some(ownership) = ownership_balance::render_report() {
                        msg.push('\n');
                        msg.push_str(&ownership);
                    }
                    return Err(msg);
                }
            }
            for task_id in host::boundary::pump_ready() {
                if let Some(rt) = &resume_task {
                    let _ = rt.call(&mut store, task_id);
                }
            }
            // Socket waits whose fd fired: do the non-blocking I/O and
            // resume the parked tasks (plan 103 U5).
            let mut fired_watches = host::boundary::take_readiness();
            for task_id in host::dispatch_socket_readiness(&mut fired_watches) {
                if let Some(rt) = &resume_task {
                    let _ = rt.call(&mut store, task_id);
                }
            }
            status = match poll.call(&mut store, ()) {
                Ok(s) => s,
                Err(e) => return Err(fail("WASM async poll error", &e, &mut store)),
            };
            host::prune_fired_timers();
            if status != 2 && status != 3 {
                // Nothing runnable after the poll (the guest scheduler drains
                // its ready queue to quiescence): park until the next wake —
                // a boundary completion (outbound call, FFI offload, reactor
                // readiness) via the condvar, or the nearest timer deadline
                // via the timeout (plan 103 U4 — retires the 1ms re-poll).
                host::park_for_next_event();
            }
        }
        if status == 3 {
            let result = task_result
                .call(&mut store, 1)
                .map_err(|e| fmt_err("WASM async result error", e))?;
            report_check_leaks(&instance, &mut store, &dbg);
            report_ownership_check();
            return Err(format!(
                "WASM async task failed: {}",
                print::format_return_value(result, &instance, &mut store)
                    .unwrap_or_else(|| "<unprintable>".to_string())
            ));
        }
        let result = task_result
            .call(&mut store, 1)
            .map_err(|e| fmt_err("WASM async result error", e))?;
        print::print_return_value(result, &instance, &mut store);
        reset_retained_host_state(&instance, &mut store)?;
        report_check_leaks(&instance, &mut store, &dbg);
        report_ownership_check();
        assert_needle_absent(&instance, &mut store)?;
        return Ok(());
    }

    let start = instance
        .get_typed_func::<(), i64>(&mut store, "_start")
        .map_err(|e| fmt_err("missing _start export", e))?;

    let result = match start.call(&mut store, ()) {
        Ok(r) => r,
        Err(e) => return Err(fail("WASM execution error", &e, &mut store)),
    };

    print::print_return_value(result, &instance, &mut store);

    reset_retained_host_state(&instance, &mut store)?;
    report_leak_check(&instance, &mut store);
    report_check_leaks(&instance, &mut store, &dbg);
    report_ownership_check();
    assert_needle_absent(&instance, &mut store)?;

    Ok(())
}

/// Plan 132 phase 3 proof hook. When `FAI_ASSERT_NOT_IN_GUEST_MEMORY` is
/// set, scan the guest's ENTIRE linear memory for the needle bytes after
/// the program completes and fail the run if found. This is a raw byte
/// scan — even freed-but-not-overwritten plaintext is caught — so a
/// passing run means the value never entered guest memory at any point
/// that survived to exit, the never-in-guest-memory property the secrets
/// egress tests assert mechanically.
fn assert_needle_absent(
    instance: &wasmtime::Instance,
    store: &mut wasmtime::Store<()>,
) -> Result<(), String> {
    let Some(needle) = std::env::var_os("FAI_ASSERT_NOT_IN_GUEST_MEMORY") else {
        return Ok(());
    };
    let needle = needle.to_string_lossy().into_owned();
    if needle.is_empty() {
        return Ok(());
    }
    let Some(mem) = instance.get_memory(&mut *store, "memory") else {
        return Ok(());
    };
    let data = mem.data(&*store);
    if data
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
    {
        return Err(format!(
            "[assert-not-in-guest-memory] needle present in guest linear memory \
             ({} bytes scanned)",
            data.len()
        ));
    }
    Ok(())
}

fn report_ownership_check() {
    if let Some(report) = ownership_balance::render_report() {
        output::stderr_line(&report);
    }
}

/// Print the `--check-leaks` live-set report (when the ledger is
/// armed) to the host stderr sink at the end of a run.
fn report_check_leaks(
    instance: &wasmtime::Instance,
    store: &mut wasmtime::Store<()>,
    dbg: &debug_table::DbgTable,
) {
    if let Some(report) = leak_ledger::render_report(instance, store, dbg) {
        output::stderr_line(&report);
    }
}

/// Leak oracle (plan 113). When `FAI_LEAK_CHECK` is set, read the
/// `__live_objects` counter the runtime maintains (++ in rt_alloc, -- in
/// rt_free) after the program finishes and report how many heap objects are
/// still live.
///
/// DEPRECATED (plan 118 U1): superseded by `--check-leaks` and the
/// ledger's stable sentinel (`leak_ledger::sentinel_line`), which carry
/// attribution this counter can't. Kept because it needs no rebuild
/// flag; its `[leak-check]` prefix intentionally differs from the
/// `[check-leaks]` sentinel so parsers can't confuse the two. Do not
/// add new consumers.
fn report_leak_check(instance: &wasmtime::Instance, store: &mut wasmtime::Store<()>) {
    if std::env::var_os("FAI_LEAK_CHECK").is_none() {
        return;
    }
    match instance.get_global(&mut *store, "__live_objects") {
        Some(g) => {
            let live = g.get(&mut *store).i32().unwrap_or(-1);
            eprintln!("[leak-check] live heap objects at exit: {live}");
        }
        None => eprintln!("[leak-check] module has no __live_objects export"),
    }
}

fn reset_retained_host_state(
    instance: &wasmtime::Instance,
    store: &mut wasmtime::Store<()>,
) -> Result<(), String> {
    let mut retained = host::drain_spy_retained_values();
    retained.extend(host::drain_router_retained_values());
    if retained.is_empty() {
        return Ok(());
    }
    let release = instance
        .get_typed_func::<i64, ()>(&mut *store, "__fai_release")
        .map_err(|e| fmt_err("missing __fai_release export for spy reset", e))?;
    for value in retained {
        release
            .call(&mut *store, value)
            .map_err(|e| fmt_err("spy reset release failed", e))?;
    }
    Ok(())
}

/// Output captured from a single [`run_wasm_capturing`] invocation.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct CapturedOutput {
    pub stdout: String,
    pub stderr: String,
}

/// Run a compiled FAI WASM module with stdout/stderr routed to in-memory
/// buffers. Intended for test harnesses that need to assert on program output
/// without process-level redirection.
///
/// The capture is scoped to this call. If `run_wasm` is called concurrently
/// from another thread, those writes are unaffected (capture is thread-local).
#[cfg(test)]
pub fn run_wasm_capturing(wasm_bytes: &[u8]) -> Result<CapturedOutput, String> {
    let guard = output::CaptureGuard::new();
    let run_result = run_wasm(wasm_bytes);
    let captured = CapturedOutput {
        stdout: guard.stdout(),
        stderr: guard.stderr(),
    };
    drop(guard);
    run_result.map(|()| captured)
}

/// Outcome of one `(suite, case)` invocation.
pub struct CaseOutcome {
    pub suite_name: String,
    pub case_desc: String,
    /// `None` on success; `Some(msg)` on trap.
    pub error: Option<String>,
    /// Source line of the suite's `test` keyword. 0 when unknown.
    /// Used by the CLI to point a failing test at its source.
    pub suite_line: u32,
}

/// Per-run options for the test runner (plan 118 U2). Threaded as a
/// parameter rather than ambient env vars so in-process callers can't
/// race each other's settings.
#[derive(Default, Clone)]
pub struct TestRunOptions {
    /// Arm per-case leak deltas (`fai test --check-leaks`). The wasm
    /// must also be BUILT with the check-leaks codegen flag or the
    /// ledger sees no events (the caller arms that before codegen).
    pub check_leaks: bool,
    /// Arm helper ownership event recording for the whole test run.
    pub check_ownership: bool,
    /// Leak allowances: exact `suite` or `suite::case` names (equality,
    /// not substring). An allowed case with a delta reports as leaked-
    /// allowed and still passes.
    pub allow_leaks: Vec<String>,
}

impl TestRunOptions {
    fn allows(&self, suite: &str, case: &str) -> bool {
        self.allow_leaks
            .iter()
            .any(|a| a == suite || a == &format!("{}::{}", suite, case))
    }
}

/// Structured summary returned by [`run_wasm_tests`].
#[derive(Debug, Default, Clone)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub total: usize,
    /// Per-suite grouped case descriptions + outcomes, in the order the
    /// suites appear in the compiled program. The CLI caller prints this
    /// in the familiar ✓/✗ form.
    pub suites: Vec<SuiteReport>,
}

const TEST_HOOK_BEFORE_ALL_CASE_IDX: i32 = u16::MAX as i32;
const TEST_HOOK_AFTER_ALL_CASE_IDX: i32 = (u16::MAX - 1) as i32;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SuiteReport {
    pub suite_name: String,
    pub cases: Vec<CaseReport>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CaseReport {
    pub description: String,
    pub passed: bool,
    pub message: Option<String>,
}

/// Execute every `(suite, case)` pair in a compiled program via the
/// `_fai_run_test` export and return a structured summary. The top-level
/// script runs once via `_start` before the suite loop so globals and
/// module initialisers fire — matching VM parity.
///
/// `on_case` is invoked per case so the caller can print ✓/✗ lines as
/// they happen, which is what the existing CLI UX expects.
#[cfg(test)]
pub fn run_wasm_tests(
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    on_case: impl FnMut(&CaseOutcome),
) -> Result<TestSummary, String> {
    run_wasm_tests_with_externs(
        wasm_bytes,
        tests,
        Vec::new(),
        &TestRunOptions::default(),
        on_case,
    )
}

/// How one `(suite, case)` executes: the legacy synchronous dispatcher, or
/// spawn-and-drive on the guest scheduler (plan 103 U7 — engine test builds).
enum CaseRunner {
    Sync(TypedFunc<(i32, i32), ()>),
    Engine(EngineCaseRunner),
}

struct EngineCaseRunner {
    spawn: TypedFunc<(i32, i32), i32>,
    poll: TypedFunc<(), i32>,
    resume: TypedFunc<i32, i32>,
    status: TypedFunc<i32, i32>,
    result: TypedFunc<i32, i64>,
    free: TypedFunc<i32, ()>,
    /// `__dbg_live` — live-task count, read to drain leaked tasks per case.
    live: Global,
}

/// Task-status values from `fai-codegen-wasm::async_engine`.
const TASK_ST_COMPLETE: i32 = 3;
const TASK_ST_FAILED: i32 = 4;

impl CaseRunner {
    fn run_case(
        &self,
        instance: &Instance,
        store: &mut Store<()>,
        suite: i32,
        case: i32,
    ) -> Result<(), wasmtime::Error> {
        match self {
            CaseRunner::Sync(f) => f.call(&mut *store, (suite, case)),
            CaseRunner::Engine(r) => r.run_case(instance, store, suite, case),
        }
    }
}

impl EngineCaseRunner {
    /// One pump-resume-poll turn of the scheduler (mirrors the async run
    /// loop). A trap inside `poll` propagates as the case's error, exactly
    /// like a trap inside the legacy `_fai_run_test` call.
    fn turn(&self, store: &mut Store<()>) -> Result<(), wasmtime::Error> {
        for task_id in host::boundary::pump_ready() {
            let _ = self.resume.call(&mut *store, task_id)?;
        }
        let mut fired = host::boundary::take_readiness();
        for task_id in host::dispatch_socket_readiness(&mut fired) {
            let _ = self.resume.call(&mut *store, task_id)?;
        }
        let _ = self.poll.call(&mut *store, ())?;
        host::prune_fired_timers();
        Ok(())
    }

    fn live_count(&self, store: &mut Store<()>) -> i32 {
        match self.live.get(&mut *store) {
            Val::I32(n) => n,
            _ => 0,
        }
    }

    fn run_case(
        &self,
        instance: &Instance,
        store: &mut Store<()>,
        suite: i32,
        case: i32,
    ) -> Result<(), wasmtime::Error> {
        let id = self.spawn.call(&mut *store, (suite, case))?;
        if id < 0 {
            return Err(wasmtime::Error::msg(format!(
                "unknown test case (suite {suite}, case {case})"
            )));
        }
        // Per-case watchdog: an engine case that never completes (a lost
        // wakeup, a task parked forever) must fail the case, not hang the
        // whole run the way a stuck synchronous call would.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let final_status = loop {
            self.turn(&mut *store)?;
            let st = self.status.call(&mut *store, id)?;
            if st >= TASK_ST_COMPLETE {
                break st;
            }
            if std::time::Instant::now() >= deadline {
                return Err(wasmtime::Error::msg(
                    "test case timed out after 60s on the scheduler",
                ));
            }
            host::park_for_next_event();
        };
        // A failed task carries the thrown error value; render it as the
        // case's failure message (assertion errors arrive this way — they
        // are guest throws, not traps, under the engine).
        let failure: Option<String> = if final_status == TASK_ST_FAILED {
            let err_val = self.result.call(&mut *store, id)?;
            Some(
                print::format_return_value(err_val, instance, &mut *store)
                    .unwrap_or_else(|| "<unprintable test error>".to_string()),
            )
        } else {
            None
        };
        self.free.call(&mut *store, id)?;
        // Drain the scheduler so nothing leaks into the next case (plan 103
        // R9): a `nowait` task or live timer left behind keeps running here,
        // bounded by a grace period, after which it fails THIS case with a
        // leak diagnostic naming the count.
        let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let live = self.live_count(store);
            if live == 0 && !host::boundary::has_inflight() {
                break;
            }
            if std::time::Instant::now() >= drain_deadline {
                let leak = format!(
                    "case leaked {live} live scheduler task(s) past its end"
                );
                return Err(match failure {
                    Some(msg) => wasmtime::Error::msg(format!("{msg}; {leak}")),
                    None => wasmtime::Error::msg(leak),
                });
            }
            self.turn(&mut *store)?;
            if self.live_count(store) == 0 && !host::boundary::has_inflight() {
                break;
            }
            host::park_for_next_event();
        }
        match failure {
            Some(msg) => Err(wasmtime::Error::msg(msg)),
            None => Ok(()),
        }
    }
}

/// Same as [`run_wasm_tests`] but populates the extern-function table
/// the host's `call_ffi` import reads. Pass an empty `Vec` when the
/// guest has no `extern` blocks.
pub fn run_wasm_tests_with_externs(
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    externs: Vec<ExternInfo>,
    opts: &TestRunOptions,
    mut on_case: impl FnMut(&CaseOutcome),
) -> Result<TestSummary, String> {
    let _extern_guard = ExternGuard::set(externs);
    // Debug instrumentation: FAI_HEAP_VERIFY feeds the host-side
    // free-list scan its bucket base; the ledger arms for FAI_CHECK_LEAKS
    // (observability) or opts.check_leaks (per-case assertion, plan 118).
    let leaks = opts.check_leaks || std::env::var_os("FAI_CHECK_LEAKS").is_some();
    let ownership = opts.check_ownership || std::env::var_os("FAI_OWNERSHIP_CHECK").is_some();
    let debug_env = std::env::var_os("FAI_HEAP_VERIFY").is_some() || leaks || ownership;
    let dbg = if debug_env {
        Some(std::rc::Rc::new(debug_table::DbgTable::from_wasm(
            wasm_bytes,
        )))
    } else {
        None
    };
    if let Some(dbg) = &dbg {
        host::util::set_bucket_base(dbg.heap_buckets.map(|(b, _)| b).unwrap_or(0));
        // FAI_CHECK_LEAKS_INTERVAL_MS streams a periodic live-set report
        // (top groups WITH allocation sites) during a test run — the way
        // to pinpoint a runaway allocator that never reaches the exit
        // report because the suite is still climbing.
        let interval_ms = std::env::var("FAI_CHECK_LEAKS_INTERVAL_MS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok());
        leak_ledger::reset(leaks, interval_ms, leaks.then(|| dbg.clone()));
    }
    ownership_balance::reset(ownership, if ownership { dbg.clone() } else { None });
    let engine = shared_engine();
    let module = Module::new(engine, wasm_bytes).map_err(|e| fmt_err("WASM load error", e))?;
    let mut store = Store::new(engine, ());
    let mut linker = Linker::new(engine);
    host::install_all(&mut linker)?;
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| fmt_err("WASM instantiation error", e))?;

    // Engine test module (plan 103 U7): cases are spawned as scheduler
    // tasks via `_fai_spawn_test` and driven by the host. Legacy modules
    // keep the synchronous `_fai_run_test` dispatcher.
    let spawn_test = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, "_fai_spawn_test")
        .ok();

    // Run the top-level script so globals / module init effects land.
    if spawn_test.is_some() {
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .map_err(|e| fmt_err("missing _start_async export", e))?;
        let _ = start_async
            .call(&mut store, ())
            .map_err(|e| fmt_err("script init error", e))?;
    } else {
        let start = instance
            .get_typed_func::<(), i64>(&mut store, "_start")
            .map_err(|e| fmt_err("missing _start export", e))?;
        let _ = start
            .call(&mut store, ())
            .map_err(|e| fmt_err("script init error", e))?;
    }

    // Library files with no `test` blocks have no `_fai_run_test`
    // export — the assembler only emits it when there are cases to
    // dispatch. Nothing to run in that situation; return an empty
    // summary so the outer pipeline can still report missing-test
    // coverage failures for the file.
    let mut summary = TestSummary::default();
    if tests.is_empty() {
        report_ownership_check();
        return Ok(summary);
    }

    let run_test = match spawn_test {
        Some(spawn) => CaseRunner::Engine(EngineCaseRunner {
            spawn,
            poll: instance
                .get_typed_func::<(), i32>(&mut store, "__fai_poll")
                .map_err(|e| fmt_err("missing __fai_poll export", e))?,
            resume: instance
                .get_typed_func::<i32, i32>(&mut store, "__fai_resume_task")
                .map_err(|e| fmt_err("missing __fai_resume_task export", e))?,
            status: instance
                .get_typed_func::<i32, i32>(&mut store, "__fai_task_status")
                .map_err(|e| fmt_err("missing __fai_task_status export", e))?,
            result: instance
                .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
                .map_err(|e| fmt_err("missing __fai_task_result export", e))?,
            free: instance
                .get_typed_func::<i32, ()>(&mut store, "__fai_free_task")
                .map_err(|e| fmt_err("missing __fai_free_task export", e))?,
            live: instance
                .get_global(&mut store, "__dbg_live")
                .ok_or_else(|| "missing __dbg_live export".to_string())?,
        }),
        None => CaseRunner::Sync(
            instance
                .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
                .map_err(|e| fmt_err("missing _fai_run_test export", e))?,
        ),
    };

    // Per-case leak deltas (plan 118 U2): when armed, snapshot the
    // ledger around each `_fai_run_test` call. The case wrapper bakes
    // setup+beforeEach+body+afterEach into that one call, so cleanup is
    // inside the window by construction — a case passes only if its
    // cleanup really releases. The memory handle feeds object tags to
    // the delta report.
    let leak_memory = if opts.check_leaks {
        instance.get_memory(&mut store, "memory")
    } else {
        None
    };

    summary.total = tests.iter().map(|t| t.case_descriptions.len()).sum();
    for (suite_i, test) in tests.iter().enumerate() {
        let mut suite_report = SuiteReport {
            suite_name: test.suite_name.clone(),
            cases: Vec::with_capacity(test.case_descriptions.len()),
        };
        // Pre-suite snapshot: feeds the suite-level report so
        // beforeAll/afterAll growth is visible (informational — shared
        // state built in beforeAll is legitimate and per-case baselines
        // start AFTER it).
        let pre_suite = if opts.check_leaks {
            leak_ledger::snapshot()
        } else {
            None
        };
        if test.has_before_all {
            reset_retained_host_state(&instance, &mut store)?;
            run_test
                .run_case(&instance, &mut store, suite_i as i32, TEST_HOOK_BEFORE_ALL_CASE_IDX)
                .map_err(|e| fmt_err(&format!("beforeAll failed in '{}'", test.suite_name), e))?;
        }
        for (case_i, desc) in test.case_descriptions.iter().enumerate() {
            // Clear spy/mock state between cases so call counts and
            // mocked values don't bleed across `it(...)` blocks.
            reset_retained_host_state(&instance, &mut store)?;
            // FAI_TRACE_TESTS: name each case on the real stderr before it
            // runs (bypassing the per-test output capture), so a hang or
            // runaway is pinned to the test that never returns.
            if std::env::var_os("FAI_TRACE_TESTS").is_some() {
                eprintln!("[trace] {} — {}", test.suite_name, desc);
                use std::io::Write as _;
                let _ = std::io::stderr().flush();
            }
            let pre_case = if opts.check_leaks {
                leak_ledger::snapshot()
            } else {
                None
            };
            let mut res =
                run_test.run_case(&instance, &mut store, suite_i as i32, case_i as i32);
            if let Err(e) = reset_retained_host_state(&instance, &mut store) {
                if res.is_ok() {
                    res = Err(wasmtime::Error::msg(e));
                }
            }
            // Leak verdict for the case window. Only evaluated on a
            // clean return — a trapped case left guest state
            // indeterminate, and the trap is the failure that matters.
            let leak_failure: Option<String> = match (&res, &pre_case) {
                (Ok(()), Some(snap)) => {
                    let data = leak_memory.as_ref().map(|m| m.data(&store)).unwrap_or(&[]);
                    match leak_ledger::delta_since(snap, data) {
                        Some(d) if d.new_count > 0 => {
                            if opts.allows(&test.suite_name, desc) {
                                eprintln!(
                                    "[check-leaks] {} — {}: leaked {} object(s), {} bytes (allowed)",
                                    test.suite_name, desc, d.new_count, d.new_bytes
                                );
                                None
                            } else {
                                Some(format!(
                                    "leaked {} object(s), {} bytes{}",
                                    d.new_count, d.new_bytes, d.report
                                ))
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            // A leak verdict converts a passing case into a failure;
            // take_trap_msg() is empty on a clean return, so the error
            // formatting below falls through to this message.
            let res = match (res, leak_failure) {
                (Ok(()), Some(msg)) => Err(wasmtime::Error::msg(msg)),
                (r, _) => r,
            };
            let outcome = match res {
                Ok(()) => {
                    summary.passed += 1;
                    suite_report.cases.push(CaseReport {
                        description: desc.clone(),
                        passed: true,
                        message: None,
                    });
                    CaseOutcome {
                        suite_name: test.display_name(),
                        case_desc: desc.clone(),
                        error: None,
                        suite_line: test.line,
                    }
                }
                Err(e) => {
                    summary.failed += 1;
                    // Prefer the guest-supplied assertion message if
                    // one was stashed by IMPORT_SET_TRAP_MSG; fall back
                    // to wasmtime's full error chain for non-assertion
                    // traps (register overflow, unreachable from other
                    // call paths, etc.).
                    let msg = host::take_trap_msg().unwrap_or_else(|| format!("{:#}", e));
                    suite_report.cases.push(CaseReport {
                        description: desc.clone(),
                        passed: false,
                        message: Some(msg.clone()),
                    });
                    CaseOutcome {
                        suite_name: test.display_name(),
                        case_desc: desc.clone(),
                        error: Some(msg),
                        suite_line: test.line,
                    }
                }
            };
            on_case(&outcome);
        }
        if test.has_after_all {
            reset_retained_host_state(&instance, &mut store)?;
            run_test
                .run_case(&instance, &mut store, suite_i as i32, TEST_HOOK_AFTER_ALL_CASE_IDX)
                .map_err(|e| fmt_err(&format!("afterAll failed in '{}'", test.suite_name), e))?;
            reset_retained_host_state(&instance, &mut store)?;
        }
        // Suite-level report (informational, never a failure): growth
        // since the pre-suite snapshot that per-case windows can't see —
        // beforeAll/afterAll allocations plus allowed-case residue.
        // Shared state built in beforeAll is legitimate, so this line
        // exists for attribution, not assertion.
        if let Some(snap) = &pre_suite {
            let data = leak_memory.as_ref().map(|m| m.data(&store)).unwrap_or(&[]);
            if let Some(d) = leak_ledger::delta_since(snap, data) {
                let net = d.new_count as i64 - d.freed_count as i64;
                if net != 0 {
                    eprintln!(
                        "[check-leaks] suite '{}': net {:+} object(s) ({} new, {} freed; {} new bytes) across the suite",
                        test.suite_name, net, d.new_count, d.freed_count, d.new_bytes
                    );
                }
            }
        }
        summary.suites.push(suite_report);
    }
    let ownership_failed = ownership_balance::has_imbalance();
    report_ownership_check();
    if ownership_failed {
        summary.failed += 1;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
