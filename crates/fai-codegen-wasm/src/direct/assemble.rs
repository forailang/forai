use super::*;

/// Best-effort source-location lookup for a `BuildError`.
///
/// The codegen has 30+ `BuildError` raise sites. Threading
/// per-expression source locations through every site is an
/// open-ended refactor (plan 108 #1, ongoing); this helper picks
/// up the cheap wins by walking the AST for the offending name and
/// returning the first match.
///
/// For `UnknownIdentifier(name)` and similar string-bearing variants
/// we look for the first call-site or identifier matching `name` in
/// the entry AST or any module. For `UnsupportedStatement` /
/// `UnsupportedExpression` we currently can't pin down the location
/// from the variant string alone; those land with no location until
/// future work threads it through.
pub fn locate_build_error(
    err: BuildError,
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
) -> crate::LocatedBuildError {
    use fai_compiler::ast::Statement;

    let target_name: Option<&str> = match &err {
        BuildError::UnknownIdentifier(name) => Some(name.as_str()),
        BuildError::ModuleAccessNotYetSupported(name) => Some(name.as_str()),
        BuildError::UnknownBinaryOp(name) => Some(name.as_str()),
        BuildError::UnknownUnaryOp(name) => Some(name.as_str()),
        BuildError::DuplicateModuleName(name) => Some(name.as_str()),
        _ => None,
    };

    if let Some(name) = target_name {
        // Walk modules first — that's where most user code lives.
        for m in modules {
            if let Some((line, col, file)) =
                find_name_in_statements(&m.statements, name, &m.file_paths)
            {
                return crate::LocatedBuildError {
                    err,
                    file,
                    line: Some(line),
                    col: Some(col),
                    module: Some(m.name.clone()),
                };
            }
        }
        // Fall back to the entry AST.
        if let Some((line, col, _)) = find_name_in_statements(&ast.statements, name, &[]) {
            return crate::LocatedBuildError {
                err,
                file: None,
                line: Some(line),
                col: Some(col),
                module: None,
            };
        }
    }

    let _ = Statement::UseStatement; // silence unused-import lint when target_name is None
    crate::LocatedBuildError::unlocated(err)
}

/// Walk top-level statements (and one level into function bodies)
/// looking for a call or identifier matching `name`. Returns the
/// `(line, col, file)` of the first match, where `file` is pulled
/// from `file_paths` aligned to the statement that contains the
/// match.
fn find_name_in_statements(
    statements: &[fai_compiler::ast::Statement],
    name: &str,
    file_paths: &[Option<String>],
) -> Option<(u32, u32, Option<String>)> {
    for (idx, stmt) in statements.iter().enumerate() {
        let file = file_paths.get(idx).cloned().flatten();
        if let Some((line, col)) = scan_statement_for_name(stmt, name) {
            return Some((line, col, file));
        }
    }
    None
}

