//! Source fixtures pinned to the exact shapes that crashed forai in
//! production (partners-1 and partners-2 API explorer sessions). These
//! are SHARED between the VM regression suite (fai-runtime) and the
//! WASM codegen regression suite (fai-codegen-wasm) so a fix that
//! works on one backend but not the other fails loudly.
//!
//! Each fixture is a function returning forai source. Using a function
//! (rather than a `const`) keeps the generated source readable — the
//! `n` parameter lets the caller dial the shape size up or down from
//! the value that originally crashed, so we can pin a regression
//! exactly OR widen the scenario to stress future cap bumps.

/// **Wide typed-record parser** — the partners-2 `parseGameObject`
/// shape. A function that builds a typed record with `n` fields,
/// each via `unwrap(getString(d, 'k'), '')`. The crash surface:
/// live temporaries per field stack up during construction and push
/// the per-function register count past the u8 cap when `n` is large.
///
/// At the partners-2 shape (`n = 10`), this must compile cleanly.
/// Larger `n` stresses the register budget — used by the parity
/// harness to verify we fail with a clean `registers: limit exceeded`
/// error rather than a Rust panic / silent wasm miscompile.
///
/// The generated source defines a `Row` type + a `parseRow` parser +
/// a `main` that calls it once on a fixed dict. It uses no external
/// dependencies so the fixture is runnable through any forai backend.
pub fn wide_typed_record_parse(n: usize) -> String {
    let mut src = String::new();
    // Type declaration — one String field per slot.
    src.push_str("type Row\n");
    for i in 0..n {
        src.push_str(&format!("  f{i} String\n"));
    }
    src.push_str("end\n\n");

    // Parser — the register-pressure shape we care about: all N
    // `unwrap(getString(d, 'kI'), '')` expressions live simultaneously
    // inside one labeled-arg constructor call. Matches the partners-2
    // pattern exactly.
    src.push_str("# ParseRow.\ndef parseRow\n    @param d Dictionary\n    @return Row\ndo\n  Row(");
    for i in 0..n {
        if i > 0 {
            src.push_str(", ");
        }
        src.push_str(&format!("f{i}: unwrap(getString(d, 'k{i}'), '')"));
    }
    src.push_str(")\nend\n\n");

    // Driver — builds a minimal input dict, parses, prints field 0.
    src.push_str("def main\n    @return Void\ndo\n  var d = {}\n");
    for i in 0..n {
        src.push_str(&format!("  d = set(d, 'k{i}', 'v{i}')\n"));
    }
    src.push_str("  let r = parseRow(d)\n  print(r.f0)\nend\n");
    src
}

/// **Array loop of a wide parser** — the partners-2 runtime shape.
/// An outer function that iterates `rows` times, calling the wide
/// parser on a minimal dict each iteration. Mirrors the `for t in
/// body.teams: out = array.append(out, parseTeam(t))` pattern.
///
/// Crash surface: each parse allocates a new typed record; the VM's
/// register stack needs to fit the loop's frame plus the parse
/// frame plus the record growing. With the dynamic register-stack
/// growth (VM side) and the compile-time register-cap check, this
/// must run to completion — even for `rows = 200` (partners-2 hit
/// 152 teams on the real demo API).
pub fn array_loop_of_wide_parse(rows: usize, fields: usize) -> String {
    let mut src = String::new();
    // Same Row + parseRow the single-parse fixture uses.
    src.push_str("type Row\n");
    for i in 0..fields {
        src.push_str(&format!("  f{i} String\n"));
    }
    src.push_str("end\n\n");

    src.push_str("# ParseRow.\ndef parseRow\n    @param d Dictionary\n    @return Row\ndo\n  Row(");
    for i in 0..fields {
        if i > 0 {
            src.push_str(", ");
        }
        src.push_str(&format!("f{i}: unwrap(getString(d, 'k{i}'), '')"));
    }
    src.push_str(")\nend\n\n");

    // `makeDict()` returns a freshly-populated dict per iteration so
    // each parse starts clean.
    src.push_str("# MakeDict.\ndef makeDict\n    @return Dictionary\ndo\n  var d = {}\n");
    for i in 0..fields {
        src.push_str(&format!("  d = set(d, 'k{i}', 'v{i}')\n"));
    }
    src.push_str("  d\nend\n\n");

    // Main — builds a list of `rows` dicts, calls parseRow on each,
    // counts successes.
    src.push_str("def main\n    @return Void\ndo\n  var count = 0\n  var i = 0\n");
    src.push_str(&format!("  while i < {rows}\n"));
    src.push_str("    let r = parseRow(makeDict())\n");
    src.push_str("    if r.f0 == 'v0'\n      count = count + 1\n    end\n");
    src.push_str("    i = i + 1\n");
    src.push_str("  end\n");
    src.push_str("  print(toString(count))\nend\n");
    src
}

/// Convenience wrappers at the exact shapes that crashed partners-2.
/// Tests should reach for these when they're asserting the specific
/// regression rather than scanning a range of sizes.
pub mod partners2 {
    //! Pinned at the values observed in the partners-2 agent session.

    /// The partners-2 `parseGameObject` shape.
    pub fn game_object_parser() -> String {
        super::wide_typed_record_parse(10)
    }

    /// The 152-team runtime load. Iterations set to 152 to match the
    /// trace-partners demo API response; fields = 10 matches a wider
    /// parser (trace Team + team-detail combined in a single record).
    pub fn large_team_list_load() -> String {
        super::array_loop_of_wide_parse(152, 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_typed_record_has_n_field_declarations() {
        let src = wide_typed_record_parse(10);
        let type_decls = src.matches("String\n").count();
        // 10 field decls in the type, no other `String` mentions in
        // the rest of the source.
        assert!(
            type_decls >= 10,
            "expected >= 10 `String` field decls, got {type_decls}\n{src}"
        );
    }

    #[test]
    fn wide_typed_record_calls_constructor_with_labeled_args() {
        let src = wide_typed_record_parse(5);
        assert!(src.contains("f0: unwrap"));
        assert!(src.contains("f4: unwrap"));
    }

    #[test]
    fn array_loop_fixture_has_the_requested_iteration_count() {
        let src = array_loop_of_wide_parse(152, 10);
        assert!(src.contains("while i < 152"));
    }

    #[test]
    fn partners2_game_object_is_10_fields_wide() {
        let src = partners2::game_object_parser();
        let field_count = src.matches("f").count();
        // Hand-wavy sanity — we expect references to f0..f9 in the
        // type, the parser, and the field access. Enough to fail if
        // someone edits the fixture down to 5 fields.
        assert!(
            field_count >= 20,
            "fixture suspiciously small ({field_count} `f` occurrences)"
        );
    }
}
