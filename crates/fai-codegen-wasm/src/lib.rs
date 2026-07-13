//! FAI AST → WebAssembly code generator.
//!
//! Compiles a parsed `Program` directly to a `.wasm` binary via the
//! `direct` module. All FAI values are represented as NaN-boxed i64
//! in WASM.
//!
//! Phase H of Plan 94 deleted the older bytecode→wasm path
//! (`module.rs` / `translate.rs`) along with its `codegen` /
//! `codegen_all` entry points. `try_codegen_direct_full` is the
//! only production path.

pub mod async_analysis;
mod async_codegen;
pub mod async_engine;
pub mod async_runtime;
pub mod debug_info;
pub mod direct;
mod program;
pub mod test_surface;
mod runtime;

// Trap-report codes (plan 116): shared with the CLI runner, which
// renders them into readable trap reasons.
pub use runtime::{
    HOST_OP_ECHO_BOXED, HOST_OP_ENV_LOAD, HOST_OP_FILE_LIST, HOST_OP_FILE_READ, HOST_OP_FILE_WRITE,
    HOST_OP_HTTP_DELETE, HOST_OP_HTTP_GET, HOST_OP_HTTP_PATCH, HOST_OP_HTTP_POST, HOST_OP_HTTP_PUT,
    HOST_OP_PROCESS_RUN, HOST_OP_PROCESS_WRITE, HOST_OP_TCP_ACCEPT, HOST_OP_TCP_CONNECT,
    HOST_OP_TCP_READ,
    HOST_OP_TCP_READ_LINE, HOST_OP_UDP_RECEIVE,
};
pub use runtime::{
    TRAP_ALLOC_TOO_BIG, TRAP_DICT_CAP_INSANE, TRAP_DOUBLE_FREE, TRAP_FORCE_UNWRAP_NULL,
    TRAP_FREED_DIRTY, TRAP_FREELIST_CORRUPT, TRAP_INDEX_OOB, TRAP_OOM, TRAP_RC_OVER_RELEASE,
    TRAP_RC_RELEASE_POISON, TRAP_RC_RETAIN_POISON, TRAP_SCHED_STALL, TRAP_TASK_OVERFLOW,
    TRAP_TASK_RESULT_NOT_DONE, TRAP_TASK_SLOT_REUSED, TRAP_UNCAUGHT_ERROR,
};
// Check-leaks codegen gate (plan 116 phase 5): the CLI flips this
// before compiling a `--check-leaks` run; tests use the guard.
pub use runtime::{check_leaks_enabled, set_check_leaks, CheckLeaksGuard};
pub use runtime::{
    debug_function_calls_enabled, set_debug_function_calls, DebugFunctionCallsGuard,
};
pub use runtime::{ownership_check_enabled, set_ownership_check, OwnershipCheckGuard};
// Checked-mode codegen gate (plan 116): `fai test --checked` flips this
// before compiling to enable the cheap alloc-guard + index-store guards.
pub use runtime::{checked_enabled, set_checked};

/// Try compiling `ast` through the direct AST→wasm builder in one
/// shot. Returns `Some(wasm)` on success; `None` is an internal
/// error (a construct the direct path can't handle) — callers
/// should surface it rather than silently swallow the refusal.
///
/// `target` controls which host imports the module declares —
/// `None` for native, `Some("wasm-html")` / `Some("wasm")` for
/// browser or headless builds that disable server-side imports.
///
/// Equivalent to [`try_codegen_direct_with_modules`] with no user
/// modules. Callers driving single-file programs can use this form.
pub fn try_codegen_direct(
    ast: &fai_compiler::ast::Program,
    checker: &direct::CheckerInfo,
    target: Option<&str>,
) -> Option<Vec<u8>> {
    try_codegen_direct_with_modules(ast, &[], checker, target)
}

/// Try compiling an AST plus its discovered sibling modules
/// through the direct AST→wasm builder. Each module's top-level
/// functions are included with canonical-prefixed names so
/// cross-module calls resolve. Returns `None` on any refusal.
pub fn try_codegen_direct_with_modules(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &direct::CheckerInfo,
    target: Option<&str>,
) -> Option<Vec<u8>> {
    try_codegen_direct_full(ast, modules, checker, target, false)
}

