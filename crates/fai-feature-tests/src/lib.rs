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

/// `# leak:` directive (plan 118 U3) — the expected-leak ratchet.
/// Two-sided: a `Flat` fixture that leaks fails; an `Expected` fixture
/// that runs flat fails ("flip the marker"). Fixtures WITHOUT the
/// directive are ungated — `Flat` is the opt-in for new baseline
/// fixtures, so the legacy corpus pays no extra leak-check run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeakExpectation {
    /// `# leak: flat` — must end with zero live heap objects. Locals
    /// release at scope exit; module-level `var`s persist, so flat
    /// fixtures avoid them (use `expected` or restructure).
    Flat,
    /// `# leak: expected <phase-tag>` — leaks today; the named plan-117
    /// phase is the one that must flip this marker to `flat`.
    Expected(String),
}

#[derive(Debug, Clone)]
pub struct Fixture {
    pub path: PathBuf,
    pub display_name: String,
    pub expect: Expect,
    pub stdout: Option<String>,
    pub browser: Option<BrowserAssertion>,
    pub error: Option<ErrorPattern>,
    pub skip: Option<String>,
    pub leak: Option<LeakExpectation>,
}

#[derive(Debug, Clone, Default)]
pub struct BrowserAssertion {
    pub selector: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub root_result: Option<String>,
    pub duration_less_than_ms: Option<u64>,
    pub duration_at_least_ms: Option<u64>,
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
    let mut browser = BrowserAssertion::default();
    let mut error: Option<ErrorPattern> = None;
    let mut skip: Option<String> = None;
    let mut leak: Option<LeakExpectation> = None;

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
            } else if active == Some("browser") {
                let value = raw[1..].trim_start_matches(' ');
                parse_browser_line(&mut browser, value)?;
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
                    if !value.is_empty() {
                        parse_browser_line(&mut browser, value)?;
                    }
                    active = Some("browser");
                }
                "leak" => {
                    leak = Some(parse_leak_value(value)?);
                    active = Some("leak");
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
    let browser = if browser.selector.is_some()
        || browser.text.is_some()
        || browser.html.is_some()
        || browser.root_result.is_some()
        || browser.duration_less_than_ms.is_some()
        || browser.duration_at_least_ms.is_some()
    {
        Some(browser)
    } else {
        None
    };

    // A leak gate only makes sense on a fixture that runs successfully.
    if leak.is_some() && expect != Expect::Ok {
        return Err("`# leak:` requires `# expect: ok`".to_string());
    }

    Ok(Fixture {
        path: main_path.to_path_buf(),
        display_name,
        expect,
        stdout,
        browser,
        error,
        skip,
        leak,
    })
}

fn parse_browser_line(browser: &mut BrowserAssertion, line: &str) -> Result<(), String> {
    let Some((key, value)) = line.split_once(':') else {
        return Err(format!(
            "browser directive lines must be `selector:`, `text:`, `html:`, `rootResult:`, `durationLessThanMs:`, or `durationAtLeastMs:`, got `{}`",
            line
        ));
    };
    let value = value.trim().to_string();
    match key.trim() {
        "selector" => browser.selector = Some(value),
        "text" => browser.text = Some(value),
        "html" => browser.html = Some(value),
        "rootResult" => browser.root_result = Some(value),
        "durationLessThanMs" => {
            browser.duration_less_than_ms = Some(value.parse::<u64>().map_err(|e| {
                format!(
                    "durationLessThanMs must be an integer millisecond value: {}",
                    e
                )
            })?)
        }
        "durationAtLeastMs" => {
            browser.duration_at_least_ms = Some(value.parse::<u64>().map_err(|e| {
                format!(
                    "durationAtLeastMs must be an integer millisecond value: {}",
                    e
                )
            })?)
        }
        other => {
            return Err(format!(
                "unknown browser assertion `{}` (want selector|text|html|rootResult|durationLessThanMs|durationAtLeastMs)",
                other
            ));
        }
    }
    Ok(())
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
            if fx.browser.is_some() {
                assert_browser_run(fx)?;
            } else {
                assert_run_ok(fx)?;
                // Native leak gate (plan 118 U3). Browser fixtures get
                // their leak gate inside the browser run instead (U4) —
                // their programs use browser natives the native host
                // lacks, so a native re-run can't be the oracle.
                if let Some(leak) = &fx.leak {
                    assert_leak_gate(fx, leak)?;
                }
            }
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

/// Parse a `# leak:` directive value. Pure so the grammar is unit-testable.
fn parse_leak_value(value: &str) -> Result<LeakExpectation, String> {
    if value == "flat" {
        return Ok(LeakExpectation::Flat);
    }
    if let Some(tag) = value.strip_prefix("expected") {
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(
                "`# leak: expected` needs a phase tag (e.g. `expected phase5`)".to_string(),
            );
        }
        return Ok(LeakExpectation::Expected(tag.to_string()));
    }
    Err(format!(
        "unknown leak value '{}' (want `flat` or `expected <phase-tag>`)",
        value
    ))
}

