//! forai CLI — the command-line interface for the FAI language.
//!
//! Usage:
//!   forai run <file.fai>           # parse and run directly
//!   forai test <file.fai>          # run tests
//!   forai check <file.fai>         # type-check only
//!   forai fmt <file.fai> [--check] # format source code
//!   forai build [target]           # compile to wasm/native/browser output
//!   forai doc [query]              # browse language and stdlib docs
//!   forai new <name>               # create new project
//!   forai interface <file.fai>     # emit interface JSON
//!   forai mcp                      # run the MCP server
//!   forai <file.fai>               # shorthand for run

use std::env;

mod doc;
mod format;
pub mod interface;
mod mcp;
mod report;
pub mod rpc_dispatch;
pub mod rpc_proxy;
mod test_meta;
mod wasm_runner;

use report::{count_fai_files_recursive, extract_verbose_flag, is_verbose, Reporter, StepStatus};

/// Magic marker appended at the end of self-extracting native
/// binaries produced by `forai build --target native` / `target =
/// "native"`. Layout of a native binary:
///
///   [ forai ELF/... binary ] [ wasm bytes ] [ NATIVE_TRAILER_MAGIC ] [ wasm_len u64 LE ]
///
/// On startup, `cli_main` reads the last 16 bytes of `argv[0]`; if
/// the magic matches, the preceding `wasm_len` bytes are the embedded
/// wasm and we dispatch to `wasm_runner::run_wasm` instead of the
/// normal CLI. See plan 99 Phase 3.
const NATIVE_TRAILER_MAGIC: &[u8; 8] = b"FAIBUND\0";

pub fn cli_main() {
    // Self-extract path: if this forai binary has a wasm bundled at
    // its tail, run it and exit. The normal CLI never sees argv.
    if let Some(wasm) = read_embedded_wasm() {
        // Forward any argv after the program name as wasm args —
        // individual programs that want to consult them can read
        // them via std.cli::args (not implemented yet; parked until
        // a real user needs it).
        if let Err(e) = wasm_runner::run_wasm(&wasm) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    let first = &args[1];

    match first.as_str() {
        "check" => cmd_check(&args[2..]),
        "test" => cmd_test(&args[2..]),
        "run" => cmd_run(&args[2..]),
        "build" => cmd_build(&args[2..]),
        "interface" => cmd_interface(&args[2..]),
        "fmt" => cmd_fmt(&args[2..]),
        "new" => cmd_new(&args[2..]),
        "doc" => cmd_doc(&args[2..]),
        "mcp" => mcp::cmd_mcp(),
        "--help" | "-h" | "help" => {
            print_usage();
        }
        _ => {
            if first.ends_with(".fai") || first.ends_with(".json") {
                cmd_run(&args[1..]);
            } else if first == "test" {
                cmd_test(&args[2..]);
            } else {
                eprintln!("unknown command '{}'\n", first);
                print_usage();
                std::process::exit(1);
            }
        }
    }
}

fn print_usage() {
    eprintln!("Usage: forai <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  fmt [path] [--check]    fmt");
    eprintln!("  check [file]            fmt → check");
    eprintln!("  test [file]             fmt → check → test");
    eprintln!("  run [file]              fmt → check → test → run");
    eprintln!("  build [target]          fmt → check → test → build");
    eprintln!("  new <name>              Create a new project");
    eprintln!("  doc [query]             Look up documentation");
    eprintln!("  interface [file]        Emit interface JSON");
    eprintln!("  mcp                     Run the fai MCP server");
    eprintln!();
    eprintln!("Each command runs all prerequisite steps. Failure at any step aborts.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -p, --project <name>    Select a project in a workspace");
    eprintln!("  build --html            Emit browser HTML runtime with wasm output");
    eprintln!("  build -o <path>         Write build output to a specific path");
    eprintln!();
    eprintln!("Shorthand:");
    eprintln!("  forai <file.fai>        Same as 'forai run <file.fai>'");
}

// ── Pipeline wrappers ────────────────────────────────────────────────
// Each cmd_* runs its prerequisite steps in order before its own step.
// Because each step_* calls process::exit(1) on failure, the pipeline
// aborts automatically at the first failing step.
//
// -p <name> / --project <name>: select a workspace member to operate on.
// Required when running from a workspace root with multiple members.

fn cmd_fmt(args: &[String]) {
    let (args, verbose) = extract_verbose_flag(args);
    let (args, project) = extract_project_flag(&args);
    let reporter = Reporter::new(verbose);
    let positional = args.iter().find(|a| !a.starts_with("--"));
    let is_project_mode = positional.is_none() || project.is_some();
    if is_project_mode {
        pipeline_enter_project(project.as_deref()).require_project();
        print_project_header(&reporter);
    }
    step_fmt(&args, &reporter);
}

fn cmd_check(args: &[String]) {
    let (args, verbose) = extract_verbose_flag(args);
    let (args, project) = extract_project_flag(&args);
    let reporter = Reporter::new(verbose);
    let positional = args.iter().find(|a| !a.starts_with("--"));
    let is_project_mode = positional.is_none() || project.is_some();
    if is_project_mode {
        pipeline_enter_project(project.as_deref()).require_project();
        print_project_header(&reporter);
    }
    step_fmt(&args, &reporter);
    step_check(&args, &reporter);
}

fn cmd_test(args: &[String]) {
    let (args, verbose) = extract_verbose_flag(args);
    let (args, project) = extract_project_flag(&args);
    let (args, project) = lift_target_name_positional(args, project);
    let reporter = Reporter::new(verbose);
    let positional = args.iter().find(|a| !a.starts_with("--"));
    let is_project_mode = positional.is_none() || project.is_some();
    if is_project_mode {
        pipeline_enter_project(project.as_deref()).require_project();
        print_project_header(&reporter);
    }
    // Scope to the selected target's entry when `--project NAME` is
    // used — same reasoning as in `cmd_run`: without it, fmt/check/
    // test run workspace-wide (effectively a no-op for sub-project
    // layouts where there's no src/main.fai), missing the target's
    // actual source tree.
    let args = scoped_pipeline_args(&args, project.as_deref());
    step_fmt(&args, &reporter);
    step_check(&args, &reporter);
    step_test(&args, &reporter);
}

fn cmd_run(args: &[String]) {
    let (args, verbose) = extract_verbose_flag(args);
    let (args, project) = extract_project_flag(&args);
    let (args, project) = lift_target_name_positional(args, project);
    let reporter = Reporter::new(verbose);
    let positional = args.iter().find(|a| !a.starts_with("--"));
    let is_prebuilt = positional
        .map(|p| p.ends_with(".wasm") || p.ends_with(".json"))
        .unwrap_or(false);

    if !is_prebuilt {
        // Only do workspace project resolution when no explicit file given (or -p used)
        let is_project_mode = positional.is_none() || project.is_some();
        if is_project_mode {
            pipeline_enter_project(project.as_deref()).require_project();
            print_project_header(&reporter);
        }
        // When `--project NAME` points at a sub-project, resolve its
        // entry file and run fmt/check/test against *that* file — same
        // way step_build recurses per-target. Without this, fmt/check/
        // test run in no-target mode (which is a near no-op for multi-
        // target workspaces where everything lives under src/<name>/)
        // and miss the target's real tests. Matches the 95/75 tests
        // `fai build` finds per target.
        let target_args = scoped_pipeline_args(&args, project.as_deref());
        step_fmt(&target_args, &reporter);
        step_check(&target_args, &reporter);
        step_test(&target_args, &reporter);
    }
    step_run(&args, project.as_deref(), &reporter);
}

/// If `project` names a sub-project of the current workspace, return a
/// copy of `args` with the target's resolved entry path prepended so
/// downstream pipeline steps (fmt/check/test) run against it rather
/// than the workspace-wide no-file default. Returns `args` unchanged
/// when there's no sub-project or when `args` already carries a
/// positional file path.
fn scoped_pipeline_args(args: &[String], project: Option<&str>) -> Vec<String> {
    let has_positional = args.iter().any(|a| !a.starts_with("--") && a != "-o");
    if has_positional {
        return args.to_vec();
    }
    let Some(name) = project else {
        return args.to_vec();
    };
    let entry = match resolve_target_entry_point(name) {
        Some(p) => p,
        None => return args.to_vec(),
    };
    let mut out = Vec::with_capacity(args.len() + 1);
    out.push(entry);
    out.extend(args.iter().cloned());
    out
}

fn cmd_build(args: &[String]) {
    let (args, verbose) = extract_verbose_flag(args);
    let (args, project) = extract_project_flag(&args);
    let reporter = Reporter::new(verbose);
    // If no --project given and the first positional is the name of a
    // sub-project in fai.toml (e.g. `fai build client`), treat it as
    // `--project client` so the pipeline steps and build scope to that
    // target instead of trying to format "client" as a file path.
    let (args, project) = lift_target_name_positional(args, project);
    let positional = args.iter().find(|a| !a.starts_with("--") && a != &"-o");
    let is_non_fai = positional
        .map(|p| !p.ends_with(".fai") && (p.contains('.') || p.contains('/')))
        .unwrap_or(false);

    if !is_non_fai {
        // Project mode: no positional file, or -p <target> selecting
        // one sub-project. The header prints once here and each
        // per-target recursion (below, `else` branch) runs with an
        // explicit file path and skips the header.
        if positional.is_none() || project.is_some() {
            let ctx = pipeline_enter_project(project.as_deref());
            if ctx.has_project() {
                print_project_header(&reporter);
                step_fmt(&args, &reporter);
                step_check(&args, &reporter);
                step_test(&args, &reporter);
            }
        } else {
            // Explicit .fai file given — always run pipeline steps.
            // Per-target recursion lands here, under the `▶ building
            // target 'X'` header that step_build emitted before calling.
            step_fmt(&args, &reporter);
            step_check(&args, &reporter);
            step_test(&args, &reporter);
        }
    }
    step_build(&args, project.as_deref(), &reporter);
}

/// If no --project flag was given and the first positional arg names a
/// sub-project from fai.toml, strip it from args and return it as the
/// effective project. Lets `fai build client` mean the same as
/// `fai build --project client`. Other commands (`fai test`, `fai run`)
/// can share this helper once they grow target-name positionals.
fn lift_target_name_positional(
    args: Vec<String>,
    project: Option<String>,
) -> (Vec<String>, Option<String>) {
    if project.is_some() {
        return (args, project);
    }
    let Some(name_idx) = args
        .iter()
        .position(|a| !a.starts_with("--") && a != "-o" && !a.contains('.') && !a.contains('/'))
    else {
        return (args, project);
    };
    let name = &args[name_idx];
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return (args, project),
    };
    let Some(root) = find_project_root(&cwd) else {
        return (args, project);
    };
    let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);
    if !info.sub_projects.contains_key(name.as_str()) {
        return (args, project);
    }
    let lifted = name.clone();
    let remaining: Vec<String> = args
        .iter()
        .enumerate()
        .filter_map(|(i, a)| if i == name_idx { None } else { Some(a.clone()) })
        .collect();
    (remaining, Some(lifted))
}

/// Print the top-level `checking N .fai files in <project> ...` banner.
/// No-op when called outside a standard project (e.g. a bare workspace
/// root or no fai.toml at all — we don't have anything meaningful to
/// count in those cases).
fn print_project_header(reporter: &Reporter) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let root = match find_project_root(&cwd) {
        Some(r) => r,
        None => return,
    };
    let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);
    let project_name = if info.name.is_empty() || info.name == "unknown" {
        root.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string()
    } else {
        info.name.clone()
    };
    // Count recursively under the project root — walks into sub-project
    // source trees (src/client, src/server) so fullstack projects get
    // one unified file count instead of per-target partials.
    let count = count_fai_files_recursive(&root);
    if count == 0 {
        return;
    }
    reporter.header(count, &project_name);
}

/// Extract -p/--project <name> from args, returning (remaining_args, project_name).
fn extract_project_flag(args: &[String]) -> (Vec<String>, Option<String>) {
    let mut remaining = Vec::new();
    let mut project = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" | "--project" => {
                if i + 1 < args.len() {
                    project = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    eprintln!("error: -p/--project requires a project name");
                    std::process::exit(1);
                }
            }
            a if a.starts_with("--project=") => {
                project = Some(a["--project=".len()..].to_string());
                i += 1;
            }
            _ => {
                remaining.push(args[i].clone());
                i += 1;
            }
        }
    }
    (remaining, project)
}

/// The result of resolving a project context for the pipeline.
enum ProjectContext {
    /// A single project — no CWD change, pipeline steps should run.
    SingleProject,
    /// Entered a workspace member directory; pipeline steps should run.
    /// The contained PathBuf is the previous CWD (held for its Drop side-effect).
    WorkspaceMember(std::path::PathBuf),
    /// Workspace root with no -p given — pipeline steps should be skipped.
    /// The overall command handles building all members itself.
    WorkspaceNone,
}

impl ProjectContext {
    /// True when pipeline steps should run (single project or selected member).
    fn has_project(&self) -> bool {
        !matches!(self, ProjectContext::WorkspaceNone)
    }

    /// Exits with an error when this context has no project (workspace without -p).
    fn require_project(&self) {
        if let ProjectContext::WorkspaceNone = self {
            eprintln!("error: this is a workspace with multiple projects");
            eprintln!("       use -p <project> to select one");
            std::process::exit(1);
        }
    }
}

/// Determine which project to operate on and optionally cd into it.
/// - Single project: returns `SingleProject` (no CWD change).
/// - Workspace + `-p name`: cds into the member dir, returns `WorkspaceMember`.
/// - Workspace + no `-p`: prints an error and returns `WorkspaceNone`.
/// - No fai.toml found: returns `SingleProject` (steps will handle the error).
fn pipeline_enter_project(project_name: Option<&str>) -> ProjectContext {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return ProjectContext::SingleProject,
    };
    let project_root = match find_project_root(&cwd) {
        Some(r) => r,
        None => return ProjectContext::SingleProject,
    };
    let toml = match std::fs::read_to_string(project_root.join("fai.toml")) {
        Ok(c) => c,
        Err(_) => return ProjectContext::SingleProject,
    };
    let info = parse_project_info(&toml);

    if info.workspace_members.is_empty() {
        return ProjectContext::SingleProject; // Standard single project
    }

    // Workspace detected
    let name = match project_name {
        Some(n) => n,
        None => return ProjectContext::WorkspaceNone,
    };

    // Find and enter the named member directory.
    // CWD changes are global — use a mutex so parallel test invocations don't race.
    static CWD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = CWD_LOCK.get_or_init(|| std::sync::Mutex::new(())).lock();

    for member in &info.workspace_members {
        let member_dir = project_root.join(member);
        let member_name = std::path::Path::new(member)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(member.as_str());
        if member_name == name || member.as_str() == name {
            if member_dir.is_dir() {
                let prev = std::env::current_dir().unwrap();
                std::env::set_current_dir(&member_dir).unwrap();
                return ProjectContext::WorkspaceMember(prev);
            }
        }
    }

    let mut members: Vec<&str> = info.workspace_members.iter().map(|s| s.as_str()).collect();
    members.sort();
    eprintln!("error: project '{}' not found in workspace", name);
    eprintln!("       available: {}", members.join(", "));
    std::process::exit(1);
}

// ── Steps ────────────────────────────────────────────────────────────

fn step_check(args: &[String], reporter: &Reporter) {
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

    // Try the entry point first (for runnable projects with main.fai)
    if let Some(entry) = resolve_entry_point(&project_root, &src_dir) {
        check_single_file(&entry.to_string_lossy(), reporter);
        return;
    }

    // Library project: no single entry point — check all source files as one module.
    // prepare_module_directory loads all .fai files in the directory together so that
    // cross-file references (Connection, SQLITE_OK, etc.) resolve correctly.
    let src_path = project_root.join(&src_dir);
    let prepared = match fai_compiler::prepare_module_directory(&src_path.to_string_lossy()) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&e);
            reporter.step(StepStatus::Fail, "check", "compile error");
            std::process::exit(1);
        }
    };
    let mut checker = fai_checker::Checker::new();
    match checker.check_with_modules(
        &prepared.serde_ast.statements,
        &prepared
            .modules
            .iter()
            .map(|m| fai_checker::PreparedModule {
                name: m.name.clone(),
                statements: m.statements.clone(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect::<Vec<_>>(),
    ) {
        Ok(()) => reporter.step(StepStatus::Ok, "check", "no type errors"),
        Err(e) => {
            reporter.error_line(&format_check_errors(&checker, &e));
            let n = checker.collected_errors.len().max(1);
            reporter.step(
                StepStatus::Fail,
                "check",
                &format!("{} type error{}", n, if n == 1 { "" } else { "s" }),
            );
            std::process::exit(1);
        }
    }
}

fn check_single_file(path: &str, reporter: &Reporter) {
    match try_check_single_file(path) {
        Ok(()) => reporter.step(StepStatus::Ok, "check", "no type errors"),
        Err(e) => {
            reporter.error_line(&e);
            // Each type error is a full "type error: ..." line in the
            // joined message; count the distinct error prefixes so the
            // summary number matches what the user sees above it.
            let n = e.matches("type error:").count().max(1);
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
fn try_check_single_file(path: &str) -> Result<(), String> {
    let mut content =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read '{}': {}", path, e))?;
    let source_root = find_source_root(path);
    let info = read_project_info_full(source_root.as_deref());
    inject_peer_hash(
        &mut content,
        &info,
        source_root.as_deref(),
        /* verbose = */ false,
    );
    inject_rpc_dispatch(&mut content, &info, source_root.as_deref());
    let synthetic_modules = generate_rpc_proxy_modules(source_root.as_deref());
    let prepared = fai_compiler::prepare_source_with_synthetic_and_entry(
        &content,
        source_root.as_deref(),
        synthetic_modules,
        Some(path),
    )
    .map_err(|e| e.to_string())?;
    let mut checker = fai_checker::Checker::new();
    match run_checker(&mut checker, &prepared) {
        Ok(()) => Ok(()),
        Err(e) => Err(format_check_errors(&checker, &e)),
    }
}

fn step_test(args: &[String], reporter: &Reporter) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));

    if let Some(path) = file_arg {
        // Test a single file
        run_tests_file(path, reporter);
        return;
    }

    // Test all .fai files in the project's source directory.
    // Skip if no standard project found (e.g. workspace root).
    let (project_root, src_dir) = match find_project_source_from_cwd() {
        Some(r) => r,
        None => return,
    };
    // Sub-project mode: `fai build` recurses into each target and runs
    // its own step_test under the `▶ building target` header. The outer
    // project-wide pass has nothing meaningful to test on top of that —
    // the target's sources live under e.g. `src/client/**`, not at the
    // shared `src/` root — and printing `[ok] test — no public functions`
    // at the top was just misleading noise. Skip the outer pass and let
    // each target report its own result.
    let toml = std::fs::read_to_string(project_root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);
    if !info.sub_projects.is_empty() {
        return;
    }
    let src_path = project_root.join(&src_dir);
    // Walk the tree recursively so nested module dirs (e.g. `pages/`,
    // `components/`, `util/`) contribute to the test+coverage rollup.
    let files = collect_fai_files_recursive(&src_path);

    let mut total_passed = 0usize;
    let mut total_failed = 0usize;
    let mut total_covered = 0usize;
    let mut total_uncovered: Vec<String> = Vec::new();

    for file in &files {
        let content = match std::fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                reporter.error_line(&format!("error reading {}: {}", file, e));
                continue;
            }
        };
        let source_root = find_source_root(file);
        let prepared = match fai_compiler::prepare_source(&content, source_root.as_deref()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let mut checker = fai_checker::Checker::new();
        if run_checker(&mut checker, &prepared).is_err() {
            continue;
        }
        let info = fai_codegen_wasm::direct::CheckerInfo {
            ufcs_calls: checker.ufcs_calls.clone(),
            named_param_reorder: checker.named_param_reorder.clone(),
            expression_types: checker.expression_types.clone(),
            generic_type_args: checker.generic_type_args.clone(),
        };
        let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned(
            &prepared.serde_ast,
            &prepared.modules,
            &info,
            None,
            true,
        ) {
            Ok(w) => w,
            Err(e) => {
                reporter.error_line(&format!(
                    "internal error: direct AST→wasm codegen refused {}: {:?}",
                    file, e,
                ));
                continue;
            }
        };
        // Test + coverage metadata walks the AST directly.
        let meta = test_meta::extract(&prepared);
        let tests = meta.suites;
        let public_fns: Vec<String> = meta
            .coverage_candidates
            .into_iter()
            .filter(|n| !n.is_empty() && !n.starts_with('<') && n != "main" && !n.starts_with("__"))
            .collect();
        let suite_names: std::collections::HashSet<String> =
            tests.iter().map(|t| t.suite_name.clone()).collect();
        let uncovered: Vec<String> = public_fns
            .into_iter()
            .filter(|n| !suite_names.contains(n))
            .collect();

        let mut current_suite: Option<String> = None;
        let externs = extract_externs_from_prepared(&prepared, source_root.as_deref());
        match wasm_runner::run_wasm_tests_with_externs(&wasm_bytes, &tests, externs, |outcome| {
            if current_suite.as_deref() != Some(&outcome.suite_name) {
                println!("  {}", outcome.suite_name);
                current_suite = Some(outcome.suite_name.clone());
            }
            match &outcome.error {
                None => println!("    ✓ {}", outcome.case_desc),
                Some(msg) => println!("    ✗ {} — {}", outcome.case_desc, msg),
            }
        }) {
            Ok(summary) => {
                total_passed += summary.passed;
                total_failed += summary.failed;
                total_covered += summary.passed + summary.failed;
                total_uncovered.extend(uncovered);
            }
            Err(e) => {
                reporter.error_line(&format!(
                    "runtime error during test setup in {}: {}",
                    file, e
                ));
                reporter.step(StepStatus::Fail, "test", "setup failed");
                std::process::exit(1);
            }
        }
    }

    // Missing tests are reported as failed tests — same `[fail] test`
    // outcome, same exit code. The scaffold treats an untested public
    // function as a build-blocking problem; folding it into the pass/
    // fail count keeps a single mental model across both kinds of
    // test failure (assertion and missing-block).
    let missing = total_uncovered.len();
    let combined_failures = total_failed + missing;
    if combined_failures > 0 {
        for name in &total_uncovered {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("{} passed, {} failed", total_passed, combined_failures),
        );
        std::process::exit(1);
    }
    if total_covered == 0 {
        reporter.step(StepStatus::Ok, "test", "no public functions to test");
    } else {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed, coverage 100%", total_covered),
        );
    }
}

