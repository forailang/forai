//! Source-code synthesizers for each resource limit in
//! `fai_core::limits::ALL_LIMITS`. Each function returns forai source
//! that exercises exactly one limit at `n` — callers pass
//! `limit.cap` for an at-cap (should-compile) source and `limit.cap + 1`
//! for an over-cap (should-error) source.
//!
//! The boundary-parity test harness (this crate + fai-codegen-wasm)
//! drives these to ensure every limit:
//!  (a) compiles at its cap on every backend,
//!  (b) errors with the unified `LimitExceeded` shape at cap + 1 on
//!      every backend.
//!
//! Adding a new resource limit is a two-step ritual:
//!   1. Add a `pub const` in `fai-core/src/limits.rs` and to
//!      `ALL_LIMITS`.
//!   2. Add a matching arm to `synthesize_source_at` here.
//! The boundary-parity test then exercises it on both backends
//! automatically.

use fai_core::limits::{self, ResourceLimit};

/// Build a program that uses exactly `n` of the resource described by
/// `limit`. Pass `limit.cap` to build an at-cap sample (should compile)
/// or `limit.cap + 1` to build an over-cap sample (should error). The
/// harness relies on these two inputs producing compile-level behavior
/// only — the programs don't need to execute meaningfully.
///
/// Returns `None` when we don't (yet) have a synthesizer for the given
/// limit. The harness treats `None` as "skip" rather than fail so this
/// module and `ALL_LIMITS` can evolve independently during work in
/// progress; the matching coverage test asserts `None` never happens
/// for a limit that's in `ALL_LIMITS`.
pub fn synthesize_source_at(limit: &ResourceLimit, n: usize) -> Option<String> {
    // Dispatch by name. `std::ptr::eq` on `&REGISTERS` doesn't work —
    // `pub const` values are inlined at each use site (no single
    // canonical address), so every caller's `&limits::REGISTERS`
    // produces a fresh pointer. The name is the single-source-of-truth
    // identifier set in `ResourceLimit` and asserted stable by
    // `limits::tests::all_limits_list_is_in_sync_with_constants`.
    match limit.name {
        "registers" => Some(registers(n)),
        "parameters" => Some(parameters(n)),
        "upvalues" => Some(upvalues(n)),
        "call arguments" => Some(call_args(n)),
        "constants" => Some(constants(n)),
        "string pool" => Some(string_pool(n)),
        "call depth" => Some(call_depth(n)),
        // METHOD_IDS is enforced by a static inventory test on the
        // wasm codegen (it's a wasm-only limit with no source-level
        // way to pressure it) — no synthesizer here.
        "wasm native method ids" => None,
        // Defensive: any new limit name that lands in ALL_LIMITS
        // without an update here should fire the coverage test in
        // this module's `tests` mod and be surfaced immediately.
        _ => None,
    }
}

/// A function body with `n` distinct let-bindings, all live through
/// the final expression. Each `let aI = I` reserves a live register.
fn registers(n: usize) -> String {
    let mut src = String::from("def main\n    @return Int\ndo\n");
    for i in 0..n {
        src.push_str(&format!("  let a{i} = {i}\n"));
    }
    // Touch the first binding so none can be DCE'd.
    src.push_str("  a0\nend\n");
    src
}

/// A function declaration with `n` `@param` entries.
fn parameters(n: usize) -> String {
    let mut src = String::from("# Many.\ndef many\n");
    for i in 0..n {
        src.push_str(&format!("    @param p{i} Int\n"));
    }
    src.push_str("    @return Int\ndo\n  p0\nend\n\n");
    src.push_str("def main\n    @return Void\ndo\n  print('ok')\nend\n");
    src
}

/// An outer function with `n` locals, captured by an inner closure.
///
/// The closure body references each `aI` via a separate `let _ = aI`
/// statement rather than a single `a0 + a1 + ...` expression. The
/// additive form blows the parser's stack at `n` ≈ 200 (left-leaning
/// binary AST recurses once per operand); the statement form is flat.
fn upvalues(n: usize) -> String {
    let mut src = String::from("# Outer.\ndef outer\n    @return Int\ndo\n");
    for i in 0..n {
        src.push_str(&format!("  let a{i} = {i}\n"));
    }
    src.push_str("  let inner = do\n");
    // One `let` per capture — flat sequence, no deep expression tree.
    // The `let _xI = aI` references aI, which forces the closure to
    // capture it as an upvalue.
    for i in 0..n {
        src.push_str(&format!("    let _x{i} = a{i}\n"));
    }
    // Tail expression; _x0 keeps at least one read alive.
    src.push_str("    _x0\n");
    src.push_str("  end\n");
    src.push_str("  inner()\nend\n\n");
    src.push_str("def main\n    @return Void\ndo\n  print(outer())\nend\n");
    src
}

