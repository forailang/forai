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
mod rpc_codegen;
mod rpc_surface;
mod scaffold;
mod templates;
mod test_meta;
mod wasm_runner;
mod web_assets;

use rpc_codegen::*;
use scaffold::*;
use web_assets::*;

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
    eprintln!("  test [file] [--checked] [--check-leaks] [--check-ownership]");
    eprintln!("                          fmt → check → test");
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
    eprintln!("  run --check-ownership   Record helper ownership events and print a");
    eprintln!("                          site/history balance report at exit/trap");
    eprintln!("  run --debug             Debug umbrella (currently: --watchdog)");
    eprintln!("  test --checked          Build tests with cheap always-on memory");
    eprintln!("                          guards: trap an out-of-bounds index store");
    eprintln!("                          (xs[i]=v) and any single alloc past 256 MB");
    eprintln!("                          at the source, with a named reason. Use this");
    eprintln!("                          first when a test suite corrupts the heap.");
    eprintln!("  test --check-leaks      Per-test leak assertion: a case that does not");
    eprintln!("                          return the heap to its baseline fails with a");
    eprintln!("                          delta report. --allow-leak=<suite[::case]>");
    eprintln!("                          (repeatable, exact match) exempts known leaks.");
    eprintln!("  test --check-ownership  Record helper ownership events and print a");
    eprintln!("                          site/history report; helper imbalance fails.");
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
    eprintln!("  FAI_OWNERSHIP_CHECK Helper ownership event stream (same as");
    eprintln!("                      'run/test --check-ownership').");
    eprintln!("  FAI_DEBUG_FUNCTION_CALLS");
    eprintln!("                      Trace FAI function START/END calls with timestamps.");
    eprintln!("  FAI_DEBUG_FUNCTION_CALLS_FILE=<path>");
    eprintln!("                      Append function-call trace lines to a file.");
    eprintln!("  FAI_TRACE_TESTS     Print each test case's name on stderr before it runs,");
    eprintln!("                      so a trap/hang is attributable to the exact case.");
    eprintln!("  FAI_ABI_CHECK       Compile-time only: log '[abi-check] DIVERGENCE' when");
    eprintln!("                      the plan-117 ownership signature table disagrees with");
    eprintln!("                      the legacy codegen heuristic. Never changes output.");
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
    // `fai test --check-leaks` (plan 118 U2): per-case leak assertion.
    // `fai test --check-ownership` (plan 117 phase 4): aggregate helper
    // ownership event recording for the whole test wasm run.
    // `--allow-leak=<name>` (repeatable) exempts an exact `suite` or
    // `suite::case` name — for the known host leaks (events.off/clear,
    // spy reset, ...) until plan-117 phases 4-6 fix them.
    let (args, check_leaks) = {
        let mut rest = Vec::new();
        let mut found = false;
        for a in args {
            if a == "--check-leaks" {
                found = true;
            } else {
                rest.push(a);
            }
        }
        (rest, found)
    };
    let (args, check_ownership) = {
        let mut rest = Vec::new();
        let mut found = false;
        for a in args {
            if a == "--check-ownership" {
                found = true;
            } else {
                rest.push(a);
            }
        }
        (rest, found)
    };
    let (args, allow_leaks) = {
        let mut rest = Vec::new();
        let mut allows = Vec::new();
        for a in args {
            if let Some(name) = a.strip_prefix("--allow-leak=") {
                allows.push(name.to_string());
            } else {
                rest.push(a);
            }
        }
        (rest, allows)
    };
    let test_opts = wasm_runner::TestRunOptions {
        check_leaks,
        check_ownership,
        allow_leaks,
    };
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
    if test_opts.check_leaks
        || test_opts.check_ownership
        || std::env::var_os("FAI_CHECK_LEAKS").is_some()
        || std::env::var_os("FAI_OWNERSHIP_CHECK").is_some()
    {
        fai_codegen_wasm::set_check_leaks(true);
    }
    if test_opts.check_ownership || std::env::var_os("FAI_OWNERSHIP_CHECK").is_some() {
        fai_codegen_wasm::set_ownership_check(true);
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
    step_test_with_opts(&args, &reporter, &test_opts);
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
            // Debug instrumentation flags need their codegen half armed before
            // project-mode `step_build`, where the wasm is produced.
            if args.iter().any(|a| a == "--check-ownership")
                || std::env::var_os("FAI_OWNERSHIP_CHECK").is_some()
            {
                fai_codegen_wasm::set_ownership_check(true);
                fai_codegen_wasm::set_check_leaks(true);
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
                // --check-leaks/--check-ownership) so the runner half
                // still sees them.
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
    step_test_with_opts(args, reporter, &wasm_runner::TestRunOptions::default());
}

fn step_test_with_opts(args: &[String], reporter: &Reporter, opts: &wasm_runner::TestRunOptions) {
    let file_arg = args.iter().find(|a| !a.starts_with("--"));

    if let Some(path) = file_arg {
        // Test a single file
        run_tests_file(path, reporter, opts);
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
            run_tests_file(&main_path.to_string_lossy(), reporter, opts);
        }
        return;
    }

    // Flat project (library or single-target app): load the source
    // root as one module and run every test in one wasm pass. Files
    // reference each other through normal module-mate visibility, so
    // extern blocks in `_ffi.fai`, private helpers, and public APIs
    // all resolve regardless of which file declares which.
    let src_path = project_root.join(&src_dir);
    run_tests_module(&src_path, reporter, opts);
}