/// Run tests for a single .fai file, exiting on failure.
/// Fails if any tests fail OR if any named public function lacks a test
/// block. Missing tests are counted as test failures (same `[fail] test`
/// outcome, same exit code) — the scaffold makes coverage mandatory.
/// Walk the entry file's directory tree (recursively) and collect
/// (has_test_blocks, public_fn_names) across every `.fai` file
/// reachable from it. Used to decide whether the whole target can
/// short-circuit as "no tests here" or needs to run the VM — the
/// entry file alone isn't enough because public functions and test
/// blocks can live in sibling or nested-module files
/// (`pages/`, `components/`, `state/`, etc.).
fn scan_module_for_tests_and_publics(entry_path: &str, entry_raw: &str) -> (bool, Vec<String>) {
    let mut has_tests = false;
    let mut publics: Vec<String> = Vec::new();
    let entry_canonical = std::fs::canonicalize(entry_path).ok();

    // Always include the entry file itself — use its already-read
    // content (which may carry CLI-injected bits like peer hash) rather
    // than re-reading from disk.
    if let Ok(ast) = fai_parser::parse(entry_raw) {
        has_tests = has_tests
            || ast
                .statements
                .iter()
                .any(|s| matches!(s, fai_parser::ast::Statement::Test(_)));
        publics.extend(collect_public_function_names(&ast.statements));
    }

    let parent = std::path::Path::new(entry_path).parent();
    if let Some(dir) = parent {
        for sibling in collect_fai_files_recursive(dir) {
            // Skip the entry we already scanned above. Compare by
            // canonical path so a relative `./main.fai` vs the walker's
            // absolute path still matches.
            let sibling_canonical = std::fs::canonicalize(&sibling).ok();
            if sibling_canonical == entry_canonical {
                continue;
            }
            let content = match std::fs::read_to_string(&sibling) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let ast = match fai_parser::parse(&content) {
                Ok(a) => a,
                Err(_) => continue,
            };
            if !has_tests {
                has_tests = ast
                    .statements
                    .iter()
                    .any(|s| matches!(s, fai_parser::ast::Statement::Test(_)));
            }
            publics.extend(collect_public_function_names(&ast.statements));
        }
    }

    publics.sort();
    publics.dedup();
    (has_tests, publics)
}

/// Recursive version of `collect_fai_files`. Walks `root` and all
/// nested directories, returning every `.fai` file as a sorted
/// absolute-ish path string. A target's source tree spans nested
/// module dirs (`pages/`, `components/`, etc.), so coverage and
/// test-block detection need the full tree, not just siblings.
fn collect_fai_files_recursive(root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("fai") {
                out.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out
}

fn run_tests_file(path: &str, reporter: &Reporter) {
    let raw_content = read_file(path);
    // If the file has no test blocks, skip the full compile-and-run —
    // complex entry-point files can overflow the VM register file in
    // test mode. We still need to know whether there are public
    // functions to cover: if there are, "no tests" becomes a failure.
    //
    // Scan the *whole module* (all sibling .fai files in the entry's
    // directory), not just the entry file. A target's public functions
    // live across multiple files — e.g. `src/client/main.fai` only has
    // `main`, but `src/client/app.fai` has `def App`. Missing that
    // breadth made the early-return report "no public functions" for a
    // module that actually had uncovered public code.
    let (has_test_blocks, public_fn_names) = scan_module_for_tests_and_publics(path, &raw_content);
    if !has_test_blocks {
        if public_fn_names.is_empty() {
            reporter.step(StepStatus::Ok, "test", "no public functions to test");
            return;
        }
        for name in &public_fn_names {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("0 passed, {} failed", public_fn_names.len()),
        );
        std::process::exit(1);
    }

    // For test compilation: use prepare_source + checker for dependency resolution
    // and UFCS marks, but don't inject the RPC dispatch (it adds many functions
    // that inflate VM register usage and may cause stack overflows). The dispatch
    // is only needed at build time, not when running tests.
    let content = raw_content;
    let source_root = find_source_root(path);
    let synthetic_modules = generate_rpc_proxy_modules(source_root.as_deref());
    // Compile without dispatch injection. Use prepare_source to resolve deps,
    // run the checker for UFCS info, then compile.
    let source_root_str = find_source_root(path);
    // Use the `_for_tests` variant so (a) test blocks in imported
    // modules aren't stripped, and (b) the PreparedProgram flags
    // every module function as in-coverage — the coverage check then
    // demands a test for every function across the whole target tree,
    // not just the entry file.
    let prepared = match fai_compiler::prepare_source_with_synthetic_and_entry_for_tests(
        &content,
        source_root_str.as_deref(),
        synthetic_modules,
        Some(path),
    ) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&e);
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    let mut checker = fai_checker::Checker::new();
    // If the checker fails (e.g. addRpcRoutes undefined without dispatch),
    // fall back to compiling with empty checker info — tests run regardless.
    let _ = run_checker(&mut checker, &prepared);
    // Direct AST→wasm is the only codegen path used for tests.
    let info = fai_codegen_wasm::direct::CheckerInfo {
        ufcs_calls: checker.ufcs_calls.clone(),
        named_param_reorder: checker.named_param_reorder.clone(),
        expression_types: checker.expression_types.clone(),
        generic_type_args: checker.generic_type_args.clone(),
    };
    let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        None,
        true,
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error_line(&format!(
                "internal error: direct AST→wasm codegen refused this program: {:?}",
                e,
            ));
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    // Test + coverage metadata walks the AST directly.
    let meta = test_meta::extract(&prepared);
    let tests = meta.suites;
    let public_fns: Vec<String> = meta
        .coverage_candidates
        .into_iter()
        .filter(|n| !n.is_empty() && !n.starts_with('<') && n != "main" && !n.starts_with("__"))
        .collect();
    let suite_names: std::collections::HashSet<String> =
        tests.iter().map(|t| t.suite_name.clone()).collect();
    let uncovered: Vec<String> = public_fns
        .into_iter()
        .filter(|n| !suite_names.contains(n))
        .collect();

    let mut current_suite: Option<String> = None;
    let externs = extract_externs_from_prepared(&prepared, source_root.as_deref());
    let (passed, failed) =
        match wasm_runner::run_wasm_tests_with_externs(&wasm_bytes, &tests, externs, |outcome| {
            if current_suite.as_deref() != Some(&outcome.suite_name) {
                println!("  {}", outcome.suite_name);
                current_suite = Some(outcome.suite_name.clone());
            }
            match &outcome.error {
                None => println!("    ✓ {}", outcome.case_desc),
                Some(msg) => println!("    ✗ {} — {}", outcome.case_desc, msg),
            }
        }) {
            Ok(summary) => (summary.passed, summary.failed),
            Err(e) => {
                reporter.error_line(&format!("runtime error during test setup: {}", e));
                reporter.step(StepStatus::Fail, "test", "setup failed");
                std::process::exit(1);
            }
        };

    // Combine assertion failures and missing-test failures into a
    // single failure count so the summary matches the scaffold's
    // promise — every public function must be tested, with no
    // separate "coverage" category that could be misread as advisory.
    let missing = uncovered.len();
    let combined_failures = failed + missing;
    if combined_failures > 0 {
        for name in &uncovered {
            reporter.error_line(&format!("{}: missing test block", name));
        }
        reporter.step(
            StepStatus::Fail,
            "test",
            &format!("{} passed, {} failed", passed, combined_failures),
        );
        std::process::exit(1);
    }
    if passed == 0 {
        reporter.step(StepStatus::Ok, "test", "no public functions to test");
    } else {
        reporter.step(
            StepStatus::Ok,
            "test",
            &format!("{} passed, coverage 100%", passed),
        );
    }
}

/// Collect public, named function declarations from a file's top-level
/// statements. Used to enforce the "every public function needs a test"
/// rule at AST level — when the file has no `test` blocks at all, we
/// can't run the VM coverage check, but we still want to fail the
/// build rather than silently letting untested public functions slip
/// through with "no tests found".
///
/// Skips: `main` (the program entry), anonymous functions (name starts
/// with `<`), functions marked `private`, and functions in a sticky
/// `private:` section (detected by the `is_private` flag on each decl).
fn collect_public_function_names(statements: &[fai_parser::ast::Statement]) -> Vec<String> {
    // Name is historical — this now collects **every** user-defined
    // function (public and private alike) except `main` and compiler-
    // synthetic names. The test policy is "every function requires a
    // test"; `private` is not a way to opt out. `main` is the single
    // explicit exception — it's the program's entry, not a unit of
    // testable behaviour.
    let mut names = Vec::new();
    for stmt in statements {
        if let fai_parser::ast::Statement::Function(fd) = stmt {
            if fd.name == "main" || fd.name.starts_with('<') || fd.name.starts_with("__") {
                continue;
            }
            names.push(fd.name.clone());
        }
    }
    names
}

/// Collect all .fai files in a directory (non-recursive, sorted).
fn collect_fai_files(dir: &str) -> Vec<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("fai") {
                Some(p.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect();
    files.sort();
    files
}

fn step_run(args: &[String], project: Option<&str>, reporter: &Reporter) {
    // Phase D: `--wasm` is no longer a toggle — wasm is the only run
    // path. Accept the flag for back-compat with scripts that pass it;
    // the explicit `use_wasm` binding is kept as `_` so the filter at
    // the top of `positional` still skips it.
    let _use_wasm = args.iter().any(|a| a == "--wasm");

    // Check if the first positional arg is a target name or a file path.
    // If no positional arg, try project-based resolution from cwd.
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--") && *a != "--wasm")
        .map(|a| a.as_str())
        .collect();

    let path = if let Some(target) = project {
        // Explicit `--project NAME` — honour it over everything else.
        resolve_target_entry_point(target).unwrap_or_else(|| {
            reporter.error_line(&format!("could not resolve target '{}'", target));
            reporter.step(StepStatus::Fail, "run", "unknown target");
            std::process::exit(1);
        })
    } else if let Some(arg) = positional.first() {
        let is_file = arg.contains('.') || arg.contains('/') || arg.contains('\\');
        if is_file {
            // Explicit file path — use it directly
            arg.to_string()
        } else {
            // Target name (legacy positional form, still supported)
            resolve_target_entry_point(arg).unwrap_or_else(|| {
                reporter.error_line(&format!("could not resolve target '{}'", arg));
                reporter.step(StepStatus::Fail, "run", "unknown target");
                std::process::exit(1);
            })
        }
    } else {
        // No arg — find the default/only target from fai.toml. When
        // the project has multiple sub-projects this prints a clear
        // `--project required` message and exits.
        resolve_default_entry_point().unwrap_or_else(|| {
            reporter.step(StepStatus::Fail, "run", "target not specified");
            std::process::exit(1);
        })
    };

    // Run pre-compiled .wasm files directly via Wasmtime
    if path.ends_with(".wasm") {
        let wasm_bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("error reading {}: {}", path, e);
                std::process::exit(1);
            }
        };
        if let Err(e) = wasm_runner::run_wasm(&wasm_bytes) {
            eprintln!("{}", e);
            std::process::exit(1);
        }
        return;
    }

    let mut content = read_file(&path);

    // Apply peerHash injection when `[remote-interface] from = "..."`
    // is set — the VM/JIT paths must see the same source the build
    // path does, or `peerHash()` won't resolve when running via
    // `forai run`. Plan 99 Phase 2.4.
    let run_source_root = find_source_root(&path);
    let run_info = read_project_info_full(run_source_root.as_deref());
    inject_peer_hash(
        &mut content,
        &run_info,
        run_source_root.as_deref(),
        /* verbose = */ false,
    );

    // Plan 101: inject generated RPC dispatch/proxies for run path too.
    inject_rpc_dispatch(&mut content, &run_info, run_source_root.as_deref());

    // Generate synthetic RPC-proxy modules so `use { X } from Server`
    // resolves during the run-path's type check. Without this, a
    // fullstack server whose entry does `use { App } from client` (and
    // whose transitive client imports reach the `Server` proxy) fails
    // with `Unknown name 'App'` — the check step in compile_fai sees
    // the client's unresolved `Server` imports and cascades. `step_build`
    // and `step_check` (via `check_single_file`) already do this; run
    // now matches.
    let synthetic_modules = generate_rpc_proxy_modules(run_source_root.as_deref());

    // Phase H: the only input format is `.fai` source. The old
    // pre-compiled JSON-bytecode path lived on top of the bytecode→wasm
    // codegen — deleted along with `translate.rs` / `module.rs`.
    if !path.ends_with(".fai") {
        eprintln!(
            "error: only .fai source files are supported (pre-compiled JSON input was removed in Phase H)",
        );
        std::process::exit(1);
    }
    let wasm_bytes = compile_fai_to_wasm(&content, &path, false, synthetic_modules.clone(), None);
    let externs = extract_extern_info_full(&content, &path, synthetic_modules);
    if let Err(e) = wasm_runner::run_wasm_with_externs(&wasm_bytes, externs) {
        reporter.error_line(&e);
        reporter.step(StepStatus::Fail, "run", "runtime error");
        std::process::exit(1);
    }
}

/// Build the host-side extern table by walking the entry file plus
/// every resolved dependency module — matching the codegen's
/// `extern_fn_indices` ordering (entry first, then modules in
/// discovery order). The wasm runner's `call_ffi` import indexes
/// into this table by `ext_fn_idx`. `[ffi.<name>].lib` in fai.toml
/// overrides the C library name; otherwise the block's own `extern
/// <name>` identifier is used.
fn extract_extern_info_full(
    content: &str,
    path: &str,
    synthetic_modules: Vec<(String, String)>,
) -> Vec<wasm_runner::ExternInfo> {
    let source_root = find_source_root(path);

    // Run the same compile pre-pass the codegen uses so we see the
    // identical set of modules (and in the same order). Extern blocks
    // live in the compiler-side AST; iterate entry.statements first,
    // then each module's statements.
    let prepared = match fai_compiler::prepare_source_with_synthetic_and_entry(
        content,
        source_root.as_deref(),
        synthetic_modules,
        Some(path),
    ) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    // Each module (or entry) may root a different `fai.toml` — load
    // the ffi config from the one the source root points at.
    let ffi_config = source_root
        .as_deref()
        .map(fai_compiler::ffi_config::load_ffi_config)
        .unwrap_or_default();
    // Library-name override: only from the entry project's own
    // fai.toml. Per-dependency `[ffi.*]` overrides would require the
    // compiler to expose each module's source root — DiscoveredModule
    // doesn't carry that today. In practice every fai dep so far uses
    // `extern <libname>` directly, so the override is optional.
    let resolve_lib = |block_name: &str| -> String {
        ffi_config
            .libraries
            .get(block_name)
            .map(|lc| lc.lib.clone())
            .unwrap_or_else(|| block_name.to_string())
    };

    let mut externs = Vec::new();
    let mut push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
                          externs: &mut Vec<wasm_runner::ExternInfo>| {
        let library = resolve_lib(&block.library);
        for decl in &block.functions {
            let param_types: Vec<wasm_runner::FfiType> = decl
                .params
                .iter()
                .map(|p| compiler_typenode_to_ffi_type(&p.type_node, p.is_out))
                .collect();
            let return_type = decl
                .return_type
                .as_ref()
                .map(|tn| compiler_typenode_to_ffi_type(tn, false))
                .unwrap_or(wasm_runner::FfiType::Void);
            externs.push(wasm_runner::ExternInfo {
                library: library.clone(),
                function: decl.name.clone(),
                param_types,
                return_type,
            });
        }
    };

    for stmt in &prepared.serde_ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
            push_block(block, &mut externs);
        }
    }
    for module in &prepared.modules {
        for stmt in &module.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
                push_block(block, &mut externs);
            }
        }
    }
    externs
}

/// Like `extract_extern_info_full` but takes an already-prepared
/// program. Used by the test-step paths that already have the
/// `PreparedProgram` on hand and don't want to re-parse.
fn extract_externs_from_prepared(
    prepared: &fai_compiler::PreparedProgram,
    source_root: Option<&str>,
) -> Vec<wasm_runner::ExternInfo> {
    let ffi_config = source_root
        .map(fai_compiler::ffi_config::load_ffi_config)
        .unwrap_or_default();
    let resolve_lib = |block_name: &str| -> String {
        ffi_config
            .libraries
            .get(block_name)
            .map(|lc| lc.lib.clone())
            .unwrap_or_else(|| block_name.to_string())
    };
    let mut externs = Vec::new();
    let mut push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
                          externs: &mut Vec<wasm_runner::ExternInfo>| {
        let library = resolve_lib(&block.library);
        for decl in &block.functions {
            let param_types: Vec<wasm_runner::FfiType> = decl
                .params
                .iter()
                .map(|p| compiler_typenode_to_ffi_type(&p.type_node, p.is_out))
                .collect();
            let return_type = decl
                .return_type
                .as_ref()
                .map(|tn| compiler_typenode_to_ffi_type(tn, false))
                .unwrap_or(wasm_runner::FfiType::Void);
            externs.push(wasm_runner::ExternInfo {
                library: library.clone(),
                function: decl.name.clone(),
                param_types,
                return_type,
            });
        }
    };
    for stmt in &prepared.serde_ast.statements {
        if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
            push_block(block, &mut externs);
        }
    }
    for module in &prepared.modules {
        for stmt in &module.statements {
            if let fai_compiler::ast::Statement::ExternBlockDeclaration(block) = stmt {
                push_block(block, &mut externs);
            }
        }
    }
    externs
}

fn compiler_typenode_to_ffi_type(
    tn: &fai_compiler::ast::TypeNode,
    is_out: bool,
) -> wasm_runner::FfiType {
    use wasm_runner::FfiType;
    if is_out {
        return FfiType::OutPtr;
    }
    let name = tn.name.as_deref().unwrap_or("");
    match name {
        "Int" => FfiType::Int,
        "Float" => FfiType::Double,
        "String" => FfiType::String,
        "Bool" => FfiType::Bool,
        "Ptr" => FfiType::Pointer,
        "Void" => FfiType::Void,
        _ => FfiType::Pointer,
    }
}

fn typenode_to_ffi_type(tn: &fai_parser::ast::TypeNode, is_out: bool) -> wasm_runner::FfiType {
    use wasm_runner::FfiType;
    if is_out {
        return FfiType::OutPtr;
    }
    let name = tn.name.as_deref().unwrap_or("");
    match name {
        "Int" => FfiType::Int,
        "Float" => FfiType::Double,
        "String" => FfiType::String,
        "Bool" => FfiType::Bool,
        "Ptr" => FfiType::Pointer,
        "Void" => FfiType::Void,
        _ => FfiType::Pointer,
    }
}

// `run_vm_with_quiet_panic` lived here to wrap the VM's synchronous
// `execute` / `run_tests` calls in a catch_unwind + hook-swap so an
// unhandled register-overflow panic surfaced as a clean `[fail]` line.
// Phase D/E removed the VM from the run and test paths; wasmtime traps
// return `Err` instead of unwinding, so the helper isn't needed.

