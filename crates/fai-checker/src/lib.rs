//! FAI type checker — validates types before bytecode compilation.

pub mod builtins;
pub mod checker;
pub mod environment;
pub mod error;
pub mod std_modules;
pub mod types;

pub use checker::{Checker, PreparedModule};
pub use error::CheckError;

/// Maximum number of wasm argument slots a function or closure may use.
/// Generic type parameters occupy a slot each, so a declaration's slot
/// count is `params + type_params`. The direct wasm backend pre-builds one
/// `FaiFunc(arity)` type per arity up to this limit (fai-codegen-wasm's
/// `MAX_DIRECT_ARITY` re-exports this value); the checker rejects
/// declarations over the limit so codegen never sees them.
pub const MAX_FUNCTION_ARITY: usize = 16;

#[cfg(test)]
mod tests;
