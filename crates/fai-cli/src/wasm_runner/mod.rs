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
mod nan_box;
pub mod output;
mod post_mortem;
mod print;

pub use fai_ffi::FfiType;
pub use host::util::{ExternGuard, ExternInfo};

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
}

impl RunOptions {
    /// Read options from the environment (`FAI_WATCHDOG=<secs>`), the
    /// channel the CLI flags use so options survive the call chain
    /// without threading parameters through every compile step.
    pub fn from_env() -> Self {
        Self {
            watchdog_secs: std::env::var("FAI_WATCHDOG")
                .ok()
                .and_then(|s| s.parse().ok()),
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
    let dbg = debug_table::DbgTable::from_wasm(wasm_bytes);
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
    let fail = |context: &str,
                e: &wasmtime::Error,
                store: &mut Store<()>|
     -> String {
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
                    return Err(msg);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
            status = match poll.call(&mut store, ()) {
                Ok(s) => s,
                Err(e) => return Err(fail("WASM async poll error", &e, &mut store)),
            };
        }
        if status == 3 {
            let result = task_result
                .call(&mut store, 1)
                .map_err(|e| fmt_err("WASM async result error", e))?;
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

    report_leak_check(&instance, &mut store);

    Ok(())
}

/// Leak oracle (plan 113). When `FAI_LEAK_CHECK` is set, read the
/// `__live_objects` counter the runtime maintains (++ in rt_alloc, -- in
/// rt_free) after the program finishes and report how many heap objects are
/// still live. Once reference counting is emitted at every reference site (P3),
/// a no-leak program returns to its root set (0 for a no-global program), so
/// this becomes a hard pass/fail oracle; for now it is observational, since
/// nothing releases yet.
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
    run_wasm_tests_with_externs(wasm_bytes, tests, Vec::new(), on_case)
}

/// Same as [`run_wasm_tests`] but populates the extern-function table
/// the host's `call_ffi` import reads. Pass an empty `Vec` when the
/// guest has no `extern` blocks.
pub fn run_wasm_tests_with_externs(
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    externs: Vec<ExternInfo>,
    mut on_case: impl FnMut(&CaseOutcome),
) -> Result<TestSummary, String> {
    let _extern_guard = ExternGuard::set(externs);
    let engine = shared_engine();
    let module = Module::new(engine, wasm_bytes).map_err(|e| fmt_err("WASM load error", e))?;
    let mut store = Store::new(engine, ());
    let mut linker = Linker::new(engine);
    host::install_all(&mut linker)?;
    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| fmt_err("WASM instantiation error", e))?;

    // Run the top-level script so globals / module init effects land.
    let start = instance
        .get_typed_func::<(), i64>(&mut store, "_start")
        .map_err(|e| fmt_err("missing _start export", e))?;
    let _ = start
        .call(&mut store, ())
        .map_err(|e| fmt_err("script init error", e))?;

    // Library files with no `test` blocks have no `_fai_run_test`
    // export — the assembler only emits it when there are cases to
    // dispatch. Nothing to run in that situation; return an empty
    // summary so the outer pipeline can still report missing-test
    // coverage failures for the file.
    let mut summary = TestSummary::default();
    if tests.is_empty() {
        return Ok(summary);
    }

    let run_test = instance
        .get_typed_func::<(i32, i32), ()>(&mut store, "_fai_run_test")
        .map_err(|e| fmt_err("missing _fai_run_test export", e))?;

    summary.total = tests.iter().map(|t| t.case_descriptions.len()).sum();
    for (suite_i, test) in tests.iter().enumerate() {
        let mut suite_report = SuiteReport {
            suite_name: test.suite_name.clone(),
            cases: Vec::with_capacity(test.case_descriptions.len()),
        };
        if test.has_before_all {
            host::reset_spy_state();
            run_test
                .call(&mut store, (suite_i as i32, TEST_HOOK_BEFORE_ALL_CASE_IDX))
                .map_err(|e| fmt_err(&format!("beforeAll failed in '{}'", test.suite_name), e))?;
        }
        for (case_i, desc) in test.case_descriptions.iter().enumerate() {
            // Clear spy/mock state between cases so call counts and
            // mocked values don't bleed across `it(...)` blocks.
            host::reset_spy_state();
            let res = run_test.call(&mut store, (suite_i as i32, case_i as i32));
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
            host::reset_spy_state();
            run_test
                .call(&mut store, (suite_i as i32, TEST_HOOK_AFTER_ALL_CASE_IDX))
                .map_err(|e| fmt_err(&format!("afterAll failed in '{}'", test.suite_name), e))?;
        }
        summary.suites.push(suite_report);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a FAI source string to WASM bytes via the direct
    /// AST→wasm path (Phase H: no bytecode fallback).
    fn compile_to_wasm(src: &str) -> Vec<u8> {
        let prepared = fai_compiler::prepare_source(src, None).expect("prepare_source failed");
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("type check failed");
        let info = fai_codegen_wasm::direct::CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };
        fai_codegen_wasm::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            false,
        )
        .expect("direct codegen refused")
    }


    /// Regression (plans/111): a function whose TAIL is a user-call await
    /// lowers to `Term::CompletePending`, which recycles the awaited child slot
    /// (`free_pending`) by reading this task's frame. Phase 4 frame reclaim made
    /// `complete` free the frame, so `free_pending` MUST run before `complete`
    /// — otherwise it reads a freed frame → corrupts the slot freelist → wasm
    /// OOB (this is what broke brain's conversation loader). Driving the shape N
    /// times must stay correct (and not trap).
    #[test]
    fn complete_pending_tail_await_is_not_use_after_free() {
        let src = "# Suspend then return a constant.\n\
                   def leaf\n    @return Int\ndo\n  sleep(0)\n  42\nend\n\n\
                   # Tail await a user fn → CompletePending.\n\
                   def step\n    @return Int\ndo\n  leaf()\nend\n\n\
                   def main\n    @return Int\ndo\n  var total Int = 0\n  var i Int = 0\n  while i < 200\n    total = total + step()\n    i = i + 1\n  end\n  total\nend\n";
        let wasm = compile_to_wasm(src);
        let (mut store, instance) = instantiate_for_async_test(&wasm);
        let start = instance.get_typed_func::<(), i32>(&mut store, "_start_async").expect("_start_async");
        let poll = instance.get_typed_func::<(), i32>(&mut store, "__fai_poll").expect("__fai_poll");
        let result = instance.get_typed_func::<i32, i64>(&mut store, "__fai_task_result").expect("task_result");
        let mut status = start.call(&mut store, ()).expect("start");
        let mut guard = 0u64;
        while status != 2 && status != 3 {
            status = poll.call(&mut store, ()).expect("poll");
            guard += 1;
            assert!(guard < 50_000_000, "did not converge");
        }
        assert_eq!(status, 2, "root completed ok (no trap/corruption)");
        // total = 200 * 42. Decode the NaN-boxed Int result.
        let boxed = result.call(&mut store, 1).expect("result") as u64;
        let int_val = (boxed & 0xFFFF_FFFF) as u32 as i32;
        assert_eq!(int_val, 200 * 42, "correct result → no slot-freelist corruption");
    }