/// Full-feature direct-path entry that also accepts `is_test` —
/// when true, each `TestDeclaration` in the entry AST or modules
/// becomes a wasm function and the emitted module exports
/// `_fai_run_test(suite_i: i32, case_i: i32) -> ()` for the CLI
/// test runner to drive.
pub fn try_codegen_direct_full(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &direct::CheckerInfo,
    target: Option<&str>,
    is_test: bool,
) -> Option<Vec<u8>> {
    codegen_direct_full_reasoned(ast, modules, checker, target, is_test).ok()
}

/// Codegen refusal carrying both the underlying [`direct::BuildError`]
/// and a best-effort source location. The CLI groups these by file
/// the same way it groups check errors (plan #38), so an agent
/// staring at a refusal sees `src/foo.fai:42` instead of an opaque
/// `UnknownIdentifier(...)`.
///
/// `file`/`line`/`col` may be `None` when the error originated from
/// a code path that doesn't (yet) thread location through —
/// callers should treat them as best-effort. Plan 108 #1 will fill
/// in more of these incrementally.
#[derive(Debug)]
pub struct LocatedBuildError {
    pub err: direct::BuildError,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    /// Qualified module name the offending statement belonged to —
    /// e.g. `"Forui.signal"`, `"data.posts"`. The CLI formatter uses
    /// this to flag errors from external packages (uppercase-first
    /// segment) as "fix this upstream" rather than as a user-fixable
    /// bug in their project.
    pub module: Option<String>,
}

impl LocatedBuildError {
    /// Wrap a `BuildError` with no location attached. Used as a fallback
    /// when the codegen path that surfaced the error doesn't yet thread
    /// source location.
    pub fn unlocated(err: direct::BuildError) -> Self {
        Self {
            err,
            file: None,
            line: None,
            col: None,
            module: None,
        }
    }

    /// `true` when this error came from an external package — by the
    /// project convention, packages start with an uppercase letter
    /// (`Forui`, `Forsqlite`, …) while user modules start lowercase
    /// (`data.posts`, `auth`, …).
    pub fn is_external_package(&self) -> bool {
        self.module
            .as_deref()
            .and_then(|m| m.split('.').next())
            .and_then(|root| root.chars().next())
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
    }

    /// Top-level package name extracted from `module` — `"Forui.signal"`
    /// → `"Forui"`. Used by the formatter when displaying external-
    /// package errors.
    pub fn package_root(&self) -> Option<&str> {
        self.module.as_deref().and_then(|m| m.split('.').next())
    }
}

impl From<direct::BuildError> for LocatedBuildError {
    fn from(err: direct::BuildError) -> Self {
        Self::unlocated(err)
    }
}

impl std::fmt::Display for LocatedBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.err)?;
        if let (Some(file), Some(line)) = (&self.file, self.line) {
            match self.col {
                Some(col) => write!(f, " at {}:{}:{}", file, line, col)?,
                None => write!(f, " at {}:{}", file, line)?,
            }
        } else if let Some(line) = self.line {
            write!(f, " (line {})", line)?;
        }
        Ok(())
    }
}

/// Same as [`try_codegen_direct_full`] but surfaces the underlying
/// `BuildError` on refusal so callers can render a diagnostic naming
/// the offending construct. The CLI uses this to turn the generic
/// "codegen refused" message into something actionable.
pub fn codegen_direct_full_reasoned(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &direct::CheckerInfo,
    target: Option<&str>,
    is_test: bool,
) -> Result<Vec<u8>, LocatedBuildError> {
    codegen_direct_full_reasoned_with_entry_file(ast, modules, checker, target, is_test, None)
}

