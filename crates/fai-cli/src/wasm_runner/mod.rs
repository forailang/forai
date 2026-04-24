//! Run compiled WASM modules via embedded Wasmtime JIT.
//!
//! The module under test must export `_start() -> i64` and `memory`.
//! Host functions are grouped into submodules under `host/`.
//!
//! A single process-wide `wasmtime::Engine` is shared via `OnceLock`.
//! Creating an Engine compiles JIT dispatch tables and is expensive; reusing
//! one across runs cuts per-run cost to roughly module compilation time.

use std::sync::OnceLock;

use wasmtime::*;

mod heap;
mod host;
mod nan_box;
pub mod output;
mod print;

pub use fai_ffi::FfiType;
pub use host::util::{ExternGuard, ExternInfo};

/// Shared wasmtime engine. Cheap to clone but expensive to construct; build
/// once per process.
fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(Engine::default)
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
pub fn run_wasm(wasm_bytes: &[u8]) -> Result<(), String> {
    run_wasm_with_externs(wasm_bytes, Vec::new())
}

/// Same as [`run_wasm`], but populates the extern-function table the
/// host's `call_ffi` import reads. Pass an empty `Vec` when the guest
/// has no `extern` blocks. Guard is scoped to this call — the next
/// run starts with a fresh table.
pub fn run_wasm_with_externs(wasm_bytes: &[u8], externs: Vec<ExternInfo>) -> Result<(), String> {
    let _extern_guard = ExternGuard::set(externs);
    let engine = shared_engine();
    let module = Module::new(engine, wasm_bytes).map_err(|e| fmt_err("WASM load error", e))?;
    let mut store = Store::new(engine, ());
    let mut linker = Linker::new(engine);

    host::install_all(&mut linker)?;

    let instance = linker
        .instantiate(&mut store, &module)
        .map_err(|e| fmt_err("WASM instantiation error", e))?;

    let start = instance
        .get_typed_func::<(), i64>(&mut store, "_start")
        .map_err(|e| fmt_err("missing _start export", e))?;

    let result = start
        .call(&mut store, ())
        .map_err(|e| fmt_err("WASM execution error", e))?;

    print::print_return_value(result, &instance, &mut store);

    Ok(())
}

/// Output captured from a single [`run_wasm_capturing`] invocation.
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
pub struct SuiteReport {
    pub suite_name: String,
    pub cases: Vec<CaseReport>,
}

#[derive(Debug, Clone)]
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
pub fn run_wasm_tests(
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    mut on_case: impl FnMut(&CaseOutcome),
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
                        suite_name: test.suite_name.clone(),
                        case_desc: desc.clone(),
                        error: None,
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
                        suite_name: test.suite_name.clone(),
                        case_desc: desc.clone(),
                        error: Some(msg),
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
        // Exercises env.run_all via the direct path's synthesized
        // `<all-task>` closures. Each call arg is wrapped in a
        // zero-arg closure, their pointers land in a scratch buffer,
        // and `run_all(buf, count)` returns a NaN-boxed tuple.
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
        assert!(run_wasm(&wasm).is_ok());
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
        // Exercises env.spawn via `nowait`
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
        assert!(run_wasm(&wasm).is_ok());
    }

    #[test]
    fn test_run_wasm_sleep_builtin() {
        // sleep(ms) is a global builtin that emits Call(IMPORT_SLEEP_MS) in WASM mode
        let src = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  sleep(0)\n",
            "  print('done')\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        assert!(run_wasm(&wasm).is_ok());
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
    fn test_run_wasm_file_list_missing_dir_returns_null() {
        let src = concat!(
            "use std.file\n",
            "\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  let entries = file.list('/tmp/fai_definitely_missing_dir_xyz')\n",
            "  0\n",
            "end\n",
        );
        let wasm = compile_to_wasm(src);
        let out = run_wasm_capturing(&wasm).expect("run failed");
        // Just checking we don't trap — file.list returns null and the
        // unused binding is dropped.
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