fn scan_statement_for_name(stmt: &fai_compiler::ast::Statement, name: &str) -> Option<(u32, u32)> {
    use fai_compiler::ast::Statement;
    match stmt {
        Statement::FunctionDeclaration(fd) => {
            for body_stmt in &fd.body {
                if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                    return Some(loc);
                }
            }
            None
        }
        Statement::TestDeclaration(td) => {
            for case in &td.cases {
                for body_stmt in &case.body {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            None
        }
        Statement::ExpressionStatement(es) => scan_expression_for_name(&es.expression, name),
        Statement::LetStatement(ls) => scan_expression_for_name(&ls.value, name),
        Statement::VarStatement(vs) => scan_expression_for_name(&vs.value, name),
        Statement::ReturnStatement(rs) => rs
            .value
            .as_ref()
            .and_then(|v| scan_expression_for_name(v, name)),
        Statement::IfStatement(is) => {
            for branch in &is.branches {
                if let Some(loc) = scan_expression_for_name(&branch.condition, name) {
                    return Some(loc);
                }
                for body_stmt in &branch.body {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            if let Some(else_branch) = &is.else_branch {
                for body_stmt in else_branch {
                    if let Some(loc) = scan_statement_for_name(body_stmt, name) {
                        return Some(loc);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn scan_expression_for_name(
    expr: &fai_compiler::ast::Expression,
    name: &str,
) -> Option<(u32, u32)> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) if id.name == name => {
            Some((id.location.line, id.location.column))
        }
        Expression::CallExpression(ce) => {
            if let Expression::IdentifierExpression(id) = &*ce.callee {
                if id.name == name {
                    return Some((ce.location.line, ce.location.column));
                }
            }
            scan_expression_for_name(&ce.callee, name).or_else(|| {
                ce.args
                    .iter()
                    .find_map(|a| scan_expression_for_name(&a.value, name))
            })
        }
        Expression::MemberExpression(me) => {
            if me.property == name {
                return Some((me.location.line, me.location.column));
            }
            scan_expression_for_name(&me.object, name)
        }
        Expression::BinaryExpression(be) => scan_expression_for_name(&be.left, name)
            .or_else(|| scan_expression_for_name(&be.right, name)),
        Expression::UnaryExpression(ue) => scan_expression_for_name(&ue.expression, name),
        Expression::IndexExpression(ie) => scan_expression_for_name(&ie.object, name)
            .or_else(|| scan_expression_for_name(&ie.index, name)),
        Expression::OptionalCheckExpression(oc) => scan_expression_for_name(&oc.expression, name),
        Expression::ForceUnwrapExpression(fu) => scan_expression_for_name(&fu.expression, name),
        _ => None,
    }
}

/// Errors the builder surfaces when it sees a construct it doesn't
/// handle. The production compiler surfaces these as actionable
/// direct-codegen diagnostics.
#[derive(Debug, Clone)]
pub enum BuildError {
    /// A Statement variant the builder hasn't migrated yet.
    UnsupportedStatement(&'static str),
    /// An Expression variant the builder hasn't migrated yet.
    UnsupportedExpression(&'static str),
    /// A boxed-returning host import has no ownership signature in the
    /// plan-117 table (checked builds only; unchecked builds log the
    /// `[abi-check] MISSING-SIGNATURE` sentinel instead).
    MissingOwnershipSignature(String),
    /// A binary operator string we don't recognise.
    UnknownBinaryOp(String),
    /// A unary operator string we don't recognise.
    UnknownUnaryOp(String),
    /// An identifier that resolves neither to a parameter nor a local
    /// binding. Module imports and globals go through dedicated paths
    /// — an `UnknownIdentifier` here means the name really isn't in
    /// scope at the AST level.
    UnknownIdentifier(String),
    /// Module-qualified member access that is not a supported std or
    /// user-module function. Field access on values uses a separate path.
    ModuleAccessNotYetSupported(String),
    /// Two discovered modules share the same canonical name. Happens
    /// when a local module directory collides with a dependency
    /// package — e.g. a `src/Forui/` directory in an app that also
    /// depends on the `Forui` package. The user must rename one
    /// rather than have the compiler silently pick a winner.
    DuplicateModuleName(String),
    /// The program contains a function marked async-effectful by
    /// Phase 1 analysis, but resumable async lowering is not
    /// implemented yet.
    AsyncLoweringUnsupported { function: String, cause: String },
}

thread_local! {
    static LAST_ASYNC_ENGINE_ERROR: RefCell<Option<crate::LocatedBuildError>> = RefCell::new(None);
}

pub fn take_last_async_engine_error() -> Option<crate::LocatedBuildError> {
    LAST_ASYNC_ENGINE_ERROR.with(|slot| slot.borrow_mut().take())
}

pub(super) fn clear_last_async_engine_error() {
    LAST_ASYNC_ENGINE_ERROR.with(|slot| {
        slot.borrow_mut().take();
    });
}

pub(super) fn record_async_engine_error(
    err: BuildError,
    fd: &FunctionDeclaration,
    module: Option<&str>,
    file: Option<&str>,
    entry_file: Option<&str>,
) {
    let file = file.map(str::to_string).or_else(|| {
        module
            .is_none()
            .then(|| entry_file.map(str::to_string))
            .flatten()
    });
    let line = (fd.location.line > 0).then_some(fd.location.line);
    let col = (fd.location.column > 0).then_some(fd.location.column);
    LAST_ASYNC_ENGINE_ERROR.with(|slot| {
        *slot.borrow_mut() = Some(crate::LocatedBuildError {
            err,
            file,
            line,
            col,
            module: module.map(str::to_string),
        });
    });
}

// ── Program-level entry point ─────────────────────────────────────
//
// All-or-nothing program codegen: every top-level function, test case,
// and closure must compile through the direct builder.

/// A fully-built wasm program ready to be serialised. `functions[0]`
/// is a synthesised `<__start__>` shim that runs the module-init
/// function and then user `main` (if one exists); its wasm index is
/// what the `_start` export points at. `functions[1]` is the
/// synthesised `<__module_init__>` that assigns every top-level
/// `var` initialiser into its wasm global. User functions follow
/// (main first if defined, then other entry-AST functions, then
/// per-module functions). `closures` are the anonymous
/// FunctionExpression heap-objects encountered inside those bodies
/// — they land in the indirect function table after the top-level
/// functions.
#[derive(Debug)]
pub struct BuiltProgram {
    pub functions: Vec<(FunctionInfo, Function)>,
    pub closures: Vec<BuiltClosure>,
    /// String-literal data to lay out at memory offset 0.
    pub string_data: Vec<u8>,
    /// One entry per (suite, case) pair when test mode is on.
    /// Index into `functions` — the wasm function for that case.
    /// The dispatcher at `_fai_run_test(suite_i, case_i)` uses
    /// this to route. Empty in non-test builds.
    pub test_cases: Vec<TestCaseEntry>,
    /// Number of top-level `var` declarations — each gets a
    /// dedicated mutable i64 wasm global appended after the four
    /// fixed runtime globals (`__heap_ptr`, `__env_ptr`,
    /// `error_flag`, `error_value`). The module assembler uses this
    /// to emit the right number of extra global slots, all
    /// initialised to `VAL_NULL`.
    pub module_var_count: u32,
    /// Helper-level ownership instrumentation sites emitted by the
    /// compiled functions.
    pub ownership_sites: Vec<crate::debug_info::OwnershipSiteDebugEntry>,
}

/// Routing entry for one test case — the dispatcher uses the
/// `(suite_idx, case_idx)` pair to find the corresponding
/// zero-arg wrapper function at `function_index`.
#[derive(Debug, Clone)]
pub struct TestCaseEntry {
    pub suite_name: String,
    pub suite_idx: u16,
    pub case_idx: u16,
    pub function_index: usize,
}


/// Try compiling every top-level function in `ast` through the
/// direct builder. Returns `Ok(BuiltProgram)` when every function
/// succeeds; on the first refusal returns the corresponding
/// `BuildError` so the caller can decide what to do (e.g., fall
/// back to the bytecode path in `module.rs`).
///
/// `main` is emitted first so its wasm function index matches the
/// `_start` export convention. All other top-level functions follow
/// in source order.
///
/// The caller provides `CheckerInfo`; `fai-checker` isn't a
/// production dep of this crate. Extract
/// `(ufcs_calls, named_param_reorder)` from a `Checker` instance
/// that ran against `ast.statements` first.
///
/// `rt_base` is the wasm function index of the first runtime helper
/// — normally `import_count` (after all host imports). A matching
/// module assembler lays functions out as `[imports, rt_helpers,
/// top_level_functions, closures]`.
///
/// `fai_func_type_indices` should cover every param-count used by
/// both top-level functions and any closures they reference. The
/// caller pre-allocates these `FaiFunc(N)` type slots in the
/// module's type section.
pub fn build_program(
    ast: &fai_compiler::ast::Program,
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
) -> Result<BuiltProgram, BuildError> {
    build_program_with_modules(ast, &[], rt, checker, fai_func_type_indices, import_remap)
}

/// Extended entry point that also compiles functions from
/// `modules` (discovered sibling `.fai` files). Each module's
/// functions are added to the unified top-level list with names
/// prefixed by the module's canonical path, e.g.,
/// `"mypkg.helpers.doThing"`. Cross-module calls via
/// `helpers.doThing(...)` route through the alias map; calls
/// between peers inside a module use the `module_context` fallback.
pub fn build_program_with_modules(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
) -> Result<BuiltProgram, BuildError> {
    build_program_full(
        ast,
        modules,
        rt,
        checker,
        fai_func_type_indices,
        import_remap,
        false,
        None,
    )
}

/// Full-feature entry point that also synthesises per-test-case
/// wrapper functions when `is_test` is true. The module assembler
/// reads `BuiltProgram.test_cases` to emit a `_fai_run_test`
/// dispatcher keyed on `(suite_idx, case_idx)`.
///
/// `entry_file` is the path of the entry source file, used only for
/// the debug side-table (plan 116) — entry-AST functions have no
/// per-decl file path the way module functions do, so trap backtraces
/// would otherwise show `main (line 3)` instead of `main (main.fai:3)`.
#[allow(clippy::too_many_arguments)]
pub fn build_program_full(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    rt: RtOffsets,
    checker: &CheckerInfo,
    fai_func_type_indices: &HashMap<u16, u32>,
    import_remap: &[Option<u32>],
    is_test: bool,
    entry_file: Option<&str>,
) -> Result<BuiltProgram, BuildError> {
    // Reject canonical module-name collisions up front. A local
    // `src/Forui/` directory and a dependency package also named
    // `Forui` both produce `m.name = "Forui"`; silently picking one
    // would scramble call-resolution in a way users can't diagnose.
    {
        use std::collections::HashMap as StdMap;
        let mut by_canonical: StdMap<&str, ()> = StdMap::new();
        for m in modules {
            if by_canonical.insert(m.name.as_str(), ()).is_some() {
                return Err(BuildError::DuplicateModuleName(m.name.clone()));
            }
        }
    }

    // Alias map merges explicit namespace `use` imports with unique
    // user-module basename aliases. If two modules share a basename
    // (`auth` and `pages.auth`), no implicit alias is created for
    // that basename; explicit named imports still resolve through
    // their canonical module path.
    let mut module_aliases: HashMap<String, String> = HashMap::new();
    {
        use std::collections::HashMap as StdMap;
        let mut basename_counts: StdMap<String, usize> = StdMap::new();
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                *basename_counts.entry(last.to_string()).or_insert(0) += 1;
            }
        }
        for m in modules {
            if let Some(last) = m.name.rsplit('.').next() {
                if basename_counts.get(last).copied().unwrap_or(0) == 1 {
                    module_aliases.insert(last.to_string(), m.name.clone());
                }
            }
        }
    }
    // Entry-AST aliases win on collision so `use std.array` isn't
    // shadowed by a user module conveniently named `array`.
    for (k, v) in collect_module_aliases_from(None, &ast.statements) {
        module_aliases.insert(k, v);
    }
    // Also fold in aliases declared inside each discovered module —
    // e.g. a helper file doing `use std.string` needs `string.isEmpty`
    // to resolve when its functions compile. Entry-level aliases
    // already won above; here we only insert keys that aren't taken.
    for m in modules {
        for (k, v) in collect_module_aliases_from(Some(&m.name), &m.statements) {
            module_aliases.entry(k).or_insert(v);
        }
    }

    let mut module_function_exports: HashMap<String, Vec<String>> = HashMap::new();
    for m in modules {
        let mut names = Vec::new();
        for s in &m.statements {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                if !m.private_names.iter().any(|n| n == &fd.name) {
                    names.push(fd.name.clone());
                }
            }
        }
        module_function_exports.insert(m.name.clone(), names);
    }

    // Named-import map: `use { X, Y } from app.models` in the entry
    // (or in any module) lets bare `X(...)` calls resolve to
    // `app.models.X`. Gathered from both the entry AST and every
    // discovered module. Entry declarations win on collision —
    // matching the alias-map precedence above.
    let mut named_imports: HashMap<String, String> = HashMap::new();
    fn record_named_imports(
        out: &mut HashMap<String, String>,
        stmts: &[fai_compiler::ast::Statement],
        current_module_name: Option<&str>,
        module_function_exports: &HashMap<String, Vec<String>>,
        insert_policy: fn(&mut HashMap<String, String>, String, String),
    ) {
        for s in stmts {
            if let fai_compiler::ast::Statement::UseStatement(u) = s {
                let qualified_prefix =
                    qualify_module_path_for_codegen(current_module_name, &u.module_path);
                if u.import_all {
                    if fai_checker::std_modules::is_std_module(&u.module_path) {
                        let std_exports = fai_checker::std_modules::std_module_exports();
                        if let Some(exports) = std_exports.get(&qualified_prefix) {
                            for (n, _) in exports {
                                insert_policy(
                                    out,
                                    n.clone(),
                                    format!("{}.{}", qualified_prefix, n),
                                );
                            }
                        }
                    } else if let Some(names) = module_function_exports.get(&qualified_prefix) {
                        for n in names {
                            insert_policy(out, n.clone(), format!("{}.{}", qualified_prefix, n));
                        }
                    }
                } else if let Some(names) = &u.imported_names {
                    let qualified_prefix =
                        qualify_module_path_for_codegen(current_module_name, &u.module_path);
                    for n in names {
                        insert_policy(out, n.clone(), format!("{}.{}", qualified_prefix, n));
                    }
                }
            }
        }
    }
    record_named_imports(
        &mut named_imports,
        &ast.statements,
        None,
        &module_function_exports,
        |m, k, v| {
            m.insert(k, v);
        },
    );
    for m in modules {
        record_named_imports(
            &mut named_imports,
            &m.statements,
            Some(&m.name),
            &module_function_exports,
            |m, k, v| {
                m.entry(k).or_insert(v);
            },
        );
    }

    // Collect extern functions from the entry AST first, then from
    // every discovered module. Entry-file names win on collision so
    // the program's own extern block overrides one re-exported from
    // a dependency. Without this merge, `use { close } from Forsqlite`
    // in the entry compiles the wrapper's `sqlite3_close(...)` call
    // through `compile_call` — and `compile_call` looks the name up in
    // `extern_fn_indices`, which previously only saw the entry file.
    let mut extern_fn_indices = collect_extern_fn_indices_from(&ast.statements);
    OFFLOADABLE_EXTERNS.with(|m| *m.borrow_mut() = offloadable_extern_indices(&ast.statements));
    // Per-extern `is_out` flags per parameter. Needed so
    // `compile_extern_call` can emit the readback for OUT slots
    // after the host writes the C-returned pointer into guest
    // scratch memory.
    let mut extern_out_params: HashMap<String, Vec<bool>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
            for f in &ext.functions {
                extern_out_params
                    .insert(f.name.clone(), f.params.iter().map(|p| p.is_out).collect());
            }
        }
    }
    let mut next_idx = extern_fn_indices
        .values()
        .max()
        .map(|m| *m + 1)
        .unwrap_or(0);
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
                for f in &ext.functions {
                    extern_fn_indices.entry(f.name.clone()).or_insert_with(|| {
                        let idx = next_idx;
                        next_idx = next_idx.checked_add(1).expect("too many extern fns");
                        idx
                    });
                    extern_out_params
                        .entry(f.name.clone())
                        .or_insert_with(|| f.params.iter().map(|p| p.is_out).collect());
                }
            }
        }
    }

    // Collect `enum Name ... end` declarations from the entry AST and
    // every discovered module. Each enum keeps the member list in
    // declaration order; `Status.ready` lowers to the integer index
    // of `ready` in Status's member list (NaN-boxed). Equality of
    // two enum values reduces to integer equality.
    let mut enum_members: HashMap<String, Vec<String>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::EnumDeclaration(ed) = s {
            enum_members.insert(ed.name.clone(), ed.members.clone());
        }
    }
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::EnumDeclaration(ed) = s {
                enum_members
                    .entry(ed.name.clone())
                    .or_insert_with(|| ed.members.clone());
            }
        }
    }

    // Collect `type Name ... end` declarations from the entry AST and
    // every module. `Name(a: 1, b: 'x')` lowers to a dict literal
    // whose entries are `(field_name, supplied_value | default |
    // null-for-optional)` in declaration order.
    let mut type_fields: HashMap<String, Vec<fai_compiler::ast::FieldDeclaration>> = HashMap::new();
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::TypeDeclaration(td) = s {
            type_fields.insert(td.name.clone(), td.fields.clone());
        }
    }
    for m in modules {
        for s in &m.statements {
            if let fai_compiler::ast::Statement::TypeDeclaration(td) = s {
                type_fields
                    .entry(td.name.clone())
                    .or_insert_with(|| td.fields.clone());
            }
        }
    }
    // Built-in named types (Event, HttpRequest, RpcCall, etc.) live
    // in the checker's `type_fields` but never reached the codegen
    // here. Without this, `let x T = from_dict(d)` for a built-in T
    // falls through the expansion at `compile_let_statement` and
    // codegen reports `UnknownIdentifier("from_dict")`. User-declared
    // types of the same name still win — they were inserted above.
    for (name, fields) in builtin_type_fields() {
        type_fields.entry(name).or_insert(fields);
    }

    // Module-level constants — top-level `let NAME = <literal>`
    // bindings. Collected from the entry AST and every module so a
    // helper file in a dependency can define `SQLITE_OK = 0` and have
    // callers in any sibling file inline it at reference sites.
    // Non-literal initialisers are skipped (we don't run them).
    let mut module_constants: HashMap<String, fai_compiler::ast::Expression> = HashMap::new();
    fn is_literal_expr(e: &fai_compiler::ast::Expression) -> bool {
        use fai_compiler::ast::Expression::*;
        matches!(
            e,
            NumberExpression(_) | BooleanExpression(_) | NullExpression(_) | StringExpression(_)
        )
    }
    fn collect_module_consts(
        stmts: &[fai_compiler::ast::Statement],
        out: &mut HashMap<String, fai_compiler::ast::Expression>,
    ) {
        for s in stmts {
            if let fai_compiler::ast::Statement::LetStatement(ls) = s {
                if ls.bindings.len() == 1 && is_literal_expr(&ls.value) {
                    out.entry(ls.bindings[0].name.clone())
                        .or_insert_with(|| ls.value.clone());
                }
            }
        }
    }
    collect_module_consts(&ast.statements, &mut module_constants);
    for m in modules {
        collect_module_consts(&m.statements, &mut module_constants);
    }

    // Module-level `var NAME = EXPR` declarations. Each gets a
    // dedicated mutable wasm global (i64) appended after the four
    // fixed runtime globals; globals start at index 4. First-seen
    // wins so a helper module can declare `var timerId = 0` and a
    // peer file referencing `timerId` resolves to that slot.
    //
    // Initialisers are grouped by their source module so each runs
    // in its own module context — otherwise a sibling-module
    // initialiser like router.fai's `createSignal('/')` can't
    // resolve `createSignal` via its own `use { createSignal }
    // from signal` import when we compile it from a dependency
    // context.
    const MODULE_VAR_GLOBAL_BASE: u32 = 4;
    let mut module_vars: HashMap<String, u32> = HashMap::new();
    // Ordered list of (module_context, name, initialiser). None
    // context means the entry AST's own top-level vars.
    let mut module_var_inits: Vec<(Option<String>, String, fai_compiler::ast::Expression)> =
        Vec::new();
    {
        fn collect_mvars(
            stmts: &[fai_compiler::ast::Statement],
            ctx_mod: Option<&str>,
            map: &mut HashMap<String, u32>,
            inits: &mut Vec<(Option<String>, String, fai_compiler::ast::Expression)>,
            base: u32,
        ) {
            for s in stmts {
                if let fai_compiler::ast::Statement::VarStatement(vs) = s {
                    if vs.bindings.len() != 1 {
                        continue;
                    }
                    let name = &vs.bindings[0].name;
                    if map.contains_key(name) {
                        continue;
                    }
                    let idx = base + inits.len() as u32;
                    map.insert(name.clone(), idx);
                    inits.push((
                        ctx_mod.map(|s| s.to_string()),
                        name.clone(),
                        vs.value.clone(),
                    ));
                }
            }
        }
        collect_mvars(
            &ast.statements,
            None,
            &mut module_vars,
            &mut module_var_inits,
            MODULE_VAR_GLOBAL_BASE,
        );
        for m in modules {
            collect_mvars(
                &m.statements,
                Some(m.name.as_str()),
                &mut module_vars,
                &mut module_var_inits,
                MODULE_VAR_GLOBAL_BASE,
            );
        }
    }
    let module_var_count = module_var_inits.len() as u32;

    // Does the user supply a `main`? `<__start__>` calls it after
    // `<__module_init__>` when present; otherwise it just runs init
    // and returns VAL_VOID via the init-call's return value.
    let has_main = ast.statements.iter().any(|s| {
        matches!(
            s,
            fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main",
        )
    });

    // Synthesise the two wrapper functions. Names start with `<` so
    // the export loop below skips them — hosts only see `_start`.
    let loc_zero = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
    let mk_call_stmt = |name: &str| -> fai_compiler::ast::Statement {
        fai_compiler::ast::Statement::ExpressionStatement(fai_compiler::ast::ExpressionStatement {
            expression: fai_compiler::ast::Expression::CallExpression(
                fai_compiler::ast::CallExpression {
                    callee: Box::new(fai_compiler::ast::Expression::IdentifierExpression(
                        fai_compiler::ast::IdentifierExpression {
                            name: name.to_string(),
                            location: loc_zero.clone(),
                        },
                    )),
                    args: Vec::new(),
                    location: loc_zero.clone(),
                },
            ),
            location: loc_zero.clone(),
        })
    };
    // Group the initialisers by their module context so each module
    // gets its own compiled init function. Per-module init functions
    // are named `<__module_init__:{module_path}>` (entry-AST vars
    // go into `<__module_init__:>`). A master `<__module_init__>`
    // calls them in declaration order.
    let mut per_module_inits: Vec<(Option<String>, Vec<fai_compiler::ast::Statement>)> = Vec::new();
    for (ctx_mod, name, value) in &module_var_inits {
        let stmt = fai_compiler::ast::Statement::AssignmentStatement(
            fai_compiler::ast::AssignmentStatement {
                target: fai_compiler::ast::AssignmentTarget::Variables {
                    names: vec![name.clone()],
                },
                value: value.clone(),
                location: loc_zero.clone(),
            },
        );
        match per_module_inits
            .iter_mut()
            .find(|(existing, _)| existing == ctx_mod)
        {
            Some((_, stmts)) => stmts.push(stmt),
            None => per_module_inits.push((ctx_mod.clone(), vec![stmt])),
        }
    }
    let per_module_init_names: Vec<String> = per_module_inits
        .iter()
        .map(|(ctx_mod, _)| match ctx_mod {
            Some(m) => format!("<__module_init__:{}>", m),
            None => "<__module_init__:>".to_string(),
        })
        .collect();
    let per_module_init_decls: Vec<(fai_compiler::ast::FunctionDeclaration, Option<String>)> =
        per_module_inits
            .iter()
            .zip(per_module_init_names.iter())
            .map(|((ctx_mod, body), fn_name)| {
                let fd = fai_compiler::ast::FunctionDeclaration {
                    name: fn_name.clone(),
                    type_params: Vec::new(),
                    params: Vec::new(),
                    return_types: Vec::new(),
                    body: body.clone(),
                    doc: None,
                    is_private: None,
                    is_abstract: false,
                    is_remote: false,
                    location: loc_zero.clone(),
                    doc_comment: None,
                };
                (fd, ctx_mod.clone())
            })
            .collect();

    // Master `<__module_init__>` just dispatches to each per-module
    // init. Order matches `module_var_inits` — first-seen wins for
    // duplicate var names, same policy as global-index allocation.
    let master_init_body: Vec<fai_compiler::ast::Statement> = per_module_init_names
        .iter()
        .map(|n| mk_call_stmt(n))
        .collect();
    let module_init_fd = fai_compiler::ast::FunctionDeclaration {
        name: "<__module_init__>".to_string(),
        type_params: Vec::new(),
        params: Vec::new(),
        return_types: Vec::new(),
        body: master_init_body,
        doc: None,
        is_private: None,
        is_abstract: false,
        is_remote: false,
        location: loc_zero.clone(),
        doc_comment: None,
    };
    let mut start_body: Vec<fai_compiler::ast::Statement> = vec![mk_call_stmt("<__module_init__>")];
    if has_main && !is_test {
        start_body.push(mk_call_stmt("main"));
    }
    let start_fd = fai_compiler::ast::FunctionDeclaration {
        name: "<__start__>".to_string(),
        type_params: Vec::new(),
        params: Vec::new(),
        return_types: Vec::new(),
        body: start_body,
        doc: None,
        is_private: None,
        is_abstract: false,
        is_remote: false,
        location: loc_zero.clone(),
        doc_comment: None,
    };

    // Enumerate functions: synthesised wrappers first (so `_start`
    // points at `<__start__>`), then user `main`, then other
    // entry-AST top-level funcs, then each module's funcs prefixed
    // with the module path. Track each decl's module context so
    // unqualified peer calls resolve correctly.
    // Decls carry (function, ctx_module, ctx_file). The file path
    // is plumbed so the per-call-site keys (UFCS, named-param
    // reorder, expression types) can disambiguate by file —
    // otherwise two files in one module with calls at the same
    // (line, col) collide and codegen reads the wrong UFCS bit.
    let mut decls: Vec<(
        fai_compiler::ast::FunctionDeclaration,
        Option<String>,
        Option<String>,
    )> = Vec::new();
    decls.push((start_fd, None, None));
    decls.push((module_init_fd, None, None));
    for (fd, ctx_mod) in per_module_init_decls {
        decls.push((fd, ctx_mod, None));
    }
    let main = ast.statements.iter().find_map(|s| match s {
        fai_compiler::ast::Statement::FunctionDeclaration(fd) if fd.name == "main" => {
            Some(fd.clone())
        }
        _ => None,
    });
    if let Some(fd) = main {
        decls.push((fd, None, None));
    }
    for s in &ast.statements {
        if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
            if fd.name != "main" {
                decls.push((fd.clone(), None, None));
            }
        }
    }
    for m in modules {
        for (idx, s) in m.statements.iter().enumerate() {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = s {
                let mut prefixed = fd.clone();
                prefixed.name = format!("{}.{}", m.name, fd.name);
                let file = m.file_paths.get(idx).cloned().flatten();
                decls.push((prefixed, Some(m.name.clone()), file));
            }
        }
    }

    let infos: Vec<FunctionInfo> = decls
        .iter()
        .map(|(fd, ctx_mod, ctx_file)| FunctionInfo {
            name: fd.name.clone(),
            // Module functions carry their own file; entry-AST functions
            // (no module context, real location) fall back to the entry
            // file. Synthesised wrappers (line 0) stay file-less.
            source_file: ctx_file.clone().or_else(|| {
                if ctx_mod.is_none() && fd.location.line > 0 {
                    entry_file.map(String::from)
                } else {
                    None
                }
            }),
            source_line: fd.location.line,
            param_count: fd.params.len() as u16 + fd.type_params.len() as u16,
            type_param_count: fd.type_params.len() as u16,
            param_names: param_names_for(fd),
            include_in_coverage: fd.name != "main",
            param_defaults: param_defaults_for(fd),
        })
        .collect();

    // Fn-id index used by the spy/mock machinery. Keep the same
    // ordering as `infos` so the runtime table index lines up with
    // the function's position in the codegen output.
    let function_by_name: HashMap<String, u32> = infos
        .iter()
        .enumerate()
        .map(|(i, info)| (info.name.clone(), i as u32))
        .collect();

    // Walk every `test` block's body (entry + each module) to find
    // functions that get mock/spy-tracked. Only these functions need
    // the `spy_check_call` preamble — everything else pays zero cost.
    // In non-test builds the set is empty so no instrumentation
    // happens regardless of what the AST contains.
    let spy_targets: SpyTargets = if is_test {
        collect_spy_targets(
            ast,
            modules,
            &function_by_name,
            &module_aliases,
            &named_imports,
        )
    } else {
        SpyTargets::default()
    };
    let mocked_fn_ids = spy_targets.fn_ids;
    let std_method_fn_ids = spy_targets.std_method_fn_ids;

    let strings = RefCell::new(StringInterner::default());
    let ownership_sites = RefCell::new(Vec::new());
    let mut functions: Vec<(FunctionInfo, Function)> = Vec::with_capacity(decls.len());
    let mut closures: Vec<BuiltClosure> = Vec::new();
    for ((fd, ctx_mod, ctx_file), info) in decls.iter().zip(infos.iter().cloned()) {
        let result = build_function_with_spy_and_offset(
            fd,
            rt,
            &infos,
            checker,
            fai_func_type_indices,
            &module_aliases,
            &extern_fn_indices,
            import_remap,
            &strings,
            &enum_members,
            &type_fields,
            &named_imports,
            &mocked_fn_ids,
            &std_method_fn_ids,
            closures.len() as u32,
            ctx_mod.as_deref(),
            &module_constants,
            &extern_out_params,
            &module_vars,
            &ownership_sites,
            ctx_file.as_deref(),
            None,
        )?;
        functions.push((info, result.main));
        closures.extend(result.closures);
    }

    // Test-mode synthesis: one zero-arg wrapper per `(suite, case)`
    // pair. Each wrapper's body is
    // `setup ++ before_each ++ case.body ++ after_each`. The
    // dispatcher emitted by the module assembler routes
    // `_fai_run_test(suite_i, case_i)` to the matching wrapper.
    let mut test_cases: Vec<TestCaseEntry> = Vec::new();
    if is_test {
        // Collect TestDeclarations from entry AST and from all
        // modules. Module-scoped tests compile with their
        // module_context so unqualified calls resolve correctly.
        let mut test_specs: Vec<(
            &fai_compiler::ast::TestDeclaration,
            Option<String>,
            Option<String>,
        )> = Vec::new();
        for s in &ast.statements {
            if let fai_compiler::ast::Statement::TestDeclaration(td) = s {
                test_specs.push((td, None, None));
            }
        }
        for m in modules {
            for (idx, s) in m.statements.iter().enumerate() {
                if let fai_compiler::ast::Statement::TestDeclaration(td) = s {
                    let file = m.file_paths.get(idx).cloned().flatten();
                    test_specs.push((td, Some(m.name.clone()), file));
                }
            }
        }

        for (suite_idx, (td, ctx_mod, ctx_file)) in test_specs.iter().enumerate() {
            for (wrapper, case_idx) in crate::test_surface::suite_wrappers(td) {
                let info = FunctionInfo {
                    name: wrapper.name.clone(),
                    param_count: 0,
                    type_param_count: 0,
                    param_names: Vec::new(),
                    include_in_coverage: false,
                    param_defaults: Vec::new(),
                    source_line: wrapper.location.line,
                    ..Default::default()
                };
                let result = build_function_with_spy_and_offset(
                    &wrapper,
                    rt,
                    &infos,
                    checker,
                    fai_func_type_indices,
                    &module_aliases,
                    &extern_fn_indices,
                    import_remap,
                    &strings,
                    &enum_members,
                    &type_fields,
                    &named_imports,
                    &mocked_fn_ids,
                    &std_method_fn_ids,
                    closures.len() as u32,
                    ctx_mod.as_deref(),
                    &module_constants,
                    &extern_out_params,
                    &module_vars,
                    &ownership_sites,
                    ctx_file.as_deref(),
                    None,
                )?;
                let function_index = functions.len();
                functions.push((info, result.main));
                closures.extend(result.closures);
                test_cases.push(TestCaseEntry {
                    suite_name: td.name.clone(),
                    suite_idx: suite_idx as u16,
                    case_idx,
                    function_index,
                });
            }
        }
    }

    Ok(BuiltProgram {
        functions,
        closures,
        string_data: strings.into_inner().bytes,
        test_cases,
        module_var_count,
        ownership_sites: ownership_sites.into_inner(),
    })
}