/// A single call site passing `n` positional arguments.
fn call_args(n: usize) -> String {
    // Callee with exactly `n` parameters so the type checker is happy.
    // If n itself exceeds PARAMETERS.cap the callee's synthesis errors
    // first — but that's fine, we only care about the cap + 1 case for
    // CALL_ARGS (cap == 255) which is also PARAMETERS.cap; both limits
    // firing at the same n is OK — the harness asserts the error names
    // *a* limit, not one specific one when caps collide.
    let mut src = String::from("# Callee.\ndef callee\n");
    for i in 0..n {
        src.push_str(&format!("    @param p{i} Int\n"));
    }
    src.push_str("    @return Int\ndo\n  p0\nend\n\n");
    src.push_str("def main\n    @return Int\ndo\n  callee(");
    for i in 0..n {
        if i > 0 {
            src.push_str(", ");
        }
        src.push_str(&format!("{i}"));
    }
    src.push_str(")\nend\n");
    src
}

/// A function body that introduces `n` distinct numeric constants.
/// Uses fractional floats so each value interns a fresh constant-pool
/// entry (integer literals go through LoadInt encoding and don't touch
/// the constant pool).
fn constants(n: usize) -> String {
    // Use `var sum = 0.0; sum = sum + F` pattern so every constant is
    // forced through `add_constant`. Summing keeps them live.
    let mut src = String::from("def main\n    @return Float\ndo\n  var sum = 0.0\n");
    for i in 0..n {
        // `i as f64 + 0.5` — fractional + unique, guaranteed new constant.
        src.push_str(&format!("  sum = sum + {}.5\n", i));
    }
    src.push_str("  sum\nend\n");
    src
}

/// A module with `n` distinct string literals pushed into the module
/// string pool. `print('sN')` in a loop works — each literal
/// interns a fresh string.
fn string_pool(n: usize) -> String {
    let mut src = String::from("def main\n    @return Void\ndo\n");
    for i in 0..n {
        src.push_str(&format!("  print('s{i}')\n"));
    }
    src.push_str("end\n");
    src
}

/// A recursive function that reaches call-depth `n`. Runtime-only —
/// the compiler doesn't refuse this, the VM/wasm runtime does when
/// the recursion is actually attempted.
fn call_depth(n: usize) -> String {
    format!(
        "# F.\ndef f\n    @param n Int\n    @return Int\ndo\n  if n <= 0\n    0\n  else\n    f(n - 1) + 1\n  end\nend\n\ndef main\n    @return Int\ndo\n  f({n})\nend\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesizer_exists_for_every_non_static_limit() {
        // Every limit in ALL_LIMITS must either have a synthesizer or
        // be a known static-inventory limit (currently METHOD_IDS —
        // enforced by a compile-time test in fai-codegen-wasm instead
        // of by source synthesis).
        let static_only: &[&str] = &["wasm native method ids"];
        for limit in limits::ALL_LIMITS {
            if static_only.contains(&limit.name) {
                continue;
            }
            assert!(
                synthesize_source_at(limit, limit.cap).is_some(),
                "no synthesizer for limit `{}` — add one in limit_synths.rs or add to `static_only`",
                limit.name,
            );
        }
    }

    #[test]
    fn register_synth_produces_exactly_n_lets() {
        let src = registers(10);
        let let_count = src.matches("let a").count();
        assert_eq!(let_count, 10);
    }

    #[test]
    fn parameter_synth_produces_exactly_n_params() {
        let src = parameters(10);
        let param_count = src.matches("@param p").count();
        assert_eq!(param_count, 10);
    }

    #[test]
    fn call_args_synth_has_matching_arity() {
        let src = call_args(5);
        let commas_in_call = src.matches(", ").count();
        // 5 args → 4 commas in the call site (plus none in param decls).
        assert!(
            commas_in_call >= 4,
            "expected ≥4 arg separators, got: {}\n{src}",
            commas_in_call
        );
    }
}
