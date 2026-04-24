//! Post-parse module discovery output.
//!
//! `DiscoveredModule` is what `prepare_source_*` yields for each
//! module resolved under the project tree. It's consumed by the
//! direct AST→wasm builder in `fai-codegen-wasm`. Phase H of
//! Plan 94 deleted the bytecode compiler that used to live
//! alongside this struct — this file keeps the small AST-level
//! carrier that remains.

use crate::ast::Statement;

/// A module discovered during prepare-source: its statements plus
/// any names declared private to the module. `name` is the
/// qualified module name (`util`, `Forui.view`, etc.) the caller
/// uses in `use { X } from name`.
#[derive(Debug, Clone)]
pub struct DiscoveredModule {
    pub name: String,
    pub statements: Vec<Statement>,
    pub private_names: Vec<String>,
}