pub(super) fn collect_module_aliases_from(
    current_module_name: Option<&str>,
    stmts: &[fai_compiler::ast::Statement],
) -> HashMap<String, String> {
    let mut aliases = HashMap::new();
    for s in stmts {
        if let fai_compiler::ast::Statement::UseStatement(u) = s {
            if u.import_all || u.imported_names.is_some() {
                continue;
            }
            if let Some(last) = u.module_path.last() {
                aliases.insert(
                    last.clone(),
                    qualify_module_path_for_codegen(current_module_name, &u.module_path),
                );
            }
        }
    }
    aliases
}

pub(super) fn qualify_module_path_for_codegen(current_module_name: Option<&str>, path: &[String]) -> String {
    if path.first().map(|s| s.as_str()) == Some("std") {
        return path.join(".");
    }
    let is_external = path
        .first()
        .map(|s| s.chars().next().map(|c| c.is_uppercase()).unwrap_or(false))
        .unwrap_or(false);
    if is_external {
        return path.join(".");
    }
    if let Some(current) = current_module_name {
        let package = current.split('.').next().unwrap_or(current);
        let is_package = package
            .chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false);
        if is_package {
            return format!("{}.{}", package, path.join("."));
        }
    }
    path.join(".")
}

pub(super) fn collect_extern_fn_indices_from(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, u16> {
    let mut indices = HashMap::new();
    let mut next = 0u16;
    for s in stmts {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
            for f in &ext.functions {
                indices.insert(f.name.clone(), next);
                next = next.checked_add(1).expect("too many extern fns");
            }
        }
    }
    indices
}

