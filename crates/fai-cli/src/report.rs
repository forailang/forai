//! Unified CLI output for the `fai fmt / check / test / build` pipeline.
//!
//! Every step prints through this module so we get a consistent shape:
//!
//! ```text
//! checking 14 .fai files in partners ...
//!   [ok] fmt   — all files already formatted
//!   [ok] check — no type errors
//!   [ok] test  — 12 tests passed, coverage 100% (14/14 functions)
//!   [ok] build — main.wasm 153 KB + assets
//! ```
//!
//! On failure, the offending step emits its error text via
//! `error_line` and then the summary line gets `[fail]` instead of
//! `[ok]`. The `-v` / `--verbose` flag restores the older per-file
//! output (`formatted X.fai`, `compiled Y.wasm → N bytes`) as
//! `detail()` lines — off by default so agents reading the pipeline
//! output aren't drowned in noise.
//!
//! Colour is auto-detected: ANSI codes only emit when stdout is a
//! terminal and `NO_COLOR` isn't set. Non-TTY output (captured in
//! tests, piped to a file, read by an MCP client) stays plain.

use std::io::IsTerminal;

/// Whether a step finished successfully, failed, or surfaced a
/// non-fatal warning (reserved for future use — no caller emits
/// `Warn` yet, but keeping the enum variant saves a breaking change).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Fail,
    #[allow(dead_code)]
    Warn,
}

/// Output formatter for the pipeline. Construct one at the top of
/// `cmd_*` and thread a reference into each `step_*`.
pub struct Reporter {
    verbose: bool,
    use_color: bool,
}

/// Process-wide verbose flag. Set by `Reporter::new` so deep code paths
/// (e.g. `inject_rpc_dispatch`, `generate_rpc_proxy_modules`) can gate
/// their own status eprintlns on it without every caller threading a
/// `Reporter` reference down. Kept as an atomic so concurrent build
/// steps don't race on reads.
static GLOBAL_VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Returns true when `-v` / `--verbose` was passed to the current
/// CLI invocation. Used by CLI helpers that emit per-step informational
/// messages from paths where threading a `&Reporter` would be painful.
pub fn is_verbose() -> bool {
    GLOBAL_VERBOSE.load(std::sync::atomic::Ordering::Relaxed)
}

