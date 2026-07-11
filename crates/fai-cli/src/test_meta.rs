//! AST-side extraction of test + coverage metadata.
//!
//! Phase H of Plan 94 removes the bytecode emitter; `cmd_test` and
//! `step_test` used to harvest `program.tests` and `program.protos`
//! from the bytecode compile. This module walks the AST directly
//! so the wasm test runner and the coverage rule don't need a
//! bytecode `CompiledProgram`.

use fai_compiler::ast::Statement;
use fai_compiler::PreparedProgram;
use serde::{Deserialize, Serialize};

/// One test suite as it reaches the wasm `_fai_run_test` dispatcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSuiteMeta {
    /// Bare suite identifier from `test <name>`. Stays unqualified so
    /// the coverage rule can compare against function names directly.
    pub suite_name: String,
    /// Module the suite was declared in. `None` for the entry module.
    /// Combined with `suite_name` to produce `<module>.<suite>` for
    /// display in test output, keeping same-named suites in different
    /// modules distinguishable.
    pub module_name: Option<String>,
    pub case_descriptions: Vec<String>,
    pub has_before_all: bool,
    pub has_after_all: bool,
    /// Source line of the `test` keyword. 0 when location info isn't
    /// available (tolerated rather than required so synthetic tests
    /// don't break the runner).
    pub line: u32,
    /// Absolute path of the `.fai` file this suite was declared in, when
    /// known. Drives incremental test selection (plan 135): a file changes
    /// → its suites are dirty → rerun. `None` for synthetic/entry suites
    /// with no file origin, which are always treated as dirty.
    pub file: Option<String>,
}

impl TestSuiteMeta {
    /// Display form: `<module>.<suite>` when the suite came from a
    /// module, plain `<suite>` for the entry program. Used by the
    /// CLI test output so two `createUser` suites in different
    /// modules don't collide visually.
    pub fn display_name(&self) -> String {
        match &self.module_name {
            Some(m) => format!("{}.{}", m, self.suite_name),
            None => self.suite_name.clone(),
        }
    }
}

/// Extraction result: tests to drive, plus candidate function names
/// the coverage rule should require a matching suite for.
/// A function that must have a co-located test (the coverage rule), plus
/// the file it was declared in (plan 135, for co-location enforcement and
/// dirty-file coverage checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageCandidate {
    pub name: String,
    pub file: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct TestMeta {
    pub suites: Vec<TestSuiteMeta>,
    /// Functions eligible for the coverage rule (name + declaring file).
    /// Caller still applies the `main` / `<...>` / `__...` name filter
    /// (mirrors the prior bytecode-era filter kept in `step_test`).
    pub coverage_candidates: Vec<CoverageCandidate>,
}

impl TestMeta {
    /// Bare names of every coverage candidate.
    pub fn candidate_names(&self) -> Vec<String> {
        self.coverage_candidates
            .iter()
            .map(|c| c.name.clone())
            .collect()
    }
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
    extract_with_entry(prepared, None)
}

/// Like [`extract`], but annotates entry-module (`serde_ast`) suites and
/// candidates with `entry_file` — the path of the file those top-level
/// statements came from. Module statements carry their own per-statement
/// origin via `DiscoveredModule::file_paths`. Used by the incremental
/// runner (plan 135) so every suite/candidate knows its source file.
pub fn extract_with_entry(prepared: &PreparedProgram, entry_file: Option<&str>) -> TestMeta {
    let mut meta = TestMeta::default();
    let entry_owned = entry_file.map(|s| s.to_string());
    collect(&prepared.serde_ast.statements, None, &|_| entry_owned.clone(), &mut meta);

    if prepared.is_test {
        for m in &prepared.modules {
            let is_external = m
                .name
                .chars()
                .next()
                .map(|c| c.is_uppercase())
                .unwrap_or(false);
            if !is_external {
                let file_paths = &m.file_paths;
                collect(
                    &m.statements,
                    Some(&m.name),
                    &|i| file_paths.get(i).cloned().flatten(),
                    &mut meta,
                );
            }
        }
    }

    meta
}

fn collect(
    statements: &[Statement],
    module_prefix: Option<&str>,
    file_of: &dyn Fn(usize) -> Option<String>,
    out: &mut TestMeta,
) {
    for (i, stmt) in statements.iter().enumerate() {
        match stmt {
            Statement::FunctionDeclaration(fd) => {
                if !fd.is_private.unwrap_or(false) {
                    out.coverage_candidates.push(CoverageCandidate {
                        name: fd.name.clone(),
                        file: file_of(i),
                    });
                }
            }
            Statement::TestDeclaration(td) => {
                out.suites.push(TestSuiteMeta {
                    suite_name: td.name.clone(),
                    module_name: module_prefix.map(|m| m.to_string()),
                    case_descriptions: td.cases.iter().map(|c| c.description.clone()).collect(),
                    has_before_all: td.before_all.is_some(),
                    has_after_all: td.after_all.is_some(),
                    line: td.location.line,
                    file: file_of(i),
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
        assert!(meta.candidate_names().contains(&"double".to_string()));
        assert!(meta.candidate_names().contains(&"main".to_string()));
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
        assert_eq!(meta.candidate_names(), vec!["main".to_string()]);
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
        assert!(meta.candidate_names().contains(&"publicFn".to_string()));
        assert!(!meta.candidate_names().contains(&"privateFn".to_string()));
    }
}