thread_local! {
    /// Extern functions whose blocking call is offloaded to the boundary
    /// (plan 101 U8): name → its `ext_fn_idx` (same numbering as
    /// `collect_extern_fn_indices_from`, since the host indexes `CURRENT_EXTERNS`
    /// by it). Populated per build; only scalar-signature externs are included
    /// (see `extern_is_offloadable`). Compile is single-threaded, so a
    /// thread-local mirrors the host's `CURRENT_EXTERNS`.
    pub(super) static OFFLOADABLE_EXTERNS: RefCell<HashMap<String, u16>> = RefCell::new(HashMap::new());
}

/// True if an extern's signature is all-scalar (Int/Float/Bool, no `out` params,
/// scalar-or-void return) — the v1 offload restriction, so the worker can build
/// its `Value`s from raw arg bits with no guest memory / pointer marshalling.
pub(crate) fn extern_is_offloadable(f: &fai_compiler::ast::ExternFunctionDecl) -> bool {
    // Word-sized arg types the offload path can pass: pointer handles are
    // resolved to raw addresses and strings to leaked C allocations on the main
    // thread before the worker runs the call. Float/Double args (separate FP
    // registers), out-params, and variadics are NOT offloaded — those externs
    // stay synchronous. The return may also be Float/Double/Void.
    let word_arg = |t: &fai_compiler::ast::TypeNode| {
        !t.is_array
            && !t.is_optional
            && matches!(
                t.name.as_deref(),
                Some("Int") | Some("Bool") | Some("String") | Some("Ptr") | Some("Pointer")
            )
    };
    let ret_ok = |t: &fai_compiler::ast::TypeNode| {
        !t.is_array
            && !t.is_optional
            && matches!(
                t.name.as_deref(),
                Some("Int")
                    | Some("Bool")
                    | Some("String")
                    | Some("Ptr")
                    | Some("Pointer")
                    | Some("Float")
                    | Some("Double")
                    | Some("Void")
            )
    };
    f.fixed_arg_count.is_none()
        && f.params.iter().all(|p| !p.is_out && word_arg(&p.type_node))
        && f.return_type.as_ref().is_none_or(ret_ok)
}

