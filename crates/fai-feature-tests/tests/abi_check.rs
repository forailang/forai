//! CI tests for the plan-117 ownership-coverage enforcement (plan 119 U2,
//! repurposed from the plan-118 parity tests after the heuristic swap).
//!
//! The table now DRIVES classification, so there is no heuristic to
//! diverge from. What must stay provable instead: a boxed host import
//! with no ownership signature is a compile error under checked flags and
//! a `MISSING-SIGNATURE` sentinel otherwise. `FAI_ABI_SEED=<import>`
//! (debug-only) makes one row invisible to the coverage check — without
//! touching classification — so both paths fire on demand. Subprocess
//! invocation keeps the env vars away from parallel tests.

use fai_feature_tests::fai_binary;
use std::fs;
use std::process::Command;

const SENTINEL: &str = "[abi-check] MISSING-SIGNATURE";

/// A probe whose `json.parse` call emits the boxed host import the seed
/// targets.
const PROBE: &str = "use std.json

def main
    @return Void
do
    let parsed = json.parse('{\"a\": 1}')
    let s = unwrap(getString(parsed, 'a'), 'x')
    print(s)
end
";

fn run_probe(name: &str, seed: Option<&str>, abi_check: bool) -> (String, bool) {
    let dir = std::env::temp_dir().join("fai-abi-check-tests").join(name);
    fs::create_dir_all(&dir).expect("create probe dir");
    let path = dir.join("main.fai");
    fs::write(&path, PROBE).expect("write probe");
    let mut cmd = Command::new(fai_binary());
    cmd.arg("run").arg(&path);
    if abi_check {
        cmd.env("FAI_ABI_CHECK", "1");
    } else {
        cmd.env_remove("FAI_ABI_CHECK");
    }
    match seed {
        Some(s) => cmd.env("FAI_ABI_SEED", s),
        None => cmd.env_remove("FAI_ABI_SEED"),
    };
    let out = cmd.output().expect("spawn fai");
    (
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout),
        out.status.success(),
    )
}

#[test]
fn full_surface_has_no_missing_signatures() {
    let (output, ok) = run_probe("clean", None, true);
    assert!(ok, "probe should build and run clean:\n{}", output);
    assert!(
        !output.contains(SENTINEL),
        "unsigned boxed import on an unseeded build:\n{}",
        output
    );
}

#[test]
fn seeded_absent_entry_is_a_checked_build_error() {
    let (output, ok) = run_probe("seeded_checked", Some("json_parse"), true);
    assert!(
        !ok,
        "checked build must FAIL on a missing signature:\n{}",
        output
    );
    assert!(
        output.contains("MissingOwnershipSignature") && output.contains("json_parse"),
        "error must name the unsigned import:\n{}",
        output
    );
}

#[test]
fn seeded_absent_entry_logs_sentinel_on_unchecked_build() {
    let (output, ok) = run_probe("seeded_unchecked", Some("json_parse"), false);
    assert!(ok, "unchecked build must still succeed:\n{}", output);
    assert!(
        output.contains(SENTINEL) && output.contains("json_parse"),
        "sentinel must fire and name the import:\n{}",
        output
    );
}

#[test]
fn seed_naming_unknown_import_warns_and_stays_inactive() {
    let (output, ok) = run_probe("unknown_seed", Some("noSuchImport"), true);
    assert!(ok, "unknown seed must not break the build:\n{}", output);
    assert!(
        output.contains("names no host-import row"),
        "unknown seed should warn:\n{}",
        output
    );
    assert!(
        !output.contains(SENTINEL),
        "unknown seed must be a no-op:\n{}",
        output
    );
}