fn step_build(args: &[String], project: Option<&str>, reporter: &Reporter) {
    // Check for target name or file path
    let positional: Vec<&str> = args
        .iter()
        .filter(|a| !a.starts_with("--") && !matches!(a.as_str(), "-o"))
        .map(|a| a.as_str())
        .collect();

    // Handle: `fai build` (no args) — build all targets
    // Handle: `fai build client` — build named target (lifted to `project` in cmd_build)
    // Handle: `fai build file.fai` — build specific file (backwards compat)
    if positional.is_empty() {
        // No args — try to build all targets from fai.toml
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Some(root) = find_project_root(&cwd) {
            let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
            let info = parse_project_info(&toml);
            // New multi-target mode
            if !info.sub_projects.is_empty() {
                let targets = select_targets(&info, project);
                if targets.is_empty() && project.is_some() {
                    std::process::exit(1);
                }
                for (name, sub) in &targets {
                    // Prefer explicit main, fall back to convention
                    let entry_opt = sub
                        .main
                        .as_ref()
                        .map(|m| root.join(m))
                        .filter(|p| p.is_file())
                        .or_else(|| {
                            sub.source.as_ref().and_then(|src| {
                                resolve_entry_point_with_hint(&root, src, Some(name))
                            })
                        });
                    if let Some(entry) = entry_opt {
                        eprintln!("\n▶ building target '{}' ({})", name, entry.display());
                        let mut build_args = vec![entry.to_string_lossy().into_owned()];
                        if matches!(sub.target, Some(BuildTarget::WasmHtml)) {
                            build_args.push("--html".to_string());
                        }
                        if let Some(bd) = &sub.build_dir {
                            let out_dir = root.join(bd);
                            let _ = std::fs::create_dir_all(&out_dir);
                            let stem = entry.file_stem().unwrap_or_default().to_string_lossy();
                            build_args.push("-o".to_string());
                            build_args.push(
                                out_dir
                                    .join(format!("{}.wasm", stem))
                                    .to_string_lossy()
                                    .into_owned(),
                            );
                        }
                        cmd_build(&build_args);
                    } else {
                        eprintln!("  warning: target '{}' — no entry point found", name);
                    }
                }
                return;
            }
            // Old workspace mode (backwards compat)
            if !info.workspace_members.is_empty() {
                cmd_build_workspace(&root, &info.workspace_members);
                return;
            }
        }
    }

    let first_arg = positional.first().copied().unwrap_or("");
    // Detect if the arg is a file path (has extension or path separator) vs a target name
    let is_file_path =
        first_arg.contains('.') || first_arg.contains('/') || first_arg.contains('\\');
    let path = if !first_arg.is_empty() && !is_file_path {
        // Target name — resolve from fai.toml and apply sub-project config
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        if let Some(root) = find_project_root(&cwd) {
            let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
            let ws_info = parse_project_info(&toml);
            if let Some(sub) = ws_info.sub_projects.get(first_arg) {
                let entry = sub
                    .main
                    .as_ref()
                    .map(|m| root.join(m))
                    .filter(|p| p.is_file())
                    .or_else(|| {
                        sub.source.as_ref().and_then(|src| {
                            resolve_entry_point_with_hint(&root, src, Some(first_arg))
                        })
                    })
                    .unwrap_or_else(|| {
                        eprintln!("error: could not resolve target '{}'", first_arg);
                        std::process::exit(1);
                    });
                eprintln!("▶ building target '{}' ({})", first_arg, entry.display());
                let mut build_args = vec![entry.to_string_lossy().into_owned()];
                if matches!(sub.target, Some(BuildTarget::WasmHtml)) {
                    build_args.push("--html".to_string());
                }
                if let Some(bd) = &sub.build_dir {
                    let out_dir = root.join(bd);
                    let _ = std::fs::create_dir_all(&out_dir);
                    let stem = entry.file_stem().unwrap_or_default().to_string_lossy();
                    build_args.push("-o".to_string());
                    build_args.push(
                        out_dir
                            .join(format!("{}.wasm", stem))
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                cmd_build(&build_args);
                return;
            }
        }
        resolve_target_entry_point(first_arg).unwrap_or_else(|| {
            eprintln!("error: could not resolve target '{}'", first_arg);
            std::process::exit(1);
        })
    } else if !first_arg.is_empty() {
        first_arg.to_string()
    } else {
        // Single project with no sub-projects — find entry point
        resolve_default_entry_point().unwrap_or_else(|| {
            eprintln!("error: no file specified and no fai.toml found");
            std::process::exit(1);
        })
    };
    let mut content = read_file(&path);
    let source_root = find_source_root(&path);
    let info = read_project_info_full(source_root.as_deref());
    let build_dir_opt = info.build_dir.clone();

    inject_peer_hash(&mut content, &info, source_root.as_deref(), is_verbose());

    let build_native = matches!(info.target, Some(BuildTarget::Native));

    // Plan 101: Generate RPC dispatch (server) and proxy modules (client).
    inject_rpc_dispatch(&mut content, &info, source_root.as_deref());

    let synthetic_modules = if !build_native {
        generate_rpc_proxy_modules(source_root.as_deref())
    } else {
        Vec::new()
    };

    // CLI --html flag wins when present; otherwise consult the toml
    // target. This keeps the old CLI flag working while letting new
    // projects declare it in toml.
    let generate_html =
        args.iter().any(|a| a == "--html") || matches!(info.target, Some(BuildTarget::WasmHtml));

    // Pass target to codegen so it can exclude unavailable host imports
    // (e.g. http_server_* for browser WASM targets).
    let codegen_target = if generate_html {
        Some("wasm-html")
    } else {
        None
    };

    // Plan 94 Phase G: for default (non-html) builds try the direct
    // AST→wasm path before falling back to the bytecode codegen.
    // `wasm-html` forces bytecode because the direct module
    // assembler doesn't honour target-filtered imports yet.
    let wasm_bytes = compile_fai_to_wasm(&content, &path, false, synthetic_modules, codegen_target);

    // Determine output directory and filename
    let output_path = if let Some(pos) = args.iter().position(|a| a == "-o") {
        args.get(pos + 1)
            .unwrap_or_else(|| {
                eprintln!("-o requires an output path");
                std::process::exit(1);
            })
            .clone()
    } else if generate_html {
        // Use build_dir from fai.toml (default: "public"), resolved relative to project root
        let build_dir = build_dir_opt.as_deref().unwrap_or("public");
        let project_root = source_root
            .as_deref()
            .and_then(|sr| std::path::Path::new(sr).parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let out_dir = project_root.join(build_dir);
        let _ = std::fs::create_dir_all(&out_dir);
        let stem = std::path::Path::new(&path)
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();
        out_dir
            .join(format!("{}.wasm", stem))
            .to_str()
            .unwrap()
            .to_string()
    } else if let Some(stem) = path.strip_suffix(".fai") {
        format!("{}.wasm", stem)
    } else {
        format!("{}.wasm", path)
    };

    match std::fs::write(&output_path, &wasm_bytes) {
        Ok(_) => {
            reporter.detail(&format!(
                "compiled {} -> {} ({})",
                path,
                output_path,
                format_bytes(wasm_bytes.len()),
            ));
        }
        Err(e) => {
            reporter.error_line(&format!("error writing {}: {}", output_path, e));
            reporter.step(StepStatus::Fail, "build", "write error");
            std::process::exit(1);
        }
    }

    // Plan 101: If the source has remote functions/types, write schema.json
    // next to the build output so client builds can consume it.
    if content.contains("remote def") || content.contains("remote type") {
        let schema_dir = std::path::Path::new(&output_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let prepared = fai_compiler::prepare_source(&content, None)
            .ok()
            .map(|p| p.serde_ast.statements);
        if let Some(stmts) = prepared {
            let spec = interface::extract_remote_schema(&stmts);
            let json = interface::spec_to_json(&spec);
            let schema_path = schema_dir.join("schema.json");
            if let Err(e) = std::fs::write(&schema_path, &json) {
                reporter.error_line(&format!("warning: could not write schema.json: {}", e));
            } else {
                reporter.detail(&format!("generated {}", schema_path.display()));
            }
        }
    }

    // `target = "native"` → pack the wasm inside a copy of the
    // current forai binary. Produces a single-file deployable that
    // runs `_start` and exits. Plan 99 Phase 3.
    if build_native {
        let wasm_path = std::path::Path::new(&output_path);
        let out_dir = wasm_path.parent().unwrap_or(std::path::Path::new("."));
        let stem = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("app");
        let native_path = out_dir.join(stem);
        match pack_native_binary(&wasm_bytes, &native_path) {
            Ok(()) => reporter.detail(&format!(
                "packed native binary -> {} ({})",
                native_path.display(),
                format_bytes(
                    std::fs::metadata(&native_path)
                        .map(|m| m.len() as usize)
                        .unwrap_or(0)
                )
            )),
            Err(e) => {
                reporter.error_line(&format!("error packing native binary: {}", e));
                reporter.step(StepStatus::Fail, "build", "native pack error");
                std::process::exit(1);
            }
        }
    }

    // If --html flag, generate index.html + fai-runtime.js in the same directory
    if generate_html {
        let out_dir = std::path::Path::new(&output_path).parent().unwrap();
        let wasm_filename = std::path::Path::new(&output_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();

        // Write the runtime JS as a separate file
        let runtime_path = out_dir.join("fai-runtime.js");
        let runtime_js = generate_runtime_js(wasm_filename);
        match std::fs::write(&runtime_path, &runtime_js) {
            Ok(_) => reporter.detail(&format!("generated {}", runtime_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                runtime_path.display(),
                e
            )),
        }

        // Write the default forui stylesheet next to the runtime.
        // User-facing modifier styles still win via inline style
        // attributes — this is just the component base look.
        let css_path = out_dir.join("forui.css");
        let css = generate_forui_css();
        match std::fs::write(&css_path, &css) {
            Ok(_) => reporter.detail(&format!("generated {}", css_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                css_path.display(),
                e
            )),
        }

        // Write a minimal HTML file that loads the runtime
        let html_path = out_dir.join("index.html");
        let html = generate_html_page();
        match std::fs::write(&html_path, &html) {
            Ok(_) => reporter.detail(&format!(
                "generated {} (open in browser)",
                html_path.display()
            )),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                html_path.display(),
                e
            )),
        }
    }

    // If `[remote-interface] expose = true`, extract the package's
    // public interface spec and write it alongside the build output.
    // Peer packages pin against `interface.hash` so changes to the
    // shared surface surface as loud 401s rather than silent drift.
    // Plan 99 Phase 2.3.
    if info.interface_expose {
        let prepared = match fai_compiler::prepare_source(&content, source_root.as_deref()) {
            Ok(p) => p,
            Err(e) => {
                reporter.error_line(&format!(
                    "warning: interface expose: prepare failed — {}",
                    e
                ));
                return;
            }
        };
        let spec =
            interface::extract_interface(&info.name, &info.version, &prepared.serde_ast.statements);
        let out_dir = std::path::Path::new(&output_path)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let json_path = out_dir.join("interface.json");
        let hash_path = out_dir.join("interface.hash");
        match std::fs::write(&json_path, interface::spec_to_json(&spec)) {
            Ok(_) => reporter.detail(&format!(
                "generated {} (hash: {})",
                json_path.display(),
                spec.hash
            )),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                json_path.display(),
                e
            )),
        }
        match std::fs::write(&hash_path, &spec.hash) {
            Ok(_) => reporter.detail(&format!("generated {}", hash_path.display())),
            Err(e) => reporter.error_line(&format!(
                "warning: could not write {}: {}",
                hash_path.display(),
                e
            )),
        }
    }

    // Final step summary. Everything above emitted detail/warning
    // lines; the user-facing roll-up is one line per target.
    let mut summary_parts: Vec<String> = Vec::new();
    let wasm_stem = std::path::Path::new(&output_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&output_path);
    summary_parts.push(format!("{} {}", wasm_stem, format_bytes(wasm_bytes.len())));
    if content.contains("remote def") || content.contains("remote type") {
        summary_parts.push("schema.json".to_string());
    }
    if generate_html {
        summary_parts.push("assets".to_string());
    }
    if build_native {
        summary_parts.push("native binary".to_string());
    }
    reporter.step(StepStatus::Ok, "build", &summary_parts.join(" + "));
}

/// Human-readable byte size for build-output summary lines.
/// `153224` → `"150 KB"`, `203957` → `"199 KB"`, `5_500_000` → `"5.2 MB"`.
/// Kept in the CLI crate (not a general util) because the only callers
/// are the build-step summary lines — we don't want to grow it into a
/// full size-formatter dependency.
fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{} B", n)
    } else if n < 1024 * 1024 {
        format!("{} KB", n / 1024)
    } else {
        // One decimal place for MB so `5.2 MB` reads naturally.
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn cmd_interface(args: &[String]) {
    let path = require_file_arg(args, "interface");
    let content = read_file(&path);
    let source_root = find_source_root(&path);

    let prepared = match fai_compiler::prepare_source(&content, source_root.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    // Read project name and version from fai.toml
    let (proj_name, proj_version, _) = read_project_info(source_root.as_deref());

    let spec =
        interface::extract_interface(&proj_name, &proj_version, &prepared.serde_ast.statements);

    let json = interface::spec_to_json(&spec);

    // Output to file or stdout
    if let Some(pos) = args.iter().position(|a| a == "-o") {
        if let Some(output_path) = args.get(pos + 1) {
            match std::fs::write(output_path, &json) {
                Ok(_) => eprintln!(
                    "interface spec written to {} (hash: {})",
                    output_path, spec.hash
                ),
                Err(e) => {
                    eprintln!("error writing {}: {}", output_path, e);
                    std::process::exit(1);
                }
            }
        } else {
            eprintln!("-o requires an output path");
            std::process::exit(1);
        }
    } else {
        println!("{}", json);
    }
}

/// Check `argv[0]` for an appended wasm + magic trailer. Returns the
/// wasm bytes if found, `None` otherwise.
///
/// Trailer layout (reading from end of file backwards):
///   bytes [N-8 .. N]   = wasm_len (u64 little-endian)
///   bytes [N-16 .. N-8] = NATIVE_TRAILER_MAGIC
///   bytes [N-16-wasm_len .. N-16] = wasm payload
fn read_embedded_wasm() -> Option<Vec<u8>> {
    let self_path = std::env::current_exe().ok()?;
    let bytes = std::fs::read(&self_path).ok()?;
    if bytes.len() < 16 {
        return None;
    }
    let n = bytes.len();
    let magic_start = n - 16;
    let len_start = n - 8;
    if &bytes[magic_start..len_start] != NATIVE_TRAILER_MAGIC {
        return None;
    }
    let len_bytes: [u8; 8] = bytes[len_start..n].try_into().ok()?;
    let wasm_len = u64::from_le_bytes(len_bytes) as usize;
    if wasm_len == 0 || wasm_len + 16 > n {
        return None;
    }
    let wasm_start = n - 16 - wasm_len;
    Some(bytes[wasm_start..magic_start].to_vec())
}

/// Produce a self-extracting native binary by copying the current
/// forai binary and appending the compiled wasm + a trailer. The
/// resulting file, when executed, loads its own tail and runs the
/// embedded wasm via wasmtime. Plan 99 Phase 3.2.
///
/// Returns Ok(path) on success. Errors bubble up from filesystem
/// operations; caller decides whether to warn or exit.
fn pack_native_binary(wasm_bytes: &[u8], output_path: &std::path::Path) -> Result<(), String> {
    // Test override: cargo test runs this code inside the test
    // binary, so `current_exe()` returns the test harness, not the
    // forai binary. Tests set FORAI_SELF_BINARY to the real forai
    // path (usually target/debug/forai).
    let self_path = if let Ok(override_path) = std::env::var("FORAI_SELF_BINARY") {
        std::path::PathBuf::from(override_path)
    } else {
        std::env::current_exe().map_err(|e| format!("could not locate forai binary: {}", e))?
    };
    let forai_bytes = std::fs::read(&self_path).map_err(|e| {
        format!(
            "could not read forai binary at {}: {}",
            self_path.display(),
            e
        )
    })?;
    let wasm_len = wasm_bytes.len() as u64;
    let mut out = Vec::with_capacity(forai_bytes.len() + wasm_bytes.len() + 16);
    out.extend_from_slice(&forai_bytes);
    out.extend_from_slice(wasm_bytes);
    out.extend_from_slice(NATIVE_TRAILER_MAGIC);
    out.extend_from_slice(&wasm_len.to_le_bytes());
    std::fs::write(output_path, out)
        .map_err(|e| format!("could not write {}: {}", output_path.display(), e))?;
    // chmod +x on Unix — the copied forai binary already has x bits
    // but some filesystems may strip them, and the permissions from
    // a fresh write() don't always mirror the source file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(output_path)
            .map_err(|e| format!("chmod: read metadata failed: {}", e))?
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(output_path, perms)
            .map_err(|e| format!("chmod: set_permissions failed: {}", e))?;
    }
    Ok(())
}

/// `[remote-interface] from = "PeerName"` → locate the peer's
/// interface.hash file (written by its own build with
/// `expose = true`) and inject a `peerHash()` function into the
/// consumer's source so user code can reference the live hash
/// instead of hand-writing a constant. Plan 99 Phase 2.4.
///
/// Applied by both `cmd_build` (when compiling to wasm) and `cmd_run`
/// (when invoking through the bytecode VM) so the injection is
/// invisible to the user regardless of execution path.
fn inject_peer_hash(
    content: &mut String,
    info: &ProjectInfo,
    source_root: Option<&str>,
    verbose: bool,
) {
    let Some(peer_name) = info.interface_from.as_deref() else {
        return;
    };
    match locate_peer_interface_hash(source_root, peer_name) {
        Some(hash) => {
            let stub = format!(
                "\n# Auto-generated by forai build/run from [remote-interface] from = \"{peer}\".\n\
                 # Returns the peer package's current interface hash.\n\
                 def peerHash\n    @return String\ndo\n  '{hash}'\nend\n",
                peer = peer_name,
                hash = hash
            );
            content.push_str(&stub);
            if verbose {
                eprintln!("  injected peerHash() = \"{}\" (from {})", hash, peer_name);
            }
        }
        None => {
            if verbose {
                eprintln!(
                    "warning: [remote-interface] from = \"{}\" — peer's interface.hash not found. \
                     Did you build the peer with [remote-interface] expose = true?",
                    peer_name
                );
            }
        }
    }
}

/// Find the `interface.hash` file produced by a peer package whose
/// `[project] name = "<peer_name>"` matches. Walks the consumer's
/// fai.toml `[dependencies]` to find the peer's project root, then
/// reads the hash from the conventional build output location
/// (currently `<peer_root>/src/interface.hash` — the default output
/// directory when no build_dir is set).
fn locate_peer_interface_hash(
    consumer_source_root: Option<&str>,
    peer_name: &str,
) -> Option<String> {
    let consumer_root = consumer_source_root.and_then(|sr| std::path::Path::new(sr).parent())?;
    let toml_path = consumer_root.join("fai.toml");
    let toml_content = std::fs::read_to_string(&toml_path).ok()?;

    let mut in_deps = false;
    for line in toml_content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        // `"file:///abs/path" = "0.1.0"` — extract the path.
        let Some((k, _v)) = t.split_once('=') else {
            continue;
        };
        let dep_spec = k.trim().trim_matches('"');
        let Some(path_str) = dep_spec.strip_prefix("file://") else {
            continue;
        };
        let dep_root = std::path::Path::new(path_str);

        // Read the dep's project name to match against peer_name.
        let dep_info =
            read_project_info_full(Some(dep_root.join("src").to_str().unwrap_or(path_str)));
        if dep_info.name != peer_name {
            continue;
        }

        // Peer matched — look for its interface.hash. Candidate
        // locations, in order: build_dir (if set), else src/.
        let build_dir = dep_info.build_dir.as_deref().unwrap_or("src");
        let hash_path = dep_root.join(build_dir).join("interface.hash");
        if let Ok(h) = std::fs::read_to_string(&hash_path) {
            return Some(h.trim().to_string());
        }
    }
    None
}

/// Plan 101: Generate RPC proxy code for remote dependencies.
/// Returns a list of synthetic modules (name, source) that should
/// be injected into the compiler. Any file can then
/// `use { getTasks } from Remote` to access the generated proxies.
pub(crate) fn generate_rpc_proxy_modules(source_root: Option<&str>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    // Find the workspace root: walk up from source_root to find a
    // fai.toml that has sub-project definitions. We may pass through
    // per-project fai.toml files (old layout) before reaching the
    // workspace root.
    let project_root = match source_root {
        Some(sr) => {
            let mut dir = std::path::Path::new(sr).to_path_buf();
            let mut found = None;
            loop {
                let toml_path = dir.join("fai.toml");
                if toml_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&toml_path) {
                        let candidate = parse_project_info(&content);
                        if !candidate.sub_projects.is_empty() {
                            found = Some(dir.clone());
                            break;
                        }
                    }
                }
                if !dir.pop() {
                    break;
                }
            }
            match found {
                Some(root) => root,
                None => return result,
            }
        }
        None => return result,
    };
    let workspace_toml = project_root.join("fai.toml");
    let workspace_info = match std::fs::read_to_string(&workspace_toml) {
        Ok(content) => parse_project_info(&content),
        Err(_) => return result,
    };

    for (_sub_name, sub) in &workspace_info.sub_projects {
        for (dep_name, env_configs) in &sub.remote_deps {
            let url = match env_configs.get("dev") {
                Some(config) => &config.url,
                None => match env_configs.values().next() {
                    Some(config) => &config.url,
                    None => continue,
                },
            };

            let schema_json = find_server_schema(&project_root, dep_name, &workspace_info);
            let proxies = if let Some(schema) = schema_json {
                rpc_proxy::generate_proxies_from_schema(&schema, url).ok()
            } else {
                let source = find_dependency_source(&project_root, dep_name, &workspace_info);
                let hash = find_dependency_hash(&project_root, dep_name, &workspace_info)
                    .unwrap_or_default();
                source.and_then(|s| rpc_proxy::generate_proxies(&s, url, &hash).ok())
            };

            if let Some(proxies) = proxies {
                if !proxies.trim().is_empty() {
                    let module_name = capitalize_first(dep_name);
                    if is_verbose() {
                        eprintln!(
                            "    generated RPC proxies as '{}' module (url: {})",
                            module_name, url
                        );
                    }
                    result.push((module_name, proxies));
                }
            }
        }
    }
    result
}

/// Plan 101 Phase 4: Inject generated RPC dispatch for server sub-projects.
/// If the server sub-project serves a shared dependency (has remote deps
/// defined on a sibling client sub-project), generate the dispatch, handler,
/// and serve() function from the shared module's remote functions.
fn inject_rpc_dispatch(content: &mut String, _info: &ProjectInfo, source_root: Option<&str>) {
    // Find workspace root with sub-projects
    let project_root = match source_root {
        Some(sr) => {
            let mut dir = std::path::Path::new(sr).to_path_buf();
            let mut found = None;
            loop {
                let toml_path = dir.join("fai.toml");
                if toml_path.exists() {
                    if let Ok(toml_content) = std::fs::read_to_string(&toml_path) {
                        let candidate = parse_project_info(&toml_content);
                        if !candidate.sub_projects.is_empty() {
                            found = Some(dir.clone());
                            break;
                        }
                    }
                }
                if !dir.pop() {
                    break;
                }
            }
            match found {
                Some(r) => r,
                None => return,
            }
        }
        None => return,
    };

    let workspace_toml = project_root.join("fai.toml");
    let workspace_info = match std::fs::read_to_string(&workspace_toml) {
        Ok(c) => parse_project_info(&c),
        Err(_) => return,
    };

    // Find any sub-project that has a remote dep — the dep name is the
    // shared module we need to generate dispatch for. The SERVER is the
    // sub-project that IMPLEMENTS those functions (target = native).
    // We inject dispatch into the current build if it's a server target.
    // Only inject dispatch if the source uses the RPC API.
    // Support both the new addRpcRoutes pattern and the legacy startRpcServer.
    let uses_rpc = content.contains("addRpcRoutes") || content.contains("startRpcServer");
    if !uses_rpc {
        return;
    }

    // Server is the source of truth — generate dispatch from its OWN
    // remote def functions. No shared module needed.
    match rpc_dispatch::generate_dispatch(content, "") {
        Ok(dispatch) => {
            if !dispatch.trim().is_empty() {
                if is_verbose() {
                    eprintln!("    generated RPC dispatch (addRpcRoutes + handler + dispatch)");
                }
                content.push('\n');
                content.push_str(&dispatch);
            } else {
                // The server source calls addRpcRoutes/startRpcServer but has no
                // 'remote def' functions for the dispatcher to route to. Without
                // this check, the build fails later with a cryptic "unknown
                // function 'addRpcRoutes'" — this tells agents exactly what
                // to add, and where.
                eprintln!(
                    "error: this server calls addRpcRoutes but declares no 'remote def' functions"
                );
                eprintln!(
                    "       addRpcRoutes is auto-generated — and only generated when the server"
                );
                eprintln!(
                    "       exposes at least one function. Mark each function you want to expose"
                );
                eprintln!("       to the client as 'remote def' (see `fai_examples rpc`).");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("  warning: failed to generate dispatch: {}", e);
        }
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Find schema.json from a server sub-project's build dir.
fn find_server_schema(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    let sub = info.sub_projects.get(dep_name)?;
    // Check build_dir first
    if let Some(bd) = &sub.build_dir {
        let schema = project_root.join(bd).join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    // Check next to the main file
    if let Some(main) = &sub.main {
        let main_dir = project_root.join(main).parent()?.to_path_buf();
        let schema = main_dir.join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    // Check source dir
    if let Some(src) = &sub.source {
        let schema = project_root.join(src).join("schema.json");
        if let Ok(content) = std::fs::read_to_string(&schema) {
            return Some(content);
        }
    }
    None
}

/// Find a sub-project dependency's source code by name.
///
/// Concatenates ALL .fai files from the server's source directory so that
/// `remote def` and `remote type` declarations spread across multiple files
/// are all visible to the proxy generator. Previously only the first file
/// found was read, causing silently incomplete proxies for multi-file servers.
fn find_dependency_source(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    // Check if the dep is a sibling sub-project with a source path
    if let Some(sub) = info.sub_projects.get(dep_name) {
        if let Some(src_dir) = &sub.source {
            let src_path = project_root.join(src_dir);
            // If source/dep_name/ subdirectory exists, search there first
            // (handles source="src" with src/server/ layout)
            let search_dirs = vec![src_path.join(dep_name), src_path.clone()];
            for dir in &search_dirs {
                if !dir.is_dir() {
                    continue;
                }
                let mut files: Vec<_> = std::fs::read_dir(dir)
                    .ok()?
                    .flatten()
                    .filter(|e| {
                        e.path().extension().map_or(false, |x| x == "fai")
                            && !e
                                .path()
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .starts_with("test")
                    })
                    .collect();
                if files.is_empty() {
                    continue;
                }
                // Sort so main.fai comes first (its imports define the RPC surface).
                files.sort_by_key(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name == "main.fai" {
                        0u8
                    } else {
                        1u8
                    }
                });
                // Concatenate all files so remote def/type across files are all visible.
                let combined: String = files
                    .iter()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect::<Vec<_>>()
                    .join("\n");
                if !combined.trim().is_empty() {
                    return Some(combined);
                }
            }
        }
    }

    // Fallback: look in conventional locations
    let fallback_paths = [
        project_root
            .join(dep_name)
            .join("src")
            .join(format!("{}.fai", dep_name)),
        project_root.join(dep_name).join("src").join("main.fai"),
    ];
    for p in &fallback_paths {
        if let Ok(content) = std::fs::read_to_string(p) {
            return Some(content);
        }
    }

    None
}

/// Find the interface hash for a dependency.
fn find_dependency_hash(
    project_root: &std::path::Path,
    dep_name: &str,
    info: &ProjectInfo,
) -> Option<String> {
    if let Some(sub) = info.sub_projects.get(dep_name) {
        if let Some(src_dir) = &sub.source {
            let hash_path = project_root.join(src_dir).join("interface.hash");
            if let Ok(h) = std::fs::read_to_string(&hash_path) {
                return Some(h.trim().to_string());
            }
        }
    }
    None
}

/// Find a project's fai.toml by walking up from the given directory
/// (typically cwd). Returns the directory containing the fai.toml.
fn find_project_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join("fai.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve the entry point .fai file for a named target. Convention:
/// 1. `main.fai` in the source dir
/// 2. `<target_name>.fai` (e.g. `server.fai` for target "server")
/// 3. Single .fai file if there's only one (ignoring test-* files)
/// The `target_name` hint helps disambiguate when multiple .fai files exist.
fn resolve_entry_point(
    project_root: &std::path::Path,
    source_dir: &str,
) -> Option<std::path::PathBuf> {
    resolve_entry_point_with_hint(project_root, source_dir, None)
}

fn resolve_entry_point_with_hint(
    project_root: &std::path::Path,
    source_dir: &str,
    target_name: Option<&str>,
) -> Option<std::path::PathBuf> {
    let src = project_root.join(source_dir);
    if !src.is_dir() {
        return None;
    }
    // 1. Prefer main.fai
    let main = src.join("main.fai");
    if main.is_file() {
        return Some(main);
    }
    // 2. Try <target_name>.fai or <target_name><anything>.fai
    if let Some(name) = target_name {
        // Exact: server.fai
        let exact = src.join(format!("{}.fai", name));
        if exact.is_file() {
            return Some(exact);
        }
        // Prefix match: todoserver.fai for target "server"
        if let Ok(entries) = std::fs::read_dir(&src) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().map_or(false, |e| e == "fai") {
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                    if stem.ends_with(name) && !stem.starts_with("test") {
                        return Some(p);
                    }
                }
            }
        }
    }
    // 3. Single non-test .fai file
    if let Ok(entries) = std::fs::read_dir(&src) {
        let candidates: Vec<_> = entries
            .flatten()
            .filter(|e| {
                let p = e.path();
                p.extension().map_or(false, |e| e == "fai")
                    && !p
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with("test")
            })
            .collect();
        if candidates.len() == 1 {
            return Some(candidates[0].path());
        }
    }
    None
}

/// Select which targets to build/run based on command args.
/// Returns (target_name, sub_project) pairs.
/// - No args: all targets (for build) or the single target (for run)
/// - Named arg: just that target
fn select_targets<'a>(
    info: &'a ProjectInfo,
    target_name: Option<&str>,
) -> Vec<(String, &'a SubProject)> {
    if info.sub_projects.is_empty() {
        // Single-project mode: return a synthetic "default" target
        return vec![];
    }
    match target_name {
        Some(name) => {
            if let Some(sub) = info.sub_projects.get(name) {
                vec![(name.to_string(), sub)]
            } else {
                eprintln!("error: unknown target '{}'. Available targets:", name);
                for k in info.sub_projects.keys() {
                    eprintln!("  - {}", k);
                }
                vec![]
            }
        }
        None => info
            .sub_projects
            .iter()
            .map(|(name, sub)| (name.clone(), sub))
            .collect(),
    }
}

/// Resolve entry point for a named target from the nearest fai.toml.
fn resolve_target_entry_point(target_name: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);
    let sub = info.sub_projects.get(target_name)?;
    // Prefer explicit main, fall back to convention-based resolution
    if let Some(main) = &sub.main {
        let entry = root.join(main);
        if entry.is_file() {
            return Some(entry.to_string_lossy().into_owned());
        }
    }
    let src = sub.source.as_ref()?;
    let entry = resolve_entry_point_with_hint(&root, src, Some(target_name))?;
    Some(entry.to_string_lossy().into_owned())
}

/// Resolve the default entry point when no target is specified.
/// For single-project apps, finds the .fai file in the source dir.
/// For multi-target projects, errors (must specify a target).
fn resolve_default_entry_point() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    resolve_default_entry_point_at(&root)
}

