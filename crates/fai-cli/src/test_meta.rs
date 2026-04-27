//! AST-side extraction of test + coverage metadata.
//!
//! Phase H of Plan 94 removes the bytecode emitter; `cmd_test` and
//! `step_test` used to harvest `program.tests` and `program.protos`
//! from the bytecode compile. This module walks the AST directly
//! so the wasm test runner and the coverage rule don't need a
//! bytecode `CompiledProgram`.

use fai_compiler::ast::Statement;
use fai_compiler::PreparedProgram;

/// One test suite as it reaches the wasm `_fai_run_test` dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSuiteMeta {
    pub suite_name: String,
    pub case_descriptions: Vec<String>,
    pub has_before_all: bool,
    pub has_after_all: bool,
}

/// Extraction result: tests to drive, plus candidate function names
/// the coverage rule should require a matching suite for.
#[derive(Debug, Default, Clone)]
pub struct TestMeta {
    pub suites: Vec<TestSuiteMeta>,
    /// Function names eligible for the coverage rule. Caller still
    /// applies the `main` / `<...>` / `__...` name filter (mirrors
    /// the prior bytecode-era filter kept in `step_test`).
    pub coverage_candidates: Vec<String>,
}

/// Walk the prepared AST and pull out test-suite specs + the list of
/// function names that should count toward the coverage rule.
///
/// Matches the bytecode compiler's `include_in_coverage` logic:
/// - Entry module: always included.
/// - Discovered modules: included only in test mode, and only for
///   *local* packages. External packages (module name begins with
///   an uppercase char, e.g. `Forui`) are skipped — their tests and
///   public functions belong to their own target.
pub fn extract(prepared: &PreparedProgram) -> TestMeta {
    let mut meta = TestMeta::default();
    collect(&prepared.serde_ast.statements, &mut meta);

    if prepared.is_test {
        for m in &prepared.modules {
            let is_external = m
                .name
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false);
            if !is_external {
                collect(&m.statements, &mut meta);
            }
        }
    }

    meta
}

fn collect(statements: &[Statement], out: &mut TestMeta) {
    for stmt in statements {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                if !fd.is_private.unwrap_or(false) {
                    out.coverage_candidates.push(fd.name.clone());
                }
            }
            Statement::TestDeclaration(td) => {
                out.suites.push(TestSuiteMeta {
                    suite_name: td.name.clone(),
                    case_descriptions: td.cases.iter().map(|c| c.description.clone()).collect(),
                    has_before_all: td.before_all.is_some(),
                    has_after_all: td.after_all.is_some(),
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(src: &str, for_tests: bool) -> PreparedProgram {
        if for_tests {
            fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
                src,
                None,
                Vec::new(),
                None,
            )
            .expect("prepare")
        } else {
            fai_compiler::prepare_source(src, None).expect("prepare")
        }
    }

    #[test]
    fn extracts_function_declarations() {
        let prepared = prepare(
            concat!(
                "# Doubles x.\n",
                "def double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "end\n",
            ),
            false,
        );
        let meta = extract(&prepared);
        assert!(meta.coverage_candidates.contains(&"double".to_string()));
        assert!(meta.coverage_candidates.contains(&"main".to_string()));
        assert!(meta.suites.is_empty());
    }

    #[test]
    fn extracts_test_suites_with_cases() {
        let prepared = prepare(
            concat!(
                "# Doubles x.\n",
                "def double\n",
                "    @param x Int\n",
                "    @return Int\n",
                "do\n",
                "  x * 2\n",
                "end\n",
                "\n",
                "test double\n",
                "  it 'doubles a positive number'\n",
                "    1\n",
                "  end\n",
                "  it 'doubles zero'\n",
                "    2\n",
                "  end\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "end\n",
            ),
            true,
        );
        let meta = extract(&prepared);
        assert_eq!(meta.suites.len(), 1);
        assert_eq!(meta.suites[0].suite_name, "double");
        assert_eq!(
            meta.suites[0].case_descriptions,
            vec![
                "doubles a positive number".to_string(),
                "doubles zero".to_string(),
            ]
        );
        assert!(!meta.suites[0].has_before_all);
        assert!(!meta.suites[0].has_after_all);
    }

    #[test]
    fn non_test_mode_skips_modules() {
        // Without is_test the module walk is skipped entirely —
        // tests live in the entry AST only for production builds.
        let prepared = prepare("def main\n    @return Void\ndo\nend\n", false);
        let meta = extract(&prepared);
        // Only the entry `main` is picked up.
        assert_eq!(meta.coverage_candidates, vec!["main".to_string()]);
    }

    #[test]
    fn private_functions_are_not_coverage_candidates() {
        let prepared = prepare(
            concat!(
                "# Public.\ndef publicFn\n",
                "    @return Int\n",
                "do\n",
                "  privateFn()\n",
                "end\n",
                "\n",
                "private:\n",
                "# Private.\ndef privateFn\n",
                "    @return Int\n",
                "do\n",
                "  1\n",
                "end\n",
            ),
            true,
        );
        let meta = extract(&prepared);
        assert!(meta.coverage_candidates.contains(&"publicFn".to_string()));
        assert!(!meta.coverage_candidates.contains(&"privateFn".to_string()));
    }
}
