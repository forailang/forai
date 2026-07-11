use crate::*;

/// How a test pass selects which suites to run (plan 135).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TestMode {
    /// `fai run`/start: only suites in files that changed since the last
    /// green run. `fai test` sets this false (full suite + coverage).
    pub incremental: bool,
    /// `fai test --shard=I/N`: run only suites whose index `% N == I-1`
    /// (1-based I). Coverage is enforced only by shard 1.
    pub shard: Option<(usize, usize)>,
    /// Worker count for compile-once parallel `fai test` (plan 135). >1 in the
    /// parent makes it compile once then fan out to shard workers. 1 = serial.
    pub jobs: usize,
    /// Debug bisection (`--suite-window=A:B`): after the normal selection
    /// (shard/full), keep only the selected suites whose 0-based ordinal falls
    /// in `[A, B)`. Lets us binary-search which earlier test corrupts a later
    /// one while keeping the late "oracle" test in the run. Coverage is not
    /// enforced when a window is set (it's a partial run).
    pub window: Option<(usize, usize)>,
}

/// Parse `--suite-window=A:B` (0-based ordinals into the selected-suite list).
pub(crate) fn parse_suite_window(args: &[String]) -> Option<(usize, usize)> {
    for a in args {
        if let Some(spec) = a.strip_prefix("--suite-window=") {
            let (lo, hi) = spec.split_once(':')?;
            let lo: usize = lo.trim().parse().ok()?;
            let hi: usize = hi.trim().parse().ok()?;
            if hi > lo {
                return Some((lo, hi));
            }
        }
    }
    None
}

/// Parse a `--shard=I/N` flag from args. **1-based**: I in `1..=N` (so the
/// last shard of 12 is `12/12`, not `11/12`), N>=1.
pub(crate) fn parse_shard(args: &[String]) -> Option<(usize, usize)> {
    for a in args {
        if let Some(spec) = a.strip_prefix("--shard=") {
            let (i, n) = spec.split_once('/')?;
            let i: usize = i.trim().parse().ok()?;
            let n: usize = n.trim().parse().ok()?;
            if n >= 1 && i >= 1 && i <= n {
                return Some((i, n));
            }
        }
    }
    None
}

// ── Compile-once parallel (plan 135, Option A) ─────────────────────────────
// The parent compiles the test wasm ONCE, then fans out to N worker processes
// that load that prebuilt artifact (+ a serialized suite/externs bundle) and
// run their shard without recompiling. Separate processes → separate wasm
// instances → separate `:memory:` DBs, so isolation is automatic.

/// Serializable mirror of `ExternInfo` (its `FfiType` isn't serde). Encoded as
/// short type tags so the bundle is a plain JSON file.
#[derive(serde::Serialize, serde::Deserialize)]
struct ExternWire {
    library: String,
    function: String,
    params: Vec<String>,
    ret: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PrebuiltBundle {
    suites: Vec<test_meta::TestSuiteMeta>,
    externs: Vec<ExternWire>,
}

fn ffitype_tag(t: &fai_ffi::FfiType) -> &'static str {
    use fai_ffi::FfiType::*;
    match t {
        Int => "int",
        Double => "double",
        String => "string",
        Bool => "bool",
        Pointer => "ptr",
        Void => "void",
        OutPtr => "outptr",
    }
}

fn ffitype_from_tag(s: &str) -> fai_ffi::FfiType {
    use fai_ffi::FfiType::*;
    match s {
        "double" => Double,
        "string" => String,
        "bool" => Bool,
        "ptr" => Pointer,
        "void" => Void,
        "outptr" => OutPtr,
        _ => Int,
    }
}

/// True when args carry a prebuilt bundle (a compile-once worker). Returns the
/// (wasm_path, bundle_path) pair.
pub(crate) fn prebuilt_paths(args: &[String]) -> Option<(String, String)> {
    let mut wasm = None;
    let mut meta = None;
    for a in args {
        if let Some(p) = a.strip_prefix("--prebuilt-wasm=") {
            wasm = Some(p.to_string());
        } else if let Some(p) = a.strip_prefix("--prebuilt-meta=") {
            meta = Some(p.to_string());
        }
    }
    Some((wasm?, meta?))
}

