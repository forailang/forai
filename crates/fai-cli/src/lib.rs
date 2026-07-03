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

mod build;
mod doc;
mod format;
pub mod interface;
mod mcp;
mod native_pack;
mod pipeline;
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

use build::*;
use native_pack::*;
use pipeline::*;
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

// `run_vm_with_quiet_panic` lived here to wrap the VM's synchronous
// `execute` / `run_tests` calls in a catch_unwind + hook-swap so an
// unhandled register-overflow panic surfaced as a clean `[fail]` line.
// Phase D/E removed the VM from the run and test paths; wasmtime traps
// return `Err` instead of unwinding, so the helper isn't needed.


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
    // Plan 132: literal `secrets.get` names must be declared in the
    // project's [secrets] manifest when one exists.
    if let Some(names) = declared_secret_names_for_path(path) {
        checker.set_declared_secrets(names);
    }
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