    fn instantiate_for_async_test(wasm: &[u8]) -> (Store<()>, Instance) {
        host::clear_timer_requests();
        let engine = shared_engine();
        let module = Module::new(engine, wasm).expect("WASM should load");
        let mut store = Store::new(engine, ());
        let mut linker = Linker::new(engine);
        host::install_all(&mut linker).expect("host imports should install");
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("WASM should instantiate");
        (store, instance)
    }

    #[test]
    fn test_run_wasm_hello_world() {
        let src = "def main\n    @return Void\ndo\n  print('hello')\nend\n";
        let wasm = compile_to_wasm(src);
        let result = run_wasm(&wasm);
        assert!(result.is_ok(), "run_wasm failed: {:?}", result);
    }

    #[test]
    fn test_run_wasm_returns_int() {
        let src = "def main\n    @return Int\ndo\n  42\nend\n";
        let wasm = compile_to_wasm(src);
        let result = run_wasm(&wasm);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_wasm_returns_bool() {
        let src = "def main\n    @return Bool\ndo\n  true\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_returns_null() {
        let src = "def main\n    @return Int?\ndo\n  null\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_returns_string() {
        let src = "def main\n    @return String\ndo\n  'world'\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_returns_float() {
        let src = "def main\n    @return Float\ndo\n  3.14\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_returns_false() {
        // Covers the `false` boolean branch in print_return_value
        let src = "def main\n    @return Bool\ndo\n  false\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_invalid_bytes_returns_error() {
        let result = run_wasm(b"not valid wasm");
        assert!(result.is_err());
    }

    #[test]
    fn test_run_wasm_with_all_concurrency() {
        // all(...) lowers through the real engine: each child is spawned as
        // a task and joined. The children here don't suspend, so they
        // complete immediately; `main` resumes once both are done.
        let src = concat!(
            "# Returns x doubled.\n",
            "def double\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  x * 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let a, b = all(double(1), double(2))\n",
            "  print('done')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("all() should run");
        assert_eq!(output.stdout.trim(), "done");
    }

    #[test]
    fn test_closure_param_call_preserves_frame_vars() {
        // Regression: calling a closure PARAMETER (`build()`) lowers to
        // `Term::AwaitClosure`, which writes a child/synth task id to
        // `frame[pending_off]`. The frame must reserve that pending slot — if
        // `stmt_pending_count` doesn't count the closure call, the write
        // overflows the frame into the adjacent heap object and silently
        // corrupts a still-live param (here `label` would print empty).
        // This is exactly what broke forui `Link` (`children()` wiped `to`).
        // Must run through the REAL async engine (not the is_test bypass).
        let src = concat!(
            "# A builder closure.\n",
            "type def Builder\n",
            "    @return Void\n",
            "end\n",
            "\n",
            "# Calls the closure param, then uses a param declared before it.\n",
            "def make\n",
            "    @param label String\n",
            "    @param build Builder\n",
            "    @return Void\n",
            "do\n",
            "  build()\n",
            "  print(label)\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  make('alpha') do\n",
            "\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("closure-param call should run");
        assert_eq!(
            output.stdout.trim(),
            "alpha",
            "param `label` must survive a closure-parameter call (Term::AwaitClosure)"
        );
    }

    #[test]
    fn test_run_wasm_json_parse() {
        // Exercises env.json_parse and build_value
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let v = json.parse('{\"x\": 1}')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_print_float() {
        // print(float) exercises env.float_to_str then env.print
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print(3.14)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_time_now() {
        // Exercises env.now_ms
        let src = concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let t = time.now()\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_random() {
        // Exercises env.random
        let src = concat!(
            "use std.math\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let r = math.random()\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_file_read() {
        // Exercises env.read_file (returns -1 for non-existent path)
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let contents = file.read('/tmp/nonexistent_fai_test')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_nowait() {
        let src = concat!(
            "# A background task.\n",
            "def background\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(25)\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 10\n",
            "  nowait background(base)\n",
            "  base + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("minimal nowait should run");
        assert_eq!(output.stdout.trim(), "11");
    }

    #[test]
    fn test_nowait_child_task_can_finish_after_root() {
        let src = concat!(
            "# A background task.\n",
            "def background\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(20)\n",
            "  x + 5\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 10\n",
            "  nowait background(base)\n",
            "  base + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let (mut store, instance) = instantiate_for_async_test(&wasm);
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("missing _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("missing __fai_poll");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("missing __fai_task_result");

        // Drain-until-idle: `main` (task 1) completes immediately with
        // base+1 = 11, but the forked `background` (task 2) is still
        // sleeping, so the scheduler is not yet idle — `_start_async`
        // reports "working" (1), not complete.
        assert_eq!(start_async.call(&mut store, ()).unwrap(), 1);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 1).unwrap() as u64),
            nan_box::ReturnKind::Int(11)
        );
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 2).unwrap() as u64),
            nan_box::ReturnKind::Void
        );

        // After the background timer elapses, polling resumes it; once it
        // finishes the scheduler is idle and reports complete (2).
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(poll.call(&mut store, ()).unwrap(), 2);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 2).unwrap() as u64),
            nan_box::ReturnKind::Int(15)
        );
    }

    #[test]
    fn test_nowait_child_throw_does_not_fail_root() {
        let src = concat!(
            "# Throws after waiting in the background.\n",
            "def background\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(20)\n",
            "  throw 7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 10\n",
            "  nowait background(base)\n",
            "  base + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let (mut store, instance) = instantiate_for_async_test(&wasm);
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("missing _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("missing __fai_poll");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("missing __fai_task_result");

        // main completes with 11 immediately, but the nowait'd `background`
        // is still sleeping — the scheduler keeps draining (status 1).
        assert_eq!(start_async.call(&mut store, ()).unwrap(), 1);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 1).unwrap() as u64),
            nan_box::ReturnKind::Int(11)
        );
        // After the background timer, it throws and fails — but it was
        // forked with `nowait`, so nobody awaits it: the root is unaffected
        // and the scheduler reports idle (2), not failed (3).
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(poll.call(&mut store, ()).unwrap(), 2);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 1).unwrap() as u64),
            nan_box::ReturnKind::Int(11)
        );
        // The failed background task's error (7) is in its result slot.
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 2).unwrap() as u64),
            nan_box::ReturnKind::Int(7)
        );
    }

    #[test]
    fn test_run_wasm_nowait_sync_body_runs() {
        // A `nowait` of a non-suspending function: the body is compiled as
        // a task that completes immediately. `main` runs to completion
        // first (it doesn't suspend), then the forked task runs.
        let src = concat!(
            "# A background task.\n",
            "def background\n",
            "    @return Void\n",
            "do\n",
            "  print('bg')\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  nowait background()\n",
            "  print('done')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("nowait sync body should run");
        assert_eq!(output.stdout.trim(), "done\nbg");
    }

    #[test]
    fn test_run_wasm_sleep_builtin() {
        // `sleep(ms)` lowers through the real async engine: the body
        // runs as a resume function that suspends on `sleep` and resumes
        // to run the rest. The `print` after the suspension must execute.
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  sleep(0)\n",
            "  print('done')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("async sleep body should run");
        assert_eq!(output.stdout.trim(), "done");
    }

    #[test]
    fn test_run_wasm_minimal_wait_async() {
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  42\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("minimal async wait should run");
        assert_eq!(output.stdout.trim(), "42");
    }

    #[test]
    fn test_run_wasm_wait_preserves_int_local() {
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let x = 41\n",
            "  sleep(1)\n",
            "  x + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("async wait should preserve local");
        assert_eq!(output.stdout.trim(), "42");
    }

    #[test]
    fn test_run_wasm_minimal_auto_await_call() {
        let src = concat!(
            "# Returns after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let x = child()\n",
            "  x + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("minimal auto-await should run");
        assert_eq!(output.stdout.trim(), "8");
    }

    #[test]
    fn test_run_wasm_auto_await_call_with_int_arg() {
        let src = concat!(
            "# Returns x after waiting.\n",
            "def child\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 7\n",
            "  let y = child(base)\n",
            "  y + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("auto-await arg should run");
        assert_eq!(output.stdout.trim(), "9");
    }

    #[test]
    fn test_run_wasm_async_throw_after_wait_fails() {
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 7\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let err = run_wasm_capturing(&wasm).expect_err("async throw should fail");
        assert!(
            err.contains("WASM async task failed: 7"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_run_wasm_try_catches_async_throw_after_wait() {
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    sleep(1)\n",
            "    throw 7\n",
            "  catch e\n",
            "    8\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("async catch should run");
        assert_eq!(output.stdout.trim(), "8");
    }

    #[test]
    fn test_run_wasm_auto_await_child_throw_fails_parent() {
        let src = concat!(
            "# Throws after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let x = child()\n",
            "  x + 1\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let err = run_wasm_capturing(&wasm).expect_err("auto-await throw should fail");
        assert!(
            err.contains("WASM async task failed: 7"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_run_wasm_try_catches_auto_await_child_throw() {
        let src = concat!(
            "# Throws after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    let x = child()\n",
            "    x + 1\n",
            "  catch e\n",
            "    8\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("auto-await catch should run");
        assert_eq!(output.stdout.trim(), "8");
    }

    #[test]
    fn test_run_wasm_minimal_all_waits() {
        let src = concat!(
            "# Returns after a slower wait.\n",
            "def slow\n",
            "    @return Int\n",
            "do\n",
            "  sleep(50)\n",
            "  1\n",
            "end\n",
            "\n",
            "# Returns after a faster wait.\n",
            "def fast\n",
            "    @return Int\n",
            "do\n",
            "  sleep(10)\n",
            "  2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a, b = all(slow(), fast())\n",
            "  a + b\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("minimal all should run");
        assert_eq!(output.stdout.trim(), "3");
    }

    #[test]
    fn test_run_wasm_all_child_throw_fails_parent() {
        let src = concat!(
            "# Returns after a slower wait.\n",
            "def slow\n",
            "    @return Int\n",
            "do\n",
            "  sleep(50)\n",
            "  1\n",
            "end\n",
            "\n",
            "# Throws after a faster wait.\n",
            "def fast\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 9\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let a, b = all(slow(), fast())\n",
            "  a + b\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let err = run_wasm_capturing(&wasm).expect_err("all child throw should fail");
        assert!(
            err.contains("WASM async task failed: 9"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_run_wasm_try_catches_all_child_throw() {
        let src = concat!(
            "# Returns after a slower wait.\n",
            "def slow\n",
            "    @return Int\n",
            "do\n",
            "  sleep(50)\n",
            "  1\n",
            "end\n",
            "\n",
            "# Throws after a faster wait.\n",
            "def fast\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 9\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    let a, b = all(slow(), fast())\n",
            "    99\n",
            "  catch e\n",
            "    10\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("all catch should run");
        assert_eq!(output.stdout.trim(), "10");
    }

    #[test]
    fn test_run_wasm_finally_runs_after_catch_after_wait_throw() {
        // try/catch/finally: sleep → throw in try, caught (value 100);
        // finally runs for effect (it does not add to the value). The
        // throw happens after the suspension point.
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    sleep(1)\n",
            "    throw 7\n",
            "  catch e\n",
            "    100\n",
            "  finally\n",
            "    5\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("finally-after-catch should run");
        assert_eq!(output.stdout.trim(), "100");
    }

    #[test]
    fn test_run_wasm_finally_runs_after_success_after_wait() {
        // No throw — the try value is 42; finally runs for effect and
        // does not change the value.
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    sleep(1)\n",
            "    42\n",
            "  catch e\n",
            "    100\n",
            "  finally\n",
            "    5\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("finally-after-success should run");
        assert_eq!(output.stdout.trim(), "42");
    }

    #[test]
    fn test_run_wasm_finally_runs_after_catch_after_auto_wait_throw() {
        // The child frame suspends; the parent auto-wait wakes the
        // parent and routes the throw into the parent try/catch (caught,
        // value 100); finally runs for effect.
        let src = concat!(
            "# Throws after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 7\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    let x = child()\n",
            "    x + 1\n",
            "  catch e\n",
            "    100\n",
            "  finally\n",
            "    5\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("finally-after-auto-await catch should run");
        assert_eq!(output.stdout.trim(), "100");
    }

    #[test]
    fn test_run_wasm_finally_runs_after_catch_after_all_throw() {
        // A failing `all` child wakes the waiting parent. The
        // finally runs for effect after the caught error from
        // parent join.
        let src = concat!(
            "# Returns after a slower wait.\n",
            "def slow\n",
            "    @return Int\n",
            "do\n",
            "  sleep(50)\n",
            "  1\n",
            "end\n",
            "\n",
            "# Throws after a faster wait.\n",
            "def fast\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw 9\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  try\n",
            "    let a, b = all(slow(), fast())\n",
            "    a + b\n",
            "  catch e\n",
            "    100\n",
            "  finally\n",
            "    5\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("finally-after-all catch should run");
        assert_eq!(output.stdout.trim(), "100");
    }

    #[test]
    fn test_run_wasm_catches_error_message() {
        // Phase 6 acceptance: `throw Error('msg')` propagates
        // through the auto-wait, the catch body reads `e.message`,
        // and the result is a String. The narrow path bakes the
        // Error dict and message string into the data section.
        let src = concat!(
            "# Throws an Error after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw Error('boom')\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  try\n",
            "    let x = child()\n",
            "    'bad'\n",
            "  catch err\n",
            "    err.message\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("error message catch should run");
        assert_eq!(output.stdout.trim(), "boom");
    }

    #[test]
    fn test_run_wasm_catch_branches_on_error_message() {
        // Catch body branches on e.message == 'boom' and returns
        // 'matched' for the matching throw. Exercises the runtime
        // `e.message` field read, the inline string-equality
        // helper, and the if/else ResultExpr shape.
        let src = concat!(
            "# Throws an Error after waiting.\n",
            "def child\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  throw Error('boom')\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  try\n",
            "    let x = child()\n",
            "    'unreached'\n",
            "  catch err\n",
            "    if err.message == 'boom'\n",
            "      'matched'\n",
            "    else\n",
            "      'other'\n",
            "    end\n",
            "  end\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("branching catch should run");
        assert_eq!(output.stdout.trim(), "matched");
    }

    #[test]
    fn test_run_wasm_minimal_all_waits_with_args() {
        let src = concat!(
            "# Returns x after a slower wait.\n",
            "def slow\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(50)\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "# Returns x after a faster wait.\n",
            "def fast\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(10)\n",
            "  x + 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 10\n",
            "  let a, b = all(slow(base), fast(base))\n",
            "  a + b\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let output = run_wasm_capturing(&wasm).expect("minimal all args should run");
        assert_eq!(output.stdout.trim(), "23");
    }

    #[test]
    fn test_all_child_task_results_are_ordered_by_task_id() {
        let src = concat!(
            "# Returns x after a slower wait.\n",
            "def slow\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(20)\n",
            "  x + 1\n",
            "end\n",
            "\n",
            "# Returns x after a faster wait.\n",
            "def fast\n",
            "    @param x Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  x + 2\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let base = 10\n",
            "  let a, b = all(slow(base), fast(base))\n",
            "  a + b\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let (mut store, instance) = instantiate_for_async_test(&wasm);
        let start_async = instance
            .get_typed_func::<(), i32>(&mut store, "_start_async")
            .expect("missing _start_async");
        let poll = instance
            .get_typed_func::<(), i32>(&mut store, "__fai_poll")
            .expect("missing __fai_poll");
        let task_result = instance
            .get_typed_func::<i32, i64>(&mut store, "__fai_task_result")
            .expect("missing __fai_task_result");

        // Children still running → scheduler not idle yet (status 1).
        assert_eq!(start_async.call(&mut store, ()).unwrap(), 1);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 2).unwrap() as u64),
            nan_box::ReturnKind::Void
        );
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(poll.call(&mut store, ()).unwrap(), 2);
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 1).unwrap() as u64),
            nan_box::ReturnKind::Int(23)
        );
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 2).unwrap() as u64),
            nan_box::ReturnKind::Int(11)
        );
        assert_eq!(
            nan_box::classify_return_value(task_result.call(&mut store, 3).unwrap() as u64),
            nan_box::ReturnKind::Int(12)
        );
    }

    #[test]
    fn test_run_wasm_extern_call_ffi_stub() {
        // Calling an extern function in WASM mode triggers Call(IMPORT_CALL_FFI)
        // The stub returns null; the program should still run without error.
        let src = concat!(
            "extern libc\n",
            "  def strlen(s: String) -> Int\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let n = strlen('hello')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_json_stringify() {
        // json.stringify(val) triggers Call(IMPORT_JSON_STRINGIFY)
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let s = json.stringify(42)\n",
            "  print('done')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_returns_integer_float() {
        // Returns an integer-valued float (e.g. 1.0), which hits the
        // `f == f.floor()` branch in format_float
        let src = "def main\n    @return Float\ndo\n  1.0\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_file_read_existing_file() {
        // Write a file to disk, then read it from WASM — covers read_file success path
        let tmp = "/tmp/fai_wasm_runner_test_read.txt";
        std::fs::write(tmp, "wasm file content").unwrap();

        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let contents = file.read('/tmp/fai_wasm_runner_test_read.txt')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let result = run_wasm(&wasm);
        let _ = std::fs::remove_file(tmp);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_wasm_set_html() {
        // setHtml(content) is a global builtin that emits Call(IMPORT_SET_HTML)
        // In CLI/Wasmtime mode it just prints the HTML to stdout
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  setHtml('<div>hello</div>')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_set_html_at() {
        // setHtmlAt(selector, content) emits Call(IMPORT_SET_HTML_AT)
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  setHtmlAt('#app', '<p>content</p>')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_file_write() {
        // file.write() in WASM mode triggers env.write_file
        let tmp = "/tmp/fai_wasm_runner_test_write.txt";
        let _ = std::fs::remove_file(tmp);

        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  file.write('/tmp/fai_wasm_runner_test_write.txt', 'written from wasm')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_run_wasm_tests_runs_before_all_and_after_all() {
        let tmp = "/tmp/fai_wasm_runner_before_all_after_all.txt";
        let _ = std::fs::remove_file(tmp);

        let src = concat!(
            "use std.file\n",
            "\n",
            "# Temp path.\n",
            "def tempPath\n",
            "    @return String\n",
            "do\n",
            "  '/tmp/fai_wasm_runner_before_all_after_all.txt'\n",
            "end\n",
            "\n",
            "# No-op.\n",
            "def noop\n",
            "    @return Void\n",
            "do\n",
            "end\n",
            "\n",
            "test noop\n",
            "beforeAll\n",
            "  file.write(tempPath(), 'before-all')\n",
            "end\n",
            "it 'sees beforeAll state'\n",
            "  assert.equals(file.read(tempPath()), 'before-all')\n",
            "end\n",
            "afterAll\n",
            "  file.write(tempPath(), 'after-all')\n",
            "end\n",
            "\n",
            "end\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "end\n",
        );
        let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            src,
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let meta = crate::test_meta::extract(&prepared);
        let mut checker = fai_checker::Checker::new();
        checker
            .check_program(&prepared.serde_ast.statements)
            .expect("type check failed");
        let wasm = fai_codegen_wasm::try_codegen_direct_full(
            &prepared.serde_ast,
            &prepared.modules,
            &fai_codegen_wasm::direct::CheckerInfo {
                ufcs_calls: checker.ufcs_calls.clone(),
                named_param_reorder: checker.named_param_reorder.clone(),
                expression_types: checker.expression_types.clone(),
                generic_type_args: checker.generic_type_args.clone(),
            },
            None,
            true,
        )
        .expect("direct codegen refused");
        let summary = run_wasm_tests(&wasm, &meta.suites, |_| {}).expect("test run");
        assert_eq!(summary.failed, 0);
        assert_eq!(
            std::fs::read_to_string(tmp).expect("afterAll should write file"),
            "after-all"
        );
        let _ = std::fs::remove_file(tmp);
    }

    // ── Crafted WASM for run_wasm error paths ────────────────────────────

    /// Minimal valid WASM module importing `foo.bar` which our linker doesn't
    /// provide — triggers the instantiation-error map_err closure.
    fn wasm_with_unknown_import() -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
            // Type section (id=1, size=4): 1 type = () -> ()
            0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            // Import section (id=2, size=11): "foo"."bar" as func type 0
            0x02, 0x0b, 0x01, 0x03, 0x66, 0x6f, 0x6f, // module name "foo" (len=3)
            0x03, 0x62, 0x61, 0x72, // import name "bar" (len=3)
            0x00, 0x00, // func import, type index 0
        ]
    }

    /// Minimal valid WASM with no `_start` export — triggers the
    /// missing-_start map_err closure.
    fn wasm_without_start() -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
            // Type section (id=1, size=5): () -> i64
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7e,
            // Function section (id=3, size=2): 1 function of type 0
            0x03, 0x02, 0x01, 0x00,
            // No export section — _start not exported
            // Code section (id=10, size=6): body = (i64.const 0; end)
            0x0a, 0x06, 0x01, 0x04, 0x00, 0x42, 0x00, 0x0b,
        ]
    }

    /// Minimal valid WASM exporting `_start: () -> i64` that executes
    /// `unreachable` — triggers the execution-error map_err closure.
    fn wasm_with_trapping_start() -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, // magic + version
            // Type section (id=1, size=5): () -> i64
            0x01, 0x05, 0x01, 0x60, 0x00, 0x01, 0x7e,
            // Function section (id=3, size=2): 1 function of type 0
            0x03, 0x02, 0x01, 0x00,
            // Export section (id=7, size=10): func 0 exported as "_start"
            0x07, 0x0a, 0x01, 0x06, 0x5f, 0x73, 0x74, 0x61, 0x72, 0x74, // "_start" (len=6)
            0x00, 0x00, // function export, index 0
            // Code section (id=10, size=5): body = (unreachable; end)
            0x0a, 0x05, 0x01, 0x03, 0x00, 0x00, 0x0b,
        ]
    }

    #[test]
    fn test_run_wasm_instantiation_error() {
        let result = run_wasm(&wasm_with_unknown_import());
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("instantiation") || msg.contains("import") || msg.contains("unknown"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn test_run_wasm_missing_start_export() {
        let result = run_wasm(&wasm_without_start());
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("_start") || msg.contains("export") || msg.contains("missing"),
            "unexpected error: {}",
            msg
        );
    }

    #[test]
    fn test_run_wasm_execution_trap() {
        let result = run_wasm(&wasm_with_trapping_start());
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(
            msg.contains("execution") || msg.contains("trap") || msg.contains("wasm"),
            "unexpected error: {}",
            msg
        );
    }

    // ── file.write error path ────────────────────────────────────────────

    #[test]
    fn test_run_wasm_file_write_invalid_path() {
        // Writing to a path whose parent directory doesn't exist → write_file Err path
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  file.write('/nonexistent_dir_xyz/file.txt', 'content')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        // The write fails silently (returns -1), program continues and prints ok
        assert!(run_wasm(&wasm).is_ok());
    }

    // ── json.parse with array → exercises build_value Array case ────────

    #[test]
    fn test_run_wasm_json_parse_array() {
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let v = json.parse('[1, 2, 3]')\n",
            "  print('ok')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    // ── print_return_value void path ─────────────────────────────────────

    #[test]
    fn test_run_wasm_void_return_value() {
        let src = "def main\n    @return Void\ndo\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
    }

    // ── Phase C: new host imports ───────────────────────────────────────

    #[test]
    fn test_run_wasm_file_exists_true() {
        let tmp = "/tmp/fai_wasm_file_exists_yes.txt";
        std::fs::write(tmp, "x").unwrap();
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/tmp/fai_wasm_file_exists_yes.txt')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let _ = std::fs::remove_file(tmp);
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_file_exists_false() {
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  file.exists('/tmp/fai_does_not_exist_xyz_abc')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "false\n");
    }

    #[test]
    fn test_run_wasm_dict_get_string() {
        // `getString(dict, key)` returns the value under key with no type
        // coercion (VM parity). Return type is optional since a missing
        // key yields null.
        let src = concat!(
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  var user = {name: 'alice', age: 30, admin: true}\n",
            "  getString(user, 'name')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "alice\n");
    }

    #[test]
    fn test_run_wasm_dict_get_int() {
        let src = concat!(
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  var user = {name: 'alice', age: 30, admin: true}\n",
            "  getInt(user, 'age')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "30\n");
    }

    #[test]
    fn test_run_wasm_dict_get_bool() {
        let src = concat!(
            "def main\n",
            "    @return Bool?\n",
            "do\n",
            "  var user = {name: 'alice', flag: true}\n",
            "  getBool(user, 'flag')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_dict_get_missing_key_returns_null() {
        let src = concat!(
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  var user = {name: 'alice'}\n",
            "  getString(user, 'missing')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "null\n");
    }

    #[test]
    fn test_run_wasm_tcp_listen_returns_handle() {
        // End-to-end: open a listener on an ephemeral port and assert we
        // got back a non-negative handle. Proves the full TCP import
        // + socket_registry path is wired and the sentinel dispatch
        // routes `tcp.listen` to its import rather than colliding with
        // server.listen or router.listen.
        let src = concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let h = tcp.listen(0)\n",
            "  tcp.close(h)\n",
            "  h > 0\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_tcp_connect_fails_fast_on_missing_peer() {
        // No server listening — connect should fail fast and return -1
        // (not hang). Proves the sentinel+import path for tcp.connect is
        // wired and the host returns the documented sentinel value.
        let src = concat!(
            "use std.net.tcp\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let h = tcp.connect('127.0.0.1', 1)\n",
            "  h < 0\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_cli_write_line() {
        // cli.writeLine routes through the shared stdout sink (unlike
        // cli.write and cli.clear/moveTo, which go direct to stdout for
        // raw-byte/ANSI fidelity). This keeps captures meaningful for
        // the most common case.
        let src = concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  cli.writeLine('line one')\n",
            "  cli.writeLine('line two')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "line one\nline two\n");
    }

    #[test]
    fn test_run_wasm_cli_coerces_non_string() {
        let src = concat!(
            "use std.cli\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  cli.writeLine(42)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "42\n");
    }

    #[test]
    fn test_run_wasm_http_server_router_serves_text() {
        // End-to-end: spin the wasm server up in a thread, fire a
        // blocking GET from the test's main thread, assert the body
        // matches the handler's response. Proves the full http_server
        // router chain (router / server.get / server.listen) compiles
        // and runs under wasmtime — Phase C router verification.
        //
        // The server thread will block forever on accept(); we leak it
        // on purpose (cargo test terminates it on process exit).
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpStream;
        use std::thread;
        use std::time::Duration;

        // Pick a free port via a probe bind.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let src = format!(
            concat!(
                "use std.http.server\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  let r = server.router()\n",
                "  server.get(r, '/') do with req HttpRequest\n",
                "    server.text(200, 'hello from wasm router')\n",
                "  end\n",
                "  server.listen(r, {})\n",
                "end\n",
            ),
            port
        );
        let wasm = compile_to_wasm(&src);
        thread::spawn(move || {
            let _ = run_wasm(&wasm);
        });

        // Wait for the listener to bind (polls for up to ~1s).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                write!(
                    s,
                    "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let reader = BufReader::new(s);
                let body: String = reader
                    .lines()
                    .map_while(Result::ok)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(
                    body.contains("hello from wasm router"),
                    "router response missing expected body. got:\n{}",
                    body
                );
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("server did not bind within 2s");
            }
            thread::sleep(Duration::from_millis(30));
        }
    }

    #[test]
    fn test_run_wasm_tcp_large_int_literal_round_trip() {
        // Regression for the i32-shadow initialisation bug uncovered
        // while wiring tcp.connect: Int constants routed through
        // LoadConst (literals > i16::MAX) used to leave the i32 shadow
        // at 0, so any downstream `push_boxed` read 0 instead of the
        // real value. Port numbers frequently land above i16::MAX,
        // which is how we hit it.
        let src = concat!(
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  65536\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "65536\n");
    }

    #[test]
    fn test_run_wasm_tcp_connect_only() {
        // Minimal: bind a real listener on the host side, have the guest
        // connect to it, verify the handle is valid, close, done. Isolates
        // a connect-only failure from the readLine blocking.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep the listener alive via the thread; `listener.accept()`
        // returns as soon as our guest's `tcp.connect` fires.
        let accept_thread = std::thread::spawn(move || {
            // Don't unwrap — if the guest never connects (e.g. dispatch
            // bug), the listener drops when the test thread panics and
            // accept() returns Err, which we want to ignore rather than
            // surface as a second panic.
            let _ = listener.accept();
        });

        let src = format!(
            concat!(
                "use std.net.tcp\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  let h = tcp.connect('127.0.0.1', {})\n",
                "  tcp.close(h)\n",
                "  h\n",
                "end\n",
            ),
            port
        );
        let wasm = compile_to_wasm(&src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        accept_thread.join().unwrap();
        let handle: i32 = out.stdout.trim().parse().expect("expected int handle");
        assert!(handle > 0, "expected positive handle, got {}", handle);
    }

    #[test]
    fn test_run_wasm_tcp_roundtrip() {
        // Listener on port 0 + background connect + accept + write + read.
        // The server thread drives accept() so the guest's connect()
        // doesn't block forever; tests should never hang.
        use std::thread;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            use std::io::Write;
            let _ = stream.write_all(b"hello from server\n");
        });

        let src = format!(
            concat!(
                "use std.net.tcp\n",
                "\n",
                "def main\n",
                "    @return String?\n",
                "do\n",
                "  let h = tcp.connect('127.0.0.1', {})\n",
                "  let line = tcp.readLine(h)\n",
                "  tcp.close(h)\n",
                "  line\n",
                "end\n",
            ),
            port
        );
        let wasm = compile_to_wasm(&src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        server.join().unwrap();
        assert_eq!(out.stdout, "hello from server\n\n");
    }

    #[test]
    fn test_run_wasm_tcp_write_to_peer() {
        // Guest writes to the TCP peer; assert the host side receives it.
        use std::io::Read;
        use std::thread;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = stream.read(&mut buf).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let src = format!(
            concat!(
                "use std.net.tcp\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  let h = tcp.connect('127.0.0.1', {})\n",
                "  let n = tcp.write(h, 'guest-to-server')\n",
                "  tcp.close(h)\n",
                "  n\n",
                "end\n",
            ),
            port
        );
        let wasm = compile_to_wasm(&src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let received = server.join().unwrap();
        assert_eq!(out.stdout, "15\n");
        assert_eq!(received, "guest-to-server");
    }

    #[test]
    fn test_run_wasm_udp_bind_returns_handle() {
        // Minimal UDP sanity — bind + broadcast flag + close path.
        // Doesn't exercise the full send/receive roundtrip (that
        // requires two coordinated sockets or guest self-send).
        let src = concat!(
            "use std.net.udp\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let h = udp.bind(0)\n",
            "  udp.broadcast(h, false)\n",
            "  h > 0\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_udp_self_roundtrip() {
        // Guest-side only roundtrip: bind a UDP socket on a known port,
        // send a datagram to 127.0.0.1 on that port, then receive. No
        // paired host thread means no timing flake. We use a port from
        // a pre-bound probe (dropped) to avoid hardcoded-port
        // collisions; the brief window between drop and rebind is
        // acceptable for a local test.
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let src = format!(
            concat!(
                "use std.net.udp\n",
                "\n",
                "def main\n",
                "    @return String?\n",
                "do\n",
                "  let h = udp.bind({p})\n",
                "  udp.send(h, '127.0.0.1', {p}, 'self-udp')\n",
                "  let packet = udp.receive(h)\n",
                "  getString(packet, 'data')\n",
                "end\n",
            ),
            p = port,
        );
        let wasm = compile_to_wasm(&src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "self-udp\n");
    }

    #[test]
    fn test_run_wasm_array_map_doubles() {
        // array.map must invoke the closure per element and collect the
        // returned values into a new Array. Proves the
        // __indirect_function_table invocation + per-closure __env_ptr
        // re-pointing work from the host.
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let nums = [1, 2, 3]\n",
            "  let doubled = array.map(nums, do with n Int\n",
            "    n * 2\n",
            "  end)\n",
            "  array.length(doubled) * 10 + doubled[2]\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        // length=3 * 10 + doubled[2]=6 → 36
        assert_eq!(out.stdout, "36\n");
    }

    #[test]
    fn test_run_wasm_array_filter_keeps_matches() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let nums = [1, 2, 3, 4, 5]\n",
            "  let evens = array.filter(nums, do with n Int\n",
            "    n % 2 == 0\n",
            "  end)\n",
            "  array.length(evens)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "2\n");
    }

    #[test]
    fn test_run_wasm_array_find_returns_first_match() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Int?\n",
            "do\n",
            "  let nums = [1, 2, 3, 4]\n",
            "  array.find(nums, do with n Int\n",
            "    n > 2\n",
            "  end)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "3\n");
    }

    #[test]
    fn test_run_wasm_array_find_no_match_returns_null() {
        // Print via `print` so we go through RT_PRINT_VAL_NEW's null
        // branch explicitly, rather than relying on the `_start` return
        // value classifier (`Int?` return is allowed to legally come
        // back as either Int or VAL_NULL at the wasm level).
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let nums = [1, 2, 3]\n",
            "  let r = array.find(nums, do with n Int\n",
            "    n > 99\n",
            "  end)\n",
            "  print(r)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "null\n");
    }

    #[test]
    fn test_run_wasm_array_is_any_true_when_one_matches() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let nums = [1, 2, 3]\n",
            "  array.isAny(nums, do with n Int\n",
            "    n > 2\n",
            "  end)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_array_is_any_false_when_none_match() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let nums = [1, 2, 3]\n",
            "  array.isAny(nums, do with n Int\n",
            "    n > 99\n",
            "  end)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "false\n");
    }

    #[test]
    fn test_run_wasm_array_is_all_true_when_all_match() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let nums = [2, 4, 6]\n",
            "  array.isAll(nums, do with n Int\n",
            "    n % 2 == 0\n",
            "  end)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_array_is_all_false_when_one_fails() {
        let src = concat!(
            "use std.array\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  let nums = [2, 3, 4]\n",
            "  array.isAll(nums, do with n Int\n",
            "    n % 2 == 0\n",
            "  end)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "false\n");
    }

    #[test]
    fn test_run_wasm_file_list_lists_entries() {
        // Real directory with known entries. We only check membership,
        // not order, since std::fs::read_dir order isn't stable.
        let tmp = std::env::temp_dir().join("fai_wasm_file_list_test");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("a.txt"), "1").unwrap();
        std::fs::write(tmp.join("b.txt"), "2").unwrap();

        let src = format!(
            concat!(
                "use std.file\n",
                "use std.array\n",
                "\n",
                "def main\n",
                "    @return Int\n",
                "do\n",
                "  let entries = file.list('{}')\n",
                "  array.length(entries)\n",
                "end\n",
            ),
            tmp.display()
        );
        let wasm = compile_to_wasm(&src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(out.stdout, "2\n");
    }

    #[test]
    fn test_run_wasm_file_list_missing_dir_returns_empty_array() {
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let entries = file.list('/tmp/fai_definitely_missing_dir_xyz')\n",
            "  length(entries)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "0\n");
    }

    #[test]
    fn test_run_wasm_json_require_string_finds_key() {
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  let d = json.parse('{\"name\": \"Alice\"}')\n",
            "  json.requireString(d, 'name')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "Alice\n");
    }

    #[test]
    fn test_run_wasm_json_require_string_missing_key_returns_null() {
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  let d = json.parse('{\"name\": \"Alice\"}')\n",
            "  json.requireString(d, 'missing')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "null\n");
    }

    #[test]
    fn test_run_wasm_json_require_string_non_string_returns_null() {
        let src = concat!(
            "use std.json\n",
            "\n",
            "def main\n",
            "    @return String?\n",
            "do\n",
            "  let d = json.parse('{\"age\": 30}')\n",
            "  json.requireString(d, 'age')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "null\n");
    }

    #[test]
    fn test_run_wasm_path_basename() {
        let src = concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  path.basename('/tmp/forai-std-platform.json')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "forai-std-platform.json\n");
    }

    #[test]
    fn test_run_wasm_path_dirname() {
        let src = concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  path.dirname('/tmp/foo/bar.txt')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "/tmp/foo\n");
    }

    #[test]
    fn test_run_wasm_path_extname() {
        let src = concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  path.extname('/tmp/foo.json')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, ".json\n");
    }

    #[test]
    fn test_run_wasm_path_join() {
        // path.join("/a", "b.txt") — must not collide with string.join or
        // array.join. Sentinel dispatch routes to IMPORT_PATH_JOIN.
        let src = concat!(
            "use std.path\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  path.join('/a', 'b.txt')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "/a/b.txt\n");
    }

    #[test]
    fn test_run_wasm_html_escape() {
        let src = concat!(
            "use std.html\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  html.escape('<script>alert(\"xss\")</script>')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(
            out.stdout,
            "&lt;script&gt;alert(&quot;xss&quot;)&lt;/script&gt;\n"
        );
    }

    #[test]
    fn test_run_wasm_html_escape_ampersand_and_apostrophe() {
        let src = concat!(
            "use std.html\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  html.escape(\"Tom & Jerry's\")\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "Tom &amp; Jerry&#39;s\n");
    }

    #[test]
    fn test_run_wasm_log_info_prefixes_message() {
        let src = concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  log.info('hello logs')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "[INFO] hello logs\n");
    }

    #[test]
    fn test_run_wasm_log_warn_prefixes_message() {
        let src = concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  log.warn('ruh roh')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "[WARN] ruh roh\n");
    }

    #[test]
    fn test_run_wasm_log_error_prefixes_message() {
        let src = concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  log.error('bad')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "[ERROR] bad\n");
    }

    #[test]
    fn test_run_wasm_log_coerces_non_string_message() {
        // log.info accepts any value; mirrors VM val_to_str coercion.
        let src = concat!(
            "use std.log\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  log.info(42)\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "[INFO] 42\n");
    }

    #[test]
    fn test_run_wasm_net_available_native_is_true() {
        // Native wasmtime — real networking capability is present, so
        // `net.available()` must report true. Proves the sentinel
        // dispatch works for 0-arg module methods too.
        let src = concat!(
            "use std.net\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  net.available()\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "true\n");
    }

    #[test]
    fn test_run_wasm_ffi_available_unknown_library_is_false() {
        let src = concat!(
            "use std.ffi\n",
            "\n",
            "def main\n",
            "    @return Bool\n",
            "do\n",
            "  ffi.available('definitely_not_a_real_library_xyz')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "false\n");
    }

    #[test]
    fn test_run_wasm_http_dispatch_reaches_host() {
        // Sanity: `request.get` should reach the host import (not the
        // dict/server.get dispatch). If it does, our host will hit the
        // file:// branch; since the file doesn't exist, we get VAL_NULL.
        // The assertion is that no trap occurs — just a null response.
        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let resp = request.get('file:///tmp/fai_does_not_exist_xyz_abc_123')\n",
            "  print('dispatched')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "dispatched\n");
    }

    #[test]
    fn test_run_wasm_http_get_file_url() {
        // End-to-end: `request.get(file://...)` returns a Response with
        // `{status, body, headers}` — same shape as native_http_get. Also
        // proves the http.request / http.server dispatch split works; the
        // method name "get" pre-Phase-C would have collided in the flat
        // method-id dispatcher with server.get / dict.get.
        let tmp = "/tmp/fai_wasm_http_get.txt";
        std::fs::write(tmp, "contents for http.get").unwrap();

        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  let resp = request.get('file:///tmp/fai_wasm_http_get.txt')\n",
            "  resp.body\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let _ = std::fs::remove_file(tmp);
        assert_eq!(out.stdout, "contents for http.get\n");
    }

    #[test]
    fn test_run_wasm_http_get_status() {
        let tmp = "/tmp/fai_wasm_http_get_status.txt";
        std::fs::write(tmp, "anything").unwrap();

        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let resp = request.get('file:///tmp/fai_wasm_http_get_status.txt')\n",
            "  resp.status\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let _ = std::fs::remove_file(tmp);
        assert_eq!(out.stdout, "200\n");
    }

    #[test]
    fn test_run_wasm_http_post_file_url_writes() {
        // `request.post(file://path, body)` mirrors native_http_post —
        // writes the body to the file and returns a 200 response.
        let tmp = "/tmp/fai_wasm_http_post.txt";
        let _ = std::fs::remove_file(tmp);

        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let resp = request.post('file:///tmp/fai_wasm_http_post.txt', 'written by http.post')\n",
            "  resp.status\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "200\n");
        let written = std::fs::read_to_string(tmp).expect("file written");
        assert_eq!(written, "written by http.post");
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_run_wasm_http_post_accepts_headers() {
        // Regression: the checker allowed optional request headers, but
        // direct wasm codegen only handled (url, body), causing an
        // arg-count compile error before the host request layer.
        let tmp = "/tmp/fai_wasm_http_post_headers.txt";
        let _ = std::fs::remove_file(tmp);

        let src = concat!(
            "use std.dictionary\n",
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var h = {}\n",
            "  h = dictionary.set(h, 'authorization', 'Bearer test')\n",
            "  let resp = request.post('file:///tmp/fai_wasm_http_post_headers.txt', 'with headers', h)\n",
            "  resp.status\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "200\n");
        assert_eq!(std::fs::read_to_string(tmp).unwrap(), "with headers");
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_run_wasm_http_put_file_url_writes() {
        let tmp = "/tmp/fai_wasm_http_put.txt";
        let _ = std::fs::remove_file(tmp);

        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let resp = request.put('file:///tmp/fai_wasm_http_put.txt', 'put body')\n",
            "  resp.status\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "200\n");
        assert_eq!(std::fs::read_to_string(tmp).unwrap(), "put body");
        let _ = std::fs::remove_file(tmp);
    }

    #[test]
    fn test_run_wasm_http_delete_file_url() {
        let tmp = "/tmp/fai_wasm_http_delete.txt";
        std::fs::write(tmp, "to be deleted").unwrap();

        let src = concat!(
            "use std.http.request\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let resp = request.delete('file:///tmp/fai_wasm_http_delete.txt')\n",
            "  resp.status\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "200\n");
        assert!(
            !std::path::Path::new(tmp).exists(),
            "file should be deleted"
        );
    }

    #[test]
    fn test_run_wasm_trim_start() {
        let src = concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  string.trimStart('  hi there  ')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "hi there  \n");
    }

    #[test]
    fn test_run_wasm_trim_end() {
        let src = concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  string.trimEnd('  hi there  ')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "  hi there\n");
    }

    #[test]
    fn test_run_wasm_trim_unchanged_when_no_whitespace() {
        let src = concat!(
            "use std.string\n",
            "\n",
            "def main\n",
            "    @return String\n",
            "do\n",
            "  string.trimStart('abc')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "abc\n");
    }

    #[test]
    fn test_run_wasm_time_unix_returns_int() {
        // time.unix() must return an Int (seconds since epoch), not a Float.
        // VM parity — pre-Phase-C this returned Float.
        let src = concat!(
            "use std.time\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  time.unix()\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        let trimmed = out.stdout.trim();
        let secs: i32 = trimmed.parse().expect("should parse as i32");
        // Any plausible unix second value works as long as it's an integer.
        assert!(secs > 1_700_000_000, "unexpected time.unix(): {}", trimmed);
    }

    // ── Phase B: engine reuse, capture, error formatting ────────────────

    #[test]
    fn test_shared_engine_is_reused_across_runs() {
        // Two sequential runs hit the same `OnceLock` Engine. The pointer
        // identity check is the assertion — we don't care about Engine's
        // public API, just that we don't construct a fresh one each run.
        let e1 = shared_engine() as *const Engine;
        let src = "def main\n    @return Void\ndo\n  print('a')\nend\n";
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
        let e2 = shared_engine() as *const Engine;
        assert_eq!(e1, e2, "shared_engine returned different instances");
    }

    #[test]
    fn test_run_wasm_capturing_collects_stdout() {
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('captured line one')\n",
            "  print('captured line two')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "captured line one\ncaptured line two\n");
        assert_eq!(out.stderr, "");
    }

    #[test]
    fn test_run_wasm_capturing_captures_return_value_print() {
        // String return values get printed by `print::print_return_value`;
        // capture must include that too (it routes through the sink).
        let src = "def main\n    @return String\ndo\n  'hello-return'\nend\n";
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        assert_eq!(out.stdout, "hello-return\n");
    }

    #[test]
    fn test_run_wasm_capturing_propagates_run_error() {
        // When the run itself fails, `run_wasm_capturing` returns the error
        // rather than a CapturedOutput. Stdout/stderr captured up to the
        // failure point are discarded — callers who need partial capture
        // can build their own guard.
        let result = run_wasm_capturing(b"not valid wasm");
        assert!(result.is_err());
    }

    #[test]
    fn test_trap_error_message_contains_cause_chain() {
        // With `{:#}` formatting the error string should carry wasmtime's
        // cause chain, not just the outermost message.
        let result = run_wasm(&wasm_with_trapping_start());
        let msg = result.expect_err("expected trap");
        // Outer context preserved.
        assert!(msg.starts_with("WASM execution error: "), "msg: {}", msg);
        // And the inner trap reason should be included somewhere.
        assert!(
            msg.to_lowercase().contains("unreachable") || msg.to_lowercase().contains("trap"),
            "error should mention trap cause, got: {}",
            msg
        );
    }
}