/// Worker: load the prebuilt wasm + bundle and run this shard's suites. No
/// compile, no co-location/coverage (the parent did those once). Returns the
/// process exit code.
pub(crate) fn run_prebuilt_shard(
    wasm_path: &str,
    bundle_path: &str,
    shard: Option<(usize, usize)>,
    opts: &wasm_runner::TestRunOptions,
    reporter: &Reporter,
) -> i32 {
    // This is the parent's precompiled artifact (serialized native code), not
    // raw wasm — the worker deserializes it instead of re-running cranelift.
    let compiled = match std::fs::read(wasm_path) {
        Ok(b) => b,
        Err(e) => {
            reporter.error_line(&format!("prebuilt artifact unreadable: {}", e));
            return 1;
        }
    };
    let bundle: PrebuiltBundle = match std::fs::read_to_string(bundle_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(b) => b,
        None => {
            reporter.error_line("prebuilt bundle unreadable");
            return 1;
        }
    };
    let externs: Vec<wasm_runner::ExternInfo> = bundle
        .externs
        .iter()
        .map(|w| wasm_runner::ExternInfo {
            library: w.library.clone(),
            function: w.function.clone(),
            param_types: w.params.iter().map(|s| ffitype_from_tag(s)).collect(),
            return_type: ffitype_from_tag(&w.ret),
        })
        .collect();
    let suites = bundle.suites;
    let mut run_opts = opts.clone();
    let (si, sn) = shard.unwrap_or((1, 1));
    run_opts.only_suites = Some((0..suites.len()).filter(|k| k % sn == si - 1).collect());
    match run_tests_with_compact_output(
        wasm_runner::TestModule::Precompiled(&compiled),
        &suites,
        externs,
        &run_opts,
        true,
    ) {
        Ok((passed, failed)) => {
            reporter.step(
                if failed > 0 {
                    StepStatus::Fail
                } else {
                    StepStatus::Ok
                },
                "test",
                &format!("{} passed (shard {}/{})", passed, si, sn),
            );
            if failed > 0 {
                1
            } else {
                0
            }
        }
        Err(e) => {
            reporter.error_line(&format!("prebuilt shard run failed: {}", e));
            1
        }
    }
}

/// Parent: the wasm is already compiled. Enforce coverage once, write the
/// prebuilt artifact + bundle to a temp dir, spawn `jobs` workers that load it
/// and run their shard, aggregate, clean up, and exit. Terminal (never
/// returns). `uncovered` is the full-set missing-test-block list.
// Returns normally on success so a multi-target project continues to its
// next target; failures still exit(1) like the sequential path.
fn run_parallel_compiled(
    wasm_bytes: &[u8],
    tests: &[test_meta::TestSuiteMeta],
    uncovered: &[String],
    externs: Vec<wasm_runner::ExternInfo>,
    jobs: usize,
    reporter: &Reporter,
) {
    // Coverage is a whole-set property — enforce it once, here.
    if !uncovered.is_empty() {
        for name in uncovered {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("0 passed, {} failed", uncovered.len()),
        );
        std::process::exit(1);
    }

    // Compile the wasm to native ONCE here (the whole point of compile-once):
    // workers `deserialize` this artifact instead of each re-running cranelift.
    let compiled = match wasm_runner::serialize_test_module(wasm_bytes) {
        Ok(b) => b,
        Err(e) => {
            reporter.error_line(&format!("could not precompile test module: {}", e));
            std::process::exit(1);
        }
    };

    // Stage the precompiled artifact + bundle in a per-process temp dir.
    let dir = std::env::temp_dir().join(format!("fai-parallel-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let artifact_path = dir.join("tests.cwasm");
    let bundle_path = dir.join("bundle.json");
    let bundle = PrebuiltBundle {
        suites: tests.to_vec(),
        externs: externs
            .iter()
            .map(|e| ExternWire {
                library: e.library.clone(),
                function: e.function.clone(),
                params: e.param_types.iter().map(|t| ffitype_tag(t).to_string()).collect(),
                ret: ffitype_tag(&e.return_type).to_string(),
            })
            .collect(),
    };
    let staged = std::fs::write(&artifact_path, &compiled).is_ok()
        && serde_json::to_string(&bundle)
            .ok()
            .map(|s| std::fs::write(&bundle_path, s).is_ok())
            .unwrap_or(false);
    if !staged {
        let _ = std::fs::remove_dir_all(&dir);
        reporter.error_line("could not stage prebuilt artifact for parallel run");
        std::process::exit(1);
    }

    // Fan out: one worker per shard, all concurrent.
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("fai"));
    let mut children = Vec::new();
    for k in 1..=jobs {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("test")
            .arg(format!("--shard={}/{}", k, jobs))
            .arg(format!("--prebuilt-wasm={}", artifact_path.display()))
            .arg(format!("--prebuilt-meta={}", bundle_path.display()))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        match cmd.spawn() {
            Ok(child) => children.push((k, child)),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                reporter.error_line(&format!("shard {}/{}: failed to spawn: {}", k, jobs, e));
                std::process::exit(1);
            }
        }
    }

    // Workers run quiet: only failing suites + a summary line, so a failure is
    // never buried. Collect in shard order.
    let mut total_pass: usize = 0;
    let mut failed_shards: Vec<usize> = Vec::new();
    let mut spawn_lost = false;
    for (k, child) in children {
        match child.wait_with_output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                total_pass += parse_pass_count(&stdout);
                if !out.status.success() {
                    failed_shards.push(k);
                    println!("── shard {}/{} ──", k, jobs);
                    print!("{}", stdout);
                    eprint!("{}", stderr);
                    println!();
                }
            }
            Err(e) => {
                reporter.error_line(&format!("shard {}/{}: {}", k, jobs, e));
                spawn_lost = true;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    // One authoritative final line.
    if failed_shards.is_empty() && !spawn_lost {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed across {} shards", total_pass, jobs),
        );
    } else {
        let mut detail = failed_shards
            .iter()
            .map(|k| format!("{}/{}", k, jobs))
            .collect::<Vec<_>>()
            .join(", ");
        if spawn_lost {
            if !detail.is_empty() {
                detail.push_str("; ");
            }
            detail.push_str("worker error");
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("{} passed; failed shards: {}", total_pass, detail),
        );
        std::process::exit(1);
    }
}

