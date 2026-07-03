//! Async-effectful build diagnostics.
//!
//! The real async engine (`direct::try_codegen_async_engine`) is the only
//! async lowering path. When it declines an async-effectful program, this
//! module renders the located `AsyncLoweringUnsupported` diagnostic from the
//! effect analysis so the program fails with a clear cause instead of
//! falling through to the sync compiler.
//!
//! The legacy minimal-wait emitter that previously lived behind this facade
//! (`async_wait_codegen` / `async_emit_spec`) was retired once the engine
//! subsumed every shape it accepted; see plan 131 for the retirement
//! evidence (full fixture, CLI, and browser suites green with it disabled).

use crate::{async_analysis::AsyncAnalysis, direct, LocatedBuildError};

/// The located "async shape unsupported" error for an async-effectful
/// program the engine declined, or `None` when the program has no async
/// effects (or is a test build, where the sync test lane applies).
pub fn async_unsupported_error(
    analysis: &AsyncAnalysis,
    is_test: bool,
) -> Option<LocatedBuildError> {
    if analysis.is_empty() || is_test {
        return None;
    }

    analysis.first_cause().map(|(function, cause)| LocatedBuildError {
        err: direct::BuildError::AsyncLoweringUnsupported {
            function: function.to_string(),
            cause: format!("{:?}", cause.kind),
        },
        file: cause.file.clone(),
        line: Some(cause.location.line),
        col: Some(cause.location.column),
        module: cause.module.clone(),
    })
}
