//! Per-function metadata for the direct AST→wasm builder.
//!
//! Phase H of Plan 94 removed the bytecode-shaped `WasmProgram`
//! helper and `CompiledProgram`-based projections; `FunctionInfo`
//! is the only surviving piece the direct path consumes.

/// Per-function metadata the wasm backend needs to build signatures,
/// exports, and the test dispatcher. Nothing bytecode-shaped.
#[derive(Debug, Clone, Default)]
pub struct FunctionInfo {
    /// Function name as declared in source (or compiler-synthesised
    /// placeholder for closures / script bodies). Exported from the
    /// wasm module when it's a real named top-level function.
    pub name: String,
    /// Number of parameters. Drives `FaiFunc(N)` type registration and
    /// call-indirect signature lookup.
    pub param_count: u16,
    /// Number of hidden generic `@type` parameters prepended before
    /// source-level parameters in the wasm signature.
    pub type_param_count: u16,
    /// CLI coverage-tracking flag. Set by the compiler when the
    /// function should count toward the "every public function needs a
    /// test" rule enforced by the CLI test runner.
    pub include_in_coverage: bool,
    /// Per-parameter default expression (parallel to param_count). A
    /// `Some(expr)` entry lets a call site omit that argument — the
    /// codegen emits the default expression in its place. Ordered the
    /// same as the function's declared parameters.
    pub param_defaults: Vec<Option<fai_compiler::ast::Expression>>,
    /// Source file the function was declared in, when known. Entry-AST
    /// functions and compiler-synthesised wrappers carry `None`.
    pub source_file: Option<String>,
    /// 1-based declaration line in `source_file`. 0 = unknown
    /// (synthesised functions). Feeds the `fai-dbg` debug side-table
    /// so trap backtraces can show `name (file:line)` (plan 116).
    pub source_line: u32,
}
