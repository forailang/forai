//! CI tests for the FAI_ABI_CHECK parity instrument (plan 118 U6).
//!
//! Two directions, both via subprocess so the env vars can't leak into
//! parallel tests: an unseeded compile must produce ZERO divergence
//! lines (the ownership table matches the legacy heuristic), and a
//! deliberately seeded misclassification (FAI_ABI_SEED, debug-only)
//! MUST be detected — if an injected wrong entry can hide from the
//! detector, a real one can too.

use fai_feature_tests::fai_binary;
use std::fs;
use std::process::Command;

const DIVERGENCE: &str = "[abi-check] DIVERGENCE";

/// A program whose `unwrap` call is classified Owned by the heuristic —
/// the seed flips the table side to Borrowed, which must surface.
const PROBE: &str = "def main
    @return Void
do
    let d = set({}, 'k', 'v')
    let s = unwrap(getString(d, 'k'), 'x')
    print(s)
end
";

fn write_probe(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("fai-abi-check-tests").join(name);
    fs::create_dir_all(&dir).expect("create probe dir");
    let path = dir.join("main.fai");
    fs::write(&path, PROBE).expect("write probe");
    path
}

fn run_probe(name: &str, seed: Option<&str>) -> String {
    let path = write_probe(name);
    let mut cmd = Command::new(fai_binary());
    cmd.arg("run").arg(&path).env("FAI_ABI_CHECK", "1");
    if let Some(seed) = seed {
        cmd.env("FAI_ABI_SEED", seed);
    } else {
        cmd.env_remove("FAI_ABI_SEED");
    }
    let out = cmd.output().expect("spawn fai");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn unseeded_compile_has_zero_divergences() {
    let stderr = run_probe("unseeded", None);
    assert!(
        !stderr.contains(DIVERGENCE),
        "table diverged from the heuristic on an unseeded build:\n{}",
        stderr
    );
}

#[test]
fn seeded_misclassification_is_detected_and_attributed() {
    let stderr = run_probe("seeded", Some("unwrap"));
    assert!(
        stderr.contains(DIVERGENCE),
        "seeded wrong entry was NOT detected — the parity instrument is blind:\n{}",
        stderr
    );
    // Attribution: module label + line:col + the seeded doc string.
    assert!(
        stderr.contains("SEEDED-BUG"),
        "divergence line lacks the seeded signature doc:\n{}",
        stderr
    );
    assert!(
        stderr.contains("<entry>:"),
        "divergence line lacks site attribution:\n{}",
        stderr
    );
}

#[test]
fn seed_naming_unknown_entry_warns_and_stays_inactive() {
    let stderr = run_probe("unknown_seed", Some("noSuchBuiltin"));
    assert!(
        stderr.contains("names no bare-call table entry"),
        "unknown seed name should warn:\n{}",
        stderr
    );
    assert!(
        !stderr.contains(DIVERGENCE),
        "unknown seed must be a no-op:\n{}",
        stderr
    );
}
