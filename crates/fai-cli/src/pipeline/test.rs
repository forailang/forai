use crate::*;

pub(crate) fn step_test(args: &[String], reporter: &Reporter) {
    step_test_with_opts(args, reporter, &wasm_runner::TestRunOptions::default());
}

pub(crate) fn step_test_with_opts(args: &[String], reporter: &Reporter, opts: &wasm_runner::TestRunOptions) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));

    if let Some(path) = file_arg {
        // Test a single file
        run_tests_file(path, reporter, opts);
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
            run_tests_file(&main_path.to_string_lossy(), reporter, opts);
        }
        return;
    }

    // Flat project (library or single-target app): load the source
    // root as one module and run every test in one wasm pass. Files
    // reference each other through normal module-mate visibility, so
    // extern blocks in `_ffi.fai`, private helpers, and public APIs
    // all resolve regardless of which file declares which.
    let src_path = project_root.join(&src_dir);
    run_tests_module(&src_path, reporter, opts);
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
    // Checker errors are surfaced by the earlier `step_check` pass —
    // here we only need the UFCS / named-param maps to feed codegen.
    let _ = run_checker(&mut checker, &prepared);
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
    let tests = meta.suites;
    let public_fns: Vec<String> = meta
        .coverage_candidates
        .into_iter()
        .filter(|n| !n.is_empty() && !n.starts_with('<') && n != "main" && !n.starts_with("__"))
        .collect();
    let suite_names: std::collections::HashSet<String> =
        tests.iter().map(|t| t.suite_name.clone()).collect();
    let uncovered: Vec<String> = public_fns
        .into_iter()
        .filter(|n| !suite_names.contains(n))
        .collect();

    let externs = extract_externs_from_prepared(&prepared, Some(&src_path_str));
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs, opts) {
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
    if passed == 0 {
        reporter.step(StepStatus::Ok, "test", "no public functions to test");
    } else {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed, coverage 100%", passed),
        );
    }
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
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    externs: Vec<wasm_runner::ExternInfo>,
    opts: &wasm_runner::TestRunOptions,
) -> Result<(usize, usize), String> {
    let mut current_suite: Option<String> = None;
    let mut suite_pass: u32 = 0;
    let mut suite_fail: u32 = 0;
    let mut failures: Vec<(String, String, u32, String)> = Vec::new();

    let summary =
        wasm_runner::run_wasm_tests_with_externs(wasm_bytes, tests, externs, opts, |outcome| {
            if current_suite.as_deref() != Some(&outcome.suite_name) {
                if let Some(name) = current_suite.take() {
                    println!("{}", format_suite_line(&name, suite_pass, suite_fail));
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
        println!("{}", format_suite_line(&name, suite_pass, suite_fail));
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

fn run_tests_file(path: &str, reporter: &Reporter, opts: &wasm_runner::TestRunOptions) {
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
    // If the checker fails (e.g. addRpcRoutes undefined without dispatch),
    // fall back to compiling with empty checker info — tests run regardless.
    let _ = run_checker(&mut checker, &prepared);
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
    // Test + coverage metadata walks the AST directly.
    let meta = test_meta::extract(&prepared);
    let tests = meta.suites;
    let public_fns: Vec<String> = meta
        .coverage_candidates
        .into_iter()
        .filter(|n| !n.is_empty() && !n.starts_with('<') && n != "main" && !n.starts_with("__"))
        .collect();
    let suite_names: std::collections::HashSet<String> =
        tests.iter().map(|t| t.suite_name.clone()).collect();
    let uncovered: Vec<String> = public_fns
        .into_iter()
        .filter(|n| !suite_names.contains(n))
        .collect();

    let externs = extract_externs_from_prepared(&prepared, source_root.as_deref());
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs, opts) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&format!("runtime error during test setup: {}", e));
            reporter.step(StepStatus::Fail, "test", "setup failed");
            std::process::exit(1);
        }
    };

    // Combine assertion failures and missing-test failures into a
    // single failure count so the summary matches the scaffold's
    // promise — every public function must be tested, with no
    // separate "coverage" category that could be misread as advisory.
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
    if passed == 0 {
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
