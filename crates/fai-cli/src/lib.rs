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
mod rpc_surface;
mod templates;
mod test_meta;
mod wasm_runner;

use report::{
    count_fai_files_recursive, extract_checked_flag, extract_verbose_flag, is_verbose, Reporter,
    StepStatus,
};

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
    eprintln!("  test [file] [--checked] fmt → check → test");
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
    eprintln!("  run --watchdog[=secs]   Kill a hung run after secs (default 10) with a");
    eprintln!("                          post-mortem dump (task table + heap stats)");
    eprintln!("  run --check-leaks       Heap allocation ledger: itemized live set at");
    eprintln!("                          exit/trap, grouped by size + allocation site");
    eprintln!("  run --check-leaks=interval:<ms>  Also print a live-set summary every");
    eprintln!("                          <ms> ms (for servers that never exit)");
    eprintln!("  run --debug             Debug umbrella (currently: --watchdog)");
    eprintln!("  test --checked          Build tests with cheap always-on memory");
    eprintln!("                          guards: trap an out-of-bounds index store");
    eprintln!("                          (xs[i]=v) and any single alloc past 256 MB");
    eprintln!("                          at the source, with a named reason. Use this");
    eprintln!("                          first when a test suite corrupts the heap.");
    eprintln!();
    eprintln!("Debugging (set as environment variables, e.g. FAI_RC_CHECK=1 fai test):");
    eprintln!("  These instrument the generated wasm for memory-corruption hunts.");
    eprintln!("  Most cost runtime, so they are off unless explicitly set. Reach for");
    eprintln!("  --checked / FAI_CHECKED first; escalate to the heavier ones below.");
    eprintln!();
    eprintln!("  FAI_CHECKED         Same guards as 'test --checked' (alloc-guard +");
    eprintln!("                      index-store bounds check). Cheap; safe to leave on.");
    eprintln!("  FAI_ALLOC_GUARD     Trap any single allocation past 256 MB — names the");
    eprintln!("                      size + backtrace of a runaway (concat/array blowup).");
    eprintln!("  FAI_RC_CHECK        Heavy: poison freed blocks and verify the free list");
    eprintln!("                      on each alloc/release; traps double-free, use-after-");
    eprintln!("                      free, free-list corruption. Implies --checked.");
    eprintln!("  FAI_HEAP_VERIFY     Scan the free-list head on every heap op (paranoid;");
    eprintln!("                      pairs with FAI_RC_CHECK to localize corruption).");
    eprintln!("  FAI_NO_REUSE        Never recycle freed blocks, so a stale read/write");
    eprintln!("                      traps at the offending op instead of after reuse.");
    eprintln!("  FAI_RC_WATCH=0xADDR Watchpoint: log every refcount change at object ADDR");
    eprintln!("                      with a backtrace (find who over-retains/releases).");
    eprintln!("  FAI_MEM_WATCH=0xADDR Watchpoint: log every write to memory word ADDR with");
    eprintln!("                      a backtrace (find who clobbers a specific address).");
    eprintln!("  FAI_CHECK_LEAKS     Allocation ledger (same as 'run --check-leaks'):");
    eprintln!("                      itemized live set at exit/trap, grouped by alloc site.");
    eprintln!("  FAI_TRACE_TESTS     Print each test case's name on stderr before it runs,");
    eprintln!("                      so a trap/hang is attributable to the exact case.");
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
    let (args, checked) = extract_checked_flag(&args);
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
    // Enable allocation-ledger codegen for the test build when
    // FAI_CHECK_LEAKS is set, mirroring cmd_run. Without this the test
    // wasm carries no `__fai_alloc_event` hooks, so the ledger (and its
    // interval report) stays silent during `fai test` — exactly when a
    // runaway allocator needs naming.
    if std::env::var_os("FAI_CHECK_LEAKS").is_some() {
        fai_codegen_wasm::set_check_leaks(true);
    }
    // `--checked` (plan 116): build the test wasm with the cheap,
    // always-safe corruption guards (alloc-guard past 256 MB + index-store
    // bounds check). These trap at the corruption site with a named reason
    // instead of letting it surface later as a runaway alloc or silent
    // clobber, and carry no measurable cost — unlike the heavy poison /
    // free-list scanning of FAI_RC_CHECK.
    if checked {
        fai_codegen_wasm::set_checked(true);
    }
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
            // wasm-only run path: drive the full build (deps + asset
            // copy + codegen) through step_build, then execute the
            // produced .wasm directly. step_build's per-target recursion
            // runs fmt/check/test for each target, so we don't run
            // them again at the cmd_run level — duplicating those
            // steps would just re-do the work the build already did.
            let mut build_args: Vec<String> = args.iter().cloned().collect();
            // Strip the ad-hoc positional `<target>` that cmd_run may
            // have already lifted into `project`; step_build pulls
            // the same name from `project` and would otherwise treat
            // the leftover positional as a file path.
            if let Some(name) = project.as_deref() {
                build_args.retain(|a| a != name);
            }
            // `--check-leaks` instruments codegen, and in project mode
            // the codegen happens HERE (step_build), not in step_run —
            // arm the gate before building or the artifact carries no
            // ledger events.
            if args
                .iter()
                .any(|a| a == "--check-leaks" || a.starts_with("--check-leaks="))
            {
                fai_codegen_wasm::set_check_leaks(true);
            }
            step_build(&build_args, project.as_deref(), &reporter);
            if let Some(wasm) = resolve_target_wasm_artifact(project.as_deref()) {
                // Keep `fai run --project <target>` rooted at the
                // project directory. Source-authored paths such as
                // `.env.dev`, `db/migrations`, and `build/web` are
                // project-root relative in the fullstack templates.
                // Users who want a self-contained build-dir runtime can
                // still `cd build/<target>` and run the wasm directly.
                // Forward the debug flags (--watchdog/--debug/
                // --check-leaks) so the runner half still sees them.
                let mut run_args: Vec<String> = vec![wasm];
                run_args.extend(args.iter().filter(|a| a.starts_with("--")).cloned());
                step_run(&run_args, None, &reporter);
                return;
            }
            // Fall through to the legacy in-memory compile-and-run if
            // the build didn't produce a resolvable artifact (e.g.
            // single-project mode without a build_dir).
        } else {
            // Raw .fai file path — keep the existing in-memory
            // compile-and-run path. fmt/check/test happen here so
            // ad-hoc scripts behave the same as before.
            let target_args = scoped_pipeline_args(&args, project.as_deref());
            step_fmt(&target_args, &reporter);
            step_check(&target_args, &reporter);
            step_test(&target_args, &reporter);
        }
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
    // A bare-name positional that survived the lift is neither a
    // sub-project in fai.toml nor something the fmt step could read as
    // a path — fail with guidance now instead of letting fmt report a
    // bare "error reading <name>: No such file or directory".
    if project.is_none() {
        let output_value_idx = args.iter().position(|a| a == "-o").map(|i| i + 1);
        let bare_name = args.iter().enumerate().find(|(i, a)| {
            Some(*i) != output_value_idx
                && !a.starts_with("--")
                && a.as_str() != "-o"
                && !a.contains('.')
                && !a.contains('/')
        });
        if let Some((_, name)) = bare_name {
            if !std::path::Path::new(name).exists() {
                fail_unknown_build_target(name);
            }
        }
    }
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

/// Report `fai build <name>` where `<name>` is neither a sub-project in
/// fai.toml nor an existing file/directory, then exit. Explains what went
/// wrong and shows how to define the target or pass a file instead.
fn fail_unknown_build_target(name: &str) -> ! {
    eprintln!(
        "error: '{}' is not a build target in fai.toml, and no file or directory named '{}' exists",
        name, name
    );
    eprintln!();
    let known: Vec<String> = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_project_root(&cwd))
        .map(|root| {
            let toml = std::fs::read_to_string(root.join("fai.toml")).unwrap_or_default();
            let info = parse_project_info(&toml);
            let mut names: Vec<String> = info.sub_projects.keys().cloned().collect();
            names.sort();
            names
        })
        .unwrap_or_default();
    if known.is_empty() {
        eprintln!("This project's fai.toml defines no named targets. To make");
        eprintln!("'{}' buildable as a target, add a section like:", name);
        eprintln!();
        eprintln!("  [project.{}]", name);
        eprintln!("  target = \"wasm-html\"        # or \"native\"");
        eprintln!("  main = \"main.fai\"");
        eprintln!("  build_dir = \"build/{}\"", name);
    } else {
        eprintln!("Available targets: {}", known.join(", "));
        eprintln!();
        eprintln!(
            "To add '{}' as a target, define a [project.{}] section in fai.toml.",
            name, name
        );
    }
    eprintln!();
    eprintln!("Alternatively, pass a source file or directory directly: fai build <file>.fai");
    std::process::exit(1);
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
    WorkspaceMember,
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
                std::env::set_current_dir(&member_dir).unwrap();
                return ProjectContext::WorkspaceMember;
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
fn run_project_check(project_root: &std::path::Path, src_dir: &str) -> Result<(), (String, usize)> {
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
    match checker.check_with_modules(&prepared.serde_ast.statements, &prepared_modules) {
        Ok(()) => Ok(()),
        Err(e) => Err((
            format_check_errors(&checker, &e),
            checker.collected_errors.len().max(1),
        )),
    }
}

fn check_single_file(path: &str, reporter: &Reporter) {
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
fn try_check_single_file(path: &str) -> Result<(), (String, usize)> {
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
    match run_checker(&mut checker, &prepared) {
        Ok(()) => Ok(()),
        Err(e) => Err((
            format_check_errors(&checker, &e),
            checker.collected_errors.len().max(1),
        )),
    }
}

fn step_test(args: &[String], reporter: &Reporter) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));

    if let Some(path) = file_arg {
        // Test a single file
        run_tests_file(path, reporter);
        return;
    }

    // Test the project's source tree. Skip if no standard project
    // found (e.g. workspace root).
    let (project_root, src_dir) = match find_project_source_from_cwd() {
        Some(r) => r,
        None => return,
    };
    let toml = std::fs::read_to_string(project_root.join("fai.toml")).unwrap_or_default();
    let info = parse_project_info(&toml);

    if !info.sub_projects.is_empty() {
        // Multi-target project: each `[project.<name>]` has its own
        // entry, target, and dependency wiring. Run them in turn so
        // every target gets its target-specific RPC dispatch and
        // available-import filtering. The shared `src/` tree means
        // many tests run more than once — that's intentional, since
        // platform-specific code (server vs web) compiles differently
        // for each target. Sort by name for deterministic order.
        let mut names: Vec<&String> = info.sub_projects.keys().collect();
        names.sort();
        for name in names {
            let sub = &info.sub_projects[name];
            let Some(main) = &sub.main else { continue };
            let main_path = project_root.join(main);
            if !main_path.exists() {
                continue;
            }
            println!("▶ testing target {}", name);
            run_tests_file(&main_path.to_string_lossy(), reporter);
        }
        return;
    }

    // Flat project (library or single-target app): load the source
    // root as one module and run every test in one wasm pass. Files
    // reference each other through normal module-mate visibility, so
    // extern blocks in `_ffi.fai`, private helpers, and public APIs
    // all resolve regardless of which file declares which.
    let src_path = project_root.join(&src_dir);
    run_tests_module(&src_path, reporter);
}

/// Run every test in a flat library/app source directory as one
/// module. Mirrors the tail of `run_tests_file` but uses
/// `prepare_module_directory_for_tests` so there's no notion of an
/// "entry file" — every `.fai` file in `src_path` contributes its
/// declarations and tests to the same module.
fn run_tests_module(src_path: &std::path::Path, reporter: &Reporter) {
    let src_path_str = src_path.to_string_lossy().to_string();
    let prepared = match fai_compiler::prepare_module_directory_for_tests(&src_path_str) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&e);
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
    let mut checker = fai_checker::Checker::new();
    // Checker errors are surfaced by the earlier `step_check` pass —
    // here we only need the UFCS / named-param maps to feed codegen.
    let _ = run_checker(&mut checker, &prepared);
    let info = fai_codegen_wasm::direct::CheckerInfo {
        ufcs_calls: checker.ufcs_calls.clone(),
        named_param_reorder: checker.named_param_reorder.clone(),
        expression_types: checker.expression_types.clone(),
        generic_type_args: checker.generic_type_args.clone(),
    };
    let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned_with_entry_file(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        None,
        true,
        Some(&src_path_str),
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error_line(&format_codegen_error(&e));
            reporter.step(StepStatus::Fail, "test", "compile error");
            std::process::exit(1);
        }
    };
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

    let externs = extract_externs_from_prepared(&prepared, Some(&src_path_str));
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs) {
        Ok(p) => p,
        Err(e) => {
            reporter.error_line(&format!("runtime error during test setup: {}", e));
            reporter.step(StepStatus::Fail, "test", "setup failed");
            std::process::exit(1);
        }
    };

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

/// Run tests for a single .fai file, exiting on failure.
/// Fails if any tests fail OR if any named public function lacks a test
/// block. Missing tests are counted as test failures (same `[fail] test`
/// outcome, same exit code) — the scaffold makes coverage mandatory.
/// Run all test suites against the wasm binary and emit compact
/// output: one summary line per suite, then a single "Failed tests"
/// section at the end with each failure's description, source line,
/// and error message.
///
/// Compact form (vs the older per-`it` output) is the difference
/// between ~3 lines per suite and ~1 — significant when an LLM
/// agent is iterating on the project. The Failed Tests section
/// gives the agent enough to grep for the failing case without
/// re-reading every passing case.
///
/// Returns `(passed, failed)` so the caller can build its own
/// step-status summary.
fn run_tests_with_compact_output(
    wasm_bytes: &[u8],
    tests: &[crate::test_meta::TestSuiteMeta],
    externs: Vec<wasm_runner::ExternInfo>,
) -> Result<(usize, usize), String> {
    let mut current_suite: Option<String> = None;
    let mut suite_pass: u32 = 0;
    let mut suite_fail: u32 = 0;
    let mut failures: Vec<(String, String, u32, String)> = Vec::new();

    let summary =
        wasm_runner::run_wasm_tests_with_externs(wasm_bytes, tests, externs, |outcome| {
            if current_suite.as_deref() != Some(&outcome.suite_name) {
                if let Some(name) = current_suite.take() {
                    println!("{}", format_suite_line(&name, suite_pass, suite_fail));
                }
                current_suite = Some(outcome.suite_name.clone());
                suite_pass = 0;
                suite_fail = 0;
            }
            match &outcome.error {
                None => suite_pass += 1,
                Some(msg) => {
                    suite_fail += 1;
                    failures.push((
                        outcome.suite_name.clone(),
                        outcome.case_desc.clone(),
                        outcome.suite_line,
                        msg.clone(),
                    ));
                }
            }
        })?;
    if let Some(name) = current_suite.take() {
        println!("{}", format_suite_line(&name, suite_pass, suite_fail));
    }
    if !failures.is_empty() {
        println!();
        println!("Failed tests:");
        for (suite, case, line, msg) in &failures {
            println!();
            println!("  ✗ {} — {}", suite, case);
            if *line > 0 {
                println!("    at line {}", line);
            }
            for line_text in msg.lines() {
                println!("    {}", line_text);
            }
        }
    }
    Ok((summary.passed, summary.failed))
}