/// Pull the pass count out of a worker's `… — N passed …` summary line.
fn parse_pass_count(output: &str) -> usize {
    for line in output.lines() {
        if let Some(pos) = line.find(" passed") {
            let prefix = &line[..pos];
            if let Some(num) = prefix.rsplit(|c: char| !c.is_ascii_digit()).next() {
                if let Ok(n) = num.parse::<usize>() {
                    return n;
                }
            }
        }
    }
    0
}

pub(crate) fn step_test(args: &[String], reporter: &Reporter) {
    // Called from the run/build pipeline (`fai run`, "start"): incremental —
    // only files that changed since the last green run are retested (plan 135).
    let mode = TestMode {
        incremental: true,
        shard: None,
        jobs: 1,
        window: None,
    };
    run_test_step(args, reporter, &wasm_runner::TestRunOptions::default(), mode);
}

pub(crate) fn step_test_with_opts(
    args: &[String],
    reporter: &Reporter,
    opts: &wasm_runner::TestRunOptions,
    jobs: usize,
) {
    // Called from `fai test`: the full suite + coverage, never incremental.
    // `jobs > 1` compiles once then fans out; `--shard=I/N` runs one shard.
    let mode = TestMode {
        incremental: false,
        shard: parse_shard(args),
        jobs,
        window: parse_suite_window(args),
    };
    run_test_step(args, reporter, opts, mode);
}

fn run_test_step(
    args: &[String],
    reporter: &Reporter,
    opts: &wasm_runner::TestRunOptions,
    mode: TestMode,
) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));

    if let Some(path) = file_arg {
        // Test a single file
        run_tests_file(path, reporter, opts, mode);
        return;
    }

    // Test the project's source tree. Skip if no standard project
    // found (e.g. workspace root).
    let (project_root, src_dir) = match find_project_source_from_cwd() {
        Some(r) => r,
        None => return,
    };
    let toml = std::fs::read_to_string(project_root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);

    if !info.sub_projects.is_empty() {
        // Multi-target project: each `[project.<name>]` has its own
        // entry, target, and dependency wiring. Run them in turn so
        // every target gets its target-specific RPC dispatch and
        // available-import filtering. The shared `src/` tree means
        // many tests run more than once — that's intentional, since
        // platform-specific code (server vs web) compiles differently
        // for each target. Sort by name for deterministic order.
        let mut names: Vec<&String> = info.sub_projects.keys().collect();
        names.sort();
        for name in names {
            let sub = &info.sub_projects[name];
            let Some(main) = &sub.main else { continue };
            let main_path = project_root.join(main);
            if !main_path.exists() {
                continue;
            }
            println!("▶ testing target {}", name);
            run_tests_file(&main_path.to_string_lossy(), reporter, opts, mode);
        }
        return;
    }

    // Flat project (library or single-target app): load the source
    // root as one module and run every test in one wasm pass. Files
    // reference each other through normal module-mate visibility, so
    // extern blocks in `_ffi.fai`, private helpers, and public APIs
    // all resolve regardless of which file declares which.
    let src_path = project_root.join(&src_dir);
    run_tests_module(&src_path, reporter, opts, mode);
}