/// Pure variant of `resolve_default_entry_point` that takes the project
/// root explicitly. Extracted so tests can exercise the multi-target
/// error path without `std::env::set_current_dir` (which is a racy
/// operation under Rust's default multi-threaded test runner).
fn resolve_default_entry_point_at(root: &std::path::Path) -> Option<String> {
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);

    if !info.sub_projects.is_empty() {
        // Multi-target: if only one target, use it; otherwise require
        // -p / --project so the user picks a specific one. "fai run"
        // in a fullstack project has no sensible default — the server
        // and the client do different things.
        if info.sub_projects.len() == 1 {
            let (_, sub) = info.sub_projects.iter().next()?;
            let src = sub.source.as_ref()?;
            let entry = resolve_entry_point(root, src)?;
            return Some(entry.to_string_lossy().into_owned());
        }
        let n = info.sub_projects.len();
        eprintln!("error: --project required — this project has {} targets", n);
        eprintln!("usage:");
        let mut names: Vec<&String> = info.sub_projects.keys().collect();
        names.sort();
        for name in &names {
            eprintln!("  fai run --project {}", name);
        }
        return None;
    }

    // Single project: look for source = "src" or source_root convention
    let src = "src";
    let entry = resolve_entry_point(root, src)?;
    Some(entry.to_string_lossy().into_owned())
}

/// Iterate workspace members and run `cmd_build` on each one's entry
/// point. Members are relative directory paths from the workspace
/// root. Entry point resolution is convention-based for now (plan 99
/// Phase 2.2): `src/main.fai` if present, otherwise
/// `src/<name_lower>.fai`. A future `[[bin]]` table will let packages
/// declare their own entry explicitly.
fn cmd_build_workspace(root: &std::path::Path, members: &[String]) {
    eprintln!("building workspace with {} members", members.len());
    for m in members {
        let member_dir = root.join(m);
        if !member_dir.is_dir() {
            eprintln!(
                "  warning: workspace member '{}' — directory not found at {}",
                m,
                member_dir.display()
            );
            continue;
        }
        let info = read_project_info_full(Some(member_dir.to_str().unwrap()));
        let src_dir = member_dir.join("src");
        let main_candidate = src_dir.join("main.fai");
        let named_candidate = src_dir.join(format!("{}.fai", info.name.to_lowercase()));
        let entry = if main_candidate.is_file() {
            main_candidate
        } else if named_candidate.is_file() {
            named_candidate
        } else {
            eprintln!(
                "  warning: workspace member '{}' — no entry point found at {} or {}",
                m,
                main_candidate.display(),
                named_candidate.display()
            );
            continue;
        };
        eprintln!("\n▶ building member '{}' ({})", m, entry.display());
        cmd_build(&[entry.to_string_lossy().into_owned()]);
    }
}

/// Build target declared in `[project] target = "..."`.
#[derive(Debug, Clone, PartialEq)]
enum BuildTarget {
    /// Plain wasm output — the default. `forai run foo.wasm` loads it
    /// via wasmtime; servers typically run this.
    Wasm,
    /// Wasm plus the browser bundle (index.html, fai-runtime.js,
    /// forui.css). Equivalent to the historical `--html` CLI flag.
    WasmHtml,
    /// Native binary — bundles wasm + wasmtime into a single
    /// executable. Not implemented yet (plan 99 Phase 3); setting
    /// this in a fai.toml produces a build error rather than silent
    /// misbehaviour.
    Native,
}

impl BuildTarget {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "wasm" => Some(BuildTarget::Wasm),
            "wasm-html" => Some(BuildTarget::WasmHtml),
            "native" => Some(BuildTarget::Native),
            _ => None,
        }
    }
}

/// Per-environment remote service configuration.
#[derive(Debug, Clone, PartialEq)]
struct RemoteEnvConfig {
    url: String,
}

/// A sub-project within a workspace (e.g. `[project.client]`).
#[derive(Debug, Default, Clone)]
struct SubProject {
    target: Option<BuildTarget>,
    source: Option<String>,
    /// Explicit entry point file (relative to project root).
    main: Option<String>,
    build_dir: Option<String>,
    /// Remote config for dependencies, keyed by dependency name then environment.
    remote_deps:
        std::collections::HashMap<String, std::collections::HashMap<String, RemoteEnvConfig>>,
}

/// Everything `forai build` reads out of the project's `fai.toml` up
/// front. Workspace + remote-interface information is also picked up
/// here so the build path only touches the file once.
#[derive(Debug, Default, Clone)]
struct ProjectInfo {
    /// `[project].name`. Defaults to `"unknown"` when absent.
    name: String,
    /// `[project].version`. Defaults to `"0.0.0"` when absent.
    version: String,
    /// `[project].build_dir`. `None` falls back to `"public"` under
    /// `wasm-html` builds.
    build_dir: Option<String>,
    /// `[project].target`. `None` means `"wasm"`.
    target: Option<BuildTarget>,
    /// `[workspace].members` — relative paths to member package
    /// directories. Non-empty means this fai.toml IS a workspace root
    /// rather than a package. In that case the `[project]` section is
    /// typically empty.
    workspace_members: Vec<String>,
    /// `[remote-interface].expose = true`. Build pipeline writes an
    /// `interface.json` + `interface.hash` alongside the wasm so peer
    /// packages can pin against it.
    interface_expose: bool,
    /// `[remote-interface].from = "..."`. Consumer packages read
    /// their peer's exposed interface.hash at build time and bake it
    /// into a generated `apiHash()` constant.
    interface_from: Option<String>,
    /// Named sub-projects (e.g. `[project.client]`, `[project.server]`).
    sub_projects: std::collections::HashMap<String, SubProject>,
}

fn read_project_info(source_root: Option<&str>) -> (String, String, Option<String>) {
    let info = read_project_info_full(source_root);
    (info.name, info.version, info.build_dir)
}

/// Parse a fai.toml content string into a ProjectInfo. Extracted from
/// `read_project_info_full` for testability.
fn parse_project_info(content: &str) -> ProjectInfo {
    let mut info = ProjectInfo {
        name: "unknown".into(),
        version: "0.0.0".into(),
        ..Default::default()
    };

    let mut section = String::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(rest) = t.strip_prefix('[') {
            if let Some(name) = rest.strip_suffix(']') {
                section = name.trim().to_string();
            }
            continue;
        }
        let Some((k_raw, v_raw)) = t.split_once('=') else {
            continue;
        };
        let k = k_raw.trim();
        let v = v_raw.trim();
        let v_unquoted = v.trim_matches('"').to_string();

        // Check for sub-project sections: [project.client], [project.server], etc.
        if let Some(sub_name) = section.strip_prefix("project.") {
            // Could be [project.client] or [project.client.dependencies.shared.remote.dev]
            let parts: Vec<&str> = sub_name.split('.').collect();
            let sub_key = parts[0];
            let sub = info
                .sub_projects
                .entry(sub_key.to_string())
                .or_insert_with(SubProject::default);

            if parts.len() == 1 {
                // [project.client] — direct sub-project fields
                match k {
                    "target" => sub.target = BuildTarget::parse(&v_unquoted),
                    "source" => sub.source = Some(v_unquoted),
                    "main" => sub.main = Some(v_unquoted),
                    "build_dir" => sub.build_dir = Some(v_unquoted),
                    _ => {}
                }
            } else if parts.len() >= 4 && parts[1] == "dependencies" && parts[3] == "remote" {
                // [project.client.dependencies.shared.remote.dev]
                let dep_name = parts[2];
                let env_name = parts.get(4).unwrap_or(&"dev");
                match k {
                    "url" => {
                        let env_map = sub
                            .remote_deps
                            .entry(dep_name.to_string())
                            .or_insert_with(std::collections::HashMap::new);
                        let config = env_map
                            .entry(env_name.to_string())
                            .or_insert_with(|| RemoteEnvConfig { url: String::new() });
                        config.url = v_unquoted;
                    }
                    _ => {}
                }
            }
            continue;
        }

        match section.as_str() {
            "project" => match k {
                "name" => info.name = v_unquoted,
                "version" => info.version = v_unquoted,
                "build_dir" => info.build_dir = Some(v_unquoted),
                "source" => { /* root-level source — for single-project */ }
                "target" => {
                    info.target = BuildTarget::parse(&v_unquoted);
                }
                _ => {}
            },
            "workspace" => {
                if k == "members" {
                    let inner = v.trim_start_matches('[').trim_end_matches(']');
                    for elem in inner.split(',') {
                        let m = elem.trim().trim_matches('"');
                        if !m.is_empty() {
                            info.workspace_members.push(m.to_string());
                        }
                    }
                }
            }
            "remote-interface" => match k {
                "expose" => info.interface_expose = v == "true",
                "from" => info.interface_from = Some(v_unquoted),
                _ => {}
            },
            _ => {}
        }
    }

    info
}

/// Parses fai.toml from a source root directory. Delegates to
/// `parse_project_info` for the actual parsing.
fn read_project_info_full(source_root: Option<&str>) -> ProjectInfo {
    let Some(root) = source_root else {
        return ProjectInfo {
            name: "unknown".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
    };
    let src_path = std::path::Path::new(root);
    let toml_path = if src_path.join("fai.toml").exists() {
        src_path.join("fai.toml")
    } else if let Some(parent) = src_path.parent() {
        parent.join("fai.toml")
    } else {
        src_path.join("fai.toml")
    };
    let Ok(content) = std::fs::read_to_string(&toml_path) else {
        return ProjectInfo {
            name: "unknown".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
    };

    parse_project_info(&content)
}

fn generate_html_loader_old(wasm_filename: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>FAI</title>
<style>*{{margin:0;padding:0;box-sizing:border-box}}body{{font-family:-apple-system,system-ui,sans-serif}}</style>
</head>
<body>
<pre id="output" style="display:none"></pre>
<script>
const output = document.getElementById('output');
function readStr(ptr, len) {{
  return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer, ptr, len));
}}
function writeStr(ptr, str) {{
  const bytes = new TextEncoder().encode(str);
  new Uint8Array(instance.exports.memory.buffer, ptr).set(bytes);
  return bytes.length;
}}
const env = {{
  print(ptr, len) {{
    output.style.display = 'block';
    output.textContent += readStr(ptr, len) + '\n';
  }},
  read_file() {{ return -1; }},
  write_file() {{ return -1; }},
  now_ms() {{ return Date.now(); }},
  random() {{ return Math.random(); }},
  sleep_ms() {{}},
  call_ffi() {{ return 0x7FFC000100000000n; }},
  run_all() {{ return 0x7FFC000100000000n; }},
  spawn(closureVal) {{var n=BigInt(closureVal);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return 0x7FFC000200000000n;var tag=dv.getInt32(a,true);if(tag!==4)return 0x7FFC000200000000n;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}return 0x7FFC000200000000n}},
  http_post(url_ptr, url_len, body_ptr, body_len, result_buf_ptr) {{
    const url = readStr(url_ptr, url_len);
    const body = readStr(body_ptr, body_len);
    try {{
      const xhr = new XMLHttpRequest();
      xhr.open('POST', url, false);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.send(body);
      if (xhr.status >= 200 && xhr.status < 300) {{
        return writeStr(result_buf_ptr, xhr.responseText);
      }}
      return writeStr(result_buf_ptr, '{{"ok":false,"error":"HTTP ' + xhr.status + '"}}');
    }} catch(e) {{
      return writeStr(result_buf_ptr, '{{"ok":false,"error":"' + e.message + '"}}');
    }}
  }},
  set_html(ptr, len) {{
    document.body.innerHTML = readStr(ptr, len);
  }},
  remote_call(url_ptr, url_len, fn_ptr, fn_len, args_ptr, args_len, hash_ptr, hash_len, result_buf_ptr) {{
    const url = readStr(url_ptr, url_len);
    const fn_name = readStr(fn_ptr, fn_len);
    const args = readStr(args_ptr, args_len);
    const hash = readStr(hash_ptr, hash_len);
    const body = JSON.stringify({{fn: fn_name, args: JSON.parse(args || '[]'), hash: hash}});
    try {{
      const xhr = new XMLHttpRequest();
      xhr.open('POST', url + '/fai/rpc', false);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.send(body);
      const resp = JSON.parse(xhr.responseText);
      if (resp.ok) {{
        const val = JSON.stringify(resp.value);
        return writeStr(result_buf_ptr, val);
      }}
      return writeStr(result_buf_ptr, '{{"ok":false,"error":"' + (resp.error || 'unknown') + '"}}');
    }} catch(e) {{
      return writeStr(result_buf_ptr, '{{"ok":false,"error":"' + e.message + '"}}');
    }}
  }},
  storage_get(kp,kl,bp){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return -1;const b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_set(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear(){{try{{window.localStorage.clear()}}catch(e){{}}}}
}};
let instance;
WebAssembly.instantiateStreaming(fetch('/{}'), {{ env }}).then(result => {{
  instance = result.instance;
  instance.exports._start();
}});
</script>
</body>
</html>"#,
        wasm_filename
    )
}

