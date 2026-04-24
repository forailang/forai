//! Harness for the `fai/tests/fixtures/language/` feature matrix.
//!
//! Walks the fixture tree, parses each fixture's leading `# ...` directive
//! block, and drives the `fai` CLI through a set of gates: fmt-check,
//! check, compile+run, and output comparison.
//!
//! The fixture format and gate semantics are documented in
//! `fai/tests/fixtures/language/README.md` — keep that file authoritative.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub const FIXTURES_SUBPATH: &str = "tests/fixtures/language";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    Ok,
    CheckError,
    CompileError,
    RuntimeError,
}

#[derive(Debug, Clone)]
pub enum ErrorPattern {
    Substring(String),
    Regex(String),
}

impl ErrorPattern {
    fn matches(&self, haystack: &str) -> bool {
        match self {
            ErrorPattern::Substring(s) => haystack.contains(s),
            ErrorPattern::Regex(_) => {
                // Regex support deferred — no regex crate dep yet.
                // For now, treat regex patterns as substring to keep the
                // door open without adding a dependency prematurely.
                panic!("regex error patterns not yet implemented — use a plain substring");
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub path: PathBuf,
    pub display_name: String,
    pub expect: Expect,
    pub stdout: Option<String>,
    pub error: Option<ErrorPattern>,
    pub skip: Option<String>,
}

#[derive(Debug)]
pub struct FixtureFailure {
    pub gate: &'static str,
    pub detail: String,
}

impl std::fmt::Display for FixtureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.gate, self.detail)
    }
}

/// Locate the fai workspace root by walking up from CARGO_MANIFEST_DIR
/// until we find a Cargo.toml with `[workspace]`.
pub fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut dir: &Path = &manifest;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            if let Ok(s) = fs::read_to_string(&candidate) {
                if s.contains("[workspace]") {
                    return dir.to_path_buf();
                }
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => panic!("could not find workspace root from {}", manifest.display()),
        }
    }
}

pub fn fixtures_root() -> PathBuf {
    workspace_root().join(FIXTURES_SUBPATH)
}

pub fn fai_binary() -> PathBuf {
    let bin = workspace_root().join("target").join("debug").join("fai");
    assert!(
        bin.exists(),
        "fai binary not found at {}; run `cargo build -p fai-cli` first",
        bin.display()
    );
    bin
}

pub fn discover_fixtures() -> Vec<Fixture> {
    let root = fixtures_root();
    let mut out = Vec::new();
    walk_fixture_dirs(&root, &root, &mut out);
    out.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    out
}

/// Each leaf fixture is a directory containing `main.fai`. The
/// directory name is the fixture's short name; the feature group is
/// its parent. This layout isolates each fixture from its siblings so
/// the pipeline's sibling-scan (which aggregates public functions and
/// test blocks across all `.fai` files in a directory) never crosses
/// fixture boundaries.
fn walk_fixture_dirs(root: &Path, dir: &Path, out: &mut Vec<Fixture>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let main_fai = path.join("main.fai");
        if main_fai.is_file() {
            match parse_fixture(root, &path, &main_fai) {
                Ok(fx) => out.push(fx),
                Err(e) => panic!("failed to parse fixture {}: {}", path.display(), e),
            }
        } else {
            walk_fixture_dirs(root, &path, out);
        }
    }
}