/// Map of offloadable extern name → `ext_fn_idx`, numbered identically to
/// `collect_extern_fn_indices_from` (every extern fn advances the counter; only
/// offloadable ones are inserted).
pub(super) fn offloadable_extern_indices(stmts: &[fai_compiler::ast::Statement]) -> HashMap<String, u16> {
    let mut out = HashMap::new();
    let mut next = 0u16;
    for s in stmts {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(ext) = s {
            for f in &ext.functions {
                let idx = next;
                next = next.checked_add(1).expect("too many extern fns");
                if extern_is_offloadable(f) {
                    out.insert(f.name.clone(), idx);
                }
            }
        }
    }
    out
}

/// If `expr` is a direct call to an offloadable extern, return its `ext_fn_idx`,
/// argument expressions, and location. Used to lower `let x = externCall(...)`
/// to a boundary suspension (`Term::AwaitFfi`).
pub(super) fn offloadable_extern_call_args(
    expr: &Expression,
) -> Option<(u16, Vec<&Expression>, &fai_compiler::ast::SourceLocation)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let Expression::IdentifierExpression(id) = &*call.callee else {
        return None;
    };
    let idx = OFFLOADABLE_EXTERNS.with(|m| m.borrow().get(&id.name).copied())?;
    Some((
        idx,
        call.args.iter().map(|a| &a.value).collect(),
        &call.location,
    ))
}

/// A spy target the mock/assert calls refer to.
#[derive(Debug, Clone)]
enum SpyTarget {
    /// A user-defined top-level function. `fn_id` is its index in
    /// the unified function table.
    UserFn(u32),
    /// A std module method (`cli.readLine`, `string.trim`, ...).
    /// The compiler-assigned `fn_id` is opaque — it just needs to
    /// match between the mock setup call and the module-call
    /// interception site.
    StdMethod(u32),
}

impl SpyTarget {
    fn fn_id(&self) -> u32 {
        match self {
            SpyTarget::UserFn(id) => *id,
            SpyTarget::StdMethod(fn_id) => *fn_id,
        }
    }
}