/// Type-check the prepared program and emit the `check` step (plan 135: the
/// test path is the sole checker for `fai test`, so it reports errors + the
/// `[ok] check` line rather than a separate `step_check` pass re-doing the
/// parse+check). Exits on error, matching `step_check`'s UX.
fn check_or_exit(
    checker: &mut fai_checker::Checker,
    prepared: &fai_compiler::PreparedProgram,
    reporter: &Reporter,
) {
    match run_checker(checker, prepared) {
        Ok(()) => {
            for w in &checker.warnings {
                eprintln!("{}", w);
            }
            reporter.step(StepStatus::Ok, "check", "no type errors");
        }
        Err(e) => {
            reporter.error_line(&format_check_errors(checker, &e));
            let n = checker.collected_errors.len().max(1);
            reporter.step(
                StepStatus::Fail,
                "check",
                &format!("{} type error{}", n, if n == 1 { "" } else { "s" }),
            );
            std::process::exit(1);
        }
    }
}

/// Run every test in a flat library/app source directory as one
/// module. Mirrors the tail of `run_tests_file` but uses
/// `prepare_module_directory_for_tests` so there's no notion of an
/// "entry file" — every `.fai` file in `src_path` contributes its
/// declarations and tests to the same module.
fn run_tests_module(
    src_path: &std::path::Path,
    reporter: &Reporter,
    opts: &wasm_runner::TestRunOptions,
    mode: TestMode,
) {
    let src_path_str = src_path.to_string_lossy().to_string();
    let prepared = match fai_compiler::prepare_module_directory_for_tests(&src_path_str) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&e);
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    let mut checker = fai_checker::Checker::new();
    // Type-check here (plan 135: check once). The old pipeline ran a separate
    // `step_check` first and this pass ignored errors; now this is the sole
    // check for `fai test`, so surface errors + emit the `check` step.
    check_or_exit(&mut checker, &prepared, reporter);
    let info = fai_codegen_wasm::direct::CheckerInfo {
        ufcs_calls: checker.ufcs_calls.clone(),
        named_param_reorder: checker.named_param_reorder.clone(),
        expression_types: checker.expression_types.clone(),
        generic_type_args: checker.generic_type_args.clone(),
        array_int_index_sites: checker.array_int_index_sites.clone(),
        record_field_read_sites: checker.record_field_read_sites.clone(),
    };
    let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned_with_entry_file(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        None,
        true,
        Some(&src_path_str),
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error_line(&format_codegen_error(&e));
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    let meta = test_meta::extract(&prepared);
    let externs = extract_externs_from_prepared(&prepared, Some(&src_path_str));
    finish_test_run(&wasm_bytes, meta, externs, opts, reporter, mode);
}

/// Run tests for a single .fai file, exiting on failure.
/// Fails if any tests fail OR if any named public function lacks a test
/// block. Missing tests are counted as test failures (same `[fail] test`
/// outcome, same exit code) — the scaffold makes coverage mandatory.
/// Run all test suites against the wasm binary and emit compact
/// output: one summary line per suite, then a single "Failed tests"
/// section at the end with each failure's description, source line,
/// and error message.
///
/// Compact form (vs the older per-`it` output) is the difference
/// between ~3 lines per suite and ~1 — significant when an LLM
/// agent is iterating on the project. The Failed Tests section
/// gives the agent enough to grep for the failing case without
/// re-reading every passing case.
///
/// Returns `(passed, failed)` so the caller can build its own
/// step-status summary.
fn run_tests_with_compact_output(
    module_src: wasm_runner::TestModule<'_>,
    tests: &[crate::test_meta::TestSuiteMeta],
    externs: Vec<wasm_runner::ExternInfo>,
    opts: &wasm_runner::TestRunOptions,
    quiet_ok: bool,
) -> Result<(usize, usize), String> {
    let mut current_suite: Option<String> = None;
    let mut suite_pass: u32 = 0;
    let mut suite_fail: u32 = 0;
    let mut failures: Vec<(String, String, u32, String)> = Vec::new();
    // In quiet mode (parallel worker) only print suites that had a failure —
    // otherwise N workers each spew hundreds of ✓ lines and a real failure is
    // lost. The "Failed tests:" section still prints in full.
    let emit_suite = |name: &str, pass: u32, fail: u32| {
        if !quiet_ok || fail > 0 {
            println!("{}", format_suite_line(name, pass, fail));
        }
    };

    let summary =
        wasm_runner::run_wasm_tests_with_externs(module_src, tests, externs, opts, |outcome| {
            if current_suite.as_deref() != Some(&outcome.suite_name) {
                if let Some(name) = current_suite.take() {
                    emit_suite(&name, suite_pass, suite_fail);
                }
                current_suite = Some(outcome.suite_name.clone());
                suite_pass = 0;
                suite_fail = 0;
            }
            match &outcome.error {
                None => suite_pass += 1,
                Some(msg) => {
                    suite_fail += 1;
                    failures.push((
                        outcome.suite_name.clone(),
                        outcome.case_desc.clone(),
                        outcome.suite_line,
                        msg.clone(),
                    ));
                }
            }
        })?;
    if let Some(name) = current_suite.take() {
        emit_suite(&name, suite_pass, suite_fail);
    }
    if !failures.is_empty() {
        println!();
        println!("Failed tests:");
        for (suite, case, line, msg) in &failures {
            println!();
            println!("  ✗ {} — {}", suite, case);
            if *line > 0 {
                println!("    at line {}", line);
            }
            for line_text in msg.lines() {
                println!("    {}", line_text);
            }
        }
    }
    Ok((summary.passed, summary.failed))
}