/// One-line summary for a test suite: pass count, fail count, and a
/// ✓/✗ glyph. `(3 pass)` for clean suites, `(2 pass, 1 fail)` for
/// mixed, `(2 fail)` when nothing passed.
fn format_suite_line(suite: &str, pass: u32, fail: u32) -> String {
    let glyph = if fail == 0 { "✓" } else { "✗" };
    let counts = match (pass, fail) {
        (p, 0) => format!("{} pass", p),
        (0, f) => format!("{} fail", f),
        (p, f) => format!("{} pass, {} fail", p, f),
    };
    format!("  {} {} ({})", glyph, suite, counts)
}

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
    // and UFCS marks, but don't inject the full RPC dispatch (it adds many
    // functions that inflate VM register usage and may cause stack overflows).
    // Tests still compile every function body, including `main`, so server
    // entrypoints need a tiny private addRpcRoutes stub.
    let mut content = raw_content;
    inject_rpc_test_stub(&mut content);
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
    let wasm_bytes = match fai_codegen_wasm::codegen_direct_full_reasoned_with_entry_file(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        None,
        true,
        Some(path),
    ) {
        Ok(w) => w,
        Err(e) => {
            reporter.error_line(&format_codegen_error(&e));
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

    let externs = extract_externs_from_prepared(&prepared, source_root.as_deref());
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs) {
        Ok(p) => p,
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

fn inject_rpc_test_stub(content: &mut String) {
    if !content.contains("addRpcRoutes") || content.contains("def addRpcRoutes") {
        return;
    }
    content.push_str(
        "\nprivate:\n# Test-mode RPC route stub.\ndef addRpcRoutes\n    @param router Router\n    @return Void\ndo\nend\n",
    );
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

fn step_run(args: &[String], project: Option<&str>, reporter: &Reporter) {
    // Phase D: `--wasm` is no longer a toggle — wasm is the only run
    // path. Accept the flag for back-compat with scripts that pass it;
    // the explicit `use_wasm` binding is kept as `_` so the filter at
    // the top of `positional` still skips it.
    let _use_wasm = args.iter().any(|a| a == "--wasm");

    // Plan 116 phase 2: `--watchdog[=secs]` / `--debug` arm the hang
    // watchdog — if the program hasn't completed after the deadline,
    // the runner interrupts it and prints a post-mortem dump (async
    // task table + heap stats). Equals-form only: positional-arg
    // detection above treats a bare value after `--watchdog` as a
    // target name. `FAI_WATCHDOG=<secs>` works as an env fallback.
    let watchdog_secs = args
        .iter()
        .find_map(|a| {
            if a == "--watchdog" || a == "--debug" {
                Some(10)
            } else {
                a.strip_prefix("--watchdog=").and_then(|v| v.parse().ok())
            }
        })
        .or(wasm_runner::RunOptions::from_env().watchdog_secs);
    // Plan 116 phase 5: `--check-leaks[=interval:<ms>]` arms the heap
    // allocation ledger. The flag has a codegen half (rt_alloc/rt_free
    // emit `__fai_alloc_event`/`__fai_free_event`) and a runner half
    // (record events, print the itemized live set at exit/trap, or on
    // an interval for servers). `FAI_CHECK_LEAKS=1|interval:<ms>` is
    // the env fallback.
    let check_leaks = args
        .iter()
        .find_map(|a| {
            if a == "--check-leaks" {
                Some(wasm_runner::CheckLeaksOptions::default())
            } else {
                a.strip_prefix("--check-leaks=")
                    .map(|v| wasm_runner::CheckLeaksOptions {
                        interval_ms: v.strip_prefix("interval:").and_then(|n| n.parse().ok()),
                    })
            }
        })
        .or(wasm_runner::RunOptions::from_env().check_leaks);
    if check_leaks.is_some() {
        // Codegen gate — must be set before compile_fai_to_wasm below.
        fai_codegen_wasm::set_check_leaks(true);
    }
    let run_opts = wasm_runner::RunOptions {
        watchdog_secs,
        check_leaks,
    };

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
        if let Err(e) = wasm_runner::run_wasm_opts(&wasm_bytes, run_opts) {
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
    inject_rpc_dispatch(
        &mut content,
        &run_info,
        run_source_root.as_deref(),
        Some(&path),
    );

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
    let wasm_bytes = compile_fai_to_wasm(
        &content,
        &path,
        false,
        synthetic_modules.clone(),
        None,
        None,
    );
    let externs = extract_extern_info_full(&content, &path, synthetic_modules);
    if let Err(e) = wasm_runner::run_wasm_with_externs_opts(&wasm_bytes, externs, run_opts) {
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
    let push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
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
    let push_block = |block: &fai_compiler::ast::ExternBlockDeclaration,
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
                // Validate the named target exists before planning;
                // an unknown name should still print the targets list.
                if let Some(name) = project {
                    if !info.sub_projects.contains_key(name) {
                        eprintln!("error: unknown target '{}'. Available targets:", name);
                        for k in info.sub_projects.keys() {
                            eprintln!("  - {}", k);
                        }
                        std::process::exit(1);
                    }
                }
                let order = match plan_build_order(&info, project) {
                    Ok(o) => o,
                    Err(msg) => {
                        eprintln!("error: {}", msg);
                        std::process::exit(1);
                    }
                };
                for name in &order {
                    if let Some(sub) = info.sub_projects.get(name) {
                        build_one_subproject(name, sub, &root, &info);
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
            if ws_info.sub_projects.contains_key(first_arg) {
                // Plan the dep order so any `required_targets` build
                // before the requested one. Asset copy happens per
                // target inside `build_one_subproject`.
                let order = match plan_build_order(&ws_info, Some(first_arg)) {
                    Ok(o) => o,
                    Err(msg) => {
                        eprintln!("error: {}", msg);
                        std::process::exit(1);
                    }
                };
                for name in &order {
                    if let Some(sub) = ws_info.sub_projects.get(name) {
                        build_one_subproject(name, sub, &root, &ws_info);
                    }
                }
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
    inject_rpc_dispatch(&mut content, &info, source_root.as_deref(), Some(&path));

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

    // Find which sub-project's `main` matches this entry path so we
    // can read its `rpc_server` flag and remote-dependency URL. When
    // `rpc_server = false` (the default for client targets), every
    // `remote def` body is rewritten to call `remoteCall(...)` so the
    // client wasm never executes server-only code (the OOB on signup
    // in the browser was caused by the unrewritten `auth.signup`
    // dereferencing null SQLite handles). When `rpc_server = true` —
    // or when no remote URL is configured — the rewrite is skipped
    // and bodies stay intact.
    let active_sub = {
        let canonical_entry = std::fs::canonicalize(&path).ok();
        info.sub_projects.values().find(|sub| {
            sub.main
                .as_ref()
                .and_then(|m| {
                    let candidate = source_root
                        .as_deref()
                        .and_then(|sr| std::path::Path::new(sr).parent())
                        .map(|root| root.join(m))
                        .unwrap_or_else(|| std::path::PathBuf::from(m));
                    std::fs::canonicalize(&candidate).ok()
                })
                .zip(canonical_entry.clone())
                .map(|(sub_main, entry)| sub_main == entry)
                .unwrap_or(false)
        })
    };
    let project_root_for_hash = source_root
        .as_deref()
        .and_then(|sr| std::path::Path::new(sr).parent().map(|p| p.to_path_buf()));
    let rpc_proxy_substitution: Option<(String, String)> = match active_sub {
        Some(sub) if !sub.rpc_server => sub.remote_deps.iter().find_map(|(dep_name, envs)| {
            let cfg = envs.get("dev").or_else(|| envs.values().next())?;
            let hash = project_root_for_hash
                .as_ref()
                .and_then(|root| find_dependency_hash(root, dep_name, &info))
                .unwrap_or_default();
            Some((cfg.url.clone(), hash))
        }),
        _ => None,
    };

    // Plan 94 Phase G: for default (non-html) builds try the direct
    // AST→wasm path before falling back to the bytecode codegen.
    // `wasm-html` forces bytecode because the direct module
    // assembler doesn't honour target-filtered imports yet.
    let mut wasm_bytes = compile_fai_to_wasm(
        &content,
        &path,
        false,
        synthetic_modules.clone(),
        codegen_target,
        rpc_proxy_substitution
            .as_ref()
            .map(|(u, h)| (u.as_str(), h.as_str())),
    );

    // Embed FFI extern metadata into the wasm so a prebuilt `.wasm`
    // dispatched via `fai run path/to/x.wasm` can rehydrate the
    // `call_ffi` table without re-reading the original source. No-op
    // when the project has no `extern` blocks (byte-identical output).
    let externs = extract_extern_info_full(&content, &path, synthetic_modules);
    wasm_runner::externs_section::embed_externs(&mut wasm_bytes, &externs);

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
        out_dir
            .join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
    } else if let Some(bd) = build_dir_opt.as_deref() {
        // For non-html targets, honor [project].build_dir when set so a
        // hello-world starter declaring build_dir = "build" actually writes
        // there instead of dropping the wasm next to main.fai. When unset,
        // fall back to the historical "next to source" behavior.
        let project_root = source_root
            .as_deref()
            .and_then(|sr| std::path::Path::new(sr).parent())
            .unwrap_or_else(|| std::path::Path::new("."));
        let out_dir = project_root.join(bd);
        let _ = std::fs::create_dir_all(&out_dir);
        out_dir
            .join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
    } else {
        // No build_dir: keep the wasm next to the source file. Filename
        // still derives from the project name when set.
        let dir = std::path::Path::new(&path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        dir.join(artifact_filename(&info.name, &path))
            .to_str()
            .unwrap()
            .to_string()
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

    // Plan 116: extract the `fai-dbg` debug side-table (function index →
    // name/file/line) to `<out>.dbg.json` next to the wasm, so external
    // tools (browser harnesses, profilers) can map trap frames to source
    // without parsing the binary. The same data stays embedded in the
    // wasm for the native runner. Best-effort: a write failure is not a
    // build failure.
    if let Some(dbg_json) = extract_dbg_section(&wasm_bytes) {
        let dbg_path = format!("{}.dbg.json", output_path.trim_end_matches(".wasm"));
        let _ = std::fs::write(&dbg_path, dbg_json);
    }

    // Plan 101: If the target graph has remote functions/types, write
    // schema.json next to the build output so client builds can consume it.
    let mut wrote_schema = false;
    if let Ok(surface) =
        rpc_surface::collect_from_source(&content, source_root.as_deref(), Some(&path))
    {
        if !surface.is_empty() {
            let schema_dir = std::path::Path::new(&output_path)
                .parent()
                .unwrap_or(std::path::Path::new("."));
            let spec = surface.to_schema();
            let json = interface::spec_to_json(&spec);
            let schema_path = schema_dir.join("schema.json");
            if let Err(e) = std::fs::write(&schema_path, &json) {
                reporter.error_line(&format!("warning: could not write schema.json: {}", e));
            } else {
                wrote_schema = true;
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
    if wrote_schema {
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
        // `Name = "file://path"` or `Name = "https://..."` — peer name
        // is on the LHS; skip entries whose LHS doesn't match the peer
        // we're looking up.
        let Some(spec) = fai_compiler::dep_url::parse_dep_line(t) else {
            continue;
        };
        if spec.name != peer_name {
            continue;
        }
        let Ok(dep_root_buf) = fai_compiler::dep_url::resolve_dep_url(&spec.url, consumer_root)
        else {
            continue;
        };
        let path_str = dep_root_buf.to_string_lossy().into_owned();
        let path_str = path_str.as_str();
        let dep_root = dep_root_buf.as_path();

        // Confirm the dep's own [project] name matches before trusting it.
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

/// Format a compiler-AST `TypeNode` back into source for round-tripping
/// when generating proxy bodies. Mirrors `rpc_proxy::format_type_node`
/// but for the compiler-AST (the parser-AST version is in rpc_proxy.rs).
fn format_compiler_type_node(tn: &fai_compiler::ast::TypeNode) -> String {
    let mut s = tn.name.clone().unwrap_or_else(|| "Void".to_string());
    if tn.is_array {
        s.push_str("[]");
    }
    if tn.is_optional {
        s.push('?');
    }
    s
}

/// Generate proxy fai source for a single `remote def`. The output is a
/// complete fai program containing one function with the same signature
/// as `fd` but a body that calls `remoteCall(url, key, args, hash)`. The
/// caller parses this, converts to compiler-AST, and lifts the body
/// statements over the original to swap in the proxy implementation.
fn generate_remote_def_proxy_source(
    fd: &fai_compiler::ast::FunctionDeclaration,
    key: &str,
    url: &str,
    hash: &str,
) -> String {
    let mut out = String::new();
    out.push_str("use std.json\n\n");
    out.push_str("# Auto-generated client proxy for a `remote def`.\n");
    out.push_str(&format!("def {}\n", fd.name));
    for p in &fd.params {
        out.push_str(&format!(
            "    @param {} {}\n",
            p.name,
            format_compiler_type_node(&p.type_node)
        ));
    }
    for r in &fd.return_types {
        out.push_str(&format!(
            "    @return {}\n",
            format_compiler_type_node(&r.type_node)
        ));
    }
    out.push_str("do\n");
    if fd.params.is_empty() {
        out.push_str(&format!(
            "  remoteCall('{}', '{}', '[]', '{}')\n",
            url, key, hash
        ));
    } else {
        let parts: Vec<String> = fd
            .params
            .iter()
            .map(|p| format!("json.stringify({})", p.name))
            .collect();
        out.push_str(&format!(
            "  let __args = '[' + {} + ']'\n",
            parts.join(" + ',' + ")
        ));
        out.push_str(&format!(
            "  remoteCall('{}', '{}', __args, '{}')\n",
            url, key, hash
        ));
    }
    out.push_str("end\n");
    out
}

/// Rewrite each `remote def` body in `modules` to call `remoteCall(...)`
/// instead of running the original (server-side) body. Triggered for
/// client targets with `rpc_server = false` (the default) so that
/// browser/wasm builds never execute server-only code paths like SQLite
/// access — the OOB seen on the signup flow was caused by `auth.signup`'s
/// real body running in the browser and dereferencing null Connection
/// handles.
///
/// Tests in the regular `fai test` flow keep the original bodies so unit
/// tests that exercise data-layer functions natively still work — this
/// rewrite is bypassed when `is_test` is true.
fn rewrite_remote_def_bodies(
    modules: &mut [fai_compiler::module::DiscoveredModule],
    url: &str,
    hash: &str,
) {
    for module in modules.iter_mut() {
        let module_name = module.name.clone();
        let mut had_rewrite = false;
        for stmt in module.statements.iter_mut() {
            if let fai_compiler::ast::Statement::FunctionDeclaration(fd) = stmt {
                if !fd.is_remote {
                    continue;
                }
                let key = format!("{}.{}", module_name, fd.name);
                let proxy_src = generate_remote_def_proxy_source(fd, &key, url, hash);
                let parsed = match fai_parser::parse(&proxy_src) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("error: failed to parse rewritten proxy for {}: {}", key, e);
                        std::process::exit(1);
                    }
                };
                let serde = fai_compiler::native_bridge::convert_program(&parsed);
                let new_body = serde
                    .statements
                    .iter()
                    .find_map(|s| match s {
                        fai_compiler::ast::Statement::FunctionDeclaration(rfd)
                            if rfd.name == fd.name =>
                        {
                            Some(rfd.body.clone())
                        }
                        _ => None,
                    })
                    .expect("regenerated proxy must contain the named function");
                fd.body = new_body;
                // Mark non-remote so downstream code (RPC dispatch
                // generation, schema export) doesn't try to wire this
                // up server-side too — the body is now a client stub.
                fd.is_remote = false;
                had_rewrite = true;
            }
        }
        if had_rewrite {
            // The proxy body uses `json.stringify(...)` to serialise
            // arguments — make sure the module sees `std.json` even
            // when the original source didn't import it. Idempotent:
            // skip when the module already has `use std.json` (named
            // or namespace) at top level.
            let already_has_json = module.statements.iter().any(|s| {
                if let fai_compiler::ast::Statement::UseStatement(u) = s {
                    u.module_path == ["std".to_string(), "json".to_string()]
                } else {
                    false
                }
            });
            if !already_has_json {
                let zero = fai_compiler::ast::SourceLocation { line: 0, column: 0 };
                module.statements.insert(
                    0,
                    fai_compiler::ast::Statement::UseStatement(fai_compiler::ast::UseStatement {
                        module_path: vec!["std".to_string(), "json".to_string()],
                        imported_names: None,
                        import_all: false,
                        is_remote: false,
                        location: zero,
                    }),
                );
            }
        }
    }
}

/// Plan 101 Phase 4: Inject generated RPC dispatch for server targets.
/// The dispatch surface is every `remote def` reachable from the prepared
/// target graph, so endpoint modules can live in normal app folders.
fn inject_rpc_dispatch(
    content: &mut String,
    _info: &ProjectInfo,
    source_root: Option<&str>,
    entry_path: Option<&str>,
) {
    // Only inject dispatch if the source uses the RPC API.
    // Support both the new addRpcRoutes pattern and the legacy startRpcServer.
    let uses_rpc = content.contains("addRpcRoutes") || content.contains("startRpcServer");
    if !uses_rpc {
        return;
    }

    // Server targets expose every `remote def` reachable in their prepared
    // build graph. Endpoints can live in normal app modules (`data.tasks`,
    // `auth`, etc.); the generated dispatch imports them as needed.
    match rpc_surface::collect_from_source(content, source_root, entry_path) {
        Ok(surface) => {
            let dispatch_functions = surface.dispatch_functions();
            let dispatch =
                match rpc_dispatch::generate_dispatch_for_functions(&dispatch_functions, "") {
                    Ok(dispatch) => dispatch,
                    Err(e) => {
                        eprintln!("error: failed to generate addRpcRoutes: {}", e);
                        std::process::exit(1);
                    }
                };
            if !dispatch.trim().is_empty() {
                if is_verbose() {
                    eprintln!("    generated RPC dispatch (addRpcRoutes + handler + dispatch)");
                }
                content.push('\n');
                content.push_str(&dispatch);
            } else {
                // The server source calls addRpcRoutes/startRpcServer but has no
                // reachable 'remote def' functions for the dispatcher to route to.
                // Without this check, the build fails later with a cryptic
                // "unknown function 'addRpcRoutes'" — this tells agents exactly
                // what to add, and where.
                eprintln!(
                    "error: this server calls addRpcRoutes but no reachable 'remote def' functions were found"
                );
                eprintln!(
                    "       addRpcRoutes is auto-generated — and only generated when the server"
                );
                eprintln!("       target graph exposes at least one function. Mark each endpoint");
                eprintln!(
                    "       you want to expose as 'remote def' in any module imported by this target"
                );
                eprintln!("       (see `fai_examples rpc`).");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("  warning: failed to discover RPC surface: {}", e);
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
#[cfg(test)]
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
    /// `true` when this sub-project hosts the RPC endpoint — `remote def`
    /// bodies stay intact and are wired into the dispatcher. When `false`
    /// (the default for client targets), each `remote def` reachable in
    /// this build has its body rewritten to `remoteCall(url, name, args,
    /// hash)` so the client never executes server-only code (e.g. SQLite
    /// access) under wasm. The URL comes from the matching
    /// `[project.X.dependencies.<dep>.remote.<env>]` entry.
    rpc_server: bool,
    /// `[project.<name>] required_targets = [...]` — names of other
    /// sub-projects whose builds must complete before this one. The
    /// build planner resolves these into a topological order and runs
    /// them first (cycle = build error).
    required_targets: Vec<String>,
    /// `[project.<name>.assets]` — ordered (from, to) pairs copied
    /// into this target's `build_dir` after a successful build.
    /// `from` starting with `$` references another target's
    /// `build_dir` (e.g. `$web` → that target's output directory);
    /// otherwise it is project-root-relative. `to` is relative to this
    /// target's `build_dir` (empty string = copy into the build_dir
    /// itself). Order is preserved so later entries overwrite earlier
    /// ones in the same destination.
    assets: Vec<(String, String)>,
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
/// Pick the `.wasm` artifact filename for a build. The project's
/// `name` field wins; when it's the parser's default placeholder
/// (`"unknown"`) or empty, we fall back to the source file's stem so
/// ad-hoc `forai build foo.fai` runs against a loose file still produce
/// `foo.wasm`. The returned string includes the `.wasm` extension.
fn artifact_filename(project_name: &str, source_path: &str) -> String {
    let stem = if !project_name.is_empty() && project_name != "unknown" {
        project_name.to_string()
    } else {
        std::path::Path::new(source_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("main")
            .to_string()
    };
    format!("{}.wasm", stem)
}

/// Compute the `-o <path>` value for a sub-project build. Used by
/// both `cmd_build` paths (build-all and `-p <name>`). The artifact
/// is always named after the sub-project key (`web.wasm`,
/// `server.wasm`) — that name has to be passed explicitly via `-o`
/// because the recursive `cmd_build` call will re-parse the fai.toml
/// and otherwise pick up the top-level `[project].name`, which would
/// collide across sub-projects. Creates the output directory as a
/// side effect.
fn sub_project_output_path(
    sub: &SubProject,
    root: &std::path::Path,
    entry: &std::path::Path,
    sub_name: &str,
) -> String {
    let out_dir = match &sub.build_dir {
        Some(bd) => root.join(bd),
        None => entry
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| root.to_path_buf()),
    };
    let _ = std::fs::create_dir_all(&out_dir);
    out_dir
        .join(format!("{}.wasm", sub_name))
        .to_string_lossy()
        .into_owned()
}

/// Resolve the on-disk path of the `.wasm` artifact a sub-project
/// build produces. Used by `cmd_run` in project mode to skip the
/// in-memory compile and execute the just-built artifact directly.
/// Returns `None` when the project has no sub-projects, the named
/// target doesn't exist, or the artifact hasn't been built yet.
fn resolve_target_wasm_artifact(project: Option<&str>) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let root = find_project_root(&cwd)?;
    let toml = std::fs::read_to_string(root.join("fai.toml")).ok()?;
    let info = parse_project_info(&toml);
    if info.sub_projects.is_empty() {
        return None;
    }
    let name = match project {
        Some(n) => n.to_string(),
        None => {
            // No explicit target — only resolve when there's exactly
            // one sub-project. Multi-target with no `--project` is
            // ambiguous; the existing resolver handles that error.
            if info.sub_projects.len() == 1 {
                info.sub_projects.keys().next().cloned()?
            } else {
                return None;
            }
        }
    };
    let sub = info.sub_projects.get(&name)?;
    let dir = target_build_dir(&name, sub, &root)?;
    let path = dir.join(format!("{}.wasm", name));
    if path.exists() {
        Some(path.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Build a single sub-project: resolve its entry point, dispatch
/// through `cmd_build` (which runs the per-target fmt/check/test
/// pipeline + codegen), then copy `[project.<name>.assets]` into the
/// target's `build_dir`. Returns `true` on success, `false` when no
/// entry point could be resolved (the per-target message is printed
/// to stderr; the caller decides whether to continue).
///
/// Used by both `step_build` branches (build-all and build-one) and
/// by `cmd_run`'s build-then-run path. Keeping the build invocation
/// in one place ensures asset copies happen everywhere a build does.
fn build_one_subproject(
    name: &str,
    sub: &SubProject,
    root: &std::path::Path,
    info: &ProjectInfo,
) -> bool {
    let entry_opt = sub
        .main
        .as_ref()
        .map(|m| root.join(m))
        .filter(|p| p.is_file())
        .or_else(|| {
            sub.source
                .as_ref()
                .and_then(|src| resolve_entry_point_with_hint(root, src, Some(name)))
        });
    let Some(entry) = entry_opt else {
        eprintln!("  warning: target '{}' — no entry point found", name);
        return false;
    };
    eprintln!("\n▶ building target '{}' ({})", name, entry.display());
    let mut build_args = vec![entry.to_string_lossy().into_owned()];
    if matches!(sub.target, Some(BuildTarget::WasmHtml)) {
        build_args.push("--html".to_string());
    }
    build_args.push("-o".to_string());
    build_args.push(sub_project_output_path(sub, root, &entry, name));
    cmd_build(&build_args);
    copy_target_assets(name, sub, info, root);
    true
}

/// Resolve the absolute on-disk directory a target writes its build
/// artifacts to. Mirrors the rule used by `sub_project_output_path`:
/// `build_dir` from fai.toml when set, otherwise the directory of the
/// resolved entry file. Returns `None` when no entry can be resolved.
fn target_build_dir(
    name: &str,
    sub: &SubProject,
    root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Some(bd) = &sub.build_dir {
        return Some(root.join(bd));
    }
    let entry = sub
        .main
        .as_ref()
        .map(|m| root.join(m))
        .filter(|p| p.is_file())
        .or_else(|| {
            sub.source
                .as_ref()
                .and_then(|src| resolve_entry_point_with_hint(root, src, Some(name)))
        })?;
    entry.parent().map(|p| p.to_path_buf())
}

/// Plan the build order for a set of targets. When `requested` is
/// `Some(name)`, returns the transitive closure of `name` (including
/// `name` itself) in dependency-first topological order. When `None`,
/// returns every sub-project in topological order. Names that don't
/// exist in `sub_projects` are dropped from `required_targets`
/// references (with a warning) rather than failing the build — this
/// keeps the parser permissive about typos in non-essential deps.
/// Cycles produce `Err(message)`; the caller exits the build.
fn plan_build_order(info: &ProjectInfo, requested: Option<&str>) -> Result<Vec<String>, String> {
    use std::collections::{HashMap, HashSet};

    // Topological sort with cycle detection. `visiting` is the
    // current DFS stack; `visited` is the finished set. Output is
    // built in post-order so dependencies appear before dependents.
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();

    // Start set: either the named target or every sub-project. When
    // walking everything, sort the roots alphabetically for a stable
    // build order across runs. Sibling subtrees that don't depend on
    // each other otherwise build in declaration-hash order, which
    // would be flaky in tests.
    let roots: Vec<String> = match requested {
        Some(name) => vec![name.to_string()],
        None => {
            let mut names: Vec<String> = info.sub_projects.keys().cloned().collect();
            names.sort();
            names
        }
    };

    fn visit(
        name: &str,
        sub_projects: &HashMap<String, SubProject>,
        visiting: &mut HashSet<String>,
        visited: &mut HashSet<String>,
        order: &mut Vec<String>,
        path: &mut Vec<String>,
    ) -> Result<(), String> {
        if visited.contains(name) {
            return Ok(());
        }
        if visiting.contains(name) {
            // Reconstruct the cycle for the error message starting
            // from where `name` first appears in `path`.
            let cycle_start = path.iter().position(|n| n == name).unwrap_or(0);
            let mut cycle: Vec<String> = path[cycle_start..].to_vec();
            cycle.push(name.to_string());
            return Err(format!("required_targets cycle: {}", cycle.join(" -> ")));
        }
        let Some(sub) = sub_projects.get(name) else {
            // The requested name has no sub-project; let downstream
            // build-resolution surface a clearer error than this
            // planner can. Drop silently.
            return Ok(());
        };
        visiting.insert(name.to_string());
        path.push(name.to_string());
        for dep in &sub.required_targets {
            if !sub_projects.contains_key(dep) {
                eprintln!(
                    "  warning: target '{}' lists required_target '{}' which is not declared in fai.toml — skipping",
                    name, dep
                );
                continue;
            }
            visit(dep, sub_projects, visiting, visited, order, path)?;
        }
        path.pop();
        visiting.remove(name);
        visited.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }

    let mut path: Vec<String> = Vec::new();
    for root in &roots {
        visit(
            root,
            &info.sub_projects,
            &mut visiting,
            &mut visited,
            &mut order,
            &mut path,
        )?;
    }
    Ok(order)
}

/// Recursive directory copy that merges into existing destinations
/// rather than replacing them. Files at the same relative path
/// overwrite. Used by `copy_target_assets` to layer multiple `assets`
/// entries that target the same directory (e.g. a generated client
/// bundle plus a project's authored `public/`).
fn copy_dir_merge(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    if src.is_file() {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_merge(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Copy every `[project.<name>.assets]` entry into the target's
/// `build_dir`. Sources beginning with `$` reference another target's
/// `build_dir`; everything else is project-root relative. Destinations
/// are relative to this target's `build_dir` (empty string = the
/// build_dir itself). Errors print to stderr but don't fail the build —
/// the build artifact is already on disk and a missing optional asset
/// shouldn't take it down.
fn copy_target_assets(name: &str, sub: &SubProject, info: &ProjectInfo, root: &std::path::Path) {
    if sub.assets.is_empty() {
        return;
    }
    let Some(target_dir) = target_build_dir(name, sub, root) else {
        eprintln!(
            "  warning: target '{}' has assets but no resolvable build_dir — skipping copy",
            name
        );
        return;
    };
    for (from, to) in &sub.assets {
        let src_path: std::path::PathBuf = if let Some(target_ref) = from.strip_prefix('$') {
            match info
                .sub_projects
                .get(target_ref)
                .and_then(|s| target_build_dir(target_ref, s, root))
            {
                Some(p) => p,
                None => {
                    eprintln!(
                        "  warning: assets source '{}' for target '{}' references unknown target — skipping",
                        from, name
                    );
                    continue;
                }
            }
        } else {
            root.join(from)
        };
        let dst_path = if to.is_empty() {
            target_dir.clone()
        } else {
            target_dir.join(to)
        };
        if !src_path.exists() {
            eprintln!(
                "  warning: assets source '{}' for target '{}' does not exist at {} — skipping",
                from,
                name,
                src_path.display()
            );
            continue;
        }
        if let Err(e) = copy_dir_merge(&src_path, &dst_path) {
            eprintln!(
                "  warning: copying assets '{}' -> '{}' for target '{}' failed: {}",
                from, to, name, e
            );
        } else {
            eprintln!(
                "  copied assets {} -> {}",
                from,
                dst_path.strip_prefix(root).unwrap_or(&dst_path).display()
            );
        }
    }
}

/// Parse a single-line TOML string-array literal like `["a", "b"]`.
/// Returns an empty vec for any input that doesn't open with `[` and
/// close with `]`. Tolerant of whitespace and trailing commas.
/// Multi-line arrays are not supported by this hand-rolled TOML pass —
/// keep `required_targets` to one line.
fn parse_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let inner = match trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(s) => s,
        None => return Vec::new(),
    };
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

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
                    "rpc_server" => sub.rpc_server = v_unquoted == "true",
                    "required_targets" => {
                        sub.required_targets = parse_string_array(v);
                    }
                    _ => {}
                }
            } else if parts.len() == 2 && parts[1] == "assets" {
                // [project.client.assets] — ordered "from" = "to" pairs.
                // The key may be quoted (e.g. `"$web" = "public"`) so
                // strip surrounding quotes from both sides.
                let from = k.trim_matches('"').to_string();
                sub.assets.push((from, v_unquoted));
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

#[cfg(test)]
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
  sleep_ms() {{ throw new Error('FAI legacy sleep_ms is disabled; sleep() must lower through the async scheduler'); }},
  host_set_timer(taskId, ms) {{  setTimeout(() => {{ if (instance?.exports.__fai_resume_task) instance.exports.__fai_resume_task(taskId); pumpAsync(); }}, Math.max(0, ms | 0)); }},
  call_ffi() {{ return 0x7FFC000100000000n; }},
  run_all() {{ throw new Error('FAI legacy run_all is disabled; all() must lower through the async scheduler'); }},
  spawn(closureVal) {{var cv=closureVal;setTimeout(function(){{var n=BigInt(cv);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return;var tag=dv.getInt32(a,true);if(tag!==4)return;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}if(typeof faiServiceScheduler==='function')faiServiceScheduler()}},0);return 0x7FFC000200000000n}},
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
  storage_get_str(){{return 0x7FFD000000000000n}},
  file_read_str(){{return 0x7FFD000000000000n}},
  storage_set(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear(){{try{{window.localStorage.clear()}}catch(e){{}}}},
  env_get(){{return 0x7FFD000000000000n}},
  env_load(){{return 0}},
  event_on(){{return 0x7FFD000000000000n}},
  event_once(){{return 0x7FFD000000000000n}},
  event_off(){{return 0}},
  event_emit(){{}},
  event_subscribers(){{return 0}},
  event_clear(){{}},
  event_clear_all(){{}},
  event_emit_deferred(){{}},
  event_drain(){{}},
  event_queue_len(){{return 0}},
  __fai_set_trap_msg(p,l){{console.error('FAI trap:',readStr(p,l))}},
  __fai_trap_report(code,a,b){{console.error('FAI trap report',{{code,a,b}})}},
  __fai_alloc_event(){{}},
  __fai_free_event(){{}}
}};
let instance;
let asyncRootDone = false;
function rootResultText(result) {{
  const n = BigInt(result);
  if ((n & 0x7FFC000400000000n) === 0x7FFC000400000000n) return String(Number(BigInt.asIntN(32, n & 0xFFFFFFFFn)));
  return '';
}}
function publishRootResult(result) {{
  window.__FAI_ROOT_RESULT_TEXT = rootResultText(result);
  window.__FAI_ROOT_FINISHED_AT = performance.now();
  window.__FAI_ROOT_DONE = true;
}}
function pumpAsync() {{
  if (!instance || !instance.exports.__fai_poll || asyncRootDone) return 0;
  const status = instance.exports.__fai_poll();
  if (status === 2) {{
    asyncRootDone = true;
    if (instance.exports.__fai_task_result) publishRootResult(instance.exports.__fai_task_result(1));
  }} else if (status === 3) {{
    asyncRootDone = true;
    window.__FAI_ROOT_FINISHED_AT = performance.now();
    window.__FAI_ROOT_DONE = true;
    console.error('FAI async task failed', instance.exports.__fai_task_result ? instance.exports.__fai_task_result(1) : null);
  }}
  return status;
}}
function startFai() {{
  window.__FAI_ROOT_DONE = false;
  window.__FAI_ROOT_RESULT_TEXT = '';
  window.__FAI_ROOT_STARTED_AT = performance.now();
  window.__FAI_ROOT_FINISHED_AT = undefined;
  if (instance.exports._start_async) {{
    asyncRootDone = false;
    instance.exports._start_async();
    pumpAsync();
  }} else {{
    publishRootResult(instance.exports._start());
  }}
}}
WebAssembly.instantiateStreaming(fetch('/{}'), {{ env }}).then(result => {{
  instance = result.instance;
  startFai();
}});
</script>
</body>
</html>"#,
        wasm_filename
    )
}

#[cfg(test)]
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
  if(Array.isArray(v)){{const dv=new DataView(instance.exports.memory.buffer);const base=instance.exports.__heap_ptr.value;const logsz=8+v.length*8;const addr=base+8;const end=(base+8+logsz+7)&~7;instance.exports.__heap_ptr.value=end;dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);const items=v.map(i=>jsToWasm(i));const m2=instance.exports.memory.buffer;for(let i=0;i<items.length;i++){{const bi=new BigInt64Array(m2,addr+8+i*8,1);bi[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{const keys=Object.keys(v);const base=instance.exports.__heap_ptr.value;const cap=Math.max(keys.length,16);const logsz=8+cap*16;const addr=base+8;const dv=new DataView(instance.exports.memory.buffer);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);instance.exports.__heap_ptr.value=(base+8+logsz+7)&~7;for(let i=0;i<keys.length;i++){{const kv=writeStrToWasm(keys[i]);const vv=jsToWasm(v[keys[i]]);const ea=addr+8+i*16;const m2=instance.exports.memory.buffer;const bi=new BigInt64Array(m2,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
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
    if(tag===1||tag===2){{const cnt=dv.getInt32(a+4,true);const r=[];for(let i=0;i<cnt;i++){{const bi=new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1);r.push(wasmToJs(bi[0]))}}return r}}
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
function writeStrToWasm(s){{const b=new TextEncoder().encode(s);const base=instance.exports.__heap_ptr.value;const logsz=8+b.length;const h=base+8;const m=new Uint8Array(instance.exports.memory.buffer);const d=new DataView(instance.exports.memory.buffer);d.setInt32(base,1,true);d.setInt32(base+4,logsz,true);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{const n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{const a=Number(n&0x0000FFFFFFFFFFFFn);const d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{const l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function invokeExport(name,...args){{const fn=instance.exports[name];if(!fn){{console.warn('FAI invokeExport missing export', name);return;}}debugLog('FAI invokeExport:start', {{name,args}});try{{const result=fn(...args);debugLog('FAI invokeExport:end', {{name,result}});return result;}}catch(e){{console.error('FAI invokeExport:failed', {{name,args,error:e}});throw e;}}}}
let asyncRootDone=false;
function rootResultText(result){{const v=wasmToJs(result);if(Array.isArray(v))return JSON.stringify(v);if(v===null||v===undefined)return'';return String(v);}}
function publishRootResult(result){{window.__FAI_ROOT_RESULT_TEXT=rootResultText(result);window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;const s=readNanBoxedStr(result);if(s&&s.startsWith('{{'))state=s;}}
function pumpAsync(){{if(!instance||!instance.exports.__fai_poll||asyncRootDone)return 0;const status=invokeExport('__fai_poll');if(status===2){{asyncRootDone=true;if(instance.exports.__fai_task_result)publishRootResult(invokeExport('__fai_task_result',1));}}else if(status===3){{asyncRootDone=true;window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;console.error('FAI async task failed',instance.exports.__fai_task_result?invokeExport('__fai_task_result',1):null)}}return status;}}
function startFai(){{window.__FAI_ROOT_DONE=false;window.__FAI_ROOT_RESULT_TEXT='';window.__FAI_ROOT_STARTED_AT=performance.now();window.__FAI_ROOT_FINISHED_AT=undefined;if(instance.exports._start_async){{asyncRootDone=false;invokeExport('_start_async');pumpAsync();}}else{{publishRootResult(invokeExport('_start'));}}}}
function callWasm(name,arg){{const fn=instance.exports[name];if(!fn){{console.warn('FAI callWasm missing export', name);return;}}console.log('FAI callWasm', {{name,arg:arg||''}});const ptr=writeStrToWasm(arg||'');const result=invokeExport(name,ptr);return readNanBoxedStr(result)}}
function rerender(stateArg){{debugLog('FAI rerender', {{stateArg:stateArg||''}});if(instance.exports.render){{callWasm('render',stateArg||'')}}else if(instance&&instance.exports&&(instance.exports._start_async||instance.exports._start)){{startFai();}}else{{console.warn('FAI rerender missing render and _start')}}}}
function wireEvents(){{debugLog('FAI wireEvents');document.querySelectorAll('[data-fai-click]').forEach(el=>{{const h=el.getAttribute('data-fai-click');el.onclick=()=>{{console.log('FAI click', h);callWasm(h);rerender('')}}}});document.querySelectorAll('[data-fai-input]').forEach(el=>{{const h=el.getAttribute('data-fai-input');el.oninput=()=>{{const d=JSON.stringify({{_state:JSON.parse(state||'{{}}'),_value:el.value}});console.log('FAI input', {{handler:h,value:el.value}});state=callWasm(h,d);rerender(state)}}}})}}
function handleEvent(id){{const fn=instance.exports.invokeHandler;if(!fn){{console.warn('FAI handleEvent: invokeHandler not exported');return;}}const boxed=BigInt(id)|0x7FFC000400000000n;debugLog('FAI handleEvent',{{id}});try{{fn(boxed)}}catch(e){{console.error('FAI handleEvent failed',{{id,error:e}})}}}}
function handleInputEvent(id,value){{const fn=instance.exports.invokeChangeHandler;if(!fn){{console.warn('FAI handleInputEvent: invokeChangeHandler not exported');return;}}const boxedId=BigInt(id)|0x7FFC000400000000n;const boxedStr=writeStrToWasm(value);debugLog('FAI handleInputEvent',{{id,value}});try{{fn(boxedId,boxedStr)}}catch(e){{console.error('FAI handleInputEvent failed',{{id,error:e}})}}}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}while(op.childNodes.length>nc.length)op.removeChild(op.lastChild)}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
const env={{
  print(p,l){{const text=readStr(p,l);debugLog('FAI print', text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file(){{return -1}},write_file(){{return -1}},now_ms(){{return Date.now()}},random(){{return Math.random()}},sleep_ms(){{throw new Error('FAI legacy sleep_ms is disabled; sleep() must lower through the async scheduler')}},host_set_timer(taskId,ms){{setTimeout(function(){{if(instance&&instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);pumpAsync()}},Math.max(0,ms|0))}},
  call_ffi(){{return 0x7FFC000100000000n}},run_all(){{throw new Error('FAI legacy run_all is disabled; all() must lower through the async scheduler')}},
  spawn(closureVal){{var cv=closureVal;setTimeout(function(){{var n=BigInt(cv);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return;var tag=dv.getInt32(a,true);if(tag!==4)return;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}if(typeof faiServiceScheduler==='function')faiServiceScheduler()}},0);return 0x7FFC000200000000n}},
  http_post(a,b,c,d,e){{try{{const x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html(p,l){{const html=readStr(p,l);console.log('FAI set_html', {{length:l}});debugLog('FAI set_html:preview', html.slice(0,240));morphDom(app,html,false);wireEvents()}},
  set_html_at(a,b,p,l){{const selector=readStr(a,b);const html=readStr(p,l);let root=document.querySelector(selector);if(!root&&selector.startsWith('#')){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root);}}if(!root){{console.error('FAI set_html_at missing root', selector);return;}}console.log('FAI set_html_at', {{selector,length:l}});debugLog('FAI set_html_at:preview', {{selector,html:html.slice(0,240)}});morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse(p,l){{try{{const s=readStr(p,l);const v=JSON.parse(s);return jsToWasm(v)}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify(v){{try{{const j=wasmToJs(v);return writeStrToWasm(JSON.stringify(j))}}catch(e){{return writeStrToWasm('null')}}}},
  remote_call(a,b,c,d,e,f,g,h){{const u=readStr(a,b),fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);const body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});console.log('FAI remote_call request', {{url:u,fn:fn_name,args:ar,hash:ha}});try{{const x=new XMLHttpRequest();x.open('POST',u.replace(/\/+$/,'')+'/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body);const resp=JSON.parse(x.responseText);console.log('FAI remote_call response', {{fn:fn_name,ok:resp.ok,value:resp.value,error:resp.error}});if(resp.ok)return jsToWasm(resp.value);console.warn('FAI remote_call returned error', resp);return NULL_VAL}}catch(e){{console.error('FAI remote_call failed', e);return NULL_VAL}}}},
  float_to_str(v,p){{const s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);const b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  storage_get(kp,kl,bp){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return -1;const b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_get_str(kp,kl){{try{{const k=readStr(kp,kl);const v=window.localStorage.getItem(k);if(v===null)return NULL_VAL;return writeStrToWasm(v)}}catch(e){{return NULL_VAL}}}},
  file_read_str(){{return NULL_VAL}},
  storage_set(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear(){{try{{window.localStorage.clear()}}catch(e){{}}}},
  env_get(){{return NULL_VAL}},
  env_load(){{return 0}},
  event_on(){{return NULL_VAL}},
  event_once(){{return NULL_VAL}},
  event_off(){{return 0}},
  event_emit(){{}},
  event_subscribers(){{return 0}},
  event_clear(){{}},
  event_clear_all(){{}},
  event_emit_deferred(){{}},
  event_drain(){{}},
  event_queue_len(){{return 0}},
  __fai_set_trap_msg(p,l){{console.error('FAI trap:',readStr(p,l))}},
  __fai_trap_report(code,a,b){{console.error('FAI trap report',{{code,a,b}})}},
  __fai_alloc_event(){{}},
  __fai_free_event(){{}}
}};
fetch('/{}').then(r=>r.arrayBuffer()).then(b=>WebAssembly.instantiate(b,{{env}})).then(r=>{{
  instance=r.instance;debugLog('FAI wasm instantiated', Object.keys(instance.exports));startFai();
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
.fai-vstack{display:flex;flex-direction:column;align-items:center;gap:12px;width:100%}
.fai-hstack{display:flex;flex-direction:row;align-items:center;gap:8px;width:100%}
.fai-zstack{position:relative;display:grid}
.fai-zstack>*{grid-area:1/1}
.fai-scrollview{overflow:auto}
.fai-spacer{flex:1}
.fai-view{}

/* ── Typography ─────────────────────────────────────────────── */
.fai-label{line-height:1.4}
.fai-paragraph{display:block;align-self:stretch;line-height:1.6;text-align:left}
.fai-heading{display:block;align-self:stretch;line-height:1.15;font-weight:700;text-align:left}

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

/// Pull the raw `fai-dbg` custom-section payload (JSON) out of a wasm
/// binary, if present. See `fai-codegen-wasm/src/debug_info.rs` for the
/// shape. Returns `None` for pre-plan-116 binaries.
fn extract_dbg_section(wasm: &[u8]) -> Option<Vec<u8>> {
    use wasmparser::{Parser, Payload};
    for payload in Parser::new(0).parse_all(wasm) {
        if let Ok(Payload::CustomSection(reader)) = payload {
            if reader.name() == fai_codegen_wasm::debug_info::DBG_SECTION_NAME {
                return Some(reader.data().to_vec());
            }
        }
    }
    None
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
  if(Array.isArray(v)){{var base=instance.exports.__heap_ptr.value,logsz=8+v.length*8,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,1,true);dv.setInt32(addr+4,v.length,true);faiLeakAlloc(addr,logsz,true);var items=v.map(function(i){{return jsToWasm(i)}});m=instance.exports.memory.buffer;for(var i=0;i<items.length;i++){{new BigInt64Array(m,addr+8+i*8,1)[0]=items[i]}}return OBJ_MASK|BigInt(addr)}}
  if(typeof v==='object'){{var keys=Object.keys(v),base=instance.exports.__heap_ptr.value,cap=Math.max(keys.length,16),logsz=8+cap*16,addr=base+8,end=(base+8+logsz+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(base,1,true);dv.setInt32(base+4,logsz,true);dv.setInt32(addr,3,true);dv.setInt32(addr+4,keys.length,true);faiLeakAlloc(addr,logsz,true);for(var i=0;i<keys.length;i++){{var kv=writeStrToWasm(keys[i]),vv=jsToWasm(v[keys[i]]),ea=addr+8+i*16;m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,ea,2);bi[0]=kv;bi[1]=vv}}return OBJ_MASK|BigInt(addr)}}
  return QNAN|TAG_NULL;
}}
function wasmToJs(v){{
  // See matching comment in generate_runtime_js above — INT_MASK
  // aliases QNAN due to a tag-bit overlap, so object/bool/null
  // checks must come before the Int fallback.
  var n=BigInt(v);if(n===NULL_VAL)return null;
  if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn);var dv=new DataView(instance.exports.memory.buffer);var tag=dv.getInt32(a,true);
    if(tag===0){{var l=dv.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}
    if(tag===1||tag===2){{var cnt=dv.getInt32(a+4,true),r=[];for(var i=0;i<cnt;i++){{r.push(wasmToJs(new BigInt64Array(instance.exports.memory.buffer,a+8+i*8,1)[0]))}}return r}}
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
function writeStrToWasm(s){{var b=new TextEncoder().encode(s),base=instance.exports.__heap_ptr.value,logsz=8+b.length,h=base+8;wasmGrow(base+8+logsz+8);var m=new Uint8Array(instance.exports.memory.buffer),d=new DataView(instance.exports.memory.buffer);d.setInt32(base,1,true);d.setInt32(base+4,logsz,true);d.setInt32(h,0,true);d.setInt32(h+4,b.length,true);m.set(b,h+8);instance.exports.__heap_ptr.value=(h+8+b.length+7)&~7;faiLeakAlloc(h,logsz,true);return OBJ_MASK|BigInt(h)}}
function readNanBoxedStr(v){{var n=BigInt(v);if((n&OBJ_MASK)===OBJ_MASK){{var a=Number(n&0x0000FFFFFFFFFFFFn),d=new DataView(instance.exports.memory.buffer);if(d.getInt32(a,true)===0){{var l=d.getInt32(a+4,true);return new TextDecoder().decode(new Uint8Array(instance.exports.memory.buffer,a+8,l))}}}}return''}}
function invokeExport(name){{var fn=instance.exports[name];if(!fn)return;var args=Array.prototype.slice.call(arguments,1);try{{return fn.apply(null,args)}}catch(e){{console.error('FAI',name,'failed',e);throw e}}}}
function rootResultText(result){{var v=wasmToJs(result);if(Array.isArray(v))return JSON.stringify(v);if(v===null||v===undefined)return'';return String(v)}}
function publishRootResult(result){{window.__FAI_ROOT_RESULT_TEXT=rootResultText(result);window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true}}
var asyncRootDone=false;
function pumpAsync(){{if(!instance||!instance.exports.__fai_poll)return 0;var status=invokeExport('__fai_poll');if(!asyncRootDone){{if(status===2){{asyncRootDone=true;if(instance.exports.__fai_task_result)publishRootResult(invokeExport('__fai_task_result',1));}}else if(status===3){{asyncRootDone=true;window.__FAI_ROOT_FINISHED_AT=performance.now();window.__FAI_ROOT_DONE=true;console.error('FAI async task failed',instance.exports.__fai_task_result?invokeExport('__fai_task_result',1):null)}}}}return status}}
function startFai(){{window.__FAI_ROOT_DONE=false;window.__FAI_ROOT_RESULT_TEXT='';window.__FAI_ROOT_STARTED_AT=performance.now();window.__FAI_ROOT_FINISHED_AT=undefined;if(instance.exports._start_async){{asyncRootDone=false;invokeExport('_start_async');pumpAsync()}}else publishRootResult(invokeExport('_start'))}}
function responseHeaders(xhr){{var headers={{}};String(xhr.getAllResponseHeaders()||'').trim().split(/[\r\n]+/).forEach(function(line){{if(!line)return;var i=line.indexOf(':');if(i>0)headers[line.slice(0,i).toLowerCase()]=line.slice(i+1).trim()}});return headers}}
function httpRequest(method,url,body){{try{{var x=new XMLHttpRequest();x.open(method,url,false);if(body!==undefined)x.setRequestHeader('Content-Type','text/plain; charset=utf-8');x.send(body===undefined?null:body);return jsToWasm({{status:x.status,body:x.responseText||'',headers:responseHeaders(x)}})}}catch(e){{console.error('FAI http request failed',e);return NULL_VAL}}}}
var faiEventRegistry={{byName:Object.create(null),nextId:0,queue:[],draining:false}};
var __faiRpcResults={{}};
// Heap allocation ledger (plan 116 phase 5, `--check-leaks`). Armed by the
// first __fai_alloc_event from a check-leaks build (or ?fai_check_leaks=1).
// Tier 1 only in the browser: live set grouped by size, dumped on demand
// from DevTools/Playwright via window.__fai_dump_leaks().
var faiLeak={{on:new URLSearchParams(location.search).get('fai_check_leaks')==='1',map:new Map(),hostAllocs:0,guestEvents:0,unknownFrees:0,bytes:0}};
function faiLeakAlloc(addr,size,host){{if(!host&&!faiLeak.on)faiLeak.on=true;if(!faiLeak.on)return;if(host)faiLeak.hostAllocs++;else faiLeak.guestEvents++;var old=faiLeak.map.get(addr);if(old!==undefined)faiLeak.bytes-=old;faiLeak.map.set(addr,size);faiLeak.bytes+=size}}
function faiLeakFree(addr,size){{if(!faiLeak.on)return;faiLeak.guestEvents++;var s=faiLeak.map.get(addr);if(s===undefined){{faiLeak.unknownFrees++}}else{{faiLeak.map.delete(addr);faiLeak.bytes-=s}}}}
window.__fai_dump_leaks=function(){{var by={{}};faiLeak.map.forEach(function(size){{by[size]=(by[size]||0)+1}});var rows=Object.keys(by).map(function(s){{return{{size:+s,count:by[s]}}}}).sort(function(a,b){{return b.size*b.count-a.size*a.count}});var live=instance&&instance.exports.__live_objects?instance.exports.__live_objects.value:null;var out='[check-leaks] live heap: '+faiLeak.map.size+' objects, '+faiLeak.bytes+' bytes ('+faiLeak.hostAllocs+' host-side, '+faiLeak.unknownFrees+' unknown frees'+(live===null?'':', __live_objects='+live)+')';if(faiLeak.guestEvents===0)out+='\n  no guest events — module not built with --check-leaks';rows.slice(0,40).forEach(function(r){{out+='\n  '+r.count+' × '+r.size+'B = '+(r.count*r.size)+'B'}});console.log(out);return out}};
function faiBuildEvent(name,dataVal){{var addr=instance.exports.__heap_ptr.value,cap=16,end=(addr+8+cap*16+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(addr,3,true);dv.setInt32(addr+4,2,true);var kn=writeStrToWasm('name'),vn=writeStrToWasm(name),kd=writeStrToWasm('data');m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,addr+8,4);bi[0]=kn;bi[1]=vn;bi[2]=kd;bi[3]=BigInt.asIntN(64,BigInt(dataVal));return OBJ_MASK|BigInt(addr)}}
function faiBuildSubscription(id,name){{var addr=instance.exports.__heap_ptr.value,cap=16,end=(addr+8+cap*16+7)&~7;wasmGrow(end+8);instance.exports.__heap_ptr.value=end;var m=instance.exports.memory.buffer,dv=new DataView(m);dv.setInt32(addr,3,true);dv.setInt32(addr+4,2,true);var ki=writeStrToWasm('id'),kn=writeStrToWasm('name'),vn=writeStrToWasm(name),iv=INT_MASK|BigInt.asUintN(32,BigInt(id));m=instance.exports.memory.buffer;var bi=new BigInt64Array(m,addr+8,4);bi[0]=ki;bi[1]=iv;bi[2]=kn;bi[3]=vn;return OBJ_MASK|BigInt(addr)}}
function faiReadSubscription(subVal){{var n=BigInt.asIntN(64,BigInt(subVal));var u=BigInt.asUintN(64,n);if((u&OBJ_MASK)!==OBJ_MASK)return null;var a=Number(u&0x0000FFFFFFFFFFFFn),dv=new DataView(instance.exports.memory.buffer);if(dv.getInt32(a,true)!==3)return null;var cnt=dv.getInt32(a+4,true),id=null,name=null;for(var i=0;i<cnt;i++){{var ea=a+8+i*16,bi=new BigInt64Array(instance.exports.memory.buffer,ea,2),k=readNanBoxedStr(bi[0]),v=BigInt.asUintN(64,bi[1]);if(k==='id')id=Number(BigInt.asIntN(32,v&0xFFFFFFFFn));else if(k==='name')name=readNanBoxedStr(bi[1])}}if(id===null||name===null)return null;return{{id:id,name:name}}}}
// Single-flight scheduler turn. Every async wakeup — an event closure, an RPC
// completion, a timer — funnels through here. A closure queued while a turn is
// already running (e.g. a signal-change `rerender()` emitted from inside a
// running handler) is appended and drained by that same turn, never started as a
// nested `__fai_drive_closure`/`__fai_poll` (a re-entrant poll reassigns
// `g_current` and corrupts the task table). Drives queued closures, then pumps
// the scheduler; repeats while the pump's resumed tasks queue more closures.
var __faiInScheduler=false,__faiClosureQueue=[];
function faiServiceScheduler(){{if(__faiInScheduler)return;__faiInScheduler=true;try{{var guard=0;do{{while(__faiClosureQueue.length){{var q=__faiClosureQueue.shift();try{{instance.exports.__fai_drive_closure(q[0],q[1])}}catch(e){{console.error('FAI async closure failed',e)}}}}pumpAsync()}}while(__faiClosureQueue.length&&guard++<100000)}}finally{{__faiInScheduler=false}}}}
function faiInvokeClosure(closureVal,arg){{var u=BigInt.asUintN(64,BigInt(closureVal));if((u&OBJ_MASK)!==OBJ_MASK)return NULL_VAL;var a=Number(u&0x0000FFFFFFFFFFFFn);if(a+16>instance.exports.memory.buffer.byteLength)return NULL_VAL;var dv=new DataView(instance.exports.memory.buffer);if(dv.getInt32(a,true)!==4)return NULL_VAL;var tidx=dv.getInt32(a+4,true),frameSize=dv.getInt32(a+12,true),envAddr=a+16;if(frameSize>0&&instance.exports.__fai_drive_closure){{__faiClosureQueue.push([BigInt.asIntN(64,BigInt(closureVal)),BigInt.asIntN(64,BigInt(arg))]);faiServiceScheduler();return NULL_VAL}}if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(!tbl)return NULL_VAL;try{{return tbl.get(tidx)(BigInt.asIntN(64,BigInt(arg)))}}catch(e){{console.error('FAI event closure failed',e);return NULL_VAL}}}}
function faiEventEmit(name,dataVal){{var list=faiEventRegistry.byName[name];if(!list||list.length===0)return;var snap=list.slice();faiEventRegistry.byName[name]=list.filter(function(s){{return !s.once}});var ev=faiBuildEvent(name,dataVal);for(var i=0;i<snap.length;i++)faiInvokeClosure(snap[i].closureVal,ev)}}
function handleEvent(id){{faiEventEmit('view:click',jsToWasm({{id:id}}))}}
function handleInputEvent(id,value){{faiEventEmit('view:input',jsToWasm({{id:id,value:value}}))}}
function handleSubmitEvent(id){{faiEventEmit('view:submit',jsToWasm({{id:id}}))}}
function morphDom(root,newHtml,replaceSelf){{var tmp=document.createElement('div');tmp.innerHTML=newHtml;if(replaceSelf&&root.parentNode&&tmp.childNodes.length===1){{morphNode(root,tmp.childNodes[0],root.parentNode);return}}morphChildren(root,tmp)}}
function morphChildren(op,np){{var oc=Array.from(op.childNodes),nc=Array.from(np.childNodes);var hasKeys=false;for(var i=0;i<nc.length;i++)if(nc[i].nodeType===1&&nc[i].getAttribute('data-fai-key')){{hasKeys=true;break}}if(hasKeys){{var oldMap={{}};for(var i=0;i<oc.length;i++)if(oc[i].nodeType===1){{var k=oc[i].getAttribute('data-fai-key');if(k)oldMap[k]=oc[i]}}for(var i=0;i<nc.length;i++){{var nk=nc[i].nodeType===1?nc[i].getAttribute('data-fai-key'):null;if(nk&&oldMap[nk]){{var old=oldMap[nk];if(i<op.childNodes.length){{if(op.childNodes[i]!==old)op.insertBefore(old,op.childNodes[i])}}else{{op.appendChild(old)}}morphNode(old,nc[i],op)}}else{{var ref=i<op.childNodes.length?op.childNodes[i]:null;op.insertBefore(nc[i],ref)}}}}while(op.childNodes.length>nc.length)op.removeChild(op.lastChild)}}else{{for(var i=0;i<Math.max(oc.length,nc.length);i++){{if(i>=nc.length){{while(op.childNodes.length>nc.length)op.removeChild(op.lastChild);break}}if(i>=oc.length){{op.appendChild(nc[i]);continue}}morphNode(oc[i],nc[i],op)}}}}}}
function morphNode(o,n,p){{if(o.nodeType!==n.nodeType){{p.replaceChild(n,o);return}}if(o.nodeType===3){{if(o.textContent!==n.textContent)o.textContent=n.textContent;return}}if(o.nodeType===1){{if(o.nodeName!==n.nodeName){{p.replaceChild(n,o);return}}patchAttrs(o,n);if(!/^(INPUT|IMG|BR|HR|META|LINK)$/.test(o.nodeName))morphChildren(o,n)}}}}
function patchAttrs(o,n){{var isF=o===document.activeElement&&o.tagName==='INPUT';var i,a,rm=[];for(i=0;i<n.attributes.length;i++){{a=n.attributes[i];if(a.name==='value'&&o.tagName==='INPUT'){{if(o.value!==a.value)o.value=a.value;continue;}}if(o.getAttribute(a.name)!==a.value)o.setAttribute(a.name,a.value)}}for(i=0;i<o.attributes.length;i++){{if(!n.hasAttribute(o.attributes[i].name))rm.push(o.attributes[i].name)}}for(i=0;i<rm.length;i++)o.removeAttribute(rm[i])}}
function wireEvents(){{document.querySelectorAll('[data-fai-click]').forEach(function(el){{var h=el.getAttribute('data-fai-click');el.onclick=function(){{invokeExport(h);startFai()}}}})}}
var env={{
  print:function(p,l){{var text=readStr(p,l);debugLog('FAI print',text);output.style.display='block';output.textContent+=text+'\n'}},
  read_file:function(){{return -1}},write_file:function(){{return -1}},now_ms:function(){{return Date.now()}},random:function(){{return Math.random()}},sleep_ms:function(){{throw new Error('FAI legacy sleep_ms is disabled; sleep() must lower through the async scheduler')}},host_set_timer:function(taskId,ms){{setTimeout(function(){{if(instance&&instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);faiServiceScheduler()}},Math.max(0,ms|0))}},
  call_ffi:function(){{return 0x7FFC000100000000n}},run_all:function(){{throw new Error('FAI legacy run_all is disabled; all() must lower through the async scheduler')}},
  spawn:function(closureVal){{var cv=closureVal;setTimeout(function(){{var n=BigInt(cv);var a=Number(n&0x0000FFFFFFFFFFFFn);var m=instance.exports.memory.buffer;var dv=new DataView(m);if(a+16>m.byteLength)return;var tag=dv.getInt32(a,true);if(tag!==4)return;var tidx=dv.getInt32(a+4,true);var envAddr=a+16;if(instance.exports.__env_ptr)instance.exports.__env_ptr.value=envAddr;var tbl=instance.exports.__indirect_function_table;if(tbl){{try{{tbl.get(tidx)()}}catch(e){{console.error('FAI spawn failed',e)}}}}if(typeof faiServiceScheduler==='function')faiServiceScheduler()}},0);return 0x7FFC000200000000n}},
  http_post:function(a,b,c,d,e){{try{{var x=new XMLHttpRequest();x.open('POST',readStr(a,b),false);x.setRequestHeader('Content-Type','application/json');x.send(readStr(c,d));return writeStr(e,x.responseText)}}catch(e){{return -1}}}},
  set_html:function(p,l){{morphDom(app,readStr(p,l),false);wireEvents()}},
  set_html_at:function(a,b,p,l){{var selector=readStr(a,b),html=readStr(p,l);var root=document.querySelector(selector);if(!root&&selector.charAt(0)==='#'){{root=document.createElement('div');root.id=selector.slice(1);app.innerHTML='';app.appendChild(root)}}if(!root)return;morphDom(root,html,selector!=='#app');wireEvents()}},
  json_parse:function(p,l){{try{{return jsToWasm(JSON.parse(readStr(p,l)))}}catch(e){{return QNAN|TAG_NULL}}}},
  json_stringify:function(v){{try{{return writeStrToWasm(JSON.stringify(wasmToJs(v)))}}catch(e){{return writeStrToWasm('null')}}}},
  crypto_available:function(){{return 0}},
  process_available:function(){{return 0}},
  remote_call:function(a,b,c,d,e,f,g,h){{var fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);var body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});function throwBack(msg){{var box=jsToWasm({{message:msg,kind:'remote'}});instance.exports.__error_flag.value=1;instance.exports.__error_value.value=BigInt.asIntN(64,BigInt(box));return NULL_VAL}}var x=new XMLHttpRequest();try{{x.open('POST','/fai/rpc',false);x.setRequestHeader('Content-Type','application/json');x.send(body)}}catch(e){{return throwBack('network error: '+(e&&e.message?e.message:'request failed'))}}if(x.status===0)return throwBack('network error: request blocked or offline');if(x.status<200||x.status>=300)return throwBack('HTTP '+x.status+(x.statusText?': '+x.statusText:''));var resp;try{{resp=JSON.parse(x.responseText)}}catch(e){{return throwBack('invalid JSON in response')}}if(resp.ok)return jsToWasm(resp.value);return throwBack(resp.error||'remote call failed')}},
  remote_begin:function(taskId,a,b,c,d,e,f,g,h){{var fn_name=readStr(c,d),ar=readStr(e,f),ha=readStr(g,h);var body=JSON.stringify({{fn:fn_name,args:JSON.parse(ar||'[]'),hash:ha}});function done(res){{__faiRpcResults[taskId]=res;if(instance.exports.__fai_resume_task)instance.exports.__fai_resume_task(taskId);faiServiceScheduler()}}fetch('/fai/rpc',{{method:'POST',headers:{{'Content-Type':'application/json'}},body:body}}).then(function(r){{var st=r.status;return r.text().then(function(t){{if(st<200||st>=300){{done({{err:'HTTP '+st}});return}}var resp;try{{resp=JSON.parse(t)}}catch(e){{done({{err:'invalid JSON in response'}});return}}if(resp.ok)done({{val:jsToWasm(resp.value)}});else done({{err:resp.error||'remote call failed'}})}})}}).catch(function(e){{done({{err:'network error: '+(e&&e.message?e.message:'request failed')}})}})}},
  remote_result:function(taskId){{var res=__faiRpcResults[taskId];delete __faiRpcResults[taskId];if(!res)return NULL_VAL;if(res.err!==undefined){{var box=jsToWasm({{message:res.err,kind:'remote'}});instance.exports.__error_flag.value=1;instance.exports.__error_value.value=BigInt.asIntN(64,BigInt(box));return NULL_VAL}}return res.val}},
  float_to_str:function(v,p){{var s=(v===Math.floor(v)&&isFinite(v))?String(BigInt(v)):String(v);var b=new TextEncoder().encode(s);new Uint8Array(instance.exports.memory.buffer,p,b.length).set(b);return b.length}},
  get_location_path:function(){{return writeStrToWasm(window.location.pathname)}},
  push_history_state:function(p,l){{history.pushState(null,'',readStr(p,l))}},
  storage_get:function(kp,kl,bp){{try{{var k=readStr(kp,kl);var v=window.localStorage.getItem(k);if(v===null)return -1;var b=new TextEncoder().encode(v);if(b.length>65536)return -1;new Uint8Array(instance.exports.memory.buffer,bp,b.length).set(b);return b.length}}catch(e){{return -1}}}},
  storage_get_str:function(kp,kl){{try{{var k=readStr(kp,kl);var v=window.localStorage.getItem(k);if(v===null)return NULL_VAL;return writeStrToWasm(v)}}catch(e){{return NULL_VAL}}}},
  file_read_str:function(){{return NULL_VAL}},
  storage_set:function(kp,kl,vp,vl){{try{{window.localStorage.setItem(readStr(kp,kl),readStr(vp,vl))}}catch(e){{}}}},
  storage_remove:function(kp,kl){{try{{window.localStorage.removeItem(readStr(kp,kl))}}catch(e){{}}}},
  storage_clear:function(){{try{{window.localStorage.clear()}}catch(e){{}}}},
  env_get:function(){{return NULL_VAL}},
  env_load:function(){{return 0}},
  event_on:function(np,nl,cv){{var name=readStr(np,nl);var id=++faiEventRegistry.nextId;if(!faiEventRegistry.byName[name])faiEventRegistry.byName[name]=[];faiEventRegistry.byName[name].push({{id:id,closureVal:BigInt.asIntN(64,BigInt(cv)),once:false}});return faiBuildSubscription(id,name)}},
  event_once:function(np,nl,cv){{var name=readStr(np,nl);var id=++faiEventRegistry.nextId;if(!faiEventRegistry.byName[name])faiEventRegistry.byName[name]=[];faiEventRegistry.byName[name].push({{id:id,closureVal:BigInt.asIntN(64,BigInt(cv)),once:true}});return faiBuildSubscription(id,name)}},
  event_off:function(sv){{var sub=faiReadSubscription(sv);if(!sub)return 0;var list=faiEventRegistry.byName[sub.name];if(!list)return 0;var before=list.length;faiEventRegistry.byName[sub.name]=list.filter(function(s){{return s.id!==sub.id}});return before!==faiEventRegistry.byName[sub.name].length?1:0}},
  event_emit:function(np,nl,dv){{faiEventEmit(readStr(np,nl),dv)}},
  event_subscribers:function(np,nl){{var list=faiEventRegistry.byName[readStr(np,nl)];return list?list.length:0}},
  event_clear:function(np,nl){{delete faiEventRegistry.byName[readStr(np,nl)]}},
  event_clear_all:function(){{faiEventRegistry.byName=Object.create(null);faiEventRegistry.nextId=0;faiEventRegistry.queue=[];faiEventRegistry.draining=false}},
  event_emit_deferred:function(np,nl,dv){{faiEventRegistry.queue.push({{name:readStr(np,nl),dataVal:BigInt.asIntN(64,BigInt(dv))}})}},
  event_drain:function(){{if(faiEventRegistry.draining)return;faiEventRegistry.draining=true;while(faiEventRegistry.queue.length>0){{var ev=faiEventRegistry.queue.shift();faiEventEmit(ev.name,ev.dataVal)}}faiEventRegistry.draining=false}},
  event_queue_len:function(){{return faiEventRegistry.queue.length}},
  file_exists:function(){{return 0}},
  http_request_get:function(p,l){{return httpRequest('GET',readStr(p,l))}},
  http_request_post:function(up,ul,bp,bl){{return httpRequest('POST',readStr(up,ul),readStr(bp,bl))}},
  http_request_put:function(up,ul,bp,bl){{return httpRequest('PUT',readStr(up,ul),readStr(bp,bl))}},
  http_request_patch:function(up,ul,bp,bl){{return httpRequest('PATCH',readStr(up,ul),readStr(bp,bl))}},
  http_request_delete:function(p,l){{return httpRequest('DELETE',readStr(p,l))}},
  net_available:function(){{return 0}},
  ffi_available:function(){{return 0}},
  log_info:function(p,l){{console.info(readStr(p,l))}},
  log_warn:function(p,l){{console.warn(readStr(p,l))}},
  log_error:function(p,l){{console.error(readStr(p,l))}},
  path_join:function(a,b,c,d){{var left=readStr(a,b).replace(/\/+$/,''),right=readStr(c,d).replace(/^\/+/,'');return writeStrToWasm(left+'/'+right)}},
  path_basename:function(p,l){{var s=readStr(p,l).replace(/\/+$/,'');var i=s.lastIndexOf('/');return writeStrToWasm(i>=0?s.slice(i+1):s)}},
  path_dirname:function(p,l){{var s=readStr(p,l).replace(/\/+$/,'');var i=s.lastIndexOf('/');return writeStrToWasm(i>0?s.slice(0,i):'.')}},
  path_extname:function(p,l){{var s=readStr(p,l),base=s.slice(s.lastIndexOf('/')+1),i=base.lastIndexOf('.');return writeStrToWasm(i>0?base.slice(i):'')}},
  html_escape:function(p,l){{return writeStrToWasm(readStr(p,l).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;'))}},
  file_list:function(){{return jsToWasm([])}},
  json_require_string:function(v,kp,kl){{var obj=wasmToJs(v),key=readStr(kp,kl);return writeStrToWasm(obj&&typeof obj[key]==='string'?obj[key]:'')}},
  array_map:function(arr){{return arr}},
  array_filter:function(arr){{return arr}},
  array_find:function(){{return NULL_VAL}},
  array_is_any:function(){{return QNAN|TAG_BOOL}},
  array_is_all:function(){{return QNAN|TAG_BOOL|1n}},
  tcp_listen:function(){{return 0}},
  tcp_accept:function(){{return NULL_VAL}},
  tcp_connect:function(){{return 0}},
  tcp_read:function(){{return NULL_VAL}},
  tcp_read_line:function(){{return NULL_VAL}},
  tcp_write:function(){{return -1}},
  tcp_close:function(){{}},
  tcp_address:function(){{return writeStrToWasm('')}},
  udp_bind:function(){{return 0}},
  udp_send:function(){{return -1}},
  udp_receive:function(){{return NULL_VAL}},
  udp_broadcast:function(){{}},
  cli_read_line:function(){{return NULL_VAL}},
  cli_write:function(p,l){{output.style.display='block';output.textContent+=readStr(p,l)}},
  cli_write_line:function(p,l){{output.style.display='block';output.textContent+=readStr(p,l)+'\n'}},
  cli_clear:function(){{output.textContent=''}},
  cli_move_to:function(){{}},
  __fai_alloc_event:function(addr,size){{faiLeakAlloc(addr>>>0,size>>>0,false)}},
  __fai_free_event:function(addr,size){{faiLeakFree(addr>>>0,size>>>0)}},
  __fai_set_trap_msg:function(p,l){{var m=readStr(p,l);window.__FAI_TRAP_MSG=m;console.error('FAI trap:',m)}},
  __fai_trap_report:function(code,a,b){{
    // Plan 116: structured trap reason, mirrored from the native host
    // (wasm_runner/host/io.rs::format_trap_report). Logged before the
    // guest executes `unreachable`, so the reason survives the trap.
    function describeVal(v){{try{{var s=readNanBoxedStr(v);if(s)return 'String "'+s.slice(0,40)+'"';var j=wasmToJs(v);return j===null?'null':(typeof j==='object'?JSON.stringify(j).slice(0,80):String(j))}}catch(e){{return '<value 0x'+BigInt.asUintN(64,BigInt(v)).toString(16)+'>'}}}}
    function addrOf(v){{return '0x'+(BigInt.asUintN(64,BigInt(v))&0x0000FFFFFFFFFFFFn).toString(16)}}
    var msg;
    switch(code){{
      case 1: msg='rc-check: retain of freed object at '+addrOf(a); break;
      case 2: msg='rc-check: release of freed object at '+addrOf(a); break;
      case 3: msg='rc-check: over-release (rc '+b+') of '+describeVal(a)+' at '+addrOf(a); break;
      case 4: msg='out of memory: failed to grow linear memory ('+a+' bytes requested, heap needs 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 5: msg='async task table full ('+a+' of '+b+' slots used)'; break;
      case 6: msg='force-unwrap (`!`) of null'; break;
      case 7: msg='uncaught error: '+describeVal(a); break;
      case 8: msg='scheduler stall: poll resumed '+a+' tasks without quiescing (livelock; task t'+b+' was about to run again)'; break;
      case 9: msg='rc-check: corrupt free-list node 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' (heap_ptr 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 10: msg='rc-check: freed block at 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' was written through a stale pointer while on the free list (tag word now 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')'; break;
      case 11: msg='rc-check: double free of block at 0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+' (block size '+b+')'; break;
      case 12: msg='checked: index store out of bounds — xs['+a+'] = ... on an array of '+b+' elements'; break;
      case 13: msg='dict grow: implausible capacity '+a+' (size word 0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+') — dictionary.set on a non-dict/stale/mis-typed pointer'; break;
      case 14: msg='alloc-guard: single allocation of '+a+' bytes ('+b+' block) exceeds 256 MB — runaway allocation'; break;
      default: msg='trap report (code '+code+', a=0x'+BigInt.asUintN(64,BigInt(a)).toString(16)+', b=0x'+BigInt.asUintN(64,BigInt(b)).toString(16)+')';
    }}
    window.__FAI_TRAP_MSG=msg;console.error('FAI trap:',msg);
  }}
}};
fetch('/{}').then(function(r){{return r.arrayBuffer()}}).then(function(b){{return WebAssembly.instantiate(b,{{env:env}})}}).then(function(r){{
  instance=r.instance;window.__fai_dbg=r.instance;startFai();
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
    let parsed = match parse_new_args(args) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };

    let project_root = std::path::Path::new(&parsed.project_dir);

    if project_root.exists() {
        eprintln!("error: target already exists: {}", project_root.display());
        std::process::exit(1);
    }

    let project_name = project_root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if let Some(tref_str) = &parsed.template {
        let tref = match templates::parse_template_ref(tref_str) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        };
        scaffold_from_template_ref(tref, project_root, &project_name);
        return;
    }

    inline_scaffold(project_root, &project_name);
}

struct NewArgs {
    project_dir: String,
    template: Option<String>,
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut template: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--template" => {
                i += 1;
                if i >= args.len() {
                    return Err("error: --template requires a value".to_string());
                }
                template = Some(args[i].clone());
            }
            "--yes" | "-y" => {
                // Reserved for future confirmation prompts (network mode).
                // Currently a no-op for the local-template path.
            }
            arg if arg.starts_with("--") => {
                return Err(format!("error: unknown flag: {}", arg));
            }
            _ => positional.push(args[i].clone()),
        }
        i += 1;
    }
    if positional.is_empty() {
        return Err("Usage: forai new <project-dir> [--template <ref>]".to_string());
    }
    if positional.len() > 1 {
        return Err(format!(
            "error: expected one project directory, got {}",
            positional.len()
        ));
    }
    Ok(NewArgs {
        project_dir: positional.into_iter().next().unwrap(),
        template,
    })
}

fn scaffold_from_template_ref(
    tref: templates::TemplateRef,
    project_root: &std::path::Path,
    project_name: &str,
) {
    match tref {
        templates::TemplateRef::Local(path) => {
            let opts = templates::ScaffoldOptions {
                template_root: &path,
                target_dir: project_root,
                project_name,
            };
            if let Err(e) = templates::scaffold_from_local(&opts) {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
            overlay_meta_files(project_root, project_name);
            println!("scaffolded {} from {}", project_name, path.display());
        }
        templates::TemplateRef::Github {
            owner,
            repo,
            git_ref,
        } => {
            scaffold_from_github(
                &owner,
                &repo,
                git_ref.as_deref(),
                project_root,
                project_name,
            );
        }
        templates::TemplateRef::Url { .. } => {
            eprintln!("error: arbitrary URL templates are not yet supported");
            eprintln!("note: use the GitHub shorthand `<owner>/<repo>[#ref]`");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "http-client")]
fn scaffold_from_github(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    project_root: &std::path::Path,
    project_name: &str,
) {
    let ref_label = git_ref.unwrap_or("HEAD");
    println!(
        "fetching https://github.com/{}/{} ({})",
        owner, repo, ref_label
    );
    let template_root = match templates::fetch_github_template(owner, repo, git_ref) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    let opts = templates::ScaffoldOptions {
        template_root: &template_root,
        target_dir: project_root,
        project_name,
    };
    let res = templates::scaffold_from_local(&opts);
    let _ = std::fs::remove_dir_all(&template_root);
    if let Err(e) = res {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    overlay_meta_files(project_root, project_name);
    println!(
        "scaffolded {} from {}/{} ({})",
        project_name, owner, repo, ref_label
    );
}

#[cfg(not(feature = "http-client"))]
fn scaffold_from_github(
    _owner: &str,
    _repo: &str,
    _git_ref: Option<&str>,
    _project_root: &std::path::Path,
    _project_name: &str,
) {
    eprintln!("error: this fai build was compiled without the `http-client` feature");
    eprintln!("note: rebuild with `--features http-client` to use network templates");
    std::process::exit(1);
}

fn inline_scaffold(project_root: &std::path::Path, project_name: &str) {
    let src_dir = project_root.join("src");
    if let Err(e) = std::fs::create_dir_all(&src_dir) {
        eprintln!("error creating directory: {}", e);
        std::process::exit(1);
    }

    let project_files: Vec<(std::path::PathBuf, String)> = vec![
        (src_dir.join("main.fai"), scaffold_main(project_name)),
        (
            project_root.join("fai.toml"),
            scaffold_fai_toml(project_name),
        ),
        (
            project_root.join("README.md"),
            scaffold_readme(project_name),
        ),
    ];

    for (path, content) in &project_files {
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("error writing {}: {}", path.display(), e);
            std::process::exit(1);
        }
    }

    overlay_meta_files(project_root, project_name);

    println!("created project '{}'", project_name);
}

/// Write language-level metadata files (`CLAUDE.md`, `AGENTS.md`,
/// `language.md`, `.mcp.json`, `.codex/config.toml`) into a project
/// directory. These belong with the language tooling, not with any
/// individual template — `fai new` overlays them onto every new
/// project regardless of template source.
///
/// `AGENTS.md` and `CLAUDE.md` are special: when the template ships
/// its own copy, the scaffold's language-level guidance is written
/// first and the template's content is appended below a separator.
/// This keeps language-level rules (doc comments, testing) visible
/// while preserving template-specific guidance the user picked.
///
/// All other files use last-write-wins semantics: a file the template
/// already shipped is left alone; anything missing is filled in.
fn overlay_meta_files(dir: &std::path::Path, project_name: &str) {
    let codex_dir = dir.join(".codex");
    if !codex_dir.exists() {
        let _ = std::fs::create_dir_all(&codex_dir);
    }

    // Append-on-collision: language scaffold + template-shipped content.
    let merging: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("CLAUDE.md"), scaffold_claude_md(project_name)),
        (dir.join("AGENTS.md"), scaffold_agents_md()),
    ];
    for (path, scaffold) in &merging {
        write_with_template_append(path, scaffold);
    }

    // Fill-only-if-missing: language reference + tool configs.
    let fill_only: Vec<(std::path::PathBuf, String)> = vec![
        (dir.join("language.md"), scaffold_language_md()),
        (dir.join(".mcp.json"), scaffold_mcp_json()),
        (dir.join(".codex/config.toml"), scaffold_codex_config()),
    ];
    for (path, content) in &fill_only {
        if path.exists() {
            continue;
        }
        if let Err(e) = std::fs::write(path, content) {
            eprintln!("warning: could not write {}: {}", path.display(), e);
        }
    }
}

/// Write `scaffold` to `path`. If the template already shipped a file
/// at this path, append its content below a separator so both stay
/// visible. The scaffold goes first because language-level rules
/// (e.g. doc-comment requirement) are universal and should be the
/// first thing an agent reads.
fn write_with_template_append(path: &std::path::Path, scaffold: &str) {
    let template_content = if path.exists() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    };
    let combined = match template_content {
        Some(t) if !t.trim().is_empty() => {
            format!(
                "{}\n---\n\n# Project-specific guidance\n\n{}",
                scaffold.trim_end(),
                t.trim_start()
            )
        }
        _ => scaffold.to_string(),
    };
    if let Err(e) = std::fs::write(path, combined) {
        eprintln!("warning: could not write {}: {}", path.display(), e);
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
                for (dep_name, dep_path) in doc_parse_file_deps(&toml_content, &project_root) {
                    // Package function docs
                    all_entries.extend(doc::collect_dependency_docs(&dep_path, &dep_name));
                    // Package sub-module overview docs (`src/<module>/docs.md`)
                    all_entries.extend(doc::collect_dependency_module_overviews(
                        &dep_path, &dep_name,
                    ));
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
        // If the namespace has an overview, print it after the child list so
        // intermediate pages like `std.http` are useful without hiding their
        // drill-down paths.
        if !query.is_empty() {
            // Show the overview entry for this namespace if one exists.
            let overview: Vec<_> = all_entries
                .iter()
                .filter(|e| {
                    e.full_path == query && matches!(e.kind, doc::EntryKind::PackageOverview)
                })
                .collect();
            if !overview.is_empty() {
                println!();
                doc::render_docs(&overview);
            }
        }
        return;
    }

    // Leaf namespace with an overview: render the prose first, then the declarations.
    let namespace_prefix = format!("{}.", query);
    let overview: Vec<_> = all_entries
        .iter()
        .filter(|e| e.full_path == query && matches!(e.kind, doc::EntryKind::PackageOverview))
        .collect();
    let namespace_entries: Vec<_> = all_entries
        .iter()
        .filter(|e| {
            (e.namespace == query || e.full_path.starts_with(&namespace_prefix))
                && !matches!(e.kind, doc::EntryKind::PackageOverview)
        })
        .collect();
    if !overview.is_empty() && !namespace_entries.is_empty() {
        doc::render_docs(&overview);
        println!();
        doc::render_docs(&namespace_entries);
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
    doc::render_search_results(&results);
}

/// Parse `[dependencies]` from a fai.toml string and return
/// `(package_name, project_root_path)` pairs.  Resolves both `file://`
/// paths and `https://` git URLs (the latter uses the local git cache).
/// Relative file:// paths resolve against `project_root`.
fn doc_parse_file_deps(
    toml_content: &str,
    project_root: &std::path::Path,
) -> Vec<(String, std::path::PathBuf)> {
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
        let Some(spec) = fai_compiler::dep_url::parse_dep_line(t) else {
            continue;
        };
        let Ok(dep_root) = fai_compiler::dep_url::resolve_dep_url(&spec.url, project_root) else {
            continue;
        };
        let path_str = dep_root.to_string_lossy().into_owned();
        let path_str = path_str.as_str();
        let dep_info =
            read_project_info_full(Some(dep_root.join("src").to_str().unwrap_or(path_str)));
        // The fai.toml LHS is the canonical name; fall back to the
        // dep's own [project] name only if the LHS was malformed.
        let dep_name = if !spec.name.is_empty() {
            spec.name
        } else if !dep_info.name.is_empty() && dep_info.name != "unknown" {
            dep_info.name
        } else {
            dep_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "dep".to_string())
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

You can chain UFCS modifiers directly on the result of a `do...end`
trailing-closure call:

```fai
let view = VStack do
    Label('hi')
end.padding(12).background('#fafafa')
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
let a, b = all(fetchUsers(), fetchPosts())          # parallel, await both
sleep(500)                                          # pause without blocking host
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

- **Document as you go.** Every named `def`, `remote def`, and `test`
  block requires a `# Description.` line directly above it. Missing
  one is a type error: `doc comment required`. `main` is exempt.
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

- **Document as you go.** Every named `def`, `remote def`, and `test`
  block requires a `# Description.` line directly above it. Missing
  one is a type error: `doc comment required`. `main` is exempt.
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
///
/// `rpc_proxy_substitution` is set by `step_build` when the active
/// sub-project has `rpc_server = false` (default) and a remote dependency
/// URL is available. Passing `Some((url, hash))` rewrites every reachable
/// `remote def` body to call `remoteCall(url, key, args, hash)` so the
/// client wasm never executes server-only code. Set to `None` for the
/// server target (`rpc_server = true`) and for `is_test = true` so unit
/// tests exercising server bodies natively keep working.
fn compile_fai_to_wasm(
    content: &str,
    path: &str,
    is_test: bool,
    synthetic_modules: Vec<(String, String)>,
    target: Option<&str>,
    rpc_proxy_substitution: Option<(&str, &str)>,
) -> Vec<u8> {
    let source_root = find_source_root(path);
    let mut prepared = match fai_compiler::prepare_source_with_synthetic_and_entry(
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

    if let Some((url, hash)) = rpc_proxy_substitution {
        rewrite_remote_def_bodies(&mut prepared.modules, url, hash);
    }

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
    match fai_codegen_wasm::codegen_direct_full_reasoned_with_entry_file(
        &prepared.serde_ast,
        &prepared.modules,
        &info,
        target,
        is_test,
        Some(path),
    ) {
        Ok(wasm) => wasm,
        Err(e) => {
            eprintln!("{}", format_codegen_error(&e));
            std::process::exit(1);
        }
    }
}

fn run_checker(
    checker: &mut fai_checker::Checker,
    prepared: &fai_compiler::PreparedProgram,
) -> Result<(), fai_checker::CheckError> {
    // R0 clean slate (plan 113): the ownership move/borrow-escape audits +
    // enforcement are removed. Memory safety comes from reference counting (R1).
    run_checker_inner(checker, prepared)
}

fn run_checker_inner(
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
                file_paths: m.file_paths.clone(),
                private_names: m.private_names.clone(),
                file_path: None,
            })
            .collect();
        checker.check_with_modules(&prepared.serde_ast.statements, &prepared_modules)
    }
}

/// Render a codegen `LocatedBuildError` in the same `Source codegen
/// errors:` block style the check phase uses, so file:line shows up
/// the same way. The variant `Debug` form (e.g.
/// `UnknownIdentifier("length")`) is what surfaces inside the file
/// section; an unlocated error falls into a `(no file)` bucket.
///
/// Errors that point into an **external package** (the module's root
/// segment starts uppercase — `Forui`, `Forsqlite`, …) get a
/// dedicated `package: <Name>` heading and an explicit "fix
/// upstream" note, so the agent doesn't waste turns trying to
/// resolve a framework-internal failure inside the user's project.
///
/// Returns the formatted block — the caller decides how to combine
/// with its own step status.
fn format_codegen_error(err: &fai_codegen_wasm::LocatedBuildError) -> String {
    let project_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_project_root(&cwd));
    let display_path = |raw: &str| -> String {
        if let Some(root) = &project_root {
            if let Ok(canon) = std::fs::canonicalize(raw) {
                if let Ok(rel) = canon.strip_prefix(root) {
                    return rel.display().to_string();
                }
            }
        }
        raw.to_string()
    };
    let mut out = String::from("\nSource codegen errors:\n");
    let body = match (err.line, err.col) {
        (Some(l), Some(c)) => format!("  {:?} (line {}:{})\n", err.err, l, c),
        (Some(l), None) => format!("  {:?} (line {})\n", err.err, l),
        _ => format!("  {:?}\n", err.err),
    };

    // Heading priority: module name (always, no conditional on
    // external vs user) → file path → `(no file)` bucket.
    if let Some(module) = err.module.as_deref() {
        out.push('\n');
        out.push_str(&format!("package: {}\n", module));
        out.push_str(&body);
        if err.is_external_package() {
            out.push_str(
                "  ** This error is in an external dependency and will need to be fixed there\n",
            );
        }
    } else if let Some(f) = err.file.as_deref() {
        out.push('\n');
        out.push_str(&display_path(f));
        out.push('\n');
        out.push_str(&body);
    } else {
        out.push_str("\n(no file)\n");
        out.push_str(&body);
    }
    out
}

/// Group check errors by source file under a `Source check errors:`
/// header. Each file gets a heading followed by indented messages —
/// no `type error:` prefix on every line, since the section header
/// already says it. Errors that arrived without file info land in a
/// trailing `(no file)` bucket. Paths are shown relative to the
/// project root when one can be located, otherwise as-is.
///
/// Falls back to the single `Err(CheckError)` when the accumulator
/// is empty (e.g. Phase 1 failures that short-circuit before
/// per-statement collection runs).
fn format_check_errors(checker: &fai_checker::Checker, first: &fai_checker::CheckError) -> String {
    if checker.collected_errors.is_empty() {
        return first.to_string();
    }

    let project_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| find_project_root(&cwd));

    let display_path = |raw: &str| -> String {
        if let Some(root) = &project_root {
            if let Ok(canon) = std::fs::canonicalize(raw) {
                if let Ok(rel) = canon.strip_prefix(root) {
                    return rel.display().to_string();
                }
            }
        }
        raw.to_string()
    };

    let mut by_file: std::collections::BTreeMap<String, Vec<&fai_checker::CheckError>> =
        std::collections::BTreeMap::new();
    let mut no_file: Vec<&fai_checker::CheckError> = Vec::new();
    for err in &checker.collected_errors {
        match err.file.as_deref() {
            Some(p) => by_file.entry(display_path(p)).or_default().push(err),
            None => no_file.push(err),
        }
    }

    let mut out = String::from("\nSource check errors:\n");
    for (file, errs) in &by_file {
        out.push('\n');
        out.push_str(file);
        out.push('\n');
        for e in errs {
            out.push_str("  ");
            out.push_str(&e.message);
            if let (Some(line), Some(col)) = (e.line, e.column) {
                out.push_str(&format!(" (line {}:{})", line, col));
            } else if let Some(line) = e.line {
                out.push_str(&format!(" (line {})", line));
            }
            out.push('\n');
        }
    }
    if !no_file.is_empty() {
        out.push_str("\n(no file)\n");
        for e in &no_file {
            out.push_str("  ");
            out.push_str(&e.message);
            out.push('\n');
        }
    }
    out
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

    #[test]
    fn test_parse_string_array_inline() {
        assert_eq!(parse_string_array(r#"["web"]"#), vec!["web".to_string()]);
        assert_eq!(
            parse_string_array(r#"["web", "other"]"#),
            vec!["web".to_string(), "other".to_string()]
        );
        // Tolerant of whitespace and a trailing comma.
        assert_eq!(
            parse_string_array(r#"[ "a" , "b", ]"#),
            vec!["a".to_string(), "b".to_string()]
        );
        // Not an array → empty vec.
        assert_eq!(parse_string_array("\"web\""), Vec::<String>::new());
        assert_eq!(parse_string_array("[]"), Vec::<String>::new());
    }

    /// Build a minimal `ProjectInfo` with a list of (name, deps,
    /// assets) tuples for the planner / asset tests below. The TOML
    /// parser is exercised separately; these tests want a fixture
    /// they can construct cheaply without round-tripping through TOML.
    fn project_with_targets(targets: &[(&str, Vec<&str>, Vec<(&str, &str)>)]) -> ProjectInfo {
        let mut info = ProjectInfo {
            name: "test".into(),
            version: "0.0.0".into(),
            ..Default::default()
        };
        for (name, deps, assets) in targets {
            let mut sub = SubProject::default();
            sub.required_targets = deps.iter().map(|d| d.to_string()).collect();
            sub.assets = assets
                .iter()
                .map(|(f, t)| (f.to_string(), t.to_string()))
                .collect();
            info.sub_projects.insert(name.to_string(), sub);
        }
        info
    }

    #[test]
    fn test_plan_build_order_single_target_no_deps() {
        let info = project_with_targets(&[("server", vec![], vec![])]);
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_dep_built_first() {
        let info =
            project_with_targets(&[("web", vec![], vec![]), ("server", vec!["web"], vec![])]);
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["web".to_string(), "server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_transitive_chain() {
        // a → b → c → d (a depends on b which depends on c which …)
        // Building `a` should produce d, c, b, a in that order.
        let info = project_with_targets(&[
            ("a", vec!["b"], vec![]),
            ("b", vec!["c"], vec![]),
            ("c", vec!["d"], vec![]),
            ("d", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, Some("a")).unwrap();
        assert_eq!(
            order,
            vec![
                "d".to_string(),
                "c".to_string(),
                "b".to_string(),
                "a".to_string()
            ]
        );
    }

    #[test]
    fn test_plan_build_order_diamond() {
        // a → b, a → c, b → d, c → d. d must come first; a must come
        // last; b and c can be in either order between them. Each
        // target should appear exactly once (no double-build of d).
        let info = project_with_targets(&[
            ("a", vec!["b", "c"], vec![]),
            ("b", vec!["d"], vec![]),
            ("c", vec!["d"], vec![]),
            ("d", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, Some("a")).unwrap();
        assert_eq!(order.len(), 4, "each target builds exactly once");
        let pos = |t: &str| order.iter().position(|n| n == t).unwrap();
        assert!(pos("d") < pos("b"));
        assert!(pos("d") < pos("c"));
        assert!(pos("b") < pos("a"));
        assert!(pos("c") < pos("a"));
    }

    #[test]
    fn test_plan_build_order_detects_cycle() {
        let info = project_with_targets(&[("a", vec!["b"], vec![]), ("b", vec!["a"], vec![])]);
        let err = plan_build_order(&info, Some("a")).unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {}", err);
        assert!(err.contains("a") && err.contains("b"));
    }

    #[test]
    fn test_plan_build_order_detects_self_cycle() {
        let info = project_with_targets(&[("a", vec!["a"], vec![])]);
        let err = plan_build_order(&info, Some("a")).unwrap_err();
        assert!(err.contains("cycle"), "expected cycle error, got: {}", err);
    }

    #[test]
    fn test_plan_build_order_unknown_dep_skipped() {
        let info = project_with_targets(&[("server", vec!["nonexistent"], vec![])]);
        // Unknown deps warn but don't fail planning. `server` still
        // builds — the warning gives the user a chance to fix the
        // typo without breaking everyone else's build.
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["server".to_string()]);
    }

    #[test]
    fn test_plan_build_order_build_all_alphabetic_roots() {
        // No `requested` → walk every sub-project. Roots are sorted
        // alphabetically for stable ordering across runs. With no
        // deps, the output is just the sorted target list.
        let info = project_with_targets(&[
            ("zeta", vec![], vec![]),
            ("alpha", vec![], vec![]),
            ("middle", vec![], vec![]),
        ]);
        let order = plan_build_order(&info, None).unwrap();
        assert_eq!(
            order,
            vec![
                "alpha".to_string(),
                "middle".to_string(),
                "zeta".to_string()
            ]
        );
    }

    #[test]
    fn test_plan_build_order_build_all_respects_deps() {
        // Build-all walks every target alphabetically as a root, but
        // each root's deps still come before it. Net effect: deps
        // appear before dependents regardless of alphabetical name.
        let info =
            project_with_targets(&[("server", vec!["web"], vec![]), ("web", vec![], vec![])]);
        let order = plan_build_order(&info, None).unwrap();
        let pos_web = order.iter().position(|n| n == "web").unwrap();
        let pos_server = order.iter().position(|n| n == "server").unwrap();
        assert!(pos_web < pos_server, "web must build before server");
    }

    #[test]
    fn test_copy_dir_merge_copies_file_tree() {
        let tmp = temp_dir("copy_dir_merge_tree");
        let src = tmp.join("src");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("nested").join("b.txt"), "world").unwrap();

        copy_dir_merge(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested").join("b.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn test_copy_dir_merge_layers_two_sources() {
        // Two sequential copies into the same destination: the second
        // overwrites overlapping files but preserves files unique to
        // the first. This is the exact pattern used by the assets
        // map to layer a generated bundle and a project's public/.
        let tmp = temp_dir("copy_dir_merge_layers");
        let src_a = tmp.join("a");
        let src_b = tmp.join("b");
        let dst = tmp.join("dst");
        std::fs::create_dir_all(&src_a).unwrap();
        std::fs::create_dir_all(&src_b).unwrap();
        std::fs::write(src_a.join("only_a.txt"), "from-a").unwrap();
        std::fs::write(src_a.join("shared.txt"), "a-version").unwrap();
        std::fs::write(src_b.join("only_b.txt"), "from-b").unwrap();
        std::fs::write(src_b.join("shared.txt"), "b-version").unwrap();

        copy_dir_merge(&src_a, &dst).unwrap();
        copy_dir_merge(&src_b, &dst).unwrap();

        assert_eq!(
            std::fs::read_to_string(dst.join("only_a.txt")).unwrap(),
            "from-a"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("only_b.txt")).unwrap(),
            "from-b"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("shared.txt")).unwrap(),
            "b-version",
            "later copy wins on overlap"
        );
    }

    #[test]
    fn test_copy_dir_merge_missing_source_is_noop() {
        let tmp = temp_dir("copy_dir_merge_missing");
        let dst = tmp.join("dst");
        let result = copy_dir_merge(&tmp.join("does_not_exist"), &dst);
        assert!(result.is_ok());
        assert!(!dst.exists(), "destination not created when source missing");
    }

    #[test]
    fn test_copy_target_assets_resolves_target_ref_and_project_path() {
        // Set up a tiny project root with a generated `build/web/`
        // and an authored `public/`, mimicking forailang.com. After
        // copy_target_assets runs, both directories should be merged
        // into `build/server/public/`.
        let root = temp_dir("copy_target_assets");
        std::fs::create_dir_all(root.join("build/web")).unwrap();
        std::fs::write(root.join("build/web/web.wasm"), "wasm-bytes").unwrap();
        std::fs::write(root.join("build/web/forui.css"), "css-bytes").unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/favicon.ico"), "icon-bytes").unwrap();
        // The server's build_dir must exist (build_one_subproject
        // would have created it via cmd_build); fake that here.
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let mut web = SubProject::default();
        web.build_dir = Some("build/web".to_string());
        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![
            ("$web".to_string(), "public".to_string()),
            ("public".to_string(), "public".to_string()),
        ];

        let mut info = ProjectInfo::default();
        info.sub_projects.insert("web".to_string(), web);
        info.sub_projects
            .insert("server".to_string(), server.clone());

        copy_target_assets("server", &server, &info, &root);

        let merged = root.join("build/server/public");
        assert!(merged.join("web.wasm").exists(), "$web/web.wasm copied");
        assert!(merged.join("forui.css").exists(), "$web/forui.css copied");
        assert!(
            merged.join("favicon.ico").exists(),
            "project public/favicon.ico copied"
        );
    }

    #[test]
    fn test_copy_target_assets_empty_to_copies_into_build_dir_root() {
        let root = temp_dir("copy_target_assets_root");
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/robots.txt"), "user-agent: *").unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![("public".to_string(), "".to_string())];

        let mut info = ProjectInfo::default();
        info.sub_projects
            .insert("server".to_string(), server.clone());

        copy_target_assets("server", &server, &info, &root);

        // Empty `to` → the public/ contents land directly inside
        // build/server/, not nested under build/server/public/.
        assert!(root.join("build/server/robots.txt").exists());
        assert!(!root.join("build/server/public/robots.txt").exists());
    }

    #[test]
    fn test_copy_target_assets_missing_source_does_not_panic() {
        let root = temp_dir("copy_target_assets_missing");
        std::fs::create_dir_all(root.join("build/server")).unwrap();
        let mut server = SubProject::default();
        server.build_dir = Some("build/server".to_string());
        server.assets = vec![("public".to_string(), "public".to_string())];
        let mut info = ProjectInfo::default();
        info.sub_projects
            .insert("server".to_string(), server.clone());

        // No `public/` directory exists. We expect a stderr warning,
        // not a panic — a missing optional asset shouldn't take down
        // the build.
        copy_target_assets("server", &server, &info, &root);
    }

    #[test]
    fn test_parser_planner_assets_e2e() {
        // End-to-end: parse a real fai.toml, plan the build order
        // from it, and run copy_target_assets in dep order. Verifies
        // the three new pieces (parser additions, planner, asset
        // copier) compose correctly when fed by the same `ProjectInfo`
        // they share at runtime — the integration `step_build` does
        // for real but without invoking the wasm compiler.
        let root = temp_dir("e2e_pipeline");
        let toml = concat!(
            "[project]\n",
            "name = \"e2eapp\"\n",
            "\n",
            "[project.web]\n",
            "target = \"wasm-html\"\n",
            "build_dir = \"build/web\"\n",
            "\n",
            "[project.server]\n",
            "target = \"native\"\n",
            "build_dir = \"build/server\"\n",
            "required_targets = [\"web\"]\n",
            "\n",
            "[project.server.assets]\n",
            "\"$web\" = \"public\"\n",
            "\"public\" = \"public\"\n",
        );
        std::fs::write(root.join("fai.toml"), toml).unwrap();
        // Pretend the web build has already deposited its artifacts.
        // build_one_subproject would do this via cmd_build; the
        // planner / asset-copy stages don't care how the bytes got
        // there, only that they exist when the dependent target
        // tries to copy them.
        std::fs::create_dir_all(root.join("build/web")).unwrap();
        std::fs::write(root.join("build/web/web.wasm"), "wasm").unwrap();
        std::fs::write(root.join("build/web/fai-runtime.js"), "js").unwrap();
        std::fs::create_dir_all(root.join("public")).unwrap();
        std::fs::write(root.join("public/favicon.ico"), "icon").unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();

        let info = parse_project_info(toml);
        // Planner: dep order is web → server.
        let order = plan_build_order(&info, Some("server")).unwrap();
        assert_eq!(order, vec!["web".to_string(), "server".to_string()]);

        // Run asset copy in planned order. Web has no assets so this
        // is a no-op; server merges $web + public into build/server/public/.
        for name in &order {
            let sub = info.sub_projects.get(name).unwrap();
            copy_target_assets(name, sub, &info, &root);
        }

        let merged = root.join("build/server/public");
        assert!(merged.join("web.wasm").exists());
        assert!(merged.join("fai-runtime.js").exists());
        assert!(merged.join("favicon.ico").exists());
    }

    #[test]
    fn test_resolve_target_wasm_artifact_returns_path_when_present() {
        // Cargo runs tests in parallel and `current_dir` is
        // process-global, so any test that mutates cwd must hold the
        // shared lock for the duration of its cwd window.
        let _guard = cwd_test_lock();
        let root = temp_dir("resolve_artifact_present");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"app\"\n\n[project.server]\nbuild_dir = \"build/server\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("build/server")).unwrap();
        std::fs::write(root.join("build/server/server.wasm"), "wasm").unwrap();

        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let resolved = resolve_target_wasm_artifact(Some("server"));
        std::env::set_current_dir(&original).unwrap();

        let path = resolved.expect("artifact resolves when present");
        assert!(path.ends_with("build/server/server.wasm"), "got {}", path);
    }

    #[test]
    fn test_resolve_target_wasm_artifact_returns_none_when_not_built() {
        let _guard = cwd_test_lock();
        let root = temp_dir("resolve_artifact_missing");
        std::fs::write(
            root.join("fai.toml"),
            "[project]\nname = \"app\"\n\n[project.server]\nbuild_dir = \"build/server\"\n",
        )
        .unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let resolved = resolve_target_wasm_artifact(Some("server"));
        std::env::set_current_dir(&original).unwrap();
        assert!(resolved.is_none());
    }

    #[test]
    fn test_parse_required_targets_and_assets() {
        let toml = concat!(
            "[project]\n",
            "name = \"app\"\n",
            "\n",
            "[project.web]\n",
            "target = \"wasm-html\"\n",
            "build_dir = \"build/web\"\n",
            "\n",
            "[project.server]\n",
            "target = \"native\"\n",
            "build_dir = \"build/server\"\n",
            "required_targets = [\"web\"]\n",
            "\n",
            "[project.server.assets]\n",
            "\"$web\" = \"public\"\n",
            "\"public\" = \"public\"\n",
        );
        let info = parse_project_info(toml);
        let server = info
            .sub_projects
            .get("server")
            .expect("server target parsed");
        assert_eq!(server.required_targets, vec!["web".to_string()]);
        assert_eq!(
            server.assets,
            vec![
                ("$web".to_string(), "public".to_string()),
                ("public".to_string(), "public".to_string()),
            ]
        );
        // The web target has no required_targets / assets — just here
        // to confirm the parser keeps them empty rather than borrowing
        // them from a sibling section.
        let web = info.sub_projects.get("web").expect("web target parsed");
        assert!(web.required_targets.is_empty());
        assert!(web.assets.is_empty());
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("fai_cli_test_{}", tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // ── Plan 116 phase 2: watchdog + post-mortem dump ──

    /// Compile a source string written to `<temp>/main.fai`.
    fn compile_snippet(tag: &str, src: &str) -> Vec<u8> {
        let dir = temp_dir(tag);
        let path = dir.join("main.fai");
        std::fs::write(&path, src).unwrap();
        compile_fai_to_wasm(src, path.to_str().unwrap(), false, Vec::new(), None, None)
    }

    #[test]
    fn watchdog_dump_names_the_waiting_tasks() {
        // `main` awaits `never`, which parks on a 60s timer in a loop —
        // the run can't complete. The watchdog must kill it and the
        // post-mortem dump must name the parked task and its waiter.
        let wasm = compile_snippet(
            "watchdog_dump",
            concat!(
                "# Parks forever: sleeps in a loop and never returns.\n",
                "def never\n",
                "    @return Int\n",
                "do\n",
                "    while true\n",
                "        sleep(60000)\n",
                "    end\n",
                "    return 1\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let x = never()\n",
                "    print(x)\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                watchdog_secs: Some(1),
                ..Default::default()
            },
        )
        .expect_err("watchdog should kill the parked program");
        // Two watchdog paths can fire first: the elapsed check between
        // polls ("no completion after Ns") or the epoch interrupt
        // landing mid-poll ("still running after Ns — interrupted").
        // Either way the dump must name the parked task and its waiter.
        assert!(err.contains("watchdog"), "{err}");
        assert!(err.contains("never#resume"), "{err}");
        assert!(err.contains("WAITING"), "{err}");
        assert!(err.contains("t1"), "{err}");
    }

    #[test]
    fn watchdog_interrupts_a_sync_infinite_loop() {
        // A sync `while true` never reaches a host call, so only epoch
        // interruption can break it. The report carries the watchdog
        // reason plus the post-mortem heap stats.
        let wasm = compile_snippet(
            "watchdog_spin",
            concat!(
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while true\n",
                "        i = i + 1\n",
                "        if i > 1000000\n",
                "            i = 0\n",
                "        end\n",
                "    end\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                watchdog_secs: Some(1),
                ..Default::default()
            },
        )
        .expect_err("watchdog should interrupt the spinning program");
        assert!(
            err.contains("watchdog: still running after 1s — interrupted"),
            "{err}",
        );
        assert!(err.contains("post-mortem:"), "{err}");
    }

    #[test]
    fn trap_in_async_run_includes_post_mortem_task_table() {
        // A trap inside an async program appends the task-table dump to
        // the decorated backtrace — no watchdog involved.
        let wasm = compile_snippet(
            "trap_post_mortem",
            concat!(
                "# Sleeps then unwraps null.\n",
                "def crashLater\n",
                "    @return Int\n",
                "do\n",
                "    var x Int? = null\n",
                "    return x!\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    sleep(5)\n",
                "    let v = crashLater()\n",
                "    print(v)\n",
                "end\n",
            ),
        );
        let err = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions::default(),
        )
        .expect_err("force-unwrap of null should trap");
        assert!(err.contains("force-unwrap"), "{err}");
        // The frame carries the (temp-dir-qualified) file and line.
        assert!(err.contains("crashLater ("), "{err}");
        assert!(err.contains("main.fai:"), "{err}");
        assert!(err.contains("post-mortem:"), "{err}");
        assert!(err.contains("main#resume"), "{err}");
    }

    // ── Plan 116 phase 5: `--check-leaks` heap allocation ledger ──

    /// Run a `--check-leaks` build and return the captured stderr
    /// (where the ledger report lands). Codegen instrumentation comes
    /// from the thread-local guard; compile and run share this thread.
    fn run_with_check_leaks(tag: &str, src: &str) -> String {
        let _cg = fai_codegen_wasm::CheckLeaksGuard::new();
        let wasm = compile_snippet(tag, src);
        let guard = wasm_runner::output::CaptureGuard::new();
        let result = wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        );
        let stderr = guard.stderr();
        drop(guard);
        result.expect("check-leaks program should run to completion");
        stderr
    }

    #[test]
    fn check_leaks_report_names_the_leaking_function() {
        // Ten same-size strings escape into a program-lifetime global —
        // the live set at exit. Tier 1: the report shows the group with
        // its count; Tier 2a: the allocation site names `makeLeak` (via
        // the backtrace captured at each rt_alloc). The self-check
        // against `__live_objects` must agree.
        let stderr = run_with_check_leaks(
            "check_leaks_named",
            concat!(
                "use std.array\n",
                "\n",
                "var cache String[] = []\n",
                "\n",
                "# Allocates strings that stay referenced by the global cache.\n",
                "def makeLeak\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 10\n",
                "        cache = array.append(cache, 'leak-string-payload-' + toString(i))\n",
                "        i = i + 1\n",
                "    end\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    makeLeak()\n",
                "    print(length(cache))\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        // Tier 1: ten leaked strings of one size, grouped.
        assert!(stderr.contains("\n     10 "), "{stderr}");
        assert!(stderr.contains("String"), "{stderr}");
        // Tier 2a: the allocation site names the leaking function.
        assert!(stderr.contains("makeLeak"), "{stderr}");
        // Self-check: ledger and __live_objects agree.
        assert!(stderr.contains("consistent"), "{stderr}");
    }

    #[test]
    fn check_leaks_clean_program_reports_empty_live_set() {
        // A loop that builds and drops temporaries must come back to an
        // empty live set — the ledger version of the reclaim fixtures.
        let stderr = run_with_check_leaks(
            "check_leaks_clean",
            concat!(
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 200\n",
                "        let label = 'item-' + toString(i)\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(
            stderr.contains("live heap: 0 objects, 0 bytes"),
            "{stderr}",
        );
        assert!(stderr.contains("consistent"), "{stderr}");
    }

    #[test]
    fn check_leaks_async_loop_bindings_are_clean() {
        // Regression for the async-frame loop leak (the brain SSR
        // ~15KB/request): a suspending loop's `let`, its awaited call
        // result, and a `html = html + part` accumulator must all be
        // reclaimed per iteration — the live set at exit contains no
        // per-iteration strings. (The one allowed survivor is the
        // scheduler's one-time startup allocation.)
        let stderr = run_with_check_leaks(
            "check_leaks_async_loop",
            concat!(
                "# Returns a fresh heap string after suspending.\n",
                "def apiece\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    sleep(0)\n",
                "    'piece-' + toString(i)\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var html = ''\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        let part = apiece(i)\n",
                "        html = html + part\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(length(html))\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-iteration leak groups: neither the awaited results nor
        // the accumulator intermediates survive to the exit report.
        assert!(!stderr.contains("apiece"), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
    }

    #[test]
    fn check_leaks_module_peer_call_results_are_clean() {
        // Regression: RC ownership classification must resolve module-peer
        // calls (`piece(i)` inside module `rend` → `rend.piece`) exactly the
        // way `compile_call` resolves them. Misclassified as borrowed, every
        // peer-call result is over-retained on bind / skipped by operand
        // mop-up and leaks once per call — the sync half of the brain SSR
        // per-request leak (plan 116).
        let dir = temp_dir("check_leaks_module_peer");
        // Module discovery roots at fai.toml's source_root.
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"modpeer\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src/rend")).unwrap();
        std::fs::write(
            dir.join("src/rend/rend.fai"),
            concat!(
                "# Returns a fresh concat string.\n",
                "def piece\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    'piece-' + toString(i)\n",
                "end\n",
                "\n",
                "# Wraps a peer-call result (one more sync call level).\n",
                "def wrap\n",
                "    @param i Int\n",
                "    @return String\n",
                "do\n",
                "    let inner = piece(i)\n",
                "    '<' + inner + '>'\n",
                "end\n",
                "\n",
                "# Accumulates peer-call results in a loop.\n",
                "def buildAll\n",
                "    @return Int\n",
                "do\n",
                "    var html = ''\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        html = html + wrap(i)\n",
                "        i = i + 1\n",
                "    end\n",
                "    length(html)\n",
                "end\n",
            ),
        )
        .unwrap();
        let main_src = concat!(
            "use { buildAll } from rend\n",
            "\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "    print(buildAll())\n",
            "end\n",
        );
        let path = dir.join("src/main.fai");
        std::fs::write(&path, main_src).unwrap();
        let _cg = fai_codegen_wasm::CheckLeaksGuard::new();
        let wasm =
            compile_fai_to_wasm(main_src, path.to_str().unwrap(), false, Vec::new(), None, None);
        let guard = wasm_runner::output::CaptureGuard::new();
        wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        )
        .expect("module program should run");
        let stderr = guard.stderr();
        drop(guard);
        assert!(stderr.contains("consistent"), "{stderr}");
        // No leak group may name the module's functions — every peer-call
        // result (piece's string, wrap's string) is reclaimed.
        assert!(!stderr.contains("rend."), "{stderr}");
    }

    #[test]
    fn check_leaks_std_host_call_results_are_clean() {
        // Regression (plan 116 host-leak pass): std host calls returning
        // fresh object graphs (json.parse/stringify, file.read, env.get)
        // were classified borrowed — over-retained on bind, one leaked
        // graph per call — and file.read leaked its 64 KiB scratch buffer
        // plus an owned literal path temp per call. All must come back to
        // an empty live set.
        std::fs::write("/tmp/fai_check_leaks_std.txt", "file-content-here").unwrap();
        let stderr = run_with_check_leaks(
            "check_leaks_std_host",
            concat!(
                "use std.env\n",
                "\n",
                "use std.file\n",
                "\n",
                "use std.json\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var i = 0\n",
                "    while i < 20\n",
                "        let v = json.parse('{\"k\": [1, 2, 3], \"s\": \"hello\"}')\n",
                "        let s = json.stringify(v)\n",
                "        let f = file.read('/tmp/fai_check_leaks_std.txt')\n",
                "        let e = env.get('HOME')\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(
            stderr.contains("live heap: 0 objects, 0 bytes"),
            "{stderr}",
        );
        assert!(stderr.contains("consistent"), "{stderr}");
    }

    #[test]
    fn check_leaks_event_dispatch_is_clean() {
        // Regression (plan 116 host-leak pass): every event dispatch leaked
        // its host-built Event{name,data} dict — `build_event` now retains
        // the data and `dispatch_event` releases the event after the
        // subscribers run. Only the one-time subscription survives.
        let stderr = run_with_check_leaks(
            "check_leaks_events",
            concat!(
                "use std.events\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let _sub = events.on('tick') do with e Event\n",
                "        let n = e.name\n",
                "    end\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        events.emit('tick', 'payload-' + toString(i))\n",
                "        i = i + 1\n",
                "    end\n",
                "    print('done')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-dispatch group: 30 leaked events would show as a
        // count-30 line.
        assert!(!stderr.contains("\n     30 "), "{stderr}");
    }

    #[test]
    fn check_leaks_from_dict_binding_is_clean() {
        // Regression (plan 116 host-leak pass): `let x T = from_dict(d)`
        // bound its fresh record without note_droppable — one leaked
        // record per call (brain's beforeRequest listener built one per
        // request, pinning request sub-dicts with it).
        let stderr = run_with_check_leaks(
            "check_leaks_from_dict",
            concat!(
                "type Point\n",
                "    x Int\n",
                "    y Int\n",
                "    label String\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let src = { x: 1 y: 2 label: 'origin' }\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        let p Point = from_dict(src)\n",
                "        total = total + p.x\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
    }

    #[test]
    fn check_leaks_cells_and_async_args_are_clean() {
        // Regression (plan 114 cell unification): captured-mutated vars
        // (cells), their value chains, the closures that capture them,
        // and owned arguments passed to async calls must all reclaim —
        // including a closure that ESCAPES its task and is called after
        // the task completed (the cell outlives the reclaimed frame).
        let stderr = run_with_check_leaks(
            "check_leaks_cells",
            concat!(
                "type def Thunk\n",
                "    @return Void\n",
                "end\n",
                "\n",
                "# Async fn taking a heap arg (param slot owns +1).\n",
                "def measure\n",
                "    @param s String\n",
                "    @return Int\n",
                "do\n",
                "    sleep(0)\n",
                "    length(s)\n",
                "end\n",
                "\n",
                "# Mutates captured cells across suspensions.\n",
                "def runOnce\n",
                "    @param i Int\n",
                "    @return Int\n",
                "do\n",
                "    var acc = ''\n",
                "    let bump = do\n",
                "        acc = acc + 'x'\n",
                "    end\n",
                "    bump()\n",
                "    sleep(0)\n",
                "    bump()\n",
                "    length(acc) + measure('fresh-' + toString(i))\n",
                "end\n",
                "\n",
                "# Returns a closure over a cell; called after the task completes.\n",
                "def makeEscaped\n",
                "    @return Thunk\n",
                "do\n",
                "    var stash = 'payload'\n",
                "    sleep(0)\n",
                "    let esc = do\n",
                "        stash = stash + '!'\n",
                "    end\n",
                "    esc\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        total = total + runOnce(i)\n",
                "        let escaped = makeEscaped()\n",
                "        escaped()\n",
                "        escaped()\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        // No per-iteration groups: cells, closures, args all reclaimed.
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("\n     60 "), "{stderr}");
        assert!(!stderr.contains("Cell"), "{stderr}");
    }

    #[test]
    fn check_leaks_fn_refs_and_tostring_owned_args_are_clean() {
        // Regression (plan 114 tail — brain's last 2 objects/request):
        // (a) a function REFERENCE used as a value compiles to a fresh
        // closure wrapper per use and must transfer ownership (it was
        // classified borrowed and the wrapper leaked once per use);
        // (b) `toString(<owned call result>)` must release its arg temp
        // (the alias-retain made the result +1 but never consumed the
        // owned arg, leaking one copy per call — `toString(s.value())`).
        let stderr = run_with_check_leaks(
            "check_leaks_fnref_tostring",
            concat!(
                "type def Producer\n",
                "    @return Int\n",
                "end\n",
                "\n",
                "# Returns a constant.\n",
                "def piece\n",
                "    @return Int\n",
                "do\n",
                "    7\n",
                "end\n",
                "\n",
                "# Async fn calling a closure-typed param.\n",
                "def callIt\n",
                "    @param f Producer\n",
                "    @return Int\n",
                "do\n",
                "    sleep(0)\n",
                "    f()\n",
                "end\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    let base = 'value-string'\n",
                "    var total = 0\n",
                "    var i = 0\n",
                "    while i < 30\n",
                "        total = total + callIt(piece)\n",
                "        let s = toString(copy(base))\n",
                "        i = i + 1\n",
                "    end\n",
                "    print(total)\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(!stderr.contains("\n     30 "), "{stderr}");
        assert!(!stderr.contains("\n     29 "), "{stderr}");
        assert!(!stderr.contains("Closure"), "{stderr}");
    }

    #[test]
    fn check_leaks_accounts_for_host_side_allocations() {
        // Host-built objects (json.parse builds the value graph via the
        // host `reserve`, not `rt_alloc`) are recorded with host origin:
        // the self-check offsets them, and the report attributes them.
        let stderr = run_with_check_leaks(
            "check_leaks_host",
            concat!(
                "use std.json\n",
                "\n",
                "var keep = json.parse('{\"name\": \"hello\", \"xs\": [1, 2]}')\n",
                "\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "    print('ok')\n",
                "end\n",
            ),
        );
        assert!(stderr.contains("[check-leaks] live heap:"), "{stderr}");
        assert!(stderr.contains("host-side"), "{stderr}");
        assert!(stderr.contains("consistent"), "{stderr}");
        assert!(stderr.contains("host import"), "{stderr}");
    }

    #[test]
    fn check_leaks_on_uninstrumented_module_reports_hint() {
        // A module compiled WITHOUT the flag emits no events; running it
        // with --check-leaks must say so instead of claiming "no leaks".
        let wasm = compile_snippet(
            "check_leaks_uninstrumented",
            "def main\n    @return Void\ndo\n    print('hi')\nend\n",
        );
        let guard = wasm_runner::output::CaptureGuard::new();
        wasm_runner::run_wasm_with_externs_opts(
            &wasm,
            Vec::new(),
            wasm_runner::RunOptions {
                check_leaks: Some(wasm_runner::CheckLeaksOptions::default()),
                ..Default::default()
            },
        )
        .expect("program should run");
        let stderr = guard.stderr();
        drop(guard);
        assert!(
            stderr.contains("not built with --check-leaks"),
            "{stderr}",
        );
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
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nWidgetPkg = \"file://{}\"\n",
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
                "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nWidgetPkg = \"file://{}\"\n",
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

    #[test]
    fn test_run_project_check_catches_errors_in_multi_target_nested_src() {
        // Regression test: a multi-target project (fai.toml has
        // [project.<name>] sub-projects) with a nested-only src/ —
        // i.e. no top-level .fai files, only subdirectories like
        // `auth/`, `data/`, `pages/` — used to silently pass
        // `fai check`. The bug: step_check fell through to its
        // flat-library mode, which builds a synthetic entry from
        // top-level `use` lines. With no top-level files those lines
        // are empty, no modules get discovered, and check_with_modules
        // walks nothing.
        //
        // The fix: detect [project.<name>] sub-projects in
        // run_project_check and dispatch to per-target check, the
        // same way step_test already does for tests.
        let proj = temp_dir("multi_target_nested_check");
        std::fs::create_dir_all(proj.join("src/auth")).unwrap();
        std::fs::create_dir_all(proj.join("src/platforms/server")).unwrap();
        std::fs::write(
            proj.join("fai.toml"),
            "[project]\nname = \"App\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\
             \n[project.server]\ntarget = \"native\"\nsource = \"src\"\n\
             main = \"src/platforms/server/main.fai\"\n\
             build_dir = \"build/server\"\n",
        )
        .unwrap();

        // Nested file with a deliberate doc-comment violation on a
        // public function. Doc comments are required language-wide
        // and `fai check` must surface this.
        std::fs::write(
            proj.join("src/auth/login.fai"),
            "def login\n    @return Bool\ndo\n  true\nend\n",
        )
        .unwrap();

        // Server entry that imports from auth.
        std::fs::write(
            proj.join("src/platforms/server/main.fai"),
            "use { login } from auth\n\n\
             def main\n    @return Void\ndo\n  let _ = login()\nend\n",
        )
        .unwrap();

        let result = run_project_check(&proj, "src");
        assert!(
            result.is_err(),
            "fai check should fail on doc-comment violation in a nested src/ \
             of a multi-target project, got Ok"
        );
        let (msg, _count) = result.unwrap_err();
        assert!(
            msg.contains("doc comment") && msg.contains("login"),
            "error should report the missing doc comment on `login`, got:\n{}",
            msg
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
        assert!(html.contains("_start_async"));
        assert!(html.contains("__fai_poll"));
        assert!(html.contains("__fai_task_result"));
        assert!(html.contains("__fai_resume_task"));
        assert!(html.contains("pumpAsync()"));
        assert!(html.contains("startFai()"));
    }

    #[test]
    fn test_generate_html_loader_old_contains_filename() {
        let html = generate_html_loader_old("bundle.wasm");
        assert!(html.contains("bundle.wasm"));
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("WebAssembly"));
        assert!(html.contains("_start_async"));
        assert!(html.contains("__fai_poll"));
        assert!(html.contains("__fai_task_result"));
        assert!(html.contains("__fai_resume_task"));
        assert!(html.contains("pumpAsync()"));
        assert!(html.contains("startFai()"));
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
        // No fai.toml + non-`.fai` extension. The naming policy strips
        // the extension via Path::file_stem, so prog.txt builds to
        // prog.wasm. (Previously the policy preserved the full filename
        // and produced prog.txt.wasm — that was an artefact of the old
        // strip-suffix branch and didn't compose with the new
        // project-name-driven naming.)
        let dir = temp_dir("cmd_build_txt");
        let txt_path = dir.join("prog.txt");
        std::fs::write(&txt_path, SIMPLE_FAI).unwrap();

        let args: Vec<String> = vec![txt_path.to_str().unwrap().to_string()];
        cmd_build(&args);

        assert!(dir.join("prog.wasm").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_runtime_js_exposes_view_event_bridges() {
        // The forui view layer wires DOM events through forai's std.events:
        // the generated inline `onclick`/`oninput`/`onkeydown` handlers call
        // `handleEvent` / `handleInputEvent` / `handleSubmitEvent`, which emit
        // `view:click`, `view:input`, `view:submit` topics. Forui subscribes
        // to those topics and runs the registered closures.
        let js = generate_runtime_js("prog.wasm");
        assert!(js.contains("function handleEvent"));
        assert!(js.contains("function handleInputEvent"));
        assert!(js.contains("function handleSubmitEvent"));
        assert!(
            js.contains("'view:click'"),
            "handleEvent should emit on view:click:\n{}",
            js
        );
        assert!(
            js.contains("'view:input'"),
            "handleInputEvent should emit on view:input:\n{}",
            js
        );
        assert!(
            js.contains("'view:submit'"),
            "handleSubmitEvent should emit on view:submit:\n{}",
            js
        );
        // The event_* env imports must be live, not stubs — std.events
        // is implemented host-side, including in the browser.
        assert!(js.contains("faiEventRegistry"));
        assert!(js.contains("faiInvokeClosure"));
        assert!(js.contains("_start_async"));
        assert!(js.contains("__fai_poll"));
        assert!(js.contains("__fai_task_result"));
        assert!(js.contains("__fai_resume_task"));
        assert!(js.contains("pumpAsync()"));
        assert!(js.contains("startFai()"));
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
        assert!(public.join("Test.wasm").exists());
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
        assert!(public.join("Test.wasm").exists());
        assert!(public.join("index.html").exists());
        assert!(public.join("fai-runtime.js").exists());
        assert!(public.join("forui.css").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cmd_build_schema_excludes_unreachable_remote_defs() {
        let dir = temp_dir("cmd_build_rpc_reachable_schema");
        let src = dir.join("src");
        let forui_pkg = dir.join("forui");
        std::fs::create_dir_all(src.join("platforms/server")).unwrap();
        std::fs::create_dir_all(src.join("data/tasks")).unwrap();
        std::fs::create_dir_all(src.join("data/admin")).unwrap();
        std::fs::create_dir_all(forui_pkg.join("src/rpc")).unwrap();

        std::fs::write(
            forui_pkg.join("fai.toml"),
            "[project]\nname = \"Forui\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(
            forui_pkg.join("src/rpc/main.fai"),
            concat!(
                "use std.http.server\n\n",
                "# Handles generated RPC requests.\n",
                "def handleRpcRequest\n",
                "    @param request HttpRequest\n",
                "    @param specJson String\n",
                "    @param specHash String\n",
                "    @param dispatch (String, String) -> String\n",
                "    @return HttpResponse\n",
                "do\n",
                "  server.json(200, '{}')\n",
                "end\n",
            ),
        )
        .unwrap();

        std::fs::write(
            dir.join("fai.toml"),
            format!(
                "[project]\nname = \"RpcReachable\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n[dependencies]\nForui = \"file://{}\"\n",
                forui_pkg.display()
            ),
        )
        .unwrap();
        let server_main = src.join("platforms/server/main.fai");
        std::fs::write(
            &server_main,
            concat!(
                "use std.http.server\n",
                "use { getTasks } from data.tasks\n\n",
                "def main\n",
                "    @return Void\n",
                "do\n",
                "  let r = server.router()\n",
                "  addRpcRoutes(r)\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/tasks/main.fai"),
            concat!(
                "# Gets reachable tasks.\n",
                "remote def getTasks\n",
                "    @return String[]\n",
                "do\n",
                "  []\n",
                "end\n",
            ),
        )
        .unwrap();
        std::fs::write(
            src.join("data/admin/main.fai"),
            concat!(
                "# Dangerous endpoint that must stay unexposed.\n",
                "remote def deleteEverything\n",
                "    @return String\n",
                "do\n",
                "  'nope'\n",
                "end\n",
            ),
        )
        .unwrap();

        let out_path = dir.join("build/server/main.wasm");
        std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
        let args: Vec<String> = vec![
            server_main.to_string_lossy().into_owned(),
            "-o".to_string(),
            out_path.to_string_lossy().into_owned(),
        ];
        cmd_build(&args);

        let schema = std::fs::read_to_string(dir.join("build/server/schema.json"))
            .expect("server build should write schema.json");
        assert!(
            schema.contains("\"key\": \"data.tasks.getTasks\""),
            "schema should expose imported remote def. Got:\n{}",
            schema
        );
        assert!(
            !schema.contains("deleteEverything"),
            "schema should not expose unimported remote def. Got:\n{}",
            schema
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_rpc_test_stub_is_private() {
        let mut content = concat!(
            "use std.http.server\n\n",
            "def main\n",
            "    @return Void\n",
            "do\n",
            "  let r = server.router()\n",
            "  addRpcRoutes(r)\n",
            "end\n",
        )
        .to_string();
        inject_rpc_test_stub(&mut content);
        let parsed = fai_parser::parse(&content).expect("stub should parse");
        let stub = parsed
            .statements
            .iter()
            .find_map(|stmt| match stmt {
                fai_parser::ast::Statement::Function(fd) if fd.name == "addRpcRoutes" => Some(fd),
                _ => None,
            })
            .expect("stub should be injected");
        assert!(stub.is_private, "test stub should not require coverage");
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

        // Native binary is named after [project].name ("NativeTest"),
        // not the source file's stem.
        let native = src.join("NativeTest");
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
        // Filename comes from each member's [project].name, not from
        // the source file's stem.
        assert!(
            a_src.join("PkgA.wasm").exists(),
            "pkg_a PkgA.wasm should exist, dir contents: {:?}",
            std::fs::read_dir(&a_src).unwrap().collect::<Vec<_>>()
        );
        assert!(
            b_src.join("PkgB.wasm").exists(),
            "pkg_b PkgB.wasm should exist, dir contents: {:?}",
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

        let client_out = dir.join("build/client/client.wasm");
        let server_out = dir.join("build/server/server.wasm");
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

    #[test]
    fn test_cmd_new_with_local_template() {
        let base = temp_dir("cmd_new_local_tpl");
        let tpl = base.join("tpl");

        // Stand up a tiny template fixture
        std::fs::create_dir_all(tpl.join("src/pages")).unwrap();
        std::fs::write(
            tpl.join("fai.toml"),
            "[project]\nname = \"TplName\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("src/pages/home.fai"),
            "def HomePage\n  @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(tpl.join("README.md"), "# tpl readme\n").unwrap();

        let project_path = base.join("scaffolded-app");
        let args: Vec<String> = vec![
            project_path.to_str().unwrap().to_string(),
            "--template".to_string(),
            tpl.to_str().unwrap().to_string(),
        ];
        cmd_new(&args);

        // Template files copied verbatim
        assert!(project_path.join("src/pages/home.fai").exists());
        assert!(project_path.join("README.md").exists());
        // Project name substituted in fai.toml
        let toml = std::fs::read_to_string(project_path.join("fai.toml")).unwrap();
        assert!(
            toml.contains("name = \"scaffolded-app\""),
            "fai.toml should carry the new project name, got:\n{}",
            toml
        );
        assert!(
            !toml.contains("TplName"),
            "old name should be gone from fai.toml, got:\n{}",
            toml
        );

        // Meta files (language reference, AI guidance, MCP config) are
        // overlaid by `fai new` regardless of template — they're
        // language-level concerns, not project-shape.
        assert!(project_path.join("CLAUDE.md").exists());
        assert!(project_path.join("AGENTS.md").exists());
        assert!(project_path.join("language.md").exists());
        assert!(project_path.join(".mcp.json").exists());
        assert!(project_path.join(".codex/config.toml").exists());

        // The auto-overlaid CLAUDE.md should reference the new project
        // name, not the template's source name.
        let claude = std::fs::read_to_string(project_path.join("CLAUDE.md")).unwrap();
        assert!(
            claude.starts_with("# scaffolded-app\n"),
            "CLAUDE.md heading should use the new project name, got:\n{}",
            &claude[..claude.len().min(80)]
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_cmd_new_appends_template_meta_to_scaffold() {
        // A template that ships its own CLAUDE.md / AGENTS.md (forui-
        // specific guidance, say) gets appended below the language-
        // level scaffold so both are visible. Language-level rules
        // (doc comments, testing) come first; project-specific
        // guidance follows under a separator.
        let base = temp_dir("cmd_new_meta_append");
        let tpl = base.join("tpl");
        std::fs::create_dir_all(tpl.join("src")).unwrap();
        std::fs::write(tpl.join("fai.toml"), "[project]\nname = \"X\"\n").unwrap();
        std::fs::write(
            tpl.join("src/main.fai"),
            "def main\n  @return Void\ndo\nend\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("CLAUDE.md"),
            "# Custom guidance\n\nTemplate-owned.\n",
        )
        .unwrap();
        std::fs::write(
            tpl.join("AGENTS.md"),
            "# Custom AGENTS\n\nTemplate-agents.\n",
        )
        .unwrap();

        let project_path = base.join("app");
        cmd_new(&[
            project_path.to_str().unwrap().to_string(),
            "--template".to_string(),
            tpl.to_str().unwrap().to_string(),
        ]);

        let claude = std::fs::read_to_string(project_path.join("CLAUDE.md")).unwrap();
        assert!(
            claude.contains("Template-owned"),
            "template-supplied CLAUDE.md content should be preserved, got:\n{}",
            claude
        );
        assert!(
            claude.contains("Project-specific guidance"),
            "merged CLAUDE.md should carry the separator header, got:\n{}",
            claude
        );
        assert!(
            claude.find("Template-owned").unwrap()
                > claude.find("Project-specific guidance").unwrap(),
            "scaffold should come first, template second:\n{}",
            claude
        );

        let agents = std::fs::read_to_string(project_path.join("AGENTS.md")).unwrap();
        assert!(
            agents.contains("Template-agents") && agents.contains("doc comment required"),
            "merged AGENTS.md should carry both scaffold (doc-comment rule) and template content, got:\n{}",
            agents
        );

        // Meta files the template *didn't* ship still get filled in.
        assert!(project_path.join("language.md").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn overlay_meta_writes_files_when_absent() {
        let base = temp_dir("overlay_writes");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        overlay_meta_files(&dir, "my-app");
        assert!(dir.join("language.md").exists());
        assert!(dir.join("CLAUDE.md").exists());
        assert!(dir.join("AGENTS.md").exists());
        assert!(dir.join(".mcp.json").exists());
        assert!(dir.join(".codex/config.toml").exists());
    }

    #[test]
    fn overlay_meta_appends_template_content_to_scaffold() {
        // Existing CLAUDE.md / AGENTS.md gets appended below the
        // scaffold rather than replaced. Other meta files (e.g.
        // .mcp.json) still use last-write-wins.
        let base = temp_dir("overlay_appends");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "OWNED").unwrap();
        overlay_meta_files(&dir, "my-app");
        let merged = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(
            merged.contains("OWNED"),
            "template content kept: {}",
            merged
        );
        assert!(
            merged.contains("Project-specific guidance"),
            "separator header present: {}",
            merged
        );
        assert!(
            merged.find("OWNED").unwrap() > merged.find("Project-specific").unwrap(),
            "scaffold first, template second: {}",
            merged
        );
    }

    #[test]
    fn overlay_meta_preserves_non_md_files() {
        // .mcp.json, .codex/config.toml, language.md keep last-write-wins
        // semantics — appending doesn't make sense for structured files.
        let base = temp_dir("overlay_preserves_structured");
        let dir = base.join("p");
        std::fs::create_dir_all(dir.join(".codex")).unwrap();
        std::fs::write(dir.join(".mcp.json"), "{\"custom\":true}").unwrap();
        overlay_meta_files(&dir, "my-app");
        assert_eq!(
            std::fs::read_to_string(dir.join(".mcp.json")).unwrap(),
            "{\"custom\":true}"
        );
    }

    #[test]
    fn overlay_meta_interpolates_project_name() {
        let base = temp_dir("overlay_name");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        overlay_meta_files(&dir, "fancy-app");
        let claude = std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap();
        assert!(claude.starts_with("# fancy-app\n"));
    }

    #[test]
    fn overlay_meta_creates_codex_dir_if_missing() {
        let base = temp_dir("overlay_codex");
        let dir = base.join("p");
        std::fs::create_dir_all(&dir).unwrap();
        // .codex doesn't exist yet
        overlay_meta_files(&dir, "p");
        assert!(dir.join(".codex").is_dir());
        assert!(dir.join(".codex/config.toml").exists());
    }

    // ── artifact_filename / sub_project_output_path helpers ─────────

    #[test]
    fn artifact_filename_uses_project_name_when_set() {
        assert_eq!(
            artifact_filename("MySuperApp", "/x/y/main.fai"),
            "MySuperApp.wasm"
        );
        assert_eq!(
            artifact_filename("Forui", "/anywhere/entry.fai"),
            "Forui.wasm"
        );
    }

    #[test]
    fn artifact_filename_falls_back_to_source_stem_when_name_is_default_unknown() {
        // The parser fills `name` with "unknown" when `name = "..."`
        // is missing from [project]. That sentinel must trigger the
        // source-stem fallback so we don't ship `unknown.wasm`.
        assert_eq!(
            artifact_filename("unknown", "/x/y/myscratch.fai"),
            "myscratch.wasm"
        );
    }

    #[test]
    fn artifact_filename_falls_back_when_name_is_empty() {
        assert_eq!(artifact_filename("", "/x/main.fai"), "main.wasm");
    }

    #[test]
    fn artifact_filename_strips_extension_in_fallback() {
        // The fallback is Path::file_stem-based, so any extension is
        // stripped — not just `.fai`.
        assert_eq!(artifact_filename("", "/x/scratch.txt"), "scratch.wasm");
    }

    #[test]
    fn sub_project_output_path_uses_build_dir_when_set() {
        let tmp = temp_dir("subproj_path_with_bd");
        let entry = tmp.join("src/web/main.fai");
        let sub = SubProject {
            build_dir: Some("build/web".to_string()),
            ..SubProject::default()
        };
        let out = sub_project_output_path(&sub, &tmp, &entry, "web");
        assert_eq!(out, tmp.join("build/web/web.wasm").to_string_lossy());
        assert!(tmp.join("build/web").is_dir(), "out dir should be created");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn sub_project_output_path_falls_back_to_entry_dir_when_no_build_dir() {
        let tmp = temp_dir("subproj_path_no_bd");
        let entry_dir = tmp.join("src/server");
        std::fs::create_dir_all(&entry_dir).unwrap();
        let entry = entry_dir.join("main.fai");
        let sub = SubProject::default();
        let out = sub_project_output_path(&sub, &tmp, &entry, "server");
        assert_eq!(out, entry_dir.join("server.wasm").to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── Build artifact naming ────────────────────────────────────────
    // The .wasm filename derives from the project's `name` field, not
    // from the source file's stem. So `name = "MyApp"` always builds
    // to `MyApp.wasm` regardless of whether the entry is main.fai,
    // entry.fai, or anything else. For multi-project files, each
    // sub-project's artifact uses the sub-project key (`web`, `server`,
    // …). Source-stem naming remains as the fallback for ad-hoc
    // builds with no fai.toml or with the default `"unknown"` name.

    #[test]
    fn test_build_uses_project_name_for_wasm_artifact() {
        let dir = temp_dir("build_name_single_wasm");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"MySuperApp\"\nversion = \"0.1.0\"\nsource_root = \"src\"\nbuild_dir = \"out\"\n",
        ).unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[src.join("main.fai").to_string_lossy().into_owned()]);
        std::env::set_current_dir(&prev).unwrap();

        let named = dir.join("out/MySuperApp.wasm");
        let stem_named = dir.join("out/main.wasm");
        assert!(
            named.exists(),
            "expected MySuperApp.wasm (project name), out dir: {:?}",
            std::fs::read_dir(dir.join("out")).ok().map(|d| d
                .filter_map(|e| e.ok().map(|x| x.file_name()))
                .collect::<Vec<_>>())
        );
        assert!(
            !stem_named.exists(),
            "main.wasm should NOT exist — naming should come from project name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_html_uses_project_name() {
        let dir = temp_dir("build_name_single_html");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"BrowserApp\"\nversion = \"0.1.0\"\nsource_root = \"src\"\ntarget = \"wasm-html\"\nbuild_dir = \"public\"\n",
        ).unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[
            src.join("main.fai").to_string_lossy().into_owned(),
            "--html".to_string(),
        ]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("public/BrowserApp.wasm").exists(),
            "wasm-html build should write BrowserApp.wasm"
        );
        assert!(
            !dir.join("public/main.wasm").exists(),
            "wasm-html build should NOT write main.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_falls_back_to_source_stem_without_fai_toml() {
        // A loose .fai file with no fai.toml has no project name to
        // use. The naming policy falls back to the source stem so
        // ad-hoc `forai build foo.fai` keeps working.
        let dir = temp_dir("build_name_no_toml");
        let path = dir.join("scratch.fai");
        std::fs::write(&path, SIMPLE_FAI).unwrap();

        cmd_build(&[path.to_string_lossy().into_owned()]);

        assert!(
            dir.join("scratch.wasm").exists(),
            "no fai.toml should fall back to <source-stem>.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_falls_back_when_name_is_default_unknown() {
        // fai.toml exists but doesn't set `name` (parser leaves it as
        // the default "unknown"). The fallback should still kick in —
        // we don't want files called `unknown.wasm`.
        let dir = temp_dir("build_name_default_unknown");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nversion = \"0.1.0\"\nsource_root = \"src\"\nbuild_dir = \"out\"\n",
        )
        .unwrap();
        std::fs::write(src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[src.join("main.fai").to_string_lossy().into_owned()]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("out/main.wasm").exists(),
            "missing name should fall back to <source-stem>.wasm"
        );
        assert!(
            !dir.join("out/unknown.wasm").exists(),
            "should not produce unknown.wasm from the default name"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_sub_project_uses_sub_project_key_as_artifact_name() {
        // Multi-project: `[project.web]` and `[project.server]` should
        // produce `web.wasm` and `server.wasm` regardless of each
        // sub-project's main.fai stem.
        let dir = temp_dir("build_name_multi");
        std::fs::write(
            dir.join("fai.toml"),
            "[project]\nname = \"AppShell\"\nversion = \"0.1.0\"\nsource_root = \"src\"\n\n\
             [project.web]\ntarget = \"wasm\"\nsource = \"src/web\"\nmain = \"src/web/main.fai\"\nbuild_dir = \"build/web\"\n\n\
             [project.server]\ntarget = \"wasm\"\nsource = \"src/server\"\nmain = \"src/server/main.fai\"\nbuild_dir = \"build/server\"\n",
        ).unwrap();
        let web_src = dir.join("src/web");
        let server_src = dir.join("src/server");
        std::fs::create_dir_all(&web_src).unwrap();
        std::fs::create_dir_all(&server_src).unwrap();
        std::fs::write(web_src.join("main.fai"), SIMPLE_FAI).unwrap();
        std::fs::write(server_src.join("main.fai"), SIMPLE_FAI).unwrap();

        let _guard = cwd_test_lock();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        cmd_build(&[]);
        std::env::set_current_dir(&prev).unwrap();

        assert!(
            dir.join("build/web/web.wasm").exists(),
            "sub-project 'web' should produce web.wasm"
        );
        assert!(
            dir.join("build/server/server.wasm").exists(),
            "sub-project 'server' should produce server.wasm"
        );
        assert!(
            !dir.join("build/web/main.wasm").exists(),
            "sub-project 'web' should NOT produce main.wasm"
        );
        assert!(
            !dir.join("build/server/main.wasm").exists(),
            "sub-project 'server' should NOT produce main.wasm"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