/// Resolve a spy-target expression. Tries user-function lookup
/// first; falls back to treating `alias.method` as a std-module
/// method reference when `alias` names a module and `method`
/// resolves through `resolve_module_call`. The caller supplies a
/// mutable `std_method_fn_ids` map — new std-method targets get a
/// fresh `fn_id` assigned lazily so the number space stays tight.
fn resolve_mock_target_full(
    expr: &fai_compiler::ast::Expression,
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
    std_method_fn_ids: &mut HashMap<(String, String), u32>,
    next_std_fn_id: &mut u32,
) -> Option<SpyTarget> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) => {
            if let Some(&p) = function_by_name.get(&id.name) {
                return Some(SpyTarget::UserFn(p));
            }
            if let Some(q) = named_imports.get(&id.name) {
                if let Some(&p) = function_by_name.get(q) {
                    return Some(SpyTarget::UserFn(p));
                }
            }
            None
        }
        Expression::MemberExpression(me) => {
            if let Expression::IdentifierExpression(obj) = &*me.object {
                if let Some(canonical) = module_aliases.get(&obj.name) {
                    let full = format!("{}.{}", canonical, me.property);
                    if let Some(&p) = function_by_name.get(&full) {
                        return Some(SpyTarget::UserFn(p));
                    }
                    if resolve_module_call(canonical, &me.property).is_some() {
                        let key = (canonical.clone(), me.property.clone());
                        let fn_id = *std_method_fn_ids.entry(key.clone()).or_insert_with(|| {
                            let id = *next_std_fn_id;
                            *next_std_fn_id += 1;
                            id
                        });
                        return Some(SpyTarget::StdMethod(fn_id));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Convenience wrapper — resolves to just the `fn_id` without
/// modifying the std-method table. Used by call sites that have
/// already collected the full set at compile-time via
/// `collect_spy_targets` and just need to look up an existing id.
pub(super) fn resolve_mock_target(
    expr: &fai_compiler::ast::Expression,
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
    std_method_fn_ids: &HashMap<(String, String), u32>,
) -> Option<u32> {
    use fai_compiler::ast::Expression;
    match expr {
        Expression::IdentifierExpression(id) => {
            if let Some(&p) = function_by_name.get(&id.name) {
                return Some(p);
            }
            if let Some(q) = named_imports.get(&id.name) {
                if let Some(&p) = function_by_name.get(q) {
                    return Some(p);
                }
            }
            None
        }
        Expression::MemberExpression(me) => {
            if let Expression::IdentifierExpression(obj) = &*me.object {
                if let Some(canonical) = module_aliases.get(&obj.name) {
                    let full = format!("{}.{}", canonical, me.property);
                    if let Some(&p) = function_by_name.get(&full) {
                        return Some(p);
                    }
                    if let Some(&id) =
                        std_method_fn_ids.get(&(canonical.clone(), me.property.clone()))
                    {
                        return Some(id);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Aggregate result of the compile-time spy-target scan.
#[derive(Debug, Default)]
pub(super) struct SpyTargets {
    /// Every `fn_id` referenced by a mock/assert target — the
    /// union of user-function ids and lazily-assigned std-method
    /// ids. Used by the preamble check and by every call-site
    /// interceptor to decide whether to route through the host.
    pub(super) fn_ids: HashSet<u32>,
    /// `(canonical_module, method_name) -> fn_id` for every std
    /// method that appeared as a mock target. The fn_id numbers
    /// are compile-time-unique; they start above the user
    /// function count so they never collide with user ids.
    pub(super) std_method_fn_ids: HashMap<(String, String), u32>,
}

/// Walk every `test` block in the entry AST and in each discovered
/// module; collect spy targets (user functions and std-method
/// references) that get mocked or asserted on. Only functions in
/// `fn_ids` get the spy preamble; only module calls whose
/// `(canonical, method)` appears in `std_method_fn_ids` get
/// wrapped with a spy check at their call site.
pub(super) fn collect_spy_targets(
    ast: &fai_compiler::ast::Program,
    modules: &[fai_compiler::compiler::DiscoveredModule],
    function_by_name: &HashMap<String, u32>,
    module_aliases: &HashMap<String, String>,
    named_imports: &HashMap<String, String>,
) -> SpyTargets {
    let mut out = SpyTargets::default();
    // Std-method fn_ids start after the last user fn_id so the
    // `fn_ids` set can treat both alike (the host side doesn't
    // care about the origin).
    let mut next_std_fn_id: u32 = function_by_name.len() as u32;
    fn walk_expr(
        expr: &fai_compiler::ast::Expression,
        fbn: &HashMap<String, u32>,
        aliases: &HashMap<String, String>,
        imports: &HashMap<String, String>,
        out: &mut SpyTargets,
        next_std_fn_id: &mut u32,
    ) {
        use fai_compiler::ast::Expression;
        if let Expression::CallExpression(ce) = expr {
            if let Some(target_name) = mock_target_name(&ce.callee) {
                if is_spy_call(&target_name) {
                    if let Some(first) = ce.args.first() {
                        if let Some(target) = resolve_mock_target_full(
                            &first.value,
                            fbn,
                            aliases,
                            imports,
                            &mut out.std_method_fn_ids,
                            next_std_fn_id,
                        ) {
                            out.fn_ids.insert(target.fn_id());
                        }
                    }
                }
            }
            for a in &ce.args {
                walk_expr(&a.value, fbn, aliases, imports, out, next_std_fn_id);
            }
            walk_expr(&ce.callee, fbn, aliases, imports, out, next_std_fn_id);
        }
    }
    fn mock_target_name(callee: &fai_compiler::ast::Expression) -> Option<String> {
        use fai_compiler::ast::Expression;
        match callee {
            Expression::IdentifierExpression(id) => Some(id.name.clone()),
            Expression::MemberExpression(me) => {
                if let Expression::IdentifierExpression(obj) = &*me.object {
                    Some(format!("{}.{}", obj.name, me.property))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
    fn is_spy_call(name: &str) -> bool {
        matches!(
            name,
            "mock"
                | "mockOnce"
                | "mockReset"
                | "assert.calledWith"
                | "assert.callCount"
                | "assert.notCalled"
        )
    }

    fn scan_test_stmts(
        stmts: &[fai_compiler::ast::Statement],
        fbn: &HashMap<String, u32>,
        aliases: &HashMap<String, String>,
        imports: &HashMap<String, String>,
        out: &mut SpyTargets,
        next_std_fn_id: &mut u32,
    ) {
        use fai_compiler::ast::Statement;
        for s in stmts {
            match s {
                Statement::ExpressionStatement(es) => {
                    walk_expr(&es.expression, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::LetStatement(ls) => {
                    walk_expr(&ls.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::VarStatement(vs) => {
                    walk_expr(&vs.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::AssignmentStatement(a) => {
                    walk_expr(&a.value, fbn, aliases, imports, out, next_std_fn_id);
                }
                Statement::IfStatement(is_stmt) => {
                    for b in &is_stmt.branches {
                        walk_expr(&b.condition, fbn, aliases, imports, out, next_std_fn_id);
                        scan_test_stmts(&b.body, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(e) = &is_stmt.else_branch {
                        scan_test_stmts(e, fbn, aliases, imports, out, next_std_fn_id);
                    }
                }
                Statement::TestDeclaration(td) => {
                    scan_test_stmts(&td.setup, fbn, aliases, imports, out, next_std_fn_id);
                    if let Some(b) = &td.before_all {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.before_each {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    for c in &td.cases {
                        scan_test_stmts(&c.body, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.after_each {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                    if let Some(b) = &td.after_all {
                        scan_test_stmts(b, fbn, aliases, imports, out, next_std_fn_id);
                    }
                }
                _ => {}
            }
        }
    }
    scan_test_stmts(
        &ast.statements,
        function_by_name,
        module_aliases,
        named_imports,
        &mut out,
        &mut next_std_fn_id,
    );
    for m in modules {
        scan_test_stmts(
            &m.statements,
            function_by_name,
            module_aliases,
            named_imports,
            &mut out,
            &mut next_std_fn_id,
        );
    }
    out
}

/// Max `FaiFunc(N)` arity the module assembler pre-allocates type
/// slots for. Covers top-level functions and closures. The checker
/// rejects declarations over this limit (`fai_checker::MAX_FUNCTION_ARITY`
/// is the single source of truth), so overflow here means a declaration
/// bypassed the checker — an internal invariant violation.
pub const MAX_DIRECT_ARITY: u16 = fai_checker::MAX_FUNCTION_ARITY as u16;

/// Compute the `FaiFunc(N) → type_index` map the builder expects.
/// Type indices are a function of the type-section layout (which
/// lists every import's type, then every runtime helper's type,
/// then the pre-allocated `FaiFunc(0..=MAX)` slots). That layout is
/// independent of target-filtering, so this doesn't need the
/// target. Exposed so callers that drive `build_program` outside
/// of `assemble_wasm_module` can share the same mapping.
pub fn direct_fai_func_type_indices() -> HashMap<u16, u32> {
    let import_count = crate::runtime::import_signatures().len() as u32;
    let rt_count = crate::runtime::type_signatures().len() as u32;
    let base = import_count + rt_count;
    (0..=MAX_DIRECT_ARITY)
        .map(|n| (n, base + n as u32))
        .collect()
}

/// Runtime-helper base index for a given target. The direct
/// builder's `Call(rt.base + RT_*)` instructions target this slot.
/// Depends on the post-filter import count, since unavailable
/// imports don't take up function-index slots.
pub fn direct_rt_base_for_target(target: Option<&str>) -> u32 {
    direct_rt_base_for_target_with_test_flag(target, true)
}

/// Same as [`direct_rt_base_for_target`] but honours `is_test` so
/// the runtime base stays in sync with the import section when
/// spy/mock imports are stripped from non-test builds.
pub fn direct_rt_base_for_target_with_test_flag(target: Option<&str>, is_test: bool) -> u32 {
    let avail = crate::runtime::available_imports_with_test_flag(target, is_test);
    let (_, actual) = crate::runtime::build_import_remap(&avail);
    actual
}

/// Backwards-compatible wrapper — equivalent to
/// `direct_rt_base_for_target(None)`. Callers that never set a
/// target can keep using this.
pub fn direct_rt_base() -> u32 {
    direct_rt_base_for_target(None)
}

/// Assemble a standalone wasm module from a `BuiltProgram`. The
/// layout matches the test infrastructure's `build_module`:
///
/// - **Types:** host imports (always all declared, even unavailable
///   ones), runtime helpers, `FaiFunc(0..=MAX)`.
/// - **Imports:** host imports filtered by `target` — unavailable
///   ones are excluded (e.g., `http_server_*` under `wasm-html`).
/// - **Functions:** runtime helpers, top-level user fns, closures.
/// - **Table:** funcref, populated with closure func indices.
/// - **Memory:** 16 pages min, grown as needed.
/// - **Globals:** `__heap_ptr` (starts above string data, 8-aligned),
///   `__env_ptr`, `error_flag`, `error_value`.
/// - **Exports:** `_start` → function index for `main` (functions\[0\]),
///   `memory`.
/// - **Elements:** table slot `i` → closure `i`'s function index.
/// - **Data:** string pool at offset 0.
///
/// `target` matches the bytecode path's `target` parameter — `None`
/// for native runs, `Some("wasm-html")` or `Some("wasm")` for
/// browser/headless builds that disable the server-side HTTP
/// imports. Callers must pass the same target to `build_program`
/// (via its `import_remap`) so the emitted code's import indices
/// agree with what the module declares.
///
/// Remaining limitations: fixed 16-page memory minimum rather than
/// derived from program size; no test-runner dispatcher (so `fai
/// test` still needs the bytecode path); no user-named top-level
/// function exports.
pub fn assemble_wasm_module(program: &BuiltProgram, target: Option<&str>) -> Vec<u8> {
    assemble_wasm_module_with_test_flag(program, target, true)
}

/// Same as [`assemble_wasm_module`] but gates spy/mock imports on
/// `is_test`. Non-test builds strip the `spy_*` imports so the
/// resulting wasm instantiates against a minimal host (e.g. the
/// native-binary runner that doesn't install the test framework).
pub fn assemble_wasm_module_with_test_flag(
    program: &BuiltProgram,
    target: Option<&str>,
    is_test: bool,
) -> Vec<u8> {
    use wasm_encoder::{
        CodeSection, ConstExpr, DataSection, ElementSection, Elements, EntityType, ExportKind,
        ExportSection, FunctionSection, GlobalSection, GlobalType, ImportSection, MemorySection,
        MemoryType, Module as EncModule, RefType, TableSection, TableType, TypeSection,
    };

    let fai_type_indices = direct_fai_func_type_indices();
    let import_available = crate::runtime::available_imports_with_test_flag(target, is_test);
    let (import_remap, actual_import_count) = crate::runtime::build_import_remap(&import_available);

    let mut module = EncModule::new();

    // ── types ──
    // Every import's type is declared regardless of availability —
    // it's harmless to have unused type entries, and keeping them
    // stable simplifies the offsets the builder bakes into its
    // instructions.
    let mut types = TypeSection::new();
    let import_sigs = crate::runtime::import_signatures();
    let mut import_type_indices = Vec::with_capacity(import_sigs.len());
    for (_, params, results) in &import_sigs {
        import_type_indices.push(types.len());
        types.ty().function(params.clone(), results.clone());
    }
    let rt_sigs = crate::runtime::type_signatures();
    let mut rt_type_indices = Vec::with_capacity(rt_sigs.len());
    for (params, results) in &rt_sigs {
        rt_type_indices.push(types.len());
        types.ty().function(params.clone(), results.clone());
    }
    for arity in 0..=MAX_DIRECT_ARITY {
        let params: Vec<ValType> = (0..arity).map(|_| ValType::I64).collect();
        let expected = types.len();
        types.ty().function(params, vec![ValType::I64]);
        assert_eq!(
            expected, fai_type_indices[&arity],
            "type layout out of sync with direct_fai_func_type_indices",
        );
    }
    // Reserve a type for the `_fai_run_test(suite_i: i32,
    // case_i: i32) -> ()` dispatcher when test cases are present.
    // Always appending it (even when empty) would waste a slot,
    // so the type is conditional on `test_cases`.
    let test_runner_type_idx: Option<u32> = if program.test_cases.is_empty() {
        None
    } else {
        let idx = types.len();
        types
            .ty()
            .function(vec![ValType::I32, ValType::I32], vec![]);
        Some(idx)
    };
    module.section(&types);

    // ── imports ──
    // Only available imports are declared. Unavailable ones (e.g.,
    // `http_server_*` under `wasm-html`) are skipped; callers that
    // tried to reach them landed on `unreachable` via
    // `emit_import_call`.
    let mut imports = ImportSection::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if import_available[i] {
            imports.import("env", name, EntityType::Function(import_type_indices[i]));
        }
    }
    module.section(&imports);

    // ── functions ──
    let mut funcs = FunctionSection::new();
    for &t in &rt_type_indices {
        funcs.function(t);
    }
    for (info, _) in &program.functions {
        let t = *fai_type_indices.get(&info.param_count).unwrap_or_else(|| {
            panic!(
                "arity {} for `{}` exceeds MAX_DIRECT_ARITY",
                info.param_count, info.name,
            )
        });
        funcs.function(t);
    }
    for c in &program.closures {
        let t = *fai_type_indices
            .get(&c.info.param_count)
            .unwrap_or_else(|| {
                panic!(
                    "closure arity {} exceeds MAX_DIRECT_ARITY",
                    c.info.param_count,
                )
            });
        funcs.function(t);
    }
    // Test runner dispatcher (when present) sits at the very end
    // of the function section — its wasm function index is
    // `top_level_base + functions.len() + closures.len()`.
    if let Some(t) = test_runner_type_idx {
        funcs.function(t);
    }
    module.section(&funcs);

    // ── tables ──
    let mut tables = TableSection::new();
    let closure_count = program.closures.len() as u32;
    tables.table(TableType {
        element_type: RefType::FUNCREF,
        minimum: closure_count as u64,
        maximum: Some(closure_count as u64),
        table64: false,
        shared: false,
    });
    module.section(&tables);

    // Append the known literal strings ("null", "true", "false")
    // after the user string pool so `RT_VALUE_TO_STR` can produce
    // them when stringifying `null` / Bool values. Without this
    // every stringification of those values reads `(0, 0)` and
    // emits the empty string.
    let mut extended = program.string_data.clone();
    fn append_known(buf: &mut Vec<u8>, s: &str) -> (u32, u32) {
        let off = buf.len() as u32;
        buf.extend_from_slice(s.as_bytes());
        (off, s.len() as u32)
    }
    let str_null = append_known(&mut extended, "null");
    let str_true = append_known(&mut extended, "true");
    let str_false = append_known(&mut extended, "false");
    let known = crate::runtime::KnownStrings {
        str_null,
        str_true,
        str_false,
        ..Default::default()
    };

    // ── memory ──
    //
    // Size: string data + 64 KiB scratch, rounded up to the next
    // page, with a 16-page (1 MiB) minimum so small programs have
    // room for heap growth. Matches `module.rs::emit_memory_section`
    // for parity — programs compiled through either path see
    // identical starting memory.
    let total_bytes = extended.len() as u32 + crate::runtime::FREE_BUCKET_REGION_BYTES + 65536;
    let pages = std::cmp::max((total_bytes / 65536) + 1, 16);
    let mut mem = MemorySection::new();
    mem.memory(MemoryType {
        minimum: pages as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    module.section(&mem);

    // ── globals ──
    // The size-bucketed free-list heads live in a zero-init region starting at
    // `bucket_base`; the heap bump pointer starts just past it.
    let bucket_base = ((extended.len() as u32) + 7) & !7;
    let heap_start = (bucket_base + crate::runtime::FREE_BUCKET_REGION_BYTES + 7) & !7;
    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(heap_start as i32),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    globals.global(
        GlobalType {
            val_type: ValType::I64,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i64_const(0),
    );
    // Module-var globals. Initialised to VAL_NULL so any read-before-
    // init observes NaN-boxed null rather than a bit-pattern 0, which
    // wouldn't round-trip through the runtime's type checks. The
    // `<__module_init__>` function emitted by the codegen writes the
    // user-supplied initialiser into each slot at program start.
    for _ in 0..program.module_var_count {
        globals.global(
            GlobalType {
                val_type: ValType::I64,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i64_const(crate::runtime::VAL_NULL),
        );
    }
    // Heap free-list head for rt_alloc reuse / rt_free, appended last so the
    // fixed (0-3) and module-var (4..) global indices are unchanged. 0 = empty.
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    // Live-object counter (plan 113): incremented in rt_alloc, decremented in
    // rt_free. The leak oracle reads it at program exit. Appended after the
    // free-list so earlier indices are unchanged. 0 = no live objects yet.
    globals.global(
        GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        },
        &ConstExpr::i32_const(0),
    );
    module.section(&globals);

    // Function indices use the POST-filter import count so they
    // agree with the import section above. This has to match what
    // `direct_rt_base_for_target(target)` returned when the program
    // was built — callers that drive `build_program` directly must
    // pass the same target.
    let top_level_base = actual_import_count + crate::runtime::RT_COUNT;
    let closure_base = top_level_base + program.functions.len() as u32;
    let main_func_idx = top_level_base;
    let test_runner_func_idx = closure_base + closure_count;

    // ── exports ──
    //
    // Parity with `module.rs::emit_export_section`. Host tooling
    // grabs `__heap_ptr` / `__env_ptr` /
    // `__indirect_function_table` to call closures from JS and to
    // inspect the NaN-boxed heap; named top-level functions are
    // exported so callbacks can reach them by name. When test
    // cases are present, `_fai_run_test` sits after all closures.
    let mut exports = ExportSection::new();
    exports.export("_start", ExportKind::Func, main_func_idx);
    exports.export("memory", ExportKind::Memory, 0);
    if closure_count > 0 {
        exports.export("__indirect_function_table", ExportKind::Table, 0);
    }
    // Host-callable refcount helpers: host registries retain guest handles
    // they store and release them on unregister/reset/drop. The async
    // assembler exports these too; without release, sync-built servers leak
    // request graphs, and without retain, browser host registries cannot mirror
    // native ownership safely (plans 116/117).
    exports.export(
        "__fai_retain",
        ExportKind::Func,
        actual_import_count + RT_RETAIN,
    );
    exports.export(
        "__fai_release",
        ExportKind::Func,
        actual_import_count + RT_RELEASE,
    );
    exports.export("__heap_ptr", ExportKind::Global, 0);
    // Live-object counter (plan 113) — the host leak oracle reads this by name
    // after a run. Index = free-list (4 + module vars) + 1.
    exports.export(
        "__live_objects",
        ExportKind::Global,
        5 + program.module_var_count,
    );
    // Heap overflow free-list head — post-mortem heap stats walk it.
    exports.export(
        "__free_list",
        ExportKind::Global,
        4 + program.module_var_count,
    );
    exports.export("__env_ptr", ExportKind::Global, 1);
    exports.export("__error_flag", ExportKind::Global, GLOBAL_ERROR_FLAG);
    exports.export("__error_value", ExportKind::Global, GLOBAL_ERROR_VALUE);
    if test_runner_type_idx.is_some() {
        exports.export("_fai_run_test", ExportKind::Func, test_runner_func_idx);
    }
    let mut exported_names = std::collections::HashSet::new();
    for (i, (info, _)) in program.functions.iter().enumerate() {
        let name = &info.name;
        if name.is_empty() || name.starts_with('<') || exported_names.contains(name) {
            continue;
        }
        let func_idx = top_level_base + i as u32;
        exports.export(name, ExportKind::Func, func_idx);
        exported_names.insert(name.clone());
    }
    module.section(&exports);

    // ── elements ──
    if closure_count > 0 {
        let mut elements = ElementSection::new();
        let func_indices: Vec<u32> = (0..closure_count).map(|i| closure_base + i).collect();
        elements.active(
            Some(0),
            &ConstExpr::i32_const(0),
            Elements::Functions(func_indices.into()),
        );
        module.section(&elements);
    }

    // ── code ──
    //
    // Runtime helpers see the same `import_remap` the builder used,
    // so their internal `emit_import_call(IMPORT_X, remap)` lands
    // on the matching post-filter wasm index (or `unreachable` for
    // unavailable imports under `wasm-html`, matching the bytecode
    // path's behaviour).
    let mut code = CodeSection::new();
    let freelist_global = 4 + program.module_var_count; // appended after fixed+module-var globals
    let live_count_global = freelist_global + 1; // appended after the free-list
    for f in crate::runtime::emit_all(
        actual_import_count,
        &import_remap,
        &known,
        freelist_global,
        live_count_global,
        bucket_base,
    ) {
        code.function(&f);
    }
    for (_, f) in &program.functions {
        code.function(f);
    }
    for c in &program.closures {
        code.function(&c.function);
    }
    // Test runner dispatcher body: `_fai_run_test(suite_i,
    // case_i) -> ()`. For each (suite, case) entry, emit:
    //     if suite_i == s && case_i == c: call wrapper; return
    // Unknown (suite, case) traps via `unreachable`. The CLI
    // test runner reads out the trap and records a failure.
    if test_runner_type_idx.is_some() {
        use wasm_encoder::{BlockType, Function, Instruction};
        let mut dispatcher = Function::new([]);
        for entry in &program.test_cases {
            dispatcher.instruction(&Instruction::LocalGet(0)); // suite_i
            dispatcher.instruction(&Instruction::I32Const(entry.suite_idx as i32));
            dispatcher.instruction(&Instruction::I32Eq);
            dispatcher.instruction(&Instruction::LocalGet(1)); // case_i
            dispatcher.instruction(&Instruction::I32Const(entry.case_idx as i32));
            dispatcher.instruction(&Instruction::I32Eq);
            dispatcher.instruction(&Instruction::I32And);
            dispatcher.instruction(&Instruction::If(BlockType::Empty));
            let wasm_idx = top_level_base + entry.function_index as u32;
            dispatcher.instruction(&Instruction::Call(wasm_idx));
            // The wrapper returns i64 (like any fai function).
            // Test runner is `-> ()`, so drop the result.
            dispatcher.instruction(&Instruction::Drop);
            dispatcher.instruction(&Instruction::Return);
            dispatcher.instruction(&Instruction::End);
        }
        dispatcher.instruction(&Instruction::Unreachable);
        dispatcher.instruction(&Instruction::End);
        code.function(&dispatcher);
    }
    module.section(&code);

    // ── data ──
    if !extended.is_empty() {
        let mut data = DataSection::new();
        data.active(0, &ConstExpr::i32_const(0), extended.iter().copied());
        module.section(&data);
    }

    // ── debug metadata (plan 116): name section + fai-dbg table ──
    let mut dbg: Vec<crate::debug_info::FnDebugEntry> = Vec::new();
    for (i, (name, _, _)) in import_sigs.iter().enumerate() {
        if let Some(idx) = import_remap.get(i).copied().flatten() {
            dbg.push(crate::debug_info::FnDebugEntry::unlocated(idx, *name));
        }
    }
    for (k, n) in crate::runtime::rt_fn_names().iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            actual_import_count + k as u32,
            *n,
        ));
    }
    for (i, (info, _)) in program.functions.iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry {
            index: top_level_base + i as u32,
            name: info.name.clone(),
            file: info.source_file.clone(),
            line: info.source_line,
        });
    }
    for (i, c) in program.closures.iter().enumerate() {
        dbg.push(crate::debug_info::FnDebugEntry {
            index: closure_base + i as u32,
            name: c.info.name.clone(),
            file: c.info.source_file.clone(),
            line: c.info.source_line,
        });
    }
    if test_runner_type_idx.is_some() {
        dbg.push(crate::debug_info::FnDebugEntry::unlocated(
            test_runner_func_idx,
            "_fai_run_test",
        ));
    }
    crate::debug_info::append_debug_sections(
        &mut module,
        &dbg,
        &crate::debug_info::DbgMeta {
            bucket_base: Some(bucket_base),
            bucket_count: crate::runtime::NUM_FREE_BUCKETS,
            ownership_sites: program.ownership_sites.clone(),
        },
    );

    module.finish()
}