/// Same as [`codegen_direct_full_reasoned`] but also records
/// `entry_file` (the entry source path) in the debug side-table so
/// trap backtraces show `main (main.fai:3)` rather than `main (line 3)`
/// for entry-AST functions. Plan 116.
pub fn codegen_direct_full_reasoned_with_entry_file(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    checker: &direct::CheckerInfo,
    target: Option<&str>,
    is_test: bool,
    entry_file: Option<&str>,
) -> Result<Vec<u8>, LocatedBuildError> {
    let async_analysis =
        async_analysis::analyze_with_ufcs(ast, modules, &[], &checker.ufcs_calls);
    // Real async engine (R2+): handles the shapes it supports through the
    // guest scheduler + resumable lowering; unsupported shapes return None
    // and fall through.
    let mut async_engine_error = None;
    if is_test {
        // Test builds run on the same engine as production (plan 103 U6/U8):
        // each (suite, case) becomes a wrapper function injected before
        // analysis, spawned by the runner via `_fai_spawn_test` and driven on
        // the guest scheduler. Shapes the engine can't lower fall through
        // silently to the legacy synchronous test path below — losing
        // engine-scheduling for that file, never the test itself
        // (FAI_ASYNC_DEBUG surfaces the reason).
        let (ast_t, modules_t, plans) = test_surface::inject_test_wrappers(ast, modules);
        let roots: Vec<String> = plans
            .iter()
            .map(|p| match &p.module {
                Some(m) => format!("{}.{}", m, p.fn_name),
                None => p.fn_name.clone(),
            })
            .collect();
        let analysis_t =
            async_analysis::analyze_with_ufcs(&ast_t, &modules_t, &roots, &checker.ufcs_calls);
        if let Some(wasm) = direct::try_codegen_async_engine(
            &ast_t,
            &modules_t,
            checker,
            target,
            &analysis_t,
            entry_file,
            Some(&plans),
        ) {
            return Ok(wasm);
        }
        let _ = direct::take_last_async_engine_error();
    } else {
        if let Some(wasm) = direct::try_codegen_async_engine(
            ast,
            modules,
            checker,
            target,
            &async_analysis,
            entry_file,
            None,
        ) {
            return Ok(wasm);
        }
        async_engine_error = direct::take_last_async_engine_error();
    }
    if let Some(err) = async_codegen::async_unsupported_error(&async_analysis, is_test) {
        return Err(async_engine_error.unwrap_or(err));
    }

    let rt = direct::RtOffsets {
        base: direct::direct_rt_base_for_target_with_test_flag(target, is_test),
    };
    let type_indices = direct::direct_fai_func_type_indices();
    let import_available = runtime::available_imports_with_test_flag(target, is_test);
    let (import_remap, _) = runtime::build_import_remap(&import_available);
    let built = direct::build_program_full(
        ast,
        modules,
        rt,
        checker,
        &type_indices,
        &import_remap,
        is_test,
        entry_file,
    )
    .map_err(|err| direct::locate_build_error(err, ast, modules))?;
    Ok(direct::assemble_wasm_module_with_test_flag(
        &built, target, is_test,
    ))
}

#[cfg(test)]
mod located_error_tests {
    use super::*;

    #[test]
    fn cross_file_ufcs_key_collision_does_not_corrupt_codegen() {
        // Regression test for plan 108: two files in the same module
        // had a call at the same `(line, col)` —
        // `home.fai:37:19 — recent.isLoaded()` (UFCS) and
        // `posts.fai:37:19 — length(posts.value)` (bare).
        // The checker's UFCS map keyed by `(module, line, col)`
        // collapsed both into one entry; codegen then read
        // `is_ufcs=true` for `length(...)` and refused with
        // `UnknownIdentifier("length")`.
        //
        // The fix keys per-call-site metadata by file path so the
        // two entries stay distinct. Setting up two files in one
        // module with the same `(line, col)` shape and asserting
        // codegen succeeds proves the fix.
        use fai_compiler::compiler::DiscoveredModule;

        let file_a = "# Helper.\ndef helperA\n    @return Bool\ndo\n  isEmpty('a')\nend\n";
        let file_b = "# Helper.\ndef helperB\n    @return Int\ndo\n  length([1])\nend\n";
        let prep_a = fai_compiler::prepare_source(file_a, None).expect("prepare A");
        let prep_b = fai_compiler::prepare_source(file_b, None).expect("prepare B");
        let mut statements = prep_a.serde_ast.statements;
        let count_a = statements.len();
        statements.extend(prep_b.serde_ast.statements);
        let mut file_paths = vec![Some("file_a.fai".to_string()); count_a];
        file_paths.resize(statements.len(), Some("file_b.fai".to_string()));

        let mod_helpers = DiscoveredModule {
            name: "helpers".to_string(),
            statements,
            file_paths,
            private_names: Vec::new(),
        };
        let entry = fai_compiler::prepare_source(
            "use helpers\n\ndef main\n    @return Void\ndo\nend\n",
            None,
        )
        .expect("prepare entry");
        let info = direct::CheckerInfo::default();
        let result =
            codegen_direct_full_reasoned(&entry.serde_ast, &[mod_helpers], &info, None, false);
        assert!(
            result.is_ok(),
            "codegen should succeed for cross-file calls at the same (line, col); got: {:?}",
            result.err()
        );
    }