fn parse_fixture(root: &Path, fixture_dir: &Path, main_path: &Path) -> Result<Fixture, String> {
    let content = fs::read_to_string(main_path).map_err(|e| format!("read failed: {}", e))?;
    let rel = fixture_dir
        .strip_prefix(root)
        .map_err(|e| format!("strip_prefix failed: {}", e))?;
    let display_name = rel
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "::");

    // Directive block: leading `#` comment lines, terminated by the
    // first line that is not a comment (blank or code).
    let mut expect = Expect::Ok;
    let mut stdout_lines: Option<Vec<String>> = None;
    let mut error: Option<ErrorPattern> = None;
    let mut skip: Option<String> = None;

    let mut active: Option<&'static str> = None;
    for raw in content.lines() {
        let line = raw;
        if !line.starts_with('#') {
            break;
        }
        // Strip leading '#' and up to one following space.
        let rest = &line[1..];
        let rest = rest.strip_prefix(' ').unwrap_or(rest);

        // A directive line is a top-level `<name>:` or `<name>: <value>`.
        // An indented continuation is part of the previous directive's
        // multi-line value (currently only `stdout:`).
        let is_continuation = raw.starts_with("#  ") || raw.starts_with("#\t");
        if is_continuation {
            if active == Some("stdout") {
                // Take everything after the leading '#' + at least one space.
                let value = raw[1..].trim_start_matches(' ').to_string();
                // Drop exactly one extra space of indentation used to mark
                // continuation: we already stripped all leading spaces, so
                // the value is what remains.
                stdout_lines.get_or_insert_with(Vec::new).push(value);
            }
            continue;
        }

        // Top-level directive
        if let Some((key, value)) = rest.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "expect" => {
                    expect = match value {
                        "ok" => Expect::Ok,
                        "check_error" => Expect::CheckError,
                        "compile_error" => Expect::CompileError,
                        "runtime_error" => Expect::RuntimeError,
                        other => {
                            return Err(format!(
                                "unknown expect value '{}' (want ok|check_error|compile_error|runtime_error)",
                                other
                            ));
                        }
                    };
                    active = Some("expect");
                }
                "stdout" => {
                    stdout_lines = Some(Vec::new());
                    if !value.is_empty() {
                        // Single-line form: `# stdout: hello`
                        stdout_lines.as_mut().unwrap().push(value.to_string());
                    }
                    active = Some("stdout");
                }
                "error" => {
                    if let Some(rx) = value.strip_prefix('/').and_then(|s| s.strip_suffix('/')) {
                        error = Some(ErrorPattern::Regex(rx.to_string()));
                    } else {
                        error = Some(ErrorPattern::Substring(value.to_string()));
                    }
                    active = Some("error");
                }
                "skip" => {
                    skip = Some(value.to_string());
                    active = Some("skip");
                }
                "browser" => {
                    // Reserved — Phase D.
                    active = Some("browser");
                }
                _ => {
                    // Unknown directive: treat as a plain human comment.
                    active = None;
                }
            }
        } else {
            // Plain human comment, not a directive. Reset active.
            active = None;
        }
    }

    // Validation: error-expecting fixtures need a pattern.
    match (&expect, &error) {
        (Expect::CheckError, None)
        | (Expect::CompileError, None)
        | (Expect::RuntimeError, None) => {
            return Err(format!(
                "{:?} fixture needs an `# error:` directive",
                expect
            ));
        }
        _ => {}
    }

    let stdout = stdout_lines.map(|v| v.join("\n"));

    Ok(Fixture {
        path: main_path.to_path_buf(),
        display_name,
        expect,
        stdout,
        error,
        skip,
    })
}

// ── Gates ────────────────────────────────────────────────────────────

pub fn run_fixture(fx: &Fixture) -> Result<(), FixtureFailure> {
    if fx.skip.is_some() {
        return Ok(());
    }

    match fx.expect {
        Expect::Ok => {
            assert_fmt_check(fx)?;
            assert_check_ok(fx)?;
            assert_run_ok(fx)?;
        }
        Expect::CheckError => {
            // Skip fmt-check — invalid programs may not parse. The
            // checker gate is where the error is asserted.
            assert_check_error(fx)?;
        }
        Expect::CompileError => {
            assert_fmt_check(fx)?;
            assert_check_ok(fx)?;
            assert_run_error(fx, "compile")?;
        }
        Expect::RuntimeError => {
            assert_fmt_check(fx)?;
            assert_check_ok(fx)?;
            assert_run_error(fx, "runtime")?;
        }
    }
    Ok(())
}

fn fai_command(args: &[&str]) -> Output {
    Command::new(fai_binary())
        .args(args)
        .output()
        .expect("failed to spawn fai binary")
}