/// One-line summary for a test suite: pass count, fail count, and a
/// ✓/✗ glyph. `(3 pass)` for clean suites, `(2 pass, 1 fail)` for
/// mixed, `(2 fail)` when nothing passed.
fn format_suite_line(suite: &str, pass: u32, fail: u32) -> String {
    let glyph = if fail == 0 { "✓" } else { "✗" };
    let counts = match (pass, fail) {
        (p, 0) => format!("{} pass", p),
        (0, f) => format!("{} fail", f),
        (p, f) => format!("{} pass, {} fail", p, f),
    };
    format!("  {} {} ({})", glyph, suite, counts)
}

/// Walk the entry file's directory tree (recursively) and collect
/// (has_test_blocks, public_fn_names) across every `.fai` file
/// reachable from it. Used to decide whether the whole target can
/// short-circuit as "no tests here" or needs to run the VM — the
/// entry file alone isn't enough because public functions and test
/// blocks can live in sibling or nested-module files
/// (`pages/`, `components/`, `state/`, etc.).
pub(crate) fn scan_module_for_tests_and_publics(entry_path: &str, entry_raw: &str) -> (bool, Vec<String>) {
    let mut has_tests = false;
    let mut publics: Vec<String> = Vec::new();
    let entry_canonical = std::fs::canonicalize(entry_path).ok();

    // Always include the entry file itself — use its already-read
    // content (which may carry CLI-injected bits like peer hash) rather
    // than re-reading from disk.
    if let Ok(ast) = fai_parser::parse(entry_raw) {
        has_tests = has_tests
            || ast
                .statements
                .iter()
                .any(|s| matches!(s, fai_parser::ast::Statement::Test(_)));
        publics.extend(collect_public_function_names(&ast.statements));
    }

    let parent = std::path::Path::new(entry_path).parent();
    if let Some(dir) = parent {
        for sibling in collect_fai_files_recursive(dir) {
            // Skip the entry we already scanned above. Compare by
            // canonical path so a relative `./main.fai` vs the walker's
            // absolute path still matches.
            let sibling_canonical = std::fs::canonicalize(&sibling).ok();
            if sibling_canonical == entry_canonical {
                continue;
            }
            let content = match std::fs::read_to_string(&sibling) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let ast = match fai_parser::parse(&content) {
                Ok(a) => a,
                Err(_) => continue,
            };
            if !has_tests {
                has_tests = ast
                    .statements
                    .iter()
                    .any(|s| matches!(s, fai_parser::ast::Statement::Test(_)));
            }
            publics.extend(collect_public_function_names(&ast.statements));
        }
    }

    publics.sort();
    publics.dedup();
    (has_tests, publics)
}