    #[test]
    fn codegen_refusal_carries_file_and_line() {
        // Plan 108 #1 regression test: when codegen rejects a program,
        // the error must carry source location info so the CLI can
        // group by file under `Source codegen errors:` instead of
        // dumping a bare `UnknownIdentifier(...)` with no context.
        //
        // Construct a program that calls a function name codegen
        // can't resolve. The precise refusal site doesn't matter —
        // what matters is that the returned error has a non-empty
        // line. Falls back to `UnknownIdentifier` since that path is
        // covered by `find_name_in_statements`.
        let src =
            "# Trigger.\ndef trigger\n    @return Int\ndo\n  somethingThatDoesNotExist()\nend\n";
        let prep = fai_compiler::prepare_source(src, None).expect("prepare");
        let info = direct::CheckerInfo::default();
        let result = codegen_direct_full_reasoned(&prep.serde_ast, &[], &info, None, false);
        let err = result.expect_err("codegen should refuse a call to an undefined name");
        assert!(
            err.line.is_some(),
            "located error should carry a line number, got: {:?}",
            err
        );
        let line = err.line.unwrap();
        assert!(line > 0, "line should be > 0, got: {}", line);
    }

    #[test]
    fn async_effect_program_compiles_post_wait_lets() {
        let src = "def main\n    @return Int\ndo\n  sleep(1)\n  let x = 42\n  x\nend\n";
        let prep = fai_compiler::prepare_source(src, None).expect("prepare");
        let info = direct::CheckerInfo::default();
        let result = codegen_direct_full_reasoned(&prep.serde_ast, &[], &info, None, false);
        assert!(
            result.is_ok(),
            "async lowering should support post-wait let bindings: {:?}",
            result.err()
        );
    }

    #[test]
    fn async_effect_program_compiles_suspending_range_for() {
        let src = concat!(
            "def embed\n",
            "    @param i Int\n",
            "    @return Int\n",
            "do\n",
            "  sleep(1)\n",
            "  i + 1\n",
            "end\n\n",
            "def main\n",
            "    @return Int\n",
            "do\n",
            "  var total = 0\n",
            "  for i in 0..4\n",
            "    let next = embed(i)\n",
            "    total = total + next\n",
            "  end\n",
            "  total\n",
            "end\n",
        );
        let prep = fai_compiler::prepare_source(src, None).expect("prepare");
        let info = direct::CheckerInfo::default();
        let result = codegen_direct_full_reasoned(&prep.serde_ast, &[], &info, None, false);
        assert!(
            result.is_ok(),
            "async lowering should support suspending range-for loops: {:?}",
            result.err()
        );
    }

    #[test]
    fn async_effect_program_compiles_in_test_mode() {
        let src = "def child\n    @return Int\ndo\n  sleep(1)\n  42\nend\n\n\
test child\n  it 'returns after wait'\n    assert.equals(child(), 42)\n  end\nend\n";
        let prep = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
            src,
            None,
            Vec::new(),
            None,
        )
        .expect("prepare");
        let info = direct::CheckerInfo::default();
        let result = codegen_direct_full_reasoned(&prep.serde_ast, &[], &info, None, true);
        assert!(
            result.is_ok(),
            "test-mode codegen should allow wait-backed functions: {:?}",
            result.err()
        );
    }
}