/// Extract the live-object count from `fai run --check-leaks` stderr.
/// Parses the STABLE sentinel pinned by `leak_ledger::sentinel_line`:
/// `[check-leaks] live heap: N objects, M bytes` (suffixes tolerated).
/// `None` when no sentinel line is present (run died before the report,
/// or the module carried no ledger events).
pub fn parse_live_objects(stderr: &str) -> Option<u64> {
    const PREFIX: &str = "[check-leaks] live heap: ";
    for line in stderr.lines() {
        if let Some(rest) = line.trim_start().strip_prefix(PREFIX) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u64>() {
                if rest[digits.len()..].starts_with(" object") {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// The `# leak:` gate (plan 118 U3): re-run the fixture under
/// `--check-leaks` and enforce the two-sided contract against the
/// sentinel. Exit code stays 0 either way (the flag is observational by
/// design) — the sentinel on stderr is the oracle.
fn assert_leak_gate(fx: &Fixture, leak: &LeakExpectation) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy().to_string();
    let out = fai_command(&["run", &path, "--check-leaks"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let live = parse_live_objects(&stderr).ok_or_else(|| FixtureFailure {
        gate: "leak",
        detail: format!(
            "no `[check-leaks] live heap:` sentinel on stderr — leak-instrumented run \
             produced no report. stderr:\n{}",
            stderr
        ),
    })?;
    match leak {
        LeakExpectation::Flat if live > 0 => Err(FixtureFailure {
            gate: "leak",
            detail: format!(
                "marked `leak: flat` but {} object(s) live at exit — an unexpected leak \
                 (regression). stderr:\n{}",
                live, stderr
            ),
        }),
        LeakExpectation::Expected(tag) if live == 0 => Err(FixtureFailure {
            gate: "leak",
            detail: format!(
                "marked `leak: expected {}` but ran FLAT — the leak is fixed; flip the \
                 marker to `leak: flat` in the same change",
                tag
            ),
        }),
        _ => Ok(()),
    }
}

fn assert_browser_run(fx: &Fixture) -> Result<(), FixtureFailure> {
    let path = fx.path.to_string_lossy().to_string();
    let build_dir = browser_build_dir(fx);
    let wasm_path = build_dir.join("main.wasm");
    let _ = fs::remove_dir_all(&build_dir);
    fs::create_dir_all(&build_dir).map_err(|e| FixtureFailure {
        gate: "browser",
        detail: format!("failed to create {}: {}", build_dir.display(), e),
    })?;

    let out = Command::new(fai_binary())
        .args(["build", path.as_str(), "--html", "-o"])
        .arg(&wasm_path)
        .output()
        .expect("failed to spawn fai binary");
    if !out.status.success() {
        return Err(FixtureFailure {
            gate: "browser-build",
            detail: format!(
                "fai build --html exited {}\n  stdout: {}\n  stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim(),
                String::from_utf8_lossy(&out.stderr).trim(),
            ),
        });
    }

    let harness = workspace_root().join("tests").join("browser-harness");
    let script = harness.join("run-fixture.mjs");
    let assertion = browser_assertion_json(fx.browser.as_ref().unwrap());
    let out = Command::new("node")
        .arg(&script)
        .arg(&build_dir)
        .arg(assertion)
        .current_dir(&harness)
        .output()
        .map_err(|e| FixtureFailure {
            gate: "browser",
            detail: format!(
                "failed to run browser harness. Install with `npm install --prefix {}` and `npm run --prefix {} install-browsers`: {}",
                harness.display(),
                harness.display(),
                e
            ),
        })?;

    if out.status.success() {
        return Ok(());
    }

    Err(FixtureFailure {
        gate: "browser",
        detail: format!(
            "browser harness exited {}\n  stdout: {}\n  stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim(),
        ),
    })
}

fn browser_build_dir(fx: &Fixture) -> PathBuf {
    let mut safe = String::new();
    for ch in fx.display_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            safe.push(ch);
        } else {
            safe.push('_');
        }
    }
    workspace_root()
        .join("target")
        .join("fai-browser-fixtures")
        .join(safe)
}

fn browser_assertion_json(assertion: &BrowserAssertion) -> String {
    let mut parts = Vec::new();
    if let Some(selector) = &assertion.selector {
        parts.push(format!("\"selector\":\"{}\"", escape_json(selector)));
    }
    if let Some(text) = &assertion.text {
        parts.push(format!("\"text\":\"{}\"", escape_json(text)));
    }
    if let Some(html) = &assertion.html {
        parts.push(format!("\"html\":\"{}\"", escape_json(html)));
    }
    if let Some(root_result) = &assertion.root_result {
        parts.push(format!("\"rootResult\":\"{}\"", escape_json(root_result)));
    }
    if let Some(ms) = assertion.duration_less_than_ms {
        parts.push(format!("\"durationLessThanMs\":{}", ms));
    }
    if let Some(ms) = assertion.duration_at_least_ms {
        parts.push(format!("\"durationAtLeastMs\":{}", ms));
    }
    format!("{{{}}}", parts.join(","))
}

fn escape_json(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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

#[cfg(test)]
mod leak_directive_tests {
    use super::*;

    #[test]
    fn parses_flat_and_expected() {
        assert_eq!(parse_leak_value("flat"), Ok(LeakExpectation::Flat));
        assert_eq!(
            parse_leak_value("expected phase5"),
            Ok(LeakExpectation::Expected("phase5".to_string()))
        );
    }

    #[test]
    fn rejects_missing_tag_and_unknown_values() {
        assert!(parse_leak_value("expected").is_err());
        assert!(parse_leak_value("expected   ").is_err());
        assert!(parse_leak_value("maybe").is_err());
        assert!(parse_leak_value("").is_err());
    }

    #[test]
    fn parses_sentinel_objects_count() {
        // The pinned sentinel shape, with and without suffixes.
        assert_eq!(
            parse_live_objects("[check-leaks] live heap: 3 objects, 99 bytes"),
            Some(3)
        );
        assert_eq!(
            parse_live_objects(
                "noise\n[check-leaks] live heap: 0 objects, 0 bytes (__live_objects=0 consistent; 2 host-side)\nmore"
            ),
            Some(0)
        );
        // Singular form would still parse (starts_with " object").
        assert_eq!(
            parse_live_objects("[check-leaks] live heap: 1 objects, 8 bytes"),
            Some(1)
        );
    }

    #[test]
    fn sentinel_absent_or_malformed_is_none() {
        assert_eq!(parse_live_objects(""), None);
        assert_eq!(parse_live_objects("[leak-check] live heap objects at exit: 3"), None);
        assert_eq!(parse_live_objects("[check-leaks] live heap: many objects"), None);
        assert_eq!(
            parse_live_objects("[check-leaks] no allocation events — not built with the flag"),
            None
        );
    }
}
