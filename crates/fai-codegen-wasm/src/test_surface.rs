//! Test-surface synthesis shared by the direct builder and the async engine
//! (plan 103 U6).
//!
//! A `test` suite's cases become zero-arg wrapper functions whose body is
//! `setup ++ beforeEach ++ case.body ++ afterEach` (hooks get their own
//! wrappers at the reserved case indices). The direct builder compiles these
//! synchronously and routes `_fai_run_test(suite, case)` to them; the async
//! engine injects them into the program *before* async analysis so a case
//! that suspends compiles as an ordinary resumable task — hook ordering
//! survives suspension because the whole wrapper is one function.

use fai_compiler::ast::{FunctionDeclaration, Program, Statement, TestDeclaration};
use fai_compiler::compiler::DiscoveredModule;

/// Reserved case indices for suite-level hooks (must match the runner).
pub const TEST_HOOK_BEFORE_ALL_CASE_IDX: u16 = u16::MAX;
pub const TEST_HOOK_AFTER_ALL_CASE_IDX: u16 = u16::MAX - 1;

/// One synthesized wrapper: which (suite, case) it answers for and the
/// wrapper's *unprefixed* function name (module-scoped wrappers get the
/// `{module}.` prefix wherever module functions are prefixed).
#[derive(Debug, Clone)]
pub struct TestWrapperPlan {
    pub suite_name: String,
    pub suite_idx: u16,
    pub case_idx: u16,
    pub fn_name: String,
    /// Module context (None = entry AST), mirroring the suite's location.
    pub module: Option<String>,
}

fn wrapper_decl(name: String, body: Vec<Statement>, td: &TestDeclaration) -> FunctionDeclaration {
    FunctionDeclaration {
        name,
        type_params: Vec::new(),
        params: Vec::new(),
        return_types: Vec::new(),
        body,
        doc: None,
        is_private: None,
        is_abstract: false,
        is_remote: false,
        auth_policy: None,
        location: td.location.clone(),
        doc_comment: None,
    }
}

/// Wrapper declarations for one suite, in emission order:
/// beforeAll?, each case, afterAll?. Bodies are cloned from the suite.
pub fn suite_wrappers(td: &TestDeclaration) -> Vec<(FunctionDeclaration, u16)> {
    let mut out = Vec::new();
    if let Some(before_all) = &td.before_all {
        let mut body: Vec<Statement> = td.setup.clone();
        body.extend(before_all.clone());
        out.push((
            wrapper_decl(format!("<test-before-all:{}>", td.name), body, td),
            TEST_HOOK_BEFORE_ALL_CASE_IDX,
        ));
    }
    for (case_idx, case) in td.cases.iter().enumerate() {
        let mut body: Vec<Statement> = td.setup.clone();
        if let Some(before) = &td.before_each {
            body.extend(before.clone());
        }
        body.extend(case.body.clone());
        if let Some(after) = &td.after_each {
            body.extend(after.clone());
        }
        let mut decl = wrapper_decl(format!("<test:{}#{}>", td.name, case_idx), body, td);
        decl.location = case.location.clone();
        out.push((decl, case_idx as u16));
    }
    if let Some(after_all) = &td.after_all {
        let mut body: Vec<Statement> = td.setup.clone();
        body.extend(after_all.clone());
        out.push((
            wrapper_decl(format!("<test-after-all:{}>", td.name), body, td),
            TEST_HOOK_AFTER_ALL_CASE_IDX,
        ));
    }
    out
}

/// Inject test wrappers as ordinary functions into a cloned program so the
/// async engine (and async analysis before it) sees them as plain roots.
/// Suites are numbered entry-AST-first then modules in order — the same
/// order `test_meta::extract` and the direct builder use, so the runner's
/// (suite, case) indices agree across all three.
pub fn inject_test_wrappers(
    ast: &Program,
    modules: &[DiscoveredModule],
) -> (Program, Vec<DiscoveredModule>, Vec<TestWrapperPlan>) {
    let mut ast = ast.clone();
    let mut modules = modules.to_vec();
    let mut plans = Vec::new();
    let mut suite_idx: u16 = 0;

    let mut entry_new: Vec<Statement> = Vec::new();
    for s in &ast.statements {
        if let Statement::TestDeclaration(td) = s {
            for (decl, case_idx) in suite_wrappers(td) {
                plans.push(TestWrapperPlan {
                    suite_name: td.name.clone(),
                    suite_idx,
                    case_idx,
                    fn_name: decl.name.clone(),
                    module: None,
                });
                entry_new.push(Statement::FunctionDeclaration(decl));
            }
            suite_idx += 1;
        }
    }
    ast.statements.extend(entry_new);

    for m in &mut modules {
        let mut module_new: Vec<Statement> = Vec::new();
        for s in &m.statements {
            if let Statement::TestDeclaration(td) = s {
                for (decl, case_idx) in suite_wrappers(td) {
                    plans.push(TestWrapperPlan {
                        suite_name: td.name.clone(),
                        suite_idx,
                        case_idx,
                        fn_name: decl.name.clone(),
                        module: Some(m.name.clone()),
                    });
                    module_new.push(Statement::FunctionDeclaration(decl));
                }
                suite_idx += 1;
            }
        }
        // Keep file_paths aligned with statements if the module tracks them.
        for _ in 0..module_new.len() {
            m.file_paths.push(None);
        }
        m.statements.extend(module_new);
    }

    (ast, modules, plans)
}
