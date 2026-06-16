//! Seeded validation for ownership instrumentation diagnostics.
//!
//! `FAI_OWNERSHIP_SEED=suppress-<op>` is debug-only and removes one helper
//! event family from codegen without changing the underlying retain/release
//! behavior. Subprocess invocation keeps the seed isolated from parallel tests.

use fai_feature_tests::fai_binary;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const PROBE: &str = "# expect: ok
# stdout:
#   done
# Returns the same string.
def id
    @param s String
    @return String
do
    s
end

test id
    it 'seeded'
        let s = \"owned\"
        assert.equal(id(s), \"owned\")
    end
end

def main
    @return Void
do
    print('done')
end
";

const MAKE_PROBE: &str = "# expect: ok
# Make value.
def make
    @return String
do
    let value = 'ok'
    value
end

test make
    it 'returns'
        assert.equal(make(), 'ok')
    end
end
";

fn write_probe(name: &str) -> (PathBuf, PathBuf) {
    write_probe_source(name, PROBE)
}

fn write_probe_source(name: &str, source: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir()
        .join("fai-ownership-instrumentation-tests")
        .join(name);
    fs::create_dir_all(&dir).expect("create probe dir");
    let path = dir.join("main.fai");
    fs::write(&path, source).expect("write probe");
    (dir, path)
}

#[test]
fn seeded_suppressed_retain_fails_ownership_check_with_site_history() {
    let (dir, path) = write_probe("suppressed-retain");
    let out = Command::new(fai_binary())
        .arg("test")
        .arg(&path)
        .arg("--check-ownership")
        .env("FAI_OWNERSHIP_SEED", "suppress-retain")
        .output()
        .expect("spawn fai");
    let output =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "seeded ownership imbalance should fail the test run:\n{}",
        output
    );
    assert!(
        output.contains("[ownership-check]") && output.contains("unmatched cleanup"),
        "report should describe the seeded imbalance:\n{}",
        output
    );
    assert!(
        output.contains("direct:cleanup:cleanup owned value") && output.contains("history:"),
        "report should include resolved site labels and history:\n{}",
        output
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn seeded_suppressed_transfer_fails_with_uncredited_store() {
    let (dir, path) = write_probe_source("suppressed-transfer", MAKE_PROBE);
    let out = Command::new(fai_binary())
        .arg("test")
        .arg(&path)
        .arg("--check-ownership")
        .env("FAI_OWNERSHIP_SEED", "suppress-transfer")
        .output()
        .expect("spawn fai");
    let output =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "seeded transfer omission should fail the test run:\n{}",
        output
    );
    assert!(
        output.contains("uncredited owning store; missing retain/transfer before store"),
        "report should describe the transfer-family proof failure:\n{}",
        output
    );
    assert!(
        output.contains("direct:store:store owned value") && output.contains("groups:"),
        "report should include grouped resolved store site:\n{}",
        output
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn seeded_suppressed_cleanup_fails_when_free_retires_credit() {
    let (dir, path) = write_probe_source("suppressed-cleanup", MAKE_PROBE);
    let out = Command::new(fai_binary())
        .arg("test")
        .arg(&path)
        .arg("--check-ownership")
        .env("FAI_OWNERSHIP_SEED", "suppress-cleanup")
        .output()
        .expect("spawn fai");
    let output =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);

    assert!(
        !out.status.success(),
        "seeded cleanup omission should fail the test run:\n{}",
        output
    );
    assert!(
        output.contains("live helper credit retired by free; missing cleanup before free"),
        "report should describe the cleanup-family proof failure:\n{}",
        output
    );
    assert!(
        output.contains("groups:") && output.contains("proof live helper credit"),
        "report should include grouped proof failure:\n{}",
        output
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn unknown_ownership_seed_warns_and_remains_inactive() {
    let (dir, path) = write_probe("unknown-seed");
    let out = Command::new(fai_binary())
        .arg("test")
        .arg(&path)
        .arg("--check-ownership")
        .env("FAI_OWNERSHIP_SEED", "suppress-not-an-op")
        .output()
        .expect("spawn fai");
    let output =
        String::from_utf8_lossy(&out.stderr).into_owned() + &String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "unknown seed should be inactive, not fail the run:\n{}",
        output
    );
    assert!(
        output.contains("FAI_OWNERSHIP_SEED='suppress-not-an-op' names no ownership op"),
        "unknown seed should produce an explicit warning:\n{}",
        output
    );
    assert!(
        output.contains("[ownership-check]")
            && output.contains("0 object(s) with helper imbalance"),
        "ownership check should still run cleanly:\n{}",
        output
    );

    let _ = fs::remove_dir_all(&dir);
}