fn generate_html_loader(wasm_filename: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>FAI</title>
<style>*{{margin:0;padding:0;box-sizing:border-box}}body{{font-family:-apple-system,system-ui,sans-serif;background:#fafafa;color:#1a1a1a;font-size:16px;display:flex;justify-content:center;padding-top:48px}}#app{{min-width:200px}}</style>
</head>
<body>
<div id="app"></div>
<pre id="output" style="display:none"></pre>
<script>
const app=document.getElementById('app'),output=document.getElementById('output');
let instance,state='{{}}';
const FAI_DEBUG=window.__FAI_DEBUG__===true||new URLSearchParams(location.search).get('fai_debug')==='1'||localStorage.getItem('fai_debug')==='1';
function debugLog(...args){{if(FAI_DEBUG)console.log(...args)}}
const QNAN=0x7FFC000000000000n,SIGN=0x8000000000000000n,OBJ_MASK=QNAN|SIGN;
const TAG_INT=0x0004000000000000n,TAG_BOOL=0x0003000000000000n,TAG_NULL=0x0001000000000000n;
const NULL_VAL=QNAN|TAG_NULL,INT_MASK=QNAN|TAG_INT,BOOL_MASK=QNAN|TAG_BOOL;
function jsToWasm(v){{
  if(v===null||v===undefined)return QNAN|TAG_NULL;
  if(typeof v==='boolean')return QNAN|TAG_BOOL|BigInt(v?1:0);
  if(typeof v==='number'){{if(Number.isInteger(v))return QNAN|TAG_INT|BigInt.asUintN(32,BigInt(v));const buf=new ArrayBuffer(8);new Float64Array(buf)[0]=v;return new BigInt64Array(buf)[0]}}
  if(typeof v==='string')return writeStrToWasm(v);
  if(Array.isArray(v)){{const m=instance.exports.memory.buffer;const dv=new DataView(m);const h=instance.exports.__heap_ptr.value;const addr=h;const end=(addr+8+v.length*8+7)&~7;instance.exports.__heap_ptr.value=end;dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);const items=v.map(i=>jsToWasm(i));for(let i=0;i<items.length;i++){{const bi=new BigInt64Array(m,addr+8+i*8,1);bi[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{const keys=Object.keys(v);const m=instance.exports.memory.buffer;const dv=new DataView(m);const h=instance.exports.__heap_ptr.value;const addr=h;const cap=Math.max(keys.length,16);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);instance.exports.__heap_ptr.value=(addr+8+cap*16+7)&~7;for(let i=0;i<keys.length;i++){{const kv=writeStrToWasm(keys[i]);const vv=jsToWasm(v[keys[i]]);const ea=addr+8+i*16;const bi=new BigInt64Array(m,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
  return QNAN|TAG_NULL;
}}
function wasmToJs(v){{
  // NaN-box tag discrimination. TAG_INT (0x0004) overlaps with QNAN's
  // bit 50 so `(n & INT_MASK) === INT_MASK` matches EVERY NaN-boxed
  // value. Order the checks so more-specific patterns (object, bool,
  // null) win before the Int fallback. Mirrors fai-core/src/value.rs
  // `is_int` / `is_object` semantics.
  const n=BigInt(v);
  if(n===NULL_VAL)return null;
  if((n&OBJ_MASK)===OBJ_MASK){{const a=Number(n&0x0000FFFFFFFFFFFFn);const dv=new DataView(instance.exports.memory.buffer);const tag=dv.getInt32(a,true);
    if(tag===0){{const l=dv.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}
    if(tag===1){{const cnt=dv.getInt32(a+4,true);const r=[];for(let i=0;i<cnt;i++){{const bi=new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1);r.push(wasmToJs(bi[0]))}}return r}}
    if(tag===3){{const cnt=dv.getInt32(a+4,true);const r={{}};for(let i=0;i<cnt;i++){{const ea=a+8+i*16;const bi=new BigInt64Array(instance.exports.memory.buffer,ea,2);const k=wasmToJs(bi[0]);const val=wasmToJs(bi[1]);if(typeof k==='string')r[k]=val}}return r}}
    // Fall through for other tags (Tuple, Closure, NativeFn, Instance, Module).
    return null;
  }}
  if((n&BOOL_MASK)===BOOL_MASK)return(n&1n)===1n;
  // Int: high 16 bits == QNAN (0x7FFC), sign bit clear, not null/bool.
  if((n&QNAN)===QNAN)return Number(BigInt.asIntN(32,n&0xFFFFFFFFn));
  // Raw f64 (non-NaN) — reinterpret bits as double.
  const buf=new ArrayBuffer(8);new BigInt64Array(buf)[0]=n;return new Float64Array(buf)[0];
}}
function readStr(p,l){{return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,p,l))}}
function writeStr(p,s){{const b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p).set(b);return b.length}}
function writeStrToWasm(s){{const b=new TextEncoder().encode(s);const h=instance.exports.__heap_ptr.value;const m=new Uint8Array(instance.exports.memory.buffer);const d=new DataView(instance.exports.memory.buffer);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{const n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{const a=Number(n&0x0000FFFFFFFFFFFFn);const d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{const l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function invokeExport(name,...args){{const fn=instance.exports[name];if(!fn){{console.warn('FAI invokeExport missing export', name);return;}}debugLog('FAI invokeExport:start', {{name,args}});try{{const result=fn(...args);debugLog('FAI invokeExport:end', {{name,result}});return result;}}catch(e){{console.error('FAI invokeExport:failed', {{name,args,error:e}});throw e;}}}}
function callWasm(name,arg){{const fn=instance.exports[name];if(!fn){{console.warn('FAI callWasm missing export', name);return;}}console.log('FAI callWasm', {{name,arg:arg||''}});const ptr=writeStrToWasm(arg||'');const result=invokeExport(name,ptr);return readNanBoxedStr(result)}}
function rerender(stateArg){{debugLog('FAI rerender', {{stateArg:stateArg||''}});if(instance.exports.render){{callWasm('render',stateArg||'')}}else if(instance&&instance.exports&&instance.exports._start){{const result=invokeExport('_start');const s=readNanBoxedStr(result);if(s&&s.startsWith('{{'))state=s;}}else{{console.warn('FAI rerender missing render and _start')}}}}
function wireEvents(){{debugLog('FAI wireEvents');document.querySelectorAll('[data-fai-click]').forEach(el=>{{const h=el.getAttribute('data-fai-click');el.onclick=()=>{{console.log('FAI click', h);callWasm(h);rerender('')}}}});document.querySelectorAll('[data-fai-input]').forEach(el=>{{const h=el.getAttribute('data-fai-input');el.oninput=()=>{{const d=JSON.stringify({{_state:JSON.parse(state||'{{}}'),_value:el.value}});console.log('FAI input', {{handler:h,value:el.value}});state=callWasm(h,d);rerender(state)}}}})}}
function handleEvent(id){{const fn=instance.exports.invokeHandler;if(!fn){{console.warn('FAI handleEvent: invokeHandler not exported');return;}}const boxed=BigInt(id)|0x7FFC000400000000n;debugLog('FAI handleEvent',{{id}});try{{fn(boxed)}}catch(e){{console.error('FAI handleEvent failed',{{id,error:e}})}}}}
function handleInputEvent(id,value){{const fn=instance.exports.invokeChangeHandler;if(!fn){{console.warn('FAI handleInputEvent: invokeChangeHandler not exported');return;}}const boxedId=BigInt(id)|0x7FFC000400000000n;const boxedStr=writeStrToWasm(value);debugLog('FAI handleInputEvent',{{id,value}});try{{fn(boxedId,boxedStr)}}catch(e){{console.error('FAI handleInputEvent failed',{{id,error:e}})}}}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}var used={{}};for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];used[nk]=true;if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}for(var i=oc.length-1;i>=0;i--){{var k=oc[i].nodeType===1?oc[i].getAttribute('data-fai-key'):null;if(k&&!used[k])op.removeChild(oc[i])}}}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
const env={{
  print(p,l){{const text=readStr(p,l);debugLog('FAI print', text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file(){{return -1}},write_file(){{return -1}},now_ms(){{return Date.now()}},random(){{return Math.random()}},sleep_ms(){{}},
  call_ffi(){{return 0x7FFC000100000000n}},run_all(){{return 0x7FFC000100000000n}},
  spawn(closureVal){{var n=BigInt(closureVal);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return 0x7FFC000200000000n;var tag=dv.getInt32(a,true);if(tag!==4)return 0x7FFC000200000000n;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}return 0x7FFC000200000000n}},
  http_post(a,b,c,d,e){{try{{const x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html(p,l){{const html=readStr(p,l);console.log('FAI set_html', {{length:l}});debugLog('FAI set_html:preview', html.slice(0,240));morphDom(app,html,false);wireEvents()}},
  set_html_at(a,b,p,l){{const selector=readStr(a,b);const html=readStr(p,l);let root=document.querySelector(selector);if(!root&&selector.startsWith('#')){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root);}}if(!root){{console.error('FAI set_html_at missing root', selector);return;}}console.log('FAI set_html_at', {{selector,length:l}});debugLog('FAI set_html_at:preview', {{selector,html:html.slice(0,240)}});morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse(p,l){{try{{const s=readStr(p,l);const v=JSON.parse(s);return jsToWasm(v)}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify(v){{try{{const j=wasmToJs(v);return writeStrToWasm(JSON.stringify(j))}}catch(e){{return writeStrToWasm('null')}}}},
  remote_call(a,b,c,d,e,f,g,h){{const u=readStr(a,b),fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);const body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});console.log('FAI remote_call request', {{url:u,fn:fn_name,args:ar,hash:ha}});try{{const x=new XMLHttpRequest();x.open('POST',u.replace(/\/+$/,'')+'/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body);const resp=JSON.parse(x.responseText);console.log('FAI remote_call response', {{fn:fn_name,ok:resp.ok,value:resp.value,error:resp.error}});if(resp.ok)return jsToWasm(resp.value);console.warn('FAI remote_call returned error', resp);return NULL_VAL}}catch(e){{console.error('FAI remote_call failed', e);return NULL_VAL}}}},
  float_to_str(v,p){{const s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);const b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  storage_get(kp,kl,bp){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return -1;const b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_set(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear(){{try{{window.localStorage.clear()}}catch(e){{}}}}
}};
fetch('/{}').then(r=>r.arrayBuffer()).then(b=>WebAssembly.instantiate(b,{{env}})).then(r=>{{
  instance=r.instance;debugLog('FAI wasm instantiated', Object.keys(instance.exports));const result=invokeExport('_start');const s=readNanBoxedStr(result);if(s&&s.startsWith('{{'))state=s;
}}).catch(e=>{{app.innerHTML='<p style="color:red;padding:20px">Error: '+e.message+'</p>'}});
</script>
</body>
</html>"#,
        wasm_filename
    )
}

fn generate_html_page() -> String {
    r#"<!DOCTYPE html>
<html>
<head><meta charset="utf-8"><title>FAI</title>
<link rel="stylesheet" href="forui.css">
</head>
<body>
<div id="app"></div>
<pre id="output" style="display:none"></pre>
<script src="fai-runtime.js"></script>
</body>
</html>"#
        .to_string()
}

/// Default forui stylesheet. Shipped alongside the runtime JS by
/// `forai build --html`. Emits iOS-leaning defaults for every
/// component kind the html-forui renderer supports.
///
/// Components opt in via the `fai-<kind>` class the renderer emits
/// (e.g. `fai-vstack`, `fai-button`, `fai-segmented`). User-facing
/// modifier styles (padding/background/foreground/...) remain inline
/// and override these defaults by CSS specificity (inline > class).
fn generate_forui_css() -> String {
    r#"/* forui default stylesheet — iOS-leaning defaults. */
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,sans-serif;background:#f2f2f7;color:#1c1c1e;font-size:16px;display:flex;justify-content:center;padding:0}
#app{width:100%;min-width:280px}

/* ── Layout primitives ──────────────────────────────────────── */
.fai-vstack{display:flex;flex-direction:column;align-items:center;gap:12px}
.fai-hstack{display:flex;flex-direction:row;align-items:center;gap:8px}
.fai-zstack{position:relative;display:grid}
.fai-zstack>*{grid-area:1/1}
.fai-scrollview{overflow:auto}
.fai-spacer{flex:1}
.fai-view{}

/* ── Typography ─────────────────────────────────────────────── */
.fai-label{line-height:1.4}

/* ── Controls ───────────────────────────────────────────────── */
.fai-button{
  padding:10px 20px;
  border:1px solid #d0d0d0;
  border-radius:8px;
  background:#fff;
  color:#1c1c1e;
  cursor:pointer;
  font-size:17px;
  font-family:inherit;
  transition:background 0.1s,opacity 0.1s;
}
.fai-button:hover{background:#f5f5f7}
.fai-button:active{opacity:0.7}

.fai-textinput{
  padding:10px 14px;
  border:1px solid #d0d0d0;
  border-radius:8px;
  background:#fff;
  font-size:16px;
  font-family:inherit;
  width:100%;
  box-sizing:border-box;
  outline:none;
  transition:border-color 0.1s;
}
.fai-textinput:focus{border-color:#007aff}

/* ── Toggle (iOS-style switch) ──────────────────────────────── */
.fai-toggle{
  --w:51px;
  --h:31px;
  --pad:2px;
  position:relative;
  width:var(--w);
  height:var(--h);
  border:none;
  border-radius:calc(var(--h)/2);
  background:#e9e9eb;
  cursor:pointer;
  padding:0;
  transition:background 0.2s;
  flex-shrink:0;
}
.fai-toggle[data-on="true"]{background:#34c759}
.fai-toggle::after{
  content:"";
  position:absolute;
  top:var(--pad);
  left:var(--pad);
  width:calc(var(--h) - 2*var(--pad));
  height:calc(var(--h) - 2*var(--pad));
  border-radius:50%;
  background:#fff;
  box-shadow:0 2px 4px rgba(0,0,0,0.2);
  transition:transform 0.2s;
}
.fai-toggle[data-on="true"]::after{transform:translateX(calc(var(--w) - var(--h)))}

/* ── Divider ────────────────────────────────────────────────── */
.fai-divider{
  width:100%;
  height:1px;
  border:none;
  background:#d0d0d5;
  margin:0;
}

/* ── SegmentedControl (iOS-style) ───────────────────────────── */
.fai-segmented{
  display:inline-flex;
  padding:2px;
  background:#e9e9eb;
  border-radius:9px;
  gap:2px;
  font-size:14px;
}
.fai-segment{
  padding:6px 14px;
  border:none;
  background:transparent;
  border-radius:7px;
  color:#1c1c1e;
  cursor:pointer;
  font-family:inherit;
  font-size:inherit;
  font-weight:500;
  transition:background 0.15s,box-shadow 0.15s;
  min-width:60px;
}
.fai-segment[data-selected="true"]{
  background:#fff;
  box-shadow:0 1px 2px rgba(0,0,0,0.08),0 0 0 0.5px rgba(0,0,0,0.04);
  font-weight:600;
}

/* ── Image ──────────────────────────────────────────────────── */
.fai-image{max-width:100%;display:block}
"#.to_string()
}

fn generate_runtime_js(wasm_filename: &str) -> String {
    format!(
        r#"const app=document.getElementById('app'),output=document.getElementById('output');
let instance,state='{{}}'
const FAI_DEBUG=window.__FAI_DEBUG__===true||new URLSearchParams(location.search).get('fai_debug')==='1'||localStorage.getItem('fai_debug')==='1';
function debugLog(){{if(FAI_DEBUG)console.log.apply(console,arguments)}}
const QNAN=0x7FFC000000000000n,SIGN=0x8000000000000000n,OBJ_MASK=QNAN|SIGN;
const TAG_INT=0x0004000000000000n,TAG_BOOL=0x0003000000000000n,TAG_NULL=0x0001000000000000n;
const NULL_VAL=QNAN|TAG_NULL,INT_MASK=QNAN|TAG_INT,BOOL_MASK=QNAN|TAG_BOOL;
function jsToWasm(v){{
  if(v===null||v===undefined)return QNAN|TAG_NULL;
  if(typeof v==='boolean')return QNAN|TAG_BOOL|BigInt(v?1:0);
  if(typeof v==='number'){{if(Number.isInteger(v))return QNAN|TAG_INT|BigInt.asUintN(32,BigInt(v));var buf=new ArrayBuffer(8);new Float64Array(buf)[0]=v;return new BigInt64Array(buf)[0]}}
  if(typeof v==='string')return writeStrToWasm(v);
  if(Array.isArray(v)){{var h=instance.exports.__heap_ptr.value,addr=h,end=(addr+8+v.length*8+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);var items=v.map(function(i){{return jsToWasm(i)}});m=instance.exports.memory.buffer;for(var i=0;i<items.length;i++){{new BigInt64Array(m,addr+8+i*8,1)[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{var keys=Object.keys(v),h=instance.exports.__heap_ptr.value,addr=h,cap=Math.max(keys.length,16),end=(addr+8+cap*16+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);for(var i=0;i<keys.length;i++){{var kv=writeStrToWasm(keys[i]),vv=jsToWasm(v[keys[i]]),ea=addr+8+i*16;m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
  return QNAN|TAG_NULL;
}}
function wasmToJs(v){{
  // See matching comment in generate_runtime_js above — INT_MASK
  // aliases QNAN due to a tag-bit overlap, so object/bool/null
  // checks must come before the Int fallback.
  var n=BigInt(v);if(n===NULL_VAL)return null;
  if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn);var dv=new DataView(instance.exports.memory.buffer);var tag=dv.getInt32(a,true);
    if(tag===0){{var l=dv.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}
    if(tag===1){{var cnt=dv.getInt32(a+4,true),r=[];for(var i=0;i<cnt;i++){{r.push(wasmToJs(new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1)[0]))}}return r}}
    if(tag===3){{var cnt=dv.getInt32(a+4,true),r={{}};for(var i=0;i<cnt;i++){{var ea=a+8+i*16,bi=new BigInt64Array(instance.exports.memory.buffer,ea,2),k=wasmToJs(bi[0]),val=wasmToJs(bi[1]);if(typeof k==='string')r[k]=val}}return r}}
    return null;
  }}
  if((n&BOOL_MASK)===BOOL_MASK)return(n&1n)===1n;
  if((n&QNAN)===QNAN)return Number(BigInt.asIntN(32,n&0xFFFFFFFFn));
  var buf=new ArrayBuffer(8);new BigInt64Array(buf)[0]=n;return new Float64Array(buf)[0];
}}
function readStr(p,l){{return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,p,l))}}
function writeStr(p,s){{var b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p).set(b);return b.length}}
function wasmGrow(needed){{var mem=instance.exports.memory;var cur=mem.buffer.byteLength;if(needed>cur){{var pages=Math.ceil((needed-cur)/65536);mem.grow(pages)}}}}
function writeStrToWasm(s){{var b=new TextEncoder().encode(s),h=instance.exports.__heap_ptr.value;wasmGrow(h+8+b.length+8);var m=new Uint8Array(instance.exports.memory.buffer),d=new DataView(instance.exports.memory.buffer);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{var n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn),d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{var l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function invokeExport(name){{var fn=instance.exports[name];if(!fn)return;var args=Array.prototype.slice.call(arguments,1);try{{return fn.apply(null,args)}}catch(e){{console.error('FAI',name,'failed',e);throw e}}}}
function handleEvent(id){{var fn=instance.exports.invokeHandler;if(!fn)return;try{{fn(BigInt(id)|0x7FFC000400000000n)}}catch(e){{console.error('FAI handleEvent failed',e)}}}}
function handleInputEvent(id,value){{var fn=instance.exports.invokeChangeHandler;if(!fn)return;try{{fn(BigInt(id)|0x7FFC000400000000n,writeStrToWasm(value))}}catch(e){{console.error('FAI handleInputEvent failed',e)}}}}
function handleSubmitEvent(id){{var fn=instance.exports.invokeSubmitHandler;if(!fn)return;try{{fn(BigInt(id)|0x7FFC000400000000n)}}catch(e){{console.error('FAI handleSubmitEvent failed',e)}}}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}var used={{}};for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];used[nk]=true;if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}for(var i=oc.length-1;i>=0;i--){{var k=oc[i].nodeType===1?oc[i].getAttribute('data-fai-key'):null;if(k&&!used[k])op.removeChild(oc[i])}}}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
function wireEvents(){{document.querySelectorAll('[data-fai-click]').forEach(function(el){{var h=el.getAttribute('data-fai-click');el.onclick=function(){{invokeExport(h);instance.exports._start()}}}})}}
var env={{
  print:function(p,l){{var text=readStr(p,l);debugLog('FAI print',text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file:function(){{return -1}},write_file:function(){{return -1}},now_ms:function(){{return Date.now()}},random:function(){{return Math.random()}},sleep_ms:function(){{}},
  call_ffi:function(){{return 0x7FFC000100000000n}},run_all:function(){{return 0x7FFC000100000000n}},
  spawn:function(closureVal){{var n=BigInt(closureVal);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return 0x7FFC000200000000n;var tag=dv.getInt32(a,true);if(tag!==4)return 0x7FFC000200000000n;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}return 0x7FFC000200000000n}},
  http_post:function(a,b,c,d,e){{try{{var x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html:function(p,l){{morphDom(app,readStr(p,l),false);wireEvents()}},
  set_html_at:function(a,b,p,l){{var selector=readStr(a,b),html=readStr(p,l);var root=document.querySelector(selector);if(!root&&selector.charAt(0)==='#'){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root)}}if(!root)return;morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse:function(p,l){{try{{return jsToWasm(JSON.parse(readStr(p,l)))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify:function(v){{try{{return writeStrToWasm(JSON.stringify(wasmToJs(v)))}}catch(e){{return writeStrToWasm('null')}}}},
  remote_call:function(a,b,c,d,e,f,g,h){{var u=readStr(a,b),fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);var body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});try{{var x=new XMLHttpRequest();x.open('POST',u.replace(/\/+$/,'')+'/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body);var resp=JSON.parse(x.responseText);if(resp.ok)return jsToWasm(resp.value);return NULL_VAL}}catch(e){{return NULL_VAL}}}},
  float_to_str:function(v,p){{var s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);var b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  get_location_path:function(){{return writeStrToWasm(window.location.pathname)}},
  push_history_state:function(p,l){{history.pushState(null,'',readStr(p,l))}},
  storage_get:function(kp,kl,bp){{try{{var k=readStr(kp,kl);var v=window.localStorage.getItem(k);if(v===null)return -1;var b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_set:function(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove:function(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear:function(){{try{{window.localStorage.clear()}}catch(e){{}}}}
}};
fetch('/{}').then(function(r){{return r.arrayBuffer()}}).then(function(b){{return WebAssembly.instantiate(b,{{env:env}})}}).then(function(r){{
  instance=r.instance;invokeExport('_start');
  window.addEventListener('popstate',function(){{if(instance&&instance.exports.setPathFromPlatform)instance.exports.setPathFromPlatform(writeStrToWasm(window.location.pathname))}});
}}).catch(function(e){{app.innerHTML='<p style="color:red;padding:20px">Error: '+e.message+'</p>'}});
"#,
        wasm_filename
    )
}

fn step_fmt(args: &[String], reporter: &Reporter) {
    let check_mode = args.iter().any(|a| a == "--check");
    let path_arg = args.iter().find(|a| !a.starts_with("--"));

    let path = match path_arg {
        Some(p) => p.to_string(),
        None => {
            // Default: format the project's source directory
            match find_project_source_from_cwd() {
                Some((project_root, src_dir)) => {
                    project_root.join(&src_dir).to_string_lossy().into_owned()
                }
                None => {
                    // No standard project found (e.g. workspace root) — skip fmt step
                    return;
                }
            }
        }
    };
    let path = &path;

    match format::format_path(path, check_mode) {
        Ok(results) => {
            let total = results.len();
            let changed: Vec<_> = results.iter().filter(|r| r.changed).collect();
            if check_mode {
                if !changed.is_empty() {
                    for r in &changed {
                        reporter.error_line(&format!("needs formatting: {}", r.file_path));
                    }
                    reporter.step(
                        StepStatus::Fail,
                        "fmt",
                        &format!("{} of {} file(s) need formatting", changed.len(), total),
                    );
                    std::process::exit(1);
                }
                reporter.step(
                    StepStatus::Ok,
                    "fmt",
                    &format!("{} file(s) already formatted", total),
                );
            } else if changed.is_empty() {
                reporter.step(StepStatus::Ok, "fmt", "all files already formatted");
            } else {
                for r in &changed {
                    reporter.detail(&format!("formatted {}", r.file_path));
                }
                reporter.step(
                    StepStatus::Ok,
                    "fmt",
                    &format!("reformatted {} of {} file(s)", changed.len(), total),
                );
            }
        }
        Err(e) => {
            reporter.error_line(&e.to_string());
            reporter.step(StepStatus::Fail, "fmt", "formatter error");
            std::process::exit(1);
        }
    }
}

fn cmd_new(args: &[String]) {
    if args.is_empty() {
        eprintln!("Usage: forai new <project-name>");
        std::process::exit(1);
    }

    let name = &args[0];
    let project_root = std::path::Path::new(name);

    if project_root.exists() {
        eprintln!("error: target already exists: {}", project_root.display());
        std::process::exit(1);
    }

    let src_dir = project_root.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("error creating directory: {}", e);
        std::process::exit(1);
    }

    let codex_dir = project_root.join(".codex");
    if let Err(e) = std::fs::create_dir_all(&codex_dir) {
        eprintln!("error creating directory: {}", e);
        std::process::exit(1);
    }

    let project_name = project_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let files: Vec<(std::path::PathBuf, String)> = vec![
        (src_dir.join("main.fai"), scaffold_main(&project_name)),
        (
            project_root.join("fai.toml"),
            scaffold_fai_toml(&project_name),
        ),
        (
            project_root.join("README.md"),
            scaffold_readme(&project_name),
        ),
        (project_root.join("language.md"), scaffold_language_md()),
        (
            project_root.join("CLAUDE.md"),
            scaffold_claude_md(&project_name),
        ),
        (project_root.join("AGENTS.md"), scaffold_agents_md()),
        (project_root.join(".mcp.json"), scaffold_mcp_json()),
        (
            project_root.join(".codex/config.toml"),
            scaffold_codex_config(),
        ),
    ];

    for (path, content) in &files {
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("error writing {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }

    println!("created project '{}'", project_name);
    for (path, _) in &files {
        println!("  {}", path.display());
    }
}

fn cmd_doc(args: &[String]) {
    let query = args.first().map(|s| s.as_str()).unwrap_or("");

    // Language reference docs are always available regardless of project context.
    let mut all_entries = doc::collect_lang_docs();
    all_entries.extend(doc::collect_stdlib_docs());

    // Load project + dependency docs if we're inside a fai project.
    match find_project_source_from_cwd() {
        Some((project_root, src_dir)) => {
            let src_path = project_root.join(&src_dir);
            all_entries.extend(doc::collect_project_docs(&src_path));

            let toml_path = project_root.join("fai.toml");
            if let Ok(toml_content) = std::fs::read_to_string(&toml_path) {
                for (dep_name, dep_path) in doc_parse_file_deps(&toml_content) {
                    // Package function docs
                    all_entries.extend(doc::collect_dependency_docs(&dep_path, &dep_name));
                    // Package overview doc (from the `docs` attribute in the package's fai.toml)
                    if let Some(overview) = doc::collect_package_overview(&dep_path, &dep_name) {
                        all_entries.push(overview);
                    }
                }
            }
        }
        None => {
            if !query.is_empty() && !query.starts_with("std.") && !query.starts_with("lang") {
                eprintln!(
                    "(Note: not inside a fai project — showing stdlib and language docs only)"
                );
            }
        }
    }

    // Namespace directory listing: if query is an intermediate namespace, show children.
    // An empty query now lists all top-level namespaces (lang, std, packages, project).
    if let Some(namespaces) = doc::query_child_namespaces(&all_entries, query) {
        doc::render_namespace_listing(&namespaces);
        // For root-level entries (overview text) also print their doc when drilling in.
        if !query.is_empty() {
            // Show the overview entry for this namespace if one exists.
            let overview: Vec<_> = all_entries
                .iter()
                .filter(|e| e.full_path == query && e.namespace.is_empty())
                .collect();
            if !overview.is_empty() {
                println!();
                doc::render_docs(&overview);
            }
        }
        return;
    }

    let results = doc::search_docs(&all_entries, query);
    if results.is_empty() {
        if query.is_empty() {
            eprintln!("No documentation available.");
        } else {
            // Collect top-level package names for the hint.
            let top: Vec<String> = {
                let mut seen = std::collections::BTreeSet::new();
                for e in &all_entries {
                    if let Some(root) = e.full_path.split('.').next() {
                        seen.insert(root.to_string());
                    }
                }
                seen.into_iter().collect()
            };
            let packages_hint = if top.is_empty() {
                String::new()
            } else {
                format!("\n  Available top-level namespaces: {}", top.join(", "))
            };
            eprintln!(
                "No documentation found for '{}'.\n\
                 \n\
                 Tip — explore docs by drilling down:\n\
                   fai doc              list all packages and language topics\n\
                   fai doc lang         language reference (variables, functions, modules…)\n\
                   fai doc lang.modules import patterns including RPC\n\
                   fai doc std          standard library modules\n\
                   fai doc <Package>    package overview and sub-modules (e.g. fai doc Forui)\n\
                   fai doc <name>       search by function or type name (e.g. fai doc Signal)\n\
                 {}",
                query, packages_hint
            );
        }
        std::process::exit(1);
    }
    doc::render_docs(&results);
}

/// Parse `[dependencies]` from a fai.toml string and return
/// `(package_name, project_root_path)` pairs for `file://` entries.
fn doc_parse_file_deps(toml_content: &str) -> Vec<(String, std::path::PathBuf)> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in toml_content.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        let Some((k, _v)) = t.split_once('=') else {
            continue;
        };
        let dep_spec = k.trim().trim_matches('"');
        let Some(path_str) = dep_spec.strip_prefix("file://") else {
            continue;
        };
        let dep_root = std::path::PathBuf::from(path_str);
        let dep_info =
            read_project_info_full(Some(dep_root.join("src").to_str().unwrap_or(path_str)));
        let dep_name = if dep_info.name.is_empty() || dep_info.name == "unknown" {
            dep_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dep".to_string())
        } else {
            dep_info.name
        };
        deps.push((dep_name, dep_root));
    }
    deps
}

fn scaffold_main(project_name: &str) -> String {
    format!(
        r#"# {name} entry point

def main
    @return Void
do
  print('hello from {name}')
end
"#,
        name = project_name
    )
}

fn scaffold_fai_toml(project_name: &str) -> String {
    format!(
        r#"[project]
name = "{name}"
version = "0.1.0"
source_root = "src"

[dependencies]
"#,
        name = project_name
    )
}

fn scaffold_readme(project_name: &str) -> String {
    format!(
        r#"# {name}

A forai project.

## Commands

```bash
fai run        # fmt → check → test → run
fai check      # fmt → check
fai test       # fmt → check → test
fai fmt        # format source files
fai build      # fmt → check → test → build (.wasm)
```
"#,
        name = project_name
    )
}

fn scaffold_language_md() -> String {
    r#"# forai Language Reference

forai is a statically-typed language with strong type inference. Comments start
with `#`. All blocks are `end`-delimited. `print()` is a builtin — no import needed.

## Variables

```fai
let x = 42           # immutable — no reassignment, no field mutation
var count = 0        # mutable — can reassign and mutate fields
let s String = 'hi'  # optional explicit type annotation
let n Int? = null    # optional type (can be null)

var user = User(name: 'Alice', age: 30)
user.age = 31        # OK — var allows field mutation
user.age             # field access
```

All assignments are **deep copies** — variables never share references.

## Functions

Named functions use `@param` / `@return` / `do...end`. A doc comment is required
on every named function except `main`.

```fai
# Add two integers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

# Greet by name.
def greet
    @param name String
    @param greeting String, default: 'hello'
    @return String
do
  "{{greeting}}, {{name}}"
end

add(1, 2)                          # positional call
greet(name: 'Alice')               # named call (uses default greeting)
greet('Alice', greeting: 'hey')    # mixed
```

### UFCS — method-style calls

Any function can be called as a method on its first argument:

```fai
5.add(3)            # same as add(5, 3)
label.fontSize(14)  # same as fontSize(label, 14) — must be imported!
```

### Mutable parameters

By default function parameters are immutable copies. Use `mutable` to allow
the function to mutate the caller's binding in place:

```fai
# Increment a counter.
def increment
    @param c Counter, mutable
    @return Void
do
  c.value = c.value + 1
end

var c = Counter(value: 0)
increment(c)     # c.value is now 1 — only var bindings can be passed as mutable
```

### Anonymous closures (`do...end`)

```fai
run(do
  print('hello')
end)

apply(5, do with n Int
  n * 2
end)

# Trailing block — when the last param is a type def:
Button('Click me', onClick: do
  print('clicked')
end)
```

### Generic functions

```fai
# Echo a value.
def echo
    @type T
    @param value T
    @return T
do
  value
end

echo(42)      # T inferred as Int
echo('hi')    # T inferred as String
```

## Types

```fai
type Point
  x Int
  y Int
end

let p = Point(x: 1, y: 2)   # construction uses named args
p.x                           # field access

var q = Point(x: 3, y: 4)
q.x = 10                      # field mutation (var only)
```

### Type-typed fields (callbacks)

```fai
type def ClickAction
    @return Void
end

type Button
  label String
  onClick ClickAction?
end
```

### Generic types

```fai
type Box
  @type T
  value T
end

let b = Box(value: 42)   # T inferred as Int
```

## Enums

```fai
enum Status
  active
  loading
  error
end

let s = Status.active

case s
when Status.active
  print('ok')
when Status.loading
  print('wait')
default
  print('error')
end
```

## Strings

```fai
let plain = 'no interpolation'
let name = 'world'
let msg = "hello {{name}}"    # double-quote + double-brace for interpolation
let joined = 'hello' + ' ' + 'world'
```

## Arrays and Dictionaries

```fai
let nums = [1, 2, 3]          # array literal (commas required)
let first = nums[0]
let count = length(nums)

var list = [1, 2, 3]
list[0] = 99                   # index mutation (var only)

let d = {}                     # empty dict
let d2 = set(d, 'key', 'val') # returns new dict
getString(d2, 'key')           # => 'val' (String?)
getInt(d2, 'num')              # => Int?
getKeys(d2)                    # => String[]
```

## Optionals

```fai
let x Int? = null
if x?              # check: is it non-null?
  print(x!)        # unwrap: force-extract value (panics if null)
end
let safe = unwrap(x, 0)   # unwrap with fallback
```

## Control Flow

```fai
if x > 0
  print('positive')
else if x == 0
  print('zero')
else
  print('negative')
end

var i = 0
while i < 10
  i = i + 1
end

for item in ['a', 'b', 'c']
  print(item)
end

for i in 0..9   # range: 0 inclusive to 9 inclusive
  print(i)
end
```

## Error Handling

```fai
try
  let data = fetchData()
catch e
  print(e.message)
finally
  cleanup()
end

throw Error('something went wrong')
```

## Concurrency

```fai
nowait logEvent('page_viewed')                      # fire and forget
let a, b = all(fetchUsers(), fetchPosts())          # parallel, wait for both
sleep(500)                                          # pause 500ms
```

## Modules and Imports

A module is a **directory** of `.fai` files. Import by directory name, not filename.

```fai
# Same project — sibling directory
use { Nav, Section } from client.components    # src/client/components/
use { HomePage } from client.pages             # src/client/pages/
use { isLoggedIn } from client.state           # src/client/state/

# External package (listed in fai.toml [dependencies])
use { mount } from Forui                       # package named "Forui"
use { Label, Button, VStack } from Forui.view  # sub-module Forui/view/
use { useSignal, isLoading, reload } from Forui.signal
use { navigate, Link } from Forui.router

# IMPORTANT: every UFCS function (e.g. label.fontSize(14)) must be explicitly
# imported in the file that uses it — there is no global namespace.
use { fontSize, foreground, padding } from Forui.view   # required per-file

# Cross-target (server importing client for SSR, when both share source = "src")
use { App } from client

# Auto-generated RPC proxy (fullstack projects — see AGENTS.md)
use { Task, getTasks } from Server
```

### Namespace import

```fai
use std.array
array.length([1, 2, 3])   # qualified call

use { length, append } from std.array
length([1, 2, 3])          # unqualified call
```

### Visibility

```fai
def publicFn           # exported by default
    @return Void
do end

private:               # everything below is NOT exported
def helper
    @return Void
do end
```

## Testing

```fai
# Tests live in the same file as the function they test.
# Every function needs at least one test — fai test fails otherwise.

# Add two integers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

test add
it 'adds positive numbers'
  assert.equals(add(1, 2), 3)
end
it 'handles negatives'
  assert.equals(add(-1, 1), 0)
end
end
```

## Standard Library

Run `fai doc std` to browse all modules, or `fai doc std.array` for a specific one.
Full signatures and examples are available via `fai doc <name>`.

Key modules: `std.string`, `std.array`, `std.dictionary`, `std.math`, `std.convert`,
`std.json`, `std.http.request`, `std.http.server`, `std.file`, `std.path`,
`std.error`, `std.time`, `std.log`, `std.cli`

```fai
use std.array
use std.string
use std.convert

array.length([1, 2, 3])          # 3
string.contains('hello', 'ell')  # true
toString(42)                     # '42'  (also available as builtin)
parseInt('42')                   # 42   (throws on invalid input)
```
"#
    .to_string()
}

fn scaffold_claude_md(project_name: &str) -> String {
    format!(
        r#"# {name}

## Development Process

forai treats testing and documentation as first-class — they're enforced by
the tooling, not optional style. Read this before writing code.

- **Document as you go.** Every `def` needs a doc comment. No exceptions.
- **Test every function.** `fai test` fails the build if any public
  function is uncovered. Private helpers covered by a tested caller are OK.
- **Red → green → refactor, one function at a time.** Write the `test`
  block first with 1–3 `it` cases, run `fai check` to catch signature
  errors, then fill in the body until `fai test` passes. Don't write
  five functions and then build — each failure hides the next.
- **Use `fai_examples` before writing a new kind of code.** Keywords:
  `rpc`, `http`, `ui`, `children`, `types`, `testing`, `function`, `fai.toml`.
  Faster than rediscovering the pattern from error messages.

## Language Quick Reference

**fai.toml** — project config. `fai_examples` for complete config patterns. `fai doc lang` for full reference.

**Functions** — every public function needs a doc comment and a test:
```fai
# Add two numbers.
def add
    @param a Int
    @param b Int
    @return Int
do
  a + b
end

test add
it 'adds numbers'
  assert.equals(add(1, 2), 3)
end
end
```

**Modules** — a directory is a module, import by directory name not filename:
```fai
use {{ HomePage }} from client.pages          # src/client/pages/*.fai
use {{ Label, Button, fontSize }} from Forui.view  # must import each UFCS function
```

**Testing** — required for every public function or `fai test` fails.
**Types** — `type Task {{ id Int\n  text String\nend }}` · constructed with named args · `Task(id: 1, text: 'x')`.
Built-ins: `Int`, `Float`, `String`, `Bool`, `Void`, `T[]`, `T?`. Arrays need commas: `[1, 2, 3]`.
Dicts: `getString(d, 'k') -> String?`, `getInt(d, 'k') -> Int?`, `set(d, 'k', v) -> Dictionary`.
Optionals: `x?` checks non-null · `x!` unwraps · `unwrap(x, fallback)` safe unwrap.

## CLI Commands

```bash
fai fmt            # format source files in src/
fai check          # fmt → type-check
fai test           # fmt → check → run tests (REQUIRED for all functions)
fai run            # fmt → check → test → run
fai build          # fmt → check → test → build
fai doc <query>    # look up docs: 'lang', 'std.array', 'fontSize', 'Forui'
fai_examples       # MCP tool: complete working code patterns
```

Output shows one `[ok]` / `[fail]` line per pipeline step (fmt, check,
test, build). Pass `-v` for per-file details. An uncovered public
function counts as a failed test — same exit code as an assertion
failure.

## File Structure

All `.fai` files in `src/` form a single module. Split code by concern:

```
src/
  types.fai      — type declarations
  main.fai       — entry point (for runnable projects)
  <name>.fai     — one file per function or logical group
  internal.fai   — private helpers (put private: at top)
```

### Module loading order

Files load alphabetically. This matters for `let` constants — they are NOT
forward-declared, so any file that references a constant must load after it.

Prefix files with `_` to sort them first: `_constants.fai`, `_ffi.fai`.

### `private:` is sticky

`private:` is a mode, not a per-declaration keyword. Once written in a file,
**all subsequent declarations in that file become private**. Keep all public
declarations above any `private:` line.

```fai
# public.fai
def publicFn ...   # ← exported

private:           # ← everything below is private
def helper ...     # ← NOT exported
```

## Writing Code

- One function per file is the preferred style for libraries
- Every function needs at least one test — `fai test` fails otherwise
- Use `test <fnName>` blocks with `it '...'` cases in the same file
- `private:` helpers are covered when their callers are tested
- `print()` is a builtin — no import needed
- String interpolation: `"hello {{{{name}}}}"` (double quotes, double braces)
- Arrays: `[1, 2, 3]` (commas required)
- Module functions: `array.length(arr)`, `dictionary.set(d, k, v)`
- Prefer `let` (immutable) over `var`

## Example function with test

```fai
# Compute the area of a rectangle.
def area
    @param width Float
    @param height Float
    @return Float
do
  width * height
end

test area
it 'multiplies width by height'
  assert.equals(area(3.0, 4.0), 12.0)
end
end
```

## Fullstack RPC (multi-target projects)

For projects with `[project.client]` + `[project.server]` in fai.toml:

**Server** (`src/server/main.fai`):
```fai
use std.http.server
use {{ handleRpcRequest }} from Forui.rpc   # required

remote type Task                            # exported to client proxy
  id Int
  text String
end

remote def getTasks                         # exported to client proxy
    @param token String
    @return Task[]
do
  # implementation
end

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)                           # auto-generated — do not define manually
  server.listen(r, 3040)
end
```

**Client** (`src/client/pages/tasks.fai`):
```fai
use {{ Task, getTasks }} from Server        # auto-generated proxy module
use {{ useSignal, isLoading }} from Forui.signal
```

**Rules:**
- Every function the client calls must be `remote def` in the server
- Every type the client uses from the server must be `remote type`
- `addRpcRoutes` is auto-generated — never write it yourself
- `use {{ handleRpcRequest }} from Forui.rpc` is required in the server entry file
- Use `fai build` (no target) to build both client and server at once
- View modifier functions (`fontSize`, `foreground`, `padding`, etc.) must be explicitly imported from `Forui.view` in every file that uses them

## MCP Server

This project ships `.mcp.json` — Claude Code picks it up automatically and can run
`fai_fmt`, `fai_check`, `fai_test`, `fai_run`, `fai_build`, and `fai_doc` as tools.
Start the server manually with `fai mcp` (runs until killed, reads stdin/writes stdout).
"#,
        name = project_name
    )
}

fn scaffold_agents_md() -> String {
    r#"# AGENTS.md

Guidelines for AI agents working on this forai project.

## Development Process

forai treats testing and documentation as first-class — they're enforced by
the tooling, not optional style. Read this before writing code.

- **Document as you go.** Every `def` needs a doc comment. No exceptions.
- **Test every function.** `fai test` fails the build if any public
  function is uncovered. Private helpers covered by a tested caller are OK.
- **Red → green → refactor, one function at a time.** Write the `test`
  block first with 1–3 `it` cases, run `fai check` to catch signature
  errors, then fill in the body until `fai test` passes. Don't write
  five functions and then build — each failure hides the next.
- **Use `fai_examples` before writing a new kind of code.** Keywords:
  `rpc`, `http`, `ui`, `children`, `types`, `testing`, `function`, `fai.toml`.
  Faster than rediscovering the pattern from error messages.

## Language Quick Reference

**fai.toml** — project config (name, version, source_root, dependencies, targets).
Call `fai_examples` with query "fai.toml" for a complete template. Call `fai doc lang` for the full reference.

**Functions** — require doc comment + test block. `fai test` fails if any function is uncovered:
```fai
# Compute the area of a rectangle.
def area
    @param width Float
    @param height Float
    @return Float
do
  width * height
end

test area
it 'multiplies dimensions'
  assert.equals(area(3.0, 4.0), 12.0)
end
end
```

**Modules** — a directory is a module. Import by directory name, not filename:
```fai
use { HomePage } from client.pages        # src/client/pages/*.fai → client.pages module
use { Label, Button, fontSize } from Forui.view  # every UFCS fn must be explicitly imported
```

**Types** — struct-like, construction uses named args, field access with dot notation:
```fai
type Task
  id Int
  text String
  done Bool
end
let t = Task(id: 1, text: 'hello', done: false)
t.text                   # field access
```
Built-ins: `Int`, `Float`, `String`, `Bool`, `T[]`, `T?`.
Arrays: `[1, 2, 3]` (commas required). Dicts: `getString(d,'k')→String?`, `getInt(d,'k')→Int?`.
Optionals: `x?` checks · `x!` unwraps · `unwrap(x, fallback)` safe.

## CLI Commands

```bash
fai fmt            # format src/
fai check          # fmt → type-check
fai test           # fmt → check → run tests (REQUIRED — missing test = failed test)
fai run            # fmt → check → test → run
fai build          # fmt → check → test → build (no target = all targets)
fai doc <query>    # look up docs: 'lang', 'std.array', 'Signal', 'Forui.view'
# add -v / --verbose to any of the above for per-file details.
```

## MCP Tools (when using fai mcp)

- `fai_doc query:"Signal"` — find type/function docs
- `fai_doc query:"lang.modules"` — import patterns
- `fai_examples query:"rpc"` — complete RPC server+client example
- `fai_examples query:"fai.toml"` — project config template
- `fai_examples query:"http"` — HTTP+JSON fetch pattern
- `fai_examples query:"ui"` — UI component testing with testMount
- `fai_examples query:"children"` — custom component that takes a do...end block

## File Structure

```
src/
  types.fai      — type declarations
  main.fai       — entry point (runnable projects only)
  <name>.fai     — one file per function or logical group
  internal.fai   — private helpers (private: at the top)
```

Files load alphabetically. `let` constants are NOT forward-declared — files referencing
a constant must load after it. Prefix with `_` to force early load: `_constants.fai`.

### `private:` is sticky

Once `private:` appears in a file, ALL declarations below it in that file
become private. Keep all public declarations ABOVE any `private:` line.

## Writing Code

- Every function needs a test — `fai test` is a hard failure otherwise
- Write `test <fnName>` blocks in the same file as the function
- `private:` helpers are tested via their callers
- Function syntax: `def name\n    @param x Type\n    @return Type\ndo\n  body\nend`
- String interpolation: `"hello {{name}}"` — double quotes and double braces
- Arrays: `[1, 2, 3]` — commas required
- Prefer `let` (immutable) over `var`

## Fullstack RPC (multi-target projects)

For projects with `[project.client]` + `[project.server]` in fai.toml:

**Server** (`src/server/main.fai`):
```fai
use std.http.server
use { handleRpcRequest } from Forui.rpc   # required

remote type Task                          # exported to client proxy
  id Int
  text String
end

remote def getTasks                       # exported to client proxy
    @param token String
    @return Task[]
do
  # implementation
end

def main
    @return Void
do
  var r = server.router()
  addRpcRoutes(r)                         # auto-generated — do not define manually
  server.listen(r, 3040)
end
```

**Client** (`src/client/pages/tasks.fai`):
```fai
use { Task, getTasks } from Server        # auto-generated proxy module
use { useSignal, isLoading } from Forui.signal
```

**Rules:**
- Every function the client calls must be `remote def` in the server
- Every type the client uses from the server must be `remote type`
- `addRpcRoutes` is auto-generated — never write it yourself
- `use { handleRpcRequest } from Forui.rpc` is required in the server entry file
- Use `fai_build` (no target) to build both client and server at once

## Common Mistakes

- **One big file** — split by function/concern, not everything in main.fai
- **Missing tests** — every function needs at least one test or the pipeline fails
- **`private:` placement** — public declarations must come BEFORE `private:` in a file
- **Wrong string syntax** — interpolation uses `"{{var}}"` not `'{{var}}'`
- **Arrays without commas** — use `[1, 2, 3]` not `[1 2 3]`
- **No import for print** — `print()` is a builtin, never import it
- **Missing modifier imports** — `fontSize`, `foreground`, `padding` etc. must be imported from `Forui.view` in every file that uses them
- **`remote def` missing** — if the client's `from Server` import is empty, the server functions are not marked `remote def`
- **`fai_build` with wrong target** — for fullstack projects, call `fai_build` with no `target` to build all sub-projects; do NOT pass `"wasm"` as the target
- **Custom container parse error** — `Section do ... end` works ONLY when `Section` has a parameter typed `Children` (the closure type). See `fai_examples query:"children"`.
- **Big functions → register overflow** — the compiler errors out when a single function needs more than 256 registers. Split complex UI blocks (e.g. long VStack catalogues) into helper functions that return `ViewNode`.

## MCP Server

This project includes `.mcp.json` (Claude Code) and `.codex/config.toml` (Codex) that
start the fai MCP server automatically. The server exposes all fai CLI commands as MCP
tools so AI agents can run `fai_fmt`, `fai_check`, `fai_test`, `fai_run`, `fai_build`,
`fai_doc`, and `fai_new` directly.

For system-wide setup outside this project, add to your agent's user config:

**Claude Code** (`~/.claude/settings.json`):
```json
{
  "mcpServers": {
    "fai": { "command": "fai", "args": ["mcp"] }
  }
}
```

**Codex** (`~/.codex/config.toml`):
```toml
[mcp_servers.fai]
command = "fai"
args = ["mcp"]
enabled = true
tool_timeout_sec = 120
```
"#
    .to_string()
}

fn scaffold_mcp_json() -> String {
    r#"{
  "mcpServers": {
    "fai": {
      "command": "fai",
      "args": ["mcp"]
    }
  }
}
"#
    .to_string()
}

fn scaffold_codex_config() -> String {
    r#"[mcp_servers.fai]
command = "fai"
args = ["mcp"]
enabled = true
startup_timeout_sec = 10
tool_timeout_sec = 120
"#
    .to_string()
}

// ── Helpers ──────────────────────────────────────────────────────────

fn require_file_arg(args: &[String], command: &str) -> String {
    let path = args.iter().find(|a| !a.starts_with("--"));
    match path {
        Some(p) => p.clone(),
        None => {
            eprintln!("Usage: forai {} <file.fai>", command);
            std::process::exit(1);
        }
    }
}

fn read_file(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error reading {}: {}", path, e);
            std::process::exit(1);
        }
    }
}

/// Compile forai source to wasm via the direct AST→wasm path.
/// If direct refuses now, it's a bug or an unsupported construct that
/// should be surfaced instead of silently falling back to another backend.
fn compile_fai_to_wasm(
    content: &str,
    path: &str,
    is_test: bool,
    synthetic_modules: Vec<(String, String)>,
    target: Option<&str>,
) -> Vec<u8> {
    let source_root = find_source_root(path);
    let prepared = match fai_compiler::prepare_source_with_synthetic_and_entry(
        content,
        source_root.as_deref(),
        synthetic_modules,
        Some(path),
    ) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{}", e);
            std::process::exit(1);
        }
    };

    let mut checker = fai_checker::Checker::new();
    if let Err(e) = run_checker(&mut checker, &prepared) {
        eprintln!("{}", format_check_errors(&checker, &e));
        std::process::exit(1);
    }

    let info = fai_codegen_wasm::direct::CheckerInfo {
        ufcs_calls: checker.ufcs_calls.clone(),
        named_param_reorder: checker.named_param_reorder.clone(),
        expression_types: checker.expression_types.clone(),
        generic_type_args: checker.generic_type_args.clone(),
    };
    match fai_codegen_wasm::codegen_direct_full_reasoned(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        target,
        is_test,
    ) {
        Ok(wasm) => wasm,
        Err(e) => {
            eprintln!(
                "internal error: direct AST→wasm codegen refused this program: {:?}",
                e,
            );
            std::process::exit(1);
        }
    }
}

fn run_checker(
    checker: &mut fai_checker::Checker,
    prepared: &fai_compiler::PreparedProgram,
) -> Result<(), fai_checker::CheckError> {
    if prepared.modules.is_empty() {
        checker.check_program(&prepared.serde_ast.statements)
    } else {
        let prepared_modules: Vec<fai_checker::PreparedModule> = prepared
            .modules
            .iter()
            .map(|m| fai_checker::PreparedModule {
                name: m.name.clone(),
                statements: m.statements.clone(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect();
        checker.check_with_modules(&prepared.serde_ast.statements, &prepared_modules)
    }
}

/// Format one error per line. If the checker accumulated more than one error
/// during this pass, include them all so users aren't forced to run the
/// pipeline once per issue. Falls back to the single `Err(CheckError)` when
/// the accumulator is empty (e.g. Phase 1 failures that short-circuit before
/// per-statement collection runs).
fn format_check_errors(checker: &fai_checker::Checker, first: &fai_checker::CheckError) -> String {
    if checker.collected_errors.is_empty() {
        return first.to_string();
    }
    checker
        .collected_errors
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Find project root and source directory from the current working directory.
/// Walks up to find fai.toml, then reads source_root from [project].
/// Exits with an error message if no fai.toml is found.
/// Find the project root and source directory from the current working directory.
/// Returns `None` when:
///   - No fai.toml found in cwd or any parent
///   - The fai.toml only has a [workspace] section (no [project]) — workspace roots
///     have member subdirectories, not a single source_root.
/// Returns `Some((project_root, src_dir))` for standard single-project setups.
fn find_project_source_from_cwd() -> Option<(std::path::PathBuf, String)> {
    let cwd = std::env::current_dir().ok()?;
    let project_root = find_project_root(&cwd)?;
    let toml_path = project_root.join("fai.toml");
    let mut src_dir = "src".to_string();
    let mut has_project_section = false;
    if let Ok(content) = std::fs::read_to_string(&toml_path) {
        let mut in_project = false;
        for line in content.lines() {
            let t = line.trim();
            if t.starts_with('[') {
                if t == "[project]" {
                    has_project_section = true;
                    in_project = true;
                } else {
                    in_project = false;
                }
                continue;
            }
            if !in_project {
                continue;
            }
            if let Some((k, v)) = t.split_once('=') {
                if k.trim() == "source_root" || k.trim() == "source" {
                    src_dir = v.trim().trim_matches('"').to_string();
                }
            }
        }
    }
    if !has_project_section {
        // Workspace or misconfigured — no standard source root
        return None;
    }
    Some((project_root, src_dir))
}

fn find_source_root(file_path: &str) -> Option<String> {
    let path = std::path::Path::new(file_path).canonicalize().ok()?;
    let mut dir = path.parent()?;
    loop {
        let toml = dir.join("fai.toml");
        if toml.exists() {
            if let Ok(content) = std::fs::read_to_string(&toml) {
                let mut src_root = ".".to_string();
                let mut in_project = false;
                for line in content.lines() {
                    let t = line.trim();
                    if t.starts_with('[') {
                        in_project = t == "[project]";
                        continue;
                    }
                    if !in_project {
                        continue;
                    }
                    if let Some((k, v)) = t.split_once('=') {
                        if k.trim() == "source_root" {
                            src_root = v.trim().trim_matches('"').to_string();
                        }
                    }
                }
                let root = dir.join(&src_root);
                return Some(root.to_string_lossy().into_owned());
            }
            return Some(dir.to_string_lossy().into_owned());
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_FAI: &str = concat!(
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hello')\n",
        "end\n",
    );

    // Note: test blocks are compiled in --is-test mode but the type-checker
    // does not validate them, so we use a program that has no test blocks
    // but compiles cleanly in test mode.
    const TEST_FAI: &str = concat!(
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('testing')\n",
        "end\n",
    );

    const INTERFACE_FAI: &str = concat!(
        "# A greeting function.\n",
        "def greet\n",
        "    @param name String\n",
        "    @return String\n",
        "do\n",
        "  'hello'\n",
        "end\n",
        "\n",
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print('hi')\n",
        "end\n",
    );

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_cli_test_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Shared mutex for tests that call `set_current_dir` — CWD is
    /// process-global and cargo runs tests in parallel by default.
    /// Acquire this before any `set_current_dir` in a test.
    fn cwd_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn write_fai(tag: &str, src: &str) -> String {
        let dir = temp_dir(tag);
        let path = dir.join("prog.fai");
        std::fs::write(&path, src).unwrap();
        path.to_string_lossy().into_owned()
    }

    // ── try_check_single_file ────────────────────────────────────────

    /// Create a minimal external package with a custom type in a temp directory.
    /// Returns (package_root, package_src_root) as absolute path strings.
    fn make_pkg_with_widget_type(tag: &str) -> std::path::PathBuf {
        let pkg = temp_dir(&format!("{}_pkg", tag));
        std::fs::create_dir_all(pkg.join("src")).unwrap();
        std::fs::write(
            pkg.join("fai.toml"),
            "[project]\nname = \"WidgetPkg\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n",
        ).unwrap();
        // Defines a Widget type and a makeWidget constructor.
        std::fs::write(
            pkg.join("src").join("widget.fai"),
            "type Widget\n  label String\nend\n\n# Make a widget.\ndef makeWidget\n    @param label String\n    @return Widget\ndo\n  Widget(label: label)\nend\n",
        ).unwrap();
        pkg
    }

    #[test]
    fn test_try_check_single_file_resolves_external_package_type() {
        // Regression test: when a .fai file has sibling files AND imports types
        // from an external package, try_check_single_file must succeed.
        //
        // The old code called prepare_module_directory which did NOT load fai.toml
        // dependencies, so types like ViewNode were "Unknown type" at check time.
        // The fix uses prepare_source which resolves all deps via fai.toml.
        let pkg = make_pkg_with_widget_type("check_ext_ok");
        let proj = temp_dir("check_ext_proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        let pkg_path = pkg.to_string_lossy();
        std::fs::write(
            proj.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                pkg_path
            ),
        ).unwrap();

        // Entry file: imports Widget from external package.
        std::fs::write(
            proj.join("src").join("main.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\ndef main\n    @return Widget\ndo\n  makeWidget('hello')\nend\n",
        ).unwrap();
        // Sibling file: also uses the external type — triggers the multi-file path.
        std::fs::write(
            proj.join("src").join("helper.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\n# Helper.\ndef helperWidget\n    @return Widget\ndo\n  makeWidget('helper')\nend\n",
        ).unwrap();

        let entry = proj.join("src").join("main.fai");
        let result = try_check_single_file(&entry.to_string_lossy());
        assert!(
            result.is_ok(),
            "check with external dep type should succeed; got: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_prepare_module_directory_does_not_load_external_deps() {
        // Documents why the old check_single_file path was broken:
        // prepare_module_directory loads only the given directory, not fai.toml
        // dependencies, so external package types are unknown.
        let pkg = make_pkg_with_widget_type("mod_dir_broken");
        let proj = temp_dir("mod_dir_broken_proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        let pkg_path = pkg.to_string_lossy();
        std::fs::write(
            proj.join("fai.toml"),
            format!(
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\n\"file://{}\" = \"0.1.0\"\n",
                pkg_path
            ),
        ).unwrap();
        std::fs::write(
            proj.join("src").join("main.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\ndef main\n    @return Widget\ndo\n  makeWidget('hello')\nend\n",
        ).unwrap();
        std::fs::write(
            proj.join("src").join("helper.fai"),
            "use { Widget, makeWidget } from WidgetPkg\n\n# Helper.\ndef helperWidget\n    @return Widget\ndo\n  makeWidget('helper')\nend\n",
        ).unwrap();

        // prepare_module_directory has no access to fai.toml — external types unknown.
        let src_dir = proj.join("src").to_string_lossy().into_owned();
        let prepared = fai_compiler::prepare_module_directory(&src_dir).unwrap();
        let mut checker = fai_checker::Checker::new();
        let result = run_checker(&mut checker, &prepared);
        assert!(
            result.is_err(),
            "prepare_module_directory without dep resolution should fail type check"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Widget") || err.contains("Unknown"),
            "error should mention the unknown type; got: {}",
            err
        );
    }

    // ── print_usage ──────────────────────────────────────────────────

    #[test]
    fn test_print_usage_no_panic() {
        // Verifies print_usage runs without panic and covers those lines
        print_usage();
    }

    // ── resolve_default_entry_point ──────────────────────────────────
    //
    // Regression tests for the multi-target error path. `fai run` in a
    // fullstack project with both `[project.client]` and
    // `[project.server]` used to print a positional-argument hint
    // (`fai run client`) — we now require `--project NAME` for
    // consistency with the rest of the CLI. These tests exercise the
    // decision logic via `resolve_default_entry_point_at` so the
    // behaviour doesn't depend on the runtime cwd (which is shared
    // across parallel tests).

    #[test]
    fn test_resolve_default_entry_multi_target_returns_none() {
        // With 2+ sub-projects and no --project flag, the function
        // must return None so the caller exits with the usage hint.
        // A silent fallback (e.g. picking the first target
        // alphabetically) would be worse — the two targets have
        // different effects (server starts a listener, client bundles
        // WASM).
        let root = temp_dir("resolve_default_multi");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\n\
             name = \"Multi\"\n\
             version = \"0.1.0\"\n\
             \n\
             [project.client]\n\
             target = \"wasm-html\"\n\
             source = \"src/client\"\n\
             \n\
             [project.server]\n\
             target = \"native\"\n\
             source = \"src/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/client")).unwrap();
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::write(
            root.join("src/client/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/server/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_none(),
            "multi-target project without --project should return None, got {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_default_entry_single_sub_project_picks_it() {
        // Only one sub-project declared — treat it as the default.
        // This preserves the ergonomic case where a workspace grows
        // one target first, then adds a second (and becomes subject
        // to the multi-target rule above).
        let root = temp_dir("resolve_default_single");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\n\
             name = \"Single\"\n\
             version = \"0.1.0\"\n\
             \n\
             [project.server]\n\
             target = \"native\"\n\
             source = \"src/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/server")).unwrap();
        std::fs::write(
            root.join("src/server/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_some(),
            "single sub-project should resolve to its main.fai"
        );
        assert!(
            result
                .as_deref()
                .unwrap_or("")
                .ends_with("src/server/main.fai"),
            "resolved path should point at src/server/main.fai, got {:?}",
            result
        );
    }

    #[test]
    fn test_resolve_default_entry_plain_project_uses_src_convention() {
        // A project with no sub-projects at all — legacy/plain
        // layout — still resolves via the `source_root = "src"`
        // convention. Nothing to do with multi-target, just making
        // sure the refactor didn't break the default path.
        let root = temp_dir("resolve_default_plain");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"Plain\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.fai"),
            "def main\n    @return Void\ndo\nend\n",
        )
        .unwrap();

        let result = resolve_default_entry_point_at(&root);
        assert!(
            result.is_some(),
            "plain project with src/main.fai should resolve"
        );
    }

    // ── Scaffold functions ───────────────────────────────────────────

    #[test]
    fn test_scaffold_main_contains_name() {
        let out = scaffold_main("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("print("));
    }

    #[test]
    fn test_scaffold_fai_toml_structure() {
        let out = scaffold_fai_toml("myproject");
        assert!(out.contains("[project]"));
        assert!(out.contains("name = \"myproject\""));
        assert!(out.contains("version = \"0.1.0\""));
        assert!(out.contains("source_root = \"src\""));
        assert!(out.contains("[dependencies]"));
    }

    #[test]
    fn test_scaffold_readme_contains_name() {
        let out = scaffold_readme("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("fai run"));
    }

    #[test]
    fn test_scaffold_language_md_has_sections() {
        let out = scaffold_language_md();
        assert!(out.contains("## Types"));
        assert!(out.contains("## Functions"));
        assert!(out.contains("## Variables"));
        assert!(out.contains("## Control Flow"));
        assert!(out.contains("## Modules and Imports"));
        assert!(out.contains("## Standard Library"));
        assert!(out.contains("## Testing"));
    }

    #[test]
    fn test_scaffold_claude_md_contains_name() {
        let out = scaffold_claude_md("myproject");
        assert!(out.contains("myproject"));
        assert!(out.contains("fai run"));
        assert!(out.contains("fai check"));
        assert!(out.contains("private:"));
        assert!(
            out.contains("one file per function")
                || out.contains("One file per function")
                || out.contains("one function per file")
                || out.contains("one-file-per-function")
        );
    }

    #[test]
    fn test_scaffold_agents_md_has_content() {
        let out = scaffold_agents_md();
        assert!(out.contains("fai run"));
        assert!(out.contains("fai check"));
        assert!(out.contains("private:"));
        assert!(out.contains("## File Structure Rules") || out.contains("## File Structure"));
    }

    // ── HTML loader generators ───────────────────────────────────────

    #[test]
    fn test_generate_html_loader_contains_filename() {
        let html = generate_html_loader("app.wasm");
        assert!(html.contains("app.wasm"));
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("<script>"));
    }

    #[test]
    fn test_generate_html_loader_old_contains_filename() {
        let html = generate_html_loader_old("bundle.wasm");
        assert!(html.contains("bundle.wasm"));
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("WebAssembly"));
    }

    // ── require_file_arg ─────────────────────────────────────────────

    #[test]
    fn test_require_file_arg_finds_path() {
        let args: Vec<String> = vec!["myfile.fai".to_string()];
        let result = require_file_arg(&args, "run");
        assert_eq!(result, "myfile.fai");
    }

    #[test]
    fn test_require_file_arg_skips_flags() {
        let args: Vec<String> = vec!["--wasm".to_string(), "myfile.fai".to_string()];
        let result = require_file_arg(&args, "run");
        assert_eq!(result, "myfile.fai");
    }

    // ── read_project_info ────────────────────────────────────────────

    #[test]
    fn test_read_project_info_no_dir() {
        let (name, version, _) = read_project_info(None);
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
    }

    #[test]
    fn test_read_project_info_with_toml() {
        let dir = temp_dir("proj_info");
        // Include a comment and unknown key to exercise lines 307 (unknown key → `_ => {}`)
        // and 309 (line with no `=` like a comment → split_once returns None)
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\n# a comment\nname = \"myapp\"\nversion = \"1.2.3\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "myapp");
        assert_eq!(version, "1.2.3");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_root_path() {
        // Covers line 290: the else branch when src_path.parent() is None
        // This happens when the path is "/" (root) which has no parent
        let (name, version, _) = read_project_info(Some("/"));
        // Should return defaults since /fai.toml doesn't exist (or isn't readable)
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
    }

    #[test]
    fn test_read_project_info_no_toml_file() {
        let dir = temp_dir("proj_info_nofile");
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "unknown");
        assert_eq!(version, "0.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── find_source_root ─────────────────────────────────────────────

    #[test]
    fn test_find_source_root_with_toml() {
        let dir = temp_dir("src_root");
        std::fs::write(dir.join("fai.toml"), "[project]\nsource_root = \"src\"\n").unwrap();
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();

        let root = find_source_root(file.to_str().unwrap());
        assert!(root.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_source_root_no_toml() {
        let dir = temp_dir("src_root_none");
        let file = dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();
        // No fai.toml in the directory tree (temp_dir is deep)
        // Result may be None or Some depending on filesystem — just verify no panic
        let _ = find_source_root(file.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_find_source_root_multi_section_toml() {
        // Covers the `if !in_project { continue; }` branch (line 1145) when
        // fai.toml has sections before [project]
        let dir = temp_dir("src_root_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[meta]\ndescription = \"test\"\n\n[project]\nsource_root = \"src\"\n",
        )
        .unwrap();
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let file = src_dir.join("main.fai");
        std::fs::write(&file, SIMPLE_FAI).unwrap();

        let root = find_source_root(file.to_str().unwrap());
        assert!(root.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_multi_section_toml() {
        // Covers the `if !in_project { continue; }` branch in read_project_info
        let dir = temp_dir("proj_info_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[meta]\nauthors = [\"foo\"]\n\n[project]\nname = \"myapp\"\nversion = \"2.0.0\"\n",
        )
        .unwrap();
        let (name, version, _) = read_project_info(Some(dir.to_str().unwrap()));
        assert_eq!(name, "myapp");
        assert_eq!(version, "2.0.0");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_check ────────────────────────────────────────────────────

    #[test]
    fn test_cmd_check_valid_file() {
        let path = write_fai("cmd_check", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_check(&args); // should print "ok" and return normally
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    // ── cmd_fmt ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_fmt_already_formatted() {
        let path = write_fai("cmd_fmt", "let x = 42\n");
        let args: Vec<String> = vec![path.clone()];
        cmd_fmt(&args); // should print "already formatted"
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_fmt_check_mode() {
        let path = write_fai("cmd_fmt_check", "let x = 42\n");
        let args: Vec<String> = vec![path.clone(), "--check".to_string()];
        cmd_fmt(&args); // check mode — should print "ok"
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_fmt_formats_and_prints_path() {
        // Covers the "formatted <path>" loop (lines 497-499)
        let dir = temp_dir("cmd_fmt_formatted");
        let path = dir.join("test.fai");
        // Write without trailing newline — needs reformatting
        std::fs::write(&path, "let x = 42").unwrap();

        let args: Vec<String> = vec![path.to_str().unwrap().to_string()];
        cmd_fmt(&args); // should print "formatted <path>"

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "let x = 42\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_run ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_run_fai_file() {
        let path = write_fai("cmd_run", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_run(&args);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_run_wasm_flag() {
        // Tests the --wasm JIT path in cmd_run
        let path = write_fai("cmd_run_wasm", SIMPLE_FAI);
        let args: Vec<String> = vec![path.clone(), "--wasm".to_string()];
        cmd_run(&args);
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_run_wasm_file() {
        // Tests running a pre-compiled .wasm file directly
        let dir = temp_dir("cmd_run_wasm_file");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        // First build the .wasm file
        let build_args: Vec<String> = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&build_args);
        assert!(wasm_path.exists(), "wasm file must exist for this test");

        // Then run it directly
        let run_args: Vec<String> = vec![wasm_path.to_str().unwrap().to_string()];
        cmd_run(&run_args);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_test ─────────────────────────────────────────────────────

    // ── cmd_build ────────────────────────────────────────────────────

    #[test]
    fn test_cmd_build_produces_wasm() {
        let dir = temp_dir("cmd_build");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        assert!(wasm_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_with_output_flag() {
        let dir = temp_dir("cmd_build_o");
        let fai_path = dir.join("prog.fai");
        let out_path = dir.join("out.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            out_path.to_str().unwrap().to_string(),
        ];
        cmd_build(&args);

        assert!(out_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_non_fai_extension() {
        // Tests the else branch when path doesn't end with .fai (line 207)
        let dir = temp_dir("cmd_build_txt");
        let txt_path = dir.join("prog.txt");
        std::fs::write(&txt_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![txt_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        assert!(dir.join("prog.txt.wasm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_runtime_js_exports_submit_handler() {
        // Regression guard: the forui view layer registers onSubmit handlers
        // for TextInput Enter-key events. The generated runtime JS must expose
        // a `handleSubmitEvent(id)` bridge that calls the wasm
        // `invokeSubmitHandler` export.
        let js = generate_runtime_js("prog.wasm");
        assert!(
            js.contains("function handleSubmitEvent"),
            "generated runtime JS is missing handleSubmitEvent:\n{}",
            js
        );
        assert!(
            js.contains("invokeSubmitHandler"),
            "handleSubmitEvent doesn't call invokeSubmitHandler:\n{}",
            js
        );
        // Sibling bridges should still be present so this test doesn't falsely
        // pass by accident if someone refactors.
        assert!(js.contains("function handleEvent"));
        assert!(js.contains("function handleInputEvent"));
    }

    #[test]
    fn test_cmd_build_with_html_flag() {
        let dir = temp_dir("cmd_build_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let fai_path = src.join("prog.fai");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Test\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();

        let args: Vec<String> = vec![fai_path.to_str().unwrap().to_string(), "--html".to_string()];
        cmd_build(&args);

        let public = dir.join("public");
        assert!(public.join("prog.wasm").exists());
        assert!(public.join("index.html").exists());
        assert!(public.join("fai-runtime.js").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_with_target_wasm_html() {
        // Same behaviour as --html, but declared in fai.toml via
        // `target = "wasm-html"`. Plan 99 Phase 2.1.
        let dir = temp_dir("cmd_build_target_wasm_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let fai_path = src.join("prog.fai");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"Test\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"wasm-html\"\n",
        ).unwrap();

        // No --html flag — target alone drives the html bundle.
        let args = vec![fai_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        let public = dir.join("public");
        assert!(public.join("prog.wasm").exists());
        assert!(public.join("index.html").exists());
        assert!(public.join("fai-runtime.js").exists());
        assert!(public.join("forui.css").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_parses_target() {
        let dir = temp_dir("proj_info_target");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"T\"\nversion = \"0.1.0\"\ntarget = \"wasm-html\"\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert_eq!(info.target, Some(BuildTarget::WasmHtml));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_parses_workspace_members() {
        let dir = temp_dir("proj_info_workspace");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[workspace]\nmembers = [\"shared\", \"server\", \"client\"]\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert_eq!(info.workspace_members, vec!["shared", "server", "client"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_read_project_info_parses_remote_interface() {
        let dir = temp_dir("proj_info_remote_iface");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"S\"\nversion = \"0.1.0\"\n\n[remote-interface]\nexpose = true\n",
        )
        .unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert!(info.interface_expose);
        assert!(info.interface_from.is_none());
        let _ = std::fs::remove_dir_all(&dir);

        let dir = temp_dir("proj_info_remote_from");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"C\"\nversion = \"0.1.0\"\n\n[remote-interface]\nfrom = \"SharedPkg\"\n",
        ).unwrap();
        let info = read_project_info_full(Some(dir.to_str().unwrap()));
        assert!(!info.interface_expose);
        assert_eq!(info.interface_from.as_deref(), Some("SharedPkg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Plan 101: Sub-project and remote dependency parsing ──────

    #[test]
    fn test_parse_sub_projects() {
        let info = parse_project_info(
            "[project]\nname = \"TodoApp\"\nversion = \"0.1.0\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\nbuild_dir = \"client/public\"\n\n\
             [project.server]\ntarget = \"native\"\nsource = \"server/src\"\n\n\
             [project.shared]\nsource = \"shared/src\"\n"
        );
        assert_eq!(info.name, "TodoApp");
        assert_eq!(info.sub_projects.len(), 3);

        let client = &info.sub_projects["client"];
        assert_eq!(client.target, Some(BuildTarget::WasmHtml));
        assert_eq!(client.source.as_deref(), Some("client/src"));
        assert_eq!(client.build_dir.as_deref(), Some("client/public"));

        let server = &info.sub_projects["server"];
        assert_eq!(server.target, Some(BuildTarget::Native));
        assert_eq!(server.source.as_deref(), Some("server/src"));
        assert!(server.build_dir.is_none());

        let shared = &info.sub_projects["shared"];
        assert!(shared.target.is_none());
        assert_eq!(shared.source.as_deref(), Some("shared/src"));
    }

    #[test]
    fn test_parse_sub_projects_dont_clobber_root() {
        // Sub-project sections shouldn't overwrite root [project] fields
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"2.0.0\"\ntarget = \"wasm\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\n",
        );
        assert_eq!(info.name, "App");
        assert_eq!(info.version, "2.0.0");
        assert_eq!(info.target, Some(BuildTarget::Wasm));
        assert_eq!(info.sub_projects.len(), 1);
        assert_eq!(
            info.sub_projects["client"].target,
            Some(BuildTarget::WasmHtml)
        );
    }

    #[test]
    fn test_parse_remote_dependency_config() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [project.client]\ntarget = \"wasm-html\"\nsource = \"client/src\"\n\n\
             [project.client.dependencies.shared.remote.dev]\nurl = \"http://localhost:3040\"\n\n\
             [project.client.dependencies.shared.remote.prod]\nurl = \"https://api.myapp.com\"\n",
        );
        let client = &info.sub_projects["client"];
        assert_eq!(client.remote_deps.len(), 1);
        let shared_remote = &client.remote_deps["shared"];
        assert_eq!(shared_remote.len(), 2);
        assert_eq!(shared_remote["dev"].url, "http://localhost:3040");
        assert_eq!(shared_remote["prod"].url, "https://api.myapp.com");
    }

    #[test]
    fn test_parse_multiple_remote_deps() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\n\n\
             [project.client]\nsource = \"src\"\n\n\
             [project.client.dependencies.auth.remote.dev]\nurl = \"http://localhost:4000\"\n\n\
             [project.client.dependencies.tasks.remote.dev]\nurl = \"http://localhost:4001\"\n",
        );
        let client = &info.sub_projects["client"];
        assert_eq!(client.remote_deps.len(), 2);
        assert_eq!(
            client.remote_deps["auth"]["dev"].url,
            "http://localhost:4000"
        );
        assert_eq!(
            client.remote_deps["tasks"]["dev"].url,
            "http://localhost:4001"
        );
    }

    #[test]
    fn test_parse_single_project_no_sub_projects() {
        // Single-project toml should still work with zero sub-projects
        let info = parse_project_info(
            "[project]\nname = \"MyTool\"\nversion = \"1.0.0\"\ntarget = \"native\"\n",
        );
        assert_eq!(info.name, "MyTool");
        assert_eq!(info.target, Some(BuildTarget::Native));
        assert!(info.sub_projects.is_empty());
    }

    #[test]
    fn test_parse_backwards_compat_workspace_members() {
        // Old workspace format should still work
        let info =
            parse_project_info("[workspace]\nmembers = [\"shared\", \"server\", \"client\"]\n");
        assert_eq!(info.workspace_members, vec!["shared", "server", "client"]);
        assert!(info.sub_projects.is_empty());
    }

    // ── Plan 101: Project root, entry point, target resolution ──

    #[test]
    fn test_find_project_root() {
        let dir = temp_dir("proj_root");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("fai.toml"), "[project]\nname = \"X\"\n").unwrap();
        // From the src subdirectory, should find the parent
        let found = find_project_root(&dir.join("src"));
        assert_eq!(found.unwrap(), dir);
        // From the root itself
        let found = find_project_root(&dir);
        assert_eq!(found.unwrap(), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_main() {
        let dir = temp_dir("entry_main");
        std::fs::create_dir_all(dir.join("server/src")).unwrap();
        std::fs::write(
            dir.join("server/src/main.fai"),
            "def main\n    @return Void\ndo\n  print('hi')\nend\n",
        )
        .unwrap();
        std::fs::write(dir.join("server/src/other.fai"), "").unwrap();
        let entry = resolve_entry_point(&dir, "server/src");
        assert_eq!(entry.unwrap().file_name().unwrap(), "main.fai");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_first_fai() {
        let dir = temp_dir("entry_first");
        std::fs::create_dir_all(dir.join("client/src")).unwrap();
        std::fs::write(dir.join("client/src/todoclient.fai"), "").unwrap();
        let entry = resolve_entry_point(&dir, "client/src");
        assert!(entry.is_some());
        assert!(entry.unwrap().extension().unwrap() == "fai");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_resolve_entry_point_missing_dir() {
        let dir = temp_dir("entry_missing");
        std::fs::create_dir_all(&dir).unwrap();
        let entry = resolve_entry_point(&dir, "nonexistent/src");
        assert!(entry.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_select_targets_by_name() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n\n\
             [project.server]\nsource = \"server/src\"\n",
        );
        let targets = select_targets(&info, Some("client"));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "client");
    }

    #[test]
    fn test_select_targets_all() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n\n\
             [project.server]\nsource = \"server/src\"\n",
        );
        let targets = select_targets(&info, None);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_select_targets_unknown_name() {
        let info = parse_project_info(
            "[project]\nname = \"App\"\n\n\
             [project.client]\nsource = \"client/src\"\n",
        );
        let targets = select_targets(&info, Some("nope"));
        assert!(targets.is_empty());
    }

    #[test]
    fn test_select_targets_single_project() {
        let info = parse_project_info("[project]\nname = \"Tool\"\ntarget = \"native\"\n");
        let targets = select_targets(&info, None);
        assert!(
            targets.is_empty(),
            "single project returns empty — handled separately"
        );
    }

    #[test]
    fn test_pack_native_binary_trailer_layout() {
        // Unit test: pack_native_binary produces [forai][wasm][magic][len]
        // and read_embedded_wasm on that file extracts the wasm back.
        // Plan 99 Phase 3.
        let dir = temp_dir("pack_native_layout");
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("embedded");
        let wasm = b"\x00asm\x01\x00\x00\x00fake-wasm-body"; // minimal wasm magic + filler
        pack_native_binary(wasm, &out).expect("pack should succeed");

        let bytes = std::fs::read(&out).unwrap();
        assert!(
            bytes.len() > 16 + wasm.len(),
            "output should include forai + wasm + trailer"
        );

        // Trailer: last 16 bytes = magic + u64 length.
        let n = bytes.len();
        assert_eq!(&bytes[n - 16..n - 8], NATIVE_TRAILER_MAGIC);
        let len = u64::from_le_bytes(bytes[n - 8..n].try_into().unwrap());
        assert_eq!(len as usize, wasm.len());

        // Wasm payload right before the trailer.
        let payload_start = n - 16 - wasm.len();
        assert_eq!(&bytes[payload_start..n - 16], wasm);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_native_build_produces_runnable_binary() {
        // End-to-end: `cmd_build` with target="native" writes a
        // self-extracting binary; spawning it should run the program
        // and emit the expected print output. Plan 99 Phase 3.
        //
        // Requires a built forai binary at <workspace>/target/debug/forai.
        // Skipped when the binary isn't present to avoid spurious
        // failures in environments that haven't built it yet.
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace = manifest.parent().unwrap().parent().unwrap();
        let forai_bin = workspace.join("target").join("debug").join("forai");
        if !forai_bin.exists() {
            eprintln!(
                "skipping native-build e2e test: {} missing. `cargo build` first.",
                forai_bin.display()
            );
            return;
        }

        let dir = temp_dir("native_e2e");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"NativeTest\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"native\"\n",
        ).unwrap();
        let fai_path = src.join("main.fai");
        let src_code = concat!(
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  print('native binary says hi')\n",
            "end\n",
        );
        std::fs::write(&fai_path, src_code).unwrap();

        // Point pack_native_binary at the real forai binary rather
        // than the cargo test harness (which is what current_exe
        // would otherwise return).
        std::env::set_var("FORAI_SELF_BINARY", &forai_bin);
        cmd_build(&[fai_path.to_str().unwrap().to_string()]);
        std::env::remove_var("FORAI_SELF_BINARY");

        let native = src.join("main");
        assert!(
            native.exists(),
            "native binary not produced at {}",
            native.display()
        );

        // Execute it and check stdout.
        let out = std::process::Command::new(&native)
            .output()
            .expect("failed to spawn native binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "native binary exited nonzero. stderr: {}",
            stderr
        );
        assert!(
            stdout.contains("native binary says hi"),
            "native binary stdout missing expected output. stdout: {}",
            stdout
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Regression: the plain forai binary (no embedded wasm) must
    // dispatch to normal CLI, not try to self-extract. Covered by
    // main.rs's test_help_flag + test_run_prints_output — both spawn
    // the forai binary without a trailer and expect CLI behaviour.

    #[test]
    fn test_cmd_build_workspace_iterates_members() {
        // Workspace with two members, each a minimal package.
        // `forai build` invoked in the workspace root (via cwd) should
        // build both. Plan 99 Phase 2.2.
        let dir = temp_dir("cmd_build_workspace");
        std::fs::create_dir_all(&dir).unwrap();

        // Workspace root toml listing the two members.
        std::fs::write(
            dir.join("fai.toml"),
            "[workspace]\nmembers = [\"pkg_a\", \"pkg_b\"]\n",
        )
        .unwrap();

        // Member A: entry point at src/main.fai
        let a_src = dir.join("pkg_a").join("src");
        std::fs::create_dir_all(&a_src).unwrap();
        std::fs::write(
            dir.join("pkg_a").join("fai.toml"),
            "[project]\nname = \"PkgA\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(a_src.join("main.fai"), SIMPLE_FAI).unwrap();

        // Member B: entry point at src/pkgb.fai (named convention).
        let b_src = dir.join("pkg_b").join("src");
        std::fs::create_dir_all(&b_src).unwrap();
        std::fs::write(
            dir.join("pkg_b").join("fai.toml"),
            "[project]\nname = \"PkgB\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(b_src.join("pkgb.fai"), SIMPLE_FAI).unwrap();

        // Change cwd into the workspace root and invoke `forai build`
        // with no file arg.
        let _guard = cwd_test_lock();
        let prev_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[]);
        std::env::set_current_dir(&prev_cwd).unwrap();

        // Both members should have produced a .wasm next to their
        // source file (default output location when no build_dir set).
        assert!(
            a_src.join("main.wasm").exists(),
            "pkg_a main.wasm should exist, dir contents: {:?}",
            std::fs::read_dir(&a_src).unwrap().collect::<Vec<_>>()
        );
        assert!(
            b_src.join("pkgb.wasm").exists(),
            "pkg_b pkgb.wasm should exist, dir contents: {:?}",
            std::fs::read_dir(&b_src).unwrap().collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── lift_target_name_positional ──────────────────────────────────

    /// Helper that writes a fai.toml with two sub-projects (`client`,
    /// `server`) at `dir`. Used by the lift + scoping tests below.
    fn write_sub_project_toml(dir: &std::path::Path) {
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.client]\ntarget = \"wasm\"\nsource = \"src/client\"\nmain = \"src/client/main.fai\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\n",
        ).unwrap();
    }

    #[test]
    fn test_lift_target_name_positional_recognises_sub_project() {
        // `fai build client` with a fai.toml that has [project.client]
        // should lift `client` to the project flag and remove it from
        // args so step_fmt/check/test don't try to open "client" as a file.
        let dir = temp_dir("lift_target_name_matches");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["client".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            args.is_empty(),
            "positional should be stripped, got {:?}",
            args
        );
        assert_eq!(project.as_deref(), Some("client"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_ignores_file_paths() {
        // `fai build src/main.fai` should NOT be lifted — it's a file
        // path, not a target name.
        let dir = temp_dir("lift_target_name_filepath");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["src/main.fai".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["src/main.fai".to_string()]);
        assert!(project.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_ignores_unknown_name() {
        // `fai build notatarget` — no match in sub_projects, leave alone.
        // cmd_build will then fall through to the file-open path which
        // produces the normal "no such file" error.
        let dir = temp_dir("lift_target_name_unknown");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) = lift_target_name_positional(vec!["notatarget".to_string()], None);
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["notatarget".to_string()]);
        assert!(project.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_lift_target_name_positional_leaves_explicit_project_flag_alone() {
        // When --project is already set, positional (even if it matches
        // a sub-project) must pass through untouched. This keeps the
        // user's explicit flag authoritative.
        let dir = temp_dir("lift_target_name_explicit");
        write_sub_project_toml(&dir);

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let (args, project) =
            lift_target_name_positional(vec!["client".to_string()], Some("server".to_string()));
        std::env::set_current_dir(&prev).unwrap();

        assert_eq!(args, vec!["client".to_string()]);
        assert_eq!(project.as_deref(), Some("server"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_build target scoping ────────────────────────────────────

    /// End-to-end test: with two sub-projects, `cmd_build` scoped to
    /// one target (via --project or positional) should only produce
    /// that target's build output.
    ///
    /// Uses `target = "wasm"` so no forai binary / HTML renderer is
    /// needed — the build step just emits a .wasm file.
    fn build_two_sub_projects_and_check(
        tag: &str,
        args: Vec<String>,
        expect_client: bool,
        expect_server: bool,
    ) {
        let dir = temp_dir(tag);
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"app\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.client]\ntarget = \"wasm\"\nsource = \"src/client\"\nmain = \"src/client/main.fai\"\nbuild_dir = \"build/client\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\nbuild_dir = \"build/server\"\n",
        ).unwrap();
        let client_src = dir.join("src/client");
        let server_src = dir.join("src/server");
        std::fs::create_dir_all(&client_src).unwrap();
        std::fs::create_dir_all(&server_src).unwrap();
        std::fs::write(client_src.join("main.fai"), SIMPLE_FAI).unwrap();
        std::fs::write(server_src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&args);
        std::env::set_current_dir(&prev).unwrap();

        let client_out = dir.join("build/client/main.wasm");
        let server_out = dir.join("build/server/main.wasm");
        assert_eq!(
            client_out.exists(),
            expect_client,
            "client wasm present={} but expected={} for args {:?}",
            client_out.exists(),
            expect_client,
            args
        );
        assert_eq!(
            server_out.exists(),
            expect_server,
            "server wasm present={} but expected={} for args {:?}",
            server_out.exists(),
            expect_server,
            args
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── scan_module_for_tests_and_publics ────────────────────────────

    const SIBLING_ENTRY_FAI: &str = concat!(
        "use { App } from client\n\n",
        "def main\n",
        "    @return Void\n",
        "do\n",
        "  print(App())\n",
        "end\n",
    );

    const SIBLING_APP_FAI: &str = concat!(
        "# The app shell.\n",
        "def App\n",
        "    @return String\n",
        "do\n",
        "  'hi'\n",
        "end\n",
    );

    #[test]
    fn test_scan_module_picks_up_sibling_public_fns() {
        // Regression for the partners bug: entry file has no public
        // functions (just `main`), but a sibling file defines `App`.
        // scan_module must report App so the early-return path fails
        // with "missing test block" instead of reporting "no public
        // functions to test".
        let dir = temp_dir("scan_module_sibling_publics");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        std::fs::write(dir.join("app.fai"), SIBLING_APP_FAI).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests, "neither file has a test block");
        assert_eq!(publics, vec!["App".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_detects_sibling_test_block() {
        // When a sibling file has a test block (but the entry file
        // doesn't), has_test_blocks must still come back true so the
        // module proceeds to the VM instead of short-circuiting.
        let dir = temp_dir("scan_module_sibling_tests");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        // Use a raw literal for the test-block variant so we don't
        // fight with the macro above.
        std::fs::write(
            dir.join("app.fai"),
            "# The app shell.\ndef App\n    @return String\ndo\n  'hi'\nend\n\ntest App\nit 'returns hi'\n  assert.equals(App(), 'hi')\nend\nend\n",
        ).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(has_tests, "sibling file has a test block");
        assert_eq!(publics, vec!["App".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_entry_only_still_works() {
        // Single-file module (no siblings) — behaviour should match
        // what the old entry-only scan produced.
        let dir = temp_dir("scan_module_entry_only");
        let entry = dir.join("solo.fai");
        std::fs::write(
            &entry,
            "# A greeting.\ndef greet\n    @return String\ndo\n  'hi'\nend\n\ndef main\n    @return Void\ndo\n  print(greet())\nend\n",
        ).unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests);
        assert_eq!(publics, vec!["greet".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_walks_nested_directories() {
        // Regression: the partners client has `pages/`, `components/`,
        // and `state/` subdirs under src/client, each holding public
        // functions. A non-recursive walker would miss all of them and
        // report `[ok] test — no public functions`. The recursive walker
        // must pick up public fns no matter how deep they are nested.
        let dir = temp_dir("scan_module_nested_dirs");
        let entry = dir.join("main.fai");
        std::fs::write(&entry, SIBLING_ENTRY_FAI).unwrap();
        std::fs::write(dir.join("app.fai"), SIBLING_APP_FAI).unwrap();

        let components = dir.join("components");
        std::fs::create_dir_all(&components).unwrap();
        std::fs::write(
            components.join("button.fai"),
            "# A button.\ndef Button\n    @return String\ndo\n  'click'\nend\n",
        )
        .unwrap();

        let pages = dir.join("pages");
        let pages_team = pages.join("team");
        std::fs::create_dir_all(&pages_team).unwrap();
        std::fs::write(
            pages.join("home.fai"),
            "# Home page.\ndef HomePage\n    @return String\ndo\n  'home'\nend\n",
        )
        .unwrap();
        // Two levels deep — must still be found.
        std::fs::write(
            pages_team.join("detail.fai"),
            "# Team detail.\ndef TeamDetail\n    @return String\ndo\n  'team'\nend\n",
        )
        .unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (has_tests, publics) = scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert!(!has_tests);
        assert_eq!(
            publics,
            vec![
                "App".to_string(),
                "Button".to_string(),
                "HomePage".to_string(),
                "TeamDetail".to_string(),
            ],
            "expected all public fns across nested module dirs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_collect_fai_files_recursive_returns_all_depths() {
        let dir = temp_dir("collect_recursive");
        std::fs::write(dir.join("a.fai"), "").unwrap();
        let nested = dir.join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("b.fai"), "").unwrap();
        std::fs::write(dir.join("sub").join("c.fai"), "").unwrap();
        // Non-.fai files must be skipped.
        std::fs::write(dir.join("notes.md"), "").unwrap();

        let files = collect_fai_files_recursive(&dir);
        let names: Vec<String> = files
            .iter()
            .map(|f| {
                std::path::Path::new(f)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(
            names,
            vec![
                "a.fai".to_string(),
                "c.fai".to_string(),
                "b.fai".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_scan_module_deduplicates_public_fns() {
        // If two files accidentally define the same public name (or the
        // entry is listed twice), the returned list must not repeat it.
        // This is a defensive check — the compiler would reject the
        // actual duplicate, but scan shouldn't inflate the missing-test
        // count before we even try to compile.
        let dir = temp_dir("scan_module_dedup");
        let entry = dir.join("main.fai");
        std::fs::write(
            &entry,
            "# A greeting.\ndef greet\n    @return String\ndo\n  'hi'\nend\n\ndef main\n    @return Void\ndo\n  print(greet())\nend\n",
        ).unwrap();
        // Second file repeats `greet` — scan_module must dedupe.
        std::fs::write(
            dir.join("other.fai"),
            "# Another greeting.\ndef greet\n    @return String\ndo\n  'yo'\nend\n",
        )
        .unwrap();

        let raw = std::fs::read_to_string(&entry).unwrap();
        let (_has_tests, publics) =
            scan_module_for_tests_and_publics(entry.to_str().unwrap(), &raw);

        assert_eq!(publics, vec!["greet".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_project_flag_scopes_to_one_target() {
        build_two_sub_projects_and_check(
            "cmd_build_project_flag",
            vec!["--project".to_string(), "client".to_string()],
            true,
            false,
        );
    }

    #[test]
    fn test_cmd_build_positional_target_name_scopes_to_one_target() {
        build_two_sub_projects_and_check(
            "cmd_build_positional_target",
            vec!["client".to_string()],
            true,
            false,
        );
    }

    #[test]
    fn test_cmd_build_no_args_builds_all_targets() {
        build_two_sub_projects_and_check("cmd_build_all", vec![], true, true);
    }

    #[test]
    fn test_cmd_build_html_write_warning() {
        // Covers the `Err(e) => eprintln!("warning...")` path (line 231)
        // by making the html output path a directory so the write fails gracefully
        let dir = temp_dir("cmd_build_html_warn");
        let fai_path = dir.join("prog.fai");
        let wasm_path = dir.join("prog.wasm");
        std::fs::write(&fai_path, SIMPLE_FAI).unwrap();
        // Create a directory named "prog.html" — fs::write to a dir fails
        std::fs::create_dir_all(dir.join("prog.html")).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            wasm_path.to_str().unwrap().to_string(),
            "--html".to_string(),
        ];
        cmd_build(&args); // html write fails with warning, wasm still written

        assert!(wasm_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_interface ────────────────────────────────────────────────

    #[test]
    fn test_cmd_interface_outputs_json() {
        let path = write_fai("cmd_iface", INTERFACE_FAI);
        let args: Vec<String> = vec![path.clone()];
        cmd_interface(&args); // prints JSON to stdout
        let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
    }

    #[test]
    fn test_cmd_interface_with_output_file() {
        let dir = temp_dir("cmd_iface_o");
        let fai_path = dir.join("prog.fai");
        let out_path = dir.join("interface.json");
        std::fs::write(&fai_path, INTERFACE_FAI).unwrap();

        let args: Vec<String> = vec![
            fai_path.to_str().unwrap().to_string(),
            "-o".to_string(),
            out_path.to_str().unwrap().to_string(),
        ];
        cmd_interface(&args);

        assert!(out_path.exists());
        let json = std::fs::read_to_string(&out_path).unwrap();
        assert!(json.contains("\"functions\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── cmd_new ──────────────────────────────────────────────────────

    #[test]
    fn test_cmd_new_creates_project() {
        let base = temp_dir("cmd_new_base");
        let project_path = base.join("myproject");

        // cmd_new creates the project at the given path
        let args: Vec<String> = vec![project_path.to_str().unwrap().to_string()];
        cmd_new(&args);

        assert!(project_path.join("src").join("main.fai").exists());
        assert!(project_path.join("fai.toml").exists());
        assert!(project_path.join("README.md").exists());
        assert!(project_path.join("language.md").exists());
        assert!(project_path.join("CLAUDE.md").exists());
        assert!(project_path.join("AGENTS.md").exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