/// Recursive version of `collect_fai_files`. Walks `root` and all
/// nested directories, returning every `.fai` file as a sorted
/// absolute-ish path string. A target's source tree spans nested
/// module dirs (`pages/`, `components/`, etc.), so coverage and
/// test-block detection need the full tree, not just siblings.
pub(crate) fn collect_fai_files_recursive(root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("fai") {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn run_tests_file(
    path: &str,
    reporter: &Reporter,
    opts: &wasm_runner::TestRunOptions,
    mode: TestMode,
) {
    let raw_content = read_file(path);
    // If the file has no test blocks, skip the full compile-and-run —
    // complex entry-point files can overflow the VM register file in
    // test mode. We still need to know whether there are public
    // functions to cover: if there are, "no tests" becomes a failure.
    //
    // Scan the *whole module* (all sibling .fai files in the entry's
    // directory), not just the entry file. A target's public functions
    // live across multiple files — e.g. `src/client/main.fai` only has
    // `main`, but `src/client/app.fai` has `def App`. Missing that
    // breadth made the early-return report "no public functions" for a
    // module that actually had uncovered public code.
    let (has_test_blocks, public_fn_names) = scan_module_for_tests_and_publics(path, &raw_content);
    if !has_test_blocks {
        if public_fn_names.is_empty() {
            reporter.step(StepStatus::Ok, "test", "no public functions to test");
            return;
        }
        for name in &public_fn_names {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("0 passed, {} failed", public_fn_names.len()),
        );
        std::process::exit(1);
    }

    // For test compilation: use prepare_source + checker for dependency resolution
    // and UFCS marks, but don't inject the full RPC dispatch (it adds many
    // functions that inflate VM register usage and may cause stack overflows).
    // Tests still compile every function body, including `main`, so server
    // entrypoints need a tiny private addRpcRoutes stub.
    let mut content = raw_content;
    inject_rpc_test_stub(&mut content);
    let source_root = find_source_root(path);
    let synthetic_modules = generate_rpc_proxy_modules(source_root.as_deref());
    // Compile without dispatch injection. Use prepare_source to resolve deps,
    // run the checker for UFCS info, then compile.
    let source_root_str = find_source_root(path);
    // Use the `_for_tests` variant so (a) test blocks in imported
    // modules aren't stripped, and (b) the PreparedProgram flags
    // every module function as in-coverage — the coverage check then
    // demands a test for every function across the whole target tree,
    // not just the entry file.
    let prepared = match fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
        &content,
        source_root_str.as_deref(),
        synthetic_modules,
        Some(path),
    ) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&e);
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    let mut checker = fai_checker::Checker::new();
    // Type-check here (plan 135: check once). The RPC test stub above defines
    // `addRpcRoutes`, so this checks the real user code; it is now the sole
    // check for `fai test` (no separate `step_check` pass).
    check_or_exit(&mut checker, &prepared, reporter);
    // Direct AST→wasm is the only codegen path used for tests.
    let info = fai_codegen_wasm::direct::CheckerInfo {
        ufcs_calls: checker.ufcs_calls.clone(),
        named_param_reorder: checker.named_param_reorder.clone(),
        expression_types: checker.expression_types.clone(),
        generic_type_args: checker.generic_type_args.clone(),
        array_int_index_sites: checker.array_int_index_sites.clone(),
        record_field_read_sites: checker.record_field_read_sites.clone(),
    };
    let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned_with_entry_file(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        None,
        true,
        Some(path),
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error_line(&format_codegen_error(&e));
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    // Test + coverage metadata walks the AST directly. Annotate entry
    // statements with `path` so incremental selection knows their file.
    let meta = test_meta::extract_with_entry(&prepared, Some(path));
    let externs = extract_externs_from_prepared(&prepared, source_root.as_deref());
    finish_test_run(&wasm_bytes, meta, externs, opts, reporter, mode);
}

/// Shared post-compile stage for both the flat-module and entry-file test
/// paths: enforce co-location, decide which suites to run (all, or — in
/// incremental mode — only those in files that changed since the last green
/// run), run them, report, and update the incremental cache (plan 135).
fn finish_test_run(
    wasm_bytes: &[u8],
    meta: test_meta::TestMeta,
    externs: Vec<wasm_runner::ExternInfo>,
    opts: &wasm_runner::TestRunOptions,
    reporter: &Reporter,
    mode: TestMode,
) {
    let tests = meta.suites.clone();
    let candidates: Vec<test_meta::CoverageCandidate> = meta
        .coverage_candidates
        .iter()
        .filter(|c| {
            !c.name.is_empty()
                && !c.name.starts_with('<')
                && c.name != "main"
                && !c.name.starts_with("__")
        })
        .cloned()
        .collect();
    let suite_names: std::collections::HashSet<String> =
        tests.iter().map(|t| t.suite_name.clone()).collect();

    // --- Co-location enforcement (plan 135 rule 1) ------------------------
    // A def's test must live in the def's file, so file-level dirtying is
    // sound. Violation: a def D has a suite named D, but none of the suites
    // named D are in D's file (the test lives elsewhere). Matching by file —
    // not just name — keeps same-named suites in different modules valid.
    // Under sharding only shard 1 checks (every shard has the full meta, so
    // running it in all shards would just duplicate the same error).
    if mode.shard.map_or(true, |(i, _)| i == 1) {
        use std::collections::HashMap;
        let mut suite_files: HashMap<&str, std::collections::HashSet<String>> = HashMap::new();
        for s in &tests {
            if let Some(sf) = &s.file {
                suite_files
                    .entry(s.suite_name.as_str())
                    .or_default()
                    .insert(test_cache::canon(sf));
            }
        }
        let mut violations: Vec<(String, String)> = Vec::new();
        for c in &candidates {
            let Some(cf) = &c.file else { continue };
            if let Some(files) = suite_files.get(c.name.as_str()) {
                if !files.contains(&test_cache::canon(cf)) {
                    violations.push((c.name.clone(), cf.clone()));
                }
            }
        }
        if !violations.is_empty() {
            for (name, file) in &violations {
                reporter.error_line(&format!(
                    "test '{}' is not co-located with its def (declared in {}); \
                     move the `test {}` block into that file (plan 135 rule 1)",
                    name, file, name
                ));
            }
            reporter.step(StepStatus::Fail, "test", "tests must be co-located");
            std::process::exit(1);
        }
    }

    // Compile-once parallel parent (plan 135): the wasm is already built, so
    // fan out to shard workers that load it instead of recompiling. Skipped
    // when leak/ownership instrumentation is on — those are serial diagnostics
    // and the prebuilt wasm carries no ledger hooks. Terminal (exits).
    if mode.jobs > 1
        && mode.shard.is_none()
        && mode.window.is_none()
        && !opts.check_leaks
        && !opts.check_ownership
    {
        let uncovered_full: Vec<String> = candidates
            .iter()
            .filter(|c| !suite_names.contains(&c.name))
            .map(|c| c.name.clone())
            .collect();
        run_parallel_compiled(wasm_bytes, &tests, &uncovered_full, externs, mode.jobs, reporter);
        return;
    }

    // Project root + full source file list, for the incremental cache.
    let project = find_project_source_from_cwd();
    let all_files: Vec<String> = match &project {
        Some((root, src_dir)) => collect_fai_files_recursive(&root.join(src_dir)),
        None => Vec::new(),
    };

    // --- Decide which suites run + which coverage to enforce --------------
    let mut run_opts = opts.clone();
    let mut uncovered: Vec<String>;
    let mut dirty_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let use_cache = mode.incremental && project.is_some() && mode.shard.is_none();

    if let Some((shard_i, shard_n)) = mode.shard {
        // Shard: run only suites whose index falls in this shard (1-based
        // shard_i, so index % N == shard_i - 1). Coverage (missing-test-block)
        // is a whole-set property, so only shard 1 enforces it — the others
        // would double-report. No cache in shard mode.
        let only: std::collections::HashSet<usize> = (0..tests.len())
            .filter(|i| i % shard_n == shard_i - 1)
            .collect();
        uncovered = if shard_i == 1 {
            candidates
                .iter()
                .filter(|c| !suite_names.contains(&c.name))
                .map(|c| c.name.clone())
                .collect()
        } else {
            Vec::new()
        };
        run_opts.only_suites = Some(only);
    } else if use_cache {
        let root = &project.as_ref().unwrap().0;
        let cache = test_cache::load(root);
        // Canonical set of source files that changed since their last pass.
        dirty_files = all_files
            .iter()
            .filter(|f| test_cache::is_dirty(&cache, f))
            .map(|f| test_cache::canon(f))
            .collect();
        let is_dirty_file = |file: &Option<String>| match file {
            Some(f) => dirty_files.contains(&test_cache::canon(f)),
            None => true, // no known origin → always retest (conservative)
        };
        // Run only suites in dirty files.
        let only: std::collections::HashSet<usize> = tests
            .iter()
            .enumerate()
            .filter(|(_, s)| is_dirty_file(&s.file))
            .map(|(i, _)| i)
            .collect();
        // Coverage in incremental mode = only the "missing test block"
        // presence check, and only for defs in dirty files (the delta the
        // agent just wrote). No whole-suite 100% coverage — that's `fai test`.
        uncovered = candidates
            .iter()
            .filter(|c| !suite_names.contains(&c.name) && is_dirty_file(&c.file))
            .map(|c| c.name.clone())
            .collect();
        if only.is_empty() && uncovered.is_empty() {
            reporter.step(StepStatus::Ok, "test", "up to date (no changed files)");
            return;
        }
        run_opts.only_suites = Some(only);
    } else {
        // Full run: every suite, full coverage.
        uncovered = candidates
            .iter()
            .filter(|c| !suite_names.contains(&c.name))
            .map(|c| c.name.clone())
            .collect();
        run_opts.only_suites = None;
    }

    // Debug bisection window (plan 135): keep only the [A, B) ordinal slice of
    // the currently-selected suites. Coverage isn't meaningful on a partial
    // run, so drop the uncovered gate when windowing.
    if let Some((a, b)) = mode.window {
        let mut base: Vec<usize> = match &run_opts.only_suites {
            Some(set) => set.iter().cloned().collect(),
            None => (0..tests.len()).collect(),
        };
        base.sort_unstable();
        let windowed: std::collections::HashSet<usize> = base
            .into_iter()
            .enumerate()
            .filter(|(ord, _)| *ord >= a && *ord < b)
            .map(|(_, i)| i)
            .collect();
        eprintln!(
            "[suite-window] running {} suite(s), ordinals [{}, {})",
            windowed.len(),
            a,
            b
        );
        run_opts.only_suites = Some(windowed);
        uncovered = Vec::new();
    }

    let (passed, failed) =
        match run_tests_with_compact_output(
            wasm_runner::TestModule::Wasm(wasm_bytes),
            &tests,
            externs,
            &run_opts,
            mode.shard.is_some(),
        ) {
            Ok(p) => p,
            Err(e) => {
                reporter.error_line(&format!("runtime error during test setup: {}", e));
                reporter.step(StepStatus::Fail, "test", "setup failed");
                std::process::exit(1);
            }
        };

    let missing = uncovered.len();
    let combined_failures = failed + missing;
    if combined_failures > 0 {
        for name in &uncovered {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("{} passed, {} failed", passed, combined_failures),
        );
        std::process::exit(1);
    }

    // Green run → update the cache. Never in shard mode: a shard ran only a
    // subset, so it can't declare any file fully passed.
    if mode.shard.is_none() {
        if let Some((root, _)) = &project {
            let mut cache = test_cache::load(root);
            if use_cache {
                // Mark every file that was dirty this run as passing.
                test_cache::mark_passed(&mut cache, &dirty_files);
            } else {
                // Full run passed → the whole tree is green.
                let all: std::collections::HashSet<String> = all_files.iter().cloned().collect();
                test_cache::mark_passed(&mut cache, &all);
            }
            test_cache::save(root, &cache);
        }
    }

    if let Some((i, n)) = mode.shard {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed (shard {}/{})", passed, i, n),
        );
    } else if use_cache {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed ({} changed file(s))", passed, dirty_files.len()),
        );
    } else if passed == 0 {
        reporter.step(StepStatus::Ok, "test", "no public functions to test");
    } else {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed, coverage 100%", passed),
        );
    }
}

