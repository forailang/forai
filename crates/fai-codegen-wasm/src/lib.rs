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

pub mod direct;
mod program;
mod runtime;

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
) -> Result<Vec<u8>, direct::BuildError> {
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
    )?;
    Ok(direct::assemble_wasm_module_with_test_flag(
        &built, target, is_test,
    ))
}