impl Reporter {
    /// Build a reporter from raw argv. Caller has already invoked
    /// `extract_verbose_flag` to split out `-v` / `--verbose`; pass
    /// the resulting bool here plus the TTY detection.
    pub fn new(verbose: bool) -> Self {
        GLOBAL_VERBOSE.store(verbose, std::sync::atomic::Ordering::Relaxed);
        Self {
            verbose,
            use_color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    /// Construct a non-colorised reporter for tests.
    #[cfg(test)]
    pub fn plain(verbose: bool) -> Self {
        Self {
            verbose,
            use_color: false,
        }
    }

    /// Print the top-level banner once per `cmd_*` invocation.
    /// Example: `checking 14 .fai files in partners ...`.
    pub fn header(&self, file_count: usize, project_name: &str) {
        println!(
            "checking {} .fai {} in {} ...",
            file_count,
            if file_count == 1 { "file" } else { "files" },
            project_name,
        );
    }

    /// Print a step summary line under the banner.
    /// Example: `  [ok] check — no type errors`.
    /// `name` is right-padded to 5 chars so dashes line up across steps.
    pub fn step(&self, status: StepStatus, name: &str, summary: &str) {
        let padded = format!("{:<5}", name);
        let marker = self.status_marker(status);
        println!("  {} {} — {}", marker, padded, summary);
    }

    /// Print a detail line (only shown under `-v` / `--verbose`).
    /// Used for today's per-file output — `formatted X.fai`,
    /// `compiled Y.wasm → N bytes`, etc.
    pub fn detail(&self, text: &str) {
        if self.verbose {
            println!("    {}", text);
        }
    }

    /// Print a full error line. Always shown (both default and
    /// verbose). Used by a failing step to expose each individual
    /// error before the `[fail]` summary.
    pub fn error_line(&self, text: &str) {
        // Use eprintln so error text lands on stderr — lets CI pipelines
        // separate diagnostic noise from the step summary on stdout.
        eprintln!("{}", text);
    }

    fn status_marker(&self, status: StepStatus) -> String {
        match (status, self.use_color) {
            (StepStatus::Ok, true) => "\x1b[32m[ok]\x1b[0m".to_string(),
            (StepStatus::Ok, false) => "[ok]".to_string(),
            (StepStatus::Fail, true) => "\x1b[31m[fail]\x1b[0m".to_string(),
            (StepStatus::Fail, false) => "[fail]".to_string(),
            (StepStatus::Warn, true) => "\x1b[33m[warn]\x1b[0m".to_string(),
            (StepStatus::Warn, false) => "[warn]".to_string(),
        }
    }
}

/// Extract `-v` / `--verbose` from argv, returning the remaining
/// args and whether the flag was present. Mirrors the existing
/// `extract_project_flag` pattern so the call sites stay consistent.
pub fn extract_verbose_flag(args: &[String]) -> (Vec<String>, bool) {
    let mut remaining = Vec::with_capacity(args.len());
    let mut verbose = false;
    for a in args {
        if a == "-v" || a == "--verbose" {
            verbose = true;
        } else {
            remaining.push(a.clone());
        }
    }
    (remaining, verbose)
}

/// Recursively count `.fai` files under `root`. Used to populate
/// the top-level `checking N .fai files in ...` banner. Skips common
/// build / vendor directories so the number matches user intuition —
/// agents don't expect the `build/` or `target/` tree to count.
pub fn count_fai_files_recursive(root: &std::path::Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                // Skip build artefacts and hidden dirs — users asking
                // "how many files am I about to check" don't include
                // generated output or .git in their mental model.
                if name.starts_with('.')
                    || name == "build"
                    || name == "target"
                    || name == "node_modules"
                {
                    continue;
                }
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("fai") {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_step_ok_format() {
        // Plain reporter mirrors what CI / piped output sees — no ANSI.
        // The exact format is load-bearing: agents learn to recognise
        // the prefix, so nothing outside this test should tweak it
        // without updating the scaffolds at the same time.
        let r = Reporter::plain(false);
        // Captured via the shared output buffer — we don't have a real
        // capture shim here, so this test verifies the format string
        // indirectly by reconstructing what step() builds.
        let marker = r.status_marker(StepStatus::Ok);
        assert_eq!(marker, "[ok]");
    }

    #[test]
    fn test_step_fail_format() {
        let r = Reporter::plain(false);
        assert_eq!(r.status_marker(StepStatus::Fail), "[fail]");
    }

    #[test]
    fn test_step_warn_format() {
        let r = Reporter::plain(false);
        assert_eq!(r.status_marker(StepStatus::Warn), "[warn]");
    }

    #[test]
    fn test_color_codes_when_enabled() {
        let r = Reporter {
            verbose: false,
            use_color: true,
        };
        assert!(r.status_marker(StepStatus::Ok).contains("\x1b[32m"));
        assert!(r.status_marker(StepStatus::Fail).contains("\x1b[31m"));
    }

    #[test]
    fn test_verbose_flag_extraction() {
        let args = vec![
            "-p".to_string(),
            "server".to_string(),
            "--verbose".to_string(),
        ];
        let (remaining, verbose) = extract_verbose_flag(&args);
        assert!(verbose);
        assert_eq!(remaining, vec!["-p".to_string(), "server".to_string()]);
    }

    #[test]
    fn test_verbose_flag_short_form() {
        let args = vec!["-v".to_string(), "--check".to_string()];
        let (remaining, verbose) = extract_verbose_flag(&args);
        assert!(verbose);
        assert_eq!(remaining, vec!["--check".to_string()]);
    }

    #[test]
    fn test_verbose_flag_absent() {
        let args = vec!["--check".to_string()];
        let (remaining, verbose) = extract_verbose_flag(&args);
        assert!(!verbose);
        assert_eq!(remaining, vec!["--check".to_string()]);
    }

    #[test]
    fn test_count_fai_files_skips_build_dir() {
        // Create a small scratch directory structure and confirm the
        // build/target/hidden dirs don't inflate the count.
        use std::fs;
        let tmp = std::env::temp_dir().join(format!("fai_count_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("build")).unwrap();
        fs::create_dir_all(tmp.join(".git")).unwrap();
        fs::write(tmp.join("src").join("a.fai"), "").unwrap();
        fs::write(tmp.join("src").join("b.fai"), "").unwrap();
        fs::write(tmp.join("build").join("c.fai"), "").unwrap(); // shouldn't count
        fs::write(tmp.join(".git").join("d.fai"), "").unwrap(); // shouldn't count
        fs::write(tmp.join("readme.md"), "").unwrap(); // not .fai
        assert_eq!(count_fai_files_recursive(&tmp), 2);
        let _ = fs::remove_dir_all(&tmp);
    }
}
