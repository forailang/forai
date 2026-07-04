use crate::*;

pub(crate) fn step_check(args: &[String], reporter: &Reporter) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));
    if let Some(path) = file_arg {
        // Check a single specified file
        check_single_file(path, reporter);
        return;
    }

    // No file given — find project from fai.toml in cwd.
    // Skip silently if no standard project found (e.g. workspace root
    // with no [project] section) — the build step handles those itself.
    let (project_root, src_dir) = match find_project_source_from_cwd() {
        Some(r) => r,
        None => return,
    };

    match run_project_check(&project_root, &src_dir) {
        Ok(()) => reporter.step(StepStatus::Ok, "check", "no type errors"),
        Err((msg, n)) => {
            reporter.error_line(&msg);
            let n = n.max(1);
            reporter.step(
                StepStatus::Fail,
                "check",
                &format!("{} type error{}", n, if n == 1 { "" } else { "s" }),
            );
            std::process::exit(1);
        }
    }
}

/// Decide which check strategy applies to a project and run it.
/// Returns the joined error message when any type errors surface.
///
/// Mirrors `step_check`'s decision tree but as a pure function: no
/// reporter side-effects, no `process::exit`. That makes the decision
/// logic unit-testable — `step_check` keeps the
/// reporter/exit-on-error glue.
///
/// Three strategies, picked in order:
/// 1. **Single-entry project** (e.g. `src/main.fai` exists): check the
///    entry, which transitively pulls in every reachable module.
/// 2. **Multi-target project** (`[project.X]` sub-projects in
///    `fai.toml`): check each sub-project's `main` in turn. Mirrors
///    `step_test`'s sub-project loop. Without this branch, a project
///    with nested-only `src/` (e.g. `src/auth/`, `src/data/` and no
///    top-level `.fai`) would fall through to the flat-library path
///    below and silently check nothing.
/// 3. **Flat library project**: load every `.fai` file at the source
///    root into a single module and check it.
pub(crate) fn run_project_check(project_root: &std::path::Path, src_dir: &str) -> Result<(), (String, usize)> {
    if let Some(entry) = resolve_entry_point(project_root, src_dir) {
        return try_check_single_file(&entry.to_string_lossy());
    }

    // Multi-target project: each [project.<name>] has its own entry,
    // and the source root may be nested-only (e.g. forui-fullstack,
    // where src/ contains only `auth/`, `data/`, `pages/`, …, with no
    // top-level .fai files). Check each sub-project's main in turn so
    // every reachable module gets walked. Mirrors the sub-project loop
    // step_test runs for the same reason.
    let toml = std::fs::read_to_string(project_root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);
    if !info.sub_projects.is_empty() {
        let mut names: Vec<&String> = info.sub_projects.keys().collect();
        names.sort();
        for name in names {
            let sub = &info.sub_projects[name];
            let Some(main) = &sub.main else {
                continue;
            };
            let main_path = project_root.join(main);
            if !main_path.exists() {
                continue;
            }
            try_check_single_file(&main_path.to_string_lossy())?;
        }
        return Ok(());
    }

    let src_path = project_root.join(src_dir);
    let prepared = fai_compiler::prepare_module_directory_for_tests(&src_path.to_string_lossy())
        .map_err(|e| (e, 1))?;
    let mut checker = fai_checker::Checker::new();
    // Plan 132: literal secrets.get names must come from the manifest.
    if let Some(secrets) = &info.secrets {
        checker.set_declared_secrets(
            secrets
                .declarations
                .iter()
                .map(|d| d.name.clone())
                .collect(),
        );
    }
    let prepared_modules: Vec<fai_checker::PreparedModule> = prepared
        .modules
        .iter()
        .map(|m| fai_checker::PreparedModule {
            name: m.name.clone(),
            statements: m.statements.clone(),
            file_paths: m.file_paths.clone(),
            private_names: m.private_names.clone(),
            file_path: None,
        })
        .collect();
    let result = checker.check_with_modules(&prepared.serde_ast.statements, &prepared_modules);
    for w in &checker.warnings {
        eprintln!("{}", w);
    }
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err((
            format_check_errors(&checker, &e),
            checker.collected_errors.len().max(1),
        )),
    }
}

pub(crate) fn check_single_file(path: &str, reporter: &Reporter) {
    match try_check_single_file(path) {
        Ok(()) => reporter.step(StepStatus::Ok, "check", "no type errors"),
        Err((msg, n)) => {
            reporter.error_line(&msg);
            let n = n.max(1);
            reporter.step(
                StepStatus::Fail,
                "check",
                &format!("{} type error{}", n, if n == 1 { "" } else { "s" }),
            );
            std::process::exit(1);
        }
    }
}

/// Check a single file and return the result. Extracted for testability.
///
/// Mirrors the same source pre-processing that `step_build` applies so the
/// checker sees the same code that actually gets compiled:
/// 1. Injects peer-hash and RPC dispatch so server-generated names like
///    `addRpcRoutes` are visible (without this the checker reports
///    "Unknown name 'addRpcRoutes'").
/// 2. Generates RPC proxy synthetic modules so their names land in
///    `synthetic_names`, preventing spurious "cannot read module directory"
///    errors when the compiler would otherwise follow `from Server` imports
///    into the server source tree.
/// 3. Uses `prepare_source_with_synthetic` so that external package
///    dependencies declared in fai.toml are fully resolved (the old
///    `prepare_module_directory` path did not do this, causing
///    "Unknown type 'X'" errors for any type from an external package).
pub(crate) fn try_check_single_file(path: &str) -> Result<(), (String, usize)> {
    let mut content =
        std::fs::read_to_string(path).map_err(|e| (format!("cannot read '{}': {}", path, e), 1))?;
    let source_root = find_source_root(path);
    let info = read_project_info_full(source_root.as_deref());
    inject_peer_hash(
        &mut content,
        &info,
        source_root.as_deref(),
        /* verbose = */ false,
    );
    inject_rpc_dispatch(&mut content, &info, source_root.as_deref(), Some(path));
    let synthetic_modules = generate_rpc_proxy_modules(source_root.as_deref());
    // Use the test-mode prepare so test bodies survive into the AST the
    // checker walks. Without this, `fai check` skips test bodies and
    // misses real type errors there — the user only finds out at
    // `fai test` time, where the codegen surfaces them as confusing
    // "internal error: UnknownIdentifier" failures instead of clean
    // checker diagnostics.
    let prepared = fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
        &content,
        source_root.as_deref(),
        synthetic_modules,
        Some(path),
    )
    .map_err(|e| (e.to_string(), 1))?;
    let mut checker = fai_checker::Checker::new();
    // Plan 132: literal secrets.get names must come from the manifest.
    if let Some(secrets) = &info.secrets {
        checker.set_declared_secrets(
            secrets
                .declarations
                .iter()
                .map(|d| d.name.clone())
                .collect(),
        );
    }
    let result = run_checker(&mut checker, &prepared);
    // Plan 133 phase 5: surface checker warnings (e.g. a public remote
    // def reaching a secrets API) on stderr. Non-fatal — the check still
    // passes; they read as `warning: ...` so tooling can grep them.
    for w in &checker.warnings {
        eprintln!("{}", w);
    }
    match result {
        Ok(()) => Ok(()),
        Err(e) => Err((
            format_check_errors(&checker, &e),
            checker.collected_errors.len().max(1),
        )),
    }
}