/// Run every test in a flat library/app source directory as one
/// module. Mirrors the tail of `run_tests_file` but uses
/// `prepare_module_directory_for_tests` so there's no notion of an
/// "entry file" — every `.fai` file in `src_path` contributes its
/// declarations and tests to the same module.
fn run_tests_module(
    src_path: &std::path::Path,
    reporter: &Reporter,
    opts: &wasm_runner::TestRunOptions,
) {
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
        array_int_index_sites: checker.array_int_index_sites.clone(),
        record_field_read_sites: checker.record_field_read_sites.clone(),
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
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs, opts) {
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
    opts: &wasm_runner::TestRunOptions,
) -> Result<(usize, usize), String> {
    let mut current_suite: Option<String> = None;
    let mut suite_pass: u32 = 0;
    let mut suite_fail: u32 = 0;
    let mut failures: Vec<(String, String, u32, String)> = Vec::new();

    let summary =
        wasm_runner::run_wasm_tests_with_externs(wasm_bytes, tests, externs, opts, |outcome| {
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

fn run_tests_file(path: &str, reporter: &Reporter, opts: &wasm_runner::TestRunOptions) {
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
        array_int_index_sites: checker.array_int_index_sites.clone(),
        record_field_read_sites: checker.record_field_read_sites.clone(),
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
    let (passed, failed) = match run_tests_with_compact_output(&wasm_bytes, &tests, externs, opts) {
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
    let check_ownership = args.iter().any(|a| a == "--check-ownership")
        || wasm_runner::RunOptions::from_env().check_ownership;
    if check_ownership {
        fai_codegen_wasm::set_ownership_check(true);
        fai_codegen_wasm::set_check_leaks(true);
    }
    let run_opts = wasm_runner::RunOptions {
        watchdog_secs,
        check_leaks,
        check_ownership,
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
    if std::env::var_os("FAI_CHECK_LEAKS").is_some() {
        fai_codegen_wasm::set_check_leaks(true);
    }
    if std::env::var_os("FAI_OWNERSHIP_CHECK").is_some() {
        fai_codegen_wasm::set_ownership_check(true);
        fai_codegen_wasm::set_check_leaks(true);
    }
    if std::env::var_os("FAI_DEBUG_FUNCTION_CALLS").is_some() {
        fai_codegen_wasm::set_debug_function_calls(true);
    }

    // Find which sub-project's `main` matches this entry path so we
    // can read its `rpc_server` flag and remote-dependency URL. When
    // `rpc_server = false` (the default for client targets), every
    // `remote def` body is rewritten to call `remoteCall(...)` so the
    // client wasm never executes server-only code (the OOB on signup
    // in the browser was caused by the unrewritten `auth.signup`
    // dereferencing null SQLite handles). When `rpc_server = true` —
    // or when no remote URL is configured — the rewrite is skipped
    // and bodies stay intact.
    let canonical_entry = std::fs::canonicalize(&path).ok();
    let project_root_for_entry = canonical_entry
        .as_ref()
        .and_then(|entry| find_project_root(entry));
    let active_sub = {
        let canonical_entry = canonical_entry.clone();
        let project_root = project_root_for_entry.clone();
        info.sub_projects.iter().find(|(_, sub)| {
            sub.main
                .as_ref()
                .and_then(|m| {
                    let candidate = project_root
                        .as_ref()
                        .map(|root| root.join(m))
                        .unwrap_or_else(|| std::path::PathBuf::from(m));
                    std::fs::canonicalize(&candidate).ok()
                })
                .zip(canonical_entry.clone())
                .map(|(sub_main, entry)| sub_main == entry)
                .unwrap_or(false)
        })
    };
    let project_root_for_hash = project_root_for_entry.or_else(|| {
        source_root
            .as_deref()
            .and_then(|sr| find_project_root(std::path::Path::new(sr)))
    });
    if std::env::var_os("FAI_RPC_DEBUG").is_some() {
        eprintln!(
            "[rpc-proxy] entry={} source_root={:?} project_root={:?} active_target={:?} remote_deps={:?}",
            path,
            source_root,
            project_root_for_hash,
            active_sub.map(|(name, _)| name.as_str()),
            active_sub
                .map(|(_, sub)| sub.remote_deps.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        );
    }
    let rpc_proxy_substitution: Option<(String, String)> = match active_sub {
        Some((_, sub)) if !sub.rpc_server => sub.remote_deps.iter().find_map(|(dep_name, envs)| {
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
        let rewritten = rewrite_remote_def_bodies(&mut prepared.modules, url, hash);
        if std::env::var_os("FAI_RPC_DEBUG").is_some() {
            eprintln!(
                "[rpc-proxy] rewrote {} remote def bod{} for {}",
                rewritten,
                if rewritten == 1 { "y" } else { "ies" },
                path
            );
        }
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
        array_int_index_sites: checker.array_int_index_sites.clone(),
        record_field_read_sites: checker.record_field_read_sites.clone(),
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
    let mut body = match (err.line, err.col) {
        (Some(l), Some(c)) => format!("  {:?} (line {}:{})\n", err.err, l, c),
        (Some(l), None) => format!("  {:?} (line {})\n", err.err, l),
        _ => format!("  {:?}\n", err.err),
    };
    body.push_str("  Suggestion: ");
    body.push_str(&codegen_error_suggestion(&err.err));
    body.push('\n');

    // Heading priority: module name (always, no conditional on
    // external vs user) → file path → `(no file)` bucket.
    if let Some(module) = err.module.as_deref() {
        out.push('\n');
        out.push_str(&format!("package: {}\n", module));
        if let Some(f) = err.file.as_deref() {
            out.push_str(&display_path(f));
            out.push('\n');
        }
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

fn codegen_error_suggestion(err: &fai_codegen_wasm::direct::BuildError) -> String {
    use fai_codegen_wasm::direct::BuildError;

    match err {
        BuildError::UnsupportedExpression("from_dict-without-typed-binding") => {
            "`from_dict` needs a target type codegen can see. Bind it as \
             `let x T = from_dict(d)` (or return it from a function whose \
             `@return` is a plain named type). Argument position and \
             optional/array/generic target types are not supported."
                .to_string()
        }
        BuildError::UnsupportedExpression(kind) => format!(
            "the direct wasm backend does not lower `{}` here yet. If this code should be unreachable for the current target, check the target's imports/reachability; otherwise reduce the construct or add backend support.",
            kind
        ),
        BuildError::UnsupportedStatement(kind) => format!(
            "the direct wasm backend does not lower `{}` here yet. Move this shape behind an unreachable target boundary or add backend support for the statement.",
            kind
        ),
        BuildError::UnknownIdentifier(name) => format!(
            "`{}` is not in scope for codegen. Check the import, module path, spelling, or generated remote proxy for this target.",
            name
        ),
        BuildError::ModuleAccessNotYetSupported(name) => format!(
            "`{}` did not resolve to a supported module function or stdlib member. Check the module import and exported function name.",
            name
        ),
        BuildError::DuplicateModuleName(name) => format!(
            "`{}` is defined by more than one discovered module. Rename the local module or dependency package so codegen has one canonical owner.",
            name
        ),
        BuildError::AsyncLoweringUnsupported { function, cause } => format!(
            "`{}` is async because of `{}` but could not be lowered. Run with FAI_ASYNC_DEBUG=1 for the concrete async-engine refusal, then fix that source location or reduce it into a compiler fixture.",
            function, cause
        ),
        other => format!(
            "codegen refused with `{:?}`. Run with FAI_ASYNC_DEBUG=1 for async-engine context, then reduce this to the smallest failing fixture under tests/fixtures/projects/ if the source looks valid.",
            other
        ),
    }
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
mod tests;