fn assert_fmt_check(fx: &Fixture) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy();
    let out = fai_command(&["fmt", &path, "--check"]);
    if out.status.success() {
        return Ok(());
    }
    Err(FixtureFailure {
        gate: "fmt",
        detail: format!(
            "fmt --check failed\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
    })
}

fn assert_check_ok(fx: &Fixture) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy();
    let out = fai_command(&["check", &path, "--check"]);
    if out.status.success() {
        return Ok(());
    }
    Err(FixtureFailure {
        gate: "check",
        detail: format!(
            "check failed\n  stdout: {}\n  stderr: {}",
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
    })
}

fn assert_check_error(fx: &Fixture) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy();
    let out = fai_command(&["check", &path, "--check"]);
    if out.status.success() {
        return Err(FixtureFailure {
            gate: "check",
            detail: "expected check to fail, but it succeeded".into(),
        });
    }
    let pattern = fx.error.as_ref().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    if pattern.matches(&stderr) || pattern.matches(&stdout) {
        return Ok(());
    }
    Err(FixtureFailure {
        gate: "check",
        detail: format!(
            "checker rejected as expected but message didn't match pattern.\n  pattern: {:?}\n  stdout: {}\n  stderr: {}",
            pattern, stdout.trim(), stderr.trim(),
        ),
    })
}

fn assert_run_ok(fx: &Fixture) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy();
    let out = fai_command(&["run", &path, "--check"]);
    if !out.status.success() {
        return Err(FixtureFailure {
            gate: "run",
            detail: format!(
                "fai run exited {}\n  stdout: {}\n  stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
        });
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let actual = strip_pipeline_chrome(&raw);
    if let Some(expected) = &fx.stdout {
        if normalize(&actual) != normalize(expected) {
            return Err(FixtureFailure {
                gate: "stdout",
                detail: format!(
                    "stdout did not match.\n  expected:\n{}\n  actual:\n{}",
                    indent(expected),
                    indent(&actual),
                ),
            });
        }
    }
    Ok(())
}

fn assert_run_error(fx: &Fixture, phase: &str) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy();
    let out = fai_command(&["run", &path, "--check"]);
    if out.status.success() {
        return Err(FixtureFailure {
            gate: "run",
            detail: format!("expected {} error but run succeeded", phase),
        });
    }
    let pattern = fx.error.as_ref().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    if pattern.matches(&stderr) || pattern.matches(&stdout) {
        return Ok(());
    }
    Err(FixtureFailure {
        gate: "run",
        detail: format!(
            "{} error as expected but message didn't match pattern.\n  pattern: {:?}\n  stdout: {}\n  stderr: {}",
            phase, pattern, stdout.trim(), stderr.trim(),
        ),
    })
}

/// The fai CLI's pipeline prints step lines like `  [ok] fmt   — ...` to
/// stdout at each stage. `fai run` runs fmt → check → test → run in
/// sequence, and the test step compiles+runs the module (including
/// `main`), so program output can appear *before* the `[ok] test` line
/// as well as after. We only care about the final run-step output:
/// return everything after the last chrome line, with any leading
/// blank line trimmed.
fn strip_pipeline_chrome(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().collect();
    let last_chrome = lines
        .iter()
        .enumerate()
        .rfind(|(_, l)| is_chrome_line(l))
        .map(|(i, _)| i);
    let start = match last_chrome {
        Some(i) => i + 1,
        None => 0,
    };
    lines[start..].join("\n")
}

fn is_chrome_line(line: &str) -> bool {
    // Pattern: `  [ok] ...` or `  [fail] ...` (leading two spaces, then
    // a bracketed status word). The reporter emits exactly this shape.
    let trimmed = line.strip_prefix("  ").unwrap_or(line);
    trimmed.starts_with("[ok]") || trimmed.starts_with("[fail]")
}

fn normalize(s: &str) -> String {
    s.trim_end_matches('\n').trim_end().to_string()
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {}", l))
        .collect::<Vec<_>>()
        .join("\n")
}