pub(crate) fn inject_rpc_test_stub(content: &mut String) {
    if !content.contains("addRpcRoutes") || content.contains("def addRpcRoutes") {
        return;
    }
    content.push_str(
        "\nprivate:\n# Test-mode RPC route stub.\ndef addRpcRoutes\n    @param router Router\n    @return Void\ndo\nend\n",
    );
}

/// Collect public, named function declarations from a file's top-level
/// statements. Used to enforce the "every public function needs a test"
/// rule at AST level — when the file has no `test` blocks at all, we
/// can't run the VM coverage check, but we still want to fail the
/// build rather than silently letting untested public functions slip
/// through with "no tests found".
///
/// Skips: `main` (the program entry), anonymous functions (name starts
/// with `<`), functions marked `private`, and functions in a sticky
/// `private:` section (detected by the `is_private` flag on each decl).
fn collect_public_function_names(statements: &[fai_parser::ast::Statement]) -> Vec<String> {
    // Name is historical — this now collects **every** user-defined
    // function (public and private alike) except `main` and compiler-
    // synthetic names. The test policy is "every function requires a
    // test"; `private` is not a way to opt out. `main` is the single
    // explicit exception — it's the program's entry, not a unit of
    // testable behaviour.
    let mut names = Vec::new();
    for stmt in statements {
        if let fai_parser::ast::Statement::Function(fd) = stmt {
            if fd.name == "main" || fd.name.starts_with('<') || fd.name.starts_with("__") {
                continue;
            }
            names.push(fd.name.clone());
        }
    }
    names
}

#[cfg(test)]
mod plan135_tests {
    use super::parse_shard;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_shard_reads_i_over_n_one_based() {
        assert_eq!(parse_shard(&v(&["--shard=1/4"])), Some((1, 4)));
        assert_eq!(parse_shard(&v(&["src/main.fai", "--shard=3/8"])), Some((3, 8)));
        // The last shard of N is N/N (1-based), not (N-1)/N.
        assert_eq!(parse_shard(&v(&["--shard=4/4"])), Some((4, 4)));
    }

    #[test]
    fn parse_shard_absent_or_invalid_is_none() {
        assert_eq!(parse_shard(&v(&["--check-leaks"])), None);
        // i is 1-based: 0 is invalid.
        assert_eq!(parse_shard(&v(&["--shard=0/4"])), None);
        // i must be <= n.
        assert_eq!(parse_shard(&v(&["--shard=5/4"])), None);
        // n must be >= 1.
        assert_eq!(parse_shard(&v(&["--shard=0/0"])), None);
        // malformed.
        assert_eq!(parse_shard(&v(&["--shard=abc"])), None);
    }
}
